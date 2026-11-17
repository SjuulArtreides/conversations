use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use conversations_core::{terminal::PtyFrameKind, AgentKind};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast;

use super::Backend;

/// The largest local message: one `ServerMessage::TransferContent` at the signed
/// limits (every byte base64, every name JSON-escaped at worst), plus framing.
const MAX_MESSAGE: usize = {
    use conversations_core::terminal_files::{MAX_FILES, MAX_NAME_BYTES, MAX_TOTAL_BYTES};
    let content = 4 * (MAX_TOTAL_BYTES / 3 + MAX_FILES);
    let names = MAX_FILES * MAX_NAME_BYTES * 6;
    let larger = content + names + 64 * 1024;
    if larger > 8 * 1024 * 1024 {
        larger
    } else {
        8 * 1024 * 1024
    }
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Launch {
    pub id: String,
    pub agent: AgentKind,
    pub cwd: PathBuf,
    pub command: Vec<String>,
}

impl Launch {
    pub fn title(&self) -> String {
        let name = if self.agent == AgentKind::Zsh {
            self.command
                .first()
                .and_then(|command| std::path::Path::new(command).file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("terminal")
        } else {
            self.agent.as_str()
        };
        format!("Local: {name}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub launch: Launch,
    pub workspace: Option<String>,
    pub surface: Option<String>,
    pub local_terminal: bool,
    /// This wrapper previously reached a local host. A new daemon must not
    /// revive a session that graduated while the daemon was offline.
    #[serde(default)]
    pub reconnecting: bool,
    /// Vestigial capability flag from the removed central file-consumer
    /// streams; kept on the wire so wrappers from the previous release still
    /// register (serde ignores the unknown field when it is dropped). New code
    /// must not read it as authority for anything.
    #[serde(default)]
    pub file_receiver: bool,
    /// This wrapper applies clipboard/file content only for transfers the host
    /// authorized. Older wrappers do not, so the host never hands them the
    /// stream credentials.
    #[serde(default)]
    pub transfer_gate: bool,
    /// This wrapper re-checks the signed authority of every remote effect
    /// (keystrokes, a reply, a peer message, a staged paste) right before it
    /// writes, and answers `CancelInteraction`. Older wrappers ignore the
    /// authority field, so the host sends them no remote effect at all.
    #[serde(default)]
    pub effect_gate: bool,
    /// `start-agent`: the agent runs in the terminal it was started from, so
    /// the host keeps it on that exact cmux surface and workspace, even the
    /// host's own, and never moves or reorders it. Placement only; it grants
    /// nothing.
    #[serde(default)]
    pub in_place: bool,
}

pub fn release_marker(dir: &std::path::Path, id: &str, pid: u32) -> PathBuf {
    dir.join("released").join(format!("{id}-{pid}"))
}

fn liveness_path(dir: &std::path::Path, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id)?;
    Ok(dir.join("wrappers").join(format!("{id}.lock")))
}

/// A reopen request belongs to one wrapper lifetime, never its replacement.
pub fn reopen_marker(dir: &std::path::Path, id: &str, pid: u32) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id)?;
    Ok(dir.join(format!("reopen-{id}-{pid}")))
}

/// Held by a wrapper for its whole lifetime, including while it is not
/// registered with any daemon. The descriptor is close-on-exec, so the agent
/// never inherits it. The file names the owning wrapper's pid.
pub fn hold_wrapper_liveness(dir: &std::path::Path, id: &str) -> Result<std::fs::File> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let path = liveness_path(dir, id)?;
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    for _ in 0..100 {
        // SAFETY: file is an open descriptor owned by this function.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            file.set_len(0)?;
            write!(file, "{}", std::process::id())?;
            return Ok(file);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("another terminal is still running conversation {id}")
}

/// Whether a wrapper process for `id` is alive, connected to a daemon or not.
pub async fn wrapper_alive(dir: &std::path::Path, id: &str) -> bool {
    wrapper_owners(dir, id).await.is_some()
}

/// The pids of live wrappers for `id`; empty when one is alive but its pid is
/// unknown. `None` when no wrapper is running.
pub async fn wrapper_owners(dir: &std::path::Path, id: &str) -> Option<Vec<u32>> {
    match locked_wrapper_owner(dir, id) {
        LockProbe::Held(owners) => Some(owners),
        LockProbe::Released => None,
        // Wrappers started by a CLI from before liveness locks hold none.
        LockProbe::Missing => {
            unlocked_wrapper_pids(std::ffi::OsStr::new("ps"), PROCESS_LIST_TIMEOUT, id).await
        }
    }
}

enum LockProbe {
    Held(Vec<u32>),
    Released,
    Missing,
}

/// Opening and probing a local lock file never waits on another process.
fn locked_wrapper_owner(dir: &std::path::Path, id: &str) -> LockProbe {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(path) = liveness_path(dir, id) else {
        return LockProbe::Released;
    };
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return LockProbe::Missing,
        Err(_) => return LockProbe::Released,
    };
    // A shared probe never blocks another probe; dropping the file releases it.
    // SAFETY: file is an open descriptor owned by this function.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return LockProbe::Released;
    }
    let mut pid = String::new();
    let _ = file.read_to_string(&mut pid);
    LockProbe::Held(pid.trim().parse().into_iter().collect())
}

