//! W3 — Prefix-key hotkey layer + status bar for the local multiplexer.
//!
//! This module is the tmux-style control layer that sits IN FRONT of the W2
//! [`LocalBridge`](super::interactive::LocalBridge) stdin→PTY pump. Every byte
//! the human types into the host terminal (in raw mode) is fed through the
//! [`PrefixKey`] state machine FIRST; the resulting [`MuxAction`] decides whether
//! the byte passes through to the focused child or is consumed as a multiplexer
//! command (switch focus / create / quit).
//!
//! Both pieces here are PURE and I/O-free so they can be exhaustively unit-tested
//! over byte sequences and child lists:
//!   - [`PrefixKey::feed`] — a byte → [`MuxAction`] state machine (no terminal,
//!     no PTY, no network).
//!   - [`render_status_bar`] — a child list + focus marker → a single status line
//!     (no cursor movement; the caller owns placement/redraw — see the wiring in
//!     `host.rs`).

/// Default prefix byte: `Ctrl-a` (0x01).
///
/// `Ctrl-a` collides with readline's "start of line" inside claude's input box
/// so the
/// prefix is a configurable field on [`PrefixKey`] rather than a hard-coded
/// constant. Prefix-prefix (`Ctrl-a` `Ctrl-a`) sends ONE literal prefix byte to
/// the child, which is how the human still gets readline's start-of-line.
pub(super) const DEFAULT_PREFIX: u8 = 0x01;

/// ESC (`0x1b`) — the introducer of the multi-byte escape encodings the prefix
/// takes under enhanced keyboard protocols (see [`enhanced_prefix_encodings`]).
const ESC: u8 = 0x1b;

/// Upper bound on a buffered CSI sequence before we give up matching it against
/// the prefix and replay it verbatim. Real key-event sequences are short
/// (`CSI 27;5;97~`, `CSI 97;5:3u`); this only bounds a pathological run so the
/// held buffer can't grow without limit.
const MAX_ESC_LEN: usize = 64;

/// How a complete CSI escape sequence relates to the configured prefix key, once
/// the focused child has enabled an enhanced keyboard protocol.
///
/// Under xterm's *modifyOtherKeys* and the *kitty keyboard protocol*,
/// `Ctrl-<letter>` is NOT reported as the legacy control byte (`0x01` for
/// `Ctrl-a`) — it arrives as a CSI sequence carrying the letter's codepoint and a
/// Ctrl modifier:
///
/// - modifyOtherKeys (`formatOtherKeys=1`): `CSI 27 ; 5 ; <code> ~`  (press only)
/// - kitty keyboard protocol:               `CSI <code> ; 5 [ : <event> ] u`
///
/// (`5` = the Ctrl modifier: `1 + 4`.) The kitty protocol additionally reports an
/// *event type* sub-parameter when the child enables event-type reporting (codex
/// does; claude does not): `1`/absent = press, `2` = repeat, `3` = release. So a
/// single `Ctrl-a` press becomes a `Press` (`CSI 97;5u`) IMMEDIATELY followed by
/// a `RepeatOrRelease` (`CSI 97;5:3u`). The state machine must arm on the former
/// and silently absorb the latter — otherwise the release event lands in
/// `AwaitingCommand`, gets misread as the command, disarms the machine, and the
/// real command byte leaks through to the child (the "Ctrl-keys do nothing under
/// codex" bug).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefixEvent {
    /// A press (or modifyOtherKeys report) of the prefix key → arm the machine.
    Press,
    /// A repeat/release of the prefix key → absorb; arm state is unchanged.
    RepeatOrRelease,
    /// A kitty keyboard release event for any non-prefix key. Some terminals
    /// emit the printable key normally and then emit this release CSI; Codex
    /// does not consume it and it lands in the prompt as `[117;1:3u`-style text.
    /// Drop it as terminal-keyboard bookkeeping, not application input.
    NonPrefixRelease,
    /// Not the prefix (a different key, a focus event, an arrow, …) → the caller
    /// replays the sequence verbatim to the child.
    NotPrefix,
}

/// Does an enhanced-protocol modifier value denote Ctrl and ONLY Ctrl (not
/// Ctrl+Shift, Alt, …)? The wire value is `1 + bitmask` where the low nibble is
/// shift(1)/alt(2)/ctrl(4)/super(8); the high bits (hyper/meta and the caps-lock
/// 64 / num-lock 128 *lock* state) are ignored so a held lock can't defeat the
/// match (`Ctrl-a` with num-lock on arrives as modifier `133`, not `5`).
fn is_ctrl_only(modifier: u32) -> bool {
    modifier >= 1 && (modifier - 1) & 0x0f == 0x04
}

