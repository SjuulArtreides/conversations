#[path = "local_terminal.rs"]
mod local_terminal;
#[path = "local_transfer.rs"]
pub(super) mod local_transfer;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use super::authz::{self, dispatch::Decision, SignedFrame};
use super::cmux::Cmux;
use super::protocol::{
    clear_pending_end, mark_pending_end, pending_end, read_message, release_marker, reopen_marker,
    wrapper_alive, wrapper_owners, write_message, ClientMessage, Launch, Registration, Screen,
    ServerMessage,
};
use super::Backend;
use crate::cli::GlobalOpts;
use crate::commands::live::host::ControlCommand;
use crate::commands::live::interactive::sanitize_reply;

struct RuntimeGuard {
    _lock: File,
    socket: PathBuf,
}
impl RuntimeGuard {
    fn acquire(dir: &Path) -> Result<Self> {
        if !dir.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let meta = std::fs::symlink_metadata(dir)?;
        // SAFETY: geteuid has no preconditions.
        ensure!(
            meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "runtime directory must be an owner-only directory (chmod 700): {}",
            dir.display()
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("host.lock"))?;
        // SAFETY: lock is an open file descriptor kept alive by this guard.
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "a host already owns {}",
            dir.display()
        );
        let socket = dir.join("host.sock");
        if socket.exists() {
            std::fs::remove_file(&socket)?;
        }
        Ok(Self {
            _lock: lock,
            socket,
        })
    }
}
impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

async fn codex_delivery_loop(host: Arc<Host>) {
    let mut workers = HashMap::new();
    loop {
        let sessions: Vec<String> = host
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, session)| {
                session.registration.launch.agent == conversations_core::AgentKind::Codex
            })
            .map(|(id, _)| id.clone())
            .collect();
        workers.retain(|id, _| sessions.contains(id));
        for id in sessions {
            if workers.contains_key(&id) {
                continue;
            }
            let client = match crate::commands::messaging::MessagingClient::with_cookie(
                &host.server,
                &id,
                host.cookie.clone(),
            )
            .map(|client| client.with_local_dir(host.dir.clone()))
            {
                Ok(client) => client,
                Err(error) => {
                    eprintln!("Conversations Codex delivery unavailable: {error}");
                    continue;
                }
            };
            let private = dirs::home_dir()
                .map(|p| p.join(".local/state/ac-codex").join(format!("{id}.sock")));
            let socket = private.filter(|p| p.exists()).or_else(|| {
                std::env::var_os("CODEX_HOME")
                    .map(PathBuf::from)
                    .or_else(|| dirs::home_dir().map(|p| p.join(".codex")))
                    .map(|p| p.join("app-server-control/app-server-control.sock"))
            });
            if let Some(socket) = socket {
                match crate::commands::delivery::start_codex_at(client, socket) {
                    Ok(guard) => {
                        workers.insert(id, guard);
                    }
                    Err(error) => eprintln!("Conversations Codex delivery unavailable: {error}"),
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

struct Session {
    local_input_bytes: u64,
    readiness: Arc<Mutex<conversations_core::private_channel::AgentReadiness>>,
    local_input_dropped_chunks: u64,
    registration: Registration,
    resume_published: Option<String>,
    opencode_title_published: Option<String>,
    commands: mpsc::Sender<ServerMessage>,
    /// The wrapper process, from the socket's peer credentials.
    pid: Option<u32>,
    /// This connection's screen, for private-channel subscribers (held weakly
    /// by them: a reconnect replaces it and needs a fresh subscribe). Also the
    /// connection's identity for transfer receipts.
    screen: Option<super::protocol::SharedScreen>,
}

pub(super) struct Host {
    dir: PathBuf,
    cwd: PathBuf,
    server: String,
    client: reqwest::Client,
    cookie: Option<String>,
    availability: crate::commands::live::availability::AvailabilitySignal,
    backend: Backend,
    cmux: Option<Cmux>,
    sessions: Mutex<HashMap<String, Session>>,
    registration_lock: tokio::sync::Mutex<()>,
    /// Serialize explicit renames with automatic title reconciliation.
    title_lock: tokio::sync::Mutex<()>,
    pending: Mutex<HashMap<String, oneshot::Sender<()>>>,
    peer_inputs: Mutex<HashMap<String, (String, oneshot::Sender<bool>)>>,
    interactions: Interactions,
    stream: StreamState,
    transfers: super::transfers::TransferLedger,
    tuning: LaunchTuning,
    /// Resolved startup recipe. The daemon reads it once from the local
    /// settings file; a server or signed op never touches it.
    launch_env: LaunchEnv,
    /// The loopback management UI this host serves, when it owns its
    /// configured port. The owner rotates its one-time link over the local
    /// socket (`open`).
    web: Option<Arc<super::web::WebState>>,
}

/// Where the daemon looks when the ambient shell did not bring the Arrakis DevKit
/// dev-shell env with it. Default is "trust only the ambient env" — a generic
/// (non-Arrakis) install leaves this empty and sees no behavior change.
#[derive(Debug, Default, Clone)]
pub(super) struct LaunchEnv {
    arrakis_home: Option<PathBuf>,
    launcher_path: Vec<PathBuf>,
}

impl LaunchEnv {
    /// Read the persisted recipe from `local-host.json`. Settings that do not
    /// resolve to an existing directory at startup are dropped with a visible
    /// message; nothing fails, so an unrelated launch mode still works.
    pub(super) fn from_settings(settings: &super::settings::HostLaunchEnv) -> Self {
        let arrakis_home = settings
            .arrakis_home
            .as_ref()
            .and_then(|path| match std::fs::canonicalize(path) {
                Ok(canonical) if canonical.is_dir() => Some(canonical),
                Ok(_) => {
                    eprintln!(
                        "local-host.json host_env.arrakis_home is not a directory: {} — ignoring",
                        path.display()
                    );
                    None
                }
                Err(error) => {
                    eprintln!(
                        "local-host.json host_env.arrakis_home {} cannot be read: {error:#} — ignoring",
                        path.display()
                    );
                    None
                }
            });
        let launcher_path = settings
            .launcher_path
            .iter()
            .filter_map(|path| match std::fs::canonicalize(path) {
                Ok(canonical) if canonical.is_dir() => Some(canonical),
                _ => {
                    eprintln!(
                        "local-host.json host_env.launcher_path entry {} is not a directory — dropping",
                        path.display()
                    );
                    None
                }
            })
            .collect();
        Self {
            arrakis_home,
            launcher_path,
        }
    }

    /// `ARRAKIS_HOME` for an `adk code` launch. Ambient env wins — the
    /// configured path is a FALLBACK, not an override, so a user who does
    /// start the daemon from the dev shell still gets whatever their direnv
    /// says.
    pub(super) fn resolved_arrakis_home(&self) -> Option<PathBuf> {
        crate::commands::live::interactive::adk_session_cwd()
            .ok()
            .or_else(|| self.arrakis_home.clone())
    }

    /// Environment used to resolve launcher binaries. The configured
    /// `launcher_path` is prepended to the ambient `PATH` so a directory the
    /// user explicitly chose wins over whatever a login shell later sets.
    pub(super) fn resolve_environment(&self) -> HashMap<std::ffi::OsString, std::ffi::OsString> {
        let mut env: HashMap<_, _> = std::env::vars_os().collect();
        if !self.launcher_path.is_empty() {
            let mut parts: Vec<PathBuf> = self.launcher_path.clone();
            if let Some(current) = env.get(std::ffi::OsStr::new("PATH")) {
                parts.extend(std::env::split_paths(current));
            }
            if let Ok(joined) = std::env::join_paths(parts) {
                env.insert("PATH".into(), joined);
            }
        }
        env
    }
}

/// Interaction cancels waiting for their wrapper's answer (one per session),
/// and which `(session, authority)` attributions this process already wrote to
/// `session_interactions` in authz.db. The durable rows are the record: they
/// have no age or count limit and outlive a host restart; this set only saves
/// rewriting them.
#[derive(Default)]
pub(super) struct Interactions {
    recorded: Mutex<std::collections::HashSet<(String, authz::cutover::Authority)>>,
    pending: Mutex<HashMap<String, (String, oneshot::Sender<super::protocol::CancelOutcome>)>>,
}

const CANCEL_WAIT: Duration = Duration::from_secs(3);
/// How long a guarded peer message handed to a wrapper may still be typed: the
/// host's own wait for its outcome. After it, the host has reported the outcome
/// uncertain, so the wrapper must not type it late.
const PEER_INPUT_WAIT: Duration = Duration::from_secs(5);

/// A remote effect refused to a wrapper that cannot enforce revocation.
#[derive(Debug)]
pub(super) struct UpgradeRequired;

impl std::fmt::Display for UpgradeRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this terminal's wrapper predates revocation enforcement and accepts no remote input; restart the session to upgrade it")
    }
}

impl std::error::Error for UpgradeRequired {}

/// How a host starts and waits for its wrappers. Fixed by the host process, never
/// by a signed operation or the central server; tests substitute them.
pub(super) struct LaunchTuning {
    /// How long a launch waits for its wrapper to register before the outcome is
    /// declared unknown (not failed).
    pub registration_deadline: Duration,
    /// The executable that runs `host wrap`; the current executable by default.
    pub executable: Option<PathBuf>,
}

impl Default for LaunchTuning {
    fn default() -> Self {
        Self {
            registration_deadline: Duration::from_secs(10),
            executable: None,
        }
    }
}

/// Per-channel ordering for stream-class signed frames. It is NOT the replay
/// defence: each frame's `jti` is consumed durably in `authz.db` before this
/// runs, so no frame is delivered twice in this process or after a restart. This
/// map only keeps the frames of ONE channel in order (a stale one is dropped).
/// Keys are (credential, session, domain, channel): input, resize and other
/// signers never share a sequence, so one cannot make another look stale.
#[derive(Default)]
pub(super) struct StreamState {
    last: Mutex<HashMap<ChannelKey, (u64, i64)>>,
}

type ChannelKey = (String, String, &'static str, String);

impl StreamState {
    const MAX_CHANNELS: usize = 4096;
    /// A channel idle this long cannot have a live frame left (lifetime + skew).
    const IDLE_SECS: i64 = 120;

    /// Run `deliver` only if `seq` is newer than the last frame accepted on this
    /// channel; the check and the hand-off are one critical section.
    fn ordered<T>(
        &self,
        op: &authz::envelope::Verified,
        domain: &'static str,
        ch: &str,
        seq: u64,
        deliver: impl FnOnce() -> T,
    ) -> Option<T> {
        let now = authz::store::now_secs();
        let mut last = self.last.lock().unwrap();
        if last.len() >= Self::MAX_CHANNELS {
            last.retain(|_, (_, used)| *used >= now - Self::IDLE_SECS);
        }
        let key = (
            op.kid.clone(),
            op.op.session_id().to_owned(),
            domain,
            ch.to_owned(),
        );
        let slot = last.entry(key).or_insert((0, now));
        if seq <= slot.0 {
            return None;
        }
        *slot = (seq, now);
        Some(deliver())
    }
}

/// Select a concrete backend for bare `host`. `Auto` promotes to cmux only when
/// `workspace_id` is present, cmux is reachable, and that workspace still
/// exists in the current tree. An inherited-but-stale `CMUX_WORKSPACE_ID`
/// resolves to Normal rather than attaching this daemon to the wrong workspace.
pub(super) async fn resolve_backend(
    request: Backend,
    cmux_bin: Option<&Path>,
    workspace_id: Option<&str>,
) -> Result<(Backend, Option<Cmux>)> {
    match request {
        Backend::Normal => Ok((Backend::Normal, None)),
        Backend::Cmux => Ok((Backend::Cmux, Some(Cmux::connect(cmux_bin).await?))),
        Backend::Auto => {
            let Some(workspace) = workspace_id.filter(|id| !id.is_empty()) else {
                return Ok((Backend::Normal, None));
            };
            let cmux = match Cmux::connect_to(cmux_bin, workspace.to_owned()).await {
                Ok(cmux) => cmux,
                Err(error) => {
                    eprintln!(
                        "cmux is unreachable from this terminal; selecting the normal backend: {error:#}"
                    );
                    return Ok((Backend::Normal, None));
                }
            };
            match cmux.host_workspace_present().await {
                Ok(true) => Ok((Backend::Cmux, Some(cmux))),
                Ok(false) => {
                    eprintln!(
                        "CMUX_WORKSPACE_ID={workspace} refers to a workspace that is no longer present; selecting the normal backend"
                    );
                    Ok((Backend::Normal, None))
                }
                Err(error) => {
                    eprintln!(
                        "cmux workspace check failed; selecting the normal backend: {error:#}"
                    );
                    Ok((Backend::Normal, None))
                }
            }
        }
    }
}

pub async fn run(
    global: &GlobalOpts,
    dir: &Path,
    backend: Backend,
    cmux_bin: Option<&Path>,
    cookie_file: Option<&Path>,
) -> Result<()> {
    let _owner = crate::commands::host_owner::HostOwner::acquire(&global.server, Some(dir))?;
    let workspace_id = std::env::var("CMUX_WORKSPACE_ID").ok();
    let (backend, cmux) = resolve_backend(backend, cmux_bin, workspace_id.as_deref()).await?;
    let guard = RuntimeGuard::acquire(dir)?;
    // Operations that were mid-dispatch when the previous host stopped have an
    // unknown outcome: mark them so they are reported, never re-executed.
    let recovered = authz::recover(dir)?;
    // A private connection challenge never outlives the host that issued it.
    authz::store::Store::open(dir)?.expire_peer_challenges(authz::store::now_secs())?;
    if recovered > 0 {
        eprintln!("Recovered {recovered} interrupted signed operation(s); none will be repeated.");
    }
    match authz::sessions::import_once(dir, &global.server) {
        Ok(0) => {}
        Ok(adopted) => eprintln!("Adopted {adopted} saved session(s) into the local record."),
        Err(error) => eprintln!("Cannot adopt saved sessions into the local record: {error:#}"),
    }
    // The persisted startup recipe. Loaded once here so a signed
    // `LaunchUpdate` cannot race it (a signed op may replace the launch block
    // on disk, but the host.env block is separate and never written by the
    // signed path). The web block rides the same read.
    let (launch_env, web_settings) = match super::settings::Settings::load(dir) {
        Ok(settings) => (LaunchEnv::from_settings(&settings.host_env), settings.web),
        Err(error) => {
            eprintln!(
                "Cannot read local-host settings; ADK launcher will depend on ambient env only: {error:#}"
            );
            (
                LaunchEnv::default(),
                super::settings::WebSettings::default(),
            )
        }
    };
    // The loopback management UI starts with the host and stops with it. A
    // port already serving something else is never taken over: the host keeps
    // working without the UI and says why.
    let web_server = if web_settings.enabled {
        match super::web::WebServer::bind(dir, &global.server, web_settings.port).await {
            Ok(server) => {
                eprintln!("Local web UI: http://localhost:{}/", server.port());
                Some(server)
            }
            Err(error) => {
                eprintln!(
                    "Local web UI not started: {error:#}. The host works without it; free the port or choose another with `host configure --web-port`."
                );
                None
            }
        }
    } else {
        None
    };
    let listener = tokio::net::UnixListener::bind(&guard.socket)?;
    std::fs::set_permissions(&guard.socket, std::fs::Permissions::from_mode(0o600))?;
    let host = Arc::new(Host {
        dir: dir.to_path_buf(),
        cwd: std::env::current_dir()?,
        server: global.server.clone(),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .build()?,
        cookie: match cookie_file {
            Some(path) => {
                let meta = std::fs::symlink_metadata(path)?;
                ensure!(
                    meta.is_file() && meta.mode() & 0o077 == 0,
                    "cookie file must be owner-only"
                );
                Some(std::fs::read_to_string(path)?.trim().to_string())
            }
            None => crate::credentials::cookie_for(&global.server),
        },
        availability: crate::commands::live::availability::AvailabilitySignal::new(),
        backend,
        cmux,
        sessions: Mutex::new(HashMap::new()),
        registration_lock: tokio::sync::Mutex::new(()),
        title_lock: tokio::sync::Mutex::new(()),
        pending: Mutex::new(HashMap::new()),
        peer_inputs: Mutex::new(HashMap::new()),
        interactions: Interactions::default(),
        stream: StreamState::default(),
        transfers: Default::default(),
        tuning: Default::default(),
        launch_env,
        web: web_server.as_ref().map(|server| server.state()),
    });
    if let (Some(parent), Some(parent_dir)) = (
        std::env::var_os("CONVERSATIONS_WRAPPED"),
        std::env::var_os("CONVERSATIONS_HOST_DIR"),
    ) {
        let parent_dir = PathBuf::from(parent_dir);
        if parent_dir != dir {
            if let Err(error) = release_parent_wrapper(&parent_dir, &parent.to_string_lossy()).await
            {
                let pid: u32 = std::env::var("CONVERSATIONS_WRAPPER_PID")
                    .context("parent wrapper is unavailable and has no process identity")?
                    .parse()
                    .context("invalid parent wrapper process identity")?;
                mark_parent_released(&parent_dir, &parent.to_string_lossy(), pid)?;
                eprintln!("Parent host unavailable; this terminal will stay outside its Conversations sessions: {error:#}");
            }
        }
    }
    if std::env::var("CONVERSATIONS_HOST_CONTROL").as_deref() == Ok("1") {
        if let (Some(cmux), Ok(surface)) = (&host.cmux, std::env::var("CMUX_SURFACE_ID")) {
            if let Err(error) = cmux
                .bind_saved_session(
                    dir,
                    &host.server,
                    &surface,
                    &super::cmux::host_checkpoint(dir),
                )
                .await
            {
                eprintln!("Cannot bind host restore metadata: {error:#}");
            }
        }
    }
    // Auth-off is useful for terminal transport tests. The existing server only
    // permits browser start/end control for an authenticated account.
    eprintln!(
        "Local {backend:?} host → {}\nRuntime: {}",
        host.server,
        dir.display()
    );
    if host.cookie.is_none() {
        eprintln!("No localhost login: terminal mirroring works; browser session creation requires login.");
    }
    let mut jobs = tokio::task::JoinSet::new();
    jobs.spawn(host.availability.clone().monitor(host.server.clone()));
    jobs.spawn(codex_delivery_loop(host.clone()));
    // The management UI lives and dies with the host; a running host never
    // needs a second, unmanaged process for it.
    if let Some(server) = web_server {
        jobs.spawn(async move {
            if let Err(error) = server.serve(std::future::pending()).await {
                eprintln!("Local web UI stopped: {error:#}");
            }
        });
    }
    // Polling reads only pull requests this host was told about by signed
    // operations (its own table), never a list the server supplies.
    jobs.spawn(pr_poll_loop(host.clone()));
    // Publication of explicitly shared sessions only, to THIS host's server.
    jobs.spawn(super::publication::run_loop(
        host.dir.clone(),
        host.server.clone(),
        host.client.clone(),
        host.cookie.clone(),
    ));
    // The browser relay needs no login on an auth-off server, and on an auth-on
    // one it uses the same login as the rest of the host.
    let h = host.clone();
    jobs.spawn(async move {
        relay::relay_loop(h).await;
    });
    if host.cmux.is_some() {
        let h = host.clone();
        jobs.spawn(async move {
            title_loop(h).await;
        });
        let h = host.clone();
        jobs.spawn(async move {
            restore_saved_sessions(h).await;
        });
    }
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
            _ = hangup.recv() => break,
            connection = listener.accept() => {
                let (socket, _) = connection?;
                let h = host.clone();
                jobs.spawn(async move {
                    if let Err(error) = handle_client(h, socket).await {
                        // Socket probes and disconnected wrappers end without another frame.
                        // Keep reporting protocol failures, but not ordinary peer closure.
                        if error.downcast_ref::<std::io::Error>()
                            .is_none_or(|error| error.kind() != std::io::ErrorKind::UnexpectedEof)
                        {
                            eprintln!("Local connection: {error:#}");
                        }
                    }
                });
            }
            Some(_) = jobs.join_next(), if !jobs.is_empty() => {}
        }
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    eprintln!("Host stopped. Wrapped terminals remain local and reconnect when restarted.");
    Ok(())
}

async fn restore_saved_sessions(host: Arc<Host>) {
    let mut delay = Duration::from_secs(1);
    let mut startup = true;
    loop {
        if restore_saved_sessions_once(host.clone(), startup).await {
            startup = false;
            delay = Duration::from_secs(2);
        } else {
            delay = (delay * 2).min(Duration::from_secs(30));
        }
        tokio::time::sleep(delay).await;
    }
}

/// Fault injection for restart-durability tests: debug builds stop the daemon
/// at a named point, as a crash would. Release builds have no such points.
fn crash_point(name: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("CONVERSATIONS_HOST_CRASH_AT").is_ok_and(|point| point == name) {
        eprintln!("CONVERSATIONS_HOST_CRASH_AT={name}: stopping the host");
        std::process::exit(86);
    }
    #[cfg(not(debug_assertions))]
    let _ = name;
}

async fn restore_saved_sessions_once(host: Arc<Host>, startup: bool) -> bool {
    // Existing panes reserve their identity, but may not have a running PTY:
    // explicitly start their adapter without selecting them or creating copies.
    let mut restored = std::collections::HashSet::new();
    let mut inspection_failed = false;
    if let Some(cmux) = &host.cmux {
        match cmux.surface_workspaces().await {
            Ok(surfaces) => {
                for (surface, workspace) in &surfaces {
                    match cmux
                        .saved_session_for_surface(&host.dir, &host.server, surface)
                        .await
                    {
                        Ok(Some(saved)) => {
                            let connected = host
                                .sessions
                                .lock()
                                .unwrap()
                                .contains_key(&saved.session_id);
                            restored.insert(saved.session_id);
                            if !connected {
                                if let Err(error) =
                                    cmux.start_saved_surface(workspace, surface).await
                                {
                                    inspection_failed = true;
                                    eprintln!("Cannot start restored surface {surface}: {error:#}");
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            inspection_failed = true;
                            eprintln!("Cannot inspect restored surface {surface}: {error:#}");
                        }
                    }
                }
            }
            Err(error) => {
                eprintln!("Cannot safely restore sessions without cmux inventory: {error:#}");
                return false;
            }
        }
    }
    if inspection_failed {
        return false;
    }
    // A surviving daemon also wakes panes restored by a later cmux instance.
    // cmux owns the open pane set. Checkpoints can outlive closed panes or a
    // web dismissal; they are resume metadata, not permission to create panes.
    if !startup {
        return true;
    }
    let saved = match authz::sessions::restore(&host.dir, &host.server) {
        Ok(saved) => saved,
        Err(error) => {
            eprintln!("Cannot read restore intent: {error:#}");
            return false;
        }
    };
    for saved in saved {
        let connected = restored.contains(&saved.session_id)
            || host
                .sessions
                .lock()
                .unwrap()
                .contains_key(&saved.session_id);
        // A live wrapper may not have re-registered with this daemon yet.
        if connected || wrapper_alive(&host.dir, &saved.session_id).await {
            continue;
        }
        if !authz::restorable(&host.dir, &saved.session_id) {
            eprintln!(
                "Saved session {} not restored: its signed start was interrupted and its outcome is unknown",
                saved.session_id
            );
            continue;
        }
        let launch = match launch_from_saved_session(saved, &host.cwd, &host.launch_env) {
            Ok(launch) => launch,
            Err(error) => {
                eprintln!("Saved session cannot be restored: {error:#}");
                continue;
            }
        };
        // Only confirmed absence is permanent. Permission and other filesystem
        // errors, like cmux failures below, must leave the checkpoint available.
        if matches!(launch.cwd.try_exists(), Ok(false)) {
            match authz::store::Store::open(&host.dir)
                .and_then(|mut store| store.close_for_restore(&launch.id))
            {
                Ok(()) => {
                    eprintln!("Dropped saved session: terminal working directory does not exist")
                }
                Err(_) => eprintln!("Cannot drop saved session: checkpoint update failed"),
            }
            continue;
        }
        if host.cmux.is_none() {
            if let Err(error) = host.launch(&launch).await {
                eprintln!("Could not restore saved session: {error:#}");
            }
        }
    }
    true
}

pub(super) fn launch_from_saved_session(
    saved: crate::open_sessions::OpenSession,
    cwd: &Path,
    env: &LaunchEnv,
) -> Result<Launch> {
    if saved.agent == conversations_core::AgentKind::Zsh {
        // Restore a fresh login shell under the same mirror identity. Commands
        // previously run inside it (including -c launch arguments) are not replayed.
        return Ok(Launch {
            id: saved.session_id,
            agent: saved.agent,
            cwd: PathBuf::from(saved.cwd),
            command: super::default_command(saved.agent, &[]),
        });
    }
    ensure!(
        !matches!(
            saved.agent,
            conversations_core::AgentKind::Codex | conversations_core::AgentKind::OpenCode
        ) || saved.resume_id.is_some(),
        "Agent has not reported its native resume identity; refusing to resume a Conversations ID"
    );
    launch_from_control(
        ControlCommand::Reopen {
            session_id: saved.session_id,
            cwd: saved.cwd,
            agent: saved.agent,
            resume_id: saved.resume_id,
            agent_args: saved.agent_args,
            launcher: saved.launcher,
            adk_statusline: saved.adk_statusline,
        },
        cwd,
        env,
    )
}

impl Host {
    pub(super) fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(cookie) = &self.cookie {
            request.header(reqwest::header::COOKIE, cookie)
        } else {
            request
        }
    }

    async fn post(&self, path: &str, body: Value) -> Result<()> {
        self.auth(self.client.post(format!("{}{path}", self.server)))
            .json(&body)
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn live_sessions(&self) -> Result<Value> {
        Ok(self
            .auth(self.client.get(format!("{}/api/live", self.server)))
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn launch(&self, launch: &Launch) -> Result<()> {
        uuid::Uuid::parse_str(&launch.id)?;
        ensure!(!launch.command.is_empty(), "missing terminal command");
        ensure!(
            launch.cwd.is_dir(),
            "terminal cwd does not exist: {}",
            launch.cwd.display()
        );
        ensure!(
            !self.sessions.lock().unwrap().contains_key(&launch.id),
            "terminal is already running"
        );
        let mut managed_launch = launch.clone();
        pin_native_executable(&mut managed_launch, &self.launch_env)?;
        let (registered, registration) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            ensure!(
                !pending.contains_key(&launch.id),
                "terminal is already starting"
            );
            pending.insert(launch.id.clone(), registered);
        }
        // An End journaled for an earlier, now exited wrapper is obsolete and
        // must not reach this replacement.
        if !wrapper_alive(&self.dir, &launch.id).await {
            clear_pending_end(&self.dir, &launch.id);
        }
        struct Pending<'a>(&'a Host, String);
        impl Drop for Pending<'_> {
            fn drop(&mut self) {
                self.0.pending.lock().unwrap().remove(&self.1);
            }
        }
        let _pending = Pending(self, launch.id.clone());
        if let Some(cmux) = &self.cmux {
            if !authz::store::Store::open(&self.dir)?
                .is_open_for_restore(&launch.id, &self.server)?
            {
                if let Some(saved) = checkpoint_for_launch(launch) {
                    save_checkpoint(&self.dir, &self.server, saved)?;
                }
            }
            if launch.agent == conversations_core::AgentKind::Claude && launch.command == ["claude"]
            {
                managed_launch
                    .command
                    .extend(["--session-id".to_owned(), launch.id.clone()]);
            }
            if let Err(error) = cmux.launch(&self.dir, &managed_launch).await {
                // An RPC that failed may still have created the workspace.
                return Err(
                    if error.downcast_ref::<super::cmux::NotStarted>().is_some() {
                        error
                    } else {
                        anyhow::Error::new(authz::dispatch::EffectUncertain(format!(
                            "cmux did not confirm the launch: {error:#}"
                        )))
                    },
                );
            }
        } else {
            if launch.agent == conversations_core::AgentKind::Claude && launch.command == ["claude"]
            {
                managed_launch
                    .command
                    .extend(["--session-id".to_owned(), launch.id.clone()]);
            }
            let mut process = tokio::process::Command::new(match &self.tuning.executable {
                Some(executable) => executable.clone(),
                None => std::env::current_exe()?,
            });
            process
                .arg("--server")
                .arg(&self.server)
                .arg("host")
                .arg("--runtime-dir")
                .arg(&self.dir)
                .arg("wrap")
                .arg("--headless")
                .arg("--session-id")
                .arg(&launch.id)
                .arg("--cwd")
                .arg(&launch.cwd)
                .arg("--agent")
                .arg(launch.agent.as_str())
                .arg("--")
                .args(&managed_launch.command)
                .env_remove("CONVERSATIONS_WRAPPED")
                .process_group(0)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null());
            // A failed spawn created no process: a definite non-start.
            let mut child = process.spawn().context("starting headless wrapper")?;
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        // A successful cmux RPC is not evidence that the wrapper started, and a
        // missing registration is not evidence that it did not: the process may
        // simply be slow. From here on a failure is an UNKNOWN outcome.
        let uncertain =
            |what: &str| anyhow::Error::new(authz::dispatch::EffectUncertain(what.to_owned()));
        match tokio::time::timeout(self.tuning.registration_deadline, registration).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(uncertain("the registration was withdrawn")),
            Err(_) => Err(uncertain(&format!(
                "terminal did not register within {} seconds",
                self.tuning.registration_deadline.as_secs_f32()
            ))),
        }
    }

    /// Reuse a detached wrapper instead of waiting for its child to exit.
    async fn reopen_existing(&self, id: &str) -> Result<bool> {
        if pending_end(&self.dir, id).is_some() {
            return Ok(false);
        }
        if self.sessions.lock().unwrap().contains_key(id) {
            return Ok(true);
        }
        let Some(owners) = wrapper_owners(&self.dir, id).await else {
            return Ok(false);
        };
        ensure!(
            !owners.is_empty(),
            "cannot identify the existing terminal wrapper"
        );
        let mut requests = Vec::new();
        for pid in owners {
            let path = reopen_marker(&self.dir, id, pid)?;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
            {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            requests.push(path);
        }
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self.sessions.lock().unwrap().contains_key(id) {
                    return true;
                }
                if !wrapper_alive(&self.dir, id).await {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        for path in requests {
            let _ = std::fs::remove_file(path);
        }
        result.context("existing terminal did not reattach; restart its legacy wrapper")
    }

    /// Records End for the wrapper that owns `id` now, before anything else
    /// acts on it. A wrapper that is reconnecting, or whose daemon dies before
    /// delivery, receives it when it registers again.
    async fn journal_end(&self, id: &str) {
        let registered = self
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|session| session.pid.into_iter().collect());
        let owners = match registered {
            Some(owners) => owners,
            None => match wrapper_owners(&self.dir, id).await {
                Some(owners) => owners,
                None => return,
            },
        };
        if let Err(error) = mark_pending_end(&self.dir, id, &owners) {
            eprintln!("Cannot record End for the terminal: {error:#}");
        }
    }

    fn deliver_end(&self, id: &str) -> bool {
        if let Some(session) = self.sessions.lock().unwrap().get(id) {
            crash_point("end-delivery");
            if session.commands.try_send(ServerMessage::End).is_ok() {
                return true;
            }
            eprintln!("Terminal control queue unavailable");
        }
        false
    }

    /// Hand a message to the terminal attached for `id`. Fails (never silently
    /// drops) when none is attached or its queue is full. Never retried:
    /// replaying keystrokes can execute a command twice.
    /// A remote effect, handed only to a wrapper that re-checks its signed
    /// authority at the write. Checked under the same lock as the hand-off, so
    /// a reconnect by an older wrapper cannot receive it.
    fn deliver_effect(&self, id: &str, message: ServerMessage) -> Result<()> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get(id)
            .context("session is not running on this host")?;
        if !session.registration.effect_gate {
            return Err(UpgradeRequired.into());
        }
        session
            .commands
            .try_send(message)
            .map_err(|_| anyhow::anyhow!("terminal control queue unavailable"))
    }

    fn effect_gated(&self, id: &str) -> Result<()> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get(id)
            .context("session is not running on this host")?;
        if !session.registration.effect_gate {
            return Err(UpgradeRequired.into());
        }
        Ok(())
    }

    /// Durably attribute `authority`'s content to `session`, BEFORE it is handed
    /// to the terminal, so a revocation committed at any later point (by this
    /// process or the `credential revoke` CLI) finds it. An effect that cannot
    /// be attributed is not delivered.
    fn attribute(&self, session: &str, authority: &authz::cutover::Authority) -> Result<()> {
        let key = (session.to_owned(), authority.clone().standing());
        if self.interactions.recorded.lock().unwrap().contains(&key) {
            return Ok(());
        }
        authz::store::Store::open(&self.dir)?.record_interaction(
            session,
            &key.1,
            authz::store::now_secs(),
        )?;
        self.interactions.recorded.lock().unwrap().insert(key);
        Ok(())
    }

    /// A fresh agent process registered for `session`: work attributed to the
    /// previous one is gone with it.
    fn clear_attribution(&self, session: &str) {
        self.interactions
            .recorded
            .lock()
            .unwrap()
            .retain(|(s, _)| s != session);
        if let Err(error) =
            authz::store::Store::open(&self.dir).and_then(|store| store.clear_interactions(session))
        {
            eprintln!("Cannot clear interaction attribution of {session}: {error:#}");
        }
    }

    /// After a local authority change committed: for every running session
    /// with attributed content resting on authority that no longer holds,
    /// ask its wrapper to cancel the CURRENT interaction (never End, detach or
    /// a signal) and report what each did. Sessions with no affected authority
    /// are not touched. An error: the affected work could not be evaluated, so
    /// nothing is known about its interruption.
    pub(super) async fn interrupt_revoked(&self) -> Result<Vec<super::protocol::Interruption>> {
        self.interrupt_revoked_in(None).await
    }

    /// [`Self::interrupt_revoked`], for one session only when `only` is set (a
    /// wrapper reconnecting to a restarted host: a revocation may have
    /// committed while no host ran).
    async fn interrupt_revoked_in(
        &self,
        only: Option<&str>,
    ) -> Result<Vec<super::protocol::Interruption>> {
        let candidates: Vec<String> = self
            .sessions
            .lock()
            .unwrap()
            .keys()
            .filter(|session| only.is_none_or(|only| only == session.as_str()))
            .cloned()
            .collect();
        let dir = self.dir.clone();
        type Affected = (String, Option<String>, Vec<authz::cutover::Authority>);
        let affected = tokio::task::spawn_blocking(move || -> Result<Vec<Affected>> {
            let store = authz::store::Store::open(&dir)?;
            let now = authz::store::now_secs();
            let mut affected = Vec::new();
            for session in candidates {
                let mut revoked = Vec::new();
                for authority in store.interactions(&session)? {
                    if store.authority_denial(&authority, now)?.is_some() {
                        revoked.push(authority);
                    }
                }
                if !revoked.is_empty() {
                    let native = store.session(&session)?.and_then(|s| s.resume_id);
                    affected.push((session, native, revoked));
                }
            }
            Ok(affected)
        })
        .await;
        let affected = affected
            .context("revoked-interaction evaluation did not finish")?
            .context("cannot evaluate revoked interactions")?;
        let mut waits = Vec::new();
        for (session, native, revoked) in affected {
            let request_id = uuid::Uuid::new_v4().to_string();
            let (sender, receiver) = oneshot::channel();
            let sent = {
                let mut pending = self.interactions.pending.lock().unwrap();
                // One outstanding cancel per session.
                if pending.values().any(|(s, _)| *s == session) {
                    None
                } else {
                    pending.insert(request_id.clone(), (session.clone(), sender));
                    Some(self.deliver_effect(
                        &session,
                        ServerMessage::CancelInteraction {
                            request_id: request_id.clone(),
                            native_session: native,
                        },
                    ))
                }
            };
            if matches!(sent, Some(Ok(()))) {
                // Handed to the terminal: these attributions are acted on. Kept
                // otherwise, for the wrapper's next registration.
                self.interactions
                    .recorded
                    .lock()
                    .unwrap()
                    .retain(|(s, a)| *s != session || !revoked.contains(a));
                if let Err(error) = authz::store::Store::open(&self.dir)
                    .and_then(|mut store| store.forget_interactions(&session, &revoked))
                {
                    eprintln!("Cannot settle interaction attribution of {session}: {error:#}");
                }
            }
            waits.push(async move {
                let outcome = match sent {
                    None => Some(super::protocol::CancelOutcome::AlreadyRequested),
                    // The session reconnected through a wrapper that cannot cancel.
                    Some(Err(error)) if error.downcast_ref::<UpgradeRequired>().is_some() => {
                        Some(super::protocol::CancelOutcome::UpgradeRequired)
                    }
                    Some(Err(_)) => None,
                    Some(Ok(())) => tokio::time::timeout(CANCEL_WAIT, receiver)
                        .await
                        .ok()
                        .and_then(Result::ok),
                };
                (session, request_id, outcome)
            });
        }
        let mut out = Vec::new();
        for (session, request_id, outcome) in futures::future::join_all(waits).await {
            self.interactions
                .pending
                .lock()
                .unwrap()
                .remove(&request_id);
            out.push(super::protocol::Interruption { session, outcome });
        }
        out.sort_by(|a, b| a.session.cmp(&b.session));
        Ok(out)
    }
}

