//! Shared agent launch and transcript helpers for host-managed terminals:
//! the argv a local launch runs (`launch_agent_for_local_host`, handoffs, the
//! managed OpenCode configuration) and the transcript tail that keeps a
//! bound session's local history current ([`BoundTranscript`]).

use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
#[cfg(test)]
use conversations_core::terminal::CODEX_TRANSCRIPT_REBUILD;
use conversations_core::{AgentKind, AgentLauncher, Message, Role};
use tokio::sync::Notify;

use super::Tailer;

/// Maximum number of UTF-8 chars kept from a single web reply before the
/// mandatory trailing CR. Anything longer is truncated.
const REPLY_CAP: usize = 4096;

/// Poll interval for the JSONL tail.
const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Bound each history write during a full re-read. A single oversized message
/// is still written alone; splitting a normalized message would change it.
const HISTORY_RECONCILE_BATCH_BYTES: usize = 4 * 1024 * 1024;

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
/// Unlike Claude, Codex does not accept the conversations session id at launch.
/// A fresh child therefore has to discover its rollout under
/// `~/.codex/sessions`. Without claims, two sessions started close together
/// can both choose the same "newest" file and push one transcript into two
/// sessions. The claim lives for the tailer's lifetime and is released on exit so a
/// later resume may attach to the same rollout.
static CODEX_TAIL_CLAIMS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// OpenCode uses one shared SQLite database, so claims reserve native session
/// ids rather than transcript paths when several sessions start together.
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

/// Build a self-clearing replay for the browser/server PTY mirror: retained
/// scrollback (oldest→newest) followed by the current visible screen. This is
/// intentionally stronger than `contents_formatted()` because a server restart
/// loses its in-memory PTY snapshot; the host has the lossless per-session
/// parser and can re-seed both the visible screen and scrollback.
pub(crate) fn mirror_replay_bytes(parser: &mut vt100::Parser) -> Vec<u8> {
    conversations_core::terminal::replay_snapshot(parser)
}

