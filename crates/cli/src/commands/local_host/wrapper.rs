use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use crate::commands::live::{interactive::Shutdown, terminal_files::FileStage};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::{broadcast, mpsc, watch};

use super::authz::cutover::{Authority, Gate};
use super::protocol::{
    hold_wrapper_liveness, read_message, release_marker, reopen_marker, write_message,
    ClientMessage, Launch, Registration, Screen, ServerMessage, SharedScreen,
};

// Deterministic race seam: hold one admitted write under the real writer lock.
// No production build installs or executes this barrier.
#[cfg(test)]
pub(super) mod writer_barrier {
    use super::*;
    use std::sync::Condvar;

    static ACTIVE: Mutex<Option<(PathBuf, Arc<State>)>> = Mutex::new(None);

    struct State {
        first: AtomicBool,
        entered: tokio::sync::Semaphore,
        queued: tokio::sync::Semaphore,
        released: Mutex<bool>,
        wake: Condvar,
    }

    pub struct Barrier(Arc<State>);

    impl Barrier {
        pub fn install(dir: &Path) -> Self {
            let state = Arc::new(State {
                first: AtomicBool::new(true),
                entered: tokio::sync::Semaphore::new(0),
                queued: tokio::sync::Semaphore::new(0),
                released: Mutex::new(false),
                wake: Condvar::new(),
            });
            let mut active = ACTIVE.lock().unwrap();
            assert!(active.is_none(), "writer barrier already installed");
            *active = Some((dir.to_owned(), state.clone()));
            Self(state)
        }

        pub async fn entered(&self) {
            tokio::time::timeout(Duration::from_secs(5), self.0.entered.acquire())
                .await
                .expect("writer did not reach final effect")
                .unwrap()
                .forget();
        }

        pub async fn queued(&self, count: u32) {
            tokio::time::timeout(Duration::from_secs(5), self.0.queued.acquire_many(count))
                .await
                .expect("input did not reach wrapper queue")
                .unwrap()
                .forget();
        }

        pub fn release(&self) {
            *self.0.released.lock().unwrap() = true;
            self.0.wake.notify_all();
        }
    }

    impl Drop for Barrier {
        fn drop(&mut self) {
            self.release();
            *ACTIVE.lock().unwrap() = None;
        }
    }

    fn state(dir: &Path) -> Option<Arc<State>> {
        ACTIVE
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(path, _)| path == dir)
            .map(|(_, state)| state.clone())
    }

    pub fn admitted(dir: &Path) {
        if let Some(state) = state(dir) {
            if state.first.swap(false, Ordering::SeqCst) {
                state.entered.add_permits(1);
                let released = state.released.lock().unwrap();
                drop(
                    state
                        .wake
                        .wait_while(released, |released| !*released)
                        .unwrap(),
                );
            }
        }
    }

    pub fn queued(dir: &Path) {
        if let Some(state) = state(dir) {
            state.queued.add_permits(1);
        }
    }
}

#[derive(Default)]
struct ComposerState {
    dirty: bool,
    paste: bool,
    escape: Vec<u8>,
}
impl ComposerState {
    fn unknown() -> Self {
        Self {
            dirty: true,
            ..Self::default()
        }
    }
    fn clean(&self) -> bool {
        !self.dirty && !self.paste && self.escape.is_empty()
    }
    fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if !self.escape.is_empty() {
                self.escape.push(byte);
                if self.escape.len() == 2 && byte != b'[' {
                    self.dirty = true;
                    self.escape.clear();
                } else if self.escape.len() >= 3 && (0x40..=0x7e).contains(&byte) {
                    match self.escape.as_slice() {
                        b"\x1b[200~" => {
                            self.paste = true;
                            self.dirty = true;
                        }
                        b"\x1b[201~" => {
                            self.paste = false;
                            self.dirty = true;
                        }
                        value
                            if byte == b'R'
                                && value[2..value.len() - 1]
                                    .iter()
                                    .all(|b| b.is_ascii_digit() || *b == b';') => {}
                        _ => self.dirty = true,
                    }
                    self.escape.clear();
                } else if self.escape.len() > 64 {
                    self.dirty = true;
                    self.escape.clear();
                }
            } else if byte == 0x1b {
                self.escape.push(byte);
            } else {
                self.dirty = byte != b'\r' || self.paste;
            }
        }
    }
}

