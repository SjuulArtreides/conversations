//! Metadata for the host-to-server terminal stream. Terminal bytes alone cannot
//! distinguish an application redraw from a replacement of retained state.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtyFrameKind {
    #[default]
    Output,
    Snapshot,
}

/// Codex's inline transcript rebuild: reset the scroll region, reset SGR, home,
/// erase the display, erase the scrollback, home again.
///
/// This starts a redraw, not proof that all earlier history will be reproduced.
/// The shared history tracker compares the completed synchronized redraw with
/// retained rows and removes only identical contiguous overlap or containment.
/// Idle startup chrome is replaced; changed or omitted answers remain readable. An explicit host snapshot still replaces
/// state because it already contains reconciled history.
///
/// Verified byte-for-byte against a real interactive capture of codex-cli
/// 0.154.0 (`crates/server/tests/fixtures/pty/codex_interactive_reflow.jsonl`),
/// where it arrives as its own 21-byte read. The leading scroll-region and SGR
/// resets plus the trailing home are what separate it from the bare
/// `ESC[H ESC[2J ESC[3J` an application emits to redraw an answer in place —
/// that one must keep archiving history, since losing it drops terminal output.
/// Match the whole signature;
/// never infer replacement from the erase pair alone.
pub const CODEX_TRANSCRIPT_REBUILD: &[u8] = b"\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H";

/// Length of the longest suffix of `bytes` that is a STRICT prefix of `needle`.
///
/// A PTY read can end in the middle of a multi-sequence signature, and the first
/// half of [`CODEX_TRANSCRIPT_REBUILD`] is a run of individually valid escape
/// sequences that would otherwise be acted on for the wrong reason. Callers hold
/// back this many trailing bytes so the next chunk completes the match. Returns
/// 0 when no suffix matches, and when `bytes` already ends with the complete
/// `needle` — that is a match for the caller's scan, not a partial one.
pub fn partial_prefix_len(bytes: &[u8], needle: &[u8]) -> usize {
    let longest = bytes.len().min(needle.len().saturating_sub(1));
    (1..=longest)
        .rev()
        .find(|&len| bytes[bytes.len() - len..] == needle[..len])
        .unwrap_or(0)
}

/// Rebuilds normal history and the active screen, retaining the screen beneath a TUI.
pub fn replay_snapshot(parser: &mut vt100::Parser) -> Vec<u8> {
    let source = parser.screen();
    let mut out = Vec::new();
    replay_screen(&mut source.normal_screen(), false, &mut out);
    if source.alternate_screen() {
        replay_screen(&mut source.clone(), true, &mut out);
    }
    out
}

fn replay_screen(screen: &mut vt100::Screen, alternate: bool, out: &mut Vec<u8>) {
    out.extend_from_slice(if alternate {
        b"\x1b[?47h"
    } else {
        b"\x1b[?47l"
    });
    out.extend_from_slice(b"\x1b[r\x1b[H\x1b[2J\x1b[3J");
    let (_, cols) = screen.size();
    screen.set_scrollback(usize::MAX);
    let total = screen.scrollback();
    for line in 0..total {
        screen.set_scrollback(total - line);
        if let Some(row) = screen.rows_formatted(0, cols).next() {
            out.extend(row);
            out.extend_from_slice(b"\x1b[m\r\n");
        }
    }
    screen.set_scrollback(0);
    for (index, row) in screen.rows_formatted(0, cols).enumerate() {
        if index > 0 {
            out.extend_from_slice(b"\r\n");
        }
        out.extend(row);
        out.extend_from_slice(b"\x1b[m");
    }
    let (top, bottom) = screen.scroll_region();
    out.extend_from_slice(format!("\x1b[{};{}r", top + 1, bottom + 1).as_bytes());
    out.extend(screen.saved_cursor_formatted());
    out.extend(screen.input_mode_formatted());
    out.extend(screen.attributes_formatted());
    out.extend_from_slice(if screen.hide_cursor() {
        b"\x1b[?25l"
    } else {
        b"\x1b[?25h"
    });
    let (row, col) = screen.cursor_position();
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottom_scroll_region_homes_cursor_using_the_origin_mode() {
        let mut parser = vt100::Parser::new(8, 30, 20);
        parser.process(b"\x1b[?1049h\x1b[5;8r");
        assert_eq!(parser.screen().cursor_position(), (0, 0));
        parser.process(b"main pane");
        assert_eq!(parser.screen().rows(0, 30).next().unwrap(), "main pane");
        parser.process(b"\x1b[?6h\x1b[6;8r");
        assert_eq!(parser.screen().cursor_position(), (5, 0));
        parser.process(b"strip update");
        assert_eq!(parser.screen().rows(0, 30).nth(5).unwrap(), "strip update");
        parser.process(b"\x1b[?6l\x1b[5;8rheader update");
        assert_eq!(parser.screen().rows(0, 30).next().unwrap(), "header update");
    }

    #[test]
    fn operations_outside_the_scroll_region_preserve_its_rows() {
        for operation in [b"\x1bM".as_slice(), b"\x1b[L", b"\x1b[M"] {
            let mut parser = vt100::Parser::new(8, 30, 20);
            parser.process(b"header\x1b[5;1Hstrip one\r\nstrip two\r\nstrip three\r\nstrip four");
            parser.process(b"\x1b[5;8r");
            let before = parser.screen().contents();
            parser.process(operation);
            assert_eq!(parser.screen().contents(), before, "{operation:?}");
            assert_eq!(parser.screen().cursor_position(), (0, 0));
        }
        for operation in [b"\x1b[L".as_slice(), b"\x1b[M"] {
            let mut parser = vt100::Parser::new(8, 30, 20);
            parser.process(b"header\x1b[2;1Hstrip one\r\nstrip two\r\nstrip three\x1b[8;1Hfooter");
            parser.process(b"\x1b[2;4r\x1b[8;1H");
            let before = parser.screen().contents();
            parser.process(operation);
            assert_eq!(parser.screen().contents(), before, "{operation:?}");
        }
    }

    #[test]
    fn partial_prefix_len_holds_back_every_split_of_the_rebuild() {
        for split in 1..CODEX_TRANSCRIPT_REBUILD.len() {
            let mut input = b"output before".to_vec();
            input.extend_from_slice(&CODEX_TRANSCRIPT_REBUILD[..split]);
            assert_eq!(
                partial_prefix_len(&input, CODEX_TRANSCRIPT_REBUILD),
                split,
                "a signature split at {split} must be held for the next chunk"
            );
        }
    }

    #[test]
    fn partial_prefix_len_ignores_complete_and_unrelated_tails() {
        let needle = CODEX_TRANSCRIPT_REBUILD;
        assert_eq!(partial_prefix_len(needle, needle), 0);
        let mut trailing = b"before".to_vec();
        trailing.extend_from_slice(needle);
        assert_eq!(partial_prefix_len(&trailing, needle), 0);
        assert_eq!(partial_prefix_len(b"plain output", needle), 0);
        // An erase pair that only resembles the signature holds back nothing.
        assert_eq!(partial_prefix_len(b"\x1b[H\x1b[2J\x1b[3J", needle), 0);
        assert_eq!(partial_prefix_len(b"", needle), 0);
    }
}