/// The decision the prefix-key state machine makes for one incoming local-stdin
/// byte. Pure data — the caller (`host.rs`) executes the side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MuxAction {
    /// Forward this byte unchanged to the focused child's PTY (the common case:
    /// a normal keystroke in Normal state).
    Passthrough(u8),
    /// Forward these bytes unchanged, in order. Emitted when a buffered escape
    /// sequence that looked like it might be the prefix's enhanced encoding
    /// diverges — the held bytes were never the prefix, so they're replayed to
    /// the child verbatim (no byte is ever lost or reordered).
    PassthroughMany(Vec<u8>),
    /// The byte was consumed by the state machine and must NOT reach the child
    /// (the prefix byte itself, an unknown command after the prefix, or a byte
    /// held mid-match while it still prefixes an enhanced-encoding target).
    Swallowed,
    /// Switch focus to the child at this 0-based index (prefix + digit).
    Focus(usize),
    /// Focus the next child in the deterministic ordering (prefix + `n`).
    FocusNext,
    /// Focus the previous child in the deterministic ordering (prefix + `p`).
    FocusPrev,
    /// Create a new session via the existing local spawn path (prefix + `c`).
    Create,
    /// Detach/quit the multiplexer: reap all children and exit (prefix + `d`/`q`).
    Quit,
    /// Send exactly ONE literal prefix byte to the child (prefix-prefix escape).
    Literal(u8),
}

/// State of the prefix-key state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Bytes pass through to the focused child; the prefix byte arms the machine.
    Normal,
    /// The previous byte was the prefix; the next byte is interpreted as a
    /// multiplexer command and is NOT forwarded to the child.
    AwaitingCommand,
}

/// A tmux-style prefix-key state machine over local-stdin bytes.
///
/// Drive it one byte at a time with [`feed`](Self::feed). It is byte-oriented and
/// completely free of I/O, so it can be unit-tested over arbitrary byte
/// sequences. The two states are [`State::Normal`] (pass-through) and
/// [`State::AwaitingCommand`] (the byte after the prefix is a command).
pub(super) struct PrefixKey {
    prefix: u8,
    state: State,
    /// The Unicode codepoint a `Ctrl-<letter>` prefix carries in its enhanced
    /// (modifyOtherKeys / kitty) encoding — `Ctrl-a` (`0x01`) → `'a'` (`97`).
    /// `None` for a non-`Ctrl-letter` prefix, which has no enhanced encoding and
    /// disables escape-sequence collection entirely (legacy single byte only).
    ctrl_code: Option<u32>,
    /// Bytes of an in-progress escape sequence. Held (NOT forwarded) while it
    /// could still be the prefix's enhanced encoding; replayed verbatim via
    /// [`MuxAction::PassthroughMany`] when it turns out to be a different key;
    /// cleared on a complete CSI (which arms or is absorbed). Empty when idle.
    esc_buf: Vec<u8>,
}

impl PrefixKey {
    /// A state machine using [`DEFAULT_PREFIX`] (`Ctrl-a`).
    pub(super) fn new() -> Self {
        Self::with_prefix(DEFAULT_PREFIX)
    }

    /// A state machine with a custom prefix byte (configurable to dodge
    /// readline collisions — see [`DEFAULT_PREFIX`]).
    pub(super) fn with_prefix(prefix: u8) -> Self {
        Self {
            prefix,
            state: State::Normal,
            // Ctrl-a (0x01) -> 'a' (0x61), … Ctrl-z (0x1a) -> 'z' (0x7a).
            ctrl_code: (0x01..=0x1a)
                .contains(&prefix)
                .then(|| prefix as u32 + 0x60),
            esc_buf: Vec::new(),
        }
    }