async fn handle_client(host: Arc<Host>, mut socket: tokio::net::UnixStream) -> Result<()> {
    let first = tokio::time::timeout(Duration::from_secs(5), read_message(&mut socket)).await??;
    match first {
        ClientMessage::Passkey { request } => {
            // Trust comes from the transport: this stream was accepted on the
            // owner-only Unix socket. No field of the request can assert it.
            let local = authz::passkey::LocalSocket::from_unix_socket(&socket);
            let dir = host.dir.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let settings = super::settings::Settings::load(&dir)?;
                authz::passkey::handle(
                    &dir,
                    &settings,
                    request,
                    Some(&local),
                    authz::store::now_secs(),
                )
            })
            .await?;
            let reply = match outcome {
                Ok(response) => ServerMessage::Passkey {
                    response: Some(response),
                    error: None,
                },
                Err(error) => ServerMessage::Passkey {
                    response: None,
                    error: Some(format!("{error:#}")),
                },
            };
            return write_message(&mut socket, &reply).await;
        }
        ClientMessage::PeerInputRequest {
            id,
            request_id,
            native_session,
            message_id,
        } => {
            uuid::Uuid::parse_str(&id)?;
            uuid::Uuid::parse_str(&request_id)?;
            uuid::Uuid::parse_str(&native_session)?;
            // The text is the host's own record of a signed message, and is
            // released only while its sender still holds a current grant for this
            // session. Nothing a local caller sends is ever typed.
            let text = {
                let (dir, id, message_id) = (host.dir.clone(), id.clone(), message_id.clone());
                tokio::task::spawn_blocking(move || {
                    let mut store = authz::store::Store::open(&dir)?;
                    let now = authz::store::now_secs();
                    // The sender's grant activation is read BEFORE the message:
                    // a removal commits the grant and the message's state together,
                    // so a message still deliverable after this read was sent under
                    // this activation (a re-grant after a removal never revives it).
                    let kid = store.inbox_sender(&id, &message_id)?;
                    let epoch = match &kid {
                        Some(kid) => store.current_epoch(
                            &authz::cutover::Authority::sender(kid, &id, None, None),
                            now,
                        )?,
                        None => None,
                    };
                    let row = store.inbox_message(&id, &message_id, now)?;
                    let deadline = now + PEER_INPUT_WAIT.as_secs() as i64;
                    row.filter(|row| Some(&row.kid) == kid.as_ref() && epoch.is_some())
                        .map(|row| {
                            let authority = authz::cutover::Authority::sender(
                                &row.kid,
                                &id,
                                epoch,
                                Some(deadline),
                            );
                            crate::commands::conversations_delivery::reply_text(
                                &row.into_agent_message(),
                            )
                            .map(|text| (text, authority))
                        })
                        .transpose()
                })
                .await??
            };
            let Some((text, authority)) = text else {
                return write_message(
                    &mut socket,
                    &ServerMessage::PeerInputResult {
                        request_id,
                        written: false,
                    },
                )
                .await;
            };
            ensure!(
                text.chars().count() <= 4096,
                "guarded peer input exceeds 4096 characters"
            );
            let (sender, receiver) = oneshot::channel();
            host.peer_inputs
                .lock()
                .unwrap()
                .insert(request_id.clone(), (id.clone(), sender));
            // Attributed before the hand-off; an unattributable message is not typed.
            let attributed = match host.attribute(&id, &authority) {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("Cannot attribute peer input to {id}: {error:#}");
                    false
                }
            };
            let sent = attributed
                && host
                    .sessions
                    .lock()
                    .unwrap()
                    .get(&id)
                    .is_some_and(|session| {
                        session.registration.launch.agent == conversations_core::AgentKind::Claude
                            && session.registration.effect_gate
                            && session
                                .commands
                                .try_send(ServerMessage::PeerInput {
                                    request_id: request_id.clone(),
                                    native_session,
                                    text,
                                    authority: Some(authority.clone()),
                                })
                                .is_ok()
                    });
            if !sent {
                host.peer_inputs.lock().unwrap().remove(&request_id);
                return write_message(
                    &mut socket,
                    &ServerMessage::PeerInputResult {
                        request_id,
                        written: false,
                    },
                )
                .await;
            }
            let outcome = tokio::time::timeout(Duration::from_secs(5), receiver).await;
            host.peer_inputs.lock().unwrap().remove(&request_id);
            let reply = match outcome {
                Ok(Ok(written)) => ServerMessage::PeerInputResult { request_id, written },
                _ => ServerMessage::Error { message: "guarded input outcome uncertain; do not replay without receipt reconciliation; update/restart legacy wrapper if necessary".into() },
            };
            write_message(&mut socket, &reply).await
        }
        ClientMessage::Handoff {
            source,
            idempotency_key,
            brief,
            agent,
            model,
            variant,
        } => {
            let reply = match handoff_local(
                &host,
                &source,
                &idempotency_key,
                &brief,
                agent,
                model.as_deref(),
                variant.as_deref(),
            )
            .await
            {
                Ok((session_id, state, error)) => ServerMessage::HandoffResult {
                    session_id,
                    state,
                    error,
                },
                Err(error) => ServerMessage::Error {
                    message: format!("{error:#}"),
                },
            };
            return write_message(&mut socket, &reply).await;
        }
        ClientMessage::EndHandoff { source, session_id } => {
            let reply = match end_handoff_local(&host, &source, &session_id).await {
                Ok((status, host_notified)) => ServerMessage::EndHandoffResult {
                    status,
                    host_notified,
                },
                Err(error) => ServerMessage::Error {
                    message: error.to_string(),
                },
            };
            write_message(&mut socket, &reply).await
        }
        ClientMessage::AuthorityChanged => {
            let reply = match host.interrupt_revoked().await {
                Ok(sessions) => ServerMessage::Interruptions { sessions },
                Err(error) => {
                    eprintln!("Interruption after an authority change: {error:#}");
                    ServerMessage::Error {
                        message: "interruption unavailable: affected work could not be evaluated"
                            .into(),
                    }
                }
            };
            write_message(&mut socket, &reply).await
        }
        ClientMessage::LocalTransfer {
            session,
            transfer,
            content,
        } => {
            let status = local_transfer::send(&host, &session, &transfer, content).await;
            write_message(&mut socket, &ServerMessage::LocalTransferResult { status }).await
        }
        ClientMessage::LocalAction { action } => {
            let result = local_terminal::action(&host, action).await;
            let reply = match result {
                Ok(id) => ServerMessage::Started { id },
                Err(error) => ServerMessage::Error {
                    message: error.to_string(),
                },
            };
            write_message(&mut socket, &reply).await
        }
        ClientMessage::OwnerRename { session, title } => {
            let reply = if !authz::envelope::valid_session_id(&session) {
                ServerMessage::Error {
                    message: "invalid session id".into(),
                }
            } else if let Err(reason) = super::history::validate_title(&title) {
                ServerMessage::Error {
                    message: reason.to_string(),
                }
            } else if !matches!(authz::lookup(&host.dir, &session), Ok(Some(_))) {
                ServerMessage::Error {
                    message: "unknown local session".into(),
                }
            } else {
                match commit_owner_rename(&host, &session, &title, None).await {
                    Ok(title) => ServerMessage::OwnerRenameApplied { title },
                    Err(error) => ServerMessage::Error {
                        message: error.to_string(),
                    },
                }
            };
            write_message(&mut socket, &reply).await
        }
        ClientMessage::LocalTerminal { id } => local_terminal::attach(host, socket, id).await,
        command @ (ClientMessage::WebBootstrap | ClientMessage::WebBootstrapReset) => {
            // The socket itself is the authority (owner-only). The link's
            // token is written only to this stream — never to the log.
            let url = host
                .web
                .as_ref()
                .map(|state| {
                    if matches!(command, ClientMessage::WebBootstrapReset) {
                        state.reset_browser_access()?;
                    }
                    state.rotate_bootstrap_url()
                })
                .transpose()?;
            write_message(&mut socket, &ServerMessage::WebBootstrap { url }).await
        }
        ClientMessage::Status => {
            let result = (|| -> Result<Vec<super::protocol::SessionStatus>> {
                let store = authz::store::Store::open(&host.dir)?;
                let sessions = host
                    .sessions
                    .lock()
                    .unwrap()
                    .values()
                    .map(|s| {
                        Ok(super::protocol::SessionStatus {
                            native_session_id: store.native_for(&s.registration.launch.id)?,
                            native_identity_supported: true,
                            readiness: Some(*s.readiness.lock().unwrap()),
                            registration: s.registration.clone(),
                            local_input_bytes: s.local_input_bytes,
                            local_input_dropped_chunks: s.local_input_dropped_chunks,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(sessions)
            })();
            let reply = match result {
                Ok(sessions) => ServerMessage::Status {
                    backend: host.backend,
                    sessions,
                },
                Err(error) => ServerMessage::Error {
                    message: format!("could not read host status: {error:#}"),
                },
            };
            write_message(&mut socket, &reply).await
        }
        ClientMessage::Start { launch } => {
            let reply = match host.launch(&launch).await {
                Ok(()) => ServerMessage::Started { id: launch.id },
                Err(error) => ServerMessage::Error {
                    message: error.to_string(),
                },
            };
            write_message(&mut socket, &reply).await
        }
        ClientMessage::Release { id } => {
            uuid::Uuid::parse_str(&id)?;
            let sent = host
                .sessions
                .lock()
                .unwrap()
                .get(&id)
                .is_some_and(|session| session.commands.try_send(ServerMessage::Detach).is_ok());
            ensure!(
                sent,
                "owning wrapper is not connected or its control queue is full"
            );
            // Dismiss graduates the mirror without sending End to the local
            // process. This is the same soft-close boundary used by viewers.
            host.post(&format!("/api/live/{id}/dismiss"), json!({}))
                .await?;
            write_message(&mut socket, &ServerMessage::Released).await
        }
        ClientMessage::Register { registration } => session(host, socket, registration).await,
        _ => bail!("first local frame must register, start, or request status"),
    }
}

pub async fn release_parent_wrapper(runtime: &Path, id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)?;
    match super::request(runtime, ClientMessage::Release { id: id.to_owned() }).await? {
        ServerMessage::Released => Ok(()),
        ServerMessage::Error { message } => bail!("{message}"),
        _ => bail!("unexpected wrapper release response"),
    }
}

fn mark_parent_released(runtime: &Path, id: &str, pid: u32) -> Result<()> {
    uuid::Uuid::parse_str(id)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(runtime)?;
    let meta = std::fs::symlink_metadata(runtime)?;
    // SAFETY: geteuid has no preconditions.
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "parent runtime must be an owner-only directory: {}",
        runtime.display()
    );
    let released = runtime.join("released");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&released)?;
    let meta = std::fs::symlink_metadata(&released)?;
    // SAFETY: geteuid has no preconditions.
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "release marker directory must be owner-only: {}",
        released.display()
    );
    let marker = release_marker(runtime, id, pid);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(marker)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Guard aborts tasks even when host shutdown cancels this connection future.
