//! `arrakis-conversations live` — the local CLI **multiplexer**.
//!
//! It multiplexes N concurrent local agent sessions, exactly one of which is
//! bridged to the real terminal at a time. The central server may hold
//! read-only metadata (title, order, the stored transcript tail); it has no
//! channel back: nothing the server stores or relays reaches a session's
//! process, terminal or arguments. Web control of this machine exists only as
//! signed operations on the private channel to the local host (`host`).
//!
//!   - **`live` (default)** seeds the user's OWN local session as the first child
//!     of the mux (focused). It exits once the user closes their last session
//!     (the registry empties).
//!   - **`live --host`** starts with NO new session of its own: it offers to
//!     resume the sessions it had open last time, and stays resident until
//!     Ctrl-C. `Ctrl-a c` starts another local session in either mode.
//!
//! Each child is spawned via [`super::interactive::spawn_child`], which wires the
//! PTY mirror, heartbeat and JSONL tail. The newly spawned child takes local
//! focus (W2 bridge: repaint + stdin pump + resize); the rest keep running
//! detached.
//!
//! **Concurrency = N concurrent children.** Each local start (and the initial
//! seed) inserts a NEW child into a per-`live_id` registry (NO busy-reject). The
//! host reaps children whose process has exited, and reaps ALL children on
//! shutdown so none are orphaned.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use conversations_core::{AgentKind, AgentLauncher};

use super::interactive::{
    install_panic_hook, spawn_child, spawn_mux_input, DetachedChild, FocusManager, MuxCommand,
    MuxWriterSlot, Shutdown, SpawnConfig,
};
use super::local::LocalHost;
use super::mux::{render_status_bar, ChildInfo};
use crate::cli::{GlobalOpts, LiveArgs};
use std::io::Write;
use std::sync::{Arc, Mutex};

/// A registry of concurrent detached children, keyed by `live_id`.
///
/// Replaces the W0 single-foreground, busy-reject model: each control command
/// inserts a NEW child here. Provides add + reap (remove + cancel) and a
/// reap-exited sweep. On host shutdown, [`reap_all`] cancels every child so no
/// process is orphaned.
#[derive(Default)]
struct ChildRegistry {
    children: HashMap<String, DetachedChild>,
    /// Insertion order of `live_id`s — the DETERMINISTIC ordering used by the
    /// prefix-key index/next/prev hotkeys AND the status bar (W3). A `HashMap`
    /// has no stable iteration order, so we keep this alongside it and treat it
    /// as the single source of truth for "child N". Entries are appended on
    /// `add` and removed on reap; a same-id re-add keeps its original slot.
    order: Vec<String>,
}

impl ChildRegistry {
    fn new() -> Self {
        Self::default()
    }

    /// Number of tracked children.
    fn len(&self) -> usize {
        self.children.len()
    }

    /// The deterministic, stable ordering of tracked `live_id`s (insertion
    /// order). Index/next/prev hotkeys and the status bar both read this.
    #[cfg_attr(not(test), allow(dead_code))]
    fn ordered_ids(&self) -> &[String] {
        &self.order
    }

    /// Insert a child, keyed by its `live_id`. If a child with the same id is
    /// already tracked (re-open of a live session), the old one is cancelled
    /// first so we never leak its process/tasks — and it keeps its original
    /// slot in the deterministic ordering.
    async fn add(&mut self, child: DetachedChild) {
        let id = child.live_id.clone();
        if let Some(mut prev) = self.children.remove(&id) {
            prev.cancel().await;
        }
        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        self.children.insert(id, child);
    }

    /// Borrow a tracked child by id (test assertions; W3 status bar lookup).
    #[cfg_attr(not(test), allow(dead_code))]
    fn get(&self, live_id: &str) -> Option<&DetachedChild> {
        self.children.get(live_id)
    }

    /// Switch local focus to `live_id`: detach the bridge from the
    /// previously-focused child, repaint `live_id`'s buffered screen to the real
    /// terminal, attach the stdin pump, and resize it to the current local
    /// terminal size. Background children keep running web-only. Returns `false`
    /// (no-op) if `live_id` is not tracked.
    ///
    /// This is W2's programmatic `focus(live_id)` entrypoint — W3 will drive it
    /// from prefix-key hotkeys. The borrow dance (look up prev + target as two
    /// shared refs before the mutable `focus` call) keeps the borrow checker
    /// happy with both children coming from the same map.
    fn focus<W: std::io::Write>(
        &self,
        fm: &mut FocusManager<W>,
        live_id: &str,
    ) -> anyhow::Result<bool> {
        let Some(target) = self.children.get(live_id) else {
            return Ok(false);
        };
        let prev = fm
            .focused_id()
            .filter(|id| *id != live_id)
            .and_then(|id| self.children.get(id));
        let size = crossterm::terminal::size().unwrap_or((80, 24));
        fm.focus(prev, target, size)?;
        Ok(true)
    }

    /// Focus the child at deterministic `index` (0-based, the same ordering the
    /// status bar shows). No-op (`false`) when the index is out of range.
    fn focus_index<W: std::io::Write>(
        &self,
        fm: &mut FocusManager<W>,
        index: usize,
    ) -> anyhow::Result<bool> {
        match self.order.get(index) {
            Some(id) => {
                let id = id.clone();
                self.focus(fm, &id)
            }
            None => Ok(false),
        }
    }

    /// Focus the next (`+1`) or previous (`-1`) child relative to the currently
    /// focused one, wrapping around the deterministic ordering. With no current
    /// focus, next picks the first child and prev the last. No-op (`false`) when
    /// there are no children.
    fn focus_step<W: std::io::Write>(
        &self,
        fm: &mut FocusManager<W>,
        forward: bool,
    ) -> anyhow::Result<bool> {
        let n = self.order.len();
        if n == 0 {
            return Ok(false);
        }
        let cur = fm
            .focused_id()
            .and_then(|id| self.order.iter().position(|o| o == id));
        let next = match (cur, forward) {
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
            (None, true) => 0,
            (None, false) => n - 1,
        };
        self.focus_index(fm, next)
    }

    /// Reap every child whose process has already exited on its own. Returns
    /// the ids that were reaped.
    async fn reap_exited(&mut self) -> Vec<String> {
        let exited: Vec<String> = self
            .children
            .iter()
            .filter(|(_, c)| c.has_exited())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &exited {
            if let Some(mut child) = self.children.remove(id) {
                child.cancel().await;
            }
            self.order.retain(|o| o != id);
        }
        exited
    }

    /// Cancel and remove ALL children — called on host shutdown so no child
    /// process is orphaned.
    async fn reap_all(&mut self) {
        let ids: Vec<String> = self.children.keys().cloned().collect();
        for id in ids {
            if let Some(mut child) = self.children.remove(&id) {
                child.cancel().await;
            }
        }
        self.order.clear();
    }