    /// Feed one local-stdin byte and get the action to take. Pure: the only
    /// effect is the internal state transition; the caller performs the I/O
    /// implied by the returned [`MuxAction`].
    ///
    /// Transitions:
    ///   - **Normal**: the prefix byte (legacy single byte OR its enhanced-encoding
    ///     CSI press) → [`MuxAction::Swallowed`] and arm ([`State::AwaitingCommand`]);
    ///     the prefix's repeat/release CSI → [`MuxAction::Swallowed`] (absorbed);
    ///     any other byte → [`MuxAction::Passthrough`] (state unchanged).
    ///   - **AwaitingCommand** (returns to Normal once a command byte arrives —
    ///     the prefix's own release CSI is absorbed first, keeping us armed):
    ///     - `1`..=`9` → [`MuxAction::Focus`] of `digit - 1` (so `1` is the
    ///       first child, index 0);
    ///     - `0` → [`MuxAction::Focus`] index 9 (the tenth child, tmux-style);
    ///     - `n` → [`MuxAction::FocusNext`]; `p` → [`MuxAction::FocusPrev`];
    ///     - `c` → [`MuxAction::Create`];
    ///     - `d` / `q` → [`MuxAction::Quit`];
    ///     - the prefix again → [`MuxAction::Literal`] of the prefix (the
    ///       prefix-prefix escape — ONE literal prefix reaches the child);
    ///     - anything else → [`MuxAction::Swallowed`] (consumed, ignored).
    pub(super) fn feed(&mut self, byte: u8) -> MuxAction {
        // An in-progress escape sequence is collected across BOTH states. This is
        // load-bearing for the kitty protocol: a key press emits its CSI press
        // event immediately followed by a release event, so the prefix's release
        // (`CSI <code>;5:3u`) arrives while we are already `AwaitingCommand`. It
        // must be absorbed there rather than misread as the command byte.
        if !self.esc_buf.is_empty() {
            return self.feed_escape_byte(byte);
        }
        // Start collecting only when the prefix actually has an enhanced encoding
        // (a `Ctrl-<letter>`). The ESC is held until we can tell whether the
        // sequence is the prefix (arm/absorb) or a different key (replayed
        // verbatim) — no byte is ever lost.
        if byte == ESC && self.ctrl_code.is_some() {
            self.esc_buf.push(byte);
            return MuxAction::Swallowed;
        }
        match self.state {
            State::Normal => self.feed_normal(byte),
            State::AwaitingCommand => {
                // Every command byte returns us to Normal.
                self.state = State::Normal;
                self.feed_command(byte)
            }
        }
    }

    /// Normal-state handling for a plain (non-escape) byte: recognize the legacy
    /// single-byte prefix and arm the machine, else pass the byte through.
    fn feed_normal(&mut self, byte: u8) -> MuxAction {
        if byte == self.prefix {
            self.state = State::AwaitingCommand;
            return MuxAction::Swallowed;
        }
        MuxAction::Passthrough(byte)
    }

    /// Extend the in-progress escape sequence with `byte`. Holds the byte
    /// ([`MuxAction::Swallowed`]) while the buffer could still be the prefix's
    /// CSI encoding; classifies a complete CSI; and replays the whole buffer
    /// verbatim ([`MuxAction::PassthroughMany`]) the moment it can't be the prefix.
    fn feed_escape_byte(&mut self, byte: u8) -> MuxAction {
        self.esc_buf.push(byte);
        match self.esc_buf.len() {
            // The 2nd byte: a CSI must continue `ESC [`. Anything else (a lone
            // ESC, an `ESC O …` SS3 key, …) is not a prefix encoding.
            2 => {
                if byte == b'[' {
                    MuxAction::Swallowed
                } else {
                    self.flush_esc()
                }
            }
            // Inside `CSI <params…> <final>`: a final byte (`0x40..=0x7e`, e.g.
            // `u`, `~`, `A`, `I`) closes the sequence; param bytes (`0x20..=0x3f`,
            // i.e. digits, `;`, `:`) keep it open; anything else is malformed.
            _ => {
                if (0x40..=0x7e).contains(&byte) {
                    let seq = std::mem::take(&mut self.esc_buf);
                    self.classify_complete_csi(seq)
                } else if (0x20..=0x3f).contains(&byte) && self.esc_buf.len() <= MAX_ESC_LEN {
                    MuxAction::Swallowed
                } else {
                    self.flush_esc()
                }
            }
        }
    }

    /// The held escape buffer turned out NOT to be the prefix — replay it verbatim
    /// to the child and return to a non-armed Normal state (a stray escape after
    /// the prefix is treated as an unknown command: it doesn't keep us armed).
    fn flush_esc(&mut self) -> MuxAction {
        self.state = State::Normal;
        MuxAction::PassthroughMany(std::mem::take(&mut self.esc_buf))
    }

    /// Act on a COMPLETE CSI sequence per [`Self::classify_csi`]: the prefix's
    /// press arms the machine (or, when already armed, is the prefix-prefix
    /// literal); its repeat/release is absorbed with the arm state untouched;
    /// anything else is replayed verbatim (or, when armed, consumed as an unknown
    /// command — mirroring the legacy single-byte path).
    fn classify_complete_csi(&mut self, seq: Vec<u8>) -> MuxAction {
        match self.classify_csi(&seq) {
            PrefixEvent::Press => {
                if self.state == State::AwaitingCommand {
                    self.state = State::Normal;
                    MuxAction::Literal(self.prefix)
                } else {
                    self.state = State::AwaitingCommand;
                    MuxAction::Swallowed
                }
            }
            PrefixEvent::RepeatOrRelease => MuxAction::Swallowed,
            PrefixEvent::NonPrefixRelease => MuxAction::Swallowed,
            PrefixEvent::NotPrefix => {
                if self.state == State::AwaitingCommand {
                    self.state = State::Normal;
                    MuxAction::Swallowed
                } else {
                    MuxAction::PassthroughMany(seq)
                }
            }
        }
    }

