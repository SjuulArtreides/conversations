//! `arrakis-conversations live` sessions — run a REAL interactive agent in a
//! PTY inside the local host wrapper.
//!
//! Each child is `arrakis-conversations host wrap … -- <agent>` under a
//! pseudo-terminal, so the local host owns its canonical session record,
//! local transcript history and the in-harness same-machine handoff, exactly
//! like a cmux or normal-backend terminal. The multiplexer only renders: a
//! reader thread feeds the child's output into a vt100 parser (for focus
//! repaints) and to the local terminal while focused, and local keystrokes go
//! to the focused child. Nothing about the session is sent to the server.
//!
//! Shutdown is driven by the child `wait()` completing: that flips the shared
//! [`Shutdown`] signal, which every thread/task observes; then we restore the
//! terminal (via the [`RawGuard`] Drop impl).
//!
//! The terminal is put into raw mode for the duration. Raw mode is restored on
//! EVERY exit path: a [`RawGuard`] Drop restores it on normal/early return, and
//! a panic hook restores it before the default hook so a panic never leaves the
//! user's terminal wedged.

use std::collections::HashSet;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
#[cfg(test)]
use conversations_core::terminal::CODEX_TRANSCRIPT_REBUILD;
use conversations_core::{AgentKind, AgentLauncher, Message, Role};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::Notify;
use tokio::task::JoinHandle as TokioJoinHandle;
use uuid::Uuid;

use super::Tailer;
use crate::cli::{GlobalOpts, LiveArgs};

/// Maximum number of UTF-8 chars kept from a single web reply before the
/// mandatory trailing CR. Anything longer is truncated.
const REPLY_CAP: usize = 4096;

/// Poll interval for the JSONL tail (mirrors the view-only loop).
const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Bound each history write during a full re-read. A single oversized message
/// is still written alone; splitting a normalized message would change it.
const HISTORY_RECONCILE_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// How long to wait for a detached child to exit after the graceful SIGHUP
/// (`kill()`) before escalating to SIGKILL. Used only by the detached
/// [`spawn_child`] path; the foreground driver lets `claude` exit on its own.
const CHILD_TERM_GRACE: Duration = Duration::from_secs(5);

/// How long to poll for the exit status after SIGKILL. A process in an
/// uninterruptible kernel wait can outlive SIGKILL briefly; teardown gives up
/// rather than block, because the caller must never wait indefinitely.
const CHILD_KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Erase the whole display and home the cursor: `CSI H` (cursor home) then
/// `CSI 2 J` (erase entire display). Written before an absolute screen repaint
/// so no stale cells survive (see [`FocusManager::focus`]).
const CLEAR_AND_HOME: &[u8] = b"\x1b[H\x1b[2J";

/// Erase the display and xterm scrollback before rebuilding a web mirror replay.
/// Local focus repaints deliberately use [`CLEAR_AND_HOME`] instead: switching
/// tabs locally should not wipe the user's terminal scrollback.
#[cfg(test)]
const CLEAR_AND_HOME_AND_SCROLLBACK: &[u8] = b"\x1b[r\x1b[H\x1b[2J\x1b[3J";

/// Retained per-child terminal history for web mirror replays/reseeds. This
/// matches the server-side snapshot cap, so a CLI host can repopulate the same
/// amount of scrollback after a server restart.
const PTY_SCROLLBACK_ROWS: usize = 2000;

fn new_vt100_parser(rows: u16, cols: u16) -> vt100::Parser {
    vt100::Parser::new(rows, cols, PTY_SCROLLBACK_ROWS)
}

use conversations_core::terminal_history::PtyEraseTracker;

/// Shared shutdown signal observed by every thread and task. The `flag` is the
/// cheap polled view (for the blocking threads); `notify` wakes the async tasks
/// promptly. [`Shutdown::trigger`] sets both.
pub(crate) struct Shutdown {
    flag: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    pub(crate) fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub(crate) fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// RAII guard that restores the terminal out of raw mode on drop, no matter how
/// the interactive driver unwinds. Pair with the panic hook installed in
/// [`run`] so a panic also restores the terminal.
struct RawGuard;

impl RawGuard {
    fn enable() -> Result<Self> {
        enable_raw_mode().context("entering terminal raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        // Best-effort: nothing useful to do if this fails during teardown.
        let _ = disable_raw_mode();
    }
}

/// Encode an absolute cwd into Claude Code's project-directory name: every `/`
/// and `.` becomes `-` (mirrors how Claude writes `~/.claude/projects/<enc>/`).
/// The transform is lossy but matches what claude itself will create, so the
/// JSONL path we compute and the one claude writes line up.
fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// Compute the JSONL path claude will write for `session_id` run in `cwd`:
/// `<projects_dir>/<encoded-cwd>/<session_id>.jsonl`. The file does NOT exist
/// yet — the tailer tolerates that and polls until it appears.
fn target_jsonl_path(projects_dir: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    projects_dir
        .join(encode_cwd(cwd))
        .join(format!("{session_id}.jsonl"))
}

/// Sanitize a web reply into the body bytes safe to inject into the PTY.
///
/// This is the security boundary: a web reply must never be able to send control
/// sequences (Ctrl-C, EOF, ESC/CSI, arrow/function keys, bracketed paste) or
/// embedded newlines into the local `claude` TUI. We:
///   - drop every char below `0x20` and the DEL `0x7F` (kills `\x03`, `\x04`,
///     `\x1b` and thus every C0 escape sequence, plus CR/LF),
///   - drop the C1 control range `U+0080..=U+009F` — notably `U+009B`, the
///     single-byte CSI, which some terminals treat as an escape introducer just
///     like `ESC [`; without this a reply could still smuggle a control seq,
///   - keep the rest of printable + multi-byte UTF-8,
///   - truncate to [`REPLY_CAP`] chars.
///
/// The submitting CR is NOT appended here: the caller writes it as a SEPARATE,
/// delayed keystroke so claude's TUI registers it as Enter rather than folding
/// it into a bracketed paste.
pub fn sanitize_reply(text: &str) -> String {
    text.chars()
        .filter(|&c| c >= ' ' && c != '\u{7f}' && !('\u{80}'..='\u{9f}').contains(&c))
        .take(REPLY_CAP)
        .collect()
}

/// The working directory an `adk code` session must be spawned in.
///
/// `adk code` always opens the Arrakis DevKit checkout (`$ARRAKIS_HOME`) — a
/// host-server OpenCode refuses any other project path. The OpenCode tailer
/// finds a NEW session by matching the session's own `directory` against the
/// spawn cwd, so spawning `adk code` anywhere else would register a live session
/// whose transcript is never located. Pin the cwd to the same root the harness
/// will use instead, and fail with the same remedy DevKit gives when the
/// variable is missing (`adk` itself also validates the checkout).
pub fn adk_session_cwd() -> Result<PathBuf> {
    let home = std::env::var("ARRAKIS_HOME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .context(
            "`adk code` needs ARRAKIS_HOME — start the host from the Arrakis DevKit \
             checkout (direnv allow) or run `nix develop` first",
        )?;
    let home = PathBuf::from(home);
    if !home.is_dir() {
        anyhow::bail!(
            "ARRAKIS_HOME points at {} which is not a directory",
            home.display()
        );
    }
    Ok(canonical_path_or_raw(&home))
}

/// How a spawned interactive `claude` should be launched.
///
/// `--interactive` always generates a fresh session id; the resident host
/// (`--host`) can instead RESUME a stored session id. Both share the same
/// driver, so the difference is captured here and applied where the
/// `CommandBuilder` is assembled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnMode {
    /// Start a brand-new session: `claude --session-id <uuid>`.
    New,
    /// Resume an existing session: `claude --resume <session-id>`.
    Resume,
}

/// Resolved parameters for one spawned interactive `claude`. Built by the
/// `--interactive` entrypoint (always `New`, host's cwd) and by the resident
/// host loop (either `New` or `Resume`, in a server-sourced cwd for resume).
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// Agent runtime to launch and tail.
    pub agent: AgentKind,
    /// The session id passed to `claude` (a fresh uuid for `New`, the stored id
    /// for `Resume`). Also the id the live session is REGISTERED under — so a
    /// `Resume` keeps the same live id across reopens (the row continues).
    pub session_id: String,
    /// The value handed to the agent's resume CLI arg (`claude --resume <X>` /
    /// `codex resume <X>`). Usually equal to `session_id`, but DIFFERS for a
    /// Codex reopen: `session_id` stays the conversations-minted live id (so the
    /// stored row continues) while `resume_target` is Codex's real session id,
    /// the only thing `codex resume` accepts. Unused in `New` mode.
    pub resume_target: String,
    /// Working directory `claude` runs in.
    pub cwd: PathBuf,
    /// Whether to start fresh or resume.
    pub mode: SpawnMode,
    /// Per-session agent args resolved locally at launch time.
    /// Historical `LiveArgs::claude_args` are appended after these.
    pub agent_args: Vec<String>,
    /// How to invoke the agent. [`AgentLauncher::Opencode`] runs the agent's own
    /// binary; [`AgentLauncher::Adk`] runs OpenCode through `adk code`.
    pub launcher: AgentLauncher,
    /// Whether an `adk code` child may draw its tmux status strip. Off by
    /// default: the strip nests a tmux pane inside the mirrored PTY, so the web
    /// terminal shows tmux chrome instead of a plain OpenCode TUI. Ignored by
    /// every other launcher.
    pub adk_statusline: bool,
}

impl SpawnConfig {
    /// Build a config that starts a brand-new session in `cwd`.
    pub fn new_session(agent: AgentKind, cwd: PathBuf) -> Self {
        let session_id = Uuid::new_v4().to_string();
        Self {
            agent,
            // `New` never resumes; mirror `session_id` so the field is always set.
            resume_target: session_id.clone(),
            session_id,
            cwd,
            mode: SpawnMode::New,
            agent_args: Vec::new(),
            launcher: AgentLauncher::default(),
            adk_statusline: false,
        }
    }

    /// Build a config that resumes `session_id` in `cwd`.
    pub fn resume_session(
        agent: AgentKind,
        cwd: PathBuf,
        session_id: String,
        resume_target: String,
    ) -> Self {
        Self {
            agent,
            session_id,
            resume_target,
            cwd,
            mode: SpawnMode::Resume,
            agent_args: Vec::new(),
            launcher: AgentLauncher::default(),
            adk_statusline: false,
        }
    }
}

// ===========================================================================
// Shared session wiring (used by the detached `spawn_child()` below).
// ===========================================================================

/// A spawned `claude` under a PTY plus the master handles needed to drive it.
///
/// `master` is owned (not shared): the foreground driver moves it into its
/// resize watcher thread; the detached child drops it (it never resizes).
/// `MasterPty` is `Send` but not `Sync`, so it cannot be shared behind an `Arc`.
struct SpawnedPty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    pty_reader: Box<dyn Read + Send>,
    pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

/// Parameters for the JSONL tailer thread.
#[derive(Debug, Clone)]
struct TailConfig {
    agent: AgentKind,
    target: PathBuf,
    cwd: PathBuf,
    tail_after: SystemTime,
    /// Native resume id when known before tailing starts. Used to select the
    /// Codex rollout or OpenCode database session instead of using recency.
    resume_target: Option<String>,
    /// Host runtime directory whose local history database receives every
    /// message. Nothing is sent to the central server.
    local_history: PathBuf,
}

/// Where reconciled transcript batches go. Production writes the host's own
/// history database; tests inject write failures to prove the full re-read.
trait HistorySink {
    fn push_events(&self, session_id: &str, messages: &[Message]) -> Result<()>;
}

/// Writes batches into `history.db` of one host runtime directory.
struct LocalHistoryTap {
    dir: PathBuf,
    source: crate::commands::local_host::history::Source,
    history: Mutex<Option<crate::commands::local_host::history::History>>,
}

impl LocalHistoryTap {
    fn new(dir: PathBuf, agent: AgentKind, native_id: &str, origin: &Path) -> Self {
        Self {
            dir,
            source: crate::commands::local_host::history::Source {
                agent,
                native_id: native_id.to_owned(),
                origin: origin.to_string_lossy().into_owned(),
            },
            history: Mutex::new(None),
        }
    }

    fn persist(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        let mut guard = self
            .history
            .lock()
            .map_err(|_| anyhow::anyhow!("local history lock poisoned"))?;
        if guard.is_none() {
            *guard = Some(crate::commands::local_host::history::History::open(
                &self.dir,
            )?);
        }
        let history = guard.as_mut().expect("opened above");
        let result = history.ingest(session_id, &self.source, messages);
        if result.is_err() {
            // Reopen next time rather than keep a possibly broken connection.
            *guard = None;
        }
        result.map(|_| ())
    }
}

impl HistorySink for LocalHistoryTap {
    fn push_events(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        self.persist(session_id, messages)
    }
}

/// Push messages in ordered, size-bounded batches. History de-duplicates each
/// message by UUID, so a complete replay after a failed write is safe while
/// retaining the transcript's source order for missing rows.
fn push_event_batches(
    sink: &(impl HistorySink + ?Sized),
    live_id: &str,
    messages: &[Message],
) -> Result<()> {
    let mut start = 0usize;
    let mut batch_bytes = 0usize;

    for (index, message) in messages.iter().enumerate() {
        let message_bytes = serde_json::to_vec(message)
            .context("serializing live message for reconciliation")?
            .len()
            + 1;
        if index > start && batch_bytes + message_bytes > HISTORY_RECONCILE_BATCH_BYTES {
            sink.push_events(live_id, &messages[start..index])?;
            start = index;
            batch_bytes = 0;
        }
        batch_bytes += message_bytes;
    }

    if start < messages.len() {
        sink.push_events(live_id, &messages[start..])?;
    }
    Ok(())
}

/// Coordinates the append-only JSONL tail with local write recovery.
///
/// A failed write cannot leave the tailer at its advanced byte offset with no
/// repair path. Once dirty, the next sync re-reads and re-writes the complete
/// transcript before accepting more deltas. UUID de-duplication makes rows
/// already stored no-ops and fills only the missing history.
struct JsonlHistorySync {
    full_reconcile_required: bool,
}

impl JsonlHistorySync {
    fn new() -> Self {
        Self {
            full_reconcile_required: false,
        }
    }

    fn require_full_reconcile(&mut self) {
        self.full_reconcile_required = true;
    }

    fn sync(
        &mut self,
        sink: &(impl HistorySink + ?Sized),
        live_id: &str,
        transcript_path: &Path,
        incremental: &[Message],
    ) -> Result<()> {
        if self.full_reconcile_required {
            let bytes = std::fs::read(transcript_path).with_context(|| {
                format!("reading {} for reconciliation", transcript_path.display())
            })?;
            let messages = super::decode_messages(&bytes);
            push_event_batches(sink, live_id, &messages)?;
            self.full_reconcile_required = false;
            Ok(())
        } else if incremental.is_empty() {
            Ok(())
        } else {
            match push_event_batches(sink, live_id, incremental) {
                Ok(()) => Ok(()),
                Err(error) => {
                    // `Tailer::poll` has already advanced its file offset. A
                    // complete replay is now the only safe retry source.
                    self.full_reconcile_required = true;
                    Err(error)
                }
            }
        }
    }
}

/// Process-wide claims for Codex rollout paths.
///
/// Unlike Claude, Codex does not accept the conversations live id at launch.
/// A fresh child therefore has to discover its rollout under
/// `~/.codex/sessions`. Without claims, two mux children started close together
/// can both choose the same "newest" file and push one transcript into two live
/// tabs. The claim lives for the tailer's lifetime and is released on exit so a
/// later resume may attach to the same rollout.
static CODEX_TAIL_CLAIMS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// OpenCode uses one shared SQLite database, so claims reserve native session
/// ids rather than transcript paths when several mux children start together.
static OPENCODE_TAIL_CLAIMS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn codex_tail_claims() -> &'static Mutex<HashSet<PathBuf>> {
    CODEX_TAIL_CLAIMS.get_or_init(|| Mutex::new(HashSet::new()))
}

struct CodexTailClaim {
    path: PathBuf,
}

impl Drop for CodexTailClaim {
    fn drop(&mut self) {
        if let Ok(mut claims) = codex_tail_claims().lock() {
            claims.remove(&self.path);
        }
    }
}

struct ResolvedTailTarget {
    path: PathBuf,
    /// Keeps a Codex path reserved until this tailer exits.
    _claim: Option<CodexTailClaim>,
}

/// A transcript owned by one registered terminal, without owning its agent.
/// Dropping the wrapper registration stops ingestion but leaves the PTY alive.
/// It follows the native transcript into the host's history database only:
/// it needs no central server, account or availability, and uploads nothing.
pub(crate) struct BoundTranscript {
    shutdown: Arc<Shutdown>,
}

impl Drop for BoundTranscript {
    fn drop(&mut self) {
        self.shutdown.trigger();
    }
}