    /// Collect a full-screen PTY keyframe per tracked child for re-seeding the
    /// server's mirror snapshot after a redeploy. Each entry is
    /// `(live_id, cols, rows, replay_bytes)`, where the replay includes retained
    /// scrollback plus the current screen. Children that have not painted
    /// anything yet (empty bytes) are skipped — there is nothing to replay.
    /// Walks the deterministic ordering so it never touches a reaped child.
    ///
    /// Not used in production (each child's terminal WebSocket re-seeds via
    /// mirror-mailbox repaint on reconnect); retained as a tested helper for the
    /// per-child keyframe collection shape. Test-only, matching
    /// `DetachedChild::pty_keyframe`.
    #[cfg(test)]
    fn pty_keyframes(&self) -> Vec<(String, u16, u16, Vec<u8>)> {
        self.order
            .iter()
            .filter_map(|id| {
                let (cols, rows, bytes) = self.children.get(id)?.pty_keyframe();
                if bytes.is_empty() {
                    return None;
                }
                Some((id.clone(), cols, rows, bytes))
            })
            .collect()
    }

    /// Snapshot the open sessions for persistence — `{session_id, cwd}` per
    /// tracked child, in the deterministic ordering (so a resume re-opens them in
    /// the same order the user had them). Walks `order` so it never includes a
    /// reaped child.
    fn open_sessions(&self) -> Vec<crate::open_sessions::OpenSession> {
        self.order
            .iter()
            .filter_map(|id| {
                let child = self.children.get(id)?;
                let (launcher, adk_statusline) = child.launch();
                Some(crate::open_sessions::OpenSession {
                    session_id: id.clone(),
                    agent: child.agent,
                    cwd: child.cwd().to_string_lossy().to_string(),
                    // Codex's real resume id (if the tailer captured it) so a
                    // `--host` restart resumes by it, not the live id.
                    resume_id: child.resume_id(),
                    // Preserve the server-resolved Codex launch policy across a
                    // host restart. Host-level trailing args remain owned by the
                    // newly started host and are not duplicated here.
                    agent_args: child.agent_args().to_vec(),
                    // Same reason, for HOW the child was launched: an `adk code`
                    // session must not come back as a bare `opencode`.
                    launcher,
                    adk_statusline,
                })
            })
            .collect()
    }

    /// Build the status-bar render input from the deterministic ordering, with a
    /// short label per child (the `live_id`, shortened to its first segment so a
    /// uuid doesn't blow the line).
    fn status_children(&self) -> Vec<ChildInfo> {
        self.order
            .iter()
            .map(|id| ChildInfo::new(short_label(id)))
            .collect()
    }

    /// 0-based index of `focused_id` in the deterministic ordering, for the
    /// status-bar focus marker.
    fn focused_index(&self, focused_id: Option<&str>) -> Option<usize> {
        let id = focused_id?;
        self.order.iter().position(|o| o == id)
    }
}

/// Shorten a `live_id` for the status bar: the first dash-separated segment,
/// capped at 8 chars. A uuid `abcd1234-...` becomes `abcd1234`.
fn short_label(live_id: &str) -> String {
    let head = live_id.split('-').next().unwrap_or(live_id);
    head.chars().take(8).collect()
}

/// A local launch request built by the host from a verified signed operation
/// or an owner-local handoff. This type is not a network protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    /// An idempotent owner-local handoff.
    Handoff {
        session_id: String,
        source_session_id: String,
        brief: String,
        agent: AgentKind,
        agent_args: Vec<String>,
        launcher: AgentLauncher,
        adk_statusline: bool,
        /// The model the child must start on; `None` keeps the agent's default.
        model: Option<conversations_core::messaging::HandoffModel>,
    },

    /// Resume a session using its locally recorded working directory and identity.
    Reopen {
        session_id: String,
        cwd: String,
        agent: AgentKind,
        /// The agent's NATIVE resume target, when it differs from `session_id`
        /// (Codex). `None` for Claude / legacy / uploaded rows → resume by
        /// `session_id`. `session_id` still keys the re-opened live session so
        /// the stored row continues across reopens.
        resume_id: Option<String>,
        /// Launch arguments resolved from local settings.
        agent_args: Vec<String>,
        /// How to invoke the agent.
        launcher: AgentLauncher,
        /// Whether an `adk code` child may draw its tmux status strip.
        adk_statusline: bool,
    },
    /// Start a session using the host's local launch settings.
    StartNew {
        cwd: Option<String>,
        agent: AgentKind,
        /// Launch arguments resolved from local settings.
        agent_args: Vec<String>,
        /// How to invoke the agent.
        launcher: AgentLauncher,
        /// Whether an `adk code` child may draw its tmux status strip.
        adk_statusline: bool,
    },
}

/// Entry point for `live` (both the default and `--host`). Builds a scoped
/// multi-thread tokio runtime and blocks on the host loop — `main()` stays sync,
/// and every spawned child reuses the same runtime to run the interactive wiring.
/// The default mode seeds the user's own local session; `--host` starts headless.
pub fn run(global: &GlobalOpts, args: &LiveArgs) -> Result<()> {
    let server = global.server.trim_end_matches('/').to_string();
    #[cfg(unix)]
    let _owner = crate::commands::host_owner::HostOwner::acquire_mux(&server)
        .context("another `live` multiplexer is running for this server")?;

    // The default `live` seeds an initial LOCAL session (the user's own `claude`,
    // mirrored to the web) as the first child of the mux; `--host` starts empty
    // (headless responder).
    // `--launcher adk` runs in $ARRAKIS_HOME rather than the invocation
    // directory (see `launcher_cwd`), and must fail here — before the runtime and
    // the mux stdin pump take over the terminal — when that is not resolvable.
    let cwd = launcher_cwd(
        args.launcher,
        std::env::current_dir().context("resolving current directory")?,
    )?;
    let mut initial = initial_session_config(args, cwd);
    if let Some(config) = initial.as_mut() {
        config.agent_args = local_launch_args(config.agent).unwrap_or_default();
    }

    // `--host` may resume the sessions it had open last time. Decide (and prompt,
    // on a TTY) BEFORE the runtime + mux stdin pump take over the terminal.
    let resume_sessions = resolve_resume_sessions(&server, args);

    let _diagnostics = crate::diagnostics::StatusGuard::enter();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building host tokio runtime")?;

    // The legacy host no longer services server-driven PR sync or polling: the
    // server must not choose what this machine reads with `gh`. See the signed
    // `pr.sync` / `pr.comment` operations in `local_host`.

    // Every session runs in a host wrapper registered with this machine's
    // local host: its record, history and handoff are local, and nothing of
    // it is mirrored to the server.
    let local = LocalHost::ensure(&server);

    rt.block_on(host_loop(
        global,
        args,
        &server,
        initial,
        resume_sessions,
        &local,
    ))
}

/// How often to sweep the registry for children whose process has exited.
const REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Grace period before the first status-bar paint when a focused seed child is
/// spawned, so its `claude` banner finishes drawing before the bar write lands
/// (otherwise the two collide and corrupt the banner). See the `initial_paint`
/// setup in `host_loop`.
const STATUS_BAR_SEED_SETTLE: std::time::Duration = std::time::Duration::from_millis(900);