    /// Parse a complete CSI sequence and decide how it relates to the prefix key.
    /// Pure (reads only `self.ctrl_code`). Recognizes the kitty form
    /// `CSI <keycode>[:alt] ; <mod>[:<event>] [; <text>] u` (with press/repeat/
    /// release event types) and the modifyOtherKeys press `CSI 27 ; <mod> ; <code> ~`.
    fn classify_csi(&self, seq: &[u8]) -> PrefixEvent {
        let Some(code) = self.ctrl_code else {
            return PrefixEvent::NotPrefix;
        };
        // The collector only hands us `ESC [ … <final>` (len >= 3), but re-check.
        if seq.len() < 3 || seq[0] != ESC || seq[1] != b'[' {
            return PrefixEvent::NotPrefix;
        }
        let final_byte = seq[seq.len() - 1];
        let Ok(params) = std::str::from_utf8(&seq[2..seq.len() - 1]) else {
            return PrefixEvent::NotPrefix;
        };
        match final_byte {
            // kitty keyboard protocol.
            b'u' => {
                let Some((keycode, modifier, event)) = parse_kitty_csi_params(params) else {
                    return PrefixEvent::NotPrefix;
                };
                if keycode != code {
                    return if event == 3 {
                        PrefixEvent::NonPrefixRelease
                    } else {
                        PrefixEvent::NotPrefix
                    };
                }
                if !is_ctrl_only(modifier) {
                    return if event == 3 {
                        PrefixEvent::NonPrefixRelease
                    } else {
                        PrefixEvent::NotPrefix
                    };
                }
                match event {
                    1 => PrefixEvent::Press,
                    2 | 3 => PrefixEvent::RepeatOrRelease,
                    _ => PrefixEvent::NotPrefix,
                }
            }
            // modifyOtherKeys (press only): `CSI 27 ; <mod> ; <keycode> ~`.
            b'~' => {
                let g: Vec<&str> = params.split(';').collect();
                let is_prefix = g.len() == 3
                    && g[0] == "27"
                    && g[2].parse::<u32>().ok() == Some(code)
                    && g[1].parse::<u32>().map(is_ctrl_only).unwrap_or(false);
                if is_prefix {
                    PrefixEvent::Press
                } else {
                    PrefixEvent::NotPrefix
                }
            }
            _ => PrefixEvent::NotPrefix,
        }
    }

    /// AwaitingCommand-state handling: interpret the byte after the prefix as a
    /// multiplexer command. Command keys (`c`/`n`/`p`/digits) arrive as plain
    /// single bytes even under enhanced keyboard protocols.
    fn feed_command(&mut self, byte: u8) -> MuxAction {
        if byte == self.prefix {
            // Prefix-prefix: emit one literal prefix to the child.
            return MuxAction::Literal(self.prefix);
        }
        match byte {
            // tmux-style: `1` selects the first child (index 0); `0`
            // selects the tenth (index 9).
            b'1'..=b'9' => MuxAction::Focus((byte - b'1') as usize),
            b'0' => MuxAction::Focus(9),
            b'n' => MuxAction::FocusNext,
            b'p' => MuxAction::FocusPrev,
            b'c' => MuxAction::Create,
            b'd' | b'q' => MuxAction::Quit,
            // Unknown command: consume it, do nothing.
            _ => MuxAction::Swallowed,
        }
    }
}

/// Parse the kitty keyboard protocol's CSI params:
/// `<keycode>[:alts] ; <modifier>[:event] [; text]`.
///
/// Returns `(keycode, modifier, event)`, with omitted modifier/event defaulting
/// to `1` (no modifier / press). Invalid keycodes are rejected; invalid optional
/// fields fall back to the protocol defaults, matching the tolerant behavior the
/// prefix matcher already had.
fn parse_kitty_csi_params(params: &str) -> Option<(u32, u32, u32)> {
    let mut groups = params.split(';');
    let keycode = groups
        .next()
        .and_then(|g| g.split(':').next())
        .and_then(|c| c.parse::<u32>().ok())?;
    let mut mod_parts = groups.next().unwrap_or("1").split(':');
    let modifier = mod_parts.next().and_then(|m| m.parse().ok()).unwrap_or(1);
    let event = mod_parts.next().and_then(|e| e.parse().ok()).unwrap_or(1);
    Some((keycode, modifier, event))
}

/// One child's display info for the status bar (pure render input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChildInfo {
    /// A short label for the child — typically the `live_id` (or a truncation of
    /// it). Rendered verbatim; the renderer truncates the WHOLE line to `cols`.
    pub(super) label: String,
}