const PROCESS_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Finds wrappers by the argv every cmux launch has used, among this user's
/// processes. A listing that fails or outlives `timeout` counts as running:
/// never start a replacement while ownership is unknown.
async fn unlocked_wrapper_pids(
    ps: &std::ffi::OsStr,
    timeout: std::time::Duration,
    id: &str,
) -> Option<Vec<u32>> {
    let listing = tokio::process::Command::new(ps)
        .args(["-A", "-ww", "-o", "uid=,pid=,args="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(Ok(output)) = tokio::time::timeout(timeout, listing).await else {
        return Some(Vec::new());
    };
    if !output.status.success() {
        return Some(Vec::new());
    }
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() }.to_string();
    let pids = wrappers_in_process_list(&String::from_utf8_lossy(&output.stdout), &uid, id);
    (!pids.is_empty()).then_some(pids)
}

fn wrappers_in_process_list(listing: &str, uid: &str, id: &str) -> Vec<u32> {
    let needle = format!(" wrap --session-id {id} ");
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let owner = fields.next()?;
            let pid = fields.next()?.parse().ok()?;
            let args = fields.collect::<Vec<_>>().join(" ");
            (owner == uid && format!(" {args} ").contains(&needle)).then_some(pid)
        })
        .collect()
}

fn pending_end_path(dir: &std::path::Path, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id)?;
    Ok(dir.join("pending-end").join(id))
}