/// The resident host loop: drive local mux hotkeys (each `Ctrl-a c` spawns a NEW
/// detached child into the registry; W1: concurrent, no busy-reject), and
/// periodically reap children whose process exited. Returns on a clean Ctrl-C or
/// detach hotkey — reaping ALL children (no orphans) before returning.
async fn host_loop(
    global: &GlobalOpts,
    args: &LiveArgs,
    server: &str,
    initial: Option<SpawnConfig>,
    resume_sessions: Vec<crate::open_sessions::OpenSession>,
    local: &LocalHost,
) -> Result<()> {
    // Seeded (default) mode auto-exits when its last session ends; headless
    // `--host` stays resident.
    let auto_exit_when_empty = initial.is_some();
    if args.host {
        live_info!(
            "conversations host: local sessions only — web start, re-open, input and end \
             need the signed local host (`arrakis-conversations host`)"
        );
    }

    // Restore the terminal out of raw mode on panic — a focused child puts the
    // terminal in raw mode (local bridge), so a panic mid-session would otherwise
    // leave it wedged. (Previously the foreground `drive()` owned this hook.)
    install_panic_hook();

    let mut registry = ChildRegistry::new();

    // W3: the prefix-key state machine owns local stdin. A single mux-input pump
    // reads stdin, feeds each byte through `PrefixKey`, and either forwards it to
    // the FOCUSED child (via `mux_slot`) or sends a `MuxCommand` to this loop.
    // The FocusManager runs in MUX mode: focus switches publish/clear the focused
    // child's writer in the slot rather than spawning a second stdin reader.
    let mux_slot: MuxWriterSlot = Arc::new(Mutex::new(None));
    let mut focus =
        FocusManager::stdout_mux(Arc::clone(&mux_slot)).with_mirror_size(args.mirror_size);
    let mux_shutdown = Shutdown::new();
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<MuxCommand>();
    let mux_pump = spawn_mux_input(Arc::clone(&mux_slot), cmd_tx, Arc::clone(&mux_shutdown));
    super::interactive::mux_debug("[host] host_loop: mux pump spawned, entering select loop");

    // Seed the user's own local session (default `live`). `--host` (no initial
    // config) starts empty as a pure resident responder. A failed initial spawn
    // is fatal — the default `live` should fail loudly, just as the old
    // foreground path did. (`auto_exit_when_empty` was set from `initial` above.)
    let seeded = initial.is_some();
    if let Some(config) = initial {
        // Seed: nothing else is focused yet, so stream this session live (its
        // banner renders byte-exact, no repaint reconstruction).
        spawn_focus_add(global, args, config, &mut registry, &mut focus, true, local)
            .await
            .context("spawning the initial live session")?;
    }

    // `--host` resume: re-open each previously-open session (detached, web-wired)
    // in its saved cwd via the same `claude --resume` path the web "Re-open as
    // live" button uses. They start detached (another may already be adding); we
    // focus the first afterwards so the user lands on a bridged terminal. The
    // saved set is consumed (cleared) once re-opened so a later start without new
    // activity doesn't re-prompt.
    // Outlives the resume block: every later checkpoint has to carry these
    // rows too, or the next periodic save (which writes only the live
    // registry) drops them again.
    let mut keep_checkpointed: Vec<crate::open_sessions::OpenSession> = Vec::new();
    if !resume_sessions.is_empty() {
        live_info!(
            "conversations host: resuming {} previous session(s)",
            resume_sessions.len()
        );
        // A session whose LAUNCHER cannot be resolved here is blocked by the
        // environment, not by anything about the session: an `adk code` session
        // needs `$ARRAKIS_HOME`, so a host started outside the Arrakis DevKit dev
        // shell cannot resume it. Keep those checkpoints instead of consuming
        // them, so a later start from inside the shell still offers them. Every
        // other outcome consumes the entry as before — a resumed session is now
        // live, and a session-specific failure would otherwise re-prompt forever.
        for sess in &resume_sessions {
            let config = match saved_resume_config(sess) {
                // Current local settings take effect on resume, as for a new
                // session; without a settings file the checkpoint stands.
                Ok(mut config) => {
                    if sess.agent != AgentKind::Zsh {
                        if let Some(agent_args) = local_launch_args(sess.agent) {
                            config.agent_args = agent_args;
                        }
                    }
                    config
                }
                Err(e) => {
                    live_error!(
                        "conversations host: cannot resume a checkpointed {:?} session in {} \
                         ({} launcher): {e:#}",
                        sess.agent,
                        sess.cwd,
                        sess.launcher
                    );
                    keep_checkpointed.push(sess.clone());
                    continue;
                }
            };
            if let Err(e) = spawn_focus_add(
                global,
                args,
                config,
                &mut registry,
                &mut focus,
                false,
                local,
            )
            .await
            {
                live_error!(
                    "conversations host: failed to resume session {}: {e:#}",
                    sess.session_id
                );
            }
        }
        // Land focus on the first resumed session (deterministic order).
        let _ = registry.focus_index(&mut focus, 0);
        let consumed = if keep_checkpointed.is_empty() {
            crate::open_sessions::clear_for(server)
        } else {
            live_info!(
                "conversations host: keeping {} unresumable session(s) checkpointed",
                keep_checkpointed.len()
            );
            crate::open_sessions::save_for(server, &keep_checkpointed)
        };
        if let Err(e) = consumed {
            live_error!("conversations host: could not update saved sessions: {e:#}");
        }
    }

    let mut reap_tick = tokio::time::interval(REAP_INTERVAL);
    // `--host` only: checkpoint the open-session set every 60s so a crash still
    // leaves a resumable set. The first tick fires immediately — harmless (it
    // just writes the current set). Gated at the arm on `args.host`.
    let mut save_tick = tokio::time::interval(OPEN_SESSIONS_SAVE_INTERVAL);
    // Poll for terminal resizes (no portable SIGWINCH); on change, route to the
    // focused child and redraw the status bar.
    let mut resize_tick = tokio::time::interval(std::time::Duration::from_millis(200));
    let mut last_size = crossterm::terminal::size().unwrap_or((80, 24));

    // Defer the first status-bar paint when we seeded a focused child. That child
    // streams its `claude` banner straight to stdout from the moment it spawns,
    // and the bar's write (absolute cursor move + DECSC/DECRC) collides with the
    // banner draw if it lands mid-burst — the bar leaks into the banner row and
    // claude's mascot art is displaced. Once the banner settles the bar paints
    // cleanly (steady-state focus switches already prove this). The headless
    // `--host` case has no seed, so paint immediately.
    let mut initial_paint = if seeded {
        Some(Box::pin(tokio::time::sleep(STATUS_BAR_SEED_SETTLE)))
    } else {
        redraw_status_bar(&registry, focus.focused_id());
        None
    };

    loop {
        tokio::select! {
            // One-shot: the deferred first status-bar paint, once the seeded
            // child's banner has settled. Disarmed after firing.
            _ = async { initial_paint.as_mut().unwrap().await }, if initial_paint.is_some() => {
                initial_paint = None;
                redraw_status_bar(&registry, focus.focused_id());
            }
            // A clean Ctrl-C exits the host loop. All tracked children are
            // reaped below so none are orphaned.
            _ = tokio::signal::ctrl_c() => {
                live_info!("conversations host: stopping ({} child(ren) to reap)", registry.len());
                break;
            }
            // A mux hotkey command from the stdin pump.
            cmd = cmd_rx.recv() => {
                super::interactive::mux_debug(&format!("[host] recv MuxCommand: {cmd:?}"));
                match cmd {
                    Some(MuxCommand::Quit) => {
                        live_info!("conversations host: detach/quit hotkey ({} child(ren) to reap)", registry.len());
                        break;
                    }
                    Some(MuxCommand::Focus(i)) => {
                        let _ = registry.focus_index(&mut focus, i);
                        redraw_status_bar(&registry, focus.focused_id());
                    }
                    Some(MuxCommand::FocusNext) => {
                        let _ = registry.focus_step(&mut focus, true);
                        redraw_status_bar(&registry, focus.focused_id());
                    }
                    Some(MuxCommand::FocusPrev) => {
                        let _ = registry.focus_step(&mut focus, false);
                        redraw_status_bar(&registry, focus.focused_id());
                    }
                    Some(MuxCommand::Create) => {
                        super::interactive::mux_debug("[host] Create — spawning a new local session");
                        start_local_session(global, args, &mut registry, &mut focus, local).await;
                        super::interactive::mux_debug(&format!(
                            "[host] Create done — {} child(ren), focus={:?}",
                            registry.len(), focus.focused_id()
                        ));
                        redraw_status_bar(&registry, focus.focused_id());
                    }
                    // Pump gone (stdin closed) — the children keep running.
                    None => {}
                }
            }
            // Periodic checkpoint (--host only): persist the open-session set so
            // a crash — not just a clean exit — still leaves a resumable set.
            _ = save_tick.tick(), if args.host => {
                persist_open_sessions(server, &registry, &mut keep_checkpointed);
            }
            // Periodic sweep: reap children whose process already exited.
            _ = reap_tick.tick() => {
                let reaped = registry.reap_exited().await;
                let any = !reaped.is_empty();
                for id in reaped {
                    // If the reaped child held local focus, drop the bridge +
                    // restore cooked mode (the child is gone, so no un-focus
                    // target to pass).
                    if focus.focused_id() == Some(id.as_str()) {
                        focus.unfocus(None);
                    }
                    // Child reaped; its exit is no longer announced to stdout.
                }
                if any {
                    redraw_status_bar(&registry, focus.focused_id());
                }
            }
            // Poll for a terminal resize → route to the focused child + redraw.
            _ = resize_tick.tick() => {
                if initial_paint.is_none() && crate::diagnostics::has_message() {
                    redraw_status_bar(&registry, focus.focused_id());
                }
                if let Ok(size) = crossterm::terminal::size() {
                    if size != last_size {
                        last_size = size;
                        let (cols, rows) = size;
                        let focused = focus
                            .focused_id()
                            .map(str::to_string)
                            .and_then(|id| registry.get(&id));
                        focus.handle_resize(focused, cols, rows);
                        redraw_status_bar(&registry, focus.focused_id());
                    }
                }
            }
        }

        // Default mode: once the user has closed their last session, the host has
        // nothing left to bridge — exit (preserves the pre-unification
        // "quit claude ⇒ live exits" behavior). A `Ctrl-a c` session keeps the
        // host alive until it too ends. `--host` (auto_exit false) stays
        // resident with zero children.
        if should_exit_when_empty(auto_exit_when_empty, registry.len()) {
            live_info!("conversations live: all sessions ended — exiting");
            break;
        }
    }

    // Save the open-session set on every exit path (--host only), BEFORE reaping
    // clears the registry. Closing every session normally ⇒ an empty set ⇒ no
    // resume prompt next time; exiting (Ctrl-C / Ctrl-a q) with sessions still
    // open ⇒ their set is saved ⇒ a resume prompt next time.
    if args.host {
        persist_open_sessions(server, &registry, &mut keep_checkpointed);
    }

    // Stop the mux-input pump (it may be parked in a blocking stdin read — the
    // shutdown flag stops it forwarding; we do not hard-join it).
    mux_shutdown.trigger();
    drop(mux_pump);

    // Drop local focus first so cooked mode is restored before we tear children
    // down, then reap ALL children on shutdown — no orphans. Finally clear the
    // status bar so the terminal is left clean.
    focus.unfocus(None);
    registry.reap_all().await;
    clear_status_bar();

    Ok(())
}