impl ChildInfo {
    pub(super) fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

/// Render a single bottom status line listing the children and marking the
/// focused one.
///
/// PURE: returns the line as a `String`; it performs NO cursor movement and NO
/// I/O. The caller positions/redraws it (see the placement comment in `host.rs`).
///
/// Format: a `conv` prefix, then one `[<n> <label>]` entry per child (1-based
/// index, matching the prefix+digit hotkeys), with the focused child marked by a
/// leading `*` inside its brackets, e.g.:
///
/// ```text
/// conv  [1 abc123]  [*2 def456]
/// ```
///
/// Zero children renders a sane empty bar (`conv  (no sessions)`). The whole line
/// is truncated to `cols` chars (never panics on a multi-byte boundary because we
/// truncate by `char`); when `cols == 0` the result is empty.
pub(super) fn render_status_bar(
    children: &[ChildInfo],
    focused: Option<usize>,
    cols: usize,
) -> String {
    let mut line = String::from("conv");
    if children.is_empty() {
        line.push_str("  (no sessions)");
    } else {
        for (i, child) in children.iter().enumerate() {
            let marker = if Some(i) == focused { "*" } else { "" };
            // 1-based index so it lines up with the prefix+digit hotkeys.
            line.push_str(&format!("  [{marker}{} {}]", i + 1, child.label));
        }
    }
    truncate_to_cols(&line, cols)
}

/// Truncate `s` to at most `cols` characters (char-boundary safe).
fn truncate_to_cols(s: &str, cols: usize) -> String {
    if cols == 0 {
        return String::new();
    }
    s.chars().take(cols).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Prefix state machine: normal pass-through --------------------------

    #[test]
    fn normal_bytes_pass_through_unchanged() {
        let mut pk = PrefixKey::new();
        for b in b"hello world" {
            assert_eq!(pk.feed(*b), MuxAction::Passthrough(*b));
        }
    }

    #[test]
    fn prefix_is_swallowed_and_arms_the_machine() {
        let mut pk = PrefixKey::with_prefix(DEFAULT_PREFIX);
        // The prefix byte itself never reaches the child.
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
    }

    // --- Prefix + command bytes ---------------------------------------------

    #[test]
    fn prefix_then_digit_focuses_that_index() {
        let mut pk = PrefixKey::new();
        // `1` → first child (index 0).
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
        assert_eq!(pk.feed(b'1'), MuxAction::Focus(0));
        // `9` → tenth-by-key (index 8).
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
        assert_eq!(pk.feed(b'9'), MuxAction::Focus(8));
        // `0` → index 9 (tmux-style tenth slot).
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
        assert_eq!(pk.feed(b'0'), MuxAction::Focus(9));
    }

    #[test]
    fn prefix_then_n_and_p_cycle_focus() {
        let mut pk = PrefixKey::new();
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
        assert_eq!(pk.feed(b'n'), MuxAction::FocusNext);
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
        assert_eq!(pk.feed(b'p'), MuxAction::FocusPrev);
    }

    #[test]
    fn prefix_then_c_creates() {
        let mut pk = PrefixKey::new();
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
        assert_eq!(pk.feed(b'c'), MuxAction::Create);
    }

    #[test]
    fn prefix_then_d_or_q_quits() {
        let mut pk = PrefixKey::new();
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
        assert_eq!(pk.feed(b'd'), MuxAction::Quit);
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
        assert_eq!(pk.feed(b'q'), MuxAction::Quit);
    }

    // --- Prefix under enhanced keyboard protocols ---------------------------

    /// Feed a whole byte slice; return the resulting actions in order.
    fn feed_all(pk: &mut PrefixKey, bytes: &[u8]) -> Vec<MuxAction> {
        bytes.iter().map(|b| pk.feed(*b)).collect()
    }

    /// Collect every byte a sequence of actions forwards to the child, in order
    /// (Passthrough + PassthroughMany + Literal). Used to assert transparency.
    fn forwarded(actions: &[MuxAction]) -> Vec<u8> {
        let mut out = Vec::new();
        for a in actions {
            match a {
                MuxAction::Passthrough(b) | MuxAction::Literal(b) => out.push(*b),
                MuxAction::PassthroughMany(bs) => out.extend_from_slice(bs),
                _ => {}
            }
        }
        out
    }

    #[test]
    fn modify_other_keys_encoded_prefix_is_recognized() {
        // claude enables xterm modifyOtherKeys, so Ctrl-a arrives as
        // `ESC [ 27 ; 5 ; 97 ~` rather than the legacy 0x01. Every byte is held
        // (swallowed) until the closing `~` completes the match and arms.
        let mut pk = PrefixKey::new();
        let actions = feed_all(&mut pk, b"\x1b[27;5;97~");
        assert!(
            actions.iter().all(|a| *a == MuxAction::Swallowed),
            "the whole encoded prefix must be swallowed, got {actions:?}",
        );
        // Armed: the following `c` is the command.
        assert_eq!(pk.feed(b'c'), MuxAction::Create);
    }

    #[test]
    fn kitty_encoded_prefix_is_recognized() {
        // kitty keyboard protocol form: `ESC [ 97 ; 5 u`.
        let mut pk = PrefixKey::new();
        let actions = feed_all(&mut pk, b"\x1b[97;5u");
        assert!(actions.iter().all(|a| *a == MuxAction::Swallowed));
        assert_eq!(pk.feed(b'n'), MuxAction::FocusNext);
    }

    #[test]
    fn non_prefix_escape_sequence_passes_through_verbatim() {
        // An arrow key (`ESC [ A`) shares the `ESC [` introducer with the prefix
        // encodings, so it's held until it diverges, then replayed intact.
        let mut pk = PrefixKey::new();
        let actions = feed_all(&mut pk, b"\x1b[A");
        assert_eq!(forwarded(&actions), b"\x1b[A");
        // The machine is NOT armed — a following byte still passes through.
        assert_eq!(pk.feed(b'x'), MuxAction::Passthrough(b'x'));
    }

    #[test]
    fn other_ctrl_key_in_modify_other_keys_passes_through() {
        // Ctrl-c under modifyOtherKeys is `ESC [ 27 ; 5 ; 99 ~` — it shares a long
        // prefix with Ctrl-a's encoding but diverges at the codepoint, so it must
        // be replayed verbatim to claude, NOT eaten as the mux prefix.
        let mut pk = PrefixKey::new();
        let actions = feed_all(&mut pk, b"\x1b[27;5;99~");
        assert_eq!(forwarded(&actions), b"\x1b[27;5;99~");
    }

    #[test]
    fn terminal_focus_events_pass_through() {
        // `ESC [ I` / `ESC [ O` (focus in/out) are emitted constantly by the
        // enhanced mode and must reach claude unchanged.
        let mut pk = PrefixKey::new();
        assert_eq!(forwarded(&feed_all(&mut pk, b"\x1b[I")), b"\x1b[I");
        assert_eq!(forwarded(&feed_all(&mut pk, b"\x1b[O")), b"\x1b[O");
    }

    // --- kitty keyboard protocol WITH event-type reporting (codex) -----------

    #[test]
    fn kitty_event_type_press_release_then_command_still_focuses() {
        // The codex bug. codex enables the kitty protocol with event-type
        // reporting, so ONE Ctrl-a press emits a press CSI (`ESC[97;5u`)
        // IMMEDIATELY followed by a release CSI (`ESC[97;5:3u`). The press arms;
        // the release must be absorbed (not consume the command slot); the
        // following `1` must focus — and NOTHING leaks to the child.
        let mut pk = PrefixKey::new();
        let press = feed_all(&mut pk, b"\x1b[97;5u");
        assert!(
            press.iter().all(|a| *a == MuxAction::Swallowed),
            "press must be swallowed, got {press:?}",
        );
        let release = feed_all(&mut pk, b"\x1b[97;5:3u");
        assert!(
            release.iter().all(|a| *a == MuxAction::Swallowed),
            "release must be absorbed, got {release:?}",
        );
        // Still armed → the command focuses; the prefix sequences never reached
        // the child (the previous behaviour leaked `[97;5:3u` and lost the focus).
        assert_eq!(pk.feed(b'1'), MuxAction::Focus(0));
        assert!(forwarded(&press).is_empty() && forwarded(&release).is_empty());
    }

    #[test]
    fn kitty_press_with_explicit_event_type_one_arms() {
        // An explicit press event type (`:1`) is equivalent to an absent one.
        let mut pk = PrefixKey::new();
        let actions = feed_all(&mut pk, b"\x1b[97;5:1u");
        assert!(actions.iter().all(|a| *a == MuxAction::Swallowed));
        assert_eq!(pk.feed(b'c'), MuxAction::Create);
    }

    #[test]
    fn kitty_prefix_repeat_is_absorbed_and_does_not_arm() {
        // A held-key repeat (`:2`) of the prefix is swallowed (never forwarded),
        // and on its own does not arm the machine.
        let mut pk = PrefixKey::new();
        let actions = feed_all(&mut pk, b"\x1b[97;5:2u");
        assert!(actions.iter().all(|a| *a == MuxAction::Swallowed));
        assert!(forwarded(&actions).is_empty());
        assert_eq!(pk.feed(b'1'), MuxAction::Passthrough(b'1'));
    }

    #[test]
    fn kitty_non_prefix_key_release_is_swallowed() {
        // Codex asks the terminal for key-release events but does not consume
        // printable-key releases when running under our PTY wrapper; if these
        // reach the child they show up as literal `[49;1:3u` prompt text.
        let mut pk = PrefixKey::new();
        let actions = feed_all(&mut pk, b"\x1b[49;1:3u");
        assert!(actions.iter().all(|a| *a == MuxAction::Swallowed));
        assert!(forwarded(&actions).is_empty());
    }

    #[test]
    fn ctrl_shift_of_prefix_letter_is_not_the_prefix() {
        // Ctrl+Shift+a is modifier 6 (1 + ctrl4 + shift1), NOT the bare Ctrl-a
        // prefix (modifier 5). It must pass through, not arm the mux.
        let mut pk = PrefixKey::new();
        let actions = feed_all(&mut pk, b"\x1b[97;6u");
        assert_eq!(forwarded(&actions), b"\x1b[97;6u");
        assert_eq!(pk.feed(b'1'), MuxAction::Passthrough(b'1'));
    }

    #[test]
    fn lock_modifiers_do_not_defeat_the_prefix_match() {
        // With num-lock held, kitty reports Ctrl-a as modifier 133 (1 + ctrl4 +
        // numlock128). The lock bit is ignored so the prefix still arms.
        let mut pk = PrefixKey::new();
        let actions = feed_all(&mut pk, b"\x1b[97;133u");
        assert!(
            actions.iter().all(|a| *a == MuxAction::Swallowed),
            "got {actions:?}",
        );
        assert_eq!(pk.feed(b'n'), MuxAction::FocusNext);
    }

    #[test]
    fn kitty_prefix_prefix_emits_one_literal_prefix() {
        // Ctrl-a Ctrl-a under the kitty protocol: press (arm) → release
        // (absorbed, still armed) → a SECOND press while armed = the
        // prefix-prefix literal — exactly one prefix byte reaches the child.
        let mut pk = PrefixKey::new();
        assert!(feed_all(&mut pk, b"\x1b[97;5u")
            .iter()
            .all(|a| *a == MuxAction::Swallowed));
        assert!(feed_all(&mut pk, b"\x1b[97;5:3u")
            .iter()
            .all(|a| *a == MuxAction::Swallowed));
        let second = feed_all(&mut pk, b"\x1b[97;5u");
        assert_eq!(forwarded(&second), &[DEFAULT_PREFIX]);
    }

    #[test]
    fn realistic_codex_prefix_one_stream_focuses_without_leaking() {
        // The exact shape from the bug report: a focus-in event, Ctrl-a
        // press+release, the `1` command (a literal byte — codex does not enable
        // "report all keys as escape codes"), then the `1` release. The mux must
        // Focus(0); only the focus-in reaches the child.
        let mut pk = PrefixKey::new();
        let mut actions = Vec::new();
        actions.extend(feed_all(&mut pk, b"\x1b[I")); // focus-in
        actions.extend(feed_all(&mut pk, b"\x1b[97;5u")); // Ctrl-a press
        actions.extend(feed_all(&mut pk, b"\x1b[97;5:3u")); // Ctrl-a release
        actions.push(pk.feed(b'1')); // command
        actions.extend(feed_all(&mut pk, b"\x1b[49;1:3u")); // `1` release
        assert!(actions.contains(&MuxAction::Focus(0)));
        assert_eq!(forwarded(&actions), b"\x1b[I");
    }

    #[test]
    fn codex_printable_typing_drops_release_artifacts() {
        // The CLI symptom: typing "Just" produced `Ju[117;1:3u...` because the
        // printable press bytes reached Codex, then kitty release events were
        // forwarded as if they were text. Keep the press bytes; drop releases.
        let mut pk = PrefixKey::new();
        let mut actions = Vec::new();
        for (ch, code) in [(b'J', 74), (b'u', 117), (b's', 115), (b't', 116)] {
            actions.push(pk.feed(ch));
            actions.extend(feed_all(&mut pk, format!("\x1b[{code};1:3u").as_bytes()));
        }
        assert_eq!(forwarded(&actions), b"Just");
    }

    #[test]
    fn lone_escape_then_text_passes_through_verbatim() {
        // `ESC` not followed by `[` is not a CSI (here an `ESC O P` SS3 key) —
        // every byte still reaches the child, just held one byte then replayed.
        let mut pk = PrefixKey::new();
        let actions = feed_all(&mut pk, b"\x1bOP");
        assert_eq!(forwarded(&actions), b"\x1bOP");
        assert_eq!(pk.feed(b'x'), MuxAction::Passthrough(b'x'));
    }

    #[test]
    fn encoded_prefix_then_command_survives_interleaved_text() {
        // Type "hi", press Ctrl-a (encoded) then `c`, then type "x".
        let mut pk = PrefixKey::new();
        assert_eq!(forwarded(&feed_all(&mut pk, b"hi")), b"hi");
        let armed = feed_all(&mut pk, b"\x1b[27;5;97~");
        assert!(armed.iter().all(|a| *a == MuxAction::Swallowed));
        assert_eq!(pk.feed(b'c'), MuxAction::Create);
        assert_eq!(pk.feed(b'x'), MuxAction::Passthrough(b'x'));
    }

    // --- Prefix-prefix literal escape ---------------------------------------

    #[test]
    fn prefix_prefix_yields_one_literal_prefix_and_returns_to_normal() {
        let mut pk = PrefixKey::with_prefix(DEFAULT_PREFIX);
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
        // Second prefix → exactly one literal prefix byte to the child.
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Literal(DEFAULT_PREFIX));
        // Back in Normal: a following normal byte passes through.
        assert_eq!(pk.feed(b'x'), MuxAction::Passthrough(b'x'));
        // And a single further prefix only swallows once (does NOT emit a
        // second literal on its own).
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
    }