/// Journals an End for the wrapper processes in `owners` (empty: unknown). It
/// survives daemon restarts and is cleared when that wrapper reports its exit.
pub fn mark_pending_end(dir: &std::path::Path, id: &str, owners: &[u32]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let path = pending_end_path(dir, id)?;
    let parent = path.parent().context("missing pending End directory")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let temporary = parent.join(format!(".{id}.{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        for pid in owners {
            writeln!(file, "{pid}")?;
        }
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// The wrapper pids a journaled End belongs to, if one is pending.
pub fn pending_end(dir: &std::path::Path, id: &str) -> Option<Vec<u32>> {
    let contents = std::fs::read_to_string(pending_end_path(dir, id).ok()?).ok()?;
    Some(
        contents
            .lines()
            .filter_map(|pid| pid.trim().parse().ok())
            .collect(),
    )
}

pub fn clear_pending_end(dir: &std::path::Path, id: &str) {
    if let Ok(path) = pending_end_path(dir, id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Input payloads are transient; status exposes only observation counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    /// Host-owned native identity. Clients must not open the host's database
    /// to resolve it: its schema can advance while they remain running.
    #[serde(default)]
    pub native_session_id: Option<String>,
    /// Distinguishes an unbound native ID from a pre-identity host response.
    #[serde(default)]
    pub native_identity_supported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<conversations_core::private_channel::AgentReadiness>,
    #[serde(flatten)]
    pub registration: Registration,
    #[serde(default)]
    pub local_input_bytes: u64,
    #[serde(default)]
    pub local_input_dropped_chunks: u64,
}

/// What a wrapper did with one `CancelInteraction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    /// One Escape was sent as a best-effort interrupt. The session stays open;
    /// this does not confirm that running work stopped.
    Requested,
    /// A cancel was already requested moments ago; nothing more was sent.
    AlreadyRequested,
    /// The agent is between turns: nothing to interrupt, nothing was sent.
    Idle,
    /// The agent's turn state could not be read: nothing was sent.
    Unknown,
    /// This agent has no non-exiting cancel this host can use.
    Unsupported,
    /// The agent's native cancel was attempted and refused or not answered.
    Failed,
    /// The terminal writer stayed busy (a write blocked in the kernel).
    WriterBusy,
    /// The terminal's wrapper predates revocation enforcement: it was sent no
    /// remote effect and cannot be asked to cancel. Restart it to upgrade.
    UpgradeRequired,
}

impl CancelOutcome {
    /// What the outcome means for the person who revoked.
    pub fn describe(outcome: Option<Self>) -> &'static str {
        match outcome {
            Some(Self::Requested) => "interrupt requested",
            Some(Self::AlreadyRequested) => "interrupt already requested",
            Some(Self::Idle) => "idle, nothing to interrupt",
            Some(Self::Unknown) => "turn state unknown, not interrupted",
            Some(Self::Unsupported) => "interrupt unsupported for this agent",
            Some(Self::Failed) => "the agent's interrupt failed; not confirmed interrupted",
            Some(Self::WriterBusy) => {
                "terminal busy: one write admitted before the revocation may still complete; not interrupted"
            }
            Some(Self::UpgradeRequired) => {
                "terminal wrapper predates revocation enforcement; restart it to upgrade"
            }
            None => "terminal did not answer, interruption not confirmed",
        }
    }
}

/// One session's interruption after a local authority change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interruption {
    pub session: String,
    /// `None`: the session's terminal did not answer in time.
    pub outcome: Option<CancelOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub b64: String,
    pub kind: PtyFrameKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    LocalTransfer {
        session: String,
        transfer: String,
        content: TransferContent,
    },
    /// Commands admitted by the local owner socket, never the remote relay.
    LocalAction {
        action: LocalAction,
    },
    /// Commit an owner rename via the running daemon so the DB override and the
    /// cmux sidebar push share the same effective title and shared-workspace
    /// invariants as the signed `session.rename` path. Accepted only on the
    /// owner-only Unix socket; carries no authority of its own.
    OwnerRename {
        session: String,
        title: String,
    },
    /// Attach a local owner to a live terminal, without the remote relay.
    LocalTerminal {
        id: String,
    },
    /// Passkey administration. Accepted only from the owner-only host socket.
    Passkey {
        request: super::authz::passkey::PasskeyRequest,
    },
    PeerInputRequest {
        id: String,
        request_id: String,
        native_session: String,
        /// Id of an admitted signed message in the host's own inbox. The host
        /// derives the text from its record; a caller cannot supply text.
        message_id: String,
    },
    PeerInputResult {
        request_id: String,
        written: bool,
    },
    /// The wrapper's outcome for `ServerMessage::TransferContent`: whether it
    /// staged and pasted the content.
    TransferResult {
        transfer: String,
        status: conversations_core::terminal_files::TerminalFileStatus,
    },
    /// The wrapper's answer to `ServerMessage::CancelInteraction`.
    CancelResult {
        request_id: String,
        outcome: CancelOutcome,
    },
    /// Sent on the owner-only host socket after a local authority change was
    /// committed (`credential revoke`). It carries no authority: the host
    /// re-reads its own database and interrupts interactions whose authority no
    /// longer holds.
    AuthorityChanged,
    /// Same-machine handoff, accepted only from the owner-only host socket.
    Handoff {
        source: String,
        idempotency_key: String,
        brief: String,
        agent: Option<conversations_core::AgentKind>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        variant: Option<String>,
    },
    EndHandoff {
        source: String,
        session_id: String,
    },
    Register {
        registration: Registration,
    },
    Frame {
        frame: Frame,
    },
    LocalInput {
        b64: String,
    },
    LocalInputGap {
        chunks: u64,
    },
    Exit {
        code: u32,
        #[serde(default)]
        interrupted: Option<bool>,
    },
    Start {
        launch: Launch,
    },
    /// A descendant host takes ownership of this terminal; stop publishing it.
    Release {
        id: String,
    },
    /// Ask the running host for a fresh one-time owner link to its loopback
    /// web UI. Accepted only on the owner-only Unix socket; the link's token
    /// stays in its URL fragment and is handed to the local browser, never
    /// logged.
    WebBootstrap,
    /// Revoke browser access before issuing a fresh link. Owner socket only.
    WebBootstrapReset,
    Status,
}