/// Rebuild the spawn configuration recorded for a child before a host restart.
/// The conversations live id remains stable, while Codex resumes by its native
/// id and receives the same server-resolved launch policy as before the restart.
///
/// The checkpointed launcher is honored too, so an `adk code` session comes back
/// through the harness. A checkpoint whose launcher no longer fits its agent
/// (hand-edited, or written by a newer build) falls back to the agent's own
/// binary rather than refusing to resume the session.
fn saved_resume_config(session: &crate::open_sessions::OpenSession) -> Result<SpawnConfig> {
    let resume_target = session
        .resume_id
        .clone()
        .unwrap_or_else(|| session.session_id.clone());
    let launcher = if session.launcher.supports(session.agent) {
        session.launcher
    } else {
        live_error!(
            "conversations host: checkpointed {:?} session in {} pairs launcher \
             `{}` with that agent; launching its own binary",
            session.agent,
            session.cwd,
            session.launcher
        );
        AgentLauncher::default()
    };
    let mut config = SpawnConfig::resume_session(
        session.agent,
        launcher_cwd(launcher, PathBuf::from(&session.cwd))?,
        session.session_id.clone(),
        resume_target,
    );
    config.agent_args = session.agent_args.clone();
    config.launcher = launcher;
    config.adk_statusline = session.adk_statusline;
    Ok(config)
}