impl BoundTranscript {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        live_id: String,
        agent: AgentKind,
        native_id: String,
        cwd: PathBuf,
        local_history: PathBuf,
    ) -> Result<Self> {
        anyhow::ensure!(
            agent != AgentKind::Zsh && !native_id.trim().is_empty(),
            "transcript ingestion requires an explicit native agent identity"
        );
        let root = crate::discover::resolve_sessions_dir(agent, None, None, None)?;
        let target = if agent == AgentKind::Claude {
            target_jsonl_path(&root, &cwd, &native_id)
        } else {
            root
        };
        let tail = TailConfig {
            agent,
            target,
            cwd,
            tail_after: SystemTime::now(),
            resume_target: Some(native_id.clone()),
            local_history,
        };
        let shutdown = Shutdown::new();
        let worker_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("bound-transcript".into())
            .spawn(move || {
                tail_loop(
                    &live_id,
                    tail,
                    &worker_shutdown,
                    &Arc::new(Mutex::new(Some(native_id))),
                )
            })?;
        Ok(Self { shutdown })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AgentCommand {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    /// Environment the child needs on top of the inherited one.
    pub(crate) env: Vec<(String, String)>,
}

/// Build the `adk code` invocation for an OpenCode session.
///
/// `adk code` is a launcher, not an agent: it opens the monorepo in OpenCode
/// with the Arrakis harness attached (MCP credentials, agent profile, host
/// server), so the session it creates is an ordinary OpenCode session in the
/// shared `opencode.db` and is tailed by the OpenCode tailer unchanged.
///
/// Two traits of the harness shape this argv:
///   - Every OpenCode argument goes after `--`. `adk code`'s own first positional
///     selects a DevKit subcommand or agent profile, so a bare `--session` would
///     be parsed as one of those (see `adk code --help`). `--session <id>` is an
///     attach-compatible option the harness resolves against its host server.
///   - `OC_STATUS` drives the tmux status strip: `wrap.sh` runs OpenCode inside a
///     tmux pane unless it is `0`, and in a live session that pane is what the web
///     mirror shows — tmux chrome instead of the OpenCode TUI. So it is always set
///     EXPLICITLY, in both directions: `0` to suppress the strip, `1` when the
///     user opted in via [`SpawnConfig::adk_statusline`], because the child
///     inherits this host's environment and an ambient `OC_STATUS=0` would
///     otherwise silently override the opt-in.
///
/// Note that `adk code` only honors an inherited `OC_STATUS` from DevKit
/// 2026-09-17 onward; an older harness overwrites it from the developer's own
/// `adk code configure` preference, which then wins over this setting.
/// One argv policy for the original live host and the local cmux host.
pub(crate) fn launch_agent(
    agent: AgentKind,
    launcher: AgentLauncher,
    session_id: &str,
    resume_target: Option<&str>,
    agent_args: Vec<String>,
    adk_statusline: bool,
) -> AgentCommand {
    launch_agent_with_env(
        agent,
        launcher,
        session_id,
        resume_target,
        agent_args,
        adk_statusline,
        |key| std::env::var_os(key),
    )
}

fn launch_agent_with_env(
    agent: AgentKind,
    launcher: AgentLauncher,
    session_id: &str,
    resume_target: Option<&str>,
    agent_args: Vec<String>,
    adk_statusline: bool,
    getenv: impl FnOnce(&str) -> Option<std::ffi::OsString>,
) -> AgentCommand {
    let mut env = Vec::new();
    if agent == AgentKind::OpenCode {
        let inherited = getenv("OPENCODE_CLI_CONFIG_CONTENT");
        let config = match managed_opencode_cli_config(inherited.clone()) {
            Ok(config) => config,
            Err(_) => match inherited.and_then(|value| value.into_string().ok()) {
                Some(value) => {
                    eprintln!("Invalid OPENCODE_CLI_CONFIG_CONTENT; forwarding unchanged. Fix or unset it.");
                    value
                }
                None => {
                    // The command protocol carries UTF-8 strings. Force a parse
                    // failure rather than silently dropping unrepresentable input.
                    eprintln!("OPENCODE_CLI_CONFIG_CONTENT is not UTF-8; forwarding invalid JSON. Fix or unset it.");
                    "INVALID_UTF8".into()
                }
            },
        };
        env.push(("OPENCODE_CLI_CONFIG_CONTENT".into(), config));
    }
    launch_agent_command(
        agent,
        launcher,
        session_id,
        resume_target,
        agent_args,
        adk_statusline,
        env,
    )
}

/// cmux's login shell owns client preferences; prepare them in that child, not daemon argv.
pub(crate) fn launch_agent_for_local_host(
    agent: AgentKind,
    launcher: AgentLauncher,
    session_id: &str,
    resume_target: Option<&str>,
    agent_args: Vec<String>,
    adk_statusline: bool,
) -> AgentCommand {
    launch_agent_command(
        agent,
        launcher,
        session_id,
        resume_target,
        agent_args,
        adk_statusline,
        Vec::new(),
    )
}

fn launch_agent_command(
    agent: AgentKind,
    launcher: AgentLauncher,
    session_id: &str,
    resume_target: Option<&str>,
    agent_args: Vec<String>,
    adk_statusline: bool,
    mut env: Vec<(String, String)>,
) -> AgentCommand {
    if launcher == AgentLauncher::Adk {
        let mut argv = vec!["code".to_string()];
        let mut passthrough = Vec::new();
        if let Some(resume_target) = resume_target {
            passthrough.push("--session".to_string());
            passthrough.push(resume_target.to_owned());
        }
        passthrough.extend(agent_args);
        if !passthrough.is_empty() {
            argv.push("--".to_string());
            argv.extend(passthrough);
        }
        env.push((
            "OC_STATUS".to_string(),
            if adk_statusline { "1" } else { "0" }.to_string(),
        ));
        return AgentCommand {
            program: "adk".to_string(),
            args: argv,
            env,
        };
    }
    let mut argv = Vec::new();
    match agent {
        AgentKind::Claude => match resume_target {
            Some(target) => argv.extend(["--resume".into(), target.to_owned()]),
            None => argv.extend(["--session-id".into(), session_id.to_owned()]),
        },
        AgentKind::Codex => {
            if !agent_args.iter().any(|arg| arg == "--no-alt-screen") {
                argv.push("--no-alt-screen".into());
            }
            if let Some(target) = resume_target {
                argv.extend(["resume".into(), target.to_owned()]);
            }
        }
        AgentKind::OpenCode => {
            if let Some(target) = resume_target {
                argv.extend(["--session".into(), target.to_owned()]);
            }
        }
        AgentKind::Zsh => {}
    }
    argv.extend(agent_args);
    AgentCommand {
        program: agent.as_str().to_owned(),
        args: argv,
        env,
    }
}

/// Environment entry carrying an OpenCode handoff with a model to the opencode
/// bridge, which removes it before OpenCode starts.
pub(crate) const OPENCODE_HANDOFF_ENV: &str = "CONVERSATIONS_OPENCODE_HANDOFF";

/// How a handoff child starts: extra argv, plus an OpenCode payload when the
/// model has to be applied inside the TUI (its CLI has no model flag).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HandoffLaunch {
    pub args: Vec<String>,
    pub opencode: Option<String>,
}

pub(crate) fn handoff_launch(
    agent: AgentKind,
    brief: &str,
    source_url: &str,
    model: Option<&conversations_core::messaging::HandoffModel>,
) -> HandoffLaunch {
    let prompt = format!("Handoff from Conversations session: {source_url}\n\n{brief}");
    if let (AgentKind::OpenCode, Some(model)) = (agent, model) {
        let opencode = serde_json::json!({
            "model": model.model,
            "variant": model.variant,
            "prompt": prompt,
        });
        return HandoffLaunch {
            args: Vec::new(),
            opencode: Some(opencode.to_string()),
        };
    }
    let mut args = model.map(|m| model_args(agent, m)).unwrap_or_default();
    match agent {
        AgentKind::OpenCode => args.extend(["--prompt".into(), prompt]),
        AgentKind::Claude | AgentKind::Codex => args.extend(["--".into(), prompt]),
        AgentKind::Zsh => {}
    }
    HandoffLaunch {
        args,
        opencode: None,
    }
}

/// Model flags for agents whose CLI takes them. Restores reuse these, so a
/// resumed child keeps its model; OpenCode keeps it on the session instead.
pub(crate) fn model_args(
    agent: AgentKind,
    model: &conversations_core::messaging::HandoffModel,
) -> Vec<String> {
    let mut args = Vec::new();
    match agent {
        AgentKind::Claude => {
            args.extend(["--model".into(), model.model.clone()]);
            if let Some(effort) = &model.variant {
                args.extend(["--effort".into(), effort.clone()]);
            }
        }
        AgentKind::Codex => {
            args.extend(["--model".into(), model.model.clone()]);
            if let Some(effort) = &model.variant {
                args.extend(["-c".into(), format!("model_reasoning_effort=\"{effort}\"")]);
            }
        }
        AgentKind::OpenCode | AgentKind::Zsh => {}
    }
    args
}

/// Inline CLI settings merge over global cli.json only in the managed child.
pub(crate) fn managed_opencode_cli_config(inherited: Option<std::ffi::OsString>) -> Result<String> {
    let mut config: serde_json::Value = match inherited {
        Some(value) => serde_json::from_str(value.to_str().context("CLI config is not UTF-8")?)?,
        None => serde_json::json!({}),
    };
    let tabs = config
        .as_object_mut()
        .context("CLI config is not an object")?
        .entry("tabs")
        .or_insert_with(|| serde_json::json!({}));
    tabs.as_object_mut()
        .context("CLI tabs config is not an object")?
        .insert("mode".into(), "off".into());
    Ok(config.to_string())
}

fn agent_command(config: &SpawnConfig, args: &LiveArgs) -> AgentCommand {
    let supplied: Vec<String> = config
        .agent_args
        .iter()
        .chain(args.claude_args.iter())
        .cloned()
        .collect();
    launch_agent(
        config.agent,
        config.launcher,
        &config.session_id,
        (config.mode == SpawnMode::Resume).then_some(config.resume_target.as_str()),
        supplied,
        config.adk_statusline,
    )
}

/// Assemble the command shared by attached, detached, and resumed PTY sessions.
fn pty_command(spec: &AgentCommand, cwd: &Path) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(&spec.program);
    for a in &spec.args {
        cmd.arg(a);
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    cmd.cwd(cwd);

    // Drop any inherited COLUMNS/LINES. portable-pty seeds the child env from our
    // process env, which may carry a stale COLUMNS/LINES from the parent shell.
    // Many TUIs (claude's included) trust those over the PTY's real winsize, so a
    // stale value lays the banner out at the wrong width and misplaces its art.
    // Removing them makes the winsize we opened the PTY with authoritative.
    cmd.env_remove("COLUMNS");
    cmd.env_remove("LINES");
    // This is an interactive terminal even when its resident host was started
    // by an automation shell with NO_COLOR=1. Let the agent detect PTY colors.
    cmd.env_remove("NO_COLOR");

    cmd
}

/// The `host wrap` invocation that runs the configured agent for `config`:
/// the wrapper owns the agent's own PTY, registers the session with the local
/// host in `host_dir` and bridges this PTY to it. The agent's argv and
/// environment are the same as an unwrapped launch; the wrapper adds only its
/// local registration (and, for Codex, its private app-server binding).
fn wrapped_command(
    config: &SpawnConfig,
    args: &LiveArgs,
    server: &str,
    host_dir: &Path,
) -> Result<CommandBuilder> {
    let spec = agent_command(config, args);
    let mut inner = vec![spec.program.clone()];
    inner.extend(spec.args.iter().cloned());
    let mut cmd = pty_command(
        &AgentCommand {
            program: std::env::current_exe()?.to_string_lossy().into_owned(),
            args: Vec::new(),
            env: spec.env.clone(),
        },
        &config.cwd,
    );
    cmd.args(["--server", server, "host", "--runtime-dir"]);
    cmd.arg(host_dir);
    cmd.args(["wrap", "--session-id", &config.session_id]);
    cmd.args(["--agent", config.agent.as_str(), "--cwd"]);
    cmd.arg(&config.cwd);
    cmd.arg("--");
    cmd.args(&inner);
    // A mux started inside another wrapper or a cmux surface still gives each
    // session its own registration: never the enclosing terminal's identity.
    for key in [
        "CONVERSATIONS_WRAPPED",
        "CONVERSATIONS_WRAPPER_PID",
        "CONVERSATIONS_HOST_DIR",
        "CMUX_SURFACE_ID",
        "CMUX_WORKSPACE_ID",
    ] {
        cmd.env_remove(key);
    }
    Ok(cmd)
}

/// Open a PTY and start the wrapped agent inside it, returning the child plus
/// the master reader/writer handles. The single PTY writer is wrapped in an
/// `Arc<Mutex<…>>` because the multiplexer's focus bridge shares it.
fn spawn_wrapped_pty(
    config: &SpawnConfig,
    args: &LiveArgs,
    server: &str,
    host_dir: &Path,
    size: PtySize,
) -> Result<SpawnedPty> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(size)
        .context("opening pseudo-terminal")?;

    let cmd = wrapped_command(config, args, server, host_dir)?;
    let child = pair
        .slave
        .spawn_command(cmd)
        .context("starting the host wrapper under the PTY")?;

    // The slave handle is owned by the child now; dropping our copy lets the
    // master see EOF when the child exits.
    drop(pair.slave);

    let master = pair.master;
    let pty_reader = master.try_clone_reader().context("cloning PTY reader")?;
    // portable-pty's `take_writer()` consumes the master's single writer, so it
    // can only be called ONCE. Every local producer that writes into the PTY
    // shares this one writer behind a mutex. Writes are short and synchronous, so the lock is
    // never held across an await.
    let pty_writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
        master.take_writer().context("taking PTY writer")?,
    ));

    Ok(SpawnedPty {
        child,
        master,
        pty_reader,
        pty_writer,
    })
}

/// Build a self-clearing replay for the browser/server PTY mirror: retained
/// scrollback (oldest→newest) followed by the current visible screen. This is
/// intentionally stronger than `contents_formatted()` because a server restart
/// loses its in-memory PTY snapshot; the host has the lossless per-child parser
/// and can re-seed both the visible screen and scrollback.
///
pub(crate) fn mirror_replay_bytes(parser: &mut vt100::Parser) -> Vec<u8> {
    conversations_core::terminal::replay_snapshot(parser)
}

// ===========================================================================
// Detached, web-only child (W1) — the multiplexer building block.
// ===========================================================================

/// A handle to a detached, web-wired child running as a background task.
///
/// The child has the full publication wiring (read-only PTY mirror,
/// heartbeat, JSONL tail) but is NOT bridged to the local TTY: no stdin→PTY,
/// no PTY→stdout, no local-SIGWINCH. Its PTY output feeds a per-child local
/// [`vt100::Parser`] (so a later wave can repaint it). Drop or [`cancel`] to
/// tear it down (SIGHUP → bounded wait → SIGKILL).
pub(super) struct DetachedChild {
    /// Server-assigned live id (== session id).
    pub(super) live_id: String,
    /// Agent runtime this child is running.
    pub(super) agent: AgentKind,
    /// The working directory this child's `claude` runs in (from
    /// [`SpawnConfig::cwd`]). Recorded so the host can persist `{session_id,
    /// cwd}` for resume — restarting `--host` re-opens each session via
    /// `claude --resume <id>` in this same `cwd`.
    cwd: PathBuf,
    /// Server-resolved agent launch arguments used for this child. The host
    /// checkpoints these with the open session so restart recovery does not
    /// silently drop Codex sandbox / approval settings.
    agent_args: Vec<String>,
    /// How this child was invoked, checkpointed with the open session so a host
    /// restart re-opens an `adk code` session through the harness again.
    launcher: AgentLauncher,
    /// Whether this child was allowed to draw the `adk code` status strip.
    adk_statusline: bool,
    /// The native id this child was resumed with, if it was a resume. A new
    /// session's native id is learned by the local host (its launch argv for
    /// Claude, its own probes for Codex and OpenCode) and read from there.
    resume_target: Option<String>,
    /// The local host runtime directory this child's wrapper registers in.
    host_dir: PathBuf,
    /// Shared shutdown signal for this child's tasks/threads.
    shutdown: Arc<Shutdown>,
    /// Kills the child process (graceful then forced) — cloned from the PTY child.
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// Polls the child's exit; also lets us detect a self-exited child.
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    /// Per-child terminal-state parser, fed from the PTY output. The reader
    /// thread holds a clone and `process()`es into it; [`focus`](FocusManager)
    /// reads it to repaint the child on focus.
    parser: Arc<Mutex<vt100::Parser>>,
    /// When set, the child's PTY-reader thread ALSO copies output to the local
    /// stdout (the PTY→stdout half of the local bridge). Cleared while detached
    /// so a background child's output never reaches the real terminal — it keeps
    /// feeding its vt100 parser + web mirror only. Shared with the reader thread.
    focused: Arc<AtomicBool>,
    /// The shared PTY writer (the stdin→PTY half of the local bridge writes
    /// here while this child is focused).
    pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// The PTY master, held for the child's whole life. Dropping it closes the
    /// pseudo-terminal and would SIGHUP the child, so we keep it here. While the
    /// child is focused the local-SIGWINCH pump resizes through it; background
    /// children keep their last size. (`MasterPty` is `Send` but not `Sync`; a
    /// `DetachedChild` is only ever owned by the single-threaded host loop, so
    /// `Send` is sufficient.)
    master: Box<dyn MasterPty + Send>,
    /// The background supervisor task driving teardown.
    supervisor: Option<TokioJoinHandle<()>>,
}

impl DetachedChild {
    /// Snapshot the child's current `vt100` screen as formatted bytes (an
    /// absolute repaint). Used by [`FocusManager::focus`] to repaint on focus
    /// (the same `contents_formatted()` trick the server snapshot uses).
    pub(super) fn screen_contents_formatted(&self) -> Vec<u8> {
        self.parser
            .lock()
            .map(|p| p.screen().contents_formatted())
            .unwrap_or_default()
    }

    /// A full-screen PTY keyframe for re-seeding the server's in-memory mirror
    /// snapshot after a redeploy wiped it: the child's `(cols, rows)` plus a
    /// self-clearing replay of retained scrollback and the current screen.
    /// Folding these into a fresh server vt100 grid reconstructs the browser's
    /// scrollback and visible screen, so a browser that (re)subscribes after the
    /// restart gets history instead of only the current frame. The byte buffer
    /// is empty only if the parser lock is poisoned (the caller skips those).
    ///
    /// Test-only: production re-seeds run through the mailbox (the forwarder
    /// builds the same bytes via [`repaint_snapshot`] as the sole `send_pty`
    /// producer). Kept as a tested helper that asserts the keyframe byte-shape —
    /// scrollback + visible-screen replay — that [`repaint_snapshot`] relies on.
    #[cfg(test)]
    pub(super) fn pty_keyframe(&self) -> (u16, u16, Vec<u8>) {
        self.parser
            .lock()
            .map(|mut p| {
                let (rows, cols) = p.screen().size();
                let bytes = mirror_replay_bytes(&mut p);
                (cols, rows, bytes)
            })
            .unwrap_or((80, 24, Vec::new()))
    }