struct GuardedWriter {
    writer: Box<dyn Write + Send>,
    composer: ComposerState,
    last_peer_turn: Option<(String, u64, std::time::SystemTime)>,
}
impl Write for GuardedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.writer.write(bytes)?;
        self.composer.observe(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

enum RemoteInput {
    /// Raw input and, when it was signed, the authority it was admitted under.
    Human(Vec<u8>, Option<Authority>),
    Peer {
        request_id: String,
        native_session: String,
        text: String,
        authority: Option<Authority>,
    },
}

/// The interaction-cancel boundary. Separate from the session lifecycle: it
/// never ends, detaches or signals anything. At most one cancel runs at a time,
/// and each terminal gets at most one Escape per [`CANCEL_INTERVAL`] (a second
/// Escape can open a rewind menu).
#[derive(Default)]
struct CancelState {
    running: AtomicBool,
    last_escape: Mutex<Option<std::time::Instant>>,
}

const CANCEL_INTERVAL: Duration = Duration::from_secs(3);
/// How long a cancel waits for an in-progress terminal write to finish. A
/// write blocked in the kernel is reported, not waited for.
const CANCEL_DRAIN: Duration = Duration::from_secs(1);

fn cancel_interaction(
    state: &Arc<CancelState>,
    supported: bool,
    writer: &Arc<Mutex<GuardedWriter>>,
    request_id: String,
    results: &broadcast::Sender<ClientMessage>,
) {
    use super::protocol::CancelOutcome;
    let reply = {
        let results = results.clone();
        move |outcome| {
            let _ = results.send(ClientMessage::CancelResult {
                request_id,
                outcome,
            });
        }
    };
    if state.running.swap(true, Ordering::SeqCst) {
        reply(CancelOutcome::AlreadyRequested);
        return;
    }
    let (state, writer) = (state.clone(), writer.clone());
    std::thread::spawn(move || {
        let outcome = (|| {
            // Drain: once the writer is ours, no write admitted under the old
            // authority is still in progress, and later ones re-check it.
            let started = std::time::Instant::now();
            let mut writer = loop {
                match writer.try_lock() {
                    Ok(writer) => break writer,
                    Err(std::sync::TryLockError::Poisoned(p)) => break p.into_inner(),
                    Err(std::sync::TryLockError::WouldBlock) => {
                        if started.elapsed() >= CANCEL_DRAIN {
                            return CancelOutcome::WriterBusy;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            };
            if !supported {
                return CancelOutcome::Unsupported;
            }
            let mut last = state.last_escape.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|at| at.elapsed() < CANCEL_INTERVAL) {
                return CancelOutcome::AlreadyRequested;
            }
            // Best effort only: the TUI may handle Escape differently depending
            // on focus. Never claim the turn stopped or terminate the session.
            // This synthetic key is not draft input: preserve composer tracking,
            // including any pre-existing draft, paste, or incomplete escape.
            if writer
                .writer
                .write_all(b"\x1b")
                .and_then(|_| writer.writer.flush())
                .is_err()
            {
                return CancelOutcome::WriterBusy;
            }
            *last = Some(std::time::Instant::now());
            CancelOutcome::Requested
        })();
        state.running.store(false, Ordering::SeqCst);
        reply(outcome);
    });
}

/// Apply the bytes of one transfer the host received on the private channel:
/// only against this terminal's own authorization for exactly this id and
/// content (consumed once, before it expires), then staged privately and
/// pasted by path like a file drop. `None`: nothing to apply.
fn apply_content<W: Write + Send + 'static>(
    table: &Arc<super::transfers::TransferTable>,
    stage: &Arc<Mutex<FileStage>>,
    writer: &Arc<Mutex<W>>,
    shutdown: &Arc<Shutdown>,
    transfer: String,
    content: super::protocol::TransferContent,
    results: &broadcast::Sender<ClientMessage>,
) -> Option<tokio::task::JoinHandle<()>> {
    use super::protocol::TransferContent;
    use crate::commands::live::terminal_files::{apply_files, stage_and_paste_image, Deadline};
    use conversations_core::terminal_files::TerminalFileStatus;
    let (stage, writer, shutdown) = (stage.clone(), writer.clone(), shutdown.clone());
    let report = {
        let (results, transfer) = (results.clone(), transfer.clone());
        move |status| {
            let _ = results.send(ClientMessage::TransferResult { transfer, status });
        }
    };
    match content {
        TransferContent::Image { mime, b64 } => {
            let claim = STANDARD
                .decode(b64)
                .ok()
                .and_then(|bytes| Some((table.claim_image(&transfer, &mime, &bytes)?, bytes)));
            let Some((claim, bytes)) = claim else {
                eprintln!("Conversations transfer content refused: no matching authorization");
                report(TerminalFileStatus::Failed);
                return None;
            };
            Some(tokio::spawn(async move {
                transfer_hook::hold("stage").await;
                let status = stage_and_paste_image(
                    &stage,
                    &writer,
                    &bytes,
                    &mime,
                    Deadline::from_claim(claim),
                    &shutdown,
                )
                .await;
                transfer_hook::hold("receipt").await;
                report(status);
            }))
        }
        TransferContent::Files { files } => {
            let Some(claim) = table.claim_files(&transfer, &files) else {
                eprintln!("Conversations transfer content refused: no matching authorization");
                report(TerminalFileStatus::Failed);
                return None;
            };
            Some(tokio::spawn(async move {
                transfer_hook::hold("stage").await;
                let status = apply_files(&stage, &writer, files, claim, &shutdown).await;
                transfer_hook::hold("receipt").await;
                report(status);
            }))
        }
    }
}

/// Debug builds only: a test holds a claimed transfer just before it is staged
/// (`stage`) or just before its receipt is sent (`receipt`), by creating
/// `<name>-hold` in the `CONVERSATIONS_TEST_DELIVERY_BARRIER` directory; this
/// writes `<name>-held` and waits (at most 30 s) for `<name>-release`. It only
/// delays: every check the transfer makes still runs.
mod transfer_hook {
    #[cfg(debug_assertions)]
    pub(super) async fn hold(name: &str) {
        let Some(dir) = std::env::var_os("CONVERSATIONS_TEST_DELIVERY_BARRIER") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        if !dir.join(format!("{name}-hold")).exists() {
            return;
        }
        let _ = std::fs::write(dir.join(format!("{name}-held")), b"");
        let release = dir.join(format!("{name}-release"));
        for _ in 0..600 {
            if std::fs::remove_file(&release).is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = std::fs::remove_file(dir.join(format!("{name}-held")));
    }

    #[cfg(not(debug_assertions))]
    pub(super) async fn hold(_name: &str) {}
}

fn queue_peer_input(
    queue: &std::sync::mpsc::SyncSender<RemoteInput>,
    results: &broadcast::Sender<ClientMessage>,
    request_id: String,
    native_session: String,
    text: String,
    authority: Option<Authority>,
) {
    if let Err(error) = queue.try_send(RemoteInput::Peer {
        request_id,
        native_session,
        text,
        authority,
    }) {
        let input = match error {
            std::sync::mpsc::TrySendError::Full(input)
            | std::sync::mpsc::TrySendError::Disconnected(input) => input,
        };
        if let RemoteInput::Peer { request_id, .. } = input {
            let _ = results.send(ClientMessage::PeerInputResult {
                request_id,
                written: false,
            });
        }
    }
}

fn fresh_claude(launch: &Launch) -> bool {
    launch.agent == conversations_core::AgentKind::Claude
        && launch
            .command
            .first()
            .and_then(|command| Path::new(command).file_name())
            .is_some_and(|name| name == "claude" || name == "claude.exe")
        && !launch.command.iter().any(|arg| {
            matches!(arg.as_str(), "--resume" | "-r" | "--continue" | "-c")
                || arg.starts_with("--resume=")
        })
        && (launch
            .command
            .windows(2)
            .any(|pair| pair[0] == "--session-id" && pair[1] == launch.id)
            || launch
                .command
                .iter()
                .any(|arg| arg == &format!("--session-id={}", launch.id)))
}

pub(crate) async fn peer_input(
    runtime: &Path,
    source: &str,
    native: &str,
    message_id: &str,
) -> Result<bool> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut socket = match tokio::net::UnixStream::connect(runtime.join("host.sock")).await {
        Ok(socket) => socket,
        Err(_) => return Ok(false), // No request could have reached a wrapper.
    };
    write_message(
        &mut socket,
        &ClientMessage::PeerInputRequest {
            id: source.into(),
            request_id: request_id.clone(),
            native_session: native.into(),
            message_id: message_id.into(),
        },
    )
    .await?;
    let response: ServerMessage =
        tokio::time::timeout(Duration::from_secs(15), read_message(&mut socket)).await??;
    match response {
        ServerMessage::PeerInputResult {
            request_id: returned,
            written,
        } if returned == request_id => Ok(written),
        ServerMessage::Error { message } => anyhow::bail!("{message}"),
        _ => anyhow::bail!("updated Conversations host/wrapper required for guarded input"),
    }
}

struct RawGuard(bool);
impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Own the child process group even on errors. portable-pty creates a new session.
struct ChildGuard(Option<u32>);
impl ChildGuard {
    fn hangup(&self) {
        if let Some(pid) = self.0 {
            // SAFETY: the child is still owned by this wrapper.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGHUP);
            }
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: the process group belongs to the child spawned by this wrapper.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

pub async fn run(dir: &Path, launch: Launch, headless: bool, server: &str) -> Result<u32> {
    run_with(dir, launch, headless, false, server).await
}

/// `start-agent`: wrap `launch` in this terminal and keep it in place there.
pub async fn run_in_place(dir: &Path, launch: Launch, server: &str) -> Result<u32> {
    run_with(dir, launch, false, true, server).await
}

async fn run_with(
    dir: &Path,
    launch: Launch,
    headless: bool,
    in_place: bool,
    server: &str,
) -> Result<u32> {
    let cleanup =
        (launch.agent == conversations_core::AgentKind::OpenCode).then(|| launch.id.clone());
    ensure!(!launch.command.is_empty(), "missing wrapped command");
    ensure!(
        std::env::var_os("CONVERSATIONS_WRAPPED").is_none(),
        "this terminal is already wrapped"
    );
    let local = !headless && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    ensure!(
        headless || local,
        "host wrap needs a terminal (or --headless)"
    );
    let _liveness = hold_wrapper_liveness(dir, &launch.id)?;
    let mut local_size = if local {
        crossterm::terminal::size()
            .ok()
            .filter(|(cols, rows)| valid_size(*cols, *rows))
    } else {
        None
    };
    let (mut cols, mut rows) = local_size.unwrap_or((80, 24));
    let mut browser_size_override = false;
    let size_path = dir
        .join("terminal-sizes")
        .join(format!("{}.json", launch.id));
    if local && std::env::var_os("CMUX_SURFACE_ID").is_some() {
        let background = async {
            let cmux = super::cmux::Cmux::connect(None).await?;
            cmux.is_background_surface(&std::env::var("CMUX_SURFACE_ID")?)
                .await
        }
        .await
        .unwrap_or(false);
        if background {
            if let Some(saved) = read_saved_size(&size_path) {
                (cols, rows) = saved;
            }
        } else {
            save_size(&size_path, cols, rows);
        }
    }
    let pair = native_pty_system().openpty(size(cols, rows))?;
    // Fresh Codex sessions need cmux's SessionStart hook to publish their native
    // identity. Keep resumed sessions on the user's original launch path:
    // injecting cmux's additional config can move them out of the shared Codex
    // daemon and leave their existing thread writer locked in that daemon.
    let mut command = agent_command(&launch);
    let mut args = launch.command[1..].to_vec();
    let _codex_binding = super::codex::prepare(&launch, &mut args, server)?;
    super::super::claude_channel::prepare(&launch.command[0], &mut args, server)?;
    super::readiness::prepare_claude(dir, &launch, &mut args)?;
    if let Some(endpoint) = _codex_binding
        .as_ref()
        .and_then(|binding| binding.endpoint.as_ref())
    {
        command.env("CONVERSATIONS_CODEX_SOCKET", endpoint);
    }
    command.args(&args);
    command.cwd(&launch.cwd);
    command.env("CONVERSATIONS_WRAPPED", &launch.id);
    command.env("CONVERSATIONS_SERVER", server);
    command.env("CONVERSATIONS_CLI", std::env::current_exe()?);
    command.env("CONVERSATIONS_HOST_DIR", dir);
    command.env("CONVERSATIONS_WRAPPER_PID", std::process::id().to_string());
    command.env_remove("COLUMNS");
    command.env_remove("LINES");
    // A cmux command shell may already have consumed its ZDOTDIR bootstrap.
    // Reinstall the integration for the actual wrapped zsh, preserving the
    // user's startup directory and the recursion guard above.
    if Path::new(&launch.command[0])
        .file_name()
        .is_some_and(|name| name == "zsh")
    {
        if let Some(integration) = std::env::var_os("CMUX_SHELL_INTEGRATION_DIR") {
            let integration = std::path::PathBuf::from(integration);
            if integration.join(".zshenv").is_file() {
                let original = std::env::var_os("ZDOTDIR")
                    .or_else(|| std::env::var_os("HOME"))
                    .unwrap_or_default();
                command.env("CMUX_ZSH_ZDOTDIR", original);
                command.env_remove("GHOSTTY_ZSH_ZDOTDIR");
                command.env("ZDOTDIR", integration);
            }
        }
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .context("starting wrapped command")?;
    let child_guard = ChildGuard(child.process_id());
    drop(pair.slave);
    let master = pair.master;
    let writer = Arc::new(Mutex::new(GuardedWriter {
        writer: master.take_writer()?,
        last_peer_turn: None,
        composer: if fresh_claude(&launch) {
            ComposerState::default()
        } else {
            ComposerState::unknown()
        },
    }));
    let (peer_results_tx, peer_results) = broadcast::channel::<ClientMessage>(16);
    // Effect-time authority: every signed write is re-checked against the
    // host's current local rows under the writer lock, right before it happens.
    let gate = Arc::new(Gate::new(dir));
    let cancel_state = Arc::new(CancelState::default());
    let supports_cancel = matches!(
        launch.agent,
        conversations_core::AgentKind::Claude
            | conversations_core::AgentKind::Codex
            | conversations_core::AgentKind::OpenCode
    );
    // A child that stops reading must not block End, exit checks, or signals.
    // This queue preserves browser input order while the writer owns blocking I/O.
    let (remote_input_tx, remote_input_rx) = std::sync::mpsc::sync_channel::<RemoteInput>(8);
    {
        let writer = writer.clone();
        let peer_results_tx = peer_results_tx.clone();
        let gate = gate.clone();
        #[cfg(test)]
        let writer_test_dir = dir.to_owned();
        std::thread::spawn(move || {
            while let Ok(input) = remote_input_rx.recv() {
                let mut writer = writer.lock().unwrap();
                match input {
                    RemoteInput::Human(bytes, authority) => {
                        if let Some(reason) = gate.denial(authority.as_ref()) {
                            eprintln!("Conversations remote input withheld: {reason}");
                            continue;
                        }
                        #[cfg(test)]
                        writer_barrier::admitted(&writer_test_dir);
                        if writer.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                    RemoteInput::Peer {
                        request_id,
                        native_session,
                        text,
                        authority,
                    } => {
                        let clean = writer.composer.clean();
                        let turn =
                            super::super::conversations_delivery::claude_turn(&native_session)
                                .ok()
                                .flatten();
                        let idle = turn.is_some_and(|(length, modified)| {
                            writer.last_peer_turn.as_ref()
                                != Some(&(native_session.clone(), length, modified))
                        });
                        if !clean || !idle {
                            let _ = peer_results_tx.send(ClientMessage::PeerInputResult {
                                request_id,
                                written: false,
                            });
                            continue;
                        }
                        let body = super::super::live::interactive::sanitize_reply(&text);
                        if body.is_empty()
                            || body.chars().count() > 4096
                            || text.chars().count() > 4096
                        {
                            let _ = peer_results_tx.send(ClientMessage::PeerInputResult {
                                request_id,
                                written: false,
                            });
                            continue;
                        }
                        if gate.denial(authority.as_ref()).is_some() {
                            let _ = peer_results_tx.send(ClientMessage::PeerInputResult {
                                request_id,
                                written: false,
                            });
                            continue;
                        }
                        // Keep both writes and the native paste/Enter separation under
                        // one lock. Human input cannot append between body and submit.
                        if writer
                            .write_all(body.as_bytes())
                            .and_then(|_| writer.flush())
                            .is_err()
                        {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(120));
                        // Re-checked before the submit: a revocation during the
                        // pause leaves the typed body unsubmitted.
                        if let Some(reason) = gate.denial(authority.as_ref()) {
                            eprintln!("Conversations peer message not submitted: {reason}");
                            let _ = peer_results_tx.send(ClientMessage::PeerInputResult {
                                request_id,
                                written: false,
                            });
                            continue;
                        }
                        if writer
                            .write_all(b"\r")
                            .and_then(|_| writer.flush())
                            .is_err()
                        {
                            break;
                        }
                        if let Some((length, modified)) = turn {
                            writer.last_peer_turn = Some((native_session, length, modified));
                        }
                        let _ = peer_results_tx.send(ClientMessage::PeerInputResult {
                            request_id,
                            written: true,
                        });
                    }
                }
            }
        });
    }
    let mut reader = master.try_clone_reader()?;
    let screen = Screen::new(cols, rows);
    let running = Arc::new(AtomicBool::new(true));
    if local {
        crossterm::terminal::enable_raw_mode()?;
    }
    let _raw = RawGuard(local);

    let screen_out = screen.clone();
    let output = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // No network I/O or queue backpressure under this lock.
                    screen_out.lock().unwrap().output(&buf[..n]);
                    if local {
                        let mut stdout = std::io::stdout().lock();
                        if stdout
                            .write_all(&buf[..n])
                            .and_then(|_| stdout.flush())
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
        screen_out.lock().unwrap().finish();
    });
    // At most 128 chunks (1 MiB) retained in RAM during daemon outages.
    // Sending never blocks the local terminal; lag is reported explicitly.
    let (local_input_tx, local_input) = broadcast::channel::<Vec<u8>>(128);
    let input = if local {
        let running = running.clone();
        let writer = writer.clone();
        Some(std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while running.load(Ordering::Relaxed) {
                let mut fd = libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: fd points to one valid pollfd; stdin is owned by this process.
                let ready = unsafe { libc::poll(&mut fd, 1, 100) };
                if ready <= 0 {
                    continue;
                }
                if fd.revents & libc::POLLIN == 0 {
                    break;
                }
                match std::io::stdin().read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut writer = writer.lock().unwrap();
                        if writer.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = local_input_tx.send(buf[..n].to_vec());
                    }
                }
            }
        }))
    } else {
        None
    };
    let registration = Registration {
        launch,
        workspace: std::env::var("CMUX_WORKSPACE_ID").ok(),
        surface: std::env::var("CMUX_SURFACE_ID").ok(),
        local_terminal: local,
        reconnecting: false,
        file_receiver: true,
        transfer_gate: true,
        effect_gate: true,
        in_place,
    };
    let marker = release_marker(dir, &registration.launch.id, std::process::id());
    let transfer_table = Arc::new(super::transfers::TransferTable::gated(gate.clone()));
    let file_stage = Arc::new(Mutex::new(FileStage::new()));
    // Content the host received on the private channel; stopped at exit.
    let content_shutdown = Shutdown::new();
    let mut applying: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let (commands_tx, mut commands) = mpsc::channel(64);
    let (exited_tx, exited) = watch::channel(None);
    let network = tokio::spawn(connect_loop(
        dir.to_path_buf(),
        registration,
        screen.clone(),
        commands_tx,
        exited,
        local_input,
        peer_results,
    ));
    let mut resize = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut remote_input_lagged = false;
    let (code, interrupted) = loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Some(status) = child.try_wait()? { break (status.exit_code(), false); }
            }
            _ = resize.recv(), if local => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    if valid_size(cols, rows)
                        && (!browser_size_override || local_size != Some((cols, rows)))
                    {
                        local_size = Some((cols, rows));
                        browser_size_override = false;
                        master.resize(size(cols, rows))?;
                        screen.lock().unwrap().resize(cols, rows);
                        if std::env::var_os("CMUX_SURFACE_ID").is_some() {
                            save_size(&size_path, cols, rows);
                        }
                    }
                }
            }
            _ = hangup.recv() => break (129, true),
            _ = terminate.recv() => break (143, true),
            _ = interrupt.recv() => break (130, false),
            Some(command) = commands.recv() => match command {
                ServerMessage::TransferContent { transfer, content } => {
                    applying.retain(|task| !task.is_finished());
                    if let Some(task) = apply_content(&transfer_table, &file_stage, &writer, &content_shutdown, transfer, content, &peer_results_tx) {
                        applying.push(task);
                    }
                }
                ServerMessage::TransferAuthorized { transfer, kind, expires_unix_ms, authority } => {
                    if let Err(reason) = transfer_table.authorize_as(&transfer, kind, expires_unix_ms, authority) {
                        eprintln!("Conversations transfer refused: {reason}");
                    }
                }
                ServerMessage::Detach => {}
                ServerMessage::PeerInput { request_id, native_session, text, authority } => {
                    queue_peer_input(&remote_input_tx, &peer_results_tx, request_id, native_session, text, authority);
                }
                ServerMessage::CancelInteraction { request_id, .. } => {
                    cancel_interaction(&cancel_state, supports_cancel, &writer, request_id, &peer_results_tx);
                }
                ServerMessage::Input { b64, authority } => {
                    if let Ok(bytes) = STANDARD.decode(b64) {
                        match remote_input_tx.try_send(RemoteInput::Human(bytes, authority)) {
                            Ok(()) => {
                                remote_input_lagged = false;
                                #[cfg(test)]
                                writer_barrier::queued(dir);
                            }
                            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                if !remote_input_lagged {
                                    eprintln!("Conversations remote input dropped: terminal is not reading");
                                    remote_input_lagged = true;
                                }
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
                        }
                    }
                }
                ServerMessage::Resize { cols, rows } if valid_size(cols, rows) => {
                    browser_size_override = local;
                    master.resize(size(cols, rows))?;
                    screen.lock().unwrap().resize(cols, rows);
                }
                ServerMessage::End => break (0, false),
                ServerMessage::Repaint => screen.lock().unwrap().repaint(),
                _ => {}
            }
        }
    };
    running.store(false, Ordering::Relaxed);
    content_shutdown.trigger();
    // Give shells and agents a short chance to save history and flush state.
    child_guard.hangup();
    for _ in 0..15 {
        if child.try_wait()?.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The guard also stops descendants still in the process group.
    drop(child_guard);
    let child_exited = child.wait().is_ok();
    for mut task in applying {
        if tokio::time::timeout(Duration::from_secs(2), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
    if child_exited {
        if let Some(id) = cleanup {
            if super::opencode_bridge::cleanup_after_exit(dir, &id).is_err() {
                eprintln!("CONVERSATIONS_BRIDGE_CLEANUP: Cannot remove private launch config. Check the host runtime directory permissions.");
            }
        }
    }
    for _ in 0..20 {
        if output.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if output.is_finished() {
        let _ = output.join();
    }
    if let Some(input) = input {
        let _ = input.join();
    }
    screen.lock().unwrap().finish();
    exited_tx.send_replace(Some((code, interrupted)));
    let mut network = network;
    if tokio::time::timeout(Duration::from_secs(4), &mut network)
        .await
        .is_err()
    {
        network.abort();
    }
    let _ = std::fs::remove_file(marker);
    drop(file_stage);
    Ok(code)
}

fn agent_command(launch: &Launch) -> CommandBuilder {
    let program = Path::new(&launch.command[0]);
    let name = program.file_name().and_then(|name| name.to_str());
    let shim_backend = match name {
        Some("codex") if !launch.command.iter().any(|arg| arg == "resume") => {
            Some("CMUX_CUSTOM_CODEX_PATH")
        }
        Some("claude") => Some("CMUX_CUSTOM_CLAUDE_PATH"),
        _ => None,
    };
    let shim = shim_backend
        .and_then(|_| std::env::var_os("CMUX_AGENT_COMMAND_SHIM_ROOT"))
        .map(PathBuf::from)
        .map(|root| root.join(name.unwrap()))
        .filter(|path| path.is_file());
    let mut command = CommandBuilder::new(shim.as_deref().unwrap_or(program));
    if shim.is_some() && program.is_absolute() {
        command.env(shim_backend.unwrap(), program);
    }
    command
}

fn read_saved_size(path: &Path) -> Option<(u16, u16)> {
    let size: (u16, u16) = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    valid_size(size.0, size.1).then_some(size)
}

pub(super) fn save_size(path: &Path, cols: u16, rows: u16) {
    if !valid_size(cols, rows) {
        return;
    }
    let result = (|| -> Result<()> {
        let parent = path.parent().context("missing terminal size directory")?;
        std::fs::create_dir_all(parent)?;
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temporary, serde_json::to_vec(&(cols, rows))?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("Cannot save terminal size: {error:#}");
    }
}

fn valid_size(cols: u16, rows: u16) -> bool {
    cols > 0 && rows > 0 && cols <= 1000 && rows <= 1000
}
fn size(cols: u16, rows: u16) -> PtySize {
    size_with_window(cols, rows, crossterm::terminal::window_size().ok())
}

fn size_with_window(
    cols: u16,
    rows: u16,
    window: Option<crossterm::terminal::WindowSize>,
) -> PtySize {
    let scale = |pixels: u16, physical_cells: u16, virtual_cells: u16| {
        if physical_cells == 0 {
            return 0;
        }
        (u32::from(pixels) * u32::from(virtual_cells) / u32::from(physical_cells))
            .min(u32::from(u16::MAX)) as u16
    };
    // Zero pixels make tmux query the physical terminal, whose grid can differ
    // from a browser override. Keep its cell metrics with the virtual grid.
    let (pixel_width, pixel_height) = window.map_or((0, 0), |window| {
        (
            scale(window.width, window.columns, cols),
            scale(window.height, window.rows, rows),
        )
    });
    PtySize {
        cols,
        rows,
        pixel_width,
        pixel_height,
    }
}

struct ConnectionState {
    sharing: Arc<AtomicBool>,
    ever_connected: bool,
    marker: std::path::PathBuf,
    peer_results: broadcast::Receiver<ClientMessage>,
}

async fn connect_loop(
    dir: std::path::PathBuf,
    registration: Registration,
    screen: SharedScreen,
    commands: mpsc::Sender<ServerMessage>,
    mut exited: watch::Receiver<Option<(u32, bool)>>,
    mut local_input: broadcast::Receiver<Vec<u8>>,
    peer_results: broadcast::Receiver<ClientMessage>,
) {
    let mut state = ConnectionState {
        sharing: Arc::new(AtomicBool::new(true)),
        ever_connected: false,
        marker: release_marker(&dir, &registration.launch.id, std::process::id()),
        peer_results,
    };
    let mut retry = Duration::from_millis(400);
    loop {
        if state.marker.exists() || !state.sharing.load(Ordering::Relaxed) {
            let _ = commands.send(ServerMessage::Detach).await;
            let request = match reopen_marker(&dir, &registration.launch.id, std::process::id()) {
                Ok(path) => path,
                Err(_) => return,
            };
            loop {
                if exited.borrow().is_some() {
                    return;
                }
                if std::fs::remove_file(&request).is_ok() {
                    let _ = std::fs::remove_file(&state.marker);
                    state.sharing.store(true, Ordering::Relaxed);
                    state.ever_connected = false;
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                    _ = exited.changed() => return,
                }
            }
        }
        let result = connection(
            &dir,
            &registration,
            &screen,
            &commands,
            &mut exited,
            &mut local_input,
            &mut state,
        )
        .await;
        if exited.borrow().is_some() {
            return;
        }
        if result.is_ok() || !state.sharing.load(Ordering::Relaxed) {
            state.sharing.store(false, Ordering::Relaxed);
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(retry) => {},
            _ = exited.changed() => return,
        }
        retry = retry.saturating_mul(2).min(Duration::from_secs(2));
    }
}

async fn connection(
    dir: &Path,
    registration: &Registration,
    screen: &SharedScreen,
    commands: &mpsc::Sender<ServerMessage>,
    exited: &mut watch::Receiver<Option<(u32, bool)>>,
    local_input: &mut broadcast::Receiver<Vec<u8>>,
    state: &mut ConnectionState,
) -> Result<()> {
    let socket = tokio::net::UnixStream::connect(dir.join("host.sock")).await?;
    let (mut read, mut write) = socket.into_split();
    write_message(
        &mut write,
        &ClientMessage::Register {
            registration: Registration {
                reconnecting: state.ever_connected,
                ..registration.clone()
            },
        },
    )
    .await?;
    state.ever_connected = true;
    let (snapshot, mut output) = screen.lock().unwrap().subscribe();
    write_message(&mut write, &ClientMessage::Frame { frame: snapshot }).await?;
    let commands = commands.clone();
    let sharing = state.sharing.clone();
    let mut reader = tokio::spawn(async move {
        loop {
            let command: ServerMessage = read_message(&mut read).await?;
            if matches!(command, ServerMessage::Detach) {
                sharing.store(false, Ordering::Relaxed);
                return Ok::<(), anyhow::Error>(());
            }
            commands.send(command).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });
    let mut input_open = true;
    let mut release_check = tokio::time::interval(Duration::from_millis(500));
    let result = async {
        loop {
            tokio::select! {
                _ = release_check.tick() => {
                    if state.marker.exists() { return Ok(()); }
                }
                result = &mut reader => { result??; return Ok(()); }
                _ = exited.changed() => {
                    let code = *exited.borrow();
                    if let Some((code, interrupted)) = code {
                        // The stdin thread has joined: flush its final observations
                        // before telling the daemon to tear down this session.
                        loop {
                            let message = match local_input.try_recv() {
                                Ok(bytes) => ClientMessage::LocalInput { b64: STANDARD.encode(bytes) },
                                Err(broadcast::error::TryRecvError::Lagged(chunks)) => ClientMessage::LocalInputGap { chunks },
                                Err(_) => break,
                            };
                            write_message(&mut write, &message).await?;
                        }
                        let snapshot = screen.lock().unwrap().snapshot();
                        write_message(&mut write, &ClientMessage::Frame { frame: snapshot }).await?;
                        write_message(&mut write, &ClientMessage::Exit { code, interrupted: Some(interrupted) }).await?;
                        return Ok(());
                    }
                }
                observation = local_input.recv(), if input_open => {
                    let message = match observation {
                        Ok(bytes) => ClientMessage::LocalInput { b64: STANDARD.encode(bytes) },
                        Err(broadcast::error::RecvError::Lagged(chunks)) => ClientMessage::LocalInputGap { chunks },
                        Err(broadcast::error::RecvError::Closed) => { input_open = false; continue; }
                    };
                    write_message(&mut write, &message).await?;
                }
                result = state.peer_results.recv() => {
                    if let Ok(result) = result { write_message(&mut write, &result).await?; }
                }
                frame = output.recv() => {
                    let frame = match frame {
                        Ok(frame) => frame,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            let (frame, receiver) = screen.lock().unwrap().subscribe();
                            output = receiver;
                            frame
                        }
                        Err(_) => return Ok(()),
                    };
                    write_message(&mut write, &ClientMessage::Frame { frame }).await?;
                }
            }
        }
    }
    .await;
    reader.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinned_agents_keep_cmux_hooks_without_redirecting_codex_resume() {
        const CHILD: &str = "CONVERSATIONS_PINNED_SHIM_TEST";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::fs::PermissionsExt;
            let root = tempfile::tempdir().unwrap();
            let bin = root.path().join("bin");
            let shim = root.path().join("shim");
            std::fs::create_dir(&bin).unwrap();
            std::fs::create_dir(&shim).unwrap();
            for name in ["codex", "claude"] {
                let binary = bin.join(name);
                std::fs::write(&binary, "#!/bin/sh\nprintf 'native:%s\\n' \"$*\"\n").unwrap();
                std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o700)).unwrap();
                let wrapper = shim.join(name);
                let variable = format!("CMUX_CUSTOM_{}_PATH", name.to_uppercase());
                std::fs::write(
                    &wrapper,
                    format!("#!/bin/sh\nprintf 'hook\\n'\nexec \"${variable}\" \"$@\"\n"),
                )
                .unwrap();
                std::fs::set_permissions(wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "commands::local_host::wrapper::tests::pinned_agents_keep_cmux_hooks_without_redirecting_codex_resume", "--nocapture"])
                .env(CHILD, &bin).env("CMUX_AGENT_COMMAND_SHIM_ROOT", &shim)
                .env("PATH", "/usr/bin:/bin").output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let bin = PathBuf::from(std::env::var_os(CHILD).unwrap());
        for name in ["codex", "claude"] {
            for resume in [false, true] {
                let mut words = vec![bin.join(name).display().to_string()];
                if resume {
                    words.extend(["resume".into(), "native-id".into()]);
                }
                let launch = Launch {
                    id: uuid::Uuid::new_v4().to_string(),
                    agent: if name == "codex" {
                        conversations_core::AgentKind::Codex
                    } else {
                        conversations_core::AgentKind::Claude
                    },
                    cwd: bin.clone(),
                    command: words,
                };
                let mut command = agent_command(&launch);
                command.args(&launch.command[1..]);
                let pair = native_pty_system()
                    .openpty(PtySize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .unwrap();
                let mut reader = pair.master.try_clone_reader().unwrap();
                let mut child = pair.slave.spawn_command(command).unwrap();
                drop(pair.slave);
                let mut output = String::new();
                let _ = reader.read_to_string(&mut output);
                assert!(child.wait().unwrap().success(), "{output}");
                assert!(output.contains("native:"), "{output}");
                assert_eq!(
                    output.contains("hook"),
                    name == "claude" || !resume,
                    "{name} resume={resume}: {output}"
                );
                if resume {
                    assert!(output.contains("native:resume native-id"), "{output}");
                }
            }
        }
    }

    #[test]
    fn virtual_resize_preserves_physical_cell_geometry() {
        let physical = crossterm::terminal::WindowSize {
            columns: 100,
            rows: 40,
            width: 800,
            height: 680,
        };
        let resized = size_with_window(150, 30, Some(physical));
        assert_eq!((resized.cols, resized.rows), (150, 30));
        assert_eq!((resized.pixel_width, resized.pixel_height), (1200, 510));
        let unknown = size_with_window(150, 30, None);
        assert_eq!((unknown.pixel_width, unknown.pixel_height), (0, 0));
        let invalid = size_with_window(
            150,
            30,
            Some(crossterm::terminal::WindowSize {
                columns: 0,
                rows: 0,
                width: 800,
                height: 680,
            }),
        );
        assert_eq!((invalid.pixel_width, invalid.pixel_height), (0, 0));
        let oversized = size_with_window(
            1000,
            1000,
            Some(crossterm::terminal::WindowSize {
                columns: 1,
                rows: 1,
                width: u16::MAX,
                height: u16::MAX,
            }),
        );
        assert_eq!(
            (oversized.pixel_width, oversized.pixel_height),
            (u16::MAX, u16::MAX)
        );
    }
    use std::os::unix::fs::PermissionsExt;

    #[derive(Clone, Default)]
    struct Received(Arc<Mutex<Vec<u8>>>);
    impl Write for Received {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancellation_preserves_composer_readiness_and_unsent_drafts() {
        use super::super::protocol::CancelOutcome;
        for input in [b"".as_slice(), b"draft", b"\x1b[200~paste", b"\x1b[12;"] {
            let received = Received::default();
            let mut writer = GuardedWriter {
                writer: Box::new(received.clone()),
                composer: ComposerState::default(),
                last_peer_turn: None,
            };
            writer.write_all(input).unwrap();
            let before = (
                writer.composer.dirty,
                writer.composer.paste,
                writer.composer.escape.clone(),
            );
            let was_clean = writer.composer.clean();
            let writer = Arc::new(Mutex::new(writer));
            let (results, mut replies) = broadcast::channel(1);
            cancel_interaction(
                &Arc::new(CancelState::default()),
                true,
                &writer,
                "revoke".into(),
                &results,
            );
            let reply = tokio::time::timeout(Duration::from_secs(3), replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                reply,
                ClientMessage::CancelResult {
                    outcome: CancelOutcome::Requested,
                    ..
                }
            ));
            assert_eq!(*received.0.lock().unwrap(), [input, b"\x1b"].concat());
            let writer = writer.lock().unwrap();
            assert_eq!(
                writer.composer.clean(),
                was_clean,
                "an interrupt must not block a clean composer or permit a draft"
            );
            assert_eq!(
                (
                    writer.composer.dirty,
                    writer.composer.paste,
                    writer.composer.escape.clone()
                ),
                before,
                "an interrupt must not alter pending user input tracking"
            );
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn private_config_survives_running_wrapper_and_is_removed_only_after_child_exit() {
        const CHILD: &str = "CONVERSATIONS_EXIT_CLEANUP_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "commands::local_host::wrapper::tests::private_config_survives_running_wrapper_and_is_removed_only_after_child_exit", "--nocapture"])
                .env(CHILD, "1").env_remove("CONVERSATIONS_WRAPPED").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let launch_dir = root.path().join("opencode").join(&id);
        std::fs::create_dir_all(launch_dir.join("plugin")).unwrap();
        for dir in [
            root.path().join("opencode"),
            launch_dir.clone(),
            launch_dir.join("plugin"),
        ] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(launch_dir.join("tui.json"), "private fixture").unwrap();
        std::fs::write(launch_dir.join("plugin/tui.js"), "private fixture").unwrap();
        std::fs::write(launch_dir.join("report.json"), "identity fixture").unwrap();
        let launch = Launch {
            id,
            agent: conversations_core::AgentKind::OpenCode,
            cwd: root.path().into(),
            command: vec!["/bin/sh".into(), "-c".into(), "/bin/sleep 0.2".into()],
        };
        let mut running = Box::pin(run(root.path(), launch, true, "http://127.0.0.1:1"));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(40)) => {
                assert!(launch_dir.join("tui.json").exists());
                assert!(launch_dir.join("plugin/tui.js").exists());
            }
            result = &mut running => panic!("child exited before the live-config check: {result:?}"),
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(6), running)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(!launch_dir.join("tui.json").exists());
        assert!(!launch_dir.join("plugin").exists());
        assert_eq!(
            std::fs::read_to_string(launch_dir.join("report.json")).unwrap(),
            "identity fixture"
        );
    }
}

#[cfg(test)]
mod peer_input_tests {
    use super::*;

    #[test]
    fn direct_file_pastes_protect_the_composer_even_with_partial_writes() {
        struct OneByte;
        impl Write for OneByte {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len().min(1))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = GuardedWriter {
            writer: Box::new(OneByte),
            composer: ComposerState::default(),
            last_peer_turn: None,
        };
        writer.write_all(b"\x1b[12;40R").unwrap();
        assert!(writer.composer.clean());
        writer
            .write_all(b"\x1b[200~/private/attachment\x1b[201~")
            .unwrap();
        assert!(
            !writer.composer.clean(),
            "peer input must not submit a file draft"
        );
        writer.write_all(b"\r").unwrap();
        assert!(writer.composer.clean());
    }

    #[test]
    fn full_or_disconnected_input_queue_defers_without_uncertain_delivery() {
        let (queue, receiver) = std::sync::mpsc::sync_channel(1);
        assert!(queue
            .try_send(RemoteInput::Human(b"human draft".to_vec(), None))
            .is_ok());
        let (results, mut receipts) = broadcast::channel(2);
        queue_peer_input(
            &queue,
            &results,
            "full-request".into(),
            "native".into(),
            "peer".into(),
            None,
        );
        assert!(
            matches!(receipts.try_recv().unwrap(), ClientMessage::PeerInputResult { request_id, written: false } if request_id == "full-request")
        );
        assert!(
            matches!(receiver.try_recv().unwrap(), RemoteInput::Human(bytes, None) if bytes == b"human draft")
        );
        drop(receiver);
        queue_peer_input(
            &queue,
            &results,
            "closed-request".into(),
            "native".into(),
            "peer".into(),
            None,
        );
        assert!(
            matches!(receipts.try_recv().unwrap(), ClientMessage::PeerInputResult { request_id, written: false } if request_id == "closed-request")
        );
    }

    #[test]
    fn preserves_unsent_input_and_bracketed_pastes_until_human_submit() {
        let mut state = ComposerState::unknown();
        assert!(!state.clean());
        state.observe(b"\r");
        assert!(state.clean());
        state.observe(b"\n");
        assert!(!state.clean()); // Ctrl-J can insert a draft newline without submitting.
        state.observe(b"\r");
        assert!(state.clean());
        state.observe(b"a draft");
        assert!(!state.clean());
        state.observe(b"\x7f\x15");
        assert!(!state.clean());
        state.observe(b"\r");
        assert!(state.clean());
        state.observe(b"\x1b[200~hello\n");
        assert!(!state.clean());
        state.observe(b"\r");
        assert!(!state.clean());
        state.observe(b"\x1b[201~");
        assert!(!state.clean());
        state.observe(b"\r");
        assert!(state.clean());
    }
    #[test]
    fn fresh_session_protocol_cursor_reply_does_not_create_a_draft() {
        let mut state = ComposerState::default();
        state.observe(b"\x1b[12;");
        assert!(!state.clean());
        state.observe(b"40R");
        assert!(state.clean());
        state.observe(b"\x1b[A");
        assert!(!state.clean());
    }
}