    #[test]
    fn literal_escape_with_custom_prefix() {
        let mut pk = PrefixKey::with_prefix(b'b'); // arbitrary custom prefix
        assert_eq!(pk.feed(b'b'), MuxAction::Swallowed);
        assert_eq!(pk.feed(b'b'), MuxAction::Literal(b'b'));
        // A plain `b` after returning to Normal is again swallowed+armed.
        assert_eq!(pk.feed(b'b'), MuxAction::Swallowed);
    }

    // --- Unknown command after prefix ---------------------------------------

    #[test]
    fn prefix_then_unknown_is_swallowed_and_returns_to_normal() {
        let mut pk = PrefixKey::new();
        assert_eq!(pk.feed(DEFAULT_PREFIX), MuxAction::Swallowed);
        // `z` is not a command → consumed, nothing happens.
        assert_eq!(pk.feed(b'z'), MuxAction::Swallowed);
        // Back to Normal: the next byte passes through.
        assert_eq!(pk.feed(b'z'), MuxAction::Passthrough(b'z'));
    }

    // --- A prefix mid-stream must not corrupt the following normal bytes ----

    #[test]
    fn prefix_midstream_does_not_corrupt_following_bytes() {
        let mut pk = PrefixKey::new();
        // Type "ab", then prefix+2 (focus index 1), then "cd".
        let mut actions = Vec::new();
        for b in b"ab" {
            actions.push(pk.feed(*b));
        }
        actions.push(pk.feed(DEFAULT_PREFIX)); // swallowed
        actions.push(pk.feed(b'2')); // Focus(1)
        for b in b"cd" {
            actions.push(pk.feed(*b));
        }
        assert_eq!(
            actions,
            vec![
                MuxAction::Passthrough(b'a'),
                MuxAction::Passthrough(b'b'),
                MuxAction::Swallowed,
                MuxAction::Focus(1),
                MuxAction::Passthrough(b'c'),
                MuxAction::Passthrough(b'd'),
            ]
        );
    }