/// The only commands accepted after a local owner terminal attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalTerminalInput {
    Repaint,
    Input { b64: String },
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalAction {
    End {
        session: String,
    },
    Start {
        session: String,
        agent: AgentKind,
        launcher: Option<conversations_core::AgentLauncher>,
    },
    Reopen {
        session: String,
    },
    Reply {
        session: String,
        text: String,
    },
}

/// Content of one transfer, as the wrapper's claim checks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransferContent {
    Image {
        mime: String,
        b64: String,
    },
    Files {
        files: Vec<conversations_core::terminal_files::TerminalFile>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    LocalTransferResult {
        status: conversations_core::private_channel::TransferStatus,
    },
    TerminalFrame {
        frame: Frame,
    },
    /// Result of a `ClientMessage::Passkey`; exactly one field is set.
    Passkey {
        response: Option<super::authz::passkey::PasskeyResponse>,
        error: Option<String>,
    },
    PeerInput {
        request_id: String,
        native_session: String,
        text: String,
        /// The sender's authority, re-checked before the body and before Enter.
        #[serde(default)]
        authority: Option<super::authz::cutover::Authority>,
    },
    PeerInputResult {
        request_id: String,
        written: bool,
    },
    /// The host verified a signed `session.paste_image` / `session.send_files`
    /// for THIS terminal. The wrapper applies a clipboard image or file drop only
    /// if the bytes it receives later match this exactly, once, before `ttl_ms`
    /// (counted against the absolute deadline the host fixed, which receipt cannot extend).
    /// The bytes of a transfer this terminal was sent `TransferAuthorized` for,
    /// received on the encrypted private channel and checked by the host
    /// against the signed sizes and hashes. The wrapper claims them against its
    /// own authorization (exact id and content, once, before it expires) before
    /// anything is staged.
    TransferContent {
        transfer: String,
        content: TransferContent,
    },
    TransferAuthorized {
        transfer: String,
        kind: super::transfers::TransferKind,
        expires_unix_ms: u64,
        /// Re-checked before the staged paste is written.
        #[serde(default)]
        authority: Option<super::authz::cutover::Authority>,
    },
    Input {
        b64: String,
        /// The signed authority this input was admitted under. When present the
        /// wrapper re-checks it under the terminal writer lock immediately
        /// before writing; absent = the local owner's own (unsigned) input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authority: Option<super::authz::cutover::Authority>,
    },
    /// Interrupt the CURRENT interaction of this terminal, keeping the session
    /// and its agent process alive. Never an exit, detach or signal.
    CancelInteraction {
        request_id: String,
        /// Native agent session id, to read the agent's turn state.
        native_session: Option<String>,
    },
    /// Answer to `ClientMessage::AuthorityChanged`: what happened per session.
    Interruptions {
        sessions: Vec<Interruption>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Repaint,
    End,
    /// Stop sharing this terminal while leaving its local child running.
    Detach,
    Started {
        id: String,
    },
    /// Owner rename committed to the DB, with any cmux sidebar push already
    /// attempted. `title` is the effective title `History::summary()` returns.
    OwnerRenameApplied {
        title: String,
    },
    Released,
    EndHandoffResult {
        status: String,
        host_notified: bool,
    },
    HandoffResult {
        session_id: String,
        state: String,
        error: Option<String>,
    },
    Status {
        backend: Backend,
        sessions: Vec<SessionStatus>,
        /// This host keeps [`Registration::in_place`] sessions where they
        /// started. Hosts that predate `start-agent` omit it.
        #[serde(default)]
        in_place_sessions: bool,
    },
    /// Answer to `ClientMessage::WebBootstrap`: the full one-time owner link,
    /// or `None` when this host is not serving the web UI (disabled, no shared
    /// frontend, or its port was already taken).
    WebBootstrap {
        url: Option<String>,
    },
    Error {
        message: String,
    },
}

/// Length framing puts a hard bound on untrusted local messages before allocation.
pub async fn read_message<R: AsyncRead + Unpin, T: DeserializeOwned>(reader: &mut R) -> Result<T> {
    let len = reader.read_u32().await? as usize;
    ensure!(len <= MAX_MESSAGE, "local host message exceeds limit");
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn write_message<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    ensure!(
        bytes.len() <= MAX_MESSAGE,
        "local host message exceeds limit"
    );
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        writer.write_u32(bytes.len() as u32).await?;
        writer.write_all(&bytes).await
    })
    .await??;
    Ok(())
}