/// The transcript file to tail, once it exists: Claude's own path, or the
/// Codex rollout of the native session (else the newest one started since
/// launch), claimed so two sessions never tail one file. `None` on shutdown.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

    #[test]
    fn managed_opencode_does_not_replace_invalid_inline_config() {
        for inherited in ["{broken", "[]", "null", r#"{"tabs":false}"#] {
            assert!(managed_opencode_cli_config(Some(inherited.into())).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_opencode_non_utf8_config_remains_invalid() {
        use std::os::unix::ffi::OsStringExt;

        let inherited = std::ffi::OsString::from_vec(vec![0xff]);
        assert!(managed_opencode_cli_config(Some(inherited)).is_err());
    }

    fn local_launch(
        agent: AgentKind,
        launcher: AgentLauncher,
        resume: Option<&str>,
        agent_args: &[&str],
    ) -> AgentCommand {
        launch_agent_for_local_host(
            agent,
            launcher,
            "session-id",
            resume,
            agent_args.iter().map(|arg| arg.to_string()).collect(),
            false,
        )
    }

    #[test]
    fn local_launch_keeps_claude_session_args_before_agent_args() {
        let args = [
            "--dangerously-skip-permissions",
            "--model",
            "claude-sonnet-4-5",
        ];
        let new = local_launch(AgentKind::Claude, AgentLauncher::Opencode, None, &args);
        assert_eq!(new.program, "claude");
        assert_eq!(new.args[..2], ["--session-id", "session-id"]);
        assert_eq!(new.args[2..], args);
        let resume = local_launch(
            AgentKind::Claude,
            AgentLauncher::Opencode,
            Some("native-id"),
            &args,
        );
        assert_eq!(resume.args[..2], ["--resume", "native-id"]);
        assert_eq!(resume.args[2..], args);
        assert!(new.env.is_empty() && resume.env.is_empty());
    }

    #[test]
    fn local_launch_puts_codex_control_args_before_agent_args_once() {
        let new = local_launch(
            AgentKind::Codex,
            AgentLauncher::Opencode,
            None,
            &["--sandbox", "workspace-write", "--search"],
        );
        assert_eq!(new.program, "codex");
        assert_eq!(
            new.args,
            [
                "--no-alt-screen",
                "--sandbox",
                "workspace-write",
                "--search"
            ]
        );
        let resume = local_launch(
            AgentKind::Codex,
            AgentLauncher::Opencode,
            Some("codex-native-id"),
            &["--search"],
        );
        assert_eq!(
            resume.args,
            ["--no-alt-screen", "resume", "codex-native-id", "--search"]
        );
        let explicit = local_launch(
            AgentKind::Codex,
            AgentLauncher::Opencode,
            None,
            &["--no-alt-screen", "--search"],
        );
        assert_eq!(explicit.args, ["--no-alt-screen", "--search"]);
    }

    #[test]
    fn local_launch_starts_and_resumes_opencode_sessions() {
        let new = local_launch(
            AgentKind::OpenCode,
            AgentLauncher::Opencode,
            None,
            &["--auto", "--model", "openai/gpt-5.6-terra"],
        );
        assert_eq!(new.program, "opencode");
        assert_eq!(new.args, ["--auto", "--model", "openai/gpt-5.6-terra"]);
        let resume = local_launch(
            AgentKind::OpenCode,
            AgentLauncher::Opencode,
            Some("ses_native"),
            &["--auto"],
        );
        assert_eq!(resume.args, ["--session", "ses_native", "--auto"]);
        assert!(new.env.is_empty() && resume.env.is_empty());
    }

    /// `adk code` takes every OpenCode argument after `--`; its own first
    /// positional selects a DevKit subcommand or agent profile, so a leaked
    /// `--session` would start the wrong thing entirely.
    #[test]
    fn local_launch_runs_opencode_through_adk_after_a_separator() {
        let bare = local_launch(AgentKind::OpenCode, AgentLauncher::Adk, None, &[]);
        assert_eq!(bare.program, "adk");
        assert_eq!(bare.args, ["code"]);
        let with_args = local_launch(
            AgentKind::OpenCode,
            AgentLauncher::Adk,
            None,
            &["--auto", "--print-logs"],
        );
        assert_eq!(with_args.args, ["code", "--", "--auto", "--print-logs"]);
        let resume = local_launch(
            AgentKind::OpenCode,
            AgentLauncher::Adk,
            Some("ses_native"),
            &["--auto"],
        );
        assert_eq!(
            resume.args,
            ["code", "--", "--session", "ses_native", "--auto"]
        );
    }

    /// The child inherits the caller's environment, so both directions are set
    /// explicitly: an ambient `OC_STATUS=0` must not quietly beat the opt-in.
    #[test]
    fn adk_status_strip_is_suppressed_unless_opted_in() {
        let status = |launcher, adk_statusline| {
            launch_agent_for_local_host(
                AgentKind::OpenCode,
                launcher,
                "session-id",
                None,
                vec![],
                adk_statusline,
            )
            .env
            .into_iter()
            .find(|(key, _)| key == "OC_STATUS")
        };
        assert_eq!(
            status(AgentLauncher::Adk, false),
            Some(("OC_STATUS".into(), "0".into()))
        );
        assert_eq!(
            status(AgentLauncher::Adk, true),
            Some(("OC_STATUS".into(), "1".into()))
        );
        assert!(status(AgentLauncher::Opencode, true).is_none());
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
    fn parser_with(bytes: &[u8]) -> Arc<Mutex<vt100::Parser>> {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 2000)));
        let mut tracker = conversations_core::terminal_history::PtyEraseTracker::default();
        let _ = tracker.process(&mut parser.lock().unwrap(), bytes);
        parser
    }

    fn repaint_snapshot(parser: &Arc<Mutex<vt100::Parser>>) -> (u16, u16, Vec<u8>) {
        let mut parser = parser.lock().unwrap_or_else(|e| e.into_inner());
        let (rows, cols) = parser.screen().size();
        (cols, rows, mirror_replay_bytes(&mut parser))
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
    fn mirror_replay_restores_opencode_mouse_reporting() {
        let mut source = vt100::Parser::new(24, 80, 2000);
        source.process(b"\x1b[?1003h\x1b[?1006hOpenCode");
        let frame = mirror_replay_bytes(&mut source);
        let mut viewer = vt100::Parser::new(24, 80, 2000);
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
        let root = std::env::temp_dir().join(format!("codex-tail-{}", uuid::Uuid::new_v4()));
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
        let root = std::env::temp_dir().join(format!("codex-tail-active-{}", uuid::Uuid::new_v4()));
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
        let root = std::env::temp_dir().join(format!("codex-tail-claim-{}", uuid::Uuid::new_v4()));
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
        let root = std::env::temp_dir().join(format!("codex-resume-tail-{}", uuid::Uuid::new_v4()));
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
        let root = std::env::temp_dir().join(format!("codex-tail-empty-{}", uuid::Uuid::new_v4()));
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