    #[test]
    fn digits_that_look_like_normal_text_pass_through_in_normal_state() {
        // Outside a prefix, digits are ordinary keystrokes (you can type "123"
        // into claude's prompt) — they must NOT be interpreted as focus commands.
        let mut pk = PrefixKey::new();
        for b in b"123" {
            assert_eq!(pk.feed(*b), MuxAction::Passthrough(*b));
        }
    }

    // --- Status bar render (pure) -------------------------------------------

    #[test]
    fn status_bar_lists_children_with_focus_marker() {
        let children = vec![
            ChildInfo::new("abc123"),
            ChildInfo::new("def456"),
            ChildInfo::new("ghi789"),
        ];
        let bar = render_status_bar(&children, Some(1), 200);
        assert_eq!(bar, "conv  [1 abc123]  [*2 def456]  [3 ghi789]");
    }

    #[test]
    fn status_bar_no_focus_marks_nothing() {
        let children = vec![ChildInfo::new("only")];
        let bar = render_status_bar(&children, None, 200);
        assert_eq!(bar, "conv  [1 only]");
    }

    #[test]
    fn status_bar_zero_children_is_a_sane_empty_bar() {
        let bar = render_status_bar(&[], None, 200);
        assert_eq!(bar, "conv  (no sessions)");
    }

    #[test]
    fn status_bar_truncates_to_cols() {
        let children = vec![ChildInfo::new("abcdefghij"), ChildInfo::new("klmnopqrst")];
        // Full line is much longer than 10 cols → truncated to exactly 10 chars.
        let bar = render_status_bar(&children, Some(0), 10);
        assert_eq!(bar.chars().count(), 10);
        assert_eq!(bar, "conv  [*1 ");
    }

    #[test]
    fn status_bar_zero_cols_is_empty() {
        let children = vec![ChildInfo::new("x")];
        assert_eq!(render_status_bar(&children, Some(0), 0), "");
    }

    #[test]
    fn status_bar_truncation_is_char_boundary_safe() {
        // A multi-byte label truncated mid-way must not panic.
        let children = vec![ChildInfo::new("café-🚀-session")];
        let bar = render_status_bar(&children, Some(0), 12);
        assert_eq!(bar.chars().count(), 12);
    }
}