/// Parser update, broadcast subscription and snapshot share a single lock. A
/// reconnect snapshot can never include bytes that are also queued as a delta.
pub struct Screen {
    parser: vt100::Parser,
    history: conversations_core::terminal_history::PtyEraseTracker,
    output: broadcast::Sender<Frame>,
}

pub type SharedScreen = Arc<Mutex<Screen>>;

impl Screen {
    pub fn new(cols: u16, rows: u16) -> SharedScreen {
        Arc::new(Mutex::new(Self {
            parser: vt100::Parser::new(rows, cols, 2000),
            history: Default::default(),
            output: broadcast::channel(128).0,
        }))
    }

    pub fn output(&mut self, bytes: &[u8]) {
        let mirrored = self.history.process(&mut self.parser, bytes);
        if self.history.replaced {
            self.repaint();
            return;
        }
        if mirrored.is_empty() {
            return;
        }
        let (rows, cols) = self.parser.screen().size();
        let _ = self.output.send(Frame {
            cols,
            rows,
            b64: STANDARD.encode(mirrored),
            kind: PtyFrameKind::Output,
        });
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.repaint();
    }

    pub fn finish(&mut self) {
        if !self.history.finish(&mut self.parser).is_empty() {
            self.repaint();
        }
    }

    pub fn repaint(&mut self) {
        let frame = self.snapshot();
        let _ = self.output.send(frame);
    }

    pub fn apply(&mut self, frame: Frame) -> Result<()> {
        ensure!(
            frame.cols > 0 && frame.rows > 0 && frame.cols <= 1000 && frame.rows <= 1000,
            "invalid terminal dimensions"
        );
        let bytes = STANDARD.decode(&frame.b64)?;
        if frame.kind == PtyFrameKind::Snapshot {
            self.parser = vt100::Parser::new(frame.rows, frame.cols, 2000);
            self.history = Default::default();
        } else {
            self.parser.screen_mut().set_size(frame.rows, frame.cols);
            // Older running wrappers still forward raw child bytes after a daemon upgrade.
            self.output(&bytes);
            return Ok(());
        }
        self.parser.screen_mut().set_size(frame.rows, frame.cols);
        self.parser.process(&bytes);
        let _ = self.output.send(frame);
        Ok(())
    }

    pub fn discovery_screens(&self) -> (vt100::Screen, Option<vt100::Screen>) {
        let screen = self.parser.screen();
        (
            screen.normal_screen(),
            screen.alternate_screen().then(|| screen.clone()),
        )
    }

    pub fn snapshot(&mut self) -> Frame {
        let (rows, cols) = self.parser.screen().size();
        Frame {
            cols,
            rows,
            b64: STANDARD.encode(crate::commands::live::interactive::mirror_replay_bytes(
                &mut self.parser,
            )),
            kind: PtyFrameKind::Snapshot,
        }
    }