    /// The working directory this child's `claude` runs in — the host persists
    /// it alongside the session id so a later `--host` can resume the session in
    /// the same place.
    pub(super) fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Launch arguments used for this child (checkpointed for `--host`
    /// resume), not the host-level trailing CLI arguments.
    pub(super) fn agent_args(&self) -> &[String] {
        &self.agent_args
    }

    /// How this child was invoked, and whether it was allowed to draw the `adk
    /// code` status strip. The host checkpoints both so restart recovery keeps
    /// the same harness.
    pub(super) fn launch(&self) -> (AgentLauncher, bool) {
        (self.launcher, self.adk_statusline)
    }

    /// The agent's native session id (resume target): the canonical local
    /// record's, else the one this child was resumed with. The host persists
    /// this for `--host` resume so a Codex session re-opens via
    /// `codex resume <real id>` rather than the live id.
    pub(super) fn resume_id(&self) -> Option<String> {
        // No authority database: no host ever recorded this session.
        let recorded = self.host_dir.join("authz.db").exists();
        recorded
            .then(|| crate::commands::local_host::authz::lookup(&self.host_dir, &self.live_id))
            .and_then(Result::ok)
            .flatten()
            .filter(|record| record.agent == self.agent)
            .and_then(|record| record.resume_id)
            .or_else(|| self.resume_target.clone())
    }

    /// Whether this child is currently bridged to the local terminal.
    #[cfg_attr(not(test), allow(dead_code))] // asserted by tests; W3 status bar reads it.
    pub(super) fn is_focused(&self) -> bool {
        self.focused.load(Ordering::SeqCst)
    }

    /// Mark this child focused: its PTY-reader thread starts copying output to
    /// local stdout. The stdin→PTY pump + resize are owned by the [`LocalBridge`]
    /// and attached separately.
    fn set_focused(&self, on: bool) {
        self.focused.store(on, Ordering::SeqCst);
    }

    /// Resize this child's PTY to `(cols, rows)`. Focus routing decides whether
    /// this may be called; the reader thread's vt100 parser is resized to match
    /// so a later repaint is laid out for the new geometry.
    pub(super) fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_size(rows, cols);
        }
    }

    /// The shared PTY writer — the target the [`LocalBridge`]'s stdin pump writes
    /// into while this child is focused.
    pub(super) fn writer(&self) -> Arc<Mutex<Box<dyn Write + Send>>> {
        Arc::clone(&self.pty_writer)
    }

    /// Has the child process already exited on its own?
    pub(super) fn has_exited(&self) -> bool {
        self.child
            .lock()
            .ok()
            .and_then(|mut c| c.try_wait().ok().flatten())
            .is_some()
    }

    /// Cancel the child: signal shutdown, terminate the process gracefully
    /// (SIGHUP via `kill()` → bounded wait → SIGKILL), and await the
    /// supervisor task so all wiring is torn down. Idempotent.
    pub(super) async fn cancel(&mut self) {
        self.shutdown.trigger();
        terminate_child(self.killer.as_ref(), &self.child).await;
        if let Some(handle) = self.supervisor.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for DetachedChild {
    fn drop(&mut self) {
        // Best-effort synchronous teardown if `cancel` was never awaited: signal
        // shutdown and force-kill so we never leak a child process.
        self.shutdown.trigger();
        if let Ok(mut c) = self.child.lock() {
            if matches!(c.try_wait(), Ok(None)) {
                force_kill(self.killer.as_ref(), c.process_id());
            }
        }
        if let Some(handle) = self.supervisor.take() {
            handle.abort();
        }
    }
}

/// Terminate a PTY child: SIGHUP (`kill()`), wait up to [`CHILD_TERM_GRACE`]
/// for it to exit, then SIGKILL its process group and poll up to
/// [`CHILD_KILL_REAP_TIMEOUT`] for the exit status. Never blocks the calling
/// thread: the host loop awaits this, so a blocking `wait()` on a child that
/// survives the signals would freeze local input and web control.
///
/// `portable_pty`'s cloned killer only sends SIGHUP on Unix (TerminateProcess on
/// Windows), so re-issuing `kill()` is not an escalation — a child that handles
/// SIGHUP without exiting needs an explicit SIGKILL.
async fn terminate_child(
    killer: &(dyn ChildKiller + Send + Sync),
    child: &Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
) {
    let pid = match child.lock() {
        Ok(mut c) => {
            // Already gone? Nothing to do.
            if matches!(c.try_wait(), Ok(Some(_))) {
                return;
            }
            c.process_id()
        }
        Err(_) => return,
    };

    let _ = killer.clone_killer().kill();
    if wait_for_exit(child, CHILD_TERM_GRACE).await {
        return;
    }

    force_kill(killer, pid);
    if !wait_for_exit(child, CHILD_KILL_REAP_TIMEOUT).await {
        live_error!(
            "conversations live: child process {} did not exit after SIGKILL",
            pid.map_or_else(|| "?".to_string(), |p| p.to_string())
        );
    }
}

/// Poll `try_wait` until the child exits or `timeout` elapses. Returns whether
/// it exited. A poisoned lock counts as exited: nothing more can be done.
async fn wait_for_exit(
    child: &Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let exited = child
            .lock()
            .ok()
            .map(|mut c| matches!(c.try_wait(), Ok(Some(_))))
            .unwrap_or(true);
        if exited {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// SIGKILL an unreaped child and its process group. `portable_pty` starts each
/// child with `setsid()`, so the child's pid is also its group id; killing the
/// group also stops grandchildren such as the native `codex` binary under its
/// Node wrapper. The caller must have seen the child unreaped, so the pid has
/// not been reused.
#[cfg(unix)]
fn force_kill(_killer: &(dyn ChildKiller + Send + Sync), pid: Option<u32>) {
    let Some(pid) = pid.and_then(|p| libc::pid_t::try_from(p).ok()) else {
        return;
    };
    // SAFETY: `killpg`/`kill` take plain integers and have no memory effects.
    unsafe {
        if libc::killpg(pid, libc::SIGKILL) != 0 {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// On Windows the cloned killer already calls TerminateProcess.
#[cfg(not(unix))]
fn force_kill(killer: &(dyn ChildKiller + Send + Sync), _pid: Option<u32>) {
    let _ = killer.clone_killer().kill();
}

/// Spawn a DETACHED child: the agent runs inside a real host wrapper
/// (`host wrap`) on its own PTY, so this machine's local host gives it its
/// canonical session record, local transcript history and the in-harness
/// same-machine handoff, exactly as for any other wrapped terminal. Nothing of
/// the session is registered with or mirrored to the central server. Returns a
/// cancellable [`DetachedChild`].
///
/// The PTY output is read on a dedicated thread that feeds a per-child local
/// [`vt100::Parser`] so the child can be repainted on focus. On PTY EOF (child
/// exit) the shared shutdown fires.
///
/// `start_focused` makes the child stream its PTY output to local stdout from
/// the very first byte (rather than starting detached and being repainted on a
/// later `focus()`). Use it ONLY for a child focused into an otherwise-empty
/// terminal — the seeded first session — so its `claude` banner renders
/// byte-identically to running `claude` directly, with NO `contents_formatted()`
/// reconstruction. It MUST be false whenever another child is currently focused,
/// or two children would write to stdout at once.
pub(super) async fn spawn_child(
    global: &GlobalOpts,
    args: &LiveArgs,
    config: SpawnConfig,
    start_focused: bool,
    host_dir: &Path,
) -> Result<DetachedChild> {
    // Size the PTY: the seeded/first child is focused into the local terminal, so
    // it tracks the real terminal size (its banner renders byte-exact and the
    // local view never garbles). Every other child starts in the background;
    // `--mirror-size` still sets its initial grid when given.
    let size = if start_focused {
        terminal_pty_size()
    } else {
        args.mirror_size
            .map(|(cols, rows)| PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap_or_else(terminal_pty_size)
    };
    let spawned = spawn_wrapped_pty(&config, args, &global.server, host_dir, size)?;
    live_error!(
        "live: started wrapped child {} in {}",
        config.session_id,
        host_dir.display()
    );
    let SpawnedPty {
        child,
        master,
        mut pty_reader,
        pty_writer,
    } = spawned;
    let killer = child.clone_killer();
    let child = Arc::new(Mutex::new(child));

    let shutdown = Shutdown::new();

    // Per-child terminal-state parser (rows, cols) — fed from PTY output.
    let parser = Arc::new(Mutex::new(new_vt100_parser(size.rows, size.cols)));

    // Local-bridge focus flag: when set, the reader thread ALSO copies PTY
    // output to local stdout. The seeded first session starts focused so its
    // banner streams live (exact, no repaint); every other child starts
    // detached and is bridged to the terminal only when `focus()` selects it.
    let focused = Arc::new(AtomicBool::new(start_focused));

    // --- PTY reader → (vt100 parser [+ local stdout iff focused]) -----------
    // It always reads the PTY (to detect EOF and feed the per-child vt100
    // parser) and, ONLY while `focused` is set, also writes the same bytes to
    // the local terminal — the PTY→stdout half of the local bridge.
    let pump_parser = {
        let shutdown = Arc::clone(&shutdown);
        let parser = Arc::clone(&parser);
        let focused = Arc::clone(&focused);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut stdout = std::io::stdout();
            // Keeps scrollback that a full-screen redraw would erase, so a
            // later focus repaints the history too.
            let mut erase_tracker = PtyEraseTracker::default();
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) => break, // PTY closed (child gone)
                    Ok(n) => {
                        if let Ok(mut p) = parser.lock() {
                            let _ = erase_tracker.process(&mut p, &buf[..n]);
                        }
                        // PTY→stdout, gated on focus. Failures are non-fatal.
                        if focused.load(Ordering::SeqCst) && stdout.write_all(&buf[..n]).is_ok() {
                            let _ = stdout.flush();
                        }
                    }
                    Err(_) => break,
                }
            }
            // The child's output ended → drive shutdown.
            if let Ok(mut p) = parser.lock() {
                let _ = erase_tracker.finish(&mut p);
            }
            shutdown.trigger();
        })
    };

    // --- Supervisor: join the reader on shutdown ----------------------------
    let supervisor = {
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move {
            shutdown.notify.notified().await;
            let _ = tokio::task::spawn_blocking(move || {
                let _ = pump_parser.join();
            })
            .await;
        })
    };

    Ok(DetachedChild {
        live_id: config.session_id.clone(),
        agent: config.agent,
        cwd: config.cwd.clone(),
        agent_args: config.agent_args.clone(),
        launcher: config.launcher,
        adk_statusline: config.adk_statusline,
        resume_target: (config.mode == SpawnMode::Resume).then(|| config.resume_target.clone()),
        host_dir: host_dir.to_path_buf(),
        shutdown,
        killer,
        child,
        parser,
        focused,
        pty_writer,
        master,
        supervisor: Some(supervisor),
    })
}

// ===========================================================================
// W2 — Focus + local bridge + repaint.
//
// Exactly ONE child in the registry is "focused" and bridged to the real
// terminal; the rest stay detached/web-only (W1 behaviour). The bridge is the
// pair of FOREGROUND pumps from `drive()`, now attach/detachable:
//
//   - PTY→stdout: NOT owned by the bridge — it lives in each child's reader
//     thread, gated on the child's `focused` flag (set/cleared by the manager).
//     Keeping it in the existing reader thread avoids a second reader on one PTY
//     and keeps the child's vt100 parser + web mirror fed whether focused or not.
//   - stdin→PTY: owned by the bridge as a per-attachment pump that forwards local
//     stdin into the focused child's PTY writer until detached.
//   - local-SIGWINCH: routed to the focused child only via `handle_resize`;
//     background children keep their last size.
// ===========================================================================

/// The local terminal bridge: the attach/detachable stdin→PTY pump. Exactly one
/// child is attached at a time. Attaching spawns a stdin pump that forwards
/// local stdin into that child's PTY writer; detaching stops the forwarding
/// (without killing the child or its web wiring) so the bridge can be re-attached
/// to a different child.
///
/// The PTY→stdout half is NOT here — it lives in the child's reader thread gated
/// on the child's `focused` flag (see [`spawn_child`]). The bridge only owns the
/// input half + a handle to the per-attachment stop flag, which is what makes
/// attach/detach reversible and unit-testable.
#[derive(Default)]
pub(super) struct LocalBridge {
    attached: Option<Attachment>,
    /// W3: when set, the bridge does NOT spawn its own raw stdin pump on
    /// [`attach`](Self::attach); instead it publishes the focused child's PTY
    /// writer into this shared slot, which the single [`spawn_mux_input`] pump
    /// reads to forward passthrough/literal bytes. This keeps exactly ONE stdin
    /// reader alive (the mux pump) so the prefix-key state machine sees every
    /// byte first. When `None` (W2 default), `attach` spawns the raw pump as
    /// before — used by the focus/repaint unit tests.
    mux_slot: Option<MuxWriterSlot>,
}

/// Shared "currently-focused child's PTY writer" slot. The [`spawn_mux_input`]
/// pump writes passthrough/literal bytes here; a focus switch swaps the inner
/// writer (or clears it on detach) so stdin is always routed to the focused
/// child only. `None` inner = nothing focused → the pump drops the byte.
pub(super) type MuxWriterSlot = Arc<Mutex<Option<Arc<Mutex<Box<dyn Write + Send>>>>>>;

/// Live state for one attachment: the per-attachment stop flag and the stdin
/// pump handle. Dropping the `Attachment` (on detach) signals the flag; the
/// parked stdin thread stops forwarding on its next read and is NOT joined (it
/// may be blocked in `stdin.read()` until the next keystroke — the same
/// trade-off the foreground `drive()` makes).
struct Attachment {
    /// `live_id` of the child currently bridged to local stdin.
    #[cfg_attr(not(test), allow(dead_code))] // read by `attached_id` (tests / W3 status bar).
    live_id: String,
    /// Set on detach → the stdin pump stops forwarding to this child's PTY.
    stop: Arc<AtomicBool>,
    /// The stdin→PTY pump thread (not joined on detach).
    pump_in: Option<JoinHandle<()>>,
}

impl LocalBridge {
    #[cfg_attr(not(test), allow(dead_code))] // W2 default path; mux mode is production now.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// A bridge in MUX mode: attaching publishes the focused child's writer into
    /// `slot` instead of spawning a raw stdin pump. The single [`spawn_mux_input`]
    /// pump owns stdin and writes passthrough/literal bytes into the slot, so the
    /// prefix-key state machine sees every byte first.
    pub(super) fn with_mux_slot(slot: MuxWriterSlot) -> Self {
        Self {
            attached: None,
            mux_slot: Some(slot),
        }
    }

    /// `live_id` of the child currently attached, if any.
    #[cfg_attr(not(test), allow(dead_code))] // asserted by tests; W3 status bar reads it.
    pub(super) fn attached_id(&self) -> Option<&str> {
        self.attached.as_ref().map(|a| a.live_id.as_str())
    }

    /// Is the bridge attached to any child?
    #[cfg_attr(not(test), allow(dead_code))] // asserted by tests; W3 status bar reads it.
    pub(super) fn is_attached(&self) -> bool {
        self.attached.is_some()
    }

    /// Attach the stdin→PTY pump to `child`: local stdin is forwarded into the
    /// child's PTY writer until [`detach`](Self::detach) (or re-attach to another
    /// child). If already attached, the previous attachment is detached first.
    fn attach(&mut self, child: &DetachedChild) {
        self.detach();

        // MUX mode: publish this child's writer into the shared slot for the
        // single mux-input pump; do NOT spawn a second stdin reader.
        if let Some(slot) = &self.mux_slot {
            if let Ok(mut s) = slot.lock() {
                *s = Some(child.writer());
            }
            self.attached = Some(Attachment {
                live_id: child.live_id.clone(),
                stop: Arc::new(AtomicBool::new(false)),
                pump_in: None,
            });
            return;
        }

        let stop = Arc::new(AtomicBool::new(false));
        let writer = child.writer();
        let pump_in = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut stdin = std::io::stdin();
                loop {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    match stdin.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            // Re-check after the (possibly long) blocking read so
                            // a keystroke that arrives just after detach is NOT
                            // forwarded to a child we no longer own.
                            if stop.load(Ordering::SeqCst) {
                                break;
                            }
                            let Ok(mut w) = writer.lock() else { break };
                            if w.write_all(&buf[..n]).is_err() {
                                break;
                            }
                            let _ = w.flush();
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        self.attached = Some(Attachment {
            live_id: child.live_id.clone(),
            stop,
            pump_in: Some(pump_in),
        });
    }

    /// Detach the stdin pump from the currently-focused child: signal the stop
    /// flag so it stops forwarding, and drop the handle. The child keeps running
    /// (web-mirrored, vt100-fed); the bridge is safe to re-attach elsewhere.
    fn detach(&mut self) {
        // MUX mode: clear the shared writer slot so the mux pump drops bytes
        // until the next focus attaches a new writer.
        if let Some(slot) = &self.mux_slot {
            if let Ok(mut s) = slot.lock() {
                *s = None;
            }
        }
        if let Some(mut a) = self.attached.take() {
            a.stop.store(true, Ordering::SeqCst);
            // Do not join: the thread may be parked in a blocking stdin read.
            // The stop flag guarantees it forwards nothing more.
            drop(a.pump_in.take());
        }
    }
}

/// Drives the single-focus model over a [`LocalBridge`] and a repaint sink.
///
/// At most ONE child is focused. [`focus`](Self::focus) switches focus:
///   1. detach the bridge + un-focus the previously-focused child,
///   2. repaint the target child's buffered `vt100` screen
///      (`contents_formatted()` — an absolute redraw, the same trick the server
///      snapshot uses) to the sink,
///   3. focus the target (its reader thread starts copying PTY→stdout) and
///      attach the bridge's stdin pump,
///   4. resize the target to the current local terminal size (focused child
///      only — background children keep their last size).
///
/// Raw mode is held for as long as ANY child is focused (a [`RawGuard`] is taken
/// on first focus and dropped on [`unfocus`](Self::unfocus)), so cooked mode is
/// always restored on detach/exit/panic — the same guarantee `drive()` gives.
pub(super) struct FocusManager<W: Write> {
    bridge: LocalBridge,
    /// `live_id` of the focused child (`None` = nothing focused locally).
    focused_id: Option<String>,
    /// Where repaints are written. Production = stdout; tests pass a buffer.
    out: W,
    /// Held while a child is focused so the terminal is in raw mode; dropped on
    /// unfocus to restore cooked mode.
    raw: Option<RawGuard>,
    /// Whether raw mode should actually be toggled. Disabled in tests (no TTY).
    use_raw: bool,
    /// Grid a child is resized back to when it LOSES focus (background/web-only
    /// size). `None` = leave a child at its last size on focus-out (the original
    /// behavior). Set from `--mirror-size` so web viewers see a roomier grid than
    /// the operator's local terminal once a session is parked in the background.
    mirror_size: Option<(u16, u16)>,
}

impl<W: Write> FocusManager<W> {
    /// Build a focus manager writing repaints to `out`. `use_raw` toggles real
    /// terminal raw mode (true in production, false in tests / non-TTY).
    #[cfg_attr(not(test), allow(dead_code))] // tests use the buffer sink; production uses with_sink_mux.
    pub(super) fn with_sink(out: W, use_raw: bool) -> Self {
        Self {
            bridge: LocalBridge::new(),
            focused_id: None,
            out,
            raw: None,
            use_raw,
            mirror_size: None,
        }
    }

    /// Set the background ("mirror") grid a child is resized back to when it loses
    /// focus. Builder-style so production call sites can thread `--mirror-size`
    /// without widening every constructor. `None` keeps the original "leave at
    /// last size" behavior.
    pub(super) fn with_mirror_size(mut self, mirror_size: Option<(u16, u16)>) -> Self {
        self.mirror_size = mirror_size;
        self
    }

    /// A focus manager whose bridge runs in MUX mode (W3): the stdin→PTY forward
    /// is owned by the single [`spawn_mux_input`] pump via the shared `slot`, so
    /// the prefix-key state machine sees every keystroke first. Focus switches
    /// publish/clear the focused child's writer in `slot`.
    pub(super) fn with_sink_mux(out: W, use_raw: bool, slot: MuxWriterSlot) -> Self {
        Self {
            bridge: LocalBridge::with_mux_slot(slot),
            focused_id: None,
            out,
            raw: None,
            use_raw,
            mirror_size: None,
        }
    }

    /// `live_id` of the currently-focused child, if any.
    pub(super) fn focused_id(&self) -> Option<&str> {
        self.focused_id.as_deref()
    }

    /// Read access to the bridge (for assertions / status).
    #[cfg_attr(not(test), allow(dead_code))] // asserted by tests; W3 status bar reads it.
    pub(super) fn bridge(&self) -> &LocalBridge {
        &self.bridge
    }

    /// Switch focus to `target`, detaching `prev` (the previously-focused child,
    /// if any and still present). Repaints `target`'s buffered screen, attaches
    /// the stdin pump, and resizes `target` to `size` (cols, rows) — the local
    /// terminal size. Background children are NOT resized.
    ///
    /// `prev` and `target` are passed explicitly (rather than looked up) so the
    /// borrow checker is happy with two `&DetachedChild` from one registry and so
    /// the switch is unit-testable without a registry.
    pub(super) fn focus(
        &mut self,
        prev: Option<&DetachedChild>,
        target: &DetachedChild,
        size: (u16, u16),
    ) -> Result<()> {
        // 1. Detach the old focus. The outgoing child is now background/web-only,
        //    so grow it back to the mirror grid (when `--mirror-size` is set) — its
        //    `master.resize` raises SIGWINCH and claude repaints at the larger size,
        //    which the web mirror picks up on the next frame.
        self.bridge.detach();
        if let Some(p) = prev {
            p.set_focused(false);
            if let Some((cols, rows)) = self.mirror_size {
                p.resize(cols, rows);
            }
        }

        // Enter raw mode on first focus (held until unfocus).
        if self.use_raw && self.raw.is_none() {
            self.raw = Some(RawGuard::enable()?);
        }

        // 2. Clear the terminal, then absolute-repaint the target's buffered
        //    screen. `contents_formatted()` optimizes away blank cells (it moves
        //    the cursor rather than writing spaces), so WITHOUT a clear, any cell
        //    blank in the new screen keeps the glyph that was there before — e.g.
        //    a stray fragment of the previously-focused session's banner. Erase
        //    the display + home the cursor first so the repaint starts clean.
        //
        //    SKIPPED for a child that is already focused (the seeded first
        //    session, spawned `start_focused`): its output has been streaming to
        //    stdout live since the first byte, so it's pixel-exact already — a
        //    reconstruction here would only double-draw and approximate the
        //    banner art. The repaint is for switching TO a background child.
        if !target.is_focused() {
            self.out
                .write_all(CLEAR_AND_HOME)
                .context("clearing screen before repaint")?;
            let painted = target.screen_contents_formatted();
            self.out
                .write_all(&painted)
                .context("repainting focused child screen")?;
            self.out.flush().context("flushing repaint")?;
        }

        // 3. Focus the target + attach the stdin pump.
        target.set_focused(true);
        self.bridge.attach(target);
        self.focused_id = Some(target.live_id.clone());

        // 4. Resize the newly-focused child to the local terminal size. ONLY the
        //    focused child is resized; background children keep their last size.
        let (cols, rows) = size;
        target.resize(cols, rows);

        Ok(())
    }

    /// Forward a local terminal resize to the focused child only. Background
    /// children keep their last size. No-op when nothing is focused.
    ///
    /// W2 ships + tests this; the local-SIGWINCH watcher that calls it on each
    /// terminal resize is wired in W3 (alongside the prefix-key state machine).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn handle_resize(&self, focused: Option<&DetachedChild>, cols: u16, rows: u16) {
        if let Some(child) = focused {
            if Some(child.live_id.as_str()) == self.focused_id.as_deref() {
                child.resize(cols, rows);
            }
        }
    }

    /// Drop local focus entirely: detach the bridge, un-focus the child, and
    /// restore cooked mode. Used when the focused child exits or the host stops.
    pub(super) fn unfocus(&mut self, focused: Option<&DetachedChild>) {
        self.bridge.detach();
        if let Some(child) = focused {
            child.set_focused(false);
            // Now web-only — restore the mirror grid (when set) so viewers keep
            // the roomier size rather than the operator's local terminal size.
            if let Some((cols, rows)) = self.mirror_size {
                child.resize(cols, rows);
            }
        }
        self.focused_id = None;
        // Restore cooked mode.
        self.raw = None;
    }
}

impl FocusManager<std::io::Stdout> {
    /// W3 production focus manager: repaints to stdout, real raw mode, and a
    /// MUX-mode bridge fed by the shared `slot` (the single [`spawn_mux_input`]
    /// pump owns stdin and routes passthrough/literal bytes through it).
    pub(super) fn stdout_mux(slot: MuxWriterSlot) -> Self {
        Self::with_sink_mux(std::io::stdout(), true, slot)
    }
}

// ===========================================================================
// W3 — Mux input pump: the SINGLE local-stdin reader.
//
// In the multiplexer, exactly one thread owns local stdin. Every byte is fed
// through the pure [`PrefixKey`](super::mux::PrefixKey) state machine FIRST:
//   - Passthrough(b) / Literal(b) → write `b` to the FOCUSED child's PTY writer
//     (read from the shared `MuxWriterSlot`; dropped if nothing is focused).
//   - Focus/FocusNext/FocusPrev/Create/Quit → forwarded to the host async loop
//     over an mpsc channel as a [`MuxCommand`]; the host performs the side
//     effect (focus switch, spawn, reap-all + exit). The prefix byte and command
//     bytes NEVER reach the focused child (except the prefix-prefix literal).
// ===========================================================================

/// A multiplexer command emitted by the stdin pump for the host async loop to
/// execute. The pure [`MuxAction`](super::mux::MuxAction) passthrough/literal
/// variants are handled inline by the pump (written to the focused child); only
/// the action variants that need host state become a [`MuxCommand`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MuxCommand {
    /// Switch focus to the child at this 0-based deterministic index.
    Focus(usize),
    /// Focus the next child in the deterministic ordering.
    FocusNext,
    /// Focus the previous child in the deterministic ordering.
    FocusPrev,
    /// Create a new session via the existing local spawn path.
    Create,
    /// Detach/quit the multiplexer (reap all children + exit).
    Quit,
}