/// Status-bar placement approach (W3): we reserve no scroll region. Instead we
/// emit the simple, robust sequence "save cursor, move to the last terminal row,
/// clear that line, write the bar, restore cursor" (DECSC/DECRC plus a CSI
/// cursor-move and an erase-line). This briefly parks the cursor on the last row
/// and returns it, so the focused child's screen is not corrupted: the child
/// never owns that row for long, and it repaints on focus anyway. It can flicker
/// under heavy child output, but per the spec correctness and restoration matter
/// more than flicker-free rendering.
fn redraw_status_bar(registry: &ChildRegistry, focused_id: Option<&str>) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let children = registry.status_children();
    let focused = registry.focused_index(focused_id);
    let bar = crate::diagnostics::status_line(cols as usize)
        .unwrap_or_else(|| render_status_bar(&children, focused, cols as usize));

    let mut out = std::io::stdout().lock();
    // ESC 7        save cursor + attrs
    // ESC[<rows>;1H move to first column of the last row
    // ESC[2K       erase entire line
    // ESC[92m      bright-green foreground for the bar
    // <bar>        the rendered status line
    // ESC[0m       reset attrs (ESC 8 also restores, but be explicit)
    // ESC 8        restore cursor + attrs
    // Color is applied here, NOT in `render_status_bar` (which truncates by
    // visible char count — SGR codes there would corrupt the width math).
    let seq = crate::diagnostics::frame(&bar, rows);
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Clear the reserved status-bar row on exit so the terminal is left clean.
fn clear_status_bar() {
    let (_, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut out = std::io::stdout().lock();
    let seq = format!("\x1b7\x1b[{};1H\x1b[2K\x1b8", rows);
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// The directory to spawn a child in, given its launcher and the cwd the caller
/// would otherwise use (the host's cwd for a start-new, the stored session cwd
/// for a reopen).
///
/// Only the `adk` launcher overrides it: `adk code` opens `$ARRAKIS_HOME` no
/// matter where it is started from, and the OpenCode tailer locates a new session
/// by matching the session's own directory against the spawn cwd — so spawning it
/// anywhere else would register a live session whose transcript never arrives.
fn launcher_cwd(launcher: AgentLauncher, requested: PathBuf) -> Result<PathBuf> {
    match launcher {
        AgentLauncher::Opencode => Ok(requested),
        AgentLauncher::Adk => super::interactive::adk_session_cwd(),
    }
}

/// `Ctrl-a c`: start another session of the agent and launcher this host was
/// invoked for, in the host's own cwd, and give it local focus. A failed spawn
/// is logged; other children keep running.
async fn start_local_session<W: std::io::Write>(
    global: &GlobalOpts,
    args: &LiveArgs,
    registry: &mut ChildRegistry,
    focus: &mut FocusManager<W>,
    local: &LocalHost,
) {
    let config = (|| -> Result<SpawnConfig> {
        // An `adk code` child opens $ARRAKIS_HOME regardless of the cwd we spawn
        // it in, so pin the spawn there too — the OpenCode tailer finds a new
        // session by matching its directory against this cwd.
        let cwd = launcher_cwd(args.launcher, std::env::current_dir()?)?;
        let mut config = SpawnConfig::new_session(args.agent, cwd);
        config.agent_args = local_launch_args(args.agent).unwrap_or_default();
        config.launcher = args.launcher;
        config.adk_statusline = args.adk_statusline;
        Ok(config)
    })();
    // NOT start_focused: another child may currently own stdout, so this one is
    // repainted on focus rather than streamed live (avoids a double writer).
    let result = match config {
        Ok(config) => spawn_focus_add(global, args, config, registry, focus, false, local)
            .await
            .map(|_| ()),
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        live_error!("conversations host: failed to start a local session: {error:#}");
    }
}

/// Spawn a detached, web-wired child for `config`, register it, and give it local
/// focus (bridge to the real terminal). Shared by [`start_local_session`]
/// (`Ctrl-a c`), resume and the initial-session seed in [`host_loop`]. Returns the new child's
/// `live_id` on success.
///
/// W2: the newly spawned child takes local focus — its PTY is bridged to the real
/// terminal (repaint + stdin pump + resize); the previously-focused child detaches
/// but keeps running web-only.
///
/// `start_focused` is passed through to [`spawn_child`]: set it ONLY for the
/// seeded first session (nothing else focused yet) so its banner streams live
/// instead of being reconstructed via a repaint. It MUST be false when another
/// child is already focused.
async fn spawn_focus_add<W: std::io::Write>(
    global: &GlobalOpts,
    args: &LiveArgs,
    config: SpawnConfig,
    registry: &mut ChildRegistry,
    focus: &mut FocusManager<W>,
    start_focused: bool,
    local: &LocalHost,
) -> Result<String> {
    let child = spawn_child(global, args, config, start_focused, &local.dir).await?;
    let id = child.live_id.clone();
    registry.add(child).await;
    match registry.focus(focus, &id) {
        Ok(true) => live_info!(
            "conversations host: child {id} spawned + focused (local bridge) — {} active\n",
            registry.len()
        ),
        Ok(false) => live_info!(
            "conversations host: child {id} spawned (detached, web-only) — {} active",
            registry.len()
        ),
        Err(e) => live_error!("conversations host: child {id} spawned but focus failed: {e:#}"),
    }
    Ok(id)
}

/// Launch arguments for `agent` from this machine's local host settings file,
/// the file the signed host launches from. `None` when there is no such file.
/// Central account settings are never consulted.
fn local_launch_args(agent: AgentKind) -> Option<Vec<String>> {
    use crate::commands::local_host::{agent_link::local_dir, settings::Settings};
    let dir = local_dir();
    let configured =
        Settings::path().is_ok_and(|path| path.exists()) || dir.join("config.json").exists();
    if !configured {
        return None;
    }
    match Settings::load(&dir).and_then(|settings| settings.launch.agent_args(agent)) {
        Ok(agent_args) => Some(agent_args),
        Err(error) => {
            live_error!("conversations host: ignoring local launch settings: {error:#}");
            None
        }
    }
}

/// Choose the initial local session to seed when `live` starts.
///
/// - `--host` ⇒ `None`: the headless resident responder starts with no local
///   session of its own and stays up until Ctrl-C.
/// - `--resume <id>` ⇒ resume that stored session in `cwd`.
/// - otherwise ⇒ a fresh session in `cwd` (the user's own `claude`).
///
/// `--launcher`/`--adk-statusline` apply to whichever of those is seeded. The
/// caller resolves `cwd` (an `adk` launcher needs `$ARRAKIS_HOME`, not the
/// invocation directory), keeping this pure (no I/O) so the
/// host/resume/new branching stays unit-testable.
fn initial_session_config(args: &LiveArgs, cwd: PathBuf) -> Option<SpawnConfig> {
    if args.host {
        return None;
    }
    let mut config = match args.resume.as_deref() {
        // The user-supplied `--resume <id>` IS the agent's resume target, so it
        // doubles as both the live id and the resume target.
        Some(id) => SpawnConfig::resume_session(args.agent, cwd, id.to_string(), id.to_string()),
        None => SpawnConfig::new_session(args.agent, cwd),
    };
    config.launcher = args.launcher;
    config.adk_statusline = args.adk_statusline;
    Some(config)
}

/// The default-mode exit rule: a seeded `live` (it had an initial local session)
/// exits once its session registry empties — i.e. the user has closed their last
/// session. `--host` (headless, `auto_exit == false`) never auto-exits; it stays
/// resident with zero children. Pure so the rule is unit-testable.
fn should_exit_when_empty(auto_exit: bool, registry_len: usize) -> bool {
    auto_exit && registry_len == 0
}

/// How often `--host` checkpoints its open-session set to disk, so a crash (not
/// just a clean exit) still leaves a resumable set behind.
const OPEN_SESSIONS_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// What to do with the saved open-session set at `--host` startup. Pure so the
/// gating (host-only, `--no-resume`, empty set, non-TTY) is unit-testable apart
/// from the actual prompt + disk I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumePlan {
    /// Don't resume and leave the saved set on disk untouched (not `--host`, an
    /// empty set, or a non-interactive `--host` that should neither prompt nor
    /// discard the set).
    Skip,
    /// Discard the saved set and start empty (`--no-resume`, or the user
    /// answering "no" at the prompt).
    Clear,
    /// A non-empty set + an interactive `--host`: ask the user.
    Prompt,
}

/// Decide how to treat the saved open-session set, given the mode flags, whether
/// stdin is a TTY, and how many sessions were saved. Pure (no I/O).
fn resume_plan(host: bool, no_resume: bool, is_tty: bool, saved_len: usize) -> ResumePlan {
    if !host {
        return ResumePlan::Skip;
    }
    if no_resume {
        return ResumePlan::Clear;
    }
    if saved_len == 0 {
        // Nothing saved → nothing to prompt about, nothing to clear.
        return ResumePlan::Skip;
    }
    if !is_tty {
        // A non-interactive `--host` (e.g. systemd) can't answer a prompt; leave
        // the set intact so an interactive restart can still offer it.
        return ResumePlan::Skip;
    }
    ResumePlan::Prompt
}

/// Prompt (TTY) whether to resume `count` previously-open sessions. Default is
/// yes (bare Enter), matching the `[Y/n]` hint. A read error ⇒ no.
fn prompt_resume_sessions(count: usize) -> bool {
    use std::io::{self, Write};
    print!("conversations host: resume {count} previous session(s)? [Y/n] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let ans = line.trim().to_ascii_lowercase();
    ans.is_empty() || ans == "y" || ans == "yes"
}

/// Resolve the saved open-session set into the list to resume at startup,
/// applying [`resume_plan`] and (on `Prompt`) asking the user. Clears the saved
/// set on `Clear` or a "no" answer; leaves it intact on `Skip`. The actual
/// re-open happens later in `host_loop` (it needs the registry + focus).
fn resolve_resume_sessions(
    server: &str,
    args: &LiveArgs,
) -> Vec<crate::open_sessions::OpenSession> {
    use std::io::IsTerminal;
    let saved = crate::open_sessions::load_for(server);
    match resume_plan(
        args.host,
        args.no_resume,
        std::io::stdin().is_terminal(),
        saved.len(),
    ) {
        ResumePlan::Skip => Vec::new(),
        ResumePlan::Clear => {
            if let Err(e) = crate::open_sessions::clear_for(server) {
                live_error!("conversations host: could not clear saved sessions: {e:#}");
            }
            Vec::new()
        }
        ResumePlan::Prompt => {
            if prompt_resume_sessions(saved.len()) {
                saved
            } else {
                if let Err(e) = crate::open_sessions::clear_for(server) {
                    live_error!("conversations host: could not clear saved sessions: {e:#}");
                }
                Vec::new()
            }
        }
    }
}

/// Persist the registry's current open-session set for `server` (best-effort:
/// a write failure is logged, never fatal). Only called for `--host`.
///
/// `kept` are sessions this host could not resume for an environmental reason
/// (see `host_loop`). They are not in the registry, so they have to be folded
/// back in or the snapshot silently discards them.
fn persist_open_sessions(
    server: &str,
    registry: &ChildRegistry,
    kept: &mut Vec<crate::open_sessions::OpenSession>,
) {
    let sessions = merge_kept_sessions(registry.open_sessions(), kept);
    if let Err(e) = crate::open_sessions::save_for(server, &sessions) {
        live_error!("conversations host: could not save open sessions: {e:#}");
    }
}

/// Append the retained checkpoints that are not already live, and forget the
/// ones that are. A session id reappears in the registry when the user opens it
/// again (the web re-open resolves its own launcher); from then on the live row
/// is the accurate one, and once that child exits the session is genuinely
/// closed — resurrecting the old checkpoint would re-offer it forever.
fn merge_kept_sessions(
    live: Vec<crate::open_sessions::OpenSession>,
    kept: &mut Vec<crate::open_sessions::OpenSession>,
) -> Vec<crate::open_sessions::OpenSession> {
    kept.retain(|sess| !live.iter().any(|open| open.session_id == sess.session_id));
    let mut merged = live;
    merged.extend(kept.iter().cloned());
    merged
}

#[cfg(test)]
mod tests {
    use super::super::interactive::SpawnMode;
    use super::*;

    /// Minimal `LiveArgs` for the pure-helper tests: only `host` / `resume` matter
    /// to `initial_session_config`; everything else is its inert default.
    fn live_args(host: bool, resume: Option<&str>) -> LiveArgs {
        LiveArgs {
            agent: AgentKind::Claude,
            launcher: AgentLauncher::Opencode,
            adk_statusline: false,
            host,
            no_resume: false,
            resume: resume.map(str::to_string),
            mirror_size: None,
            claude_args: Vec::new(),
        }
    }

    #[test]
    fn initial_config_none_for_host() {
        // `--host` is the headless resident: no local session of its own.
        let cfg = initial_session_config(&live_args(true, None), PathBuf::from("/work"));
        assert!(cfg.is_none());
    }

    #[test]
    fn initial_config_new_for_default() {
        // Plain `live` seeds a brand-new session in the cwd.
        let cfg = initial_session_config(&live_args(false, None), PathBuf::from("/work"))
            .expect("default live seeds an initial session");
        assert_eq!(cfg.mode, SpawnMode::New);
        assert_eq!(cfg.cwd, PathBuf::from("/work"));
        assert!(!cfg.session_id.is_empty());
    }

    #[test]
    fn initial_config_resume_when_requested() {
        // `live --resume <id>` seeds a resume of that exact session id.
        let cfg =
            initial_session_config(&live_args(false, Some("sess-42")), PathBuf::from("/work"))
                .expect("resume seeds an initial session");
        assert_eq!(cfg.mode, SpawnMode::Resume);
        assert_eq!(cfg.session_id, "sess-42");
    }

    #[test]
    fn saved_resume_config_restores_codex_launch_policy() {
        let saved = crate::open_sessions::OpenSession {
            session_id: "live-id".into(),
            agent: AgentKind::Codex,
            cwd: "/repo".into(),
            resume_id: Some("native-id".into()),
            agent_args: vec!["--sandbox".into(), "danger-full-access".into()],
            launcher: AgentLauncher::Opencode,
            adk_statusline: false,
        };

        let config = saved_resume_config(&saved).unwrap();

        assert_eq!(config.mode, SpawnMode::Resume);
        assert_eq!(config.session_id, "live-id");
        assert_eq!(config.resume_target, "native-id");
        assert_eq!(config.cwd, PathBuf::from("/repo"));
        assert_eq!(config.agent_args, ["--sandbox", "danger-full-access"]);
        assert_eq!(config.launcher, AgentLauncher::Opencode);
    }

    /// A checkpoint written by a build that did not know about launchers (no
    /// `launcher` key at all) must keep resuming through the agent's own binary,
    /// and a nonsense pairing must degrade to that rather than refuse to resume.
    #[test]
    fn saved_resume_config_falls_back_to_the_agents_own_binary() {
        let legacy: crate::open_sessions::OpenSession = serde_json::from_str(
            r#"{"session_id":"live-id","agent":"opencode","cwd":"/repo","resume_id":"ses_native"}"#,
        )
        .unwrap();
        let config = saved_resume_config(&legacy).unwrap();
        assert_eq!(config.launcher, AgentLauncher::Opencode);
        assert_eq!(config.cwd, PathBuf::from("/repo"));

        let mismatched = crate::open_sessions::OpenSession {
            session_id: "live-id".into(),
            agent: AgentKind::Claude,
            cwd: "/repo".into(),
            resume_id: None,
            agent_args: Vec::new(),
            launcher: AgentLauncher::Adk,
            adk_statusline: false,
        };
        let config = saved_resume_config(&mismatched).unwrap();
        assert_eq!(config.launcher, AgentLauncher::Opencode);
        // …and therefore keeps the saved cwd instead of $ARRAKIS_HOME.
        assert_eq!(config.cwd, PathBuf::from("/repo"));
    }

    /// The periodic checkpoint writes the live registry, which by definition
    /// does not contain a session this host could not resume. Those rows have to
    /// survive every later snapshot — until the session turns up live, which
    /// retires the retained copy for good.
    #[test]
    fn merging_kept_sessions_keeps_them_until_the_session_is_live_again() {
        let row = |id: &str| crate::open_sessions::OpenSession {
            session_id: id.into(),
            agent: AgentKind::OpenCode,
            cwd: "/repo".into(),
            resume_id: Some("ses_native".into()),
            agent_args: Vec::new(),
            launcher: AgentLauncher::Adk,
            adk_statusline: false,
        };
        let mut kept = vec![row("live-again"), row("still-blocked")];
        let mut live = row("live-again");
        live.launcher = AgentLauncher::Opencode;

        let merged = merge_kept_sessions(vec![live], &mut kept);

        let ids: Vec<&str> = merged.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["live-again", "still-blocked"]);
        // The live row won, and the retained copy is gone — so once that child
        // exits, the next snapshot does not resurrect a closed session.
        assert_eq!(merged[0].launcher, AgentLauncher::Opencode);
        let kept_ids: Vec<&str> = kept.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(kept_ids, ["still-blocked"]);

        let merged = merge_kept_sessions(Vec::new(), &mut kept);
        let ids: Vec<&str> = merged.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["still-blocked"]);
    }

    #[test]
    fn exit_when_empty_only_in_seeded_mode() {
        // Seeded (default) mode exits once the registry empties…
        assert!(should_exit_when_empty(true, 0));
        // …but not while a session (e.g. a web-spawned one) is still running.
        assert!(!should_exit_when_empty(true, 1));
        // Headless `--host` never auto-exits, even with zero children.
        assert!(!should_exit_when_empty(false, 0));
        assert!(!should_exit_when_empty(false, 2));
    }

    #[test]
    fn resume_plan_gates() {
        // Not --host → never touch the saved set.
        assert_eq!(resume_plan(false, false, true, 3), ResumePlan::Skip);
        assert_eq!(resume_plan(false, true, true, 3), ResumePlan::Skip);
        // --host --no-resume → clear and start empty (regardless of TTY/count).
        assert_eq!(resume_plan(true, true, true, 3), ResumePlan::Clear);
        assert_eq!(resume_plan(true, true, false, 0), ResumePlan::Clear);
        // --host, nothing saved → nothing to do.
        assert_eq!(resume_plan(true, false, true, 0), ResumePlan::Skip);
        // --host, saved, non-TTY → leave intact (can't prompt), don't resume.
        assert_eq!(resume_plan(true, false, false, 2), ResumePlan::Skip);
        // --host, saved, TTY → prompt.
        assert_eq!(resume_plan(true, false, true, 2), ResumePlan::Prompt);
    }

    #[tokio::test]
    async fn open_sessions_snapshot_follows_order() {
        // The persisted snapshot walks the deterministic ordering and carries
        // each child's cwd. `test_sleeper` children all report cwd "/tmp".
        let mut reg = ChildRegistry::new();
        reg.add(
            DetachedChild::test_sleeper("alpha")
                .with_agent_args(&["--sandbox", "danger-full-access"]),
        )
        .await;
        reg.add(DetachedChild::test_sleeper("beta")).await;
        let snap = reg.open_sessions();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].session_id, "alpha");
        assert_eq!(snap[1].session_id, "beta");
        assert_eq!(snap[0].agent, AgentKind::Claude);
        assert_eq!(snap[0].cwd, "/tmp");
        assert_eq!(snap[0].agent_args, ["--sandbox", "danger-full-access"]);
        assert!(snap[1].agent_args.is_empty());
        // `test_sleeper` children have no captured resume id (Claude).
        assert_eq!(snap[0].resume_id, None);
        // Reap the sleeper processes so the test doesn't leak them.
        reg.reap_all().await;
    }

    // --- ChildRegistry: add / reap / reap_exited / reap_all -----------------
    // These exercise the W1 registry against REAL detached children built from
    // a long-lived `sleep` PTY process (no server, no `claude`) so the
    // concurrent multi-child model can be tested in isolation.

    use super::super::interactive::DetachedChild;

    #[tokio::test]
    async fn registry_add_inserts_and_reap_all_removes_and_cancels() {
        let mut reg = ChildRegistry::new();
        assert_eq!(reg.len(), 0);

        reg.add(DetachedChild::test_sleeper("a")).await;
        reg.add(DetachedChild::test_sleeper("b")).await;
        assert_eq!(reg.len(), 2, "two distinct children must be tracked");

        reg.reap_all().await;
        assert_eq!(reg.len(), 0);
    }

    #[tokio::test]
    async fn registry_add_same_id_replaces_and_cancels_previous() {
        let mut reg = ChildRegistry::new();
        reg.add(DetachedChild::test_sleeper("dup")).await;
        // Re-adding the same id (e.g. a re-open) cancels the previous child and
        // keeps exactly one entry.
        reg.add(DetachedChild::test_sleeper("dup")).await;
        assert_eq!(reg.len(), 1, "same-id add must replace, not duplicate");
        reg.reap_all().await;
    }

    #[tokio::test]
    async fn registry_reap_exited_removes_only_dead_children() {
        let mut reg = ChildRegistry::new();
        // A still-running child …
        reg.add(DetachedChild::test_sleeper("alive")).await;
        // … and one we kill out from under the registry so it has exited.
        let mut dead = DetachedChild::test_sleeper("dead");
        dead.cancel().await; // terminate its process
        assert!(dead.has_exited());
        reg.add(dead).await;
        assert_eq!(reg.len(), 2);

        let reaped = reg.reap_exited().await;
        assert_eq!(reaped, vec!["dead".to_string()], "only the exited child");
        assert_eq!(reg.len(), 1, "the live child must remain");

        reg.reap_all().await;
        assert_eq!(reg.len(), 0);
    }

    // --- keyframe re-seed: ChildRegistry::pty_keyframes ---------------------

    #[tokio::test]
    async fn pty_keyframes_collects_painted_children_in_order() {
        let mut reg = ChildRegistry::new();
        assert!(
            reg.pty_keyframes().is_empty(),
            "an empty registry has no keyframes"
        );

        let a = DetachedChild::test_sleeper("a");
        a.feed_parser(b"SCREEN A");
        reg.add(a).await;
        let b = DetachedChild::test_sleeper("b");
        b.feed_parser(b"SCREEN B");
        reg.add(b).await;

        let frames = reg.pty_keyframes();
        assert_eq!(frames.len(), 2, "one keyframe per painted child");
        // Deterministic insertion order: a before b.
        assert_eq!(frames[0].0, "a");
        assert_eq!(frames[1].0, "b");
        // Dims from the child PTY (80×24) and the painted screen carried through.
        assert_eq!((frames[0].1, frames[0].2), (80, 24));
        assert!(String::from_utf8_lossy(&frames[0].3).contains("SCREEN A"));
        assert!(String::from_utf8_lossy(&frames[1].3).contains("SCREEN B"));

        reg.reap_all().await;
    }

    // --- W2: ChildRegistry::focus — the `focus(live_id)` host entrypoint -----

    #[tokio::test]
    async fn registry_focus_switches_bridge_and_repaints() {
        let mut reg = ChildRegistry::new();
        reg.add(DetachedChild::test_sleeper("one")).await;
        reg.add(DetachedChild::test_sleeper("two")).await;

        // Capture sink, raw mode OFF (no TTY in the test harness).
        let mut fm = FocusManager::with_sink(Vec::<u8>::new(), false);

        // Focus a tracked child → true, bridge attached, id updated.
        assert!(reg.focus(&mut fm, "one").expect("focus one"));
        assert_eq!(fm.focused_id(), Some("one"));
        assert_eq!(fm.bridge().attached_id(), Some("one"));
        assert!(reg.get("one").unwrap().is_focused());

        // Switch focus → previous child detached, new child focused.
        assert!(reg.focus(&mut fm, "two").expect("focus two"));
        assert_eq!(fm.focused_id(), Some("two"));
        assert!(!reg.get("one").unwrap().is_focused(), "previous unfocused");
        assert!(reg.get("two").unwrap().is_focused());

        // Focusing an unknown id is a no-op (false), focus unchanged.
        assert!(!reg.focus(&mut fm, "missing").expect("focus missing"));
        assert_eq!(fm.focused_id(), Some("two"));

        reg.reap_all().await;
    }

    // --- W3: deterministic ordering + index/next/prev focus -----------------

    #[tokio::test]
    async fn registry_preserves_insertion_order_through_reaps() {
        let mut reg = ChildRegistry::new();
        reg.add(DetachedChild::test_sleeper("a")).await;
        reg.add(DetachedChild::test_sleeper("b")).await;
        reg.add(DetachedChild::test_sleeper("c")).await;
        assert_eq!(reg.ordered_ids(), &["a", "b", "c"]);

        // Reaping the middle child (exited, re-added in its slot) keeps the
        // relative order of the rest.
        let mut b = DetachedChild::test_sleeper("b");
        b.cancel().await;
        reg.add(b).await;
        assert_eq!(reg.reap_exited().await, ["b"]);
        assert_eq!(reg.ordered_ids(), &["a", "c"]);

        // A same-id re-add keeps its original slot (does not jump to the end).
        reg.add(DetachedChild::test_sleeper("a")).await;
        assert_eq!(reg.ordered_ids(), &["a", "c"]);

        reg.reap_all().await;
        assert!(reg.ordered_ids().is_empty());
    }

    #[tokio::test]
    async fn registry_focus_index_uses_deterministic_order() {
        let mut reg = ChildRegistry::new();
        reg.add(DetachedChild::test_sleeper("a")).await;
        reg.add(DetachedChild::test_sleeper("b")).await;
        reg.add(DetachedChild::test_sleeper("c")).await;
        let mut fm = FocusManager::with_sink(Vec::<u8>::new(), false);

        // Index 0 → first inserted ("a").
        assert!(reg.focus_index(&mut fm, 0).expect("focus 0"));
        assert_eq!(fm.focused_id(), Some("a"));
        // Index 2 → third inserted ("c").
        assert!(reg.focus_index(&mut fm, 2).expect("focus 2"));
        assert_eq!(fm.focused_id(), Some("c"));
        // Out-of-range index is a no-op (false), focus unchanged.
        assert!(!reg.focus_index(&mut fm, 9).expect("focus 9"));
        assert_eq!(fm.focused_id(), Some("c"));

        reg.reap_all().await;
    }

    #[tokio::test]
    async fn registry_focus_step_wraps_both_directions() {
        let mut reg = ChildRegistry::new();
        reg.add(DetachedChild::test_sleeper("a")).await;
        reg.add(DetachedChild::test_sleeper("b")).await;
        reg.add(DetachedChild::test_sleeper("c")).await;
        let mut fm = FocusManager::with_sink(Vec::<u8>::new(), false);

        // With no focus, next picks the first child.
        assert!(reg.focus_step(&mut fm, true).expect("next"));
        assert_eq!(fm.focused_id(), Some("a"));
        // next → b → c → wrap to a.
        reg.focus_step(&mut fm, true).expect("next");
        assert_eq!(fm.focused_id(), Some("b"));
        reg.focus_step(&mut fm, true).expect("next");
        assert_eq!(fm.focused_id(), Some("c"));
        reg.focus_step(&mut fm, true).expect("next");
        assert_eq!(fm.focused_id(), Some("a"), "next wraps from last to first");
        // prev wraps from first to last.
        reg.focus_step(&mut fm, false).expect("prev");
        assert_eq!(fm.focused_id(), Some("c"), "prev wraps from first to last");

        reg.reap_all().await;
    }

    #[tokio::test]
    async fn registry_focus_step_is_noop_with_no_children() {
        let reg = ChildRegistry::new();
        let mut fm = FocusManager::with_sink(Vec::<u8>::new(), false);
        assert!(!reg.focus_step(&mut fm, true).expect("next"));
        assert!(!reg.focus_step(&mut fm, false).expect("prev"));
        assert_eq!(fm.focused_id(), None);
    }

    // --- W3: status-bar input derivation ------------------------------------

    #[tokio::test]
    async fn status_children_and_focused_index_track_order() {
        let mut reg = ChildRegistry::new();
        reg.add(DetachedChild::test_sleeper("aaaa1111-xxxx")).await;
        reg.add(DetachedChild::test_sleeper("bbbb2222-yyyy")).await;

        let children = reg.status_children();
        assert_eq!(
            children,
            vec![ChildInfo::new("aaaa1111"), ChildInfo::new("bbbb2222")],
            "labels are the shortened first uuid segment, in insertion order"
        );
        assert_eq!(reg.focused_index(Some("bbbb2222-yyyy")), Some(1));
        assert_eq!(reg.focused_index(None), None);
        assert_eq!(reg.focused_index(Some("not-tracked")), None);

        reg.reap_all().await;
    }

    #[test]
    fn short_label_shortens_uuid() {
        assert_eq!(short_label("abcd1234-5678-90ef"), "abcd1234");
        assert_eq!(short_label("short"), "short");
        // A long first segment is capped at 8 chars.
        assert_eq!(short_label("0123456789abcdef"), "01234567");
    }
}