struct SessionGuard {
    host: Arc<Host>,
    id: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// This connection's screen; removed only if it is still the current one.
    screen: Option<super::protocol::SharedScreen>,
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.host
            .peer_inputs
            .lock()
            .unwrap()
            .retain(|_, (source, _)| source != &self.id);
        self.host
            .interactions
            .pending
            .lock()
            .unwrap()
            .retain(|_, (source, _)| source != &self.id);
        self.host.sessions.lock().unwrap().remove(&self.id);
        if let Some(own) = &self.screen {
            private_channel::abandon_receipts(&self.host.dir, &self.id, own);
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn session(
    host: Arc<Host>,
    mut socket: tokio::net::UnixStream,
    mut registration: Registration,
) -> Result<()> {
    let pid = socket
        .peer_cred()
        .ok()
        .and_then(|cred| cred.pid())
        .and_then(|pid| u32::try_from(pid).ok());
    if std::env::var("CONVERSATIONS_WRAPPED").is_ok_and(|own| own == registration.launch.id)
        || (host.cmux.is_some()
            && registration.surface.as_deref().is_some_and(|surface| {
                std::env::var("CMUX_SURFACE_ID").is_ok_and(|own| own.eq_ignore_ascii_case(surface))
            }))
    {
        write_message(&mut socket, &ServerMessage::Detach).await?;
        return Ok(());
    }
    // Serialize workspace assignment and insertion so simultaneous wrappers
    // cannot both claim the same sidebar entry.
    let registration_lock = host.registration_lock.lock().await;
    let id = registration.launch.id.clone();
    ensure!(
        !host.sessions.lock().unwrap().contains_key(&id),
        "terminal already connected"
    );
    uuid::Uuid::parse_str(&id)?;
    // Every managed terminal gets its canonical local record here, whatever
    // started it and whether or not its native id is known yet.
    {
        let (dir, id, agent, cwd, command) = (
            host.dir.clone(),
            id.clone(),
            registration.launch.agent,
            registration.launch.cwd.to_string_lossy().into_owned(),
            registration.launch.command.clone(),
        );
        tokio::task::spawn_blocking(move || {
            authz::register_launch(&dir, &id, agent, &cwd, &command)?;
            // A fresh Claude continuation has an explicitly allocated native
            // UUID, independent of the imported Conversations ID.
            if agent == conversations_core::AgentKind::Claude
                && authz::store::Store::open(&dir)?
                    .import_resume_mode(&id)?
                    .as_deref()
                    == Some("continuation")
            {
                if let Some(native) = command
                    .windows(2)
                    .find(|pair| pair[0] == "--session-id")
                    .map(|pair| &pair[1])
                {
                    uuid::Uuid::parse_str(native)?;
                    authz::bind_native(&dir, &id, native)?;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
    }
    if let (Some(cmux), Some(surface)) = (&host.cmux, &registration.surface) {
        let surfaces = cmux.surface_workspaces().await?;
        let Some(mut workspace) = surfaces.get(&surface.to_ascii_lowercase()).cloned() else {
            // A successful inventory proves this wrapper's old terminal is gone.
            // Detach preserves its local child; failed inventories instead retry.
            write_message(&mut socket, &ServerMessage::Detach).await?;
            return Ok(());
        };
        let occupied = host.sessions.lock().unwrap().values().any(|session| {
            session.registration.launch.id != registration.launch.id
                && session
                    .registration
                    .workspace
                    .as_deref()
                    .is_some_and(|current| current.eq_ignore_ascii_case(&workspace))
        });
        if occupied || workspace.eq_ignore_ascii_case(&cmux.host_workspace) {
            workspace = cmux
                .separate_surface(surface, &host.dir, &registration.launch)
                .await?;
        }
        if !registration.reconnecting
            && !authz::store::Store::open(&host.dir)?.is_open_for_restore(&id, &host.server)?
        {
            // A fresh wrapper can adopt an existing workspace. Restored sessions
            // and reconnecting wrappers keep their previous position.
            if let Err(error) = cmux.stack_workspace(&workspace).await {
                eprintln!("cmux new workspace placement failed: {error:#}");
            }
        }
        registration.workspace = Some(workspace);
        if registration.launch.agent == conversations_core::AgentKind::Zsh
            && !registration.reconnecting
        {
            save_checkpoint(
                &host.dir,
                &host.server,
                crate::open_sessions::OpenSession {
                    session_id: id.clone(),
                    agent: registration.launch.agent,
                    cwd: registration.launch.cwd.to_string_lossy().into_owned(),
                    resume_id: None,
                    agent_args: vec![],
                    launcher: conversations_core::AgentLauncher::default(),
                    adk_statusline: false,
                },
            )?;
        }
        if authz::store::Store::open(&host.dir)?.is_open_for_restore(&id, &host.server)?
            && authz::lookup(&host.dir, &id)?.is_some_and(|saved| {
                saved.resume_id.is_some() || saved.agent == conversations_core::AgentKind::Zsh
            })
        {
            if let Err(error) = cmux
                .bind_saved_session(&host.dir, &host.server, surface, &registration.launch.id)
                .await
            {
                eprintln!("Cannot bind restore metadata for {surface}: {error:#}");
            }
        }
    }
    let initially_hidden = if let (Some(cmux), Some(surface)) = (&host.cmux, &registration.surface)
    {
        cmux.is_background_surface(surface).await.unwrap_or(true)
    } else {
        true
    };
    let mut previous_size = None;
    let local_terminal = registration.local_terminal;
    let (commands, mut commands_rx) = mpsc::channel(64);
    // The terminal stays on this machine: no central live registration, PTY
    // mirror, heartbeat, completion, native id or file receiver. Remote readers
    // use the private channel; sharing is an explicit, separate publication.
    let screen = Screen::new(80, 24);
    let readiness = Arc::new(Mutex::new(
        conversations_core::private_channel::AgentReadiness::Unknown,
    ));
    {
        let mut sessions = host.sessions.lock().unwrap();
        // A journaled End belongs to the wrapper that was running then. Keep it
        // until that wrapper reports its exit; a fresh replacement clears it.
        if let Some(owners) = pending_end(&host.dir, &id) {
            let owed = match pid {
                Some(pid) if !owners.is_empty() => owners.contains(&pid),
                _ => registration.reconnecting,
            };
            if owed {
                crash_point("end-delivery");
                let _ = commands.try_send(ServerMessage::End);
            } else {
                clear_pending_end(&host.dir, &id);
            }
        }
        if !registration.effect_gate {
            eprintln!("Terminal {id}: {UpgradeRequired}");
        }
        sessions.insert(
            id.clone(),
            Session {
                readiness: readiness.clone(),
                local_input_bytes: 0,
                local_input_dropped_chunks: 0,
                registration: registration.clone(),
                resume_published: None,
                opencode_title_published: None,
                commands,
                pid,
                screen: Some(screen.clone()),
            },
        );
    }
    drop(registration_lock);
    if registration.reconnecting {
        // The host may have restarted: a revocation committed while no host ran
        // (or before this process knew the session) is acted on now.
        let (host, id) = (host.clone(), id.clone());
        tokio::spawn(async move {
            match host.interrupt_revoked_in(Some(&id)).await {
                Ok(interruptions) => {
                    for interruption in interruptions {
                        eprintln!(
                            "Interaction of {} on reconnect after a revocation: {}",
                            interruption.session,
                            super::protocol::CancelOutcome::describe(interruption.outcome)
                        );
                    }
                }
                Err(error) => eprintln!("Interaction of {id} on reconnect: {error:#}"),
            }
        });
    } else {
        host.clear_attribution(&id);
    }
    let mut guard = SessionGuard {
        host: host.clone(),
        id: id.clone(),
        tasks: vec![],
        screen: Some(screen.clone()),
    };
    // Durable before the waiting launch hears of it: the journal, not the wait,
    // decides a signed start's outcome (see `authz::confirm_launch`).
    {
        let (dir, id, agent, cwd) = (
            host.dir.clone(),
            id.clone(),
            registration.launch.agent,
            registration.launch.cwd.to_string_lossy().into_owned(),
        );
        match tokio::task::spawn_blocking(move || authz::confirm_launch(&dir, &id, agent, &cwd))
            .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => eprintln!("Cannot confirm the start of this terminal: {error:#}"),
            Err(error) => eprintln!("Cannot confirm the start of this terminal: {error}"),
        }
    }
    if let Some(registered) = host.pending.lock().unwrap().remove(&id) {
        let _ = registered.send(());
    }
    let (mut read, mut write) = socket.into_split();
    guard.tasks.push(tokio::spawn(async move {
        while let Some(message) = commands_rx.recv().await {
            if write_message(&mut write, &message).await.is_err() {
                return;
            }
        }
    }));
    let h = host.clone();
    let readiness_registration = registration.clone();
    guard.tasks.push(tokio::spawn(async move {
        readiness_loop(h, readiness_registration, pid, readiness).await;
    }));
    let registered_surface = registration.surface.clone();
    let transcript_registration = registration.clone();
    let h = host.clone();
    let identity_registration = transcript_registration.clone();
    guard.tasks.push(tokio::spawn(async move {
        transcript_loop(h, transcript_registration).await;
    }));
    let h = host.clone();
    guard.tasks.push(tokio::spawn(async move {
        native_identity_loop(h, identity_registration, &CODEX_IDENTITY_PROBE).await;
    }));
    eprintln!("Terminal connected to {}", host.server);
    loop {
        match read_message(&mut read).await? {
            ClientMessage::Frame { frame } => {
                let dimensions = (frame.cols, frame.rows);
                // Also checkpoint older wrappers on daemon upgrade, without restarting
                // their agents. A changed grid is a real local resize after bootstrap.
                if host.cmux.is_some()
                    && local_terminal
                    && (!initially_hidden || previous_size.is_some_and(|old| old != dimensions))
                    && previous_size != Some(dimensions)
                {
                    super::wrapper::save_size(
                        &host.dir.join("terminal-sizes").join(format!("{id}.json")),
                        frame.cols,
                        frame.rows,
                    );
                }
                previous_size = Some(dimensions);
                screen.lock().unwrap().apply(frame)?;
            }
            ClientMessage::CancelResult {
                request_id,
                outcome,
            } => {
                let entry = host
                    .interactions
                    .pending
                    .lock()
                    .unwrap()
                    .remove(&request_id);
                if let Some((source, sender)) = entry {
                    ensure!(source == id, "cancel receipt belongs to another session");
                    let _ = sender.send(outcome);
                }
            }
            ClientMessage::TransferResult { transfer, status } => {
                private_channel::transfer_receipt(&host.dir, &id, &screen, &transfer, status);
            }
            ClientMessage::PeerInputResult {
                request_id,
                written,
            } => {
                let entry = host.peer_inputs.lock().unwrap().remove(&request_id);
                if let Some((source, sender)) = entry {
                    ensure!(
                        source == id,
                        "guarded input receipt belongs to another source"
                    );
                    let _ = sender.send(written);
                }
            }
            ClientMessage::LocalInput { b64 } => {
                let bytes = STANDARD.decode(b64)?;
                ensure!(bytes.len() <= 8192, "oversized local input observation");
                if let Some(session) = host.sessions.lock().unwrap().get_mut(&id) {
                    session.local_input_bytes += bytes.len() as u64;
                }
                // Observation only: never replay this input to the child or log it.
            }
            ClientMessage::LocalInputGap { chunks } => {
                if let Some(session) = host.sessions.lock().unwrap().get_mut(&id) {
                    session.local_input_dropped_chunks =
                        session.local_input_dropped_chunks.saturating_add(chunks);
                }
            }
            ClientMessage::Exit { code, interrupted } => {
                screen.lock().unwrap().finish();
                if interrupted.unwrap_or(matches!(code, 129 | 143)) {
                    if let Some(cmux) = &host.cmux {
                        // Closing one pane and quitting cmux both signal the
                        // wrapper. Let cmux finish removing the pane, then only
                        // retain checkpoints for surviving panes or app shutdown
                        // (when the socket is no longer available).
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        let removed = match cmux.surface_workspaces().await {
                            Ok(surfaces) => registered_surface.as_ref().is_some_and(|surface| {
                                !surfaces.contains_key(&surface.to_ascii_lowercase())
                            }),
                            Err(_) => false,
                        };
                        if !removed {
                            drop(guard);
                            clear_pending_end(&host.dir, &id);
                            return Ok(());
                        }
                    }
                }
                authz::store::Store::open(&host.dir)?.close_for_restore(&id)?;
                // Keep End pending until the old registration is gone.
                drop(guard);
                clear_pending_end(&host.dir, &id);
                return Ok(());
            }
            _ => bail!("unexpected wrapper message"),
        }
    }
}

async fn title_loop(host: Arc<Host>) {
    let mut titles = HashMap::<String, LocalTitle>::new();
    let mut order_baseline = None;
    loop {
        if let Err(error) = sync_order(&host, &mut order_baseline).await {
            eprintln!("cmux order sync: {error:#}");
        }
        if let Err(error) = sync_titles(&host, &mut titles).await {
            eprintln!("cmux title sync: {error:#}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

struct OrderBaseline {
    ids: Vec<String>,
    revision: i64,
}

async fn sync_order(host: &Host, baseline: &mut Option<OrderBaseline>) -> Result<()> {
    use super::order::changed;
    let cmux = host.cmux.as_ref().context("missing cmux backend")?;
    // cmux order is owned by local gestures. The central order is only the
    // compare-and-set revision for publishing them: a web reorder is an unsigned
    // poll result and never moves a local workspace. Read the revision before
    // observing native changes so a later web gesture makes our conditional
    // write fail rather than overwrite it with a stale observation.
    let response = host
        .auth(host.client.get(format!("{}/api/live/order", host.server)))
        .timeout(Duration::from_secs(5))
        .send()
        .await?;
    let Some(order) = read_order_response(response).await? else {
        return Ok(());
    };
    let live = host.live_sessions().await?;
    let owned: std::collections::HashSet<_> = live["live_sessions"]
        .as_array()
        .context("missing live sessions")?
        .iter()
        .filter(|s| s["viewer_is_author"] == true)
        .filter_map(|s| s["id"].as_str())
        .collect();
    let actual = cmux.surface_workspaces().await?;
    let registrations: Vec<_> = host
        .sessions
        .lock()
        .unwrap()
        .values()
        .map(|s| s.registration.clone())
        .collect();
    let mut workspace_sessions = HashMap::<String, String>::new();
    for registration in &registrations {
        if !owned.contains(registration.launch.id.as_str()) {
            continue;
        }
        let Some(workspace) = registration
            .surface
            .as_ref()
            .and_then(|surface| actual.get(&surface.to_ascii_lowercase()))
        else {
            continue;
        };
        if workspace.eq_ignore_ascii_case(&cmux.host_workspace) {
            continue;
        }
        let occupants = registrations
            .iter()
            .filter(|r| {
                r.surface
                    .as_ref()
                    .and_then(|surface| actual.get(&surface.to_ascii_lowercase()))
                    .is_some_and(|w| w.eq_ignore_ascii_case(workspace))
            })
            .count();
        if occupants == 1 {
            workspace_sessions.insert(
                workspace.to_ascii_lowercase(),
                registration.launch.id.clone(),
            );
        }
    }
    let windows = cmux.workspace_order().await?;
    let observed: Vec<_> = windows
        .iter()
        .flat_map(|w| &w.ids)
        .filter_map(|id| workspace_sessions.get(id).cloned())
        .collect();
    if observed.is_empty() {
        return Ok(());
    }
    let native_changed = baseline
        .as_ref()
        .is_some_and(|previous| changed(&previous.ids, &observed));
    let stable_initial = order.revision == 0
        && baseline
            .as_ref()
            .is_some_and(|previous| previous.ids == observed);
    if native_changed || stable_initial {
        let result = write_order(host, order.revision, &observed).await;
        // A CAS conflict consumes this observation. Rebase on the winner next
        // tick; never retry this stale native snapshot against a fresh revision.
        if matches!(result, Ok(false)) {
            *baseline = Some(OrderBaseline {
                ids: observed,
                revision: order.revision,
            });
            return Ok(());
        }
        result?;
        *baseline = Some(OrderBaseline {
            ids: observed,
            revision: order.revision + 1,
        });
        return Ok(());
    }
    if baseline
        .as_ref()
        .is_some_and(|previous| previous.revision < order.revision)
    {
        eprintln!(
            "Web reorder (revision {}) is not applied to cmux; reorder workspaces locally.",
            order.revision
        );
    }
    *baseline = Some(OrderBaseline {
        ids: observed,
        revision: order.revision,
    });
    Ok(())
}

async fn read_order_response(
    response: reqwest::Response,
) -> Result<Option<super::order::LiveOrder>> {
    let status = response.status();
    if matches!(
        status,
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
    ) {
        return Ok(None);
    }
    let response = response.error_for_status()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if status == reqwest::StatusCode::OK && content_type.eq_ignore_ascii_case("text/html") {
        // Older servers route an unknown API GET to the SPA fallback.
        return Ok(None);
    }
    ensure!(
        content_type.eq_ignore_ascii_case("application/json"),
        "live order returned unexpected Content-Type {content_type:?}"
    );
    Ok(Some(response.json().await?))
}

async fn write_order(host: &Host, revision: i64, ids: &[String]) -> Result<bool> {
    let response = host
        .auth(host.client.put(format!("{}/api/live/order", host.server)))
        .json(&json!({"revision": revision, "session_ids": ids}))
        .timeout(Duration::from_secs(5))
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::CONFLICT {
        return Ok(false);
    }
    response.error_for_status()?;
    Ok(true)
}

/// Whether the title sync has already reconciled the private-channel title
/// for this session during this process lifetime. On first sight (including
/// the first tick after a daemon restart) the reconciliation must run even
/// when the current sidebar name is `None`, so a stale `host_titles` row
/// written by an earlier daemon run does not survive a workspace clear made
/// while the daemon was offline.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
enum ReconciledPrivateTitle {
    #[default]
    Uninitialized,
    Applied(Option<String>),
}

/// What the title sync last saw in, or wrote to, one session's workspace.
#[derive(Debug, Default)]
struct LocalTitle {
    /// The workspace name last observed or applied here.
    applied: Option<String>,
    /// The user renamed the workspace locally; automatic titles stop.
    manual: bool,
    /// What the title sync last persisted into `history.host_titles` for
    /// this session — or `Uninitialized` before the first reconciliation, so
    /// a first-sight tick clears a stale persisted row even when the current
    /// sidebar name is `None`.
    history_applied: ReconciledPrivateTitle,
}

/// Reconcile the local `history.host_titles` row for `id` with the cmux sidebar
/// name the daemon currently believes to be effective. `effective` names the
/// sidebar title the user (or an automatic namer) wants on display; `None`
/// means nothing is registered (the first-user-message fallback resumes).
///
/// Errors are logged, never bubbled up: a failing SQLite write must not break
/// the title loop. The reconciliation is why the private channel's
/// `SessionsList` can surface a running cmux rename without any central push.
fn reconcile_private_title(host: &Host, id: &str, effective: Option<&str>, state: &mut LocalTitle) {
    let next = ReconciledPrivateTitle::Applied(effective.map(str::to_owned));
    if next == state.history_applied {
        return;
    }
    match super::history::History::open(&host.dir) {
        Ok(history) => {
            let result = match effective {
                Some(title) => history.set_host_title(id, title),
                None => history.clear_host_title(id),
            };
            if let Err(error) = result {
                eprintln!("private title sync for {id}: {error:#}");
                return;
            }
            state.history_applied = next;
        }
        Err(error) => eprintln!("open history for private title sync: {error:#}"),
    }
}

/// Keep cmux workspace names local. Central titles and title overrides are
/// unsigned poll results and never rename a workspace. A local rename is
/// kept local; a locally known automatic title (the OpenCode
/// bridge report or the Codex rollout subject) names the workspace until the
/// user renames it. The host and shared workspaces are never renamed. Every
/// effective cmux name for a registered conversation is mirrored into
/// `history.host_titles` so the private channel's `SessionsList` (read by
/// the direct-localhost `/private` page AND the hosted private page) surfaces
/// the running sidebar name without a central-DB upload.
async fn sync_titles(host: &Host, titles: &mut HashMap<String, LocalTitle>) -> Result<()> {
    let _title_guard = host.title_lock.lock().await;
    let cmux = host.cmux.as_ref().context("missing cmux backend")?;
    let mut sessions: Vec<_> = host
        .sessions
        .lock()
        .unwrap()
        .values()
        .map(|session| session.registration.clone())
        .collect();
    titles.retain(|id, _| sessions.iter().any(|s| &s.launch.id == id));
    if sessions.is_empty() {
        return Ok(());
    }
    for session in &sessions {
        if let Err(error) = sync_resume_identity(host, cmux, session).await {
            eprintln!("cmux resume identity: {error:#}");
        }
    }
    // Read bridge reports before local naming so a fresh OpenCode title can
    // name its workspace on this tick.
    for session in &sessions {
        if session.launch.agent == conversations_core::AgentKind::OpenCode {
            if let Err(error) = sync_opencode_title(host, session).await {
                eprintln!("OpenCode title sync: {error:#}");
            }
        }
    }
    let actual = cmux.surface_workspaces().await?;
    for session in &mut sessions {
        if let Some(surface) = &session.surface {
            let workspace = actual.get(&surface.to_ascii_lowercase()).cloned();
            if session.workspace != workspace {
                titles.remove(&session.launch.id);
                session.workspace = workspace.clone();
                if let Some(connected) = host.sessions.lock().unwrap().get_mut(&session.launch.id) {
                    connected.registration.workspace = workspace;
                }
            }
        }
    }
    let workspace_titles = cmux.workspace_titles().await?;
    for session in &sessions {
        let Some(workspace) = session.workspace.as_deref() else {
            continue;
        };
        // A shared workspace cannot represent two conversation names. In
        // particular, never rename the dedicated host workspace.
        if workspace.eq_ignore_ascii_case(&cmux.host_workspace)
            || sessions
                .iter()
                .filter(|s| {
                    s.workspace
                        .as_deref()
                        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(workspace))
                })
                .count()
                != 1
        {
            continue;
        }
        let id = &session.launch.id;
        let local = workspace_titles
            .get(workspace)
            .map(String::as_str)
            .filter(|title| !title.trim().is_empty());
        let placeholder = session.launch.title();
        // A persisted explicit rename wins over automatic titles after a host
        // restart or surface move too. A later observed sidebar gesture still
        // wins when it differs from the title this loop last applied.
        let history = super::history::History::open(&host.dir)?;
        let web = history.local_title_with_updated_at(id)?;
        let observed = history.host_title_with_updated_at(id)?;
        let explicit = web.clone().filter(|(title, at)| {
            observed
                .as_ref()
                .is_none_or(|(host_title, host_at)| title == host_title || at >= host_at)
        });
        let unchanged = titles
            .get(id)
            .is_none_or(|state| local == state.applied.as_deref());
        if let Some((title, _)) =
            explicit.filter(|(title, _)| unchanged || local == Some(title.as_str()))
        {
            if local != Some(title.as_str()) {
                if let Err(error) = cmux.rename(workspace, &title).await {
                    eprintln!("cmux explicit title sync: {error:#}");
                    continue;
                }
            }
            let state = titles.entry(id.clone()).or_default();
            state.applied = Some(title.clone());
            state.manual = true;
            reconcile_private_title(host, id, Some(&title), state);
            continue;
        }
        let Some(state) = titles.get_mut(id) else {
            // First sight (including after a host restart): adopt the current
            // name without republishing it as a manual title. The sidebar
            // name still becomes the private-channel title so a rename made
            // while the daemon was offline reaches `/private/<id>` on the
            // next tick.
            let newer_sidebar = web.as_ref().zip(observed.as_ref()).is_some_and(
                |((web_title, web_at), (host_title, host_at))| {
                    host_at > web_at
                        && host_title != web_title
                        && local == Some(host_title.as_str())
                },
            );
            let mut inserted = LocalTitle {
                applied: local.map(str::to_owned),
                manual: newer_sidebar,
                history_applied: ReconciledPrivateTitle::Uninitialized,
            };
            reconcile_private_title(host, id, local, &mut inserted);
            titles.insert(id.clone(), inserted);
            continue;
        };
        let automatic = match session.launch.agent {
            conversations_core::AgentKind::OpenCode => host
                .sessions
                .lock()
                .unwrap()
                .get(id)
                .and_then(|session| session.opencode_title_published.clone()),
            conversations_core::AgentKind::Codex
                if !state.manual && super::subject::may_detect(None, local, &placeholder) =>
            {
                codex_subject(host, id).await?
            }
            _ => None,
        };
        if let Some(local) = local.filter(|local| {
            *local != placeholder
                && Some(*local) != state.applied.as_deref()
                && Some(*local) != automatic.as_deref()
        }) {
            state.applied = Some(local.to_owned());
            state.manual = true;
            reconcile_private_title(host, id, Some(local), state);
            continue;
        }
        if let Some(automatic) = automatic.filter(|_| !state.manual) {
            if local != Some(automatic.as_str()) {
                if let Err(error) = cmux.rename(workspace, &automatic).await {
                    eprintln!("cmux title sync: {error:#}");
                    continue;
                }
            }
            reconcile_private_title(host, id, Some(automatic.as_str()), state);
            state.applied = Some(automatic);
        } else {
            // Nothing automatic to apply this tick; keep the private-channel
            // title aligned with the sidebar (clear it when the user removed
            // the custom title and no automatic takes over).
            reconcile_private_title(host, id, local, state);
        }
    }
    Ok(())
}

/// Commit an owner rename: write the DB override (authoritative), then
/// best-effort mirror the new title into the session's current cmux workspace.
///
/// Shared by the signed `session.rename` path and the owner-cookie web rename
/// path so both apply the SAME effective title under the same invariants. The
/// signed path passes an `Operation` whose `begin` side effect records cutover
/// state; the owner-cookie path passes `None` because its trust comes from the
/// owner-only Unix socket. Returns the effective title `History::summary()`
/// picks after the rename.
async fn commit_owner_rename(
    host: &Host,
    session: &str,
    title: &str,
    signed_op: Option<&authz::envelope::Verified>,
) -> Result<String> {
    let _title_guard = host.title_lock.lock().await;
    let (dir, session_arg, title_arg) = (host.dir.clone(), session.to_owned(), title.to_owned());
    let signed = signed_op.cloned();
    let effective = tokio::task::spawn_blocking(move || -> Result<String> {
        let history = super::history::History::open(&dir)?;
        if let Some(op) = signed.as_ref() {
            authz::dispatch::begin(&dir, op)?;
        }
        history.rename(&session_arg, &title_arg)
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result)?;
    push_rename_to_cmux_workspace(host, session, title).await;
    Ok(effective)
}

/// Mirror an explicit rename into the session's current cmux workspace.
///
/// Must match `sync_titles` on two invariants: resolve the target workspace
/// through `cmux.surface_workspaces()` on this call (the cached
/// `registration.workspace` only catches up on the next title tick and may be
/// stale after a surface move), and never touch a workspace the host already
/// shares with another registered conversation or the dedicated host
/// workspace — a shared sidebar cannot represent two conversation titles.
/// Best-effort: missing cmux, missing surface, cleared title, or an RPC
/// failure are silent. `history.rename` is the authoritative commit.
async fn push_rename_to_cmux_workspace(host: &Host, session: &str, title: &str) {
    let Some(cmux) = host.cmux.as_ref() else {
        return;
    };
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return;
    }
    let (target_surface, registrations) = {
        let sessions = host.sessions.lock().unwrap();
        let target = sessions
            .get(session)
            .and_then(|s| s.registration.surface.clone())
            .filter(|surface| !surface.trim().is_empty());
        let all: Vec<_> = sessions.values().map(|s| s.registration.clone()).collect();
        (target, all)
    };
    let Some(target_surface) = target_surface else {
        return;
    };
    let surfaces = match cmux.surface_workspaces().await {
        Ok(map) => map,
        Err(error) => {
            eprintln!("cmux surface lookup for rename of {session}: {error:#}");
            return;
        }
    };
    let Some(workspace) = surfaces.get(&target_surface.to_ascii_lowercase()).cloned() else {
        return;
    };
    if workspace.eq_ignore_ascii_case(&cmux.host_workspace) {
        return;
    }
    let occupants = registrations
        .iter()
        .filter_map(|registration| {
            match registration
                .surface
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                Some(surface) => surfaces.get(&surface.to_ascii_lowercase()),
                None => registration.workspace.as_ref(),
            }
        })
        .filter(|candidate| candidate.eq_ignore_ascii_case(&workspace))
        .count();
    if occupants != 1 {
        return;
    }
    if let Err(error) = cmux.rename(&workspace, trimmed).await {
        eprintln!("cmux title sync (web rename for {session}): {error:#}");
    }
}

/// The first user prompt of this Codex session's own rollout, if any yet.
async fn codex_subject(host: &Host, id: &str) -> Result<Option<String>> {
    let native = authz::lookup(&host.dir, id)?.and_then(|saved| saved.resume_id);
    let Some((root, native)) = conversations_core::codex_sessions_dir().zip(native) else {
        return Ok(None);
    };
    Ok(tokio::task::spawn_blocking(move || super::subject::codex_subject(&root, &native)).await?)
}

fn save_checkpoint(
    dir: &Path,
    server: &str,
    saved: crate::open_sessions::OpenSession,
) -> Result<()> {
    authz::store::Store::open(dir)?.save_restore(&saved, server)
}

fn checkpoint_for_launch(launch: &Launch) -> Option<crate::open_sessions::OpenSession> {
    use conversations_core::AgentKind;

    let mut resume_id = (launch.agent == AgentKind::Claude).then(|| launch.id.clone());
    let mut agent_args = vec![];
    if launch.agent == AgentKind::Zsh {
        return None;
    }
    if launch.command != [launch.agent.as_str()] {
        // Recognize direct Codex resume argv, including an absolute executable.
        // Shell programs and ambiguous option forms remain opaque.
        let (program, args) = launch.command.split_first()?;
        if launch.agent != AgentKind::Codex || Path::new(program).file_name()?.to_str()? != "codex"
        {
            return None;
        }
        let args = args
            .strip_prefix(&["--no-alt-screen".to_owned()])
            .unwrap_or(args);
        let [verb, native, rest @ ..] = args else {
            return None;
        };
        if verb != "resume" || uuid::Uuid::parse_str(native).is_err() {
            return None;
        }
        resume_id = Some(native.clone());
        agent_args = rest.to_vec();
    }
    Some(crate::open_sessions::OpenSession {
        session_id: launch.id.clone(),
        agent: launch.agent,
        cwd: launch.cwd.to_string_lossy().into_owned(),
        resume_id,
        agent_args,
        launcher: conversations_core::AgentLauncher::default(),
        adk_statusline: false,
    })
}

async fn readiness_loop(
    host: Arc<Host>,
    registration: Registration,
    pid: Option<u32>,
    evidence: Arc<Mutex<conversations_core::private_channel::AgentReadiness>>,
) {
    use conversations_core::{private_channel::AgentReadiness, AgentKind};
    loop {
        let observed = async {
            match registration.launch.agent {
                AgentKind::Claude => {
                    let pid = pid.context("wrapper identity unavailable")?;
                    super::readiness::claude(&host.dir, &registration.launch.id, pid)
                }
                AgentKind::OpenCode => {
                    super::opencode_bridge::readiness(&host.dir, &registration.launch.id, pid)
                }
                AgentKind::Codex => {
                    let native = authz::lookup(&host.dir, &registration.launch.id)?
                        .and_then(|saved| saved.resume_id)
                        .context("native identity unavailable")?;
                    super::codex::readiness(&registration.launch.id, &native).await
                }
                AgentKind::Zsh => Ok(AgentReadiness::Unknown),
            }
        };
        let next = tokio::time::timeout(Duration::from_secs(3), observed)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        *evidence.lock().unwrap() = next;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn transcript_loop(host: Arc<Host>, registration: Registration) {
    if registration.launch.agent == conversations_core::AgentKind::Zsh {
        return;
    }
    let mut transcript = None;
    loop {
        if transcript.is_none() {
            // The title loop captures a late native binding for this exact surface.
            // A neighboring pane's cwd, transcript mtime or inherited workspace ID
            // is never evidence that its transcript belongs to this registration.
            // The canonical local record is the source of the native id, not the
            // cmux-only checkpoint collection.
            let (dir, id) = (host.dir.clone(), registration.launch.id.clone());
            let native = tokio::task::spawn_blocking(move || authz::lookup(&dir, &id))
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .filter(|saved| saved.agent == registration.launch.agent)
                .and_then(|saved| saved.resume_id);
            if let Some(native) = native {
                match crate::commands::live::interactive::BoundTranscript::start(
                    registration.launch.id.clone(),
                    registration.launch.agent,
                    native,
                    registration.launch.cwd.clone(),
                    host.dir.clone(),
                ) {
                    Ok(worker) => transcript = Some(worker),
                    Err(error) => eprintln!("Transcript binding: {error:#}"),
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Ask the agent-specific local probe which native session this terminal runs:
/// the OpenCode bridge's report for this exact launch, or the root thread loaded
/// on this launch's private Codex app-server. `None` means not known yet. Nothing
/// here looks at transcripts, working directories or modification times.
async fn probe_native_identity(
    dir: &Path,
    agent: conversations_core::AgentKind,
    id: &str,
    codex: &CodexIdentityProbe,
) -> Result<Option<String>> {
    use conversations_core::AgentKind;
    match agent {
        AgentKind::OpenCode => {
            Ok(super::opencode_bridge::report(dir, id, None)?.map(|report| report.native_id))
        }
        AgentKind::Codex => {
            let endpoint = codex.endpoint(id)?;
            super::codex::private_root_thread(&endpoint, codex.deadline).await
        }
        _ => Ok(None),
    }
}

/// Persist the native identity of a Codex or OpenCode terminal locally, as soon
/// as the agent's own seam reports it, on every backend (not only under cmux).
/// The identity is never published to the central server.
async fn native_identity_loop(
    host: Arc<Host>,
    registration: Registration,
    codex: &'static CodexIdentityProbe,
) {
    use conversations_core::AgentKind;
    let (agent, id) = (registration.launch.agent, registration.launch.id.clone());
    if !matches!(agent, AgentKind::Codex | AgentKind::OpenCode) {
        return;
    }
    let native = loop {
        let dir = host.dir.clone();
        let lookup_id = id.clone();
        let known = tokio::task::spawn_blocking(move || authz::lookup(&dir, &lookup_id))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
        if let Some(native) = known.as_ref().and_then(|saved| saved.resume_id.clone()) {
            break native;
        }
        match probe_native_identity(&host.dir, agent, &id, codex).await {
            Ok(Some(native)) => {
                let (dir, bind_id, bound) = (host.dir.clone(), id.clone(), native.clone());
                match tokio::task::spawn_blocking(move || {
                    authz::bind_native(&dir, &bind_id, &bound)
                })
                .await
                {
                    Ok(Ok(_)) => break native,
                    Ok(Err(error)) => eprintln!("Cannot bind native session identity: {error:#}"),
                    Err(error) => eprintln!("Cannot bind native session identity: {error}"),
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!("Native identity probe: {error:#}"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    // The native identity stays local: it is resume state, never published.
    let _ = native;
}

/// Where and how long to look for a fresh Codex launch's native thread.
struct CodexIdentityProbe {
    endpoint: fn(&str) -> Result<PathBuf>,
    deadline: Duration,
}

const CODEX_IDENTITY_PROBE: CodexIdentityProbe = CodexIdentityProbe {
    endpoint: super::codex::private_endpoint,
    deadline: Duration::from_secs(5),
};

impl CodexIdentityProbe {
    fn endpoint(&self, session: &str) -> Result<PathBuf> {
        (self.endpoint)(session)
    }
}

async fn sync_resume_identity(host: &Host, cmux: &Cmux, registration: &Registration) -> Result<()> {
    sync_resume_identity_with(host, cmux, registration, &CODEX_IDENTITY_PROBE).await
}

async fn sync_resume_identity_with(
    host: &Host,
    cmux: &Cmux,
    registration: &Registration,
    codex: &CodexIdentityProbe,
) -> Result<()> {
    if host
        .sessions
        .lock()
        .unwrap()
        .get(&registration.launch.id)
        .is_some_and(|session| session.resume_published.is_some())
    {
        return Ok(());
    }
    let Some(surface) = registration.surface.as_deref() else {
        return Ok(());
    };
    if !authz::store::Store::open(&host.dir)?
        .is_open_for_restore(&registration.launch.id, &host.server)?
    {
        return Ok(());
    }
    let Some(mut saved) = authz::lookup(&host.dir, &registration.launch.id)? else {
        return Ok(());
    };
    if saved.agent == conversations_core::AgentKind::Zsh {
        cmux.bind_saved_session(&host.dir, &host.server, surface, &saved.session_id)
            .await?;
        if let Some(session) = host.sessions.lock().unwrap().get_mut(&saved.session_id) {
            session.resume_published = Some(saved.session_id);
        }
        return Ok(());
    }
    if saved.resume_id.is_none() {
        // A fresh OpenCode thread reports through its bridge and a fresh Codex
        // thread through the private app-server; neither gets a cmux binding. A
        // failed probe falls back to the binding; it never guesses a root.
        let probed =
            match probe_native_identity(&host.dir, saved.agent, &saved.session_id, codex).await {
                Ok(native) => native,
                Err(error) => {
                    eprintln!("Native identity probe failed; trying cmux binding: {error:#}");
                    None
                }
            };
        let native = if let Some(native) = probed {
            native
        } else {
            let Some(binding) = cmux.resume_binding(surface).await? else {
                return Ok(());
            };
            if binding["kind"].as_str() != Some(saved.agent.as_str()) {
                return Ok(());
            }
            let Some(native) = binding["checkpoint_id"]
                .as_str()
                .filter(|id| !id.trim().is_empty())
            else {
                return Ok(());
            };
            native.to_owned()
        };
        authz::bind_native(&host.dir, &saved.session_id, &native)?;
        saved.resume_id = Some(native);
    }
    if let Some(native) = &saved.resume_id {
        cmux.bind_saved_session(&host.dir, &host.server, surface, &saved.session_id)
            .await?;
        if let Some(session) = host.sessions.lock().unwrap().get_mut(&saved.session_id) {
            session.resume_published = Some(native.clone());
        }
    }
    Ok(())
}

async fn sync_opencode_title(host: &Host, registration: &Registration) -> Result<()> {
    let id = &registration.launch.id;
    let known = host
        .sessions
        .lock()
        .unwrap()
        .get(id)
        .and_then(|session| session.resume_published.clone());
    let Some(known) = known else {
        return Ok(());
    };
    let Some(report) = super::opencode_bridge::report(&host.dir, id, Some(&known))? else {
        return Ok(());
    };
    let Some(title) = report.title.filter(|title| !title.trim().is_empty()) else {
        return Ok(());
    };
    if host
        .sessions
        .lock()
        .unwrap()
        .get(id)
        .is_some_and(|session| session.opencode_title_published.as_deref() == Some(&title))
    {
        return Ok(());
    }
    // Bridge titles are local metadata; private sessions have no central live row.
    if let Some(session) = host.sessions.lock().unwrap().get_mut(id) {
        session.opencode_title_published = Some(title);
    }
    Ok(())
}

/// Verify, authorize and execute one signed operation. The effect adapter
/// (`start_signed` -> `Host::launch`) is reachable only through `Decision::Execute`.
/// A signed share, unshare or status request for ONE session. The owner and
/// destination are this host's local configuration (never the frame or the
/// login); the publisher sends the content afterwards. Nothing here touches the
/// session's terminal, agent or local history.
async fn publication_signed(
    host: &Host,
    op: &authz::envelope::Verified,
    session: &str,
    audience: Option<conversations_core::publication::Audience>,
    change: bool,
) -> Result<String> {
    use super::publication;
    let (dir, session, server) = (host.dir.clone(), session.to_owned(), host.server.clone());
    let op = op.clone();
    tokio::task::spawn_blocking(move || {
        authz::dispatch::begin(&dir, &op)?;
        let status = match (change, audience) {
            (true, Some(audience)) => {
                let settings = super::settings::Settings::load(&dir)?;
                let owner = publication::configured_owner(settings.publication.account.as_deref())?;
                publication::share(&dir, &session, audience, owner.as_deref(), &server)?
            }
            (true, None) => publication::unshare(&dir, &session)?,
            (false, _) => publication::status(&dir, &session)?,
        };
        if !change {
            // A phase alone cannot tell the phone which audience is actually
            // selected. Report the local canonical publication state; never
            // infer an owner-only audience from a missing central projection.
            return Ok(serde_json::to_string(&serde_json::json!({
                "state": status.state,
                "rev": status.rev,
                "audience": status.audience,
                "published": status.published,
                "pending": status.pending,
            }))?);
        }
        Ok(status.state.as_str().to_owned())
    })
    .await?
}

/// The outcome to report for an operation that was already admitted.
pub(super) fn replay_outcome(
    state: &str,
) -> (conversations_core::host_relay::OpStatus, &'static str) {
    use conversations_core::host_relay::OpStatus;
    match state {
        "done" => (OpStatus::Replayed, "already done; not repeated"),
        "failed" => (OpStatus::Failed, "failed earlier; not repeated"),
        // Admitted, dispatching or uncertain: the effect may or may not have happened.
        _ => (
            OpStatus::Uncertain,
            "an earlier attempt did not finish; not repeated",
        ),
    }
}

/// Verify, admit and execute one signed operation, and say what happened in
/// terms that are safe to send over the relay: a status and, for refusals, the
/// verifier's generic reason. Host-side failure details (paths, native ids)
/// stay in the host's log.
pub(super) async fn dispatch_signed(
    host: &Host,
    frame: SignedFrame,
) -> (conversations_core::host_relay::OpStatus, Option<String>) {
    use conversations_core::host_relay::OpStatus;
    // Private content sent TO the host crosses only the private channel.
    if authz::envelope::claimed_operation(&frame.payload)
        .is_some_and(|op| authz::envelope::channel_only(&op))
    {
        return (
            OpStatus::Refused,
            Some("this operation is accepted only on a private connection".into()),
        );
    }
    execute_signed(host, frame).await
}

/// [`dispatch_signed`] for an operation that arrived on the private channel.
pub(super) async fn execute_signed(
    host: &Host,
    frame: SignedFrame,
) -> (conversations_core::host_relay::OpStatus, Option<String>) {
    use conversations_core::host_relay::OpStatus;
    // Reads of local content are answered by the channel's read path, never here.
    if authz::envelope::claimed_operation(&frame.payload)
        .is_some_and(|op| authz::envelope::private_only(&op))
    {
        return (
            OpStatus::Refused,
            Some("this operation is answered only on a private connection".into()),
        );
    }
    let now = authz::store::now_secs();
    let dir = host.dir.clone();
    let decided = {
        let frame = frame.clone();
        tokio::task::spawn_blocking(move || authz::dispatch::decide(&dir, &frame, now)).await
    };
    let op = match decided {
        Ok(Ok(Decision::Execute(op))) => op,
        Ok(Ok(Decision::Replay { state, .. })) => {
            // Keep what actually happened the first time: a replay of a failed or
            // interrupted operation is never reported as a success.
            let (status, detail) = replay_outcome(&state);
            return (status, Some(detail.into()));
        }
        Ok(Ok(Decision::Applied(detail))) => {
            if detail == "removed" {
                // The removal is committed whatever happens next; the caller is
                // told what happened to the work it affected.
                return (
                    OpStatus::Executed,
                    Some(removal_report(host.interrupt_revoked().await)),
                );
            }
            return (OpStatus::Executed, Some(detail.into()));
        }
        Ok(Ok(Decision::Stream(op))) => return stream_signed(host, &op).await,
        Ok(Ok(Decision::Refused(reason))) => return (OpStatus::Refused, Some(reason)),
        Ok(Err(error)) => {
            eprintln!("Signed operation not admitted: {error:#}");
            return (
                OpStatus::Failed,
                Some("the host could not check the operation".into()),
            );
        }
        Err(error) => {
            eprintln!("Signed operation check aborted: {error}");
            return (
                OpStatus::Failed,
                Some("the host could not check the operation".into()),
            );
        }
    };
    let mut status_detail: Option<String> = None;
    let outcome = match &op.op {
        authz::envelope::Operation::MigrationImport { input } => {
            let (dir, op, input) = (host.dir.clone(), op.clone(), input.clone());
            tokio::task::spawn_blocking(move || {
                authz::dispatch::begin(&dir, &op)?;
                super::migration::import(&dir, &op.kid, &input).map(|done| {
                    if done {
                        "copied".to_owned()
                    } else {
                        "receiving".to_owned()
                    }
                })
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .map(|detail| status_detail = Some(detail))
        }
        authz::envelope::Operation::AgentStatus { session, message } => {
            agent_status_signed(host, &op, session, message)
                .await
                .map(|state| status_detail = Some(state))
        }
        authz::envelope::Operation::SessionStart { .. } => start_signed(host, &op).await,
        authz::envelope::Operation::SessionReopen { .. } => reopen_signed(host, &op).await,
        authz::envelope::Operation::LinearUnlink { .. } => {
            let (dir, op) = (host.dir.clone(), op.clone());
            tokio::task::spawn_blocking(move || super::linear_metadata::unlink_signed(&dir, &op))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
        }
        authz::envelope::Operation::ProjectCreate { .. }
        | authz::envelope::Operation::SessionProjectSet { .. } => {
            let (dir, op) = (host.dir.clone(), op.clone());
            tokio::task::spawn_blocking(move || super::projects::execute(&dir, &op))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
                .map(|result| status_detail = Some(result))
        }
        authz::envelope::Operation::SessionRename { session, title } => {
            let result = commit_owner_rename(host, session, title, Some(&op)).await;
            if let Ok(effective) = &result {
                status_detail = Some(effective.clone());
            }
            result.map(|_| ())
        }
        authz::envelope::Operation::SessionReply { session, text } => {
            reply_signed(host, &op, session, text).await
        }
        authz::envelope::Operation::AgentMessage {
            source,
            source_host,
            text,
            reply_to,
            ..
        } => agent_message_signed(host, &op, source, source_host, text, reply_to.as_deref()).await,
        authz::envelope::Operation::SessionEnd { session } => end_signed(host, &op, session).await,
        authz::envelope::Operation::SessionPasteImage { .. }
        | authz::envelope::Operation::SessionSendFiles { .. } => transfer_signed(host, &op).await,
        authz::envelope::Operation::PrUnlink {
            session,
            owner,
            repo,
            number,
        } => {
            let (dir, op, session, owner, repo, number) = (
                host.dir.clone(),
                op.clone(),
                session.clone(),
                owner.clone(),
                repo.clone(),
                *number,
            );
            tokio::task::spawn_blocking(move || {
                authz::dispatch::begin(&dir, &op)?;
                super::pr_metadata::unlink(&dir, &session, &owner, &repo, number)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
        }
        authz::envelope::Operation::PrSync {
            session,
            owner,
            repo,
            number,
        } => pr_sync_signed(host, &op, session, owner, repo, *number).await,
        authz::envelope::Operation::PrComment {
            session,
            owner,
            repo,
            number,
            body,
            in_reply_to,
        } => pr_comment_signed(host, &op, session, owner, repo, *number, body, *in_reply_to)
            .await
            .map(|detail| status_detail = detail.map(str::to_owned)),
        authz::envelope::Operation::PublicationShare { session, audience } => {
            publication_signed(host, &op, session, Some(audience.clone()), true)
                .await
                .map(|state| status_detail = Some(state))
        }
        authz::envelope::Operation::PublicationUnshare { session } => {
            publication_signed(host, &op, session, None, true)
                .await
                .map(|state| status_detail = Some(state))
        }
        authz::envelope::Operation::PublicationStatus { session } => {
            publication_signed(host, &op, session, None, false)
                .await
                .map(|state| status_detail = Some(state))
        }
        authz::envelope::Operation::SettingsUpdate { launch } => {
            settings_update_signed(host, &op, launch).await
        }
        authz::envelope::Operation::PasskeyEnrollStart {
            label,
            kind,
            grants,
        } => passkey_enroll_start_signed(host, &op, label, kind, grants)
            .await
            .map(|link| status_detail = Some(link)),
        authz::envelope::Operation::PasskeyPending => passkey_pending_signed(host, &op)
            .await
            .map(|pending| status_detail = Some(pending)),
        authz::envelope::Operation::PasskeyApprove { cred_id } => {
            passkey_approve_signed(host, &op, cred_id)
                .await
                .map(|approved| status_detail = Some(approved))
        }
        authz::envelope::Operation::PasskeyVouch {
            for_host,
            challenge,
            cred_id,
        } => passkey_vouch_signed(host, &op, for_host, challenge, cred_id)
            .await
            .map(|vouch| status_detail = Some(vouch)),
        // Applied inside admission, handled as input-class frames above, or
        // refused before admission (private reads).
        authz::envelope::Operation::GrantAdd { .. }
        | authz::envelope::Operation::GrantRemove { .. }
        | authz::envelope::Operation::CommunicationsList { .. }
        | authz::envelope::Operation::AttentionRequest { .. }
        | authz::envelope::Operation::AttentionRecipients { .. }
        | authz::envelope::Operation::NotificationsList { .. }
        | authz::envelope::Operation::NotificationMark { .. }
        | authz::envelope::Operation::PrCacheList { .. }
        | authz::envelope::Operation::MigrationExport { .. }
        | authz::envelope::Operation::HistoryGet { .. }
        | authz::envelope::Operation::HistoryFragment { .. }
        | authz::envelope::Operation::TerminalSubscribe { .. }
        | authz::envelope::Operation::HistorySearch { .. }
        | authz::envelope::Operation::SessionsList { .. }
        | authz::envelope::Operation::SettingsGet
        | authz::envelope::Operation::ProjectsList
        | authz::envelope::Operation::AccessList
        | authz::envelope::Operation::LinearList { .. }
        | authz::envelope::Operation::SessionProjectGet { .. }
        | authz::envelope::Operation::SessionInput { .. }
        | authz::envelope::Operation::SessionResize { .. }
        | authz::envelope::Operation::SessionRepaint { .. } => {
            Err(anyhow::anyhow!("operation reached the wrong adapter"))
        }
    };
    let dir = host.dir.clone();
    let recorded = {
        let (op, outcome_text) = (
            op.clone(),
            outcome
                .as_ref()
                .map(|_| ())
                .map_err(authz::dispatch::outcome_error),
        );
        tokio::task::spawn_blocking(move || authz::dispatch::finish(&dir, &op, &outcome_text)).await
    };
    if !matches!(recorded, Ok(Ok(()))) {
        eprintln!("Cannot record signed operation outcome: {recorded:?}");
    }
    match outcome {
        Ok(()) => (OpStatus::Executed, status_detail),
        Err(error)
            if authz::dispatch::is_revoked(&error)
                || error.downcast_ref::<UpgradeRequired>().is_some() =>
        {
            (OpStatus::Refused, Some(error.to_string()))
        }
        Err(error) if error.downcast_ref::<PrCacheUnavailable>().is_some() => {
            let PrCacheUnavailable(detail) = error.downcast_ref().copied().unwrap();
            eprintln!("Signed PR sync: {detail}");
            (OpStatus::Failed, Some(detail.into()))
        }
        Err(error) if authz::dispatch::is_uncertain(&error) => {
            eprintln!("Signed operation outcome unknown: {error:#}");
            (
                OpStatus::Uncertain,
                Some("the operation may have taken effect; the host will not repeat it".into()),
            )
        }
        Err(error) => {
            eprintln!("Signed operation failed: {error:#}");
            (
                OpStatus::Failed,
                Some("the host accepted the operation but could not complete it".into()),
            )
        }
    }
}

/// What a signed `grant.remove` caller is told: the authority is removed, and
/// per affected session (session id and the fixed outcome wording only) what
/// its interruption did.
pub(super) fn removal_report(interruptions: Result<Vec<super::protocol::Interruption>>) -> String {
    match interruptions {
        Ok(interruptions) if interruptions.is_empty() => {
            "removed; no running session had input from it".into()
        }
        Ok(interruptions) => {
            let mut report = String::from("removed");
            for interruption in interruptions {
                report.push_str(&format!(
                    "; {}: {}",
                    interruption.session,
                    super::protocol::CancelOutcome::describe(interruption.outcome)
                ));
            }
            report
        }
        Err(error) => {
            eprintln!("Interruption after grant removal: {error:#}");
            "removed; interruption unavailable: affected work could not be evaluated".into()
        }
    }
}

/// Stream-class frames: authorized against current rows and consumed durably
/// (at most once) by `decide`. Here they are ordered per channel and handed to
/// the terminal's queue. The input carries its signed authority, and the
/// wrapper re-checks it under its writer lock right before the PTY write.
async fn stream_signed(
    host: &Host,
    op: &authz::envelope::Verified,
) -> (conversations_core::host_relay::OpStatus, Option<String>) {
    use authz::envelope::Operation;
    use conversations_core::host_relay::OpStatus;
    let message = match &op.op {
        Operation::SessionInput { data, .. } => ServerMessage::Input {
            b64: STANDARD.encode(data),
            authority: Some(authz::cutover::Authority::of(op)),
        },
        Operation::SessionResize { cols, rows, .. } => ServerMessage::Resize {
            cols: *cols,
            rows: *rows,
        },
        Operation::SessionRepaint { .. } => ServerMessage::Repaint,
        _ => return (OpStatus::Failed, Some("not an input frame".into())),
    };
    if let Err(error) = host.effect_gated(op.op.session_id()) {
        return (OpStatus::Refused, Some(error.to_string()));
    }
    // Input is attributed before it is handed to the terminal.
    let deliver = || {
        if matches!(op.op, Operation::SessionInput { .. }) {
            host.attribute(op.op.session_id(), &authz::cutover::Authority::of(op))?;
        }
        host.deliver_effect(op.op.session_id(), message)
    };
    let delivered = match op.op.stream_order() {
        Some((domain, ch, seq)) => host.stream.ordered(op, domain, ch, seq, deliver),
        None => Some(deliver()),
    };
    match delivered {
        None => (
            OpStatus::Refused,
            Some("stale frame on this channel".into()),
        ),
        Some(Ok(())) => (OpStatus::Executed, None),
        Some(Err(error)) => (OpStatus::Failed, Some(error.to_string())),
    }
}

/// A signed clipboard-image or file-drop authorization. Nothing is applied here:
/// the bytes travel on the central stream and THIS terminal's wrapper applies them
/// only if they match what was signed (once, before it expires). Only a wrapper
/// that enforces that gate is addressed; the wrapper re-checks the signed
/// authority again immediately before it writes the staged paste.
async fn transfer_signed(host: &Host, op: &authz::envelope::Verified) -> Result<()> {
    use super::transfers::{deadline_in, TransferItem, TransferKind, AUTH_TTL};
    use authz::envelope::Operation;
    let (session, transfer, kind) = match &op.op {
        Operation::SessionPasteImage {
            session,
            transfer,
            mime,
            size,
            sha256,
        } => (
            session,
            transfer,
            TransferKind::Image {
                mime: mime.clone(),
                size: *size,
                sha256: sha256.clone(),
            },
        ),
        Operation::SessionSendFiles {
            session,
            transfer,
            files,
        } => (
            session,
            transfer,
            TransferKind::Files {
                files: files
                    .iter()
                    .map(|f| TransferItem {
                        name: f.name.clone(),
                        size: f.size,
                        sha256: f.sha256.clone(),
                    })
                    .collect(),
            },
        ),
        _ => anyhow::bail!("not a transfer"),
    };
    {
        let sessions = host.sessions.lock().unwrap();
        let live = sessions
            .get(session)
            .context("session is not running on this host")?;
        ensure!(
            live.registration.transfer_gate && live.registration.file_receiver,
            "this terminal does not accept guarded transfers"
        );
        if !live.registration.effect_gate {
            return Err(UpgradeRequired.into());
        }
    }
    host.transfers
        .reserve(&op.kid, session, transfer, kind.bytes())
        .map_err(|reason| anyhow::anyhow!(reason))?;
    if let Err(error) = authz::dispatch::begin(&host.dir, op) {
        host.transfers.release(transfer);
        return Err(error);
    }
    // An absolute deadline: never later than the signed operation's own expiry,
    // so delay between here and the wrapper can only shorten the permit.
    let expires_unix_ms = deadline_in(AUTH_TTL).min((op.exp.max(0) as u64).saturating_mul(1000));
    let sent = host
        .attribute(session, &authz::cutover::Authority::of(op))
        .and_then(|()| {
            host.deliver_effect(
                session,
                ServerMessage::TransferAuthorized {
                    transfer: transfer.clone(),
                    kind,
                    expires_unix_ms,
                    authority: Some(authz::cutover::Authority::of(op)),
                },
            )
        });
    if sent.is_err() {
        host.transfers.release(transfer);
    }
    sent
}

/// A local coding session may end only its durable handoff children.
async fn end_handoff_local(host: &Host, source: &str, child: &str) -> Result<(String, bool)> {
    ensure!(
        authz::envelope::valid_session_id(source) && authz::envelope::valid_session_id(child),
        "invalid session id"
    );
    ensure!(source != child, "a session cannot end itself");
    ensure!(
        host.sessions.lock().unwrap().contains_key(source),
        "source is not registered on this host"
    );
    let state = authz::store::Store::open(&host.dir)?
        .handoff_child_state(source, child)?
        .context("session is not this source's handoff child")?;
    if !host.sessions.lock().unwrap().contains_key(child) {
        return Ok((
            if matches!(state.as_str(), "starting" | "uncertain") {
                "starting"
            } else {
                "not_running"
            }
            .into(),
            false,
        ));
    }
    host.journal_end(child).await;
    authz::store::Store::open(&host.dir)?.close_for_restore(child)?;
    let notified = host.deliver_end(child);
    Ok(("ending".into(), notified))
}

fn handoff_request_digest(
    brief: &str,
    model: Option<&conversations_core::messaging::HandoffModel>,
) -> Result<Vec<u8>> {
    // Preserve old model-less request hashes; a model/variant becomes part of the idempotent request.
    let request_bytes = match model {
        Some(model) => serde_json::to_vec(&(brief, model))?,
        None => brief.as_bytes().to_vec(),
    };
    let mut digest = ring::digest::digest(&ring::digest::SHA256, &request_bytes)
        .as_ref()
        .to_vec();
    // Legacy hashes are exactly 32 bytes. A separate 33-byte domain prevents a
    // crafted plain brief from colliding with the serialized model request.
    if model.is_some() {
        digest.insert(0, 1);
    }
    Ok(digest)
}

/// Start a handoff child for a session that is registered on THIS host. The
/// request arrives on the owner-only local socket; nothing remote can reach it
/// (agent keys have no signed operation that starts a session). The child is
/// launched from local settings and the host's own working directory.
async fn handoff_local(
    host: &Host,
    source: &str,
    idem: &str,
    brief: &str,
    agent: Option<conversations_core::AgentKind>,
    model: Option<&str>,
    variant: Option<&str>,
) -> Result<(String, String, Option<String>)> {
    use conversations_core::AgentKind;
    ensure!(
        authz::envelope::valid_session_id(source),
        "invalid source session"
    );
    ensure!(
        (1..=128).contains(&idem.len()),
        "idempotency_key must be 1-128 bytes"
    );
    ensure!(
        !brief.trim().is_empty() && brief.len() <= 16 * 1024,
        "brief must be 1 byte to 16 KiB"
    );
    let source_agent = host
        .sessions
        .lock()
        .unwrap()
        .get(source)
        .map(|s| s.registration.launch.agent)
        .context("the source session is not registered with this host")?;
    let agent = agent.unwrap_or(source_agent);
    ensure!(agent != AgentKind::Zsh, "a handoff needs a coding agent");
    let model = conversations_core::messaging::validate_handoff_model(agent, model, variant)
        .map_err(anyhow::Error::msg)?;
    let digest = handoff_request_digest(brief, model.as_ref())?;
    let child = uuid::Uuid::new_v4().to_string();
    let (dir, s, i, c, a, d) = (
        host.dir.clone(),
        source.to_owned(),
        idem.to_owned(),
        child.clone(),
        agent.as_str().to_owned(),
        digest.clone(),
    );
    let reserved = tokio::task::spawn_blocking(move || {
        authz::store::Store::open(&dir)?.handoff_reserve(&s, &i, &c, &a, &d)
    })
    .await??;
    if let Err(prior) = reserved {
        ensure!(
            prior.brief_sha256 == digest && prior.agent == agent.as_str(),
            "idempotency key already used for a different handoff"
        );
        return Ok((prior.child, prior.state, prior.error));
    }
    let outcome: Result<()> = async {
        let settings = super::settings::Settings::load(&host.dir)?;
        let launcher = if settings.launch.default_launcher.supports(agent) {
            settings.launch.default_launcher
        } else {
            conversations_core::AgentLauncher::default()
        };
        let command = ControlCommand::Handoff {
            session_id: child.clone(),
            source_session_id: source.to_owned(),
            brief: brief.to_owned(),
            model: model.clone(),
            agent,
            agent_args: settings.launch.agent_args(agent)?,
            launcher,
            adk_statusline: launcher == conversations_core::AgentLauncher::Adk
                && settings.launch.adk_statusline,
        };
        let launch = launch_from_control_with_id(
            command.clone(),
            &host.cwd,
            Some(&host.server),
            None,
            &host.launch_env,
        )?;
        let recorded = authz::record_started(
            &host.dir,
            &authz::sessions::LocalSession {
                session_id: launch.id.clone(),
                agent: launch.agent,
                cwd: launch.cwd.to_string_lossy().into_owned(),
                resume_id: (launch.agent == AgentKind::Claude).then(|| launch.id.clone()),
                launcher,
                adk_statusline: launcher == conversations_core::AgentLauncher::Adk
                    && settings.launch.adk_statusline,
            },
        )?;
        ensure!(recorded, "a local record for this session already exists");
        if let Some(model) = &model {
            authz::store::Store::open(&host.dir)?.set_handoff_model(&child, model)?;
        }
        if host.cmux.is_some() {
            authz::store::Store::open(&host.dir)?.open_for_restore(&child, &host.server)?;
        }
        if let Err(error) = host.launch(&launch).await {
            if !authz::dispatch::is_uncertain(&error) {
                let _ = authz::forget_started(&host.dir, &launch.id);
            }
            return Err(error);
        }
        Ok(())
    }
    .await;
    if outcome.is_ok() && agent == AgentKind::OpenCode && model.is_some() {
        let (dir, source, idem, monitor_child) = (
            host.dir.clone(),
            source.to_owned(),
            idem.to_owned(),
            child.clone(),
        );
        tokio::spawn(async move {
            let result =
                await_opencode_handoff(&dir, &monitor_child, OPENCODE_HANDOFF_TIMEOUT).await;
            let (state, error) = match result {
                Ok(()) => ("started", None),
                Err(f) => {
                    if !f.may_be_running {
                        let _ = authz::store::Store::open(&dir)
                            .and_then(|mut s| s.close_for_restore(&monitor_child));
                    }
                    (
                        if f.may_be_running {
                            "uncertain"
                        } else {
                            "failed"
                        },
                        Some(f.error.to_string()),
                    )
                }
            };
            if let Err(error) = authz::store::Store::open(&dir)
                .and_then(|mut s| s.handoff_finish(&source, &idem, state, error.as_deref()))
            {
                eprintln!("Cannot persist handoff confirmation: {error:#}");
            }
        });
        return Ok((child, "starting".into(), None));
    }
    let (state, error) = match &outcome {
        Ok(()) => ("started".to_owned(), None),
        Err(error) if authz::dispatch::is_uncertain(error) => (
            "uncertain".to_owned(),
            Some(format!("{error:#}").chars().take(500).collect::<String>()),
        ),
        Err(error) => (
            "failed".to_owned(),
            Some(format!("{error:#}").chars().take(500).collect::<String>()),
        ),
    };
    let (dir, s, i, st, er) = (
        host.dir.clone(),
        source.to_owned(),
        idem.to_owned(),
        state.clone(),
        error.clone(),
    );
    tokio::task::spawn_blocking(move || {
        authz::store::Store::open(&dir)?.handoff_finish(&s, &i, &st, er.as_deref())
    })
    .await??;
    Ok((child, state, error))
}

/// Report the recorded state of the message the signing key sent as operation
/// `message`, for the host-signed result: `absent`, `queued`, `received`,
/// `acknowledged`, `revoked` or `expired`. Another key's messages are `absent`.
async fn agent_status_signed(
    host: &Host,
    op: &authz::envelope::Verified,
    session: &str,
    message: &str,
) -> Result<String> {
    authz::dispatch::begin(&host.dir, op)?;
    let (dir, kid, session, message) = (
        host.dir.clone(),
        op.kid.clone(),
        session.to_owned(),
        message.to_owned(),
    );
    tokio::task::spawn_blocking(move || {
        authz::store::Store::open(&dir)?.inbox_state(&kid, &session, &message)
    })
    .await?
}

/// Record a signed agent message in the local inbox. This is queueing only: no
/// terminal or agent input happens here. The agent's delivery consumers read the
/// inbox and the host re-checks the sender's current grant before each hand-off.
async fn agent_message_signed(
    host: &Host,
    op: &authz::envelope::Verified,
    source: &str,
    source_host: &str,
    text: &str,
    reply_to: Option<&str>,
) -> Result<()> {
    authz::dispatch::begin(&host.dir, op)?;
    let (dir, op, source, source_host, text, reply_to) = (
        host.dir.clone(),
        op.clone(),
        source.to_owned(),
        source_host.to_owned(),
        text.to_owned(),
        reply_to.map(str::to_owned),
    );
    tokio::task::spawn_blocking(move || {
        authz::store::Store::open(&dir)?
            .inbox_admit(
                &op,
                &source,
                &source_host,
                &text,
                reply_to.as_deref(),
                authz::store::now_secs(),
            )
            .map(|_| ())
    })
    .await?
}

/// Builds a blocking API client for pushing PR snapshots with this host's login.
/// The client owns a runtime, so create and use it only on a blocking thread.
fn pr_client_parts(host: &Host) -> (String, Option<String>) {
    (host.server.clone(), host.cookie.clone())
}

/// A signed `pr.sync` whose target was recorded but whose PR cache stayed on
/// this host. Reported as `failed` with this (host-neutral) reason, never as a
/// completed sync: there is no private PR view the result could have gone to.
#[derive(Debug, Clone, Copy)]
struct PrCacheUnavailable(&'static str);

impl std::fmt::Display for PrCacheUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PrCacheUnavailable {}

/// Re-read one named pull request with the local `gh`. The target is the signed
/// one; it is also remembered so the local poll can keep it fresh.
async fn pr_sync_signed(
    host: &Host,
    op: &authz::envelope::Verified,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<()> {
    authz::dispatch::begin(&host.dir, op)?;
    let authority = authz::cutover::Authority::of(op);
    let dir = host.dir.clone();
    let (session, owner, repo) = (session.to_owned(), owner.to_owned(), repo.to_owned());
    let (server, cookie) = pr_client_parts(host);
    tokio::task::spawn_blocking(move || {
        authz::record_pr_target(&dir, &session, &owner, &repo, number, &authority)?;
        let client = crate::client::ApiClient::with_cookie(&server, cookie)?;
        match super::github::sync_pr_explicit(&dir, &client, &session, &owner, &repo, number)? {
            super::github::PrCache::Pushed | super::github::PrCache::CachedLocally => Ok(()),
            super::github::PrCache::Withheld(reason) => {
                Err(PrCacheUnavailable(reason.sync_detail()).into())
            }
        }
    })
    .await?
}

/// Post ONE comment exactly as signed, then refresh that PR's comments (a failed
/// refresh does not undo or fail a comment that was posted). Returns the detail
/// to report when the refresh was withheld (e.g. a private session).
#[allow(clippy::too_many_arguments)]
async fn pr_comment_signed(
    host: &Host,
    op: &authz::envelope::Verified,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
    body: &str,
    in_reply_to: Option<i64>,
) -> Result<Option<&'static str>> {
    authz::dispatch::begin(&host.dir, op)?;
    let authority = authz::cutover::Authority::of(op);
    let dir = host.dir.clone();
    let (session, owner, repo, body) = (
        session.to_owned(),
        owner.to_owned(),
        repo.to_owned(),
        body.to_owned(),
    );
    let (server, cookie) = pr_client_parts(host);
    tokio::task::spawn_blocking(move || {
        authz::record_pr_target(&dir, &session, &owner, &repo, number, &authority)?;
        super::github::post_comment(&owner, &repo, number, &body, in_reply_to)?;
        let refreshed = crate::client::ApiClient::with_cookie(&server, cookie).and_then(|client| {
            super::github::resync_comments_explicit(&dir, &client, &session, &owner, &repo, number)
        });
        Ok(match refreshed {
            Ok(super::github::PrCache::Pushed | super::github::PrCache::CachedLocally) => None,
            Ok(super::github::PrCache::Withheld(reason)) => Some(reason.comment_detail()),
            Err(error) => {
                eprintln!("PR comment posted; refresh failed: {error:#}");
                None
            }
        })
    })
    .await?
}

/// Replace the launch block of the local settings file with exactly what was
/// signed. Nothing else in the file can change through this path.
async fn settings_update_signed(
    host: &Host,
    op: &authz::envelope::Verified,
    launch: &authz::envelope::LaunchUpdate,
) -> Result<()> {
    authz::dispatch::begin(&host.dir, op)?;
    let dir = host.dir.clone();
    let launch = super::settings::LaunchSettings::from(launch);
    tokio::task::spawn_blocking(move || super::settings::replace_launch(&dir, launch)).await?
}

/// Run one owner-side passkey ceremony over the admitted channel operation
/// (`passkey.manage`, owner-only, never grantable): the owner's delegated
/// browser as a signed remote control for what the local CLI does. The
/// operation was admitted by `dispatch::decide` before this runs; `begin`
/// journals it so a crash never leaves the ceremony ambiguous.
async fn passkey_channel(
    host: &Host,
    op: &authz::envelope::Verified,
    request: authz::passkey::PasskeyRequest,
) -> Result<authz::passkey::PasskeyResponse> {
    authz::dispatch::begin(&host.dir, op)?;
    let dir = host.dir.clone();
    tokio::task::spawn_blocking(move || {
        let settings = super::settings::Settings::load(&dir)?;
        authz::passkey::handle_channel(&dir, &settings, request, authz::store::now_secs())
    })
    .await?
}

/// Begin an enrollment and answer with the invite link. The link carries
/// everything in the fragment, which browsers never send to a server; it
/// grants nothing — the registration still waits for `PasskeyApprove`.
async fn passkey_enroll_start_signed(
    host: &Host,
    op: &authz::envelope::Verified,
    label: &str,
    kind: &str,
    grants: &[String],
) -> Result<String> {
    let response = passkey_channel(
        host,
        op,
        authz::passkey::PasskeyRequest::EnrollStart {
            label: label.to_owned(),
            kind: kind.to_owned(),
            grants: grants.to_vec(),
        },
    )
    .await?;
    match response {
        authz::passkey::PasskeyResponse::Started { challenge, options } => {
            let server = host.server.clone();
            let link = tokio::task::spawn_blocking({
                let dir = host.dir.clone();
                move || {
                    let descriptor = authz::attest::descriptor(&dir)?;
                    authz::passkey::enrollment_link(&server, &descriptor, &challenge, &options)
                }
            })
            .await??;
            Ok(serde_json::to_string(&serde_json::json!({ "link": link }))?)
        }
        other => bail!("unexpected enroll-start answer: {other:?}"),
    }
}

/// List this host's pending registrations — the key offers it is holding.
async fn passkey_pending_signed(host: &Host, op: &authz::envelope::Verified) -> Result<String> {
    match passkey_channel(host, op, authz::passkey::PasskeyRequest::Pending).await? {
        authz::passkey::PasskeyResponse::Pending { pending } => {
            Ok(serde_json::to_string(&pending)?)
        }
        other => bail!("unexpected pending answer: {other:?}"),
    }
}

/// Approve one pending registration: the host records the trust locally.
async fn passkey_approve_signed(
    host: &Host,
    op: &authz::envelope::Verified,
    cred_id: &str,
) -> Result<String> {
    match passkey_channel(
        host,
        op,
        authz::passkey::PasskeyRequest::Approve {
            cred_id: cred_id.to_owned(),
        },
    )
    .await?
    {
        authz::passkey::PasskeyResponse::Approved { cred_id } => Ok(serde_json::to_string(
            &serde_json::json!({ "cred_id": cred_id }),
        )?),
        other => bail!("unexpected approve answer: {other:?}"),
    }
}

/// Sign one enrollment-ceremony vouch with THIS host's identity key for a
/// ceremony on another host (EX-04): the statement binds that host, that
/// ceremony and the passkey id the enrollment is expected to produce, and
/// the answer also carries this host's public key so the enrolling host can
/// verify the signature and both humans can derive the same pairwise code.
/// Admitted against `host.settings_read` (owner-only); it names no session
/// and grants nothing.
#[allow(clippy::too_many_arguments)]
async fn passkey_vouch_signed(
    host: &Host,
    op: &authz::envelope::Verified,
    for_host: &str,
    challenge: &str,
    cred_id: &str,
) -> Result<String> {
    authz::dispatch::begin(&host.dir, op)?;
    let dir = host.dir.clone();
    let (for_host, challenge, cred_id) = (
        for_host.to_owned(),
        challenge.to_owned(),
        cred_id.to_owned(),
    );
    let value = tokio::task::spawn_blocking(move || {
        authz::passkey::vouch(
            &mut authz::store::Store::open(&dir)?,
            &for_host,
            &challenge,
            &cred_id,
        )
    })
    .await??;
    Ok(serde_json::to_string(&value)?)
}

/// Re-read recorded pull requests of the sessions running on this host, on
/// a per-target gradual-backoff schedule. The targets are this host's own
/// table (filled by signed operations). The loop wakes every
/// [`PR_POLL_TICK_SECS`] and `sync_recorded` only reads targets whose
/// `next_poll_at` is now or earlier. Per-target state in
/// `session_pr_targets` (next_poll_at, current_delay_secs, fingerprint)
/// survives restart, so a long-quiet host wakes near the long end of the
/// previous backoff rather than the floor.
const PR_POLL_TICK_SECS: u64 = 10;

async fn pr_poll_loop(host: Arc<Host>) {
    loop {
        tokio::time::sleep(Duration::from_secs(PR_POLL_TICK_SECS)).await;
        pr_poll_once(&host).await;
    }
}

/// One pass of the poll: only sessions running on THIS host, only pull requests
/// recorded for them by signed operations AND currently due per their
/// schedule. Private sessions are refreshed into the local pr-cache only
/// (never uploaded to the central server). A target that reports no changes
/// across successive passes doubles its delay toward
/// [`authz::store::PR_POLL_MAX_DELAY_SECS`]; a change or a fresh signed
/// `pr.sync` resets it to [`authz::store::PR_POLL_MIN_DELAY_SECS`].
pub(super) async fn pr_poll_once(host: &Host) {
    let live: Vec<String> = host.sessions.lock().unwrap().keys().cloned().collect();
    if live.is_empty() {
        return;
    }
    let dir = host.dir.clone();
    let (server, cookie) = pr_client_parts(host);
    let _ = tokio::task::spawn_blocking(move || {
        let client = match crate::client::ApiClient::with_cookie(&server, cookie) {
            Ok(client) => client,
            Err(error) => {
                eprintln!("PR poll: no client: {error:#}");
                return;
            }
        };
        for session in live {
            if let Err(error) = super::github::sync_recorded(&dir, &client, &session) {
                eprintln!("PR poll for {session} failed: {error:#}");
            }
        }
    })
    .await;
}

/// A signed message: sanitized text, then Enter as a separate, delayed keystroke
/// so the agent's TUI registers it as a submit rather than part of a paste.
async fn reply_signed(
    host: &Host,
    op: &authz::envelope::Verified,
    session: &str,
    text: &str,
) -> Result<()> {
    let text = sanitize_reply(text);
    ensure!(!text.trim().is_empty(), "message is empty after sanitizing");
    host.effect_gated(session)?;
    authz::dispatch::begin(&host.dir, op)?;
    let authority = authz::cutover::Authority::of(op);
    host.attribute(session, &authority)?;
    host.deliver_effect(
        session,
        ServerMessage::Input {
            b64: STANDARD.encode(text),
            authority: Some(authority.clone()),
        },
    )?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    host.deliver_effect(
        session,
        ServerMessage::Input {
            b64: STANDARD.encode(b"\r"),
            authority: Some(authority),
        },
    )
}

async fn end_signed(host: &Host, op: &authz::envelope::Verified, session: &str) -> Result<()> {
    ensure!(
        host.sessions.lock().unwrap().contains_key(session),
        "session is not running on this host"
    );
    authz::dispatch::begin(&host.dir, op)?;
    // Journal End before the checkpoint goes: a crash between the two must not
    // leave a running agent with neither.
    host.journal_end(session).await;
    authz::store::Store::open(&host.dir)?.close_for_restore(session)?;
    // Restart is End followed by Reopen with the same ID. Keep the last visible
    // grid for that replacement's hidden PTY. This cache cannot restore an
    // agent: End still removes its open-session checkpoint above.
    host.deliver_end(session);
    Ok(())
}

async fn start_signed(host: &Host, op: &authz::envelope::Verified) -> Result<()> {
    let settings = super::settings::Settings::load(&host.dir)?;
    let (id, command) = authz::dispatch::start_command(&settings, &op.op)?;
    let launch = launch_from_control_with_id(
        command.clone(),
        &host.cwd,
        Some(&host.server),
        Some(id),
        &host.launch_env,
    )?;
    // A signed start creates a NEW session. It must never overwrite or later
    // delete the checkpoint or state of one that exists, so refuse before
    // anything is persisted (the journal records it as failed, never dispatched).
    ensure!(
        !host.sessions.lock().unwrap().contains_key(&launch.id)
            && !host.pending.lock().unwrap().contains_key(&launch.id)
            && !wrapper_alive(&host.dir, &launch.id).await
            && authz::lookup(&host.dir, &launch.id)?.is_none(),
        "session {} already exists on this host",
        launch.id
    );
    // Order matters: the journal says `dispatching` BEFORE any restorable
    // checkpoint exists, so a checkpoint can never outlive an `admitted`
    // record. A crash after this point leaves `uncertain`, which startup
    // restore treats as untrusted (see `restore_saved_sessions_once`).
    authz::dispatch::begin(&host.dir, op)?;
    crash_point("signed_start_after_begin");
    let recorded = authz::record_started(
        &host.dir,
        &authz::sessions::LocalSession {
            session_id: launch.id.clone(),
            agent: launch.agent,
            cwd: launch.cwd.to_string_lossy().into_owned(),
            resume_id: (launch.agent == conversations_core::AgentKind::Claude)
                .then(|| launch.id.clone()),
            launcher: match &command {
                ControlCommand::StartNew { launcher, .. } => *launcher,
                _ => conversations_core::AgentLauncher::default(),
            },
            adk_statusline: matches!(
                &command,
                ControlCommand::StartNew {
                    adk_statusline: true,
                    ..
                }
            ),
        },
    )?;
    ensure!(recorded, "a local record for this session already exists");
    if host.cmux.is_some() {
        authz::store::Store::open(&host.dir)?.open_for_restore(&launch.id, &host.server)?;
    }
    crash_point("signed_start_after_checkpoint");
    let result = host.launch(&launch).await;
    // Only a DEFINITE non-start undoes local state. When the spawn or RPC may
    // have acted, the record and checkpoint stay: they name a process that may be
    // running, and deleting them would orphan it and invite a blind duplicate.
    let definite_failure = result
        .as_ref()
        .err()
        .is_some_and(|error| !authz::dispatch::is_uncertain(error));
    if definite_failure {
        // The record exists only because of this start (a prior one was
        // refused above), and no terminal was created.
        if let Err(error) = authz::forget_started(&host.dir, &launch.id) {
            eprintln!("Cannot remove failed session record: {error:#}");
        }
    }
    result
}

/// Resume an existing local session. Everything execution-affecting (cwd, native
/// resume id, harness, flags) comes from this host's own records and settings.
async fn reopen_signed(host: &Host, op: &authz::envelope::Verified) -> Result<()> {
    let store = authz::store::Store::open(&host.dir)?;
    ensure!(
        store.can_reopen_import(op.op.session_id())?,
        "imported session is read-only"
    );
    let settings = super::settings::Settings::load(&host.dir)?;
    let session = authz::lookup(&host.dir, op.op.session_id())?.context("unknown local session")?;
    authz::dispatch::begin(&host.dir, op)?;
    reopen_prepared(host, store, settings, session).await
}

async fn reopen_prepared(
    host: &Host,
    store: authz::store::Store,
    settings: super::settings::Settings,
    session: authz::sessions::LocalSession,
) -> Result<()> {
    // Reattach before preparing fresh context or allocating a native identity.
    if host.reopen_existing(&session.session_id).await? {
        if host.cmux.is_some() {
            authz::store::Store::open(&host.dir)?
                .open_for_restore(&session.session_id, &host.server)?;
        }
        return Ok(());
    }
    let continuation = session.resume_id.is_none()
        && store.import_resume_mode(&session.session_id)?.as_deref() == Some("continuation");
    let mut command = if continuation {
        super::continuation::prepare(&host.dir, &settings, &session)?
    } else {
        authz::dispatch::reopen_command(&settings, &session)?
    };
    if let (ControlCommand::Reopen { agent_args, .. }, Some(model)) =
        (&mut command, store.handoff_model(&session.session_id)?)
    {
        agent_args.extend(crate::commands::live::interactive::model_args(
            session.agent,
            &model,
        ));
    }
    let cwd = if continuation {
        Path::new(&session.cwd)
    } else {
        &host.cwd
    };
    let launch = launch_from_control_at(command, cwd, Some(&host.server), &host.launch_env)?;
    let created_intent = host.cmux.is_some()
        && authz::store::Store::open(&host.dir)?.open_for_restore(&launch.id, &host.server)?;
    let result = host.launch(&launch).await;
    let definite_failure = result
        .as_ref()
        .err()
        .is_some_and(|error| !authz::dispatch::is_uncertain(error));
    if definite_failure && created_intent {
        authz::store::Store::open(&host.dir)?.close_for_restore(&launch.id)?;
    }
    result
}

/// adk can wait on a 1Password approval before OpenCode even starts, and the
/// plugin waits up to 20 seconds for the model catalog.
const OPENCODE_HANDOFF_TIMEOUT: Duration = Duration::from_secs(120);

/// Why a model handoff did not confirm. `may_be_running` means the plugin holds
/// the claim, so the child may still prompt and must keep its checkpoint.
#[derive(Debug)]
struct HandoffFailure {
    error: anyhow::Error,
    may_be_running: bool,
}

/// Wait for the OpenCode plugin to create the session on the chosen model and
/// submit the brief, or to say why it could not. At the deadline the host takes
/// the plugin's one-shot claim, so a handoff reported as failed never starts later.
async fn await_opencode_handoff(
    dir: &Path,
    id: &str,
    timeout: Duration,
) -> std::result::Result<(), HandoffFailure> {
    let stopped = |error: anyhow::Error| HandoffFailure {
        error,
        may_be_running: false,
    };
    let verdict = |dir: &Path| -> std::result::Result<Option<()>, HandoffFailure> {
        match super::opencode_bridge::take_handoff_result(dir, id).map_err(stopped)? {
            Some(Ok(())) => Ok(Some(())),
            Some(Err(failure)) if failure.session_exists => Err(HandoffFailure {
                error: anyhow::anyhow!(
                    "{} The child terminal already has a session; check it before any retry.",
                    failure.message
                ),
                may_be_running: true,
            }),
            Some(Err(failure)) => Err(stopped(anyhow::anyhow!(failure.message))),
            None => Ok(None),
        }
    };
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if verdict(dir)?.is_some() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if verdict(dir)?.is_some() {
        return Ok(());
    }
    if super::opencode_bridge::claim_handoff_for_host(dir, id).map_err(stopped)? {
        return Err(stopped(anyhow::anyhow!("OpenCode did not start the handoff within {} seconds, so it was cancelled and will not run. Check the child terminal, then retry with a new key.", timeout.as_secs())));
    }
    Err(HandoffFailure {
        error: anyhow::anyhow!("OpenCode began the handoff but did not confirm it within {} seconds. It may still be running: check the child terminal before any retry.", timeout.as_secs()),
        may_be_running: true,
    })
}

fn launch_from_control(command: ControlCommand, cwd: &Path, env: &LaunchEnv) -> Result<Launch> {
    launch_from_control_at(command, cwd, None, env)
}

/// Build a terminal launch, pinning the host's ADK executable and workspace
/// before a login shell can replace their environment.
fn launch_from_control_at(
    command: ControlCommand,
    cwd: &Path,
    server: Option<&str>,
    env: &LaunchEnv,
) -> Result<Launch> {
    launch_from_control_with_id(command, cwd, server, None, env)
}

/// `start_id` is the session id a verified, signed start carries; the host
/// never invents it, so a retried start addresses the same session.
fn launch_from_control_with_id(
    command: ControlCommand,
    cwd: &Path,
    server: Option<&str>,
    start_id: Option<String>,
    env: &LaunchEnv,
) -> Result<Launch> {
    let mut opencode_handoff = None;
    let mut fresh_native = None;
    let (id, agent, args, launcher, adk_statusline, resume, requested_cwd) = match command {
        ControlCommand::Handoff {
            session_id,
            source_session_id,
            brief,
            agent,
            mut agent_args,
            launcher,
            adk_statusline,
            model,
        } => {
            if agent == conversations_core::AgentKind::Claude {
                if let Some(index) = agent_args
                    .windows(2)
                    .position(|pair| pair[0] == "--session-id")
                {
                    fresh_native = Some(agent_args.remove(index + 1));
                    agent_args.remove(index);
                }
            }
            let source_url = format!(
                "{}/sessions/{source_session_id}",
                server.unwrap_or_default()
            );
            let launch = crate::commands::live::interactive::handoff_launch(
                agent,
                &brief,
                &source_url,
                model.as_ref(),
            );
            agent_args.extend(launch.args);
            opencode_handoff = launch.opencode;
            (
                session_id,
                agent,
                agent_args,
                launcher,
                adk_statusline,
                None,
                cwd.to_path_buf(),
            )
        }
        ControlCommand::StartNew {
            agent,
            agent_args,
            launcher,
            adk_statusline,
            ..
        } => (
            start_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            agent,
            agent_args,
            launcher,
            adk_statusline,
            None,
            cwd.to_path_buf(),
        ),
        ControlCommand::Reopen {
            session_id,
            agent,
            agent_args,
            launcher,
            adk_statusline,
            resume_id,
            cwd: previous_cwd,
            ..
        } => {
            let resume = resume_id.unwrap_or_else(|| session_id.clone());
            (
                session_id,
                agent,
                agent_args,
                launcher,
                adk_statusline,
                Some(resume),
                PathBuf::from(previous_cwd),
            )
        }
    };
    let launch_cwd = local_launcher_cwd(launcher, &requested_cwd, env)?;
    let mut invocation = crate::commands::live::interactive::launch_agent_for_local_host(
        agent,
        launcher,
        fresh_native.as_deref().unwrap_or(&id),
        resume.as_deref(),
        args,
        adk_statusline,
    );
    if let Some(payload) = opencode_handoff {
        invocation.env.push((
            crate::commands::live::interactive::OPENCODE_HANDOFF_ENV.into(),
            payload,
        ));
    }
    // The terminal's login shell can replace the daemon's dev-shell PATH.
    // Pin ADK before crossing that boundary, without serializing the whole PATH
    // or overriding terminal-owned OpenCode configuration. Launcher resolution
    // honors a configured `host_env.launcher_path` so a daemon started outside
    // the dev shell can still find `adk`.
    if launcher == conversations_core::AgentLauncher::Adk {
        invocation.env.push((
            "ARRAKIS_HOME".into(),
            launch_cwd.to_string_lossy().into_owned(),
        ));
        let program = super::opencode_bridge::executable(
            Path::new(&invocation.program),
            &env.resolve_environment(),
        )
        .context("adk is not executable on the host PATH; restart the Conversations host from the Arrakis DevKit dev shell, or run `arrakis-conversations host configure --arrakis-home <checkout> --launcher-path <dir-with-adk>`")?;
        invocation.program = program.to_string_lossy().into_owned();
    }
    let mut command = if invocation.env.is_empty() {
        vec![invocation.program]
    } else {
        let mut words = vec!["/usr/bin/env".to_owned()];
        words.extend(
            invocation
                .env
                .into_iter()
                .map(|(key, value)| format!("{key}={value}")),
        );
        words.push(invocation.program);
        words
    };
    command.extend(invocation.args);
    Ok(Launch {
        id,
        agent,
        command,
        cwd: launch_cwd,
    })
}

/// Index of the program word, after an optional `/usr/bin/env K=V ...` prefix.
fn program_index(command: &[String]) -> usize {
    if command
        .first()
        .is_some_and(|word| word == "/usr/bin/env" || word == "env")
    {
        1 + command[1..]
            .iter()
            .take_while(|word| word.contains('='))
            .count()
    } else {
        0
    }
}

fn pin_native_executable(launch: &mut Launch, env: &LaunchEnv) -> Result<()> {
    use conversations_core::AgentKind;
    let index = program_index(&launch.command);
    let Some(program) = launch.command.get(index) else {
        return Ok(());
    };
    if !matches!(
        launch.agent,
        AgentKind::Codex | AgentKind::Claude | AgentKind::OpenCode
    ) || Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        != Some(launch.agent.as_str())
    {
        return Ok(());
    }
    // Pre-seed PATH with the configured launcher directories so a daemon
    // without the dev-shell env can still resolve a codex/claude/opencode
    // binary the user explicitly pointed at.
    let mut environment: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
        env.resolve_environment();
    if let Some(path) = environment.get(std::ffi::OsStr::new("PATH")) {
        let paths = std::env::split_paths(path).filter(|path| {
            !path
                .components()
                .any(|part| part.as_os_str() == "cmux-cli-shims")
                && ![
                    "CMUX_AGENT_COMMAND_SHIM_ROOT",
                    "CMUX_CODEX_WRAPPER_SHIM_ROOT",
                    "CMUX_CLAUDE_WRAPPER_SHIM_ROOT",
                ]
                .iter()
                .any(|key| {
                    environment
                        .get(std::ffi::OsStr::new(key))
                        .is_some_and(|root| path == Path::new(root))
                })
        });
        let path = std::env::join_paths(paths)?;
        environment.insert("PATH".into(), path);
    }
    let custom = if program == launch.agent.as_str() {
        match launch.agent {
            AgentKind::Codex => std::env::var_os("CMUX_CUSTOM_CODEX_PATH"),
            AgentKind::Claude => std::env::var_os("CMUX_CUSTOM_CLAUDE_PATH"),
            _ => None,
        }
    } else {
        None
    };
    let resolved = custom.as_deref().filter(|path| !path.is_empty())
        .and_then(|path| super::opencode_bridge::executable(Path::new(path), &environment))
        .or_else(|| super::opencode_bridge::executable(Path::new(program), &environment))
        .with_context(|| format!("{} is not executable on the host PATH; restart the Conversations host from a shell where {} is installed", launch.agent.as_str(), launch.agent.as_str()))?;
    launch.command[index] = resolved.to_string_lossy().into_owned();
    Ok(())
}

fn local_launcher_cwd(
    launcher: conversations_core::AgentLauncher,
    cwd: &Path,
    env: &LaunchEnv,
) -> Result<PathBuf> {
    match launcher {
        conversations_core::AgentLauncher::Adk => {
            // Ambient env wins — a dev shell is authoritative when it is
            // present. Only when it is not do we fall back to the persisted
            // startup recipe. Nothing below is reached by a generic install.
            if let Some(home) = env.resolved_arrakis_home() {
                Ok(home)
            } else {
                crate::commands::live::interactive::adk_session_cwd()
                    .context("set host_env.arrakis_home in local-host.json (e.g. `host configure --arrakis-home /path/to/devkit`) or start the host from the Arrakis DevKit dev shell")
            }
        }
        conversations_core::AgentLauncher::Opencode => Ok(cwd.to_path_buf()),
    }
}

#[path = "relay.rs"]
mod relay;

#[path = "private_channel.rs"]
mod private_channel;

#[cfg(test)]
#[path = "daemon_authz_tests.rs"]
mod authz_tests;

#[cfg(test)]
#[path = "publication_tests.rs"]
mod publication_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use conversations_core::{AgentKind, AgentLauncher};

    #[test]
    fn normal_wrapper_uses_daemon_server_instead_of_saved_default() {
        use clap::Parser;
        const MODE: &str = "CONVERSATIONS_WRAPPER_SERVER_FIXTURE";
        match std::env::var(MODE).as_deref() {
            Err(_) => {
                let home = tempfile::tempdir().unwrap();
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "commands::local_host::daemon::tests::normal_wrapper_uses_daemon_server_instead_of_saved_default", "--nocapture"])
                    .env(MODE, "daemon")
                    .env("CONVERSATIONS_FIXTURE_TEST_BINARY", std::env::current_exe().unwrap())
                    .env("HOME", home.path())
                    .env("XDG_CONFIG_HOME", home.path().join("config"))
                    .env_remove("CONVERSATIONS_SERVER")
                    .env_remove("CONVERSATIONS_WRAPPED")
                    .output().unwrap();
                assert!(
                    output.status.success(),
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok("wrapper") => {
                // Re-enter the real CLI parser and host wrap command from the
                // spawned shim, using exactly the arguments supplied by Host::launch.
                let home = PathBuf::from(std::env::var_os("HOME").unwrap());
                let captured = std::fs::read_to_string(home.join("wrapper-args")).unwrap();
                let mut args = crate::cli::Cli::parse_from(
                    std::iter::once("arrakis-conversations").chain(captured.lines()),
                );
                args.global.server = args
                    .global
                    .server_arg
                    .as_deref()
                    .map(|s| s.trim_end_matches('/').to_string())
                    .or_else(crate::config::default_server)
                    .unwrap_or_else(|| crate::cli::DEFAULT_SERVER.to_string());
                let crate::cli::Command::Host(host) = &args.command else {
                    panic!("expected host wrapper")
                };
                crate::commands::local_host::run(&args.global, host).unwrap();
            }
            Ok("daemon") => {
                crate::config::set_default_server("https://saved-default.example.test").unwrap();
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    let mut host = checkpoint_test_host("https://daemon.example.test".into());
                    host.backend = Backend::Normal;
                    host.cmux = None;
                    let shim = host.cwd.join("wrapper-shim");
                    std::fs::write(&shim, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/wrapper-args\"\nexport CONVERSATIONS_WRAPPER_SERVER_FIXTURE=wrapper\n\"$CONVERSATIONS_FIXTURE_TEST_BINARY\" --exact commands::local_host::daemon::tests::normal_wrapper_uses_daemon_server_instead_of_saved_default --nocapture\nresult=$?\nprintf '%s' \"$result\" > \"$HOME/wrapper-done.tmp\"\n/bin/mv \"$HOME/wrapper-done.tmp\" \"$HOME/wrapper-done\"\nexit \"$result\"\n").unwrap();
                    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
                    host.tuning.executable = Some(shim);
                    let launch = Launch { id: uuid::Uuid::new_v4().to_string(), agent: AgentKind::Zsh, cwd: host.cwd.clone(), command: vec!["/bin/sh".into(), "-c".into(), "printf '%s' \"$CONVERSATIONS_SERVER\" > \"$HOME/observed-server.tmp\" && /bin/mv \"$HOME/observed-server.tmp\" \"$HOME/observed-server\"".into()] };
                    let running = host.launch(&launch);
                    tokio::pin!(running);
                    let observed = async { loop { if let Ok(value) = std::fs::read_to_string(host.cwd.join("observed-server")) { return value; } tokio::time::sleep(Duration::from_millis(10)).await; } };
                    let value = tokio::select! {
                        result = &mut running => panic!("wrapper launch ended before agent ran: {result:?}"),
                        result = tokio::time::timeout(Duration::from_secs(10), observed) => result.expect("wrapper did not launch fixture agent"),
                    };
                    assert_eq!(value, "https://daemon.example.test");
                    tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            if let Ok(code) = std::fs::read_to_string(host.cwd.join("wrapper-done")) {
                                assert_eq!(code, "0");
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }).await.expect("fixture wrapper did not exit");
                });
            }
            _ => panic!("unexpected fixture mode"),
        }
    }

    #[test]
    fn native_launch_survives_login_shell_path_reset() {
        const CHILD: &str = "CONVERSATIONS_NATIVE_PATH_TEST";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let bin = home.path().join("bin");
            let shim = home.path().join("cmux-cli-shims/session");
            for directory in [&bin, &shim] {
                std::fs::create_dir_all(directory).unwrap();
                for name in ["codex", "claude", "opencode"] {
                    let path = directory.join(name);
                    let body = if directory == &bin {
                        "#!/bin/sh\nprintf '%s\\n' \"$@\"\n"
                    } else {
                        "#!/bin/sh\nexit 127\n"
                    };
                    std::fs::write(&path, body).unwrap();
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
                }
            }
            for available in [true, false] {
                let path = if available {
                    std::env::join_paths([&shim, &bin]).unwrap()
                } else {
                    shim.as_os_str().to_owned()
                };
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                // A cmux agent override would bypass the stub agents on PATH.
                for (key, _) in std::env::vars_os() {
                    if key.to_string_lossy().starts_with("CMUX_") {
                        child.env_remove(key);
                    }
                }
                let output = child
                    .args(["--exact", "commands::local_host::daemon::tests::native_launch_survives_login_shell_path_reset", "--nocapture"])
                    .env(CHILD, if available { "available" } else { "missing" })
                    .env("HOME", home.path()).env("PATH", path)
                    .env("CONVERSATIONS_TEST_PYTHON", super::super::opencode_bridge::executable(Path::new("python3"), &std::env::vars_os().collect()).unwrap())
                    .output().unwrap();
                assert!(
                    output.status.success(),
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let host = checkpoint_test_host("https://native-path.example.test".into());
            let binary = host.cwd.join("fake-cmux");
            std::fs::write(
                &binary,
                r#"#!/usr/bin/python3
import json,os,shlex,subprocess,sys
root=os.path.dirname(__file__)
if sys.argv[1]=='--json':
 print(json.dumps({'windows':[{'id':'window','workspaces':[{'id':'host'}]}]}))
elif sys.argv[1:3]==['rpc','workspace.create']:
 args=shlex.split(json.loads(sys.argv[3])['initial_command'])
 command=args[args.index('--')+1:]
 try:
  output=subprocess.run(command,env={'PATH':'/usr/bin:/bin'},capture_output=True)
  result=str(output.returncode).encode()+b'\n'+output.stdout+output.stderr
 except OSError as error: result=str(error).encode()
 open(root+'/outcome.tmp','wb').write(result)
 os.replace(root+'/outcome.tmp',root+'/outcome')
 print('{"workspace_id":"child"}')
else: print('{}')
"#
                .replace(
                    "#!/usr/bin/python3",
                    &format!("#!{}", std::env::var("CONVERSATIONS_TEST_PYTHON").unwrap()),
                ),
            )
            .unwrap();
            for agent in [AgentKind::Codex, AgentKind::Claude, AgentKind::OpenCode] {
                for resume in [false, true] {
                    let control = if resume {
                        ControlCommand::Reopen {
                            session_id: uuid::Uuid::new_v4().to_string(),
                            cwd: host.cwd.display().to_string(),
                            agent,
                            resume_id: Some("native-id".into()),
                            agent_args: vec!["literal $(false); two words".into()],
                            launcher: AgentLauncher::Opencode,
                            adk_statusline: false,
                        }
                    } else {
                        ControlCommand::StartNew {
                            cwd: None,
                            agent,
                            agent_args: vec!["literal $(false); two words".into()],
                            launcher: AgentLauncher::Opencode,
                            adk_statusline: false,
                        }
                    };
                    let launch =
                        launch_from_control(control, &host.cwd, &LaunchEnv::default()).unwrap();
                    let outcome = host.cwd.join("outcome");
                    let _ = std::fs::remove_file(&outcome);
                    if std::env::var(CHILD).unwrap() == "missing" {
                        let result = tokio::time::timeout(Duration::from_secs(5), host.launch(&launch)).await;
                        assert!(
                            matches!(result, Ok(Err(_))),
                            "missing {agent:?} must fail before terminal creation"
                        );
                        assert!(!outcome.exists());
                    } else {
                        let running = host.launch(&launch);
                        tokio::pin!(running);
                        let executed = async {
                            loop {
                                if outcome.exists() {
                                    return std::fs::read_to_string(&outcome).unwrap();
                                }
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                        };
                        let output = tokio::select! {
                            result = &mut running => panic!("launch ended before the agent executed: {result:?}"),
                            result = tokio::time::timeout(Duration::from_secs(10), executed) =>
                                result.expect("terminal launch must execute the agent"),
                        };
                        assert!(output.starts_with("0\n"), "{agent:?}: {output}");
                        assert!(output.contains("literal $(false); two words"), "{output}");
                    }
                }
            }
        });
    }

    #[test]
    fn adk_ui_launch_survives_login_shell_path_reset() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "CONVERSATIONS_ADK_PATH_TEST";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let path = home.path().join("adk");
            std::fs::write(
                &path,
                "#!/bin/sh\n[ \"$ARRAKIS_HOME\" = \"$PWD\" ] || exit 94\nprintf '%s\\n' \"$OC_STATUS\" \"$@\"\necho harness-ready\n",
            )
            .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            for available in [true, false] {
                let search_path = if available {
                    std::env::join_paths([home.path(), Path::new("/usr/bin"), Path::new("/bin")])
                        .unwrap()
                } else {
                    std::ffi::OsString::from("/usr/bin:/bin")
                };
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "commands::local_host::daemon::tests::adk_ui_launch_survives_login_shell_path_reset", "--nocapture"])
                    .env(CHILD, if available { "available" } else { "missing" })
                    .env("ARRAKIS_HOME", home.path())
                    .env("PATH", search_path)
                    .output().unwrap();
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        for mode in ["new", "reopen", "handoff"] {
            let command = if mode == "reopen" {
                ControlCommand::Reopen {
                    session_id: uuid::Uuid::new_v4().to_string(),
                    cwd: "/unused".into(),
                    agent: AgentKind::OpenCode,
                    resume_id: Some("ses_native".into()),
                    agent_args: vec!["--auto".into()],
                    launcher: AgentLauncher::Adk,
                    adk_statusline: false,
                }
            } else if mode == "handoff" {
                ControlCommand::Handoff {
                    session_id: uuid::Uuid::new_v4().to_string(),
                    source_session_id: "source-session".into(),
                    brief: "Continue the review".into(),
                    agent: AgentKind::OpenCode,
                    agent_args: vec!["--auto".into()],
                    launcher: AgentLauncher::Adk,
                    adk_statusline: false,
                    model: None,
                }
            } else {
                ControlCommand::StartNew {
                    cwd: None,
                    agent: AgentKind::OpenCode,
                    agent_args: vec!["--auto".into()],
                    launcher: AgentLauncher::Adk,
                    adk_statusline: false,
                }
            };
            let launch = launch_from_control_at(
                command,
                Path::new("/unused"),
                Some("https://conversations.example"),
                &LaunchEnv::default(),
            );
            if std::env::var(CHILD).unwrap() == "missing" {
                let error = launch
                    .expect_err("missing ADK must fail before creating a terminal")
                    .to_string();
                assert!(
                    error.contains("adk") && error.contains("PATH") && error.contains("dev shell"),
                    "{error}"
                );
                continue;
            }
            let launch = launch.unwrap();
            // Model the login shell replacing the daemon's dev-shell PATH.
            let output = std::process::Command::new(&launch.command[0])
                .args(&launch.command[1..])
                .current_dir(&launch.cwd)
                .env("PATH", "/usr/bin:/bin")
                .env_remove("ARRAKIS_HOME")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            if mode == "handoff" {
                assert!(
                    text.starts_with("0\ncode\n--\n--auto\n--prompt\n"),
                    "{text}"
                );
                assert!(text.contains("Continue the review"), "{text}");
                assert!(
                    text.contains("https://conversations.example/sessions/source-session"),
                    "{text}"
                );
                assert!(text.ends_with("harness-ready\n"), "{text}");
                continue;
            }
            let expected = if mode == "reopen" {
                "0\ncode\n--\n--session\nses_native\n--auto\nharness-ready\n"
            } else {
                "0\ncode\n--\n--auto\nharness-ready\n"
            };
            assert_eq!(text, expected);
        }
    }

    /// The persisted startup recipe (`host_env.arrakis_home` and
    /// `host_env.launcher_path` in `local-host.json`) must make `adk code`
    /// launches work even when the daemon started with no `ARRAKIS_HOME` and no
    /// `adk` on its `PATH` — the regression root fixed by hand on 2026-11-07.
    /// Without the recipe, both failures must still surface with actionable
    /// text; a generic install (no recipe, no ambient env) sees no behavior
    /// change.
    #[test]
    fn adk_launch_recovers_from_persisted_recipe_when_env_is_bare() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "CONVERSATIONS_ADK_RECIPE_FALLBACK_TEST";
        if std::env::var_os(CHILD).is_none() {
            // Isolated workspace: a fake DevKit checkout, a sibling bin/
            // holding a stub `adk` the launcher can resolve by PATH.
            let root = tempfile::tempdir().unwrap();
            let devkit = root.path().join("devkit");
            let bin = root.path().join("bin");
            std::fs::create_dir_all(&devkit).unwrap();
            std::fs::create_dir_all(&bin).unwrap();
            let adk = bin.join("adk");
            std::fs::write(
                &adk,
                "#!/bin/sh\n[ \"$ARRAKIS_HOME\" = \"$PWD\" ] || exit 94\nprintf '%s\\n' \"$@\"\necho harness-ready\n",
            )
            .unwrap();
            std::fs::set_permissions(adk, std::fs::Permissions::from_mode(0o700)).unwrap();
            // Configure through stable profile links, then switch them before
            // startup. A stored canonical target would freeze the old profile.
            let old_home = root.path().join("old-home");
            let old_bin = root.path().join("old-bin");
            std::fs::create_dir_all(&old_home).unwrap();
            std::fs::create_dir_all(&old_bin).unwrap();
            let home_link = root.path().join("home-link");
            let bin_link = root.path().join("bin-link");
            std::os::unix::fs::symlink(&old_home, &home_link).unwrap();
            std::os::unix::fs::symlink(&old_bin, &bin_link).unwrap();
            let home_setting = super::super::configured_directory(&home_link, "home").unwrap();
            let bin_setting = super::super::configured_directory(&bin_link, "bin").unwrap();
            std::fs::remove_file(&home_link).unwrap();
            std::fs::remove_file(&bin_link).unwrap();
            std::os::unix::fs::symlink(&devkit, &home_link).unwrap();
            std::os::unix::fs::symlink(&bin, &bin_link).unwrap();
            // Drive the child with BOTH ambient entries cleared — same shape as
            // a plain cmux surface started outside the dev shell.
            let bare_path =
                std::env::join_paths([Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
            for configured in [
                "configured",
                "configured-empty",
                "configured-stale",
                "configured-ambient-symlink",
                "configured-ambient-relative",
                "missing",
            ] {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child.args(["--exact", "commands::local_host::daemon::tests::adk_launch_recovers_from_persisted_recipe_when_env_is_bare", "--nocapture"])
                    .env(CHILD, configured)
                    .env("CONVERSATIONS_RECIPE_DEVKIT", &devkit)
                    .env("CONVERSATIONS_RECIPE_LAUNCHER_BIN", &bin)
                    .env("CONVERSATIONS_RECIPE_HOME_SETTING", &home_setting)
                    .env("CONVERSATIONS_RECIPE_BIN_SETTING", &bin_setting)
                    .env("PATH", &bare_path)
                    .env_remove("ARRAKIS_HOME");
                if configured == "configured-empty" {
                    child.env("ARRAKIS_HOME", "");
                } else if configured == "configured-stale" {
                    child.env("ARRAKIS_HOME", root.path().join("removed-checkout"));
                }
                if configured == "configured-ambient-symlink" {
                    child.env("ARRAKIS_HOME", &home_link);
                } else if configured == "configured-ambient-relative" {
                    child.current_dir(root.path()).env("ARRAKIS_HOME", "devkit");
                }
                let output = child.output().unwrap();
                assert!(
                    output.status.success(),
                    "{configured}:\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let devkit = PathBuf::from(std::env::var_os("CONVERSATIONS_RECIPE_DEVKIT").unwrap());
        let bin = PathBuf::from(std::env::var_os("CONVERSATIONS_RECIPE_LAUNCHER_BIN").unwrap());
        let configured = std::env::var(CHILD).unwrap();
        let env = if configured.starts_with("configured") {
            LaunchEnv::from_settings(&super::super::settings::HostLaunchEnv {
                arrakis_home: Some(PathBuf::from(
                    std::env::var_os("CONVERSATIONS_RECIPE_HOME_SETTING").unwrap(),
                )),
                launcher_path: vec![PathBuf::from(
                    std::env::var_os("CONVERSATIONS_RECIPE_BIN_SETTING").unwrap(),
                )],
            })
        } else {
            LaunchEnv::default()
        };
        for mode in ["new", "reopen"] {
            let command = if mode == "reopen" {
                ControlCommand::Reopen {
                    session_id: uuid::Uuid::new_v4().to_string(),
                    cwd: "/unused".into(),
                    agent: AgentKind::OpenCode,
                    resume_id: Some("ses_native".into()),
                    agent_args: vec!["--auto".into()],
                    launcher: AgentLauncher::Adk,
                    adk_statusline: false,
                }
            } else {
                ControlCommand::StartNew {
                    cwd: None,
                    agent: AgentKind::OpenCode,
                    agent_args: vec!["--auto".into()],
                    launcher: AgentLauncher::Adk,
                    adk_statusline: false,
                }
            };
            let launch = launch_from_control_at(
                command,
                Path::new("/unused"),
                Some("https://conversations.example"),
                &env,
            );
            if configured == "missing" {
                let error = launch
                    .expect_err("without a recipe, the daemon must refuse up front")
                    .to_string();
                assert!(
                    error.contains("ARRAKIS_HOME") || error.contains("arrakis_home"),
                    "{mode}: {error}"
                );
                continue;
            }
            // With the recipe loaded, the build must succeed AND the child must
            // be the stub adk in `bin/`, and ARRAKIS_HOME must be the resolved
            // DevKit directory.
            let launch = launch.unwrap();
            let adk_path = bin.join("adk");
            let adk_canonical = std::fs::canonicalize(&adk_path).unwrap();
            let devkit_canonical = std::fs::canonicalize(&devkit).unwrap();
            assert_eq!(launch.cwd, devkit_canonical, "{mode}");
            let program_word = launch.command.iter().find(|w| w.ends_with("/adk")).unwrap();
            assert_eq!(
                Path::new(program_word),
                adk_canonical.as_path(),
                "{mode}: {:?}",
                launch.command
            );
            let arrakis_env = launch
                .command
                .iter()
                .find_map(|w| w.strip_prefix("ARRAKIS_HOME="))
                .unwrap();
            assert_eq!(Path::new(arrakis_env), devkit_canonical.as_path(), "{mode}");
            // Executing it reproduces a bare login-shell boundary: the stub
            // refuses unless `ARRAKIS_HOME == PWD`, which proves the launcher
            // pinned both before the terminal took over.
            let output = std::process::Command::new(&launch.command[0])
                .args(&launch.command[1..])
                .current_dir(&launch.cwd)
                .env("PATH", "/usr/bin:/bin")
                .env_remove("ARRAKIS_HOME")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{mode}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.ends_with("harness-ready\n"), "{mode}: {text}");
        }
    }

    #[tokio::test]
    async fn unchanged_revision_does_not_move_workspaces_when_registration_arrives() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const A: &str = "00000000-0000-4000-8000-000000000001";
        const B: &str = "00000000-0000-4000-8000-000000000002";
        const NEW: &str = "00000000-0000-4000-8000-000000000003";
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("cmux");
        let calls = root.path().join("calls");
        let tree = json!({"windows":[{"id":"window", "workspaces":[
            {"id":"a", "panes":[{"surfaces":[{"id":"sa"}]}]},
            {"id":"new", "panes":[{"surfaces":[{"id":"snew"}]}]},
            {"id":"b", "panes":[{"surfaces":[{"id":"sb"}]}]}
        ]}]});
        std::fs::write(
            &binary,
            format!("#!/bin/sh\necho \"$*\" >> {calls:?}\necho '{tree}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let requests = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    headers.push(socket.read_u8().await.unwrap());
                }
                let headers = String::from_utf8(headers).unwrap();
                let body = if headers.starts_with("GET /api/live/order ") {
                    json!({"revision":4, "session_ids":[NEW,A,B]})
                } else {
                    assert!(headers.starts_with("GET /api/live "), "{headers}");
                    json!({"live_sessions":[
                        {"id":A, "viewer_is_author":true},
                        {"id":B, "viewer_is_author":true},
                        {"id":NEW, "viewer_is_author":true}
                    ]})
                }
                .to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let sessions = [("a", A), ("b", B), ("new", NEW)]
            .into_iter()
            .map(|(workspace, id)| {
                (
                    id.to_owned(),
                    Session {
                        registration: Registration {
                            launch: Launch {
                                id: id.into(),
                                agent: AgentKind::Zsh,
                                cwd: root.path().into(),
                                command: vec!["zsh".into()],
                            },
                            workspace: Some(workspace.into()),
                            surface: Some(format!("s{workspace}")),
                            local_terminal: true,
                            reconnecting: false,
                            file_receiver: false,
                            transfer_gate: false,
                            effect_gate: true,
                        },
                        resume_published: None,
                        opencode_title_published: None,
                        readiness: Default::default(),
                        local_input_bytes: 0,
                        local_input_dropped_chunks: 0,
                        commands: mpsc::channel(1).0,
                        pid: None,
                        screen: None,
                    },
                )
            })
            .collect();
        let host = Host {
            dir: root.path().into(),
            cwd: root.path().into(),
            server,
            client: reqwest::Client::new(),
            cookie: None,
            availability: crate::commands::live::availability::AvailabilitySignal::new(),
            backend: Backend::Cmux,
            cmux: Some(Cmux::test_backend(binary)),
            sessions: Mutex::new(sessions),
            registration_lock: tokio::sync::Mutex::new(()),
            title_lock: tokio::sync::Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
            peer_inputs: Mutex::new(HashMap::new()),
            interactions: Interactions::default(),
            stream: StreamState::default(),
            transfers: Default::default(),
            tuning: Default::default(),
            launch_env: LaunchEnv::default(),
            web: None,
        };
        let mut baseline = Some(OrderBaseline {
            ids: vec![A.into(), B.into()],
            revision: 4,
        });
        for _ in 0..3 {
            sync_order(&host, &mut baseline).await.unwrap();
        }
        requests.abort();
        let duplicate = host.sessions.lock().unwrap()[B].registration.clone();
        let (socket, _peer) = tokio::net::UnixStream::pair().unwrap();
        let error = session(Arc::new(host), socket, duplicate)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("terminal already connected"));
        assert!(!std::fs::read_to_string(calls)
            .unwrap()
            .contains("reorder-workspaces"));
        assert_eq!(baseline.unwrap().ids, vec![A, NEW, B]);
    }

    #[tokio::test]
    async fn opencode_reports_update_titles_without_contacting_the_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let registration = Registration {
            launch: Launch {
                id: id.clone(),
                agent: AgentKind::OpenCode,
                cwd: root.path().into(),
                command: vec!["opencode".into()],
            },
            workspace: None,
            surface: None,
            local_terminal: true,
            reconnecting: false,
            file_receiver: false,
            transfer_gate: false,
            effect_gate: true,
        };
        let host = Host {
            dir: root.path().into(),
            cwd: root.path().into(),
            server,
            client: reqwest::Client::new(),
            cookie: None,
            availability: crate::commands::live::availability::AvailabilitySignal::new(),
            backend: Backend::Cmux,
            cmux: None,
            sessions: Mutex::new(HashMap::from([(
                id.clone(),
                Session {
                    registration: registration.clone(),
                    resume_published: Some("native".into()),
                    opencode_title_published: None,
                    readiness: Default::default(),
                    local_input_bytes: 0,
                    local_input_dropped_chunks: 0,
                    commands: mpsc::channel(1).0,
                    pid: None,
                    screen: None,
                },
            )])),
            registration_lock: tokio::sync::Mutex::new(()),
            title_lock: tokio::sync::Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
            peer_inputs: Mutex::new(HashMap::new()),
            interactions: Interactions::default(),
            stream: StreamState::default(),
            transfers: Default::default(),
            tuning: Default::default(),
            launch_env: LaunchEnv::default(),
            web: None,
        };
        let report_file = root.path().join("opencode").join(&id).join("report.json");
        std::fs::create_dir_all(report_file.parent().unwrap()).unwrap();
        let write_report = |native: &str, title: Option<&str>| {
            std::fs::write(
                &report_file,
                json!({"launchID":id,"nativeID":native,"title":title}).to_string(),
            )
            .unwrap();
        };
        write_report("other", Some("Wrong title"));
        sync_opencode_title(&host, &registration).await.unwrap();
        write_report("native", None);
        sync_opencode_title(&host, &registration).await.unwrap();
        write_report("native", Some("First title"));
        sync_opencode_title(&host, &registration).await.unwrap();
        sync_opencode_title(&host, &registration).await.unwrap(); // Unchanged: no request.
        write_report("native", Some("Revised title"));
        sync_opencode_title(&host, &registration).await.unwrap();
        assert_eq!(
            host.sessions.lock().unwrap()[&id]
                .opencode_title_published
                .as_deref(),
            Some("Revised title")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "local title updates must never contact the central server"
        );
    }

    async fn order_response(status: u16, content_type: &str, body: &str) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!("HTTP/1.1 {status} response\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        reqwest::Client::new()
            .get(format!("http://{address}/api/live/order"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn older_server_order_fallback_is_optional_but_real_failures_remain_errors() {
        for (status, content_type, body) in [
            (
                200,
                "text/html; charset=utf-8",
                "<!doctype html><html>SPA</html>",
            ),
            (404, "application/json", "{}"),
            (405, "text/plain", "unsupported"),
        ] {
            assert!(
                read_order_response(order_response(status, content_type, body).await)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        for (status, content_type, body) in [
            (401, "text/html", "login required"),
            (500, "text/html", "server failed"),
            (200, "application/json", "<!doctype html>"),
            (200, "application/json", "{}"),
            (200, "text/plain", "unexpected"),
        ] {
            assert!(
                read_order_response(order_response(status, content_type, body).await)
                    .await
                    .is_err()
            );
        }
        let order = read_order_response(
            order_response(
                200,
                "application/json; charset=utf-8",
                r#"{"revision":7,"session_ids":["a","b"]}"#,
            )
            .await,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(order.revision, 7);
        assert_eq!(order.session_ids, ["a", "b"]);
    }

    #[test]
    fn fresh_codex_identity_persists_for_restart_and_falls_back_on_a_stalled_probe() {
        const CHILD: &str = "CONVERSATIONS_CODEX_IDENTITY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp");
            std::fs::create_dir_all(&scratch).unwrap();
            let temp = tempfile::tempdir_in(scratch).unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "commands::local_host::daemon::tests::fresh_codex_identity_persists_for_restart_and_falls_back_on_a_stalled_probe", "--nocapture"])
                .env(CHILD, "1")
                .env("CMUX_WORKSPACE_ID", "host").env("HOME", temp.path()).env("XDG_CONFIG_HOME", temp.path())
                .output().unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        // macOS caps socket paths near 104 bytes, so the endpoint lives in a
        // short temporary directory rather than under the isolated HOME.
        static ENDPOINT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        let sockets = tempfile::tempdir().unwrap();
        ENDPOINT.set(sockets.path().join("p.sock")).unwrap();
        let probe = CodexIdentityProbe {
            endpoint: |_| Ok(ENDPOINT.get().unwrap().clone()),
            deadline: Duration::from_millis(300),
        };
        let root = "01a168c9-b038-70a3-9f0f-4d3aebae2d94";
        let legacy = "01a162f3-0f5a-76e2-889a-532742e610be";
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            // The host runtime directory (here the isolated home) is owner-only in production.
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
            let rollout = home.join("rollout.jsonl");
            std::fs::write(&rollout, "").unwrap();
            let cmux_bin = home.join("cmux");
            let binding = home.join("binding.json");
            let binds = home.join("binds");
            std::fs::write(&cmux_bin, format!(r#"#!/bin/sh
case "$1 $2 $3" in
 'rpc system.ping '*) echo '{{}}' ;;
 'surface resume show') printf '{{"resume_binding":%s}}' "$(/bin/cat '{}')" ;;
 'surface resume set') printf '%s\n' "$*" >> '{}' ;;
 *) exit 97 ;;
esac
"#, binding.display(), binds.display())).unwrap();
            std::fs::set_permissions(&cmux_bin, std::fs::Permissions::from_mode(0o700)).unwrap();
            let cmux = Cmux::connect(Some(&cmux_bin)).await.unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let server = format!("http://{}", listener.local_addr().unwrap());
            let posts = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
            let http_posts = posts.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        headers.push(socket.read_u8().await.unwrap());
                    }
                    let headers = String::from_utf8(headers).unwrap();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").map(str::to_owned))
                        .unwrap_or_else(|| "0".into())
                        .parse()
                        .unwrap();
                    let mut body = vec![0; length];
                    socket.read_exact(&mut body).await.unwrap();
                    http_posts.lock().unwrap().push((
                        headers.lines().next().unwrap().to_owned(),
                        serde_json::from_slice(&body).unwrap_or(Value::Null),
                    ));
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
                }
            });
            for scenario in ["fresh", "stalled-with-binding", "nothing-known"] {
                let id = uuid::Uuid::new_v4().to_string();
                save_checkpoint(&home, &server, crate::open_sessions::OpenSession {
                    session_id: id.clone(),
                    agent: AgentKind::Codex,
                    cwd: home.display().to_string(),
                    resume_id: None,
                    agent_args: vec!["--sandbox".into(), "danger-full-access".into()],
                    launcher: AgentLauncher::default(),
                    adk_statusline: false,
                }).unwrap();
                let endpoint = ENDPOINT.get().unwrap();
                let _ = std::fs::remove_file(endpoint);
                std::fs::write(&binding, "null").unwrap();
                let app_server = match scenario {
                    "fresh" => Some(super::super::codex::tests::app_server(
                        std::os::unix::net::UnixListener::bind(endpoint).unwrap(),
                        vec![super::super::codex::tests::thread(root, Some(&rollout), json!("vscode"), None, false)],
                    )),
                    "stalled-with-binding" => {
                        std::fs::write(&binding, json!({"kind":"codex","checkpoint_id":legacy}).to_string()).unwrap();
                        let listener = std::os::unix::net::UnixListener::bind(endpoint).unwrap();
                        // Accepts the connection and never answers.
                        Some(std::thread::spawn(move || {
                            let (mut stream, _) = listener.accept().unwrap();
                            let _ = std::io::Read::read_to_end(&mut stream, &mut Vec::new());
                        }))
                    }
                    _ => None,
                };
                let registration = Registration {
                    launch: Launch {
                        id: id.clone(),
                        agent: AgentKind::Codex,
                        cwd: home.clone(),
                        command: vec!["codex".into()],
                    },
                    workspace: Some("workspace".into()),
                    surface: Some("surface".into()),
                    local_terminal: true,
                    reconnecting: false,
                    file_receiver: false,
                    transfer_gate: false, effect_gate: true,
                };
                let host = Host {
                    dir: home.clone(),
                    cwd: home.clone(),
                    server: server.clone(),
                    client: reqwest::Client::new(),
                    cookie: None,
                    availability: crate::commands::live::availability::AvailabilitySignal::new(),
                    backend: Backend::Cmux,
                    cmux: None,
                    sessions: Mutex::new(HashMap::from([(id.clone(), Session {
                        registration: registration.clone(),
                        resume_published: None,
                        opencode_title_published: None,
                        readiness: Default::default(),
                local_input_bytes: 0,
                        local_input_dropped_chunks: 0,
                        commands: mpsc::channel(1).0,
                        pid: None,
                        screen: None,
                    })])),
                    registration_lock: tokio::sync::Mutex::new(()),
            title_lock: tokio::sync::Mutex::new(()),
                    pending: Mutex::new(HashMap::new()),
                    peer_inputs: Mutex::new(HashMap::new()),
                    interactions: Interactions::default(),
        stream: StreamState::default(),
        transfers: Default::default(),
        tuning: Default::default(),
        launch_env: LaunchEnv::default(),
        web: None,
                };
                let started = std::time::Instant::now();
                sync_resume_identity_with(&host, &cmux, &registration, &probe).await.unwrap();
                assert!(started.elapsed() < Duration::from_secs(3), "{scenario}: {:?}", started.elapsed());
                let expected = match scenario {
                    "fresh" => Some(root),
                    "stalled-with-binding" => Some(legacy),
                    _ => None,
                };
                let saved = authz::sessions::restore(&home, &server).unwrap()
                    .into_iter()
                    .find(|saved| saved.session_id == id)
                    .unwrap();
                assert_eq!(saved.resume_id.as_deref(), expected, "{scenario}");
                // The same registration update lands in the host's own session
                // record, which is what a signed reopen resumes from.
                let local = authz::lookup(&home, &id).unwrap();
                assert_eq!(local.as_ref().and_then(|l| l.resume_id.as_deref()), expected, "{scenario}");
                if let Some(local) = local {
                    let settings = super::super::settings::Settings::default();
                    assert_eq!(authz::dispatch::reopen_command(&settings, &local).is_ok(), expected.is_some(), "unbound native identity must remain unrestorable");
                }
                let published = posts.lock().unwrap().iter().find(|(line, _)| line.contains(&format!("/api/live/{id}/agent-session-id"))).map(|(_, body)| body["agent_session_id"].clone());
                // Native resume identity is local state: it is never published.
                assert_eq!(published, None, "{scenario}");
                let bound = std::fs::read_to_string(&binds).unwrap_or_default().lines().any(|line| line.contains(&format!("--checkpoint {id}")));
                assert_eq!(bound, expected.is_some(), "{scenario}");
                // The next restart resumes the same native thread under the
                // same Conversations ID, or still refuses to guess one.
                match expected {
                    Some(native) => {
                        let launch = launch_from_saved_session(saved, &home, &LaunchEnv::default())
                            .unwrap();
                        assert_eq!(launch.id, id);
                        assert!(launch.command.windows(2).any(|pair| pair == ["resume", native]), "{scenario}: {:?}", launch.command);
                    }
                    None => assert!(
                        launch_from_saved_session(saved, &home, &LaunchEnv::default()).is_err()
                    ),
                }
                drop(host);
                if let Some(app_server) = app_server {
                    let _ = std::fs::remove_file(endpoint);
                    tokio::time::timeout(Duration::from_secs(2), tokio::task::spawn_blocking(move || app_server.join().unwrap()))
                        .await
                        .expect("the identity probe kept its connection open")
                        .unwrap();
                }
            }
        });
    }

    fn checkpoint_test_child(name: &str) -> Option<std::process::Output> {
        const CHILD: &str = "CONVERSATIONS_FAILED_CHECKPOINT_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return None;
        }
        let temp = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("commands::local_host::daemon::tests::{name}"),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HOME", temp.path())
            .env("XDG_CONFIG_HOME", temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Some(output)
    }

    #[tokio::test]
    async fn reopen_requests_existing_wrapper_and_preserves_pending_end() {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let lock = super::super::protocol::hold_wrapper_liveness(root.path(), &id).unwrap();
        let host = Host {
            dir: root.path().to_owned(),
            cwd: root.path().to_owned(),
            server: "http://unused.invalid".into(),
            client: reqwest::Client::new(),
            cookie: None,
            availability: crate::commands::live::availability::AvailabilitySignal::new(),
            backend: Backend::Normal,
            cmux: None,
            sessions: Mutex::new(HashMap::new()),
            registration_lock: tokio::sync::Mutex::new(()),
            title_lock: tokio::sync::Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
            peer_inputs: Mutex::new(HashMap::new()),
            interactions: Interactions::default(),
            stream: StreamState::default(),
            transfers: Default::default(),
            tuning: Default::default(),
            launch_env: LaunchEnv::default(),
            web: None,
        };
        let marker = reopen_marker(root.path(), &id, std::process::id()).unwrap();
        let (reopened, ()) = tokio::join!(host.reopen_existing(&id), async {
            tokio::time::timeout(Duration::from_secs(2), async {
                while !marker.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(host.sessions.lock().unwrap().is_empty());
            assert!(
                host.pending.lock().unwrap().is_empty(),
                "started a duplicate wrapper"
            );
            host.sessions.lock().unwrap().insert(
                id.clone(),
                Session {
                    registration: Registration {
                        launch: Launch {
                            id: id.clone(),
                            agent: AgentKind::Codex,
                            cwd: root.path().to_owned(),
                            command: vec!["codex".into()],
                        },
                        workspace: None,
                        surface: None,
                        local_terminal: true,
                        reconnecting: false,
                        file_receiver: true,
                        transfer_gate: true,
                        effect_gate: true,
                    },
                    resume_published: None,
                    opencode_title_published: None,
                    readiness: Default::default(),
                    local_input_bytes: 0,
                    local_input_dropped_chunks: 0,
                    commands: mpsc::channel(1).0,
                    pid: Some(std::process::id()),
                    screen: None,
                },
            );
        });
        assert!(reopened.unwrap());
        assert!(!marker.exists());
        assert!(
            host.reopen_existing(&id).await.unwrap(),
            "already attached reopen was not idempotent"
        );

        mark_pending_end(root.path(), &id, &[std::process::id()]).unwrap();
        assert!(
            !host.reopen_existing(&id).await.unwrap(),
            "reopen bypassed an explicit End"
        );
        assert!(!marker.exists());
        clear_pending_end(root.path(), &id);
        host.sessions.lock().unwrap().clear();
        drop(lock);
        assert!(
            !host.reopen_existing(&id).await.unwrap(),
            "exited wrapper blocked replacement"
        );
    }

    fn checkpoint_test_host(server: String) -> Host {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let binary = home.join("fake-cmux");
        std::fs::write(
            &binary,
            "#!/bin/sh\nif [ \"$1\" = --json ]; then echo '{\"windows\":[]}'; else exit 97; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        Host {
            dir: home.clone(),
            cwd: home,
            server,
            client: reqwest::Client::new(),
            cookie: None,
            availability: crate::commands::live::availability::AvailabilitySignal::new(),
            backend: Backend::Cmux,
            cmux: Some(Cmux::test_backend(binary)),
            sessions: Mutex::new(HashMap::new()),
            registration_lock: tokio::sync::Mutex::new(()),
            title_lock: tokio::sync::Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
            peer_inputs: Mutex::new(HashMap::new()),
            interactions: Interactions::default(),
            stream: StreamState::default(),
            transfers: Default::default(),
            tuning: Default::default(),
            launch_env: LaunchEnv::default(),
            web: None,
        }
    }

    #[test]
    fn startup_drops_missing_cwd_once_and_preserves_transient_failures() {
        if let Some(output) =
            checkpoint_test_child("startup_drops_missing_cwd_once_and_preserves_transient_failures")
        {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                stderr.matches("Dropped saved session").count(),
                1,
                "{stderr}"
            );
            assert!(
                !stderr.contains("00000000-0000-4000-8000-000000000001"),
                "{stderr}"
            );
            assert!(!stderr.contains("/missing"), "{stderr}");
            assert!(
                !stderr.contains("Could not restore saved session: terminal cwd"),
                "{stderr}"
            );
            return;
        }
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let host = Arc::new(checkpoint_test_host(
                "https://checkpoint.example.test".into(),
            ));
            let missing = "00000000-0000-4000-8000-000000000001";
            let transient = uuid::Uuid::new_v4().to_string();
            let unbound = uuid::Uuid::new_v4().to_string();
            for (id, cwd, resume_id) in [
                (missing, host.cwd.join("missing"), Some("native".into())),
                (transient.as_str(), host.cwd.clone(), Some("native".into())),
                (unbound.as_str(), host.cwd.clone(), None),
            ] {
                save_checkpoint(
                    &host.dir,
                    &host.server,
                    crate::open_sessions::OpenSession {
                        session_id: id.into(),
                        agent: AgentKind::Codex,
                        cwd: cwd.display().to_string(),
                        resume_id,
                        agent_args: vec![],
                        launcher: AgentLauncher::default(),
                        adk_statusline: false,
                    },
                )
                .unwrap();
            }
            // Inventory failure must stop restoration without retiring anything.
            let binary = host.cwd.join("fake-cmux");
            let script = std::fs::read_to_string(&binary).unwrap();
            std::fs::write(&binary, "#!/bin/sh\nexit 97\n").unwrap();
            assert!(!restore_saved_sessions_once(host.clone(), true).await);
            assert_eq!(
                authz::sessions::restore(&host.dir, &host.server)
                    .unwrap()
                    .len(),
                3
            );
            std::fs::write(&binary, script).unwrap();
            for _ in 0..2 {
                assert!(restore_saved_sessions_once(host.clone(), true).await);
                let saved = authz::sessions::restore(&host.dir, &host.server).unwrap();
                assert!(
                    !saved.iter().any(|s| s.session_id == missing),
                    "missing cwd survived startup"
                );
                assert!(saved.iter().any(|s| s.session_id == transient));
                assert!(saved.iter().any(|s| s.session_id == unbound));
            }
        });
    }

    #[test]
    fn concurrent_checkpoint_updates_preserve_all_sessions() {
        const CHILD: &str = "CONVERSATIONS_CHECKPOINT_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp");
            std::fs::create_dir_all(&scratch).unwrap();
            let temp = tempfile::tempdir_in(scratch).unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "commands::local_host::daemon::tests::concurrent_checkpoint_updates_preserve_all_sessions", "--nocapture"])
                .env(CHILD, "1").env("HOME", temp.path()).env("XDG_CONFIG_HOME", temp.path())
                .output().unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        std::fs::set_permissions(
            std::env::var("HOME").unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for index in 0..20 {
                        save_checkpoint(
                            Path::new(&std::env::var("HOME").unwrap()),
                            "https://checkpoint.example.test",
                            crate::open_sessions::OpenSession {
                                session_id: uuid::Uuid::new_v4().to_string(),
                                agent: AgentKind::Codex,
                                cwd: ".".into(),
                                resume_id: Some(format!("native-{worker}-{index}")),
                                agent_args: vec![],
                                launcher: AgentLauncher::default(),
                                adk_statusline: false,
                            },
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            authz::sessions::restore(
                Path::new(&std::env::var("HOME").unwrap()),
                "https://checkpoint.example.test"
            )
            .unwrap()
            .len(),
            160
        );
    }

    #[test]
    fn restored_shell_does_not_replay_saved_commands() {
        if checkpoint_test_child("restored_shell_does_not_replay_saved_commands").is_some() {
            return;
        }
        let root = std::env::current_dir().unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let marker = home.join("replayed-command");
        // This child has a private HOME; sibling tests keep their shell settings.
        std::env::set_var("SHELL", "/bin/sh");
        let id = uuid::Uuid::new_v4().to_string();
        let launch = launch_from_saved_session(
            crate::open_sessions::OpenSession {
                session_id: id.clone(),
                agent: AgentKind::Zsh,
                cwd: home.display().to_string(),
                resume_id: None,
                agent_args: vec!["-c".into(), "touch replayed-command".into()],
                launcher: AgentLauncher::default(),
                adk_statusline: false,
            },
            &root,
            &LaunchEnv::default(),
        )
        .unwrap();
        let status = std::process::Command::new(&launch.command[0])
            .args(&launch.command[1..])
            .current_dir(&launch.cwd)
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            !marker.exists(),
            "restoration replayed an old shell command"
        );
        assert_eq!(launch.id, id);
        assert_eq!(launch.cwd, home);
    }

    #[test]
    fn checkpoint_without_native_id_remains_unrestorable() {
        for agent in [AgentKind::Codex, AgentKind::OpenCode] {
            let saved = crate::open_sessions::OpenSession {
                session_id: uuid::Uuid::new_v4().to_string(),
                cwd: ".".into(),
                agent,
                resume_id: None,
                agent_args: vec![],
                launcher: AgentLauncher::default(),
                adk_statusline: false,
            };
            let cwd = std::env::current_dir().unwrap();
            assert!(
                launch_from_saved_session(saved, &cwd, &LaunchEnv::default()).is_err(),
                "{agent}"
            );
        }
    }

    #[test]
    fn imported_continuation_launch_keeps_conversation_and_fresh_native_distinct() {
        use conversations_core::{AgentKind, AgentLauncher};
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let native = uuid::Uuid::new_v4().to_string();
        let launch = launch_from_control_at(
            ControlCommand::Handoff {
                session_id: id.clone(),
                source_session_id: id.clone(),
                brief: "Read context file; wait for user".into(),
                agent: AgentKind::Claude,
                agent_args: vec!["--session-id".into(), native.clone()],
                launcher: AgentLauncher::default(),
                adk_statusline: false,
                model: None,
            },
            root.path(),
            None,
            &LaunchEnv::default(),
        )
        .unwrap();
        assert_eq!(launch.id, id);
        assert_eq!(launch.cwd, root.path());
        assert_eq!(
            launch
                .command
                .iter()
                .filter(|arg| *arg == "--session-id")
                .count(),
            1
        );
        assert!(launch
            .command
            .windows(2)
            .any(|pair| pair == ["--session-id", native.as_str()]));
        assert!(!launch.command.iter().any(|arg| arg == "--resume"));
    }

    #[test]
    fn reopen_uses_saved_directory_and_preserves_native_resume_arguments() {
        let root = std::env::current_dir().unwrap();
        let saved = root.join("src");
        let args = vec![
            "--model".into(),
            "value with spaces '$HOME; `literal`".into(),
            "".into(),
        ];
        let launch = launch_from_control(
            ControlCommand::Reopen {
                session_id: "conversation-id".into(),
                cwd: saved.display().to_string(),
                agent: AgentKind::Codex,
                resume_id: Some("native-id".into()),
                agent_args: args.clone(),
                launcher: AgentLauncher::Opencode,
                adk_statusline: false,
            },
            &root,
            &LaunchEnv::default(),
        )
        .unwrap();
        assert_eq!(launch.cwd, saved);
        assert_eq!(launch.id, "conversation-id");
        assert_eq!(
            &launch.command[..4],
            ["codex", "--no-alt-screen", "resume", "native-id"]
        );
        assert_eq!(&launch.command[4..], args);
    }

    #[tokio::test]
    async fn a_timed_out_opencode_handoff_cannot_start_later() {
        let dir = tempfile::tempdir().unwrap();
        let short = Duration::from_millis(20);
        // Nothing answered: the host takes the claim, so a late plugin does nothing.
        let silent = uuid::Uuid::new_v4().to_string();
        let failure = await_opencode_handoff(dir.path(), &silent, short)
            .await
            .unwrap_err();
        assert!(!failure.may_be_running);
        let error = failure.error.to_string();
        assert!(error.contains("was cancelled and will not run"), "{error}");
        assert!(
            !super::super::opencode_bridge::claim_handoff_for_host(dir.path(), &silent).unwrap()
        );
        // The plugin already claimed it: the child may still prompt, so say so.
        let busy = uuid::Uuid::new_v4().to_string();
        assert!(super::super::opencode_bridge::claim_handoff_for_host(dir.path(), &busy).unwrap());
        let failure = await_opencode_handoff(dir.path(), &busy, short)
            .await
            .unwrap_err();
        assert!(
            failure.may_be_running,
            "a live child must keep its checkpoint"
        );
        let error = failure.error.to_string();
        assert!(error.contains("may still be running"), "{error}");
        // A failure after the session exists leaves a live child, which keeps its checkpoint.
        let late = uuid::Uuid::new_v4().to_string();
        let launch = dir.path().join("opencode").join(&late);
        std::fs::create_dir_all(&launch).unwrap();
        std::fs::write(
            launch.join("handoff-result.json"),
            r#"{"error":"Provider rejected the prompt","sessionID":"ses_child"}"#,
        )
        .unwrap();
        let failure = await_opencode_handoff(dir.path(), &late, short)
            .await
            .unwrap_err();
        assert!(
            failure.may_be_running,
            "a child with a session must keep its checkpoint"
        );
        let error = failure.error.to_string();
        assert!(error.contains("Provider rejected the prompt"), "{error}");
        assert!(error.contains("already has a session"), "{error}");
        // A confirmation is started; a reported failure keeps the plugin's reason.
        for (verdict, expected) in [
            (r#"{"ok":true}"#, None),
            (
                r#"{"error":"Model x is not available"}"#,
                Some("Model x is not available"),
            ),
        ] {
            let id = uuid::Uuid::new_v4().to_string();
            let launch = dir.path().join("opencode").join(&id);
            std::fs::create_dir_all(&launch).unwrap();
            std::fs::write(launch.join("handoff-result.json"), verdict).unwrap();
            let result = await_opencode_handoff(dir.path(), &id, short).await;
            assert!(result.as_ref().err().is_none_or(|f| !f.may_be_running));
            assert_eq!(
                result.err().map(|f| f.error.to_string()).as_deref(),
                expected
            );
        }
    }

    #[test]
    fn executable_pinning_looks_past_an_env_prefix() {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(program_index(&words(&["opencode", "--auto"])), 0);
        assert_eq!(
            program_index(&words(&[
                "/usr/bin/env",
                "A=1",
                "CONVERSATIONS_OPENCODE_HANDOFF={\"x\":1}",
                "opencode",
                "--auto"
            ])),
            3
        );
    }

    #[test]
    fn handoff_model_digest_cannot_alias_a_legacy_json_brief() {
        let model = conversations_core::messaging::HandoffModel {
            model: "x".into(),
            variant: None,
        };
        let crafted = serde_json::to_string(&("work", &model)).unwrap();
        let legacy = handoff_request_digest(&crafted, None).unwrap();
        let selected = handoff_request_digest("work", Some(&model)).unwrap();
        assert_ne!(legacy, selected);
        assert_eq!(
            legacy,
            ring::digest::digest(&ring::digest::SHA256, crafted.as_bytes()).as_ref()
        );
    }

    #[test]
    fn handoff_model_reaches_codex_argv_and_the_opencode_bridge() {
        let handoff = |agent, launcher| ControlCommand::Handoff {
            session_id: uuid::Uuid::new_v4().to_string(),
            source_session_id: "source".into(),
            brief: "Review the change".into(),
            agent,
            agent_args: vec!["--auto".into()],
            launcher,
            adk_statusline: false,
            model: Some(conversations_core::messaging::HandoffModel {
                model: if agent == AgentKind::Codex {
                    "gpt-6.1-sol"
                } else {
                    "anthropic/claude-haiku-4-5"
                }
                .into(),
                variant: Some("high".into()),
            }),
        };
        let root = std::env::current_dir().unwrap();
        let codex = launch_from_control_at(
            handoff(AgentKind::Codex, AgentLauncher::Opencode),
            &root,
            Some("https://conversations.example"),
            &LaunchEnv::default(),
        )
        .unwrap();
        let separator = codex.command.iter().position(|word| word == "--").unwrap();
        let flags = &codex.command[..separator];
        assert!(
            flags.windows(2).any(|w| w == ["--model", "gpt-6.1-sol"]),
            "{flags:?}"
        );
        assert!(
            flags
                .windows(2)
                .any(|w| w == ["-c", "model_reasoning_effort=\"high\""]),
            "{flags:?}"
        );
        assert!(codex.command[separator + 1].ends_with("Review the change"));

        // The adk launcher shares this env prefix; the bridge test covers adk itself.
        {
            let launch = launch_from_control_at(
                handoff(AgentKind::OpenCode, AgentLauncher::Opencode),
                &root,
                None,
                &LaunchEnv::default(),
            )
            .unwrap();
            assert!(
                !launch
                    .command
                    .iter()
                    .any(|w| w == "--prompt" || w == "--model"),
                "{:?}",
                launch.command
            );
            let payload = launch
                .command
                .iter()
                .find_map(|w| w.strip_prefix("CONVERSATIONS_OPENCODE_HANDOFF="))
                .expect("payload in the env prefix");
            let payload: serde_json::Value = serde_json::from_str(payload).unwrap();
            assert_eq!(payload["model"], "anthropic/claude-haiku-4-5");
            assert_eq!(payload["variant"], "high");
            assert!(payload["prompt"]
                .as_str()
                .unwrap()
                .ends_with("Review the change"));
            assert_eq!(launch.command[0], "/usr/bin/env");
        }
    }

    #[test]
    fn managed_opencode_launch_defers_config_to_terminal_boundary() {
        const CHILD: &str = "CONVERSATIONS_TABS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::fs::PermissionsExt;
            let tools = tempfile::tempdir().unwrap();
            let adk = tools.path().join("adk");
            std::fs::write(&adk, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(adk, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut path = vec![tools.path().to_path_buf()];
            path.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            let path = std::env::join_paths(path).unwrap();
            for inherited in [
                None,
                Some(r#"{"mouse":false,"tabs":{"mode":"on","scope":"global"}}"#),
                Some(r#"{"plugins":["daemon-private-plugin"],"tabs":{"mode":"on"}}"#),
                Some("{broken"),
            ] {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child.args(["--exact", "commands::local_host::daemon::tests::managed_opencode_launch_defers_config_to_terminal_boundary", "--nocapture"])
                    .env(CHILD, "1")
                    .env("PATH", &path)
                    .env("ARRAKIS_HOME", std::env::current_dir().unwrap())
                    .env_remove("OPENCODE_CLI_CONFIG_CONTENT");
                if let Some(config) = inherited {
                    child.env("OPENCODE_CLI_CONFIG_CONTENT", config);
                }
                let output = child.output().unwrap();
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }

        let root = std::env::current_dir().unwrap();
        for launcher in [AgentLauncher::Opencode, AgentLauncher::Adk] {
            let handoff_id = uuid::Uuid::new_v4().to_string();
            let handoff = ControlCommand::Handoff {
                session_id: handoff_id.clone(),
                source_session_id: "source-session".into(),
                brief: "Continue review\nKeep --session=literal inside the prompt".into(),
                agent: AgentKind::OpenCode,
                agent_args: vec!["--auto".into()],
                launcher,
                adk_statusline: false,
                model: None,
            };
            let launch = launch_from_control_at(
                handoff.clone(),
                &root,
                Some("https://conversations.example"),
                &LaunchEnv::default(),
            )
            .unwrap();
            assert_eq!(launch.id, handoff_id);
            assert!(!launch
                .command
                .iter()
                .any(|word| word.starts_with("OPENCODE_CLI_CONFIG_CONTENT=")));
            let prompt = launch
                .command
                .windows(2)
                .find(|pair| pair[0] == "--prompt")
                .unwrap();
            assert_eq!(prompt[1], "Handoff from Conversations session: https://conversations.example/sessions/source-session\n\nContinue review\nKeep --session=literal inside the prompt");
            assert!(launch.command.iter().any(|word| word == "--auto"));
            let wrapped = super::super::opencode_bridge::command(
                Path::new("/private/runtime"),
                &launch.id,
                launch.command.clone(),
            )
            .unwrap();
            let boundary = wrapped.iter().position(|word| word == "--").unwrap();
            assert_eq!(
                wrapped[boundary + 1..],
                launch.command,
                "wrapper must preserve the opaque handoff prompt and argument order"
            );
            for reopen in [false, true] {
                let command = if reopen {
                    ControlCommand::Reopen {
                        session_id: "conversation-id".into(),
                        cwd: root.display().to_string(),
                        agent: AgentKind::OpenCode,
                        resume_id: Some("ses_native".into()),
                        agent_args: vec![],
                        launcher,
                        adk_statusline: false,
                    }
                } else {
                    ControlCommand::StartNew {
                        cwd: None,
                        agent: AgentKind::OpenCode,
                        agent_args: vec![],
                        launcher,
                        adk_statusline: false,
                    }
                };
                let launch = launch_from_control(command, &root, &LaunchEnv::default()).unwrap();
                assert!(
                    launch
                        .command
                        .iter()
                        .find_map(|word| word.strip_prefix("OPENCODE_CLI_CONFIG_CONTENT="))
                        .is_none(),
                    "daemon settings must not reach cmux argv or overwrite login-shell settings"
                );
                let program = if launcher == AgentLauncher::Adk {
                    "adk"
                } else {
                    "opencode"
                };
                assert!(launch
                    .command
                    .iter()
                    .position(|word| Path::new(word)
                        .file_name()
                        .is_some_and(|name| name == program))
                    .is_some());
            }
        }
    }

    #[test]
    fn new_claude_invocation_matches_live_host_policy() {
        let root = std::env::current_dir().unwrap();
        let claude = launch_from_control(
            ControlCommand::StartNew {
                cwd: None,
                agent: AgentKind::Claude,
                agent_args: vec!["--model".into(), "sonnet".into()],
                launcher: AgentLauncher::Opencode,
                adk_statusline: false,
            },
            &root,
            &LaunchEnv::default(),
        )
        .unwrap();
        assert_eq!(
            claude.command,
            ["claude", "--session-id", &claude.id, "--model", "sonnet"]
        );
    }

    #[test]
    fn startup_arguments_reach_child_as_distinct_literal_words() {
        let root = std::env::current_dir().unwrap();
        let words = [
            "two words",
            "quote'\"",
            "$HOME; $(false) `false`",
            "",
            "--flag=value",
            "line\nbreak",
        ];
        let mut args = vec!["-c".into(), "printf '%s\\0' \"$@\"".into(), "probe".into()];
        args.extend(words.iter().map(|s| s.to_string()));
        let launch = launch_from_control(
            ControlCommand::StartNew {
                cwd: Some("/ignored-web-directory".into()),
                agent: AgentKind::Zsh,
                agent_args: args,
                launcher: AgentLauncher::Opencode,
                adk_statusline: false,
            },
            &root,
            &LaunchEnv::default(),
        )
        .unwrap();
        assert_eq!(launch.command[0], "zsh");
        // POSIX sh is available on both CI Linux and macOS; zsh itself is
        // optional on CI. This verifies the exact argv handed to the child.
        let output = std::process::Command::new("/bin/sh")
            .args(&launch.command[1..])
            .current_dir(&launch.cwd)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            words
                .iter()
                .flat_map(|s| s.bytes().chain([0]))
                .collect::<Vec<_>>()
        );
        assert_eq!(launch.cwd, root);
    }

    /// Central order revisions, titles and title overrides are unsigned poll
    /// results: they must never rename or move cmux workspaces. Local gestures
    /// still work: a native rename and a native reorder are published, and a
    /// locally known automatic title names its own workspace. The host and a
    /// shared workspace are never renamed.
    #[tokio::test]
    async fn private_titles_stay_local_and_central_state_never_renames_cmux() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const A: &str = "00000000-0000-4000-8000-00000000000a";
        const B: &str = "00000000-0000-4000-8000-00000000000b";
        const O: &str = "00000000-0000-4000-8000-00000000000c";
        const H: &str = "00000000-0000-4000-8000-00000000000d";
        const S1: &str = "00000000-0000-4000-8000-00000000000e";
        const S2: &str = "00000000-0000-4000-8000-00000000000f";
        const FORGED: &str = "Forged central title";
        let root = tempfile::tempdir().unwrap();
        // Owner-only perms let `History::open` (private SQLite) succeed in
        // this same root alongside the mocked cmux state.
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let binary = root.path().join("cmux");
        let calls = root.path().join("calls");
        let state = root.path().join("state.json");
        // One document answers `tree` (windows) and `workspace list` (names).
        let write_state = |order: &[&str], names: &[(&str, &str)]| {
            let surfaces = |ws: &str| match ws {
                "shared" => {
                    json!([{"surfaces":[{"id":"sshared1"}]},{"surfaces":[{"id":"sshared2"}]}])
                }
                ws => json!([{"surfaces":[{"id":format!("s{ws}")}]}]),
            };
            let workspaces: Vec<_> = order
                .iter()
                .map(|ws| json!({"id":ws, "pinned":false, "panes":surfaces(ws)}))
                .collect();
            let names: Vec<_> = names
                .iter()
                .map(|(ws, title)| json!({"id":ws, "custom_title":title}))
                .collect();
            std::fs::write(
                &state,
                json!({"windows":[{"id":"window","workspaces":workspaces}], "workspaces":names})
                    .to_string(),
            )
            .unwrap();
        };
        write_state(&["a", "b", "o", "host", "shared"], &[]);
        std::fs::write(
            &binary,
            format!("#!/bin/sh\necho \"$*\" >> {calls:?}\ncat {state:?}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();

        // A central server that forges a reversed order at a moving revision and
        // the same title and override for every session, including the host's.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let central = Arc::new(Mutex::new((7_i64, Vec::<String>::new())));
        let recorded = Arc::clone(&central);
        let http = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(socket.read_u8().await.unwrap());
                }
                let head = String::from_utf8(head).unwrap();
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                let request = format!(
                    "{} {}",
                    head.lines().next().unwrap(),
                    String::from_utf8(body).unwrap()
                );
                let revision = recorded.lock().unwrap().0;
                let response = if head.starts_with("GET /api/live/order ") {
                    json!({"revision":revision, "session_ids":[S2, S1, H, O, B, A]})
                } else if head.starts_with("GET /api/live ") {
                    let forged: Vec<_> = [A, B, O, H, S1, S2]
                        .iter()
                        .map(|id| json!({"id":id, "title":FORGED, "title_override":FORGED, "viewer_is_author":true}))
                        .collect();
                    json!({"live_sessions":forged})
                } else {
                    recorded.lock().unwrap().1.push(request);
                    json!({})
                }
                .to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
        });
        let sessions = [
            ("a", "sa", A, AgentKind::Claude),
            ("b", "sb", B, AgentKind::Claude),
            ("o", "so", O, AgentKind::OpenCode),
            ("host", "shost", H, AgentKind::Claude),
            ("shared", "sshared1", S1, AgentKind::Claude),
            ("shared", "sshared2", S2, AgentKind::Claude),
        ]
        .into_iter()
        .map(|(workspace, surface, id, agent)| {
            (
                id.to_owned(),
                Session {
                    registration: Registration {
                        launch: Launch {
                            id: id.into(),
                            agent,
                            cwd: root.path().into(),
                            command: vec![agent.as_str().into()],
                        },
                        workspace: Some(workspace.into()),
                        surface: Some(surface.into()),
                        local_terminal: true,
                        reconnecting: false,
                        file_receiver: false,
                        transfer_gate: false,
                        effect_gate: false,
                    },
                    resume_published: None,
                    // The OpenCode bridge already reported this automatic title.
                    opencode_title_published: (id == O).then(|| "OpenCode automatic".into()),
                    readiness: Default::default(),
                    local_input_bytes: 0,
                    local_input_dropped_chunks: 0,
                    commands: mpsc::channel(1).0,
                    pid: None,
                    screen: None,
                },
            )
        })
        .collect();
        let host = Host {
            dir: root.path().into(),
            cwd: root.path().into(),
            server,
            client: reqwest::Client::new(),
            cookie: None,
            availability: crate::commands::live::availability::AvailabilitySignal::new(),
            backend: Backend::Cmux,
            cmux: Some(Cmux::test_backend(binary)),
            sessions: Mutex::new(sessions),
            registration_lock: tokio::sync::Mutex::new(()),
            title_lock: tokio::sync::Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
            peer_inputs: Mutex::new(HashMap::new()),
            stream: StreamState::default(),
            transfers: Default::default(),
            tuning: Default::default(),
            interactions: Default::default(),
            launch_env: LaunchEnv::default(),
            web: None,
        };
        let mut titles = Default::default();
        let mut order = None;
        macro_rules! tick {
            () => {{
                let _ = sync_order(&host, &mut order).await;
                let _ = sync_titles(&host, &mut titles).await;
            }};
        }
        for _ in 0..3 {
            tick!();
        }
        // A web reorder advances the shared revision.
        central.lock().unwrap().0 = 8;
        for _ in 0..2 {
            tick!();
        }
        let cmux_effects = |calls: &str| -> Vec<String> {
            calls
                .lines()
                .filter(|l| {
                    l.starts_with("reorder-workspaces") || l.starts_with("workspace rename")
                })
                .map(str::to_owned)
                .collect()
        };
        let remote = cmux_effects(&std::fs::read_to_string(&calls).unwrap_or_default());

        // Local gestures: a sidebar rename, then a native drag.
        write_state(
            &["a", "b", "o", "host", "shared"],
            &[("a", "My local name"), ("o", "My manual OpenCode name")],
        );
        tick!();
        write_state(
            &["b", "a", "o", "host", "shared"],
            &[("a", "My local name"), ("o", "My manual OpenCode name")],
        );
        tick!();
        tick!();
        http.abort();
        let all = cmux_effects(&std::fs::read_to_string(&calls).unwrap());
        let published = central.lock().unwrap().1.clone();
        let observed = json!({
            "cmux effects from central": remote.iter().filter(|c| !c.contains("OpenCode automatic")).collect::<Vec<_>>(),
            "cmux effects after local gestures": all[remote.len()..].iter().collect::<Vec<_>>(),
            "local automatic title applied": remote.contains(&"workspace rename o --title=OpenCode automatic".to_owned()),
            "manual rename published": published.iter().any(|r| r.contains(A) && r.contains("My local name")),
            "native order published": published.iter().any(|r| r.starts_with("PUT /api/live/order ") && r.contains(&format!("[\"{B}\",\"{A}\",\"{O}\"]"))),
            "any title published": published.iter().any(|r| r.contains("/title ")),
            "host or shared title published": published.iter().any(|r| [H, S1, S2].iter().any(|id| r.contains(&format!("/{id}/")))),
        });
        assert_eq!(
            observed,
            json!({
                "cmux effects from central": [],
                "cmux effects after local gestures": [],
                "local automatic title applied": true,
                "manual rename published": false,
                "native order published": true,
                "host or shared title published": false,
                "any title published": false,
            }),
            "central writes: {published:#?}"
        );

        // The private channel's SessionsList reads `summary.title` for each
        // session. The cmux sidebar name — manual for A, bridge-automatic for
        // O — must reach the host's local history so hosted and localhost
        // `/private/<id>` tabs show the current effective name without any
        // central-DB upload. Shared and host workspaces never carry a
        // conversation title, so their history stays untouched.
        let history = super::super::history::History::open(root.path()).expect("open history");
        assert_eq!(
            history.host_title(A).unwrap().as_deref(),
            Some("My local name"),
            "A's sidebar rename must surface in the private-channel title"
        );
        assert_eq!(
            history.host_title(O).unwrap().as_deref(),
            Some("My manual OpenCode name"),
            "O's effective sidebar name must surface in the private-channel title"
        );
        for untitled in [B, H, S1, S2] {
            assert!(
                history.host_title(untitled).unwrap().is_none(),
                "{untitled} has no cmux sidebar name — its private title must stay unset"
            );
        }

        // Clear the cmux sidebar name for A; the next tick must remove the
        // host title so the transcript fallback resumes on the private page.
        write_state(
            &["b", "a", "o", "host", "shared"],
            &[("o", "My manual OpenCode name")],
        );
        tick!();
        assert!(
            history.host_title(A).unwrap().is_none(),
            "a cleared sidebar name must clear the private-channel title"
        );

        // Simulate a daemon restart: drop the in-memory title-state map and
        // seed a stale `host_titles` row for B (which carries no cmux sidebar
        // name). The next first-sight tick must clear that row — otherwise a
        // user who cleared their workspace title while the daemon was offline
        // would see the pre-restart private title persist on `/private/<id>`.
        history
            .set_host_title(B, "Stale leftover")
            .expect("seed stale host title before restart");
        assert_eq!(
            history.host_title(B).unwrap().as_deref(),
            Some("Stale leftover"),
            "sanity: stale row must be present before the restart tick"
        );
        titles = Default::default();
        tick!();
        assert!(
            history.host_title(B).unwrap().is_none(),
            "a restart first-sight tick must clear a stale persisted title when cmux reports none"
        );

        // An explicit web rename must survive the next ticks and a restart,
        // even while the OpenCode bridge continues to report its old title.
        commit_owner_rename(&host, O, "Web title", None)
            .await
            .unwrap();
        write_state(&["b", "a", "o", "host", "shared"], &[("o", "Web title")]);
        titles = Default::default();
        let before = cmux_effects(&std::fs::read_to_string(&calls).unwrap()).len();
        tick!();
        tick!();
        let effects = cmux_effects(&std::fs::read_to_string(&calls).unwrap());
        assert_eq!(
            effects.len(),
            before,
            "automatic title must not replace explicit rename: {effects:?}"
        );
        assert_eq!(
            history.summary(O).unwrap().title.as_deref(),
            Some("Web title")
        );
        // Moving the same surface invalidates the old in-memory observation;
        // the explicit title must follow it to the new workspace.
        write_state(
            &["b", "a", "o2", "host", "shared"],
            &[("o2", "OpenCode automatic")],
        );
        let moved_tree = std::fs::read_to_string(&state)
            .unwrap()
            .replace("\"so2\"", "\"so\"");
        std::fs::write(&state, moved_tree).unwrap();
        tick!();
        let effects = cmux_effects(&std::fs::read_to_string(&calls).unwrap());
        assert_eq!(
            effects.last().map(String::as_str),
            Some("workspace rename o2 --title=Web title")
        );
        // A later sidebar gesture wins even in the same second and remains
        // manual after the daemon loses its in-memory title state.
        write_state(
            &["b", "a", "o2", "host", "shared"],
            &[("o2", "Later sidebar title")],
        );
        let moved_tree = std::fs::read_to_string(&state)
            .unwrap()
            .replace("\"so2\"", "\"so\"");
        std::fs::write(&state, moved_tree).unwrap();
        tick!();
        assert_eq!(
            history.summary(O).unwrap().title.as_deref(),
            Some("Later sidebar title")
        );
        titles = Default::default();
        let before = cmux_effects(&std::fs::read_to_string(&calls).unwrap()).len();
        tick!();
        tick!();
        assert_eq!(
            cmux_effects(&std::fs::read_to_string(&calls).unwrap()).len(),
            before
        );
        assert_eq!(
            history.summary(O).unwrap().title.as_deref(),
            Some("Later sidebar title")
        );
    }

    /// `resolve_backend` is the only thing standing between an auto-detected
    /// daemon and a stale CMUX_WORKSPACE_ID attaching it to the wrong
    /// workspace, so each outcome needs a direct proof.
    mod resolve_backend {
        use super::super::*;
        use std::os::unix::fs::PermissionsExt;

        /// Minimal cmux stub: answers `system.ping` RPCs; `--json … tree …`
        /// returns a one-window tree whose workspaces are `workspaces`, so a
        /// test can present or hide the host workspace at will.
        fn stub_cmux(dir: &Path, workspaces: &[&str]) -> PathBuf {
            let ids = workspaces
                .iter()
                .map(|id| format!("{{\"id\":\"{id}\"}}"))
                .collect::<Vec<_>>()
                .join(",");
            let binary = dir.join("cmux");
            std::fs::write(
                &binary,
                format!(
                    "#!/bin/sh\ncase \"$*\" in\n*tree*) printf '{{\"windows\":[{{\"id\":\"w\",\"workspaces\":[{ids}]}}]}}';;\n*) printf '{{}}';;\nesac\n",
                ),
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            binary
        }

        #[tokio::test]
        async fn normal_is_returned_untouched_even_inside_cmux() {
            let dir = tempfile::tempdir().unwrap();
            let binary = stub_cmux(dir.path(), &["host"]);
            let (backend, cmux) = resolve_backend(Backend::Normal, Some(&binary), Some("host"))
                .await
                .unwrap();
            assert_eq!(backend, Backend::Normal);
            assert!(
                cmux.is_none(),
                "explicit normal must not open a cmux client"
            );
        }

        #[tokio::test]
        async fn auto_outside_cmux_selects_normal_without_touching_the_binary() {
            // A missing binary would panic on ping if auto were to connect.
            let (backend, cmux) = resolve_backend(Backend::Auto, None, None).await.unwrap();
            assert_eq!(backend, Backend::Normal);
            assert!(cmux.is_none());
        }

        #[tokio::test]
        async fn auto_empty_workspace_id_is_treated_as_outside_cmux() {
            let dir = tempfile::tempdir().unwrap();
            let binary = stub_cmux(dir.path(), &["host"]);
            let (backend, cmux) = resolve_backend(Backend::Auto, Some(&binary), Some(""))
                .await
                .unwrap();
            assert_eq!(backend, Backend::Normal);
            assert!(cmux.is_none());
        }

        #[tokio::test]
        async fn auto_inside_a_live_cmux_workspace_selects_cmux() {
            let dir = tempfile::tempdir().unwrap();
            let binary = stub_cmux(dir.path(), &["host", "other"]);
            let (backend, cmux) = resolve_backend(Backend::Auto, Some(&binary), Some("HOST"))
                .await
                .unwrap();
            assert_eq!(backend, Backend::Cmux);
            // Case-insensitive match keeps the resolver aligned with `create_workspace`.
            assert!(cmux.is_some());
        }

        #[tokio::test]
        async fn auto_refuses_a_stale_workspace_id_instead_of_attaching_elsewhere() {
            let dir = tempfile::tempdir().unwrap();
            // Only `other` is live: CMUX_WORKSPACE_ID=stale was inherited by a
            // background host whose original workspace was closed, which has
            // bitten root when launching a daemon without context verification.
            let binary = stub_cmux(dir.path(), &["other"]);
            let (backend, cmux) = resolve_backend(Backend::Auto, Some(&binary), Some("stale"))
                .await
                .unwrap();
            assert_eq!(backend, Backend::Normal);
            assert!(cmux.is_none());
        }

        #[tokio::test]
        async fn auto_without_a_reachable_cmux_binary_falls_back_to_normal() {
            // The stub was never written: the ping has nothing to run and
            // errors. The resolver must not propagate that as a daemon failure.
            let dir = tempfile::tempdir().unwrap();
            let missing = dir.path().join("never-installed-cmux");
            let (backend, cmux) = resolve_backend(Backend::Auto, Some(&missing), Some("host"))
                .await
                .unwrap();
            assert_eq!(backend, Backend::Normal);
            assert!(cmux.is_none());
        }
    }
}