/// Spawn the single mux-input stdin pump (a dedicated OS thread — stdin reads are
/// blocking). Owns a [`PrefixKey`](super::mux::PrefixKey); for each byte it either
/// writes the byte to the currently-focused child's PTY writer (via `slot`) or
/// sends a [`MuxCommand`] to `cmd_tx` for the host loop. Exits when stdin closes,
/// `shutdown` fires, or the command receiver is dropped.
pub(super) fn spawn_mux_input(
    slot: MuxWriterSlot,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<MuxCommand>,
    shutdown: Arc<Shutdown>,
) -> JoinHandle<()> {
    use super::mux::{MuxAction, PrefixKey};
    std::thread::spawn(move || {
        let mut pk = PrefixKey::new();
        let mut byte = [0u8; 1];
        let mut stdin = std::io::stdin();
        mux_debug("[mux] pump started (owns stdin)");
        loop {
            if shutdown.is_set() {
                mux_debug("[mux] shutdown set before read — exit");
                break;
            }
            match stdin.read(&mut byte) {
                Ok(0) => {
                    mux_debug("[mux] stdin closed (read 0) — exit");
                    break;
                }
                Ok(_) => {
                    if shutdown.is_set() {
                        mux_debug("[mux] shutdown set after read — exit");
                        break;
                    }
                    let action = pk.feed(byte[0]);
                    mux_debug(&format!("[mux] byte=0x{:02x} -> {action:?}", byte[0]));
                    match action {
                        MuxAction::Passthrough(b) | MuxAction::Literal(b) => {
                            write_to_focused(&slot, &[b]);
                        }
                        MuxAction::PassthroughMany(bytes) => {
                            write_to_focused(&slot, &bytes);
                        }
                        MuxAction::Swallowed => {}
                        MuxAction::Focus(i) => {
                            if cmd_tx.send(MuxCommand::Focus(i)).is_err() {
                                mux_debug("[mux] cmd_tx closed (Focus) — exit");
                                break;
                            }
                        }
                        MuxAction::FocusNext => {
                            if cmd_tx.send(MuxCommand::FocusNext).is_err() {
                                mux_debug("[mux] cmd_tx closed (FocusNext) — exit");
                                break;
                            }
                        }
                        MuxAction::FocusPrev => {
                            if cmd_tx.send(MuxCommand::FocusPrev).is_err() {
                                mux_debug("[mux] cmd_tx closed (FocusPrev) — exit");
                                break;
                            }
                        }
                        MuxAction::Create => {
                            if cmd_tx.send(MuxCommand::Create).is_err() {
                                mux_debug("[mux] cmd_tx closed (Create) — exit");
                                break;
                            }
                        }
                        MuxAction::Quit => {
                            let _ = cmd_tx.send(MuxCommand::Quit);
                            break;
                        }
                    }
                }
                Err(e) => {
                    mux_debug(&format!("[mux] stdin read error: {e} — exit"));
                    break;
                }
            }
        }
    })
}

/// Append a line to the mux debug log when `CONV_MUX_DEBUG` is set in the
/// environment. The log path is `CONV_MUX_DEBUG` itself when it looks like a
/// path, else `<tmp>/conv-mux-debug.log`. A no-op (and never panics) when the
/// var is unset — this is a diagnostic hatch for the prefix-key input path, not
/// a production code path. Writing to a file (not stdout/stderr) keeps it from
/// corrupting the raw-mode TUI; `tail -f` it from another terminal.
pub(super) fn mux_debug(msg: &str) {
    let Some(val) = std::env::var_os("CONV_MUX_DEBUG") else {
        return;
    };
    let path = {
        let p = std::path::PathBuf::from(&val);
        // A bare flag like `1`/`true` → default temp-dir log; anything that
        // looks like a path (has a separator or parent) is used verbatim.
        if p.parent()
            .map(|x| !x.as_os_str().is_empty())
            .unwrap_or(false)
        {
            p
        } else {
            std::env::temp_dir().join("conv-mux-debug.log")
        }
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// Write one complete input sequence to the currently-focused child's PTY
/// writer, if any. Keeping a multi-byte escape sequence in one write prevents
/// the child from observing its `ESC` introducer as a standalone key. A `None`
/// slot (nothing focused) drops the bytes — there is no child to receive them.
fn write_to_focused(slot: &MuxWriterSlot, bytes: &[u8]) {
    let writer = {
        let Ok(s) = slot.lock() else { return };
        s.clone()
    };
    if let Some(writer) = writer {
        if let Ok(mut w) = writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }
}

/// Open a PTY for a test.
///
/// Serializes the `openpty` syscall across the test binary's threads and
/// retries briefly on transient resource exhaustion (`Os { code: -6 }`). Many
/// tests in this module spawn PTY children concurrently, and under a parallel
/// `cargo test --workspace` run the simultaneous `openpty` calls intermittently
/// fail with `failed to openpty: Os { code: -6 }`. Serializing the syscall (the
/// PTY is still held by the test afterwards) removes the race. Test-only —
/// production allocation in `spawn_agent_pty` is single-shot per spawn.
#[cfg(test)]
fn open_test_pty(size: PtySize) -> portable_pty::PtyPair {
    use std::sync::Mutex;
    static PTY_OPEN_LOCK: Mutex<()> = Mutex::new(());

    let _guard = PTY_OPEN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let system = native_pty_system();
    let mut last_err = None;
    for _ in 0..40 {
        match system.openpty(size) {
            Ok(pair) => return pair,
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    panic!("test openpty failed after retries: {last_err:?}");
}

#[cfg(test)]
impl DetachedChild {
    /// Build a detached child around a real long-lived PTY process (`sleep`),
    /// WITHOUT any server wiring. Used by registry/cancel tests so the cancel
    /// machinery (SIGTERM→wait→SIGKILL), `has_exited`, and the parser can be
    /// exercised without a live server or `claude`.
    pub(super) fn test_sleeper(live_id: &str) -> Self {
        let pair = open_test_pty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("300");
        let child = pair.slave.spawn_command(cmd).expect("spawn sleep");
        drop(pair.slave);
        let killer = child.clone_killer();
        let pty_writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().expect("take writer")));
        DetachedChild {
            live_id: live_id.to_string(),
            agent: AgentKind::Claude,
            cwd: PathBuf::from("/tmp"),
            agent_args: Vec::new(),
            launcher: AgentLauncher::Opencode,
            adk_statusline: false,
            resume_target: None,
            host_dir: PathBuf::from("/nonexistent-conversations-host"),
            shutdown: Shutdown::new(),
            killer,
            child: Arc::new(Mutex::new(child)),
            parser: Arc::new(Mutex::new(new_vt100_parser(24, 80))),
            focused: Arc::new(AtomicBool::new(false)),
            pty_writer,
            master: pair.master,
            supervisor: None,
        }
    }

    /// Set server-resolved launch args on a test child so host checkpoint tests
    /// can cover restart persistence without a live server.
    pub(super) fn with_agent_args(mut self, args: &[&str]) -> Self {
        self.agent_args = args.iter().map(|arg| (*arg).to_string()).collect();
        self
    }

    /// Feed bytes directly into this child's vt100 parser (the reader thread does
    /// this from PTY output in production). Used to seed a screen before
    /// asserting on the repaint-on-focus.
    pub(super) fn feed_parser(&self, bytes: &[u8]) {
        let mut tracker = PtyEraseTracker::default();
        let _ = tracker.process(&mut self.parser.lock().unwrap(), bytes);
    }

    /// The child's PTY master size as `(cols, rows)`. Used to assert that a
    /// resize routed only to the focused child.
    pub(super) fn pty_size(&self) -> (u16, u16) {
        let s = self.master.get_size().expect("get_size");
        (s.cols, s.rows)
    }

    /// The child's vt100 screen size as `(cols, rows)` — kept in step with the
    /// PTY by [`resize`](Self::resize).
    pub(super) fn parser_size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.lock().unwrap().screen().size();
        (cols, rows)
    }
}

/// The blocking tail loop: tolerate the JSONL not existing yet, then bootstrap
/// (full read → register-derived offset) and tail message deltas via the
/// existing blocking [`ApiClient`], exactly like view-only.
fn resolve_tail_target(
    agent: AgentKind,
    target: &Path,
    tail_after: SystemTime,
    resume_target: Option<&str>,
    shutdown: &Shutdown,
) -> Option<ResolvedTailTarget> {
    match agent {
        AgentKind::Claude => {
            while !shutdown.is_set() && !target.exists() {
                std::thread::sleep(TAIL_POLL_INTERVAL);
            }
            (!shutdown.is_set()).then(|| ResolvedTailTarget {
                path: target.to_path_buf(),
                _claim: None,
            })
        }
        AgentKind::Codex => {
            if let Some(resume_target) = resume_target.filter(|id| !id.trim().is_empty()) {
                while !shutdown.is_set() {
                    if let Some(claim) = claim_newest_codex_jsonl_for_session(target, resume_target)
                    {
                        return Some(ResolvedTailTarget {
                            path: claim.path.clone(),
                            _claim: Some(claim),
                        });
                    }
                    std::thread::sleep(TAIL_POLL_INTERVAL);
                }
                return None;
            }
            let since = tail_after
                .checked_sub(Duration::from_secs(2))
                .unwrap_or(tail_after);
            while !shutdown.is_set() {
                if let Some(claim) = claim_newest_jsonl_since(target, since) {
                    return Some(ResolvedTailTarget {
                        path: claim.path.clone(),
                        _claim: Some(claim),
                    });
                }
                std::thread::sleep(TAIL_POLL_INTERVAL);
            }
            None
        }
        AgentKind::OpenCode | AgentKind::Zsh => None,
    }
}

#[derive(Debug)]
struct CodexRolloutCandidate {
    started_at: SystemTime,
    modified: SystemTime,
    path: PathBuf,
    session_id: String,
}

/// Read the immutable session metadata from the start of a Codex rollout.
///
/// `metadata.modified()` is unsuitable for deciding whether a rollout belongs
/// to a newly-spawned child: every active Codex process updates that timestamp,
/// so an older tab can look newer than the rollout that is still being created.
/// The `session_meta` timestamp is fixed at rollout creation and provides the
/// correct cutoff key.
fn codex_rollout_candidate(path: &Path) -> Option<CodexRolloutCandidate> {
    let file = std::fs::File::open(path).ok()?;
    let modified = file.metadata().ok()?.modified().ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        if value.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
            continue;
        }
        let payload = value.get("payload")?;
        let session_id = payload.get("id")?.as_str()?.to_string();
        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("timestamp").and_then(|v| v.as_str()))?;
        let started_at: SystemTime = chrono::DateTime::parse_from_rfc3339(timestamp)
            .ok()?
            .with_timezone(&chrono::Utc)
            .into();
        return Some(CodexRolloutCandidate {
            started_at,
            modified,
            path: path.to_path_buf(),
            session_id,
        });
    }
}

fn codex_rollout_candidates(root: &Path) -> Vec<CodexRolloutCandidate> {
    fn visit(dir: &Path, out: &mut Vec<CodexRolloutCandidate>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                visit(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Some(candidate) = codex_rollout_candidate(&path) {
                    out.push(candidate);
                }
            }
        }
    }

    let mut out = Vec::new();
    visit(root, &mut out);
    out
}

#[cfg(test)]
fn newest_jsonl_since(root: &Path, since: SystemTime) -> Option<PathBuf> {
    codex_rollout_candidates(root)
        .into_iter()
        .filter(|candidate| candidate.started_at >= since)
        .max_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.path.cmp(&b.path))
        })
        .map(|candidate| candidate.path)
}

#[cfg(test)]
fn newest_codex_jsonl_for_session(root: &Path, session_id: &str) -> Option<PathBuf> {
    codex_rollout_candidates(root)
        .into_iter()
        .filter(|candidate| candidate.session_id == session_id)
        .max_by(|a, b| {
            a.modified
                .cmp(&b.modified)
                .then_with(|| a.path.cmp(&b.path))
        })
        .map(|candidate| candidate.path)
}