    pub fn subscribe(&mut self) -> (Frame, broadcast::Receiver<Frame>) {
        (self.snapshot(), self.output.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternate_snapshot_restores_normal_screen_and_history_when_agent_exits() {
        let source = Screen::new(30, 4);
        let mut source = source.lock().unwrap();
        source.output(
            b"history0\r\nhistory1\r\nhistory2\r\nhistory3\r\nhistory4\r\nhistory5\x1b[?1049hADK",
        );
        let viewer = Screen::new(30, 4);
        let mut viewer = viewer.lock().unwrap();
        viewer.apply(source.snapshot()).unwrap();
        let (_, mut updates) = source.subscribe();
        source.output(b"\x1b[?1049l");
        viewer.apply(updates.try_recv().unwrap()).unwrap();
        assert_eq!(
            source.parser.screen().contents(),
            viewer.parser.screen().contents()
        );
        for screen in [&mut source.parser, &mut viewer.parser] {
            screen.screen_mut().set_scrollback(usize::MAX);
        }
        assert_eq!(
            source.parser.screen().contents(),
            viewer.parser.screen().contents()
        );
        assert!(viewer.parser.screen().contents().contains("history0"));
    }

    #[test]
    fn status_strip_redraw_after_snapshot_keeps_main_pane_and_scroll_region() {
        let source = Screen::new(30, 6);
        let mut source = source.lock().unwrap();
        source.output(b"\x1b[?1049h\x1b[HMAIN PANE\x1b[5;1Hstrip old\r\nquota old\x1b[5;6r");
        let viewer = Screen::new(30, 6);
        let mut viewer = viewer.lock().unwrap();
        viewer.apply(source.snapshot()).unwrap();
        let (_, mut updates) = source.subscribe();
        // tmux retains its strip-only scrolling region across periodic updates.
        source.output(b"\x1b[6;1H\r\nquota new\x1b[5;1Hstrip new\x1b[K");
        viewer.apply(updates.try_recv().unwrap()).unwrap();
        assert_eq!(
            source.parser.screen().contents(),
            viewer.parser.screen().contents()
        );
        assert!(viewer.parser.screen().alternate_screen());
        assert_eq!(
            viewer.parser.screen().rows(0, 30).next().unwrap(),
            "MAIN PANE"
        );
        source.resize(40, 8);
        source.output(b"\x1b[7;8r\x1b[7;1Hstrip resized\r\nquota resized");
        viewer.apply(source.snapshot()).unwrap();
        let (_, mut updates) = source.subscribe();
        source.output(b"\x1b[8;1H\r\nquota latest\x1b[7;1Hstrip latest\x1b[K");
        viewer.apply(updates.try_recv().unwrap()).unwrap();
        assert_eq!(
            source.parser.screen().contents(),
            viewer.parser.screen().contents()
        );
        assert_eq!(
            viewer.parser.screen().rows(0, 40).next().unwrap(),
            "MAIN PANE"
        );
    }

    #[tokio::test]
    async fn a_running_wrapper_is_alive_until_it_exits() {
        let dir = tempfile::tempdir().unwrap();
        let id = "5f0c1a52-7e0f-4c5b-9d7e-2a6f1c3b8e41";
        let held = hold_wrapper_liveness(dir.path(), id).unwrap();
        assert_eq!(
            wrapper_owners(dir.path(), id).await,
            Some(vec![std::process::id()])
        );
        assert!(
            wrapper_alive(dir.path(), id).await,
            "probing must not release it"
        );
        drop(held);
        // The released lock decides; no process listing is consulted. A
        // concurrent test's fork can share the descriptor until its exec.
        let mut alive = true;
        for _ in 0..50 {
            alive = wrapper_alive(dir.path(), id).await;
            if !alive {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!alive, "the wrapper has exited");
        assert!(!wrapper_alive(dir.path(), "../escape").await);
    }

    fn fake_ps(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let ps = dir.join("ps");
        std::fs::write(&ps, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&ps, std::fs::Permissions::from_mode(0o700)).unwrap();
        ps
    }

    #[tokio::test]
    async fn a_pre_lock_wrapper_is_found_through_the_process_listing() {
        let dir = tempfile::tempdir().unwrap();
        let id = "5f0c1a52-7e0f-4c5b-9d7e-2a6f1c3b8e41";
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let ps = fake_ps(
            dir.path(),
            &format!("echo '{uid} 4242 conversations host --runtime-dir /r wrap --session-id {id} --cwd /'"),
        );
        let timeout = std::time::Duration::from_secs(5);
        assert_eq!(
            unlocked_wrapper_pids(ps.as_os_str(), timeout, id).await,
            Some(vec![4242])
        );
        let other = "00000000-0000-4000-8000-000000000000";
        assert_eq!(
            unlocked_wrapper_pids(ps.as_os_str(), timeout, other).await,
            None
        );
        let failing = fake_ps(dir.path(), "exit 1");
        assert_eq!(
            unlocked_wrapper_pids(failing.as_os_str(), timeout, id).await,
            Some(vec![])
        );
        let missing = dir.path().join("missing-ps");
        assert_eq!(
            unlocked_wrapper_pids(missing.as_os_str(), timeout, id).await,
            Some(vec![])
        );
    }

    #[tokio::test]
    async fn a_stalled_process_listing_is_bounded_and_counts_as_running() {
        let dir = tempfile::tempdir().unwrap();
        let ps = fake_ps(dir.path(), "exec sleep 30");
        let started = std::time::Instant::now();
        let owners = unlocked_wrapper_pids(
            ps.as_os_str(),
            std::time::Duration::from_millis(200),
            "5f0c1a52-7e0f-4c5b-9d7e-2a6f1c3b8e41",
        )
        .await;
        assert_eq!(
            owners,
            Some(vec![]),
            "unknown ownership must not allow a replacement"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_wrapper_without_a_liveness_lock_is_found_by_its_launch_argv() {
        let id = "5f0c1a52-7e0f-4c5b-9d7e-2a6f1c3b8e41";
        let wrapper = format!(
            "  501  4242 /usr/local/bin/conversations host --runtime-dir /run/a b wrap --session-id {id} --cwd /repo --agent claude -- claude --session-id {id}"
        );
        assert_eq!(wrappers_in_process_list(&wrapper, "501", id), vec![4242]);
        assert!(
            wrappers_in_process_list(&wrapper, "502", id).is_empty(),
            "another user's process"
        );
        // The agent carries the same ID but is not a wrapper.
        let agent = format!("501 4243 claude --session-id {id}");
        assert!(wrappers_in_process_list(&agent, "501", id).is_empty());
        let other = "501 4244 conversations host wrap --session-id 00000000-0000-4000-8000-000000000000 --cwd /";
        assert!(wrappers_in_process_list(other, "501", id).is_empty());
    }

    #[test]
    fn a_journaled_end_names_its_wrapper_until_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let id = "5f0c1a52-7e0f-4c5b-9d7e-2a6f1c3b8e41";
        assert_eq!(pending_end(dir.path(), id), None);
        mark_pending_end(dir.path(), id, &[4242]).unwrap();
        assert_eq!(pending_end(dir.path(), id), Some(vec![4242]));
        // Delivery does not consume it; only the wrapper's exit does.
        assert_eq!(pending_end(dir.path(), id), Some(vec![4242]));
        mark_pending_end(dir.path(), id, &[]).unwrap();
        assert_eq!(pending_end(dir.path(), id), Some(vec![]));
        clear_pending_end(dir.path(), id);
        assert_eq!(pending_end(dir.path(), id), None);
        assert!(mark_pending_end(dir.path(), "../escape", &[]).is_err());
    }

    fn retained_text(screen: &mut Screen) -> String {
        let snapshot = screen.snapshot();
        let mut replay = vt100::Parser::new(snapshot.rows, snapshot.cols, 2000);
        replay.process(&STANDARD.decode(snapshot.b64).unwrap());
        String::from_utf8_lossy(&crate::commands::live::interactive::mirror_replay_bytes(
            &mut replay,
        ))
        .into_owned()
    }

    #[test]
    fn daemon_preserves_raw_redraw_from_running_older_wrapper() {
        let screen = Screen::new(80, 6);
        let mut screen = screen.lock().unwrap();
        for bytes in [
            b"opening question\r\nfirst answer".as_slice(),
            conversations_core::terminal::CODEX_TRANSCRIPT_REBUILD,
            b"\x1b[?2026hlater answer\x1b[?2026l".as_slice(),
        ] {
            screen
                .apply(Frame {
                    cols: 80,
                    rows: 6,
                    b64: STANDARD.encode(bytes),
                    kind: PtyFrameKind::Output,
                })
                .unwrap();
        }
        let text = retained_text(&mut screen);
        for expected in ["opening question", "first answer", "later answer"] {
            assert_eq!(text.matches(expected).count(), 1, "{text:?}");
        }
    }

    #[test]
    fn forwarded_redraw_preserves_history_through_daemon_and_reconnect() {
        let wrapper = Screen::new(80, 6);
        let daemon = Screen::new(80, 6);
        let mut wrapper = wrapper.lock().unwrap();
        let mut daemon = daemon.lock().unwrap();
        wrapper.output(b"opening question\r\nfirst answer");
        let (snapshot, mut output) = wrapper.subscribe();
        daemon.apply(snapshot).unwrap();
        let redraw = conversations_core::terminal::CODEX_TRANSCRIPT_REBUILD;
        let split = redraw.len() - 3;
        wrapper.output(&redraw[..split]);
        wrapper.output(&redraw[split..]);
        wrapper.output(b"\x1b[?2026hlater answer\x1b[?2026l");
        while let Ok(frame) = output.try_recv() {
            daemon.apply(frame).unwrap();
        }
        let text = retained_text(&mut daemon);
        for expected in ["opening question", "first answer", "later answer"] {
            assert_eq!(text.matches(expected).count(), 1, "{text:?}");
        }
    }

    #[test]
    fn eof_flushes_interrupted_redraw_without_losing_opening() {
        let screen = Screen::new(80, 6);
        let mut screen = screen.lock().unwrap();
        screen.output(b"opening question");
        screen.output(conversations_core::terminal::CODEX_TRANSCRIPT_REBUILD);
        screen.output(b"\x1b[?2026hfinal answer");
        screen.finish();
        let text = retained_text(&mut screen);
        assert!(text.contains("opening question"), "{text:?}");
        assert!(text.contains("final answer"), "{text:?}");
    }

    #[test]
    fn reconnect_preserves_opening_after_codex_redraw() {
        let screen = Screen::new(80, 6);
        let mut screen = screen.lock().unwrap();
        screen.output(b"opening question\r\nfirst answer");
        screen.output(conversations_core::terminal::CODEX_TRANSCRIPT_REBUILD);
        screen.output(b"\x1b[?2026hlater question\r\nlater answer\x1b[?2026l");
        let snapshot = screen.snapshot();
        let mut replay = vt100::Parser::new(6, 80, 2000);
        replay.process(&STANDARD.decode(snapshot.b64).unwrap());
        let history = String::from_utf8_lossy(
            &crate::commands::live::interactive::mirror_replay_bytes(&mut replay),
        )
        .into_owned();
        for text in [
            "opening question",
            "first answer",
            "later question",
            "later answer",
        ] {
            assert!(history.contains(text), "missing {text}: {history:?}");
        }
    }

    #[test]
    fn reconnect_snapshot_and_following_output_do_not_duplicate_bytes() {
        let screen = Screen::new(80, 24);
        let mut screen = screen.lock().unwrap();
        screen.output(b"before");
        let (snapshot, mut stream) = screen.subscribe();
        screen.output(b"after");
        let mut reconstructed = vt100::Parser::new(24, 80, 100);
        reconstructed.process(&STANDARD.decode(snapshot.b64).unwrap());
        reconstructed.process(&STANDARD.decode(stream.try_recv().unwrap().b64).unwrap());
        assert_eq!(reconstructed.screen().contents(), "beforeafter");
        assert!(stream.try_recv().is_err());
    }
}