fn claim_path(path: PathBuf) -> Option<CodexTailClaim> {
    let mut claims = codex_tail_claims().lock().ok()?;
    if !claims.insert(path.clone()) {
        return None;
    }
    Some(CodexTailClaim { path })
}

fn claim_newest_jsonl_since(root: &Path, since: SystemTime) -> Option<CodexTailClaim> {
    let mut candidates: Vec<_> = codex_rollout_candidates(root)
        .into_iter()
        .filter(|candidate| candidate.started_at >= since)
        .collect();
    candidates.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| b.path.cmp(&a.path))
    });
    candidates
        .into_iter()
        .find_map(|candidate| claim_path(candidate.path))
}

fn claim_newest_codex_jsonl_for_session(root: &Path, session_id: &str) -> Option<CodexTailClaim> {
    let mut candidates: Vec<_> = codex_rollout_candidates(root)
        .into_iter()
        .filter(|candidate| candidate.session_id == session_id)
        .collect();
    candidates.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.path.cmp(&a.path))
    });
    candidates
        .into_iter()
        .find_map(|candidate| claim_path(candidate.path))
}

fn tail_loop(
    live_id: &str,
    tail: TailConfig,
    shutdown: &Shutdown,
    agent_session_id: &Arc<Mutex<Option<String>>>,
) {
    if tail.agent == AgentKind::OpenCode {
        opencode_tail_loop(live_id, &tail, shutdown, agent_session_id);
        return;
    }
    let target = match resolve_tail_target(
        tail.agent,
        &tail.target,
        tail.tail_after,
        tail.resume_target.as_deref(),
        shutdown,
    ) {
        Some(path) => path,
        None => return,
    };
    let target_path = &target.path;
    let sink = LocalHistoryTap::new(
        tail.local_history.clone(),
        tail.agent,
        tail.resume_target.as_deref().unwrap_or_default(),
        target_path,
    );

    // Codex mints its OWN session id (it can't be told one at launch), so the
    // live id is not a valid `codex resume` target. Read the real id from the
    // rollout's `session_meta` into the shared cell so `--host` resume works.
    // Claude needs none of this (its live id IS the resume id), so it starts
    // "already captured".
    let mut codex_id_captured = !matches!(tail.agent, AgentKind::Codex);
    let mut history_sync = JsonlHistorySync::new();

    // Bootstrap: read the whole file once and push the backlog, deriving the
    // initial offset from the bytes read.
    let mut tailer = match std::fs::read(target_path) {
        Ok(buf) => {
            let offset = buf.len() as u64;
            let messages = super::decode_messages(&buf);
            if !messages.is_empty() {
                if let Err(error) = push_event_batches(&sink, live_id, &messages) {
                    history_sync.require_full_reconcile();
                    live_error!("live: initial transcript write failed for {live_id}: {error:#}");
                }
            }
            if !codex_id_captured {
                if let Some(id) = super::decode_codex_session_id(&buf) {
                    capture_agent_session_id(&id, agent_session_id);
                    codex_id_captured = true;
                }
            }
            Tailer::new(offset)
        }
        Err(_) => Tailer::new(0),
    };

    while !shutdown.is_set() {
        // The file may briefly vanish on rotation; keep polling.
        if let Ok(batch) = tailer.poll(target_path) {
            if let Err(error) = history_sync.sync(&sink, live_id, target_path, &batch) {
                live_error!("live: transcript sync failed for {live_id}: {error:#}");
            }
        }
        // Late capture of Codex's native id — only when the bootstrap read missed
        // it (e.g. the file was briefly empty/absent at first read). Re-reads the
        // rollout until the `session_meta` line appears, then stops; the common
        // case captured at bootstrap, so this never runs.
        if !codex_id_captured {
            if let Ok(buf) = std::fs::read(target_path) {
                if let Some(id) = super::decode_codex_session_id(&buf) {
                    capture_agent_session_id(&id, agent_session_id);
                    codex_id_captured = true;
                }
            }
        }
        // Sleep in slices so shutdown is observed promptly.
        let mut waited = Duration::ZERO;
        while waited < TAIL_POLL_INTERVAL && !shutdown.is_set() {
            std::thread::sleep(Duration::from_millis(50));
            waited += Duration::from_millis(50);
        }
    }
}

/// Record a session's native id in the shared cell (for `--host` resume).
fn capture_agent_session_id(native_id: &str, cell: &Arc<Mutex<Option<String>>>) {
    if let Ok(mut guard) = cell.lock() {
        *guard = Some(native_id.to_string());
    }
}

struct OpenCodeSessionClaim {
    id: String,
}

impl Drop for OpenCodeSessionClaim {
    fn drop(&mut self) {
        if let Some(claims) = OPENCODE_TAIL_CLAIMS.get() {
            if let Ok(mut claims) = claims.lock() {
                claims.remove(&self.id);
            }
        }
    }
}

fn claim_opencode_session(id: &str) -> Option<OpenCodeSessionClaim> {
    let claims = OPENCODE_TAIL_CLAIMS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut claims = claims.lock().ok()?;
    if !claims.insert(id.to_string()) {
        return None;
    }
    Some(OpenCodeSessionClaim { id: id.to_string() })
}

fn canonical_path_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn opencode_reference_matches(
    reference: &conversations_core::SessionFileRef,
    resume_target: Option<&str>,
    since: chrono::DateTime<chrono::Utc>,
    cwd: &Path,
) -> bool {
    if let Some(id) = resume_target.map(str::trim).filter(|id| !id.is_empty()) {
        // A resumed session predates this process, so its native id is the
        // authoritative selector and must bypass the new-session time filter.
        return reference.session_id == id;
    }

    reference
        .summary
        .as_ref()
        .and_then(|summary| summary.started_at)
        .is_some_and(|started| started >= since)
        && canonical_path_or_raw(Path::new(&reference.project_dir)) == cwd
}

/// Poll OpenCode's supported read-only database seam. User turns are stored as
/// soon as they appear; assistant rows wait for `time.completed` so history's
/// uuid de-duplication never freezes a partially-streamed response.
fn opencode_tail_loop(
    live_id: &str,
    tail: &TailConfig,
    shutdown: &Shutdown,
    agent_session_id: &Arc<Mutex<Option<String>>>,
) {
    let since = tail
        .tail_after
        .checked_sub(Duration::from_secs(2))
        .unwrap_or(tail.tail_after);
    let since: chrono::DateTime<chrono::Utc> = since.into();
    let cwd = canonical_path_or_raw(&tail.cwd);
    let resume_target = tail.resume_target.as_deref();
    let mut attempts = 0usize;

    let (reference, _claim) = loop {
        if shutdown.is_set() {
            return;
        }
        match crate::opencode_store::discover(&tail.target) {
            Ok(refs) => {
                let claimed = refs
                    .into_iter()
                    .filter(|reference| {
                        opencode_reference_matches(reference, resume_target, since, &cwd)
                    })
                    .find_map(|reference| {
                        claim_opencode_session(&reference.session_id)
                            .map(|claim| (reference, claim))
                    });
                if let Some(claimed) = claimed {
                    break claimed;
                }
                attempts += 1;
                if attempts == 20 {
                    live_error!(
                        "live: still waiting for an OpenCode session in {}",
                        cwd.display()
                    );
                }
            }
            Err(error) => {
                live_error!("live: could not inspect OpenCode sessions: {error:#}");
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    };

    capture_agent_session_id(&reference.session_id, agent_session_id);
    let sink = LocalHistoryTap::new(
        tail.local_history.clone(),
        tail.agent,
        &reference.session_id,
        &tail.target,
    );
    let mut sent = HashSet::new();
    loop {
        match crate::opencode_store::parse_session(&reference) {
            Ok(session) => {
                let batch = session
                    .messages
                    .into_iter()
                    .filter(|message| {
                        !sent.contains(&message.uuid)
                            && (message.role == Role::User
                                || message
                                    .raw
                                    .get("info")
                                    .and_then(|info| info.get("time"))
                                    .and_then(|time| time.get("completed"))
                                    .is_some())
                    })
                    .collect::<Vec<_>>();
                if !batch.is_empty() && push_event_batches(&sink, live_id, &batch).is_ok() {
                    sent.extend(batch.into_iter().map(|message| message.uuid));
                }
            }
            Err(error) => {
                live_error!("live: could not read OpenCode session: {error:#}");
            }
        }

        // Run one final read after child exit so OpenCode's last completed
        // assistant row is not lost between the final DB commit and shutdown.
        if shutdown.is_set() {
            break;
        }

        let mut waited = Duration::ZERO;
        while waited < Duration::from_millis(750) && !shutdown.is_set() {
            std::thread::sleep(Duration::from_millis(50));
            waited += Duration::from_millis(50);
        }
    }
}

/// Current terminal size as a [`PtySize`], defaulting to 80x24 when it can't be
/// read (e.g. not a TTY).
fn terminal_pty_size() -> PtySize {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Install a panic hook that restores the terminal out of raw mode BEFORE the
/// default hook prints the panic — otherwise a panic mid-session leaves the
/// user's terminal wedged in raw mode. Called by the host loop, which enters raw
/// mode whenever a child holds local focus.
pub(super) fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A history store whose writes can be made to fail.
    #[derive(Default)]
    struct FlakyHistory {
        state: Mutex<FlakyHistoryState>,
    }

    #[derive(Default)]
    struct FlakyHistoryState {
        failing: bool,
        stored_ids: Vec<String>,
        seen_ids: HashSet<String>,
    }

    impl FlakyHistory {
        fn set_failing(&self, failing: bool) {
            self.state.lock().unwrap().failing = failing;
        }

        fn stored_ids(&self) -> Vec<String> {
            self.state.lock().unwrap().stored_ids.clone()
        }
    }

    impl HistorySink for FlakyHistory {
        fn push_events(&self, _session_id: &str, messages: &[Message]) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            if state.failing {
                anyhow::bail!("history write failed");
            }
            for message in messages {
                if state.seen_ids.insert(message.uuid.clone()) {
                    state.stored_ids.push(message.uuid.clone());
                }
            }
            Ok(())
        }
    }

    fn claude_user_line(uuid: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": "history-reconcile",
            "timestamp": "2026-08-22T10:00:00.000Z",
            "message": { "role": "user", "content": text }
        })
        .to_string()
    }

    #[test]
    fn interactive_pty_emits_colors_when_host_inherits_no_color() {
        let spec = AgentCommand {
            program: "sh".into(),
            args: vec!["-c".into(), "if [ -n \"${NO_COLOR-}\" ]; then printf plain; else printf '\\033[31mred\\033[0m'; fi".into()],
            // Reproduce a host started from a non-interactive agent shell,
            // without mutating the test process's global environment.
            env: vec![("NO_COLOR".into(), "1".into())],
        };
        let pair = open_test_pty(PtySize {
            rows: 6,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
        let cmd = pty_command(&spec, Path::new("."));
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut output = Vec::new();
        pair.master
            .try_clone_reader()
            .unwrap()
            .read_to_end(&mut output)
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(output, b"\x1b[31mred\x1b[0m");
    }

    #[test]
    fn failed_history_write_rereads_complete_jsonl_history_in_order() {
        let mut transcript = tempfile::NamedTempFile::new().unwrap();
        writeln!(transcript, "{}", claude_user_line("one", "first")).unwrap();
        transcript.flush().unwrap();
        let initial_bytes = std::fs::read(transcript.path()).unwrap();

        let history = FlakyHistory::default();
        push_event_batches(
            &history,
            "history-reconcile",
            &super::super::decode_messages(&initial_bytes),
        )
        .unwrap();

        let mut tailer = Tailer::new(initial_bytes.len() as u64);
        let mut history_sync = JsonlHistorySync::new();

        // The tailer consumes the second turn while writes fail. Its byte
        // offset advances, so retrying only future deltas would lose it.
        writeln!(transcript, "{}", claude_user_line("two", "during failure")).unwrap();
        transcript.flush().unwrap();
        let during_failure = tailer.poll(transcript.path()).unwrap();
        history.set_failing(true);
        assert!(history_sync
            .sync(
                &history,
                "history-reconcile",
                transcript.path(),
                &during_failure,
            )
            .is_err());

        // The next sync re-reads the whole transcript, in order, exactly once.
        writeln!(transcript, "{}", claude_user_line("three", "after failure")).unwrap();
        transcript.flush().unwrap();
        let after_failure = tailer.poll(transcript.path()).unwrap();
        history.set_failing(false);
        history_sync
            .sync(
                &history,
                "history-reconcile",
                transcript.path(),
                &after_failure,
            )
            .unwrap();
        assert_eq!(history.stored_ids(), ["one", "two", "three"]);

        history_sync
            .sync(&history, "history-reconcile", transcript.path(), &[])
            .unwrap();
        assert_eq!(history.stored_ids(), ["one", "two", "three"]);
    }

    #[derive(Default)]
    struct WriteProbe {
        state: Arc<Mutex<WriteProbeState>>,
    }

    #[derive(Default)]
    struct WriteProbeState {
        writes: Vec<Vec<u8>>,
        flushes: usize,
    }

    impl Write for WriteProbe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.state.lock().unwrap().writes.push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.state.lock().unwrap().flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn mux_escape_sequence_is_written_as_one_pty_batch() {
        let probe = WriteProbe::default();
        let state = Arc::clone(&probe.state);
        let writer: Box<dyn Write + Send> = Box::new(probe);
        let slot = Arc::new(Mutex::new(Some(Arc::new(Mutex::new(writer)))));

        write_to_focused(&slot, b"\x1b[A");

        let state = state.lock().unwrap();
        assert_eq!(state.writes, vec![b"\x1b[A".to_vec()]);
        assert_eq!(state.flushes, 1);
    }

    fn live_args(agent_args: &[&str]) -> LiveArgs {
        LiveArgs {
            agent: AgentKind::Claude,
            launcher: AgentLauncher::Opencode,
            adk_statusline: false,
            host: false,
            no_resume: false,
            mirror_size: None,
            resume: None,
            claude_args: agent_args.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn agent_command_forwards_claude_permission_setting_for_new_and_resume() {
        let mut config = SpawnConfig::new_session(AgentKind::Claude, PathBuf::from("/work"));
        config.agent_args = vec!["--dangerously-skip-permissions".to_string()];
        for mode in [SpawnMode::New, SpawnMode::Resume] {
            config.mode = mode;
            let command = agent_command(&config, &live_args(&[]));
            assert_eq!(command.program, "claude");
            assert_eq!(
                command.args,
                vec![
                    if mode == SpawnMode::New {
                        "--session-id"
                    } else {
                        "--resume"
                    }
                    .to_string(),
                    if mode == SpawnMode::New {
                        config.session_id.clone()
                    } else {
                        config.resume_target.clone()
                    },
                    "--dangerously-skip-permissions".to_string(),
                ]
            );
        }
    }

    #[test]
    fn agent_command_keeps_claude_session_args_before_extra_args() {
        let config = SpawnConfig::new_session(AgentKind::Claude, PathBuf::from("/work"));
        let spec = agent_command(
            &config,
            &live_args(&[
                "--model",
                "claude-sonnet-4-5",
                "--permission-mode",
                "acceptEdits",
            ]),
        );

        assert_eq!(spec.program, "claude");
        assert_eq!(
            spec.args,
            vec![
                "--session-id".to_string(),
                config.session_id,
                "--model".to_string(),
                "claude-sonnet-4-5".to_string(),
                "--permission-mode".to_string(),
                "acceptEdits".to_string(),
            ]
        );
    }

    #[test]
    fn agent_command_forwards_extra_args_to_codex_new_sessions() {
        let config = SpawnConfig::new_session(AgentKind::Codex, PathBuf::from("/work"));
        let spec = agent_command(
            &config,
            &live_args(&[
                "--sandbox",
                "danger-full-access",
                "--ask-for-approval",
                "on-request",
            ]),
        );

        assert_eq!(spec.program, "codex");
        assert_eq!(
            spec.args,
            vec![
                "--no-alt-screen".to_string(),
                "--sandbox".to_string(),
                "danger-full-access".to_string(),
                "--ask-for-approval".to_string(),
                "on-request".to_string(),
            ]
        );
    }

    #[test]
    fn agent_command_puts_control_args_before_host_args() {
        let mut config = SpawnConfig::new_session(AgentKind::Codex, PathBuf::from("/work"));
        config.agent_args = vec![
            "--sandbox".to_string(),
            "workspace-write".to_string(),
            "--ask-for-approval".to_string(),
            "never".to_string(),
        ];
        let spec = agent_command(&config, &live_args(&["--search"]));

        assert_eq!(spec.program, "codex");
        assert_eq!(
            spec.args,
            vec![
                "--no-alt-screen".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--ask-for-approval".to_string(),
                "never".to_string(),
                "--search".to_string(),
            ]
        );
    }

    #[test]
    fn agent_command_does_not_duplicate_codex_no_alt_screen() {
        let config = SpawnConfig::new_session(AgentKind::Codex, PathBuf::from("/work"));
        let spec = agent_command(&config, &live_args(&["--no-alt-screen", "--search"]));

        assert_eq!(spec.program, "codex");
        assert_eq!(
            spec.args,
            vec!["--no-alt-screen".to_string(), "--search".to_string()]
        );
    }

    #[test]
    fn agent_command_forwards_extra_args_to_codex_resume_sessions() {
        let config = SpawnConfig::resume_session(
            AgentKind::Codex,
            PathBuf::from("/work"),
            "live-id".to_string(),
            "codex-native-id".to_string(),
        );
        let spec = agent_command(
            &config,
            &live_args(&["--sandbox", "danger-full-access", "--search"]),
        );

        assert_eq!(spec.program, "codex");
        assert_eq!(
            spec.args,
            vec![
                "--no-alt-screen".to_string(),
                "resume".to_string(),
                "codex-native-id".to_string(),
                "--sandbox".to_string(),
                "danger-full-access".to_string(),
                "--search".to_string(),
            ]
        );
    }

    #[test]
    fn managed_opencode_child_receives_tabs_override_without_losing_preferences() {
        for launcher in [AgentLauncher::Opencode, AgentLauncher::Adk] {
            for resume in [None, Some("ses_native")] {
                for statusline in [false, true] {
                    for inherited in [
                        None,
                        Some(
                            r#"{"theme":{"name":"catppuccin"},"tabs":{"mode":"on","scope":"global","layout":"vertical"},"prompt":{"paste":"full"},"keybinds":{"app.exit":["ctrl+c","ctrl+d"]}}"#,
                        ),
                        Some(r#"{"mouse":false}"#),
                    ] {
                        let mut spec = launch_agent_with_env(
                            AgentKind::OpenCode,
                            launcher,
                            "live-id",
                            resume,
                            vec![],
                            statusline,
                            |_| inherited.map(Into::into),
                        );
                        assert_eq!(
                            spec.program,
                            if launcher == AgentLauncher::Adk {
                                "adk"
                            } else {
                                "opencode"
                            }
                        );
                        assert_eq!(
                            spec.env
                                .iter()
                                .find(|(key, _)| key == "OC_STATUS")
                                .map(|(_, value)| value.as_str()),
                            (launcher == AgentLauncher::Adk).then_some(if statusline {
                                "1"
                            } else {
                                "0"
                            })
                        );
                        // Probe the real child environment through the legacy PTY builder,
                        // without starting OpenCode or changing process-global settings.
                        spec.program = "sh".into();
                        spec.args = vec![
                            "-c".into(),
                            "printf '%s' \"$OPENCODE_CLI_CONFIG_CONTENT\"".into(),
                        ];
                        let pair = open_test_pty(PtySize {
                            rows: 6,
                            cols: 80,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                        let mut child = pair
                            .slave
                            .spawn_command(pty_command(&spec, Path::new(".")))
                            .unwrap();
                        drop(pair.slave);
                        let mut output = Vec::new();
                        pair.master
                            .try_clone_reader()
                            .unwrap()
                            .read_to_end(&mut output)
                            .unwrap();
                        assert!(child.wait().unwrap().success());
                        let config: serde_json::Value = serde_json::from_slice(&output).unwrap();
                        assert_eq!(config["tabs"]["mode"], "off");
                        if let Some(inherited) = inherited {
                            let mut expected: serde_json::Value =
                                serde_json::from_str(inherited).unwrap();
                            expected["tabs"]["mode"] = "off".into();
                            assert_eq!(config, expected);
                        } else {
                            assert_eq!(config, serde_json::json!({"tabs": {"mode": "off"}}));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn managed_opencode_does_not_replace_invalid_inline_config() {
        for inherited in ["{broken", "[]", "null", r#"{"tabs":false}"#] {
            assert!(managed_opencode_cli_config(Some(inherited.into())).is_err());
            let spec = launch_agent_with_env(
                AgentKind::OpenCode,
                AgentLauncher::Opencode,
                "id",
                None,
                vec![],
                false,
                |_| Some(inherited.into()),
            );
            // The legacy child policy preserves invalid input for native validation.
            assert_eq!(
                spec.env,
                vec![("OPENCODE_CLI_CONFIG_CONTENT".into(), inherited.into())]
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_opencode_non_utf8_config_remains_invalid() {
        use std::os::unix::ffi::OsStringExt;

        let inherited = std::ffi::OsString::from_vec(vec![0xff]);
        assert!(managed_opencode_cli_config(Some(inherited.clone())).is_err());
        let spec = launch_agent_with_env(
            AgentKind::OpenCode,
            AgentLauncher::Opencode,
            "id",
            None,
            vec![],
            false,
            |_| Some(inherited),
        );
        let value = spec
            .env
            .iter()
            .find(|(key, _)| key == "OPENCODE_CLI_CONFIG_CONTENT")
            .expect("legacy child policy must preserve invalid non-UTF8 config");
        assert!(serde_json::from_str::<serde_json::Value>(&value.1).is_err());
        assert!(!value.1.contains('\u{fffd}'));
    }

    #[test]
    fn managed_non_opencode_leaves_inline_config_untouched() {
        for agent in [AgentKind::Claude, AgentKind::Codex, AgentKind::Zsh] {
            for resume in [None, Some("native-id")] {
                let spec = launch_agent_with_env(
                    agent,
                    AgentLauncher::Opencode,
                    "id",
                    resume,
                    vec![],
                    false,
                    |_| panic!("non-OpenCode launches must not read CLI config"),
                );
                assert!(spec.env.is_empty());
                assert_eq!(spec.program, agent.as_str());
            }
        }
    }

    #[test]
    fn agent_command_starts_and_resumes_opencode_sessions() {
        let mut new = SpawnConfig::new_session(AgentKind::OpenCode, PathBuf::from("/work"));
        new.agent_args = vec!["--auto".to_string()];
        let new_spec = agent_command(&new, &live_args(&["--model", "openai/gpt-5.6-terra"]));
        assert_eq!(new_spec.program, "opencode");
        assert_eq!(
            new_spec.args,
            vec![
                "--auto".to_string(),
                "--model".to_string(),
                "openai/gpt-5.6-terra".to_string(),
            ]
        );

        let mut resume = SpawnConfig::resume_session(
            AgentKind::OpenCode,
            PathBuf::from("/work"),
            "live-id".to_string(),
            "ses_native".to_string(),
        );
        resume.agent_args = vec!["--auto".to_string()];
        let resume_spec = agent_command(&resume, &live_args(&[]));
        assert_eq!(resume_spec.program, "opencode");
        assert_eq!(
            resume_spec.args,
            vec![
                "--session".to_string(),
                "ses_native".to_string(),
                "--auto".to_string(),
            ]
        );
        assert_eq!(resume.resume_target, "ses_native");
    }

    /// `adk code` takes every OpenCode argument after `--`; its own first
    /// positional selects a DevKit subcommand or agent profile, so a leaked
    /// `--session` would start the wrong thing entirely.
    #[test]
    fn agent_command_launches_opencode_through_adk_after_a_separator() {
        let mut new = SpawnConfig::new_session(AgentKind::OpenCode, PathBuf::from("/work"));
        new.launcher = AgentLauncher::Adk;
        let bare = agent_command(&new, &live_args(&[]));
        assert_eq!(bare.program, "adk");
        assert_eq!(bare.args, vec!["code".to_string()]);

        new.agent_args = vec!["--auto".to_string()];
        let with_args = agent_command(&new, &live_args(&["--print-logs"]));
        assert_eq!(
            with_args.args,
            vec![
                "code".to_string(),
                "--".to_string(),
                "--auto".to_string(),
                "--print-logs".to_string(),
            ]
        );

        let mut resume = SpawnConfig::resume_session(
            AgentKind::OpenCode,
            PathBuf::from("/work"),
            "live-id".to_string(),
            "ses_native".to_string(),
        );
        resume.launcher = AgentLauncher::Adk;
        resume.agent_args = vec!["--auto".to_string()];
        let resume_spec = agent_command(&resume, &live_args(&[]));
        assert_eq!(resume_spec.program, "adk");
        assert_eq!(
            resume_spec.args,
            vec![
                "code".to_string(),
                "--".to_string(),
                "--session".to_string(),
                "ses_native".to_string(),
                "--auto".to_string(),
            ]
        );
    }

    /// The status strip runs OpenCode inside a tmux pane, and in a live session
    /// that pane is what the web mirror shows — so it stays off unless the user
    /// opted in. Both directions are set explicitly: the child inherits this
    /// host's environment, so an ambient `OC_STATUS=0` must not quietly beat the
    /// opt-in.
    #[test]
    fn adk_status_strip_is_suppressed_unless_opted_in() {
        let mut config = SpawnConfig::new_session(AgentKind::OpenCode, PathBuf::from("/work"));
        config.launcher = AgentLauncher::Adk;
        let status = |config: &SpawnConfig| {
            agent_command(config, &live_args(&[]))
                .env
                .into_iter()
                .find(|(key, _)| key == "OC_STATUS")
        };
        assert_eq!(status(&config), Some(("OC_STATUS".into(), "0".into())));

        config.adk_statusline = true;
        assert_eq!(status(&config), Some(("OC_STATUS".into(), "1".into())));

        // Direct OpenCode does not receive the adk status-strip policy.
        config.launcher = AgentLauncher::Opencode;
        assert!(status(&config).is_none());
    }

    #[test]
    fn opencode_resume_matches_native_id_without_recency_or_path_requirements() {
        let started_at = chrono::Utc::now() - chrono::Duration::days(7);
        let reference = conversations_core::SessionFileRef {
            path: PathBuf::from("/opencode.db"),
            session_id: "ses_native".to_string(),
            agent: AgentKind::OpenCode,
            project_dir: "/an/old/project".to_string(),
            size_bytes: 0,
            modified: Some(started_at),
            summary: Some(conversations_core::SessionSummary {
                id: "ses_native".to_string(),
                agent: AgentKind::OpenCode,
                title: "Old session".to_string(),
                project_path: Some("/an/old/project".to_string()),
                git_branch: None,
                message_count: 1,
                started_at: Some(started_at),
                ended_at: Some(started_at),
            }),
        };

        assert!(opencode_reference_matches(
            &reference,
            Some("ses_native"),
            chrono::Utc::now(),
            Path::new("/different/current/project"),
        ));
        assert!(!opencode_reference_matches(
            &reference,
            None,
            chrono::Utc::now(),
            Path::new("/an/old/project"),
        ));
    }

    #[test]
    fn sanitize_strips_ctrl_c() {
        // Ctrl-C (0x03) must be removed; the rest passes.
        let out = sanitize_reply("ab\u{03}cd");
        assert_eq!(out, "abcd");
    }

    #[test]
    fn sanitize_strips_esc_csi_sequence() {
        // ESC [ B (down-arrow CSI) — the ESC and all control bytes go; the
        // printable `[` and `B` remain.
        let out = sanitize_reply("\x1b[B");
        assert_eq!(out, "[B");
    }

    #[test]
    fn sanitize_strips_c1_csi_introducer() {
        // U+009B is the single-byte CSI — a control-sequence introducer some
        // terminals honor like `ESC [`. It must be stripped along with the rest
        // of the C1 range (U+0080..=U+009F), leaving only the printable bytes.
        let out = sanitize_reply("a\u{9b}2Jb\u{85}c\u{80}d");
        assert_eq!(out, "a2Jbcd");
    }

    #[test]
    fn sanitize_removes_embedded_newlines() {
        let out = sanitize_reply("line1\nline2\r\nline3");
        assert_eq!(out, "line1line2line3");
    }

    #[test]
    fn sanitize_passes_plain_text_with_no_trailing_cr() {
        // The CR is no longer appended by the sanitizer — the caller sends it
        // as a separate, delayed keystroke .
        let out = sanitize_reply("hello world");
        assert_eq!(out, "hello world");
        assert_eq!(out.matches('\r').count(), 0);
    }

    #[test]
    fn sanitize_keeps_multibyte_utf8() {
        let out = sanitize_reply("café 🚀");
        assert_eq!(out, "café 🚀");
    }

    #[test]
    fn sanitize_caps_overlong_input() {
        let long = "x".repeat(REPLY_CAP + 500);
        let out = sanitize_reply(&long);
        // REPLY_CAP printable chars, no trailing CR.
        assert_eq!(out.chars().count(), REPLY_CAP);
        assert!(!out.ends_with('\r'));
    }

    #[test]
    fn sanitize_strips_eof_byte() {
        // Ctrl-D / EOF (0x04) must not reach claude.
        let out = sanitize_reply("done\u{04}");
        assert_eq!(out, "done");
    }

    #[test]
    fn encode_cwd_matches_claude_dash_encoding() {
        let p = Path::new("/Users/x/proj.dir/sub");
        assert_eq!(encode_cwd(p), "-Users-x-proj-dir-sub");
    }

    #[test]
    fn target_jsonl_path_composes_expected_layout() {
        let projects = Path::new("/home/me/.claude/projects");
        let cwd = Path::new("/Users/x/proj");
        let got = target_jsonl_path(projects, cwd, "abc-123");
        assert_eq!(
            got,
            Path::new("/home/me/.claude/projects/-Users-x-proj/abc-123.jsonl")
        );
    }
    fn repaint_snapshot(parser: &Arc<Mutex<vt100::Parser>>) -> (u16, u16, Vec<u8>) {
        let mut parser = parser.lock().unwrap_or_else(|e| e.into_inner());
        let (rows, cols) = parser.screen().size();
        (cols, rows, mirror_replay_bytes(&mut parser))
    }

    fn parser_with(bytes: &[u8]) -> Arc<Mutex<vt100::Parser>> {
        let parser = Arc::new(Mutex::new(new_vt100_parser(24, 80)));
        let mut tracker = PtyEraseTracker::default();
        let _ = tracker.process(&mut parser.lock().unwrap(), bytes);
        parser
    }

    #[test]
    fn repaint_snapshot_preserves_named_link_targets() {
        let parser =
            parser_with(b"\x1b]8;;https://example.com/project\x1b\\Project\x1b]8;;\x1b\\ plain");
        let (cols, rows, bytes) = repaint_snapshot(&parser);
        let mut browser = vt100::Parser::new(rows, cols, 100);
        browser.process(&bytes);
        assert_eq!(
            browser.screen().cell(0, 0).unwrap().hyperlink(),
            Some("https://example.com/project")
        );
        assert_eq!(browser.screen().cell(0, 8).unwrap().hyperlink(), None);
    }

    #[test]
    fn repaint_snapshot_replays_at_the_captured_geometry_after_resize() {
        let parser = parser_with(b"old size");
        {
            let mut locked = parser.lock().unwrap();
            locked.screen_mut().set_size(6, 40);
            locked.process(b"\x1b[2J\x1b[6;40HX");
        }
        let (cols, rows, bytes) = repaint_snapshot(&parser);
        // A later resize cannot change how the already-captured frame replays.
        parser.lock().unwrap().screen_mut().set_size(12, 80);
        let mut replay = vt100::Parser::new(rows, cols, 100);
        replay.process(&bytes);
        assert_eq!(
            replay.screen().contents(),
            "\n\n\n\n\n                                       X"
        );
        assert_eq!(replay.screen().cursor_position(), (5, 39));
    }

    #[test]
    fn repaint_preserves_existing_saved_cursor_in_alternate_screen() {
        let seed = b"\x1b[?1049h\x1b[2;3H\x1b7\x1b[4;5H";
        let mut source = vt100::Parser::new(6, 30, 0);
        source.process(seed);
        let mut viewer = vt100::Parser::new(6, 30, 0);
        viewer.process(seed);
        viewer.process(&mirror_replay_bytes(&mut source));
        viewer.process(b"\x1b8X");
        assert_eq!(viewer.screen().cell(1, 2).unwrap().contents(), "X");
    }

    #[test]
    fn repaint_bytes_lead_with_clear_and_reflect_parser() {
        let parser = parser_with(b"abc123");
        let (_, _, out) = repaint_snapshot(&parser);
        assert!(out
            .strip_prefix(b"\x1b[?47l")
            .unwrap_or(&out)
            .starts_with(CLEAR_AND_HOME_AND_SCROLLBACK));
        assert!(String::from_utf8_lossy(&out).contains("abc123"));
    }

    #[test]
    fn repaint_bytes_include_retained_scrollback_history() {
        let mut bytes = Vec::new();
        for i in 0..60 {
            bytes.extend_from_slice(format!("line{i}\r\n").as_bytes());
        }
        let parser = parser_with(&bytes);

        let (_, _, out) = repaint_snapshot(&parser);
        let rendered = String::from_utf8_lossy(&out);

        assert!(out
            .strip_prefix(b"\x1b[?47l")
            .unwrap_or(&out)
            .starts_with(CLEAR_AND_HOME_AND_SCROLLBACK));
        assert!(
            rendered.contains("line0"),
            "repaint must carry scrolled-off history, got: {rendered:?}"
        );
        assert!(
            rendered.contains("line59"),
            "repaint must carry the current visible screen, got: {rendered:?}"
        );
    }

    #[test]
    fn repaint_bytes_preserve_screen_erased_by_display_clear() {
        let parser =
            parser_with(b"codex page one\r\nanswer before clear\x1b[H\x1b[2Jcodex page two");

        let (_, _, out) = repaint_snapshot(&parser);
        let rendered = String::from_utf8_lossy(&out);

        assert!(out
            .strip_prefix(b"\x1b[?47l")
            .unwrap_or(&out)
            .starts_with(CLEAR_AND_HOME_AND_SCROLLBACK));
        assert!(
            rendered.contains("codex page one"),
            "a full-screen erase must preserve the erased viewport in replay history, got {rendered:?}"
        );
        assert!(
            rendered.contains("answer before clear"),
            "the second erased row must also survive"
        );
        assert!(
            rendered.contains("codex page two"),
            "repaint must carry the current visible screen, got: {rendered:?}"
        );
    }

    #[test]
    fn repaint_bytes_preserve_screen_erased_by_home_clear_below() {
        let parser =
            parser_with(b"codex page one\r\nanswer before clear\x1b[H\x1b[Jcodex page two");

        let (_, _, out) = repaint_snapshot(&parser);
        let rendered = String::from_utf8_lossy(&out);

        assert!(out
            .strip_prefix(b"\x1b[?47l")
            .unwrap_or(&out)
            .starts_with(CLEAR_AND_HOME_AND_SCROLLBACK));
        assert!(
            rendered.contains("codex page one"),
            "a home+erase-below redraw must preserve the erased viewport in replay history, got {rendered:?}"
        );
        assert!(
            rendered.contains("answer before clear"),
            "the second erased row must also survive"
        );
        assert!(
            rendered.contains("codex page two"),
            "repaint must carry the current visible screen, got: {rendered:?}"
        );
    }

    #[test]
    fn repaint_bytes_keep_text_omitted_by_a_codex_reflow() {
        let mut seeded = b"old codex screen".to_vec();
        seeded.extend_from_slice(CODEX_TRANSCRIPT_REBUILD);
        seeded.extend_from_slice(b"fresh codex screen");
        let parser = parser_with(&seeded);

        let (_, _, out) = repaint_snapshot(&parser);
        let rendered = String::from_utf8_lossy(&out);

        assert!(
            rendered.contains("old codex screen"),
            "a rebuild must retain earlier text it omits, got {rendered:?}"
        );
        assert!(
            rendered.contains("fresh codex screen"),
            "repaint must carry the rebuilt screen, got {rendered:?}"
        );
    }

    #[test]
    fn repaint_bytes_preserve_history_across_a_bare_scrollback_purge() {
        // The narrowness guard for the test above: an `ESC[3J` that is NOT part
        // of a whole transcript rebuild is still suppressed, so an application
        // redrawing an answer in place keeps its history.
        let parser = parser_with(b"old codex screen\x1b[H\x1b[2J\x1b[3Jfresh codex screen");

        let (_, _, out) = repaint_snapshot(&parser);
        let rendered = String::from_utf8_lossy(&out);

        assert!(
            rendered.contains("old codex screen"),
            "a bare scrollback purge must not clear retained history, got {rendered:?}"
        );
        assert!(
            rendered.contains("fresh codex screen"),
            "repaint must retain the visible screen after CSI 3J, got {rendered:?}"
        );
    }

    #[test]
    fn erase_tracker_forwards_a_split_codex_transcript_rebuild_to_the_mirror() {
        let mut parser = new_vt100_parser(6, 80);
        let mut tracker = PtyEraseTracker::default();

        // Split inside the signature, where the ED2 half arrives one read before
        // the ED3 half that retires the copy it erased.
        let first = tracker.process(
            &mut parser,
            b"old transcript\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3",
        );
        let second = tracker.process(&mut parser, b"J\x1b[Hreflowed transcript");
        let mirrored = [first, second].concat();

        assert!(
            tracker.replaced,
            "send the merged result as an explicit snapshot"
        );
        let mut viewer = new_vt100_parser(6, 80);
        viewer.process(&mirrored);
        let viewer_replay = mirror_replay_bytes(&mut viewer);
        assert!(String::from_utf8_lossy(&viewer_replay).contains("old transcript"));

        let replay = mirror_replay_bytes(&mut parser);
        let rendered = String::from_utf8_lossy(&replay);
        assert!(
            rendered.contains("old transcript"),
            "the host keyframe must retain the omitted viewport, got {rendered:?}"
        );
        assert!(
            rendered.contains("reflowed transcript"),
            "the host keyframe must include the post-reflow viewport, got {rendered:?}"
        );
    }

    /// The host leg of the chain, on real bytes: run the captured interactive
    /// Codex stream through the tracker exactly as the reader thread does.
    ///
    /// Rebuilds become explicit reconciled snapshots. The capture contains
    /// changing startup metadata; only the final idle startup screen survives.
    /// Retention of actual answers is tested separately.
    #[test]
    fn codex_capture_replaces_startup_chrome_in_host_history() {
        use base64::Engine as _;

        // The sanitized capture is shared from the server's fixture directory.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../server/tests/fixtures/pty/codex_interactive_reflow.jsonl");
        let text = std::fs::read_to_string(&path).expect("read the codex reflow capture");

        let mut parser = new_vt100_parser(24, 80);
        let mut tracker = PtyEraseTracker::default();
        let mut raw = Vec::new();
        let mut mirrored = Vec::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let frame: serde_json::Value = serde_json::from_str(line).expect("captured frame");
            let rows = frame["rows"].as_u64().expect("rows") as u16;
            let cols = frame["cols"].as_u64().expect("cols") as u16;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(frame["b64"].as_str().expect("b64"))
                .expect("base64");
            parser.screen_mut().set_size(rows, cols);
            raw.extend_from_slice(&decoded);
            mirrored.extend_from_slice(&tracker.process(&mut parser, &decoded));
        }

        let count_rebuilds = |bytes: &[u8]| {
            bytes
                .windows(CODEX_TRANSCRIPT_REBUILD.len())
                .filter(|window| *window == CODEX_TRANSCRIPT_REBUILD)
                .count()
        };
        assert_eq!(
            count_rebuilds(&raw),
            4,
            "the capture must still carry its rebuilds"
        );
        assert_eq!(
            count_rebuilds(&mirrored),
            0,
            "destructive child rebuilds are normalized"
        );

        let replay = mirror_replay_bytes(&mut parser);
        let banners = String::from_utf8_lossy(&replay)
            .lines()
            .filter(|line| line.contains("OpenAI Codex"))
            .count();
        assert_eq!(
            banners, 1,
            "superseded startup chrome must not remain in history"
        );
    }

    #[test]
    fn erase_tracker_suppresses_a_bare_scrollback_purge_from_the_mirror() {
        let mut parser = new_vt100_parser(6, 80);
        let mut tracker = PtyEraseTracker::default();

        let mirrored = tracker.process(&mut parser, b"old answer\x1b[H\x1b[2J\x1b[3Jnew answer");

        assert_eq!(mirrored, b"old answer\x1b[H\x1b[2Jnew answer");
        assert!(
            !mirrored.windows(4).any(|window| window == b"\x1b[3J"),
            "outside a rebuild, the browser must not receive a scrollback purge"
        );
    }

    #[test]
    fn repaint_bytes_restore_cursor_to_the_live_position() {
        // Regression: `mirror_replay_bytes` rebuilt the grid cells but never
        // re-emitted the cursor, so a repaint (focus switch / shed / reconnect)
        // healed the screen text but stranded the cursor at the end of the
        // replay. Put the cursor at a known spot (row 3, col 5 → CSI 3;5H) and
        // assert the repaint ends by moving there with the cursor shown.
        let parser = parser_with(b"abc\x1b[3;5H");
        let (_, _, out) = repaint_snapshot(&parser);
        let rendered = String::from_utf8_lossy(&out);
        assert!(
            rendered.contains("\x1b[?25h"),
            "repaint must restore cursor visibility, got: {rendered:?}"
        );
        assert!(
            out.ends_with(b"\x1b[3;5H"),
            "repaint must end by moving the cursor to the live position, got: {rendered:?}"
        );
    }

    #[test]
    fn repaint_bytes_preserve_hidden_cursor() {
        // A child that hid its cursor (`\x1b[?25l`) must have that state mirrored
        // by the repaint — otherwise a repaint would flash the cursor back on.
        let parser = parser_with(b"working\x1b[?25l");
        let (_, _, out) = repaint_snapshot(&parser);
        assert!(
            String::from_utf8_lossy(&out).contains("\x1b[?25l"),
            "repaint must keep the cursor hidden when the source hid it"
        );
    }

    // --- terminate_child: SIGTERM → wait → SIGKILL --------------------------

    type TestKiller = Box<dyn ChildKiller + Send + Sync>;
    type TestChild = Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>;

    /// Spawn a trivial long-lived child (`sleep 300`) under a PTY and return the
    /// killer, shared child handle and master to keep the PTY open. No server,
    /// no `claude`, no local pumps — just the cancel machinery under test.
    fn spawn_sleeper() -> (TestKiller, TestChild, Box<dyn MasterPty + Send>) {
        let pair = open_test_pty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("300");
        let child = pair.slave.spawn_command(cmd).expect("spawn sleep");
        drop(pair.slave);
        let killer = child.clone_killer();
        (killer, Arc::new(Mutex::new(child)), pair.master)
    }

    #[tokio::test]
    async fn terminate_child_kills_a_live_process() {
        let (killer, child, _master) = spawn_sleeper();
        // Sanity: it's running.
        assert!(
            child.lock().unwrap().try_wait().unwrap().is_none(),
            "sleeper should still be running before terminate"
        );

        terminate_child(killer.as_ref(), &child).await;

        // After terminate it must be reaped (exited).
        assert!(
            child.lock().unwrap().try_wait().unwrap().is_some(),
            "terminate_child must reap the process (SIGTERM→wait→SIGKILL)"
        );
    }

    #[tokio::test]
    async fn terminate_child_is_noop_on_already_exited() {
        let pair = open_test_pty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
        let cmd = CommandBuilder::new("true");
        let mut child = pair.slave.spawn_command(cmd).expect("spawn true");
        drop(pair.slave);
        let killer = child.clone_killer();
        // Let `true` exit.
        let _ = child.wait();
        let child = Arc::new(Mutex::new(child));

        // Must return promptly without error on an already-dead process.
        terminate_child(killer.as_ref(), &child).await;
        assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    }

    /// A child that survives SIGHUP (the only signal portable-pty's cloned
    /// killer sends) must still be SIGKILLed, together with the grandchildren in
    /// its process group, without blocking the caller.
    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_child_kills_a_sighup_ignoring_process_group() {
        let pair = open_test_pty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg("trap '' HUP; sleep 300 & wait");
        let child = pair.slave.spawn_command(cmd).expect("spawn sh");
        drop(pair.slave);
        let killer = child.clone_killer();
        let child: TestChild = Arc::new(Mutex::new(child));

        let pid = child.lock().unwrap().process_id().expect("child pid");
        let grandchild = wait_for_first_child(pid).await;

        tokio::time::timeout(
            CHILD_TERM_GRACE + CHILD_KILL_REAP_TIMEOUT + Duration::from_secs(5),
            terminate_child(killer.as_ref(), &child),
        )
        .await
        .expect("terminate_child must not hang on a SIGHUP-ignoring child");

        assert!(
            child.lock().unwrap().try_wait().unwrap().is_some(),
            "the SIGHUP-ignoring child must be reaped"
        );
        assert!(
            wait_for_process_gone(grandchild).await,
            "the grandchild in the child's process group must be killed"
        );
    }

    /// Find the `sleep` the shell forked. Once it exists the shell has already
    /// run its `trap`, so the SIGHUP it is about to receive is ignored.
    #[cfg(unix)]
    async fn wait_for_first_child(parent: u32) -> libc::pid_t {
        for _ in 0..200 {
            let out = std::process::Command::new("pgrep")
                .arg("-P")
                .arg(parent.to_string())
                .output()
                .expect("run pgrep");
            if let Some(pid) = String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the shell never forked its grandchild");
    }

    /// The killed grandchild is orphaned; under a PID 1 that never reaps (some
    /// CI containers) it stays a zombie, which `kill(pid, 0)` still reports.
    #[cfg(unix)]
    async fn wait_for_process_gone(pid: libc::pid_t) -> bool {
        for _ in 0..40 {
            // SAFETY: signal 0 only checks that the pid exists.
            if unsafe { libc::kill(pid, 0) } != 0 || is_zombie(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[cfg(target_os = "linux")]
    fn is_zombie(pid: libc::pid_t) -> bool {
        // `/proc/<pid>/stat` is `pid (comm) state ...`; `comm` may contain spaces.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next().map(|s| s == "Z"))
            })
            .unwrap_or(false)
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn is_zombie(_pid: libc::pid_t) -> bool {
        false
    }

    // --- per-child vt100 parser maintenance ---------------------------------

    #[test]
    fn vt100_parser_tracks_fed_pty_bytes() {
        // The detached child keeps a per-child vt100 parser fed from PTY output
        // so a later wave can repaint it. Assert the maintained screen reflects
        // bytes we feed and that contents_formatted() yields a non-empty
        // absolute repaint.
        let parser = Arc::new(Mutex::new(new_vt100_parser(24, 80)));
        {
            let mut p = parser.lock().unwrap();
            p.process(b"hello multiplexer");
        }
        let formatted = parser
            .lock()
            .map(|p| p.screen().contents_formatted())
            .unwrap_or_default();
        assert!(
            !formatted.is_empty(),
            "a fed parser must produce a non-empty absolute repaint"
        );
        let visible = String::from_utf8_lossy(&formatted);
        assert!(
            visible.contains("hello multiplexer"),
            "the maintained screen must contain the fed text, got: {visible:?}"
        );
    }

    // --- DetachedChild: cancel + exit detection -----------------------------

    #[tokio::test]
    async fn detached_child_cancel_terminates_process_and_is_idempotent() {
        let mut child = DetachedChild::test_sleeper("live-1");
        assert!(!child.has_exited(), "sleeper should be running");

        child.cancel().await;
        assert!(
            child.has_exited(),
            "cancel must terminate the child process"
        );

        // Idempotent: a second cancel must not panic or hang.
        child.cancel().await;
        assert!(child.has_exited());
    }

    #[test]
    fn detached_child_screen_contents_formatted_reflects_parser() {
        let child = DetachedChild::test_sleeper("live-2");
        // Feed the per-child parser directly (the reader thread does this from
        // PTY output in production).
        child.parser.lock().unwrap().process(b"painted screen");
        let visible = String::from_utf8_lossy(&child.screen_contents_formatted()).into_owned();
        assert!(
            visible.contains("painted screen"),
            "screen_contents_formatted must reflect the maintained parser, got: {visible:?}"
        );
        // Tear down the sleeper so the test leaves no process behind.
        let _ = child.killer.clone_killer().kill();
    }

    // --- W2: LocalBridge attach/detach reversibility ------------------------
    // The bridge owns the stdin→PTY half. Attaching records the target; detaching
    // drops the forwarding handle so nothing more is forwarded; re-attaching to a
    // different child re-targets cleanly. These assert on the attach STATE (the
    // forwarding handle is owned by the `Attachment`, dropped on detach), which
    // is the observable, race-free proxy for "forwarding stopped".

    #[test]
    fn bridge_attach_then_detach_is_reversible() {
        let a = DetachedChild::test_sleeper("bridge-a");
        let mut bridge = LocalBridge::new();
        assert!(!bridge.is_attached(), "starts detached");
        assert_eq!(bridge.attached_id(), None);

        bridge.attach(&a);
        assert!(bridge.is_attached(), "attach records the target");
        assert_eq!(bridge.attached_id(), Some("bridge-a"));

        bridge.detach();
        assert!(!bridge.is_attached(), "detach drops the forwarding handle");
        assert_eq!(
            bridge.attached_id(),
            None,
            "after detach no child is bridged to stdin"
        );

        let _ = a.killer.clone_killer().kill();
    }

    #[test]
    fn bridge_reattach_retargets_to_another_child() {
        let a = DetachedChild::test_sleeper("bridge-a");
        let b = DetachedChild::test_sleeper("bridge-b");
        let mut bridge = LocalBridge::new();

        bridge.attach(&a);
        assert_eq!(bridge.attached_id(), Some("bridge-a"));
        // Re-attach to a different child without an explicit detach: the bridge
        // detaches the old one first, then targets the new one.
        bridge.attach(&b);
        assert_eq!(
            bridge.attached_id(),
            Some("bridge-b"),
            "re-attach must re-target stdin→PTY to the new child"
        );
        assert!(bridge.is_attached());

        bridge.detach();
        assert!(!bridge.is_attached());

        let _ = a.killer.clone_killer().kill();
        let _ = b.killer.clone_killer().kill();
    }

    // --- W2: repaint produces an ABSOLUTE redraw ----------------------------

    #[test]
    fn focus_repaints_target_screen_to_sink() {
        let child = DetachedChild::test_sleeper("paint-1");
        child.feed_parser(b"REPAINT ME");
        let expected = child.screen_contents_formatted();
        assert!(!expected.is_empty());

        // Capture sink, raw mode OFF (no TTY in tests).
        let mut fm = FocusManager::with_sink(Vec::<u8>::new(), false);
        fm.focus(None, &child, (80, 24)).expect("focus");

        // The sink must be a clear-and-home followed by exactly the absolute
        // repaint bytes the child's vt100 buffer produces (contents_formatted).
        // The clear is what prevents stale cells from a prior screen surviving.
        let mut want = CLEAR_AND_HOME.to_vec();
        want.extend_from_slice(&expected);
        assert_eq!(
            fm.out, want,
            "focus must clear the screen then write the child's contents_formatted() repaint"
        );
        let visible = String::from_utf8_lossy(&fm.out);
        assert!(
            visible.contains("REPAINT ME"),
            "repaint must reconstruct the buffered screen, got: {visible:?}"
        );

        let _ = child.killer.clone_killer().kill();
    }

    // --- keyframe: dims + scrollback replay for the redeploy re-seed --------

    #[test]
    fn pty_keyframe_reports_dims_and_painted_screen() {
        let child = DetachedChild::test_sleeper("kf-1");
        // Dims come from the child's PTY size (test_sleeper spawns 80×24).
        let (cols0, rows0, _) = child.pty_keyframe();
        assert_eq!((cols0, rows0), (80, 24));

        child.feed_parser(b"KEYFRAME ME");
        let (cols, rows, bytes) = child.pty_keyframe();
        assert_eq!((cols, rows), (80, 24));
        assert!(!bytes.is_empty(), "a painted screen yields a keyframe");
        assert!(bytes
            .strip_prefix(b"\x1b[?47l")
            .unwrap_or(&bytes)
            .starts_with(CLEAR_AND_HOME_AND_SCROLLBACK));
        let painted = String::from_utf8_lossy(&bytes);
        assert!(
            painted.contains("KEYFRAME ME"),
            "the keyframe must carry the visible screen, got: {painted:?}"
        );

        let _ = child.killer.clone_killer().kill();
    }

    #[test]
    fn mirror_replay_restores_opencode_mouse_reporting() {
        let mut source = new_vt100_parser(24, 80);
        source.process(b"\x1b[?1003h\x1b[?1006hOpenCode");
        let frame = mirror_replay_bytes(&mut source);
        let mut viewer = new_vt100_parser(24, 80);
        viewer.process(&frame);

        assert_eq!(
            viewer.screen().mouse_protocol_mode(),
            source.screen().mouse_protocol_mode(),
            "a reseeded viewer must keep sending wheel reports to OpenCode"
        );
        assert_eq!(
            viewer.screen().mouse_protocol_encoding(),
            source.screen().mouse_protocol_encoding()
        );
    }

    #[test]
    fn pty_keyframe_includes_retained_scrollback_history() {
        let child = DetachedChild::test_sleeper("kf-scrollback");
        let mut bytes = Vec::new();
        for i in 0..60 {
            bytes.extend_from_slice(format!("line{i}\r\n").as_bytes());
        }
        child.feed_parser(&bytes);

        let (_cols, _rows, frame) = child.pty_keyframe();
        let painted = String::from_utf8_lossy(&frame);

        assert!(frame
            .strip_prefix(b"\x1b[?47l")
            .unwrap_or(&frame)
            .starts_with(CLEAR_AND_HOME_AND_SCROLLBACK));
        assert!(
            painted.contains("line0"),
            "keyframe must carry scrolled-off history, got: {painted:?}"
        );
        assert!(
            painted.contains("line59"),
            "keyframe must carry the current visible screen, got: {painted:?}"
        );

        let _ = child.killer.clone_killer().kill();
    }

    #[test]
    fn pty_keyframe_preserves_screen_erased_by_display_clear() {
        let child = DetachedChild::test_sleeper("kf-codex-clear");
        child.feed_parser(b"codex page one\r\nanswer before clear\x1b[H\x1b[2Jcodex page two");

        let (_cols, _rows, frame) = child.pty_keyframe();
        let painted = String::from_utf8_lossy(&frame);

        assert!(frame
            .strip_prefix(b"\x1b[?47l")
            .unwrap_or(&frame)
            .starts_with(CLEAR_AND_HOME_AND_SCROLLBACK));
        assert!(
            painted.contains("codex page one"),
            "keyframe must carry the erased viewport in scrollback, got: {painted:?}"
        );
        assert!(
            painted.contains("codex page two"),
            "keyframe must carry the current visible screen, got: {painted:?}"
        );

        let _ = child.killer.clone_killer().kill();
    }

    #[test]
    fn pty_keyframe_preserves_screen_erased_by_home_clear_below() {
        let child = DetachedChild::test_sleeper("kf-codex-home-clear-below");
        child.feed_parser(b"codex page one\r\nanswer before clear\x1b[H\x1b[Jcodex page two");

        let (_cols, _rows, frame) = child.pty_keyframe();
        let painted = String::from_utf8_lossy(&frame);

        assert!(frame
            .strip_prefix(b"\x1b[?47l")
            .unwrap_or(&frame)
            .starts_with(CLEAR_AND_HOME_AND_SCROLLBACK));
        assert!(
            painted.contains("codex page one"),
            "keyframe must carry the home+erase-below viewport in scrollback, got: {painted:?}"
        );
        assert!(
            painted.contains("codex page two"),
            "keyframe must carry the current visible screen, got: {painted:?}"
        );

        let _ = child.killer.clone_killer().kill();
    }

    // --- W2: focus switch updates focused-id + detaches the previous focus ---

    #[test]
    fn focus_switch_updates_id_and_detaches_previous() {
        let a = DetachedChild::test_sleeper("focus-a");
        let b = DetachedChild::test_sleeper("focus-b");
        let mut fm = FocusManager::with_sink(Vec::<u8>::new(), false);

        // Focus A.
        fm.focus(None, &a, (80, 24)).expect("focus a");
        assert_eq!(fm.focused_id(), Some("focus-a"));
        assert!(a.is_focused(), "A's reader thread now copies PTY→stdout");
        assert_eq!(fm.bridge().attached_id(), Some("focus-a"));

        // Switch focus to B: A must be un-focused + detached, B focused.
        fm.focus(Some(&a), &b, (80, 24)).expect("focus b");
        assert_eq!(fm.focused_id(), Some("focus-b"));
        assert!(
            !a.is_focused(),
            "previous focus is detached (no PTY→stdout)"
        );
        assert!(b.is_focused(), "new target is focused");
        assert_eq!(
            fm.bridge().attached_id(),
            Some("focus-b"),
            "stdin pump re-targeted to B"
        );

        // Unfocus drops focus entirely.
        fm.unfocus(Some(&b));
        assert_eq!(fm.focused_id(), None);
        assert!(!b.is_focused());
        assert!(!fm.bridge().is_attached());

        let _ = a.killer.clone_killer().kill();
        let _ = b.killer.clone_killer().kill();
    }

    // --- W2: resize routes to the FOCUSED child only ------------------------

    #[test]
    fn focus_resizes_only_the_focused_child() {
        let focused = DetachedChild::test_sleeper("resize-focused");
        let background = DetachedChild::test_sleeper("resize-bg");
        // Both start at the test_sleeper default 80x24.
        assert_eq!(focused.pty_size(), (80, 24));
        assert_eq!(background.pty_size(), (80, 24));

        let mut fm = FocusManager::with_sink(Vec::<u8>::new(), false);
        // Focusing resizes the focused child to the supplied local size …
        fm.focus(None, &focused, (120, 40)).expect("focus");
        assert_eq!(
            focused.pty_size(),
            (120, 40),
            "focused child PTY resized to local terminal size"
        );
        assert_eq!(
            focused.parser_size(),
            (120, 40),
            "focused child vt100 parser resized to match"
        );
        // … and the background child is untouched.
        assert_eq!(
            background.pty_size(),
            (80, 24),
            "background child keeps its last size"
        );

        // A subsequent local resize routes to the focused child only.
        fm.handle_resize(Some(&focused), 100, 30);
        assert_eq!(focused.pty_size(), (100, 30), "live resize hits focused");
        // A resize that names a non-focused child is ignored.
        fm.handle_resize(Some(&background), 10, 10);
        assert_eq!(
            background.pty_size(),
            (80, 24),
            "handle_resize must NOT resize a non-focused child"
        );

        let _ = focused.killer.clone_killer().kill();
        let _ = background.killer.clone_killer().kill();
    }

    // --- mirror-size: a child losing focus is resized to the mirror grid -------

    #[test]
    fn focus_out_resizes_outgoing_child_to_mirror_size() {
        let a = DetachedChild::test_sleeper("mirror-a");
        let b = DetachedChild::test_sleeper("mirror-b");

        // Mirror grid 120×40; the local terminal we "focus into" is 80×24.
        let mut fm =
            FocusManager::with_sink(Vec::<u8>::new(), false).with_mirror_size(Some((120, 40)));

        // Focus A at the local size.
        fm.focus(None, &a, (80, 24)).expect("focus a");
        assert_eq!(a.pty_size(), (80, 24), "focused child tracks local size");

        // Switch to B: A becomes background → grown to the mirror grid; B (now
        // focused) tracks the local size.
        fm.focus(Some(&a), &b, (80, 24)).expect("focus b");
        assert_eq!(
            a.pty_size(),
            (120, 40),
            "outgoing child grows to the mirror grid on focus-out"
        );
        assert_eq!(
            a.parser_size(),
            (120, 40),
            "outgoing child's vt100 parser matches the mirror grid"
        );
        assert_eq!(b.pty_size(), (80, 24), "newly-focused child tracks local");

        // Unfocus B → it too is restored to the mirror grid.
        fm.unfocus(Some(&b));
        assert_eq!(
            b.pty_size(),
            (120, 40),
            "unfocus restores the child to the mirror grid"
        );

        let _ = a.killer.clone_killer().kill();
        let _ = b.killer.clone_killer().kill();
    }

    /// Create an empty file at `path` (making parents) with a fixed mtime.
    fn touch_at(path: &Path, modified: SystemTime) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let f = std::fs::File::create(path).unwrap();
        f.set_modified(modified).unwrap();
    }

    /// Create a Codex rollout with independent immutable start time and mutable
    /// file mtime. Active rollouts keep changing mtime, but assignment must use
    /// the `session_meta` timestamp.
    fn write_codex_rollout_started_at(
        path: &Path,
        session_id: &str,
        started_at: SystemTime,
        modified: SystemTime,
    ) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let timestamp = chrono::DateTime::<chrono::Utc>::from(started_at).to_rfc3339();
        let line = serde_json::json!({
            "timestamp": timestamp,
            "type": "session_meta",
            "payload": { "id": session_id }
        });
        std::fs::write(path, format!("{line}\n")).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(modified).unwrap();
    }

    fn write_codex_rollout_at(path: &Path, session_id: &str, started_at: SystemTime) {
        write_codex_rollout_started_at(path, session_id, started_at, started_at);
    }

    #[test]
    fn newest_jsonl_since_picks_newest_recursively_after_cutoff() {
        // Codex names its own `rollout-*.jsonl` under dated nested dirs, so the
        // tailer can't precompute the path — it grabs the newest rollout whose
        // immutable session start is at/after the spawn time.
        let root = std::env::temp_dir().join(format!("codex-tail-{}", Uuid::new_v4()));
        let day = root.join("2026").join("06").join("20");
        let since = SystemTime::now();
        let before = since.checked_sub(Duration::from_secs(60)).unwrap();
        let after = since.checked_add(Duration::from_secs(60)).unwrap();
        let newest = since.checked_add(Duration::from_secs(120)).unwrap();

        write_codex_rollout_at(&day.join("old.jsonl"), "old", before);
        write_codex_rollout_at(&day.join("mid.jsonl"), "mid", after);
        write_codex_rollout_at(&day.join("nested").join("win.jsonl"), "win", newest);
        touch_at(
            &root.join("notes.txt"),
            newest.checked_add(Duration::from_secs(60)).unwrap(),
        ); // newest overall but not .jsonl → ignored

        let got = newest_jsonl_since(&root, since).expect("a qualifying file");
        assert_eq!(got.file_name().unwrap().to_str().unwrap(), "win.jsonl");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn newest_jsonl_since_ignores_an_old_active_rollout_with_a_fresh_mtime() {
        let root = std::env::temp_dir().join(format!("codex-tail-active-{}", Uuid::new_v4()));
        let day = root.join("2026").join("07").join("24");
        let since = SystemTime::now();
        let old_start = since.checked_sub(Duration::from_secs(300)).unwrap();
        let new_start = since.checked_add(Duration::from_secs(1)).unwrap();
        let active_mtime = since.checked_add(Duration::from_secs(120)).unwrap();

        // The older tab wrote most recently. The former mtime-based selector
        // incorrectly attached a new live tab to this rollout.
        write_codex_rollout_started_at(
            &day.join("old-active.jsonl"),
            "old-active",
            old_start,
            active_mtime,
        );
        write_codex_rollout_at(&day.join("new-child.jsonl"), "new-child", new_start);

        let got = newest_jsonl_since(&root, since).expect("new rollout qualifies");
        assert_eq!(
            got.file_name().unwrap().to_str().unwrap(),
            "new-child.jsonl"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_rollout_claim_is_exclusive_and_released() {
        let root = std::env::temp_dir().join(format!("codex-tail-claim-{}", Uuid::new_v4()));
        let path = root.join("2026").join("07").join("24").join("only.jsonl");
        let since = SystemTime::now();
        let started_at = since.checked_add(Duration::from_secs(1)).unwrap();
        write_codex_rollout_at(&path, "only", started_at);

        let first = claim_newest_jsonl_since(&root, since).expect("first tailer claims rollout");
        assert_eq!(first.path, path);
        assert!(
            claim_newest_jsonl_since(&root, since).is_none(),
            "a second tailer must not share the claimed rollout"
        );

        drop(first);
        let reclaimed = claim_newest_jsonl_since(&root, since).expect("claim releases with tailer");
        assert_eq!(reclaimed.path, path);
        drop(reclaimed);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn newest_codex_jsonl_for_session_matches_native_id() {
        let root = std::env::temp_dir().join(format!("codex-resume-tail-{}", Uuid::new_v4()));
        let day = root.join("2026").join("06").join("20");
        let base = SystemTime::now();
        let older = base.checked_sub(Duration::from_secs(60)).unwrap();
        let newer = base.checked_add(Duration::from_secs(60)).unwrap();
        let newest = base.checked_add(Duration::from_secs(120)).unwrap();

        write_codex_rollout_at(&day.join("target-old.jsonl"), "codex-target", older);
        write_codex_rollout_at(&day.join("unrelated-newest.jsonl"), "other-session", newest);
        write_codex_rollout_at(
            &day.join("nested").join("target-new.jsonl"),
            "codex-target",
            newer,
        );

        let got = newest_codex_jsonl_for_session(&root, "codex-target").expect("matching rollout");
        assert_eq!(
            got.file_name().unwrap().to_str().unwrap(),
            "target-new.jsonl"
        );
        assert!(newest_codex_jsonl_for_session(&root, "missing").is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn newest_jsonl_since_returns_none_when_nothing_qualifies() {
        let root = std::env::temp_dir().join(format!("codex-tail-empty-{}", Uuid::new_v4()));
        let since = SystemTime::now();
        // An old .jsonl (before the cutoff) and a recent non-.jsonl — neither qualifies.
        write_codex_rollout_at(
            &root.join("old.jsonl"),
            "old",
            since.checked_sub(Duration::from_secs(120)).unwrap(),
        );
        touch_at(
            &root.join("recent.txt"),
            since.checked_add(Duration::from_secs(120)).unwrap(),
        );
        assert!(newest_jsonl_since(&root, since).is_none());

        std::fs::remove_dir_all(&root).ok();
    }
    #[test]
    fn mirror_replay_preserves_distinct_osc8_targets_for_identical_labels() {
        let mut parser = vt100::Parser::new(3, 80, 20);
        parser.process(b"\x1b]8;;https://example.com/one\x1b\\PR #216\x1b]8;;\x1b\\ \x1b]8;;https://example.com/two\x07PR #216\x1b]8;;\x07");
        let replay = mirror_replay_bytes(&mut parser);
        let mut viewer = vt100::Parser::new(3, 80, 20);
        viewer.process(&replay);
        assert_eq!(
            viewer.screen().cell(0, 0).unwrap().hyperlink(),
            Some("https://example.com/one")
        );
        assert_eq!(
            viewer.screen().cell(0, 8).unwrap().hyperlink(),
            Some("https://example.com/two")
        );
        viewer.process(b"ordinary");
        assert!(viewer.screen().cell(0, 15).unwrap().hyperlink().is_none());
    }
    #[test]
    fn mirror_replay_restores_open_hyperlink_for_continued_output() {
        let mut parser = vt100::Parser::new(3, 80, 20);
        parser.process(b"\x1b]8;;https://example.com/active\x1b\\A");
        let mut viewer = vt100::Parser::new(3, 80, 20);
        viewer.process(&mirror_replay_bytes(&mut parser));
        viewer.process(b"B");
        assert_eq!(
            viewer.screen().cell(0, 1).unwrap().hyperlink(),
            Some("https://example.com/active")
        );
    }

    /// Real Codex rollout shapes (session_meta, message, function_call, output).
    fn codex_rollout(native: &str, lines: &[serde_json::Value]) -> String {
        let mut out = serde_json::json!({
            "timestamp": "2026-11-02T09:00:00.000Z", "type": "session_meta",
            "payload": {"id": native, "cwd": "/work"}
        })
        .to_string();
        out.push('\n');
        for line in lines {
            out.push_str(&line.to_string());
            out.push('\n');
        }
        out
    }

    fn codex_text(role: &str, kind: &str, at: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": at, "type": "response_item",
            "payload": {"type": "message", "role": role,
                        "content": [{"type": kind, "text": text}]}
        })
    }

    fn codex_session_a() -> Vec<serde_json::Value> {
        vec![
            codex_text(
                "user",
                "input_text",
                "2026-11-02T09:00:01.000Z",
                "Please add the invoices migration",
            ),
            serde_json::json!({"timestamp": "2026-11-02T09:00:02.000Z", "type": "response_item",
                "payload": {"type": "function_call", "name": "shell", "call_id": "c1",
                            "arguments": "{\"command\":[\"psql\",\"-f\",\"invoices.sql\"]}"}}),
            serde_json::json!({"timestamp": "2026-11-02T09:00:03.000Z", "type": "response_item",
                "payload": {"type": "function_call_output", "call_id": "c1", "output": "CREATE TABLE ok"}}),
            codex_text(
                "assistant",
                "output_text",
                "2026-11-02T09:00:04.000Z",
                "The invoices migration is applied",
            ),
        ]
    }

    struct RunningTail {
        shutdown: Arc<Shutdown>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl RunningTail {
        fn start(root: &Path, native: &str, history_dir: &Path) -> Self {
            let shutdown = Shutdown::new();
            let root = root.to_path_buf();
            let native = native.to_owned();
            let history_dir = history_dir.to_path_buf();
            let stop = shutdown.clone();
            let worker = std::thread::spawn(move || {
                tail_loop(
                    "11111111-2222-4333-8444-555555555555",
                    TailConfig {
                        agent: AgentKind::Codex,
                        target: root,
                        cwd: PathBuf::from("/work"),
                        tail_after: SystemTime::now(),
                        resume_target: Some(native.clone()),
                        local_history: history_dir,
                    },
                    &stop,
                    &Arc::new(Mutex::new(Some(native))),
                );
            });
            Self {
                shutdown,
                worker: Some(worker),
            }
        }

        fn stop(mut self) {
            self.shutdown.trigger();
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn wait_for_local_messages(dir: &Path, session: &str, count: usize) -> Vec<Message> {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let found = crate::commands::local_host::history::History::open(dir)
                .and_then(|history| history.messages(session, 100))
                .unwrap_or_default();
            if found.len() >= count || std::time::Instant::now() > deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    const LIVE: &str = "11111111-2222-4333-8444-555555555555";

    fn private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn production_tailer_stores_the_bound_codex_session_locally() {
        use crate::commands::local_host::history::History;
        let root = tempfile::tempdir().unwrap();
        let history = private_tempdir();
        let day = root.path().join("2026/11/02");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-a.jsonl"),
            codex_rollout("native-a", &codex_session_a()),
        )
        .unwrap();
        // A newer, unrelated rollout in the same tree must never be adopted.
        let unrelated = vec![codex_text(
            "user",
            "input_text",
            "2026-11-02T10:00:01.000Z",
            "zebra unrelated secret",
        )];
        std::fs::write(
            day.join("rollout-b.jsonl"),
            codex_rollout("native-b", &unrelated),
        )
        .unwrap();

        let tail = RunningTail::start(root.path(), "native-a", history.path());
        let stored = wait_for_local_messages(history.path(), LIVE, 4);
        assert_eq!(stored.len(), 4, "backlog persisted locally");
        use conversations_core::ContentBlock;
        assert!(stored.iter().any(|m| m
            .blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "shell"))));
        assert!(stored.iter().any(|m| m.blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { content, .. } if content.contains("CREATE TABLE")))));

        let db = History::open(history.path()).unwrap();
        let hits = db.search("invoices migration", Some(LIVE), 10).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(
            db.search("zebra", None, 10).unwrap().is_empty(),
            "unrelated transcript leaked"
        );
        assert!(db
            .search("invoices", Some("another-session"), 10)
            .unwrap()
            .is_empty());
        tail.stop();
    }

    #[test]
    fn production_tailer_ingests_appends_rotation_and_restart_without_duplicates() {
        use crate::commands::local_host::history::History;
        let root = tempfile::tempdir().unwrap();
        let history = private_tempdir();
        let day = root.path().join("2026/11/02");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-a.jsonl");
        std::fs::write(&rollout, codex_rollout("native-a", &codex_session_a())).unwrap();
        let tail = RunningTail::start(root.path(), "native-a", history.path());
        assert_eq!(wait_for_local_messages(history.path(), LIVE, 4).len(), 4);

        // Incremental append.
        let mut lines = codex_session_a();
        lines.push(codex_text(
            "user",
            "input_text",
            "2026-11-02T09:01:00.000Z",
            "Now add the receipts index",
        ));
        std::fs::write(&rollout, codex_rollout("native-a", &lines)).unwrap();
        let after_append = wait_for_local_messages(history.path(), LIVE, 5);
        assert_eq!(after_append.len(), 5);
        assert_eq!(after_append[4].text, "Now add the receipts index");

        // Native rotation: the file is replaced by a shorter one with one new
        // message. Earlier local history is kept, nothing is duplicated.
        let rotated = vec![codex_text(
            "user",
            "input_text",
            "2026-11-02T09:02:00.000Z",
            "Fresh transcript after rotation",
        )];
        std::fs::write(&rollout, codex_rollout("native-a", &rotated)).unwrap();
        let after_rotation = wait_for_local_messages(history.path(), LIVE, 6);
        assert_eq!(
            after_rotation.len(),
            6,
            "{:?}",
            after_rotation.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        tail.stop();

        // Restart over the same file: backfill is a no-op.
        let again = RunningTail::start(root.path(), "native-a", history.path());
        std::thread::sleep(Duration::from_millis(1500));
        again.stop();
        let db = History::open(history.path()).unwrap();
        assert_eq!(db.messages(LIVE, 100).unwrap().len(), 6);
        assert_eq!(db.search("receipts", Some(LIVE), 10).unwrap().len(), 1);
        assert_eq!(db.search("rotation", Some(LIVE), 10).unwrap().len(), 1);
    }
}
