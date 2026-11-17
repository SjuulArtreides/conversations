//! Behavioural tests for the host's execution authority. They drive the real
//! host entry points against a recording fake `cmux` process and observe
//! launches (`workspace.create` calls). Relay requests use strict decoding and
//! the current relay handler. Private operations use [`decrypted_channel_request`],
//! the dispatcher called after channel authentication and decryption. Transport
//! binding and encryption have separate production-path coverage.

use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn status_reports_the_hosts_native_identity_for_live_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(dir.path());
    let host = Arc::new(fixture.host(fixture.server()));
    let id = uuid::Uuid::new_v4().to_string();
    let native = uuid::Uuid::new_v4().to_string();
    let mut store = authz::store::Store::open(dir.path()).unwrap();
    store.bind_native(&id, &native).unwrap();
    let (commands, _receiver) = mpsc::channel(8);
    host.sessions.lock().unwrap().insert(
        id.clone(),
        Session {
            screen: None,
            readiness: Default::default(),
            local_input_bytes: 0,
            local_input_dropped_chunks: 0,
            registration: Registration {
                launch: Launch {
                    id: id.clone(),
                    agent: conversations_core::AgentKind::Codex,
                    cwd: dir.path().to_owned(),
                    command: vec![],
                },
                workspace: None,
                surface: None,
                local_terminal: true,
                reconnecting: false,
                file_receiver: false,
                transfer_gate: false,
                effect_gate: false,
                in_place: false,
            },
            resume_published: None,
            opencode_title_published: None,
            commands,
            pid: None,
        },
    );
    let (mut client, socket) = tokio::net::UnixStream::pair().unwrap();
    let serving = tokio::spawn(handle_client(host, socket));
    write_message(&mut client, &ClientMessage::Status)
        .await
        .unwrap();
    let response: ServerMessage = read_message(&mut client).await.unwrap();
    let ServerMessage::Status { sessions, .. } = response else {
        panic!("expected status")
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].registration.launch.id, id);
    assert!(sessions[0].native_identity_supported);
    assert_eq!(
        sessions[0].native_session_id.as_deref(),
        Some(native.as_str())
    );
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn status_store_failure_is_returned_to_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(dir.path());
    drop(authz::store::Store::open(dir.path()).unwrap());
    let db = rusqlite::Connection::open(dir.path().join("authz.db")).unwrap();
    db.pragma_update(None, "user_version", 999).unwrap();
    let host = Arc::new(fixture.host(fixture.server()));
    let (mut client, socket) = tokio::net::UnixStream::pair().unwrap();
    let serving = tokio::spawn(handle_client(host, socket));
    write_message(&mut client, &ClientMessage::Status)
        .await
        .unwrap();
    let response: ServerMessage = read_message(&mut client).await.unwrap();
    let ServerMessage::Error { message } = response else {
        panic!("expected an explicit error")
    };
    assert!(message.contains("schema 999"), "{message}");
    serving.await.unwrap().unwrap();
}

/// Re-run `name` in a child process with an isolated HOME/config so launch
/// checkpoints and settings never touch the developer's real files.
/// Returns true when the caller is that child and must run the test body.
pub(super) fn isolated(name: &str) -> bool {
    crate::commands::local_host::test_support::isolated(
        &format!("commands::local_host::daemon::authz_tests::{name}"),
        &[],
    )
}

/// A host runtime directory plus a recording fake `cmux`. Each `run_control`
/// builds a FRESH `Host` over the same directory, i.e. a host restart.
pub(super) struct Fixture {
    pub dir: PathBuf,
    binary: PathBuf,
    log: PathBuf,
    /// Stable across `run_control` calls (and processes) so checkpoints, which
    /// are keyed by server URL, are visible to a "restarted" host.
    port: u16,
}

impl Fixture {
    pub fn new(dir: &Path) -> Self {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let log = dir.join("cmux.log");
        let binary = dir.join("fake-cmux");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{}'\ncase \"$*\" in\n  *'tree --all'*) echo '{{\"windows\":[{{\"id\":\"w1\",\"workspaces\":[{{\"id\":\"host\"}}]}}]}}';;\n  'rpc workspace.create'*) echo '{{\"ok\":true,\"result\":{{\"workspace_id\":\"ws1\"}}}}';;\n  *) echo '{{}}';;\nesac\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let port = std::env::var("AUTHZ_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| {
                std::net::TcpListener::bind("127.0.0.1:0")
                    .unwrap()
                    .local_addr()
                    .unwrap()
                    .port()
            });
        Self {
            dir: dir.to_path_buf(),
            binary,
            log,
            port,
        }
    }

    pub fn server(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn host_without_cmux(&self) -> Host {
        let mut host = self.host(self.server());
        host.cmux = None;
        host
    }

    fn host(&self, server: String) -> Host {
        Host {
            dir: self.dir.clone(),
            cwd: self.dir.clone(),
            server,
            client: reqwest::Client::new(),
            cookie: None,
            availability: crate::commands::live::availability::AvailabilitySignal::new(),
            backend: Backend::Cmux,
            cmux: Some(Cmux::test_backend(self.binary.clone())),
            sessions: Mutex::new(HashMap::new()),
            registration_lock: tokio::sync::Mutex::new(()),
            title_lock: tokio::sync::Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
            peer_inputs: Mutex::new(HashMap::new()),
            interactions: Interactions::default(),
            stream: super::StreamState::default(),
            transfers: Default::default(),
            tuning: Default::default(),
            launch_env: super::LaunchEnv::default(),
            web: None,
        }
    }

    /// Terminals the host asked cmux to create.
    pub fn launches(&self) -> usize {
        self.launch_log()
            .lines()
            .filter(|line| line.starts_with("rpc workspace.create"))
            .count()
    }

    pub fn launch_log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Where the fake cmux binary records its argv. Tests that
    /// `rewrite_binary` can point their replacement script at the same file.
    pub fn launch_log_path(&self) -> &Path {
        &self.log
    }

    /// Replace the recording fake cmux binary with `script`. Lets individual
    /// tests teach the mock to return a richer tree (`tree --all`) than the
    /// baseline `workspace.create`-only script, so RPCs like
    /// `surface_workspaces()` can resolve concrete workspaces.
    pub fn rewrite_binary(&self, script: &str) {
        std::fs::write(&self.binary, script).unwrap();
        std::fs::set_permissions(&self.binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// Decode wire requests using the relay's strict schema and invoke the real
    /// relay handler. Invalid/unsigned requests cannot reach the dispatcher.
    /// Terminal registration is simulated so admitted launches finish promptly.
    pub async fn run_control(&self, frames: &[String]) -> Result<()> {
        self.run_control_on(&self.host(self.server()), frames).await
    }

    pub async fn run_control_on(&self, host: &Host, frames: &[String]) -> Result<()> {
        let consume = async {
            for frame in frames {
                if let Ok(request) =
                    serde_json::from_str::<conversations_core::host_relay::RelayRequest>(frame)
                {
                    let _ = super::relay::respond(host, request).await;
                }
            }
            Ok(())
        };
        tokio::pin!(consume);
        let result = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                tokio::select! {
                    outcome = &mut consume => return outcome,
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        let ids: Vec<String> = host.pending.lock().unwrap().keys().cloned().collect();
                        for id in ids {
                            if let Some(tx) = host.pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(());
                            }
                        }
                    }
                }
            }
        })
        .await;
        result.unwrap_or_else(|_| Err(anyhow::anyhow!("relay dispatch timed out")))
    }

    /// Hand each signed frame (the wire form `test_signing::wire` produces) to
    /// [`decrypted_channel_request`] on a FRESH host, in order, and return what
    /// each one got. A frame that does not decode is dropped, as every transport
    /// drops it before dispatch. Terminal registration is simulated as in
    /// `run_control_on`.
    pub async fn run_private(
        &self,
        frames: &[String],
    ) -> Result<Vec<(conversations_core::host_relay::OpStatus, Option<String>)>> {
        self.run_private_on(&self.host(self.server()), frames).await
    }

    pub async fn run_private_on(
        &self,
        host: &Host,
        frames: &[String],
    ) -> Result<Vec<(conversations_core::host_relay::OpStatus, Option<String>)>> {
        let deliver = async {
            let mut outcomes = Vec::new();
            for frame in frames {
                if let Ok(conversations_core::host_relay::RelayRequest::SignedOp { p, s, k }) =
                    serde_json::from_str(frame)
                {
                    if let Some(frame) = SignedFrame::from_wire(&p, &s, &k) {
                        outcomes.push(decrypted_channel_request(host, frame).await);
                    }
                }
            }
            outcomes
        };
        tokio::pin!(deliver);
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                tokio::select! {
                    outcomes = &mut deliver => return outcomes,
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        let ids: Vec<String> = host.pending.lock().unwrap().keys().cloned().collect();
                        for id in ids {
                            if let Some(tx) = host.pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(());
                            }
                        }
                    }
                }
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("private dispatch timed out"))
    }
}

/// A request that has ALREADY arrived on the private channel: authenticated to
/// its signer's paired key and decrypted. `execute_signed` is the call
/// `private_channel::answer` makes at that point, so a test through here
/// exercises the verifier, journal, grants and effect adapters behind the
/// channel. It does NOT exercise WebRTC, DTLS, TURN, the channel's
/// key-to-connection binding or its signed reply; that transport has its own
/// end-to-end proof (real central, host and Chromium) in `tests/private_transport.rs`.
pub(super) async fn decrypted_channel_request(
    host: &Host,
    frame: SignedFrame,
) -> (conversations_core::host_relay::OpStatus, Option<String>) {
    execute_signed(host, frame).await
}

/// Hand a signed frame to the host entry point of the transport the current
/// contract assigns its operation: every private operation
/// (`envelope::channel_only`: input, replies, transfers, starts, reopens, PR
/// actions, settings, terminal controls, grants, publication, agent messages
/// and their status questions) to [`decrypted_channel_request`]; everything
/// else to `dispatch_signed`, the relay entry point.
pub(super) async fn deliver_signed(
    host: &Host,
    frame: SignedFrame,
) -> (conversations_core::host_relay::OpStatus, Option<String>) {
    if authz::envelope::claimed_operation(&frame.payload)
        .is_some_and(|op| authz::envelope::channel_only(&op))
    {
        decrypted_channel_request(host, frame).await
    } else {
        dispatch_signed(host, frame).await
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

#[test]
fn unsigned_start_and_reopen_frames_launch_nothing() {
    if !isolated("unsigned_start_and_reopen_frames_launch_nothing") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(dir.path());
    let id = uuid::Uuid::new_v4().to_string();
    let frames = vec![
        // what a (compromised) server sends today: a plain account-authorized start
        json!({"type":"start_new","cwd":null,"agent":"zsh","agent_args":["-c","echo owned"]})
            .to_string(),
        // reopen in a server-chosen directory
        json!({"type":"reopen","session_id":id,"cwd":"/tmp","agent":"zsh"}).to_string(),
    ];
    runtime().block_on(fixture.run_control(&frames)).unwrap();
    assert_eq!(
        fixture.launches(),
        0,
        "unsigned control frames must not create terminals; cmux saw:\n{}",
        fixture.launch_log()
    );
    assert!(
        crate::open_sessions::try_load_for("http://x")
            .unwrap()
            .is_empty(),
        "no launch checkpoint"
    );
}

mod signed {
    use super::*;
    use crate::commands::local_host::authz::{
        self,
        envelope::test_signing::{wire, Signer},
        store::now_secs,
    };
    use base64::engine::general_purpose::STANDARD as B64;

    const SESSION: &str = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";
    const JTI: &str = "0123456789abcdef0123456789abcdef";

    fn enroll(dir: &Path, kind: &str, signer: &Signer, grant: &str) -> String {
        authz::enroll_from_cli(
            dir,
            kind,
            "ed25519",
            &B64.encode(&signer.public),
            "t",
            &[grant.into()],
        )
        .map(|(_, host)| host)
        .unwrap()
    }

    fn frame(signer: &Signer, host: &str, session: &str, agent: &str, jti: &str) -> String {
        let now = now_secs();
        let payload = signer.start_payload(host, session, agent, jti, now, now + 60);
        wire(&signer.sign_bytes(payload.as_bytes()))
    }

    #[test]
    fn signed_rename_requires_owner_write_and_preserves_private_transport_and_replay() {
        if !isolated(
            "signed::signed_rename_requires_owner_write_and_preserves_private_transport_and_replay",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let owner = Signer::new();
        let agent = Signer::new();
        let host = enroll(
            dir.path(),
            "owner",
            &owner,
            &format!("session.write:{SESSION}"),
        );
        enroll(
            dir.path(),
            "agent",
            &agent,
            &format!("session.write:{SESSION}"),
        );
        authz::store::Store::open(dir.path())
            .unwrap()
            .insert_session(
                &authz::sessions::LocalSession {
                    session_id: SESSION.into(),
                    agent: conversations_core::AgentKind::Codex,
                    cwd: "/project".into(),
                    resume_id: None,
                    launcher: Default::default(),
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        let rename = |signer: &Signer, title: &str, n: u32| {
            let now = now_secs();
            let args = serde_json::json!({"session":SESSION,"title":title});
            wire(
                &signer.sign_bytes(
                    signer
                        .op_payload(
                            &host,
                            "session.rename",
                            SESSION,
                            &args.to_string(),
                            &format!("{n:032x}"),
                            now,
                            now + 60,
                        )
                        .as_bytes(),
                ),
            )
        };
        let denied = runtime()
            .block_on(fixture.run_private(&[rename(&agent, "Agent overwrite", 1)]))
            .unwrap();
        assert_eq!(
            denied[0].0,
            conversations_core::host_relay::OpStatus::Refused
        );
        let good = rename(&owner, "My title", 2);
        runtime()
            .block_on(fixture.run_control(std::slice::from_ref(&good)))
            .unwrap();
        assert!(
            crate::commands::local_host::history::History::open(dir.path())
                .unwrap()
                .summary(SESSION)
                .unwrap()
                .title
                .is_none()
        );
        let outcomes = runtime()
            .block_on(fixture.run_private(&[good.clone(), good]))
            .unwrap();
        assert_eq!(
            outcomes[0].0,
            conversations_core::host_relay::OpStatus::Executed
        );
        assert_eq!(outcomes[0].1.as_deref(), Some("My title"));
        assert_eq!(
            outcomes[1].0,
            conversations_core::host_relay::OpStatus::Replayed
        );
        assert_eq!(
            crate::commands::local_host::history::History::open(dir.path())
                .unwrap()
                .summary(SESSION)
                .unwrap()
                .title
                .as_deref(),
            Some("My title")
        );
        let reset = runtime()
            .block_on(fixture.run_private(&[rename(&owner, "  ", 3)]))
            .unwrap();
        assert_eq!(
            reset[0].0,
            conversations_core::host_relay::OpStatus::Executed
        );
        assert!(
            crate::commands::local_host::history::History::open(dir.path())
                .unwrap()
                .summary(SESSION)
                .unwrap()
                .title
                .is_none()
        );
    }

    /// Regression for the frontend → cmux rename gap AND for the two
    /// invariants `sync_titles` already protects:
    ///   - The signed rename pushes to the session's CURRENT workspace
    ///     (resolved from `registration.surface` via `cmux.surface_workspaces`),
    ///     not the cached `registration.workspace` that lags between title
    ///     ticks.
    ///   - A workspace shared with another registered session, and a surface
    ///     the current cmux tree no longer knows about, both leave the
    ///     sidebar alone — a shared sidebar cannot represent two
    ///     conversations, and a guess at the previous workspace would rename
    ///     the wrong one.
    ///   - A session that is imported/stopped (not in `host.sessions`), a
    ///     session registered without a surface, and a cleared rename all
    ///     commit the DB override without a cmux call.
    #[test]
    fn signed_rename_resolves_current_workspace_and_refuses_shared_or_detached() {
        if !isolated(
            "signed::signed_rename_resolves_current_workspace_and_refuses_shared_or_detached",
        ) {
            return;
        }
        const MOVED: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        const SHARED_A: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        const SHARED_B: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        const DETACHED: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        const IMPORTED: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        // Teach the mock cmux to answer `tree --all` with a chosen layout so
        // `surface_workspaces()` returns real entries. `cat` is not on the
        // isolated PATH, so the JSON is embedded in the script — `echo` is a
        // POSIX shell builtin. The tree keeps alphanumeric+hyphen names so no
        // single quote escaping is needed inside the heredoc.
        let install_tree = |panes: &[(&str, &[&str])]| {
            let workspaces: Vec<_> = panes
                .iter()
                .map(|(ws, surfaces)| {
                    let surfaces: Vec<_> = surfaces
                        .iter()
                        .map(|surface| serde_json::json!({"id": surface}))
                        .collect();
                    serde_json::json!({
                        "id": ws,
                        "pinned": false,
                        "panes": [{"surfaces": surfaces}],
                    })
                })
                .collect();
            let tree = serde_json::json!({
                "windows": [{"id": "w1", "workspaces": workspaces}],
            })
            .to_string();
            assert!(
                !tree.contains('\''),
                "tree JSON must not contain single quotes"
            );
            let log = fixture.launch_log_path().display().to_string();
            assert!(!log.contains('\''), "log path must not contain quotes");
            fixture.rewrite_binary(&format!(
                "#!/bin/sh\necho \"$@\" >> '{log}'\ncase \"$*\" in\n  *'tree --all'*) echo '{tree}';;\n  *) echo '{{}}';;\nesac\n"
            ));
        };
        // MOVED's surface now lives in `ws-new` (the registration below still
        // claims `ws-stale`, which must NOT be the rename target).
        // SHARED_A and SHARED_B share `ws-shared`.
        // DETACHED's surface is absent from the tree altogether.
        install_tree(&[
            ("ws-new", &["s-moved"]),
            ("ws-shared", &["s-shared-a", "s-shared-b"]),
            ("ws-ghost", &[]),
        ]);
        let owner = Signer::new();
        let host_id = authz::enroll_from_cli(
            dir.path(),
            "owner",
            "ed25519",
            &B64.encode(&owner.public),
            "t",
            &[
                format!("session.write:{MOVED}"),
                format!("session.write:{SHARED_A}"),
                format!("session.write:{SHARED_B}"),
                format!("session.write:{DETACHED}"),
                format!("session.write:{IMPORTED}"),
            ],
        )
        .map(|(_, host)| host)
        .unwrap();
        let mut store = authz::store::Store::open(dir.path()).unwrap();
        for id in [MOVED, SHARED_A, SHARED_B, DETACHED, IMPORTED] {
            store
                .insert_session(
                    &authz::sessions::LocalSession {
                        session_id: id.into(),
                        agent: conversations_core::AgentKind::Claude,
                        cwd: "/w".into(),
                        resume_id: None,
                        launcher: Default::default(),
                        adk_statusline: false,
                    },
                    "adopted",
                )
                .unwrap();
        }
        drop(store);
        let host = fixture.host(fixture.server());
        let register = |id: &str, workspace: Option<&str>, surface: Option<&str>| {
            host.sessions.lock().unwrap().insert(
                id.into(),
                Session {
                    readiness: Default::default(),
                    local_input_bytes: 0,
                    local_input_dropped_chunks: 0,
                    registration: Registration {
                        launch: Launch {
                            id: id.into(),
                            agent: conversations_core::AgentKind::Claude,
                            cwd: "/w".into(),
                            command: vec!["claude".into()],
                        },
                        workspace: workspace.map(str::to_owned),
                        surface: surface.map(str::to_owned),
                        local_terminal: true,
                        reconnecting: false,
                        file_receiver: false,
                        transfer_gate: false,
                        effect_gate: true,
                        in_place: false,
                    },
                    resume_published: None,
                    opencode_title_published: None,
                    commands: mpsc::channel(1).0,
                    pid: None,
                    screen: None,
                },
            );
        };
        // Stale `workspace` on MOVED: the registration was last reconciled
        // when the surface lived in `ws-stale`. A correct rename must target
        // the CURRENT workspace the surface resolves to today (`ws-new`),
        // never the stale value.
        register(MOVED, Some("ws-stale"), Some("s-moved"));
        register(SHARED_A, Some("ws-shared"), Some("s-shared-a"));
        register(SHARED_B, Some("ws-shared"), Some("s-shared-b"));
        register(DETACHED, Some("ws-detached"), Some("s-nowhere"));
        // IMPORTED is NOT inserted into host.sessions — it stands for an
        // imported or stopped session that still exists in the DB.
        let mut counter: u32 = 0;
        let rename = |session: &str, title: &str, counter: &mut u32| {
            *counter += 1;
            let args = serde_json::json!({"session": session, "title": title});
            let now = now_secs();
            wire(
                &owner.sign_bytes(
                    owner
                        .op_payload(
                            &host_id,
                            "session.rename",
                            session,
                            &args.to_string(),
                            &format!("{:032x}", *counter),
                            now,
                            now + 60,
                        )
                        .as_bytes(),
                ),
            )
        };
        let rename_calls = || -> Vec<String> {
            fixture
                .launch_log()
                .lines()
                .filter(|line| line.starts_with("workspace rename "))
                .map(str::to_owned)
                .collect()
        };
        let db_title = |session: &str| {
            crate::commands::local_host::history::History::open(dir.path())
                .unwrap()
                .summary(session)
                .unwrap()
                .title
        };
        // Submit all renames; a single dispatch per frame.
        let dispatch = |frame: String| {
            let out = runtime()
                .block_on(fixture.run_private_on(&host, std::slice::from_ref(&frame)))
                .unwrap();
            assert_eq!(
                out[0].0,
                conversations_core::host_relay::OpStatus::Executed,
                "signed rename executes"
            );
        };
        dispatch(rename(MOVED, "Moved-ws title", &mut counter));
        dispatch(rename(SHARED_A, "Shared title", &mut counter));
        dispatch(rename(DETACHED, "Detached title", &mut counter));
        dispatch(rename(IMPORTED, "Imported title", &mut counter));
        // DB commits: every session recorded its override regardless of cmux.
        assert_eq!(db_title(MOVED).as_deref(), Some("Moved-ws title"));
        assert_eq!(db_title(SHARED_A).as_deref(), Some("Shared title"));
        assert_eq!(db_title(DETACHED).as_deref(), Some("Detached title"));
        assert_eq!(db_title(IMPORTED).as_deref(), Some("Imported title"));
        // Only MOVED reached cmux, and it targeted the surface's CURRENT
        // workspace (`ws-new`), not the stale `ws-stale` the registration
        // still carried.
        assert_eq!(
            rename_calls(),
            vec!["workspace rename ws-new --title=Moved-ws title".to_owned()],
            "exactly one cmux rename, against the current workspace"
        );
        // A registered session with only a workspace (no surface) still
        // occupies that workspace and must block an explicit mirror.
        register("ffffffff-ffff-4fff-8fff-ffffffffffff", Some("ws-new"), None);
        dispatch(rename(
            MOVED,
            "Shared with surface-less session",
            &mut counter,
        ));
        assert_eq!(
            rename_calls().len(),
            1,
            "surface-less occupant must block rename"
        );

        // Cleared rename on MOVED: DB override drops, the sidebar is left
        // alone (automatic or earlier manual names survive).
        dispatch(rename(MOVED, "   ", &mut counter));
        assert!(db_title(MOVED).is_none(), "cleared override drops DB row");
        assert_eq!(
            rename_calls(),
            vec!["workspace rename ws-new --title=Moved-ws title".to_owned()],
            "a cleared rename must not push an empty title into cmux"
        );
    }

    /// A normal (non-cmux) host must not crash on a signed rename and must
    /// still commit the DB override: the cmux mirror is best-effort, the DB
    /// commit is the real rename.
    #[test]
    fn signed_rename_on_a_host_without_cmux_still_commits_the_db_override() {
        if !isolated("signed::signed_rename_on_a_host_without_cmux_still_commits_the_db_override") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let owner = Signer::new();
        let host_id = enroll(
            dir.path(),
            "owner",
            &owner,
            &format!("session.write:{SESSION}"),
        );
        authz::store::Store::open(dir.path())
            .unwrap()
            .insert_session(
                &authz::sessions::LocalSession {
                    session_id: SESSION.into(),
                    agent: conversations_core::AgentKind::Codex,
                    cwd: "/project".into(),
                    resume_id: None,
                    launcher: Default::default(),
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        let host = fixture.host_without_cmux();
        let args = serde_json::json!({"session": SESSION, "title": "No cmux here"});
        let now = now_secs();
        let frame = wire(
            &owner.sign_bytes(
                owner
                    .op_payload(
                        &host_id,
                        "session.rename",
                        SESSION,
                        &args.to_string(),
                        "0123456789abcdef0123456789abcde0",
                        now,
                        now + 60,
                    )
                    .as_bytes(),
            ),
        );
        let out = runtime()
            .block_on(fixture.run_private_on(&host, std::slice::from_ref(&frame)))
            .unwrap();
        assert_eq!(out[0].0, conversations_core::host_relay::OpStatus::Executed);
        assert_eq!(out[0].1.as_deref(), Some("No cmux here"));
        assert_eq!(
            crate::commands::local_host::history::History::open(dir.path())
                .unwrap()
                .summary(SESSION)
                .unwrap()
                .title
                .as_deref(),
            Some("No cmux here")
        );
        // The fake cmux binary was never invoked — no workspace rename line in
        // the launch log.
        assert!(
            !fixture.launch_log().contains("workspace rename "),
            "no cmux backend = no cmux rename call ever reaches the recorder"
        );
    }

    #[test]
    fn signed_owner_start_launches_once_and_survives_restart_and_replay() {
        if !isolated("signed::signed_owner_start_launches_once_and_survives_restart_and_replay") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let owner = Signer::new();
        let host = enroll(dir.path(), "owner", &owner, "session.start");
        let good = frame(&owner, &host, SESSION, "claude", JTI);
        // The central relay may not carry a start: refused before
        // admission, so the frame is not spent and nothing launches.
        runtime()
            .block_on(fixture.run_control(std::slice::from_ref(&good)))
            .unwrap();
        assert_eq!(fixture.launches(), 0, "{}", fixture.launch_log());
        // same frame twice on the private channel: replay in the same process
        let outcomes = runtime()
            .block_on(fixture.run_private(&[good.clone(), good.clone()]))
            .unwrap();
        assert_eq!(
            outcomes.iter().map(|o| o.0).collect::<Vec<_>>(),
            [
                conversations_core::host_relay::OpStatus::Executed,
                conversations_core::host_relay::OpStatus::Replayed
            ]
        );
        assert_eq!(fixture.launches(), 1, "{}", fixture.launch_log());
        // fresh Host (restart) re-delivering the frame: still no second effect
        runtime().block_on(fixture.run_private(&[good])).unwrap();
        assert_eq!(fixture.launches(), 1, "{}", fixture.launch_log());
    }

    #[test]
    fn tampered_wrong_audience_and_unauthorized_frames_launch_nothing() {
        if !isolated("signed::tampered_wrong_audience_and_unauthorized_frames_launch_nothing") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let owner = Signer::new();
        let agent = Signer::new();
        let host = enroll(dir.path(), "owner", &owner, "session.start");
        authz::enroll_from_cli(
            dir.path(),
            "agent",
            "ed25519",
            &B64.encode(&agent.public),
            "a",
            &[format!("session.write:{SESSION}")],
        )
        .unwrap();
        let now = now_secs();
        let mut frames = Vec::new();
        // changed args after signing (agent swapped), bad signature
        let good = frame(&owner, &host, SESSION, "claude", JTI);
        frames.push(good.replace("\"s\":\"", "\"s\":\"AAAA").to_string());
        let mut value: serde_json::Value = serde_json::from_str(&good).unwrap();
        let mut sig = value["s"].as_str().unwrap().to_owned();
        sig.replace_range(0..1, if sig.starts_with('A') { "B" } else { "A" });
        value["s"] = sig.into();
        frames.push(value.to_string());
        // wrong audience, known agent key, unknown key, bad expiry
        frames.push(frame(
            &owner,
            "WRONGHOST",
            SESSION,
            "claude",
            "11111111111111111111111111111111",
        ));
        frames.push(frame(
            &agent,
            &host,
            SESSION,
            "claude",
            "22222222222222222222222222222222",
        ));
        frames.push(frame(
            &Signer::new(),
            &host,
            SESSION,
            "claude",
            "33333333333333333333333333333333",
        ));
        for (iat, exp) in [(i64::MIN, i64::MAX), (now, now - 1), (now, now + 100_000)] {
            let p = owner.start_payload(
                &host,
                SESSION,
                "claude",
                "44444444444444444444444444444444",
                iat,
                exp,
            );
            frames.push(wire(&owner.sign_bytes(p.as_bytes())));
        }
        runtime().block_on(fixture.run_private(&frames)).unwrap();
        assert_eq!(fixture.launches(), 0, "{}", fixture.launch_log());
    }

    #[test]
    fn interrupted_start_cannot_be_relaunched_by_checkpoint_restore() {
        let name = "signed::interrupted_start_cannot_be_relaunched_by_checkpoint_restore";
        if !isolated(name) {
            return;
        }
        let dir = std::path::PathBuf::from(std::env::var("AUTHZ_DIR").unwrap_or_else(|_| {
            tempfile::tempdir()
                .unwrap()
                .keep()
                .to_string_lossy()
                .into_owned()
        }));
        if std::env::var("AUTHZ_PHASE").is_ok() {
            // The doomed host: stops itself (exit 86) right after the
            // checkpoint is saved and before the terminal is created.
            let fixture = Fixture::new(&dir);
            let frame = std::env::var("AUTHZ_FRAME").unwrap();
            let _ = runtime().block_on(fixture.run_private(&[frame]));
            std::process::exit(0); // reaching this means the crash point never fired
        }
        let fixture = Fixture::new(&dir);
        let owner = Signer::new();
        let host = enroll(&dir, "owner", &owner, "session.start");
        let frame = frame(&owner, &host, SESSION, "claude", JTI);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("commands::local_host::daemon::authz_tests::{name}"),
                "--nocapture",
            ])
            .env("AUTHZ_PHASE", "crash")
            .env("AUTHZ_DIR", &dir)
            .env("AUTHZ_PORT", fixture.port.to_string())
            .env("AUTHZ_FRAME", &frame)
            .env(
                "CONVERSATIONS_HOST_CRASH_AT",
                "signed_start_after_checkpoint",
            )
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(86),
            "host did not stop at the crash point"
        );
        assert_eq!(
            fixture.launches(),
            0,
            "crash point is before the terminal is created"
        );
        let server = fixture.server();
        assert_eq!(
            authz::sessions::restore(&dir, &server).unwrap().len(),
            1,
            "the interrupted start left a restore checkpoint"
        );

        // Restarted host: recovery marks the dispatch uncertain.
        assert_eq!(authz::recover(&dir).unwrap(), 1);
        assert!(!authz::restorable(&dir, SESSION));
        // Redelivery and a fresh operation id for the same session both do nothing.
        let again = frame_with(&owner, &dir, "fedcba9876543210fedcba9876543210");
        runtime()
            .block_on(fixture.run_private(&[frame.clone(), again]))
            .unwrap();
        assert_eq!(fixture.launches(), 0, "{}", fixture.launch_log());
        // Startup checkpoint restore on a host that launches directly (no cmux)
        // must skip it rather than create the terminal.
        let direct = Arc::new(fixture.host_without_cmux());
        let finished = runtime().block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                restore_saved_sessions_once(direct.clone(), true),
            )
            .await
        });
        assert!(finished.is_ok());
        assert!(direct.pending.lock().unwrap().is_empty());
        assert!(direct.sessions.lock().unwrap().is_empty());
        assert_eq!(
            authz::sessions::restore(&dir, &server).unwrap().len(),
            1,
            "checkpoint is kept, not consumed"
        );
    }

    fn frame_with(owner: &Signer, dir: &Path, jti: &str) -> String {
        let host = authz::host_id(dir).unwrap();
        frame(owner, &host, SESSION, "claude", jti)
    }

    #[test]
    fn completed_start_stays_restorable_and_session_ids_cannot_collide() {
        let name = "signed::completed_start_stays_restorable_and_session_ids_cannot_collide";
        if !isolated(name) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let owner = Signer::new();
        let host = enroll(dir.path(), "owner", &owner, "session.start");
        let first = frame(&owner, &host, SESSION, "claude", JTI);
        runtime().block_on(fixture.run_private(&[first])).unwrap();
        assert_eq!(fixture.launches(), 1);
        assert!(
            authz::restorable(dir.path(), SESSION),
            "settled start keeps auto-resume"
        );
        let before = authz::sessions::restore(dir.path(), &fixture.server()).unwrap();
        assert_eq!(before.len(), 1);

        // Same session id under a new operation id: no second launch, checkpoint intact.
        let second = frame_with(&owner, dir.path(), "fedcba9876543210fedcba9876543210");
        runtime().block_on(fixture.run_private(&[second])).unwrap();
        assert_eq!(fixture.launches(), 1, "{}", fixture.launch_log());
        assert_eq!(
            authz::sessions::restore(dir.path(), &fixture.server()).unwrap(),
            before
        );

        // A pre-existing native checkpoint (never in the journal) is not overwritten.
        let other = "5e1aeb93-5aba-419e-8856-a7c721406279";
        let mut native = before[0].clone();
        native.session_id = other.into();
        native.resume_id = Some("native-resume-id".into());
        save_checkpoint(dir.path(), &fixture.server(), native.clone()).unwrap();
        let collide = frame(
            &owner,
            &host,
            other,
            "claude",
            "00112233445566778899aabbccddeeff",
        );
        runtime().block_on(fixture.run_private(&[collide])).unwrap();
        assert_eq!(fixture.launches(), 1);
        assert!(authz::sessions::restore(dir.path(), &fixture.server())
            .unwrap()
            .contains(&native));
    }

    fn enroll_with(dir: &Path, kind: &str, signer: &Signer, grants: &[&str]) -> String {
        authz::enroll_from_cli(
            dir,
            kind,
            "ed25519",
            &B64.encode(&signer.public),
            "t",
            &grants.iter().map(|g| g.to_string()).collect::<Vec<_>>(),
        )
        .map(|(_, host)| host)
        .unwrap()
    }

    fn reopen_frame(signer: &Signer, host: &str, session: &str, jti: &str) -> String {
        let now = now_secs();
        let payload = signer.reopen_payload(host, session, jti, now, now + 60);
        wire(&signer.sign_bytes(payload.as_bytes()))
    }

    fn workspace_creates(fixture: &Fixture) -> Vec<String> {
        fixture
            .launch_log()
            .lines()
            .filter(|line| line.starts_with("rpc workspace.create"))
            .map(str::to_owned)
            .collect()
    }

    fn write_settings(json: &str) {
        std::fs::write(
            std::env::var("CONVERSATIONS_HOST_SETTINGS_FILE").unwrap(),
            json,
        )
        .unwrap();
    }

    #[test]
    fn signed_reopen_resumes_the_local_binding_whatever_the_carrier_adds() {
        let name = "signed::signed_reopen_resumes_the_local_binding_whatever_the_carrier_adds";
        if !isolated(name) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let owner = Signer::new();
        let host = enroll_with(
            dir.path(),
            "owner",
            &owner,
            &["session.start", "session.reopen"],
        );
        runtime()
            .block_on(fixture.run_private(&[frame(&owner, &host, SESSION, "claude", JTI)]))
            .unwrap();
        assert_eq!(fixture.launches(), 1);
        let record = authz::lookup(dir.path(), SESSION)
            .unwrap()
            .expect("local session record");
        assert_eq!(record.cwd, dir.path().to_string_lossy());
        assert_eq!(record.resume_id.as_deref(), Some(SESSION));
        let mut checkpoint_before =
            authz::sessions::restore(dir.path(), &fixture.server()).unwrap();

        // This host's own settings change; no central setting exists to consult.
        write_settings(r#"{"launch":{"claude_skip_permissions":true}}"#);
        checkpoint_before[0].agent_args = vec!["--dangerously-skip-permissions".into()];

        // A carrier decorates a genuine signed frame with cwd/resume/args
        // outside the signature: rejected by strict request decoding.
        let genuine = reopen_frame(&owner, &host, SESSION, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mut decorated: serde_json::Value = serde_json::from_str(&genuine).unwrap();
        decorated["cwd"] = "/tmp/evil".into();
        decorated["resume_id"] = "evil-resume".into();
        decorated["agent_args"] = serde_json::json!(["--evil"]);
        // A signed payload that itself carries the extras is rejected outright.
        let now = now_secs();
        let smuggled = owner
            .reopen_payload(
                &host,
                SESSION,
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                now,
                now + 60,
            )
            .replace(
                &format!("\"args\":{{\"session\":\"{SESSION}\""),
                &format!("\"args\":{{\"session\":\"{SESSION}\",\"cwd\":\"/tmp/evil\""),
            );
        let smuggled = wire(&owner.sign_bytes(smuggled.as_bytes()));
        runtime()
            .block_on(fixture.run_private(&[smuggled, decorated.to_string()]))
            .unwrap();
        assert_eq!(
            workspace_creates(&fixture).len(),
            1,
            "untrusted fields must not reopen a session"
        );
        runtime()
            .block_on(fixture.run_private(std::slice::from_ref(&genuine)))
            .unwrap();
        let creates = workspace_creates(&fixture);
        assert_eq!(creates.len(), 2, "one start, one reopen: {creates:#?}");
        let reopen = &creates[1];
        assert!(
            reopen.contains(&format!("--resume {SESSION}"))
                || reopen.contains(&format!("'--resume' '{SESSION}'")),
            "{reopen}"
        );
        assert!(
            reopen.contains("dangerously-skip-permissions"),
            "local settings apply: {reopen}"
        );
        assert!(reopen.contains(&*dir.path().to_string_lossy()), "{reopen}");
        for forbidden in ["evil", "/tmp/evil"] {
            assert!(!reopen.contains(forbidden), "{reopen}");
        }

        // Replay (same host and after restart): no further effect, checkpoint intact.
        runtime()
            .block_on(fixture.run_private(&[genuine.clone(), genuine]))
            .unwrap();
        assert_eq!(workspace_creates(&fixture).len(), 2);
        assert_eq!(
            authz::sessions::restore(dir.path(), &fixture.server()).unwrap(),
            checkpoint_before
        );
    }

    #[test]
    fn reopen_requires_an_owner_grant_and_a_known_local_session() {
        let name = "signed::reopen_requires_an_owner_grant_and_a_known_local_session";
        if !isolated(name) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let owner = Signer::new();
        let start_only = Signer::new();
        let agent = Signer::new();
        let host = enroll_with(
            dir.path(),
            "owner",
            &owner,
            &["session.start", "session.reopen"],
        );
        enroll_with(dir.path(), "owner", &start_only, &["session.start"]);
        enroll_with(
            dir.path(),
            "agent",
            &agent,
            &[&format!("session.write:{SESSION}")],
        );
        runtime()
            .block_on(fixture.run_private(&[frame(&owner, &host, SESSION, "claude", JTI)]))
            .unwrap();
        assert_eq!(fixture.launches(), 1);
        let stranger_session = "5e1aeb93-5aba-419e-8856-a7c721406279";
        let frames = [
            reopen_frame(&agent, &host, SESSION, "11111111111111111111111111111111"),
            reopen_frame(
                &start_only,
                &host,
                SESSION,
                "22222222222222222222222222222222",
            ),
            reopen_frame(
                &owner,
                &host,
                stranger_session,
                "33333333333333333333333333333333",
            ),
            reopen_frame(
                &owner,
                "WRONGHOST",
                SESSION,
                "44444444444444444444444444444444",
            ),
        ];
        runtime().block_on(fixture.run_private(&frames)).unwrap();
        assert_eq!(fixture.launches(), 1, "{}", fixture.launch_log());
    }

    #[test]
    fn existing_checkpoints_are_adopted_and_a_failed_reopen_leaves_them_alone() {
        let name = "signed::existing_checkpoints_are_adopted_and_a_failed_reopen_leaves_them_alone";
        if !isolated(name) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let owner = Signer::new();
        let host = enroll_with(
            dir.path(),
            "owner",
            &owner,
            &["session.start", "session.reopen"],
        );
        let native = "01a168c9-b038-70a3-9f0f-4d3aebae2d94";
        let saved = crate::open_sessions::OpenSession {
            session_id: SESSION.into(),
            agent: conversations_core::AgentKind::Codex,
            cwd: dir.path().to_string_lossy().into_owned(),
            resume_id: Some(native.into()),
            agent_args: vec![],
            launcher: conversations_core::AgentLauncher::default(),
            adk_statusline: false,
        };
        let checkpoints = dirs::config_dir()
            .unwrap()
            .join("arrakis-conversations/open-sessions.json");
        std::fs::create_dir_all(checkpoints.parent().unwrap()).unwrap();
        std::fs::write(
            &checkpoints,
            serde_json::json!({"servers": {fixture.server(): [saved.clone()]}}).to_string(),
        )
        .unwrap();
        // Before adoption the host has no record: nothing to reopen.
        runtime()
            .block_on(fixture.run_private(&[reopen_frame(
                &owner,
                &host,
                SESSION,
                "55555555555555555555555555555555",
            )]))
            .unwrap();
        assert_eq!(fixture.launches(), 0);
        assert_eq!(
            authz::sessions::import_once(dir.path(), &fixture.server()).unwrap(),
            1
        );
        assert_eq!(
            authz::sessions::import_once(dir.path(), &fixture.server()).unwrap(),
            0,
            "idempotent"
        );
        runtime()
            .block_on(fixture.run_private(&[reopen_frame(
                &owner,
                &host,
                SESSION,
                "66666666666666666666666666666666",
            )]))
            .unwrap();
        let creates = workspace_creates(&fixture);
        assert_eq!(creates.len(), 1, "{}", fixture.launch_log());
        assert!(creates[0].contains(native), "{}", creates[0]);

        // Break the local cwd: the reopen fails, the prior checkpoint is untouched.
        authz::store::Store::open(dir.path())
            .unwrap()
            .connection()
            .execute(
                "UPDATE sessions SET cwd = '/nonexistent/dir' WHERE session_id = ?1",
                [SESSION],
            )
            .unwrap();
        runtime()
            .block_on(fixture.run_private(&[reopen_frame(
                &owner,
                &host,
                SESSION,
                "77777777777777777777777777777777",
            )]))
            .unwrap();
        assert_eq!(workspace_creates(&fixture).len(), 1);
        assert_eq!(
            authz::sessions::restore(dir.path(), &fixture.server()).unwrap(),
            vec![crate::open_sessions::OpenSession {
                cwd: "/nonexistent/dir".into(),
                ..saved
            }]
        );
    }

    #[test]
    fn a_session_with_an_unknown_start_outcome_cannot_be_reopened() {
        let name = "signed::a_session_with_an_unknown_start_outcome_cannot_be_reopened";
        if !isolated(name) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let owner = Signer::new();
        let host = enroll_with(
            dir.path(),
            "owner",
            &owner,
            &["session.start", "session.reopen"],
        );
        // A start left `dispatching` by a crash, with its local record.
        authz::record_started(
            dir.path(),
            &authz::sessions::LocalSession {
                session_id: SESSION.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: dir.path().to_string_lossy().into_owned(),
                resume_id: Some(SESSION.into()),
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
        )
        .unwrap();
        let store = authz::store::Store::open(dir.path()).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO op_journal(kid, jti, payload_sha256, state, session_id, admitted_at, cap) VALUES('k','j',x'00','uncertain',?1,1,'session.start')",
                [SESSION],
            )
            .unwrap();
        drop(store);
        runtime()
            .block_on(fixture.run_private(&[reopen_frame(
                &owner,
                &host,
                SESSION,
                "88888888888888888888888888888888",
            )]))
            .unwrap();
        assert_eq!(fixture.launches(), 0, "{}", fixture.launch_log());
    }
}

mod owner_cookie {
    //! Owner-cookie direct-localhost rename goes through the same effective
    //! title and shared-workspace invariants as the signed hosted path. The
    //! web server holds no cmux of its own; its rename message
    //! is a typed `OwnerRename` on the daemon's owner-only Unix socket. Both
    //! paths meet at `commit_owner_rename`.
    use super::*;
    use crate::commands::local_host::authz::sessions::LocalSession;

    /// End-to-end proof that the HTTP `/api/local/sessions/{id}/rename` path
    /// (the owner-cookie frontend route) mirrors the rename into cmux through
    /// the running daemon with the same invariants as the signed path:
    ///   - Resolves the current workspace from `surface_workspaces()`, not the
    ///     possibly stale `registration.workspace`.
    ///   - Refuses to touch a workspace that two registered conversations
    ///     share.
    ///   - Always commits the DB override; only the sidebar push is best-effort.
    ///   - A missing surface (detached) silently no-ops rather than guessing.
    #[test]
    fn owner_cookie_rename_resolves_current_workspace_and_refuses_shared_or_detached() {
        if !isolated(
            "owner_cookie::owner_cookie_rename_resolves_current_workspace_and_refuses_shared_or_detached",
        ) {
            return;
        }
        const MOVED: &str = "11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        const SHARED_A: &str = "22222222-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        const SHARED_B: &str = "33333333-cccc-4ccc-8ccc-cccccccccccc";
        const DETACHED: &str = "44444444-dddd-4ddd-8ddd-dddddddddddd";
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let log = fixture.launch_log_path().display().to_string();
        assert!(!log.contains('\''), "log path must not contain quotes");
        let tree = serde_json::json!({
            "windows": [{"id": "w1", "workspaces": [
                {"id": "ws-new", "pinned": false, "panes": [{"surfaces": [{"id": "s-moved"}]}]},
                {"id": "ws-shared", "pinned": false, "panes": [{"surfaces": [
                    {"id": "s-shared-a"}, {"id": "s-shared-b"}
                ]}]},
            ]}],
        })
        .to_string();
        assert!(
            !tree.contains('\''),
            "tree JSON must not contain single quotes"
        );
        fixture.rewrite_binary(&format!(
            "#!/bin/sh\necho \"$@\" >> '{log}'\ncase \"$*\" in\n  *'tree --all'*) echo '{tree}';;\n  *) echo '{{}}';;\nesac\n"
        ));
        let mut store = authz::store::Store::open(dir.path()).unwrap();
        for id in [MOVED, SHARED_A, SHARED_B, DETACHED] {
            store
                .insert_session(
                    &LocalSession {
                        session_id: id.into(),
                        agent: conversations_core::AgentKind::Claude,
                        cwd: "/w".into(),
                        resume_id: None,
                        launcher: Default::default(),
                        adk_statusline: false,
                    },
                    "adopted",
                )
                .unwrap();
        }
        drop(store);
        let host = Arc::new(fixture.host(fixture.server()));
        let register = |id: &str, workspace: Option<&str>, surface: Option<&str>| {
            host.sessions.lock().unwrap().insert(
                id.into(),
                Session {
                    readiness: Default::default(),
                    local_input_bytes: 0,
                    local_input_dropped_chunks: 0,
                    registration: Registration {
                        launch: Launch {
                            id: id.into(),
                            agent: conversations_core::AgentKind::Claude,
                            cwd: "/w".into(),
                            command: vec!["claude".into()],
                        },
                        workspace: workspace.map(str::to_owned),
                        surface: surface.map(str::to_owned),
                        local_terminal: true,
                        reconnecting: false,
                        file_receiver: false,
                        transfer_gate: false,
                        effect_gate: true,
                        in_place: false,
                    },
                    resume_published: None,
                    opencode_title_published: None,
                    commands: mpsc::channel(1).0,
                    pid: None,
                    screen: None,
                },
            );
        };
        // MOVED's surface moved to `ws-new` since the last registration tick.
        register(MOVED, Some("ws-stale"), Some("s-moved"));
        register(SHARED_A, Some("ws-shared"), Some("s-shared-a"));
        register(SHARED_B, Some("ws-shared"), Some("s-shared-b"));
        register(DETACHED, Some("ws-detached"), Some("s-nowhere"));
        let rt = runtime();
        rt.block_on(async move {
            let socket_path = dir.path().join("host.sock");
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let host_for_accept = host.clone();
            let accept = tokio::spawn(async move {
                loop {
                    let (socket, _) = listener.accept().await.unwrap();
                    let h = host_for_accept.clone();
                    tokio::spawn(async move {
                        let _ = super::super::handle_client(h, socket).await;
                    });
                }
            });
            let unknown = uuid::Uuid::new_v4().to_string();
            let direct = super::super::super::request(
                dir.path(),
                ClientMessage::OwnerRename {
                    session: unknown,
                    title: "Unknown direct socket".into(),
                },
            )
            .await
            .unwrap();
            assert!(
                matches!(direct, ServerMessage::Error { .. }),
                "daemon must reject unknown sessions"
            );
            let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = tcp.local_addr().unwrap().port();
            let state = crate::commands::local_host::web::WebState::for_test(
                dir.path(),
                "https://example.com",
                port,
                "test-owner-cookie",
            );
            let web = tokio::spawn(async move {
                axum::serve(tcp, crate::commands::local_host::web::router(state))
                    .await
                    .unwrap();
            });
            let base = format!("http://127.0.0.1:{port}");
            let client = reqwest::Client::new();
            let rename = |id: &str, title: &str| {
                let url = format!("{base}/api/local/sessions/{id}/rename");
                client
                    .post(url)
                    .header("Cookie", "ac_local_owner=test-owner-cookie")
                    .header("Origin", &base)
                    .json(&serde_json::json!({"title": title}))
            };
            let rename_calls = || -> Vec<String> {
                fixture
                    .launch_log()
                    .lines()
                    .filter(|line| line.starts_with("workspace rename "))
                    .map(str::to_owned)
                    .collect()
            };
            let db_title = |session: &str| {
                crate::commands::local_host::history::History::open(dir.path())
                    .unwrap()
                    .summary(session)
                    .unwrap()
                    .title
            };
            let response = rename(MOVED, "Moved from web").send().await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.json::<Value>().await.unwrap()["title"],
                "Moved from web"
            );
            let response = rename(SHARED_A, "Shared from web").send().await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.json::<Value>().await.unwrap()["title"],
                "Shared from web"
            );
            let response = rename(DETACHED, "Detached from web").send().await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.json::<Value>().await.unwrap()["title"],
                "Detached from web"
            );
            let unknown = uuid::Uuid::new_v4().to_string();
            let response = rename(&unknown, "Unknown from web").send().await.unwrap();
            assert_eq!(response.status(), 404);
            assert_eq!(db_title(MOVED).as_deref(), Some("Moved from web"));
            assert_eq!(db_title(SHARED_A).as_deref(), Some("Shared from web"));
            assert_eq!(db_title(DETACHED).as_deref(), Some("Detached from web"));
            assert_eq!(
                rename_calls(),
                vec!["workspace rename ws-new --title=Moved from web".to_owned()],
                "exactly one cmux rename (MOVED → ws-new); shared/detached never touch cmux"
            );
            // Cleared rename on MOVED must not push an empty title.
            let response = rename(MOVED, "   ").send().await.unwrap();
            assert_eq!(response.status(), 200);
            assert!(db_title(MOVED).is_none(), "cleared override drops DB row");
            assert_eq!(
                rename_calls(),
                vec!["workspace rename ws-new --title=Moved from web".to_owned()],
                "a cleared rename must not push an empty title into cmux"
            );
            accept.abort();
            web.abort();
        });
    }

    /// The web process without a running daemon commits the DB rename
    /// directly and never claims a cmux mirror (there is no daemon to attempt
    /// it through). Same truthful result as a normal (non-cmux) host.
    #[test]
    fn owner_cookie_rename_without_a_daemon_commits_the_db_override() {
        if !isolated("owner_cookie::owner_cookie_rename_without_a_daemon_commits_the_db_override") {
            return;
        }
        const SESSION: &str = "55555555-eeee-4eee-8eee-eeeeeeeeeeee";
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = authz::store::Store::open(dir.path()).unwrap();
        store
            .insert_session(
                &LocalSession {
                    session_id: SESSION.into(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/w".into(),
                    resume_id: None,
                    launcher: Default::default(),
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        drop(store);
        let rt = runtime();
        rt.block_on(async move {
            let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = tcp.local_addr().unwrap().port();
            let state = crate::commands::local_host::web::WebState::for_test(
                dir.path(),
                "https://example.com",
                port,
                "test-owner-cookie",
            );
            let web = tokio::spawn(async move {
                axum::serve(tcp, crate::commands::local_host::web::router(state))
                    .await
                    .unwrap();
            });
            let base = format!("http://127.0.0.1:{port}");
            let client = reqwest::Client::new();
            let response = client
                .post(format!("{base}/api/local/sessions/{SESSION}/rename"))
                .header("Cookie", "ac_local_owner=test-owner-cookie")
                .header("Origin", &base)
                .json(&serde_json::json!({"title": "No daemon here"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.json::<Value>().await.unwrap()["title"],
                "No daemon here"
            );
            assert_eq!(
                crate::commands::local_host::history::History::open(dir.path())
                    .unwrap()
                    .summary(SESSION)
                    .unwrap()
                    .title
                    .as_deref(),
                Some("No daemon here"),
                "DB override is the authoritative rename and must commit without a daemon"
            );
            web.abort();
        });
    }
}

mod passkey {
    use super::*;
    use crate::commands::local_host::authz::{
        self,
        passkey::{PasskeyRequest, PasskeyResponse},
        store::{now_secs, test_clock, Store},
    };
    use crate::commands::local_host::protocol::{read_message, write_message};
    use serde_json::Value;
    use webauthn_authenticator_rs::{softpasskey::SoftPasskey, WebauthnAuthenticator};
    use webauthn_rs::prelude::{
        CreationChallengeResponse, PublicKeyCredential, RegisterPublicKeyCredential,
        RequestChallengeResponse, Url,
    };

    const ORIGIN: &str = "http://localhost:8080";
    const FIXTURE: &str = include_str!("authz/fixtures/chrome_delegated_ops.json");

    fn configure() {
        std::fs::write(
            super::super::super::settings::Settings::path().unwrap(),
            json!({"webauthn": {"rp_id": "localhost", "origins": [ORIGIN]}}).to_string(),
        )
        .unwrap();
    }

    /// The real adapter: a request over the host's Unix socket, handled by
    /// `handle_client` exactly as `passkey ...` would be.
    async fn call(fixture: &Fixture, request: PasskeyRequest) -> Result<PasskeyResponse, String> {
        let host = Arc::new(fixture.host(fixture.server()));
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let served = tokio::spawn(handle_client(host, server));
        write_message(&mut client, &ClientMessage::Passkey { request })
            .await
            .unwrap();
        let reply: ServerMessage = read_message(&mut client).await.unwrap();
        let _ = served.await;
        match reply {
            ServerMessage::Passkey {
                response: Some(response),
                ..
            } => Ok(response),
            ServerMessage::Passkey {
                error: Some(error), ..
            } => Err(error),
            other => panic!("unexpected reply {other:?}"),
        }
    }

    fn started(response: PasskeyResponse) -> (String, Value) {
        match response {
            PasskeyResponse::Started { challenge, options }
            | PasskeyResponse::Offered {
                challenge, options, ..
            } => (challenge, options),
            other => panic!("expected Started, got {other:?}"),
        }
    }

    fn register(auth: &mut SoftPasskey, origin: &str, options: Value) -> Result<Value, String> {
        let options: CreationChallengeResponse = serde_json::from_value(options).unwrap();
        let credential: RegisterPublicKeyCredential = auth
            .do_registration(Url::parse(origin).unwrap(), options)
            .map_err(|e| format!("{e:?}"))?;
        Ok(serde_json::to_value(credential).unwrap())
    }

    fn assert_(auth: &mut SoftPasskey, origin: &str, options: Value) -> Result<Value, String> {
        let options: RequestChallengeResponse = serde_json::from_value(options).unwrap();
        let credential: PublicKeyCredential = auth
            .do_authentication(Url::parse(origin).unwrap(), options)
            .map_err(|e| format!("{e:?}"))?;
        Ok(serde_json::to_value(credential).unwrap())
    }

    fn rt() -> tokio::runtime::Runtime {
        runtime()
    }

    /// Enroll + locally approve one passkey holding `grants`.
    async fn approved(fixture: &Fixture, auth: &mut SoftPasskey, grants: &[&str]) -> String {
        let (challenge, options) = started(
            call(
                fixture,
                PasskeyRequest::EnrollStart {
                    label: "laptop".into(),
                    kind: "owner".into(),
                    grants: grants.iter().map(|g| g.to_string()).collect(),
                },
            )
            .await
            .unwrap(),
        );
        let response = register(auth, ORIGIN, options).unwrap();
        let PasskeyResponse::Registered { cred_id, .. } = call(
            fixture,
            PasskeyRequest::EnrollFinish {
                challenge,
                response,
                vouch: None,
            },
        )
        .await
        .unwrap() else {
            panic!("not registered")
        };
        let PasskeyResponse::Approved { .. } = call(
            fixture,
            PasskeyRequest::Approve {
                cred_id: cred_id.clone(),
            },
        )
        .await
        .unwrap() else {
            panic!("not approved")
        };
        cred_id
    }

    fn count(dir: &Path, sql: &str) -> i64 {
        Store::open(dir)
            .unwrap()
            .connection()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    /// The key-trust ceremony over the SIGNED CHANNEL:
    /// an owner credential holding `passkey.manage` creates an
    /// enrollment invite, lists the resulting key offer, and approves it —
    /// the host recording the trust locally. An agent key, even one holding a
    /// session write grant, is refused the whole ceremony: `passkey.manage` is
    /// owner-only and never grantable.
    #[test]
    fn key_ceremony_over_the_channel_is_owner_only() {
        let name = "passkey::key_ceremony_over_the_channel_is_owner_only";
        if !isolated(name) {
            return;
        }
        configure();
        use crate::commands::local_host::authz::envelope::test_signing::{wire, Signer};
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());

        fn enrolled(dir: &Path, kind: &str, signer: &Signer, grants: &[&str]) -> String {
            authz::enroll_from_cli(
                dir,
                kind,
                "ed25519",
                &base64::engine::general_purpose::STANDARD.encode(&signer.public),
                "t",
                &grants.iter().map(|g| g.to_string()).collect::<Vec<_>>(),
            )
            .map(|(_, host)| host)
            .unwrap()
        }

        fn frame(signer: &Signer, host: &str, op: &str, args: &str, jti: &str) -> String {
            let now = now_secs();
            wire(
                &signer.sign_bytes(
                    signer
                        .op_payload(host, op, "host", args, jti, now, now + 60)
                        .as_bytes(),
                ),
            )
        }

        async fn one(
            fixture: &Fixture,
            frames: &[String],
        ) -> (conversations_core::host_relay::OpStatus, Option<String>) {
            let mut outcomes = fixture.run_private(frames).await.unwrap();
            assert_eq!(outcomes.len(), 1);
            outcomes.pop().unwrap()
        }

        rt().block_on(async {
            let session = uuid::Uuid::new_v4().to_string();

            // An agent key holding a session write grant: the ceremony is
            // entirely out of reach.
            let agent = Signer::new();
            let agent_host = enrolled(
                dir.path(),
                "agent",
                &agent,
                &[&format!("session.write:{session}")],
            );
            let outcome = one(
                &fixture,
                &[frame(
                    &agent,
                    &agent_host,
                    "passkey.pending",
                    "{}",
                    "11111111111111111111111111111111",
                )],
            )
            .await;
            assert!(
                matches!(
                    outcome.0,
                    conversations_core::host_relay::OpStatus::Refused
                ),
                "an agent key reached the key ceremony: {outcome:?}"
            );

            // An owner credential holding the ceremony capability.
            let owner = Signer::new();
            let owner_host = enrolled(dir.path(), "owner", &owner, &["passkey.manage"]);

            // No offers yet.
            let outcome = one(
                &fixture,
                &[frame(
                    &owner,
                    &owner_host,
                    "passkey.pending",
                    "{}",
                    "22222222222222222222222222222222",
                )],
            )
            .await;
            assert!(
                matches!(
                    outcome.0,
                    conversations_core::host_relay::OpStatus::Executed
                ),
                "owner could not list offers: {outcome:?}"
            );
            assert_eq!(outcome.1.as_deref(), Some("[]"));

            // The invite link: everything after the # stays in the browser.
            let outcome = one(
                &fixture,
                &[frame(
                    &owner,
                    &owner_host,
                    "passkey.enroll_start",
                    &format!(
                        r#"{{"label":"alice@example.com","kind":"agent","grants":["session.write:{session}","history.read:{session}"]}}"#
                    ),
                    "33333333333333333333333333333333",
                )],
            )
            .await;
            assert!(
                matches!(
                    outcome.0,
                    conversations_core::host_relay::OpStatus::Executed
                ),
                "owner could not create an invite: {outcome:?}"
            );
            let invite: Value = serde_json::from_str(&outcome.1.unwrap()).unwrap();
            let link = invite["link"].as_str().unwrap().to_owned();
            let fragment = link.split('#').nth(1).expect("invite fragment");
            assert!(link.contains("/host-link#desc="), "{link}");
            let challenge = fragment
                .split("challenge=")
                .nth(1)
                .unwrap()
                .split('&')
                .next()
                .unwrap()
                .to_owned();
            let options: Value = serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(fragment.split("options=").nth(1).unwrap())
                    .unwrap(),
            )
            .unwrap();

            // The colleague completes the passkey against the host (the relay
            // path a browser uses); the registration waits for approval.
            let mut auth = SoftPasskey::new(true);
            let response = register(&mut auth, ORIGIN, options).unwrap();
            let PasskeyResponse::Registered { cred_id, .. } = call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge,
                    response,
                vouch: None,
                },
            )
            .await
            .unwrap() else {
                panic!("not registered")
            };

            // The offer shows up over the channel with its label and grants.
            let outcome = one(
                &fixture,
                &[frame(
                    &owner,
                    &owner_host,
                    "passkey.pending",
                    "{}",
                    "44444444444444444444444444444444",
                )],
            )
            .await;
            let pending: Vec<Value> = serde_json::from_str(&outcome.1.unwrap()).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0]["label"], json!("alice@example.com"));
            assert_eq!(pending[0]["cred_id"], json!(cred_id));

            // Approving over the channel: the HOST records the trust locally.
            let outcome = one(
                &fixture,
                &[frame(
                    &owner,
                    &owner_host,
                    "passkey.approve",
                    &format!(r#"{{"cred_id":"{cred_id}"}}"#),
                    "55555555555555555555555555555555",
                )],
            )
            .await;
            assert!(
                matches!(
                    outcome.0,
                    conversations_core::host_relay::OpStatus::Executed
                ),
                "owner could not approve the offer: {outcome:?}"
            );
            assert_eq!(
                count(
                    dir.path(),
                    &format!(
                        "SELECT COUNT(*) FROM credentials WHERE cred_id = '{cred_id}' AND revoked_at IS NULL"
                    ),
                ),
                1,
                "the host did not record the trusted key"
            );
            assert_eq!(
                count(
                    dir.path(),
                    &format!(
                        "SELECT COUNT(*) FROM grants WHERE cred_id = '{cred_id}' AND cap = 'session.write' AND session_id = '{session}' AND revoked_at IS NULL"
                    ),
                ),
                1,
                "the trusted key did not receive the session it was invited to"
            );
            // Main's AddDevice path creates owner offers locally. A browser
            // delegation must not turn one into a permanent owner credential.
            let (challenge, options) = started(call(&fixture, PasskeyRequest::EnrollStart {
                label: "owner device".into(), kind: "owner".into(),
                grants: vec!["passkey.manage".into()],
            }).await.unwrap());
            let response = register(&mut SoftPasskey::new(true), ORIGIN, options).unwrap();
            let PasskeyResponse::Registered { cred_id, .. } = call(&fixture, PasskeyRequest::EnrollFinish {
                challenge, response, vouch: None,
            }).await.unwrap() else { panic!("not registered") };
            let outcome = one(&fixture, &[frame(&owner, &owner_host, "passkey.approve",
                &format!(r#"{{"cred_id":"{cred_id}"}}"#), "66666666666666666666666666666666")]).await;
            assert!(!matches!(outcome.0, conversations_core::host_relay::OpStatus::Executed),
                "channel approved a permanent owner: {outcome:?}");
            assert_eq!(count(dir.path(), &format!("SELECT COUNT(*) FROM pending_credentials WHERE cred_id = '{cred_id}'")), 1);
            assert!(matches!(call(&fixture, PasskeyRequest::Approve { cred_id }).await.unwrap(), PasskeyResponse::Approved { .. }));
        });
    }

    /// The cross-host vouch (EX-04/EX-05): a key offer vouched by the
    /// colleague's OWN host carries that host's identity, a vouch for another
    /// host or ceremony is refused, and a SECOND key vouched by an
    /// already-trusted host inherits that trust.
    #[test]
    fn vouched_offers_carry_their_host_and_inherit_trust() {
        let name = "passkey::vouched_offers_carry_their_host_and_inherit_trust";
        if !isolated(name) {
            return;
        }
        configure();
        use crate::commands::local_host::authz::envelope::test_signing::{wire, Signer};
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        // The colleague's own host: a second fixture over its own runtime dir.
        let alice_dir = tempfile::tempdir().unwrap();
        let alice_fixture = Fixture::new(alice_dir.path());

        fn enrolled(dir: &Path, kind: &str, signer: &Signer, grants: &[&str]) -> String {
            authz::enroll_from_cli(
                dir,
                kind,
                "ed25519",
                &base64::engine::general_purpose::STANDARD.encode(&signer.public),
                "t",
                &grants.iter().map(|g| g.to_string()).collect::<Vec<_>>(),
            )
            .map(|(_, host)| host)
            .unwrap()
        }

        fn frame(signer: &Signer, host: &str, op: &str, args: &str, jti: &str) -> String {
            let now = now_secs();
            wire(
                &signer.sign_bytes(
                    signer
                        .op_payload(host, op, "host", args, jti, now, now + 60)
                        .as_bytes(),
                ),
            )
        }

        async fn one(
            fixture: &Fixture,
            frames: &[String],
        ) -> (conversations_core::host_relay::OpStatus, Option<String>) {
            let mut outcomes = fixture.run_private(frames).await.unwrap();
            assert_eq!(outcomes.len(), 1);
            outcomes.pop().unwrap()
        }

        /// Invite one agent key scoped to `session`; answer the ceremony.
        async fn invite(
            fixture: &Fixture,
            owner: &Signer,
            owner_host: &str,
            session: &str,
            jti: &str,
        ) -> (String, Value) {
            let outcome = one(
                fixture,
                &[frame(
                    owner,
                    owner_host,
                    "passkey.enroll_start",
                    &format!(
                        r#"{{"label":"alice@example.com","kind":"agent","grants":["session.write:{session}","history.read:{session}"]}}"#
                    ),
                    jti,
                )],
            )
            .await;
            assert!(
                matches!(
                    outcome.0,
                    conversations_core::host_relay::OpStatus::Executed
                ),
                "no invite: {outcome:?}"
            );
            let invite: Value = serde_json::from_str(&outcome.1.unwrap()).unwrap();
            let fragment = invite["link"]
                .as_str()
                .unwrap()
                .split('#')
                .nth(1)
                .expect("fragment")
                .to_owned();
            let challenge = fragment
                .split("challenge=")
                .nth(1)
                .unwrap()
                .split('&')
                .next()
                .unwrap()
                .to_owned();
            let options: Value = serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(fragment.split("options=").nth(1).unwrap())
                    .unwrap(),
            )
            .unwrap();
            (challenge, options)
        }

        /// The colleague's host vouches for one ceremony and one expected
        /// passkey id, over its own channel op.
        async fn vouch(
            alice_fixture: &Fixture,
            alice: &Signer,
            alice_host: &str,
            owner_host: &str,
            challenge: &str,
            cred_id: &str,
            jti: &str,
        ) -> conversations_core::host_relay::VouchProof {
            let outcome = one(
                alice_fixture,
                &[frame(
                    alice,
                    alice_host,
                    "passkey.vouch",
                    &format!(
                        r#"{{"for_host":"{owner_host}","challenge":"{challenge}","cred_id":"{cred_id}"}}"#
                    ),
                    jti,
                )],
            )
            .await;
            assert!(
                matches!(
                    outcome.0,
                    conversations_core::host_relay::OpStatus::Executed
                ),
                "no vouch: {outcome:?}"
            );
            let v: Value = serde_json::from_str(&outcome.1.unwrap()).unwrap();
            conversations_core::host_relay::VouchProof {
                statement: serde_json::from_value(v["statement"].clone()).unwrap(),
                host_key: v["host_key"].as_str().unwrap().to_owned(),
            }
        }

        /// What the browser derives from its authenticator before submitting:
        /// `base64url(SHA-256(raw credential id))` — the id the host will store.
        fn expected_cred_id(response: &Value) -> String {
            let parsed: RegisterPublicKeyCredential =
                serde_json::from_value(response.clone()).unwrap();
            URL_SAFE_NO_PAD.encode(ring::digest::digest(
                &ring::digest::SHA256,
                parsed.raw_id.as_ref(),
            ))
        }

        rt().block_on(async {
            let session = uuid::Uuid::new_v4().to_string();
            // The owner (ceremony host) and the colleague's own host identity.
            let owner = Signer::new();
            let owner_host = enrolled(dir.path(), "owner", &owner, &["passkey.manage"]);
            let alice = Signer::new();
            let alice_host =
                enrolled(alice_dir.path(), "owner", &alice, &["host.settings_read"]);
            let vouch_host_id = Store::open(alice_dir.path())
                .unwrap()
                .host_id()
                .unwrap();

            // A channel invitation can never mint an OWNER credential or
            // grant an owner-only capability — not even from the owner's own
            // delegated browser (which holds `passkey.manage`).
            for (args, jti) in [
                (
                    r#"{"label":"evil","kind":"owner","grants":["session.start"]}"#,
                    "77777777777777777777777777777777",
                ),
                (
                    r#"{"label":"evil","kind":"agent","grants":["passkey.manage"]}"#,
                    "88888888888888888888888888888888",
                ),
            ] {
                let outcome = one(
                    &fixture,
                    &[frame(&owner, &owner_host, "passkey.enroll_start", args, jti)],
                )
                .await;
                assert!(
                    !matches!(
                        outcome.0,
                        conversations_core::host_relay::OpStatus::Executed
                    ),
                    "a channel invitation minted forbidden authority: {outcome:?}"
                );
            }

            // Ceremony 1 + a real passkey. The browser derives the expected
            // id from its authenticator before anything is submitted.
            let mut auth = SoftPasskey::new(true);
            let (challenge, options) =
                invite(&fixture, &owner, &owner_host, &session, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                    .await;
            let response = register(&mut auth, ORIGIN, options.clone()).unwrap();
            let cred1 = expected_cred_id(&response);

            // A vouch for ANOTHER host: refused — and it burns nothing, the
            // same ceremony and response still complete below.
            let evil = {
                let mut store = Store::open(alice_dir.path()).unwrap();
                let vouch = conversations_core::host_relay::KeyVouch {
                    v: 1,
                    typ: "key_vouch".to_owned(),
                    for_host: "wrong-host".to_owned(),
                    challenge: challenge.clone(),
                    cred_id: cred1.clone(),
                };
                let statement = authz::attest::sign_value(&mut store, &vouch).unwrap();
                conversations_core::host_relay::VouchProof {
                    statement,
                    host_key: URL_SAFE_NO_PAD.encode(store.host_public_key().unwrap()),
                }
            };
            let refused = call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge: challenge.clone(),
                    response: response.clone(),
                    vouch: Some(evil),
                },
            )
            .await;
            assert!(refused.is_err(), "a vouch for another host was accepted");

            // The refused vouch consumed nothing: the live vouch completes
            // the SAME ceremony with the SAME registration.
            let proof = vouch(
                &alice_fixture,
                &alice,
                &alice_host,
                &owner_host,
                &challenge,
                &cred1,
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .await;
            let PasskeyResponse::Registered { cred_id, .. } = call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge: challenge.clone(),
                    response,
                    vouch: Some(proof),
                },
            )
            .await
            .unwrap() else {
                panic!("not registered")
            };
            assert_eq!(cred_id, cred1);

            // The offer carries the vouching host's identity (EX-04); the
            // first offer claims no inherited trust.
            let outcome = one(
                &fixture,
                &[frame(
                    &owner,
                    &owner_host,
                    "passkey.pending",
                    "{}",
                    "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                )],
            )
            .await;
            let pending: Vec<Value> = serde_json::from_str(&outcome.1.unwrap()).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0]["vouch_host_id"], json!(vouch_host_id));
            assert!(
                pending[0]["trusted_host"].is_null(),
                "the first offer must not claim inherited trust"
            );

            // A vouch naming a DIFFERENT key than the enrollment produced is
            // refused: a replayed or stolen vouch cannot be re-attached.
            let (challenge2, options2) =
                invite(&fixture, &owner, &owner_host, &session, "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd")
                    .await;
            let response2 = register(&mut auth, ORIGIN, options2).unwrap();
            let _cred2 = expected_cred_id(&response2);
            let mismatched = vouch(
                &alice_fixture,
                &alice,
                &alice_host,
                &owner_host,
                &challenge2,
                &cred1, // the OLD key: bound to ceremony 2 but not to this key
                "efefefefefefefefefefefefefefefef",
            )
            .await;
            let refused = call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge: challenge2,
                    response: response2,
                    vouch: Some(mismatched),
                },
            )
            .await;
            assert!(
                refused.is_err(),
                "a vouch naming another key than the enrollment was accepted"
            );

            // Approving the first offer: the host records the trust locally,
            // carrying the vouching host (EX-05).
            let outcome = one(
                &fixture,
                &[frame(
                    &owner,
                    &owner_host,
                    "passkey.approve",
                    &format!(r#"{{"cred_id":"{cred_id}"}}"#),
                    "ffffffffffffffffffffffffffffffff",
                )],
            )
            .await;
            assert!(
                matches!(
                    outcome.0,
                    conversations_core::host_relay::OpStatus::Executed
                ),
                "could not approve: {outcome:?}"
            );
            assert_eq!(
                count(
                    dir.path(),
                    &format!(
                        "SELECT COUNT(*) FROM credentials WHERE cred_id = '{cred_id}' AND vouch_host_id = '{vouch_host_id}'"
                    ),
                ),
                1,
                "the approved key does not carry its vouching host"
            );

            // A second key vouched by the SAME host inherits the trust.
            let (challenge3, options3) =
                invite(&fixture, &owner, &owner_host, &session, "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f")
                    .await;
            let response3 = register(&mut SoftPasskey::new(true), ORIGIN, options3).unwrap();
            let cred3 = expected_cred_id(&response3);
            let proof3 = vouch(
                &alice_fixture,
                &alice,
                &alice_host,
                &owner_host,
                &challenge3,
                &cred3,
                "1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f",
            )
            .await;
            call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge: challenge3,
                    response: response3,
                    vouch: Some(proof3),
                },
            )
            .await
            .unwrap();

            let outcome = one(
                &fixture,
                &[frame(
                    &owner,
                    &owner_host,
                    "passkey.pending",
                    "{}",
                    "2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f",
                )],
            )
            .await;
            let pending: Vec<Value> = serde_json::from_str(&outcome.1.unwrap()).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(
                pending[0]["vouch_host_id"], json!(vouch_host_id),
                "the second offer lost its vouch"
            );
            assert_eq!(
                pending[0]["trusted_host"], json!(true),
                "a key vouched by an already-trusted host does not inherit the trust"
            );
        });
    }

    #[test]
    fn enrollment_needs_local_approval_and_rejects_bad_ceremonies() {
        let name = "passkey::enrollment_needs_local_approval_and_rejects_bad_ceremonies";
        if !isolated(name) {
            return;
        }
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        rt().block_on(async {
            let grant = vec!["session.start".to_string()];
            // Agents can never be granted session creation, passkey or not.
            assert!(call(
                &fixture,
                PasskeyRequest::EnrollStart {
                    label: "a".into(),
                    kind: "agent".into(),
                    grants: grant.clone(),
                }
            )
            .await
            .is_err());

            let (challenge, options) = started(
                call(
                    &fixture,
                    PasskeyRequest::EnrollStart {
                        label: "laptop".into(),
                        kind: "owner".into(),
                        grants: grant.clone(),
                    },
                )
                .await
                .unwrap(),
            );
            let mut auth = SoftPasskey::new(true);
            let good = register(&mut auth, ORIGIN, options.clone()).unwrap();

            // Wrong origin: the browser says it ran on another site.
            let evil = register(
                &mut SoftPasskey::new(true),
                "http://localhost:9999",
                options.clone(),
            );
            if let Ok(response) = evil {
                assert!(
                    call(
                        &fixture,
                        PasskeyRequest::EnrollFinish {
                            challenge: challenge.clone(),
                            response,
                            vouch: None,
                        }
                    )
                    .await
                    .is_err(),
                    "foreign origin accepted"
                );
            }
            // The failed attempt consumed the ceremony: even the valid response is now dead.
            assert!(call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge: challenge.clone(),
                    response: good.clone(),
                    vouch: None,
                }
            )
            .await
            .is_err());

            // Fresh ceremony. Unknown challenge, then the real thing.
            let (challenge, options) = started(
                call(
                    &fixture,
                    PasskeyRequest::EnrollStart {
                        label: "laptop".into(),
                        kind: "owner".into(),
                        grants: grant.clone(),
                    },
                )
                .await
                .unwrap(),
            );
            let response = register(&mut auth, ORIGIN, options.clone()).unwrap();
            assert!(call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge: "00000000000000000000000000000000".into(),
                    response: response.clone(),
                    vouch: None,
                }
            )
            .await
            .is_err());
            // A response made for ANOTHER ceremony does not satisfy this challenge.
            let (other, _) = started(
                call(
                    &fixture,
                    PasskeyRequest::EnrollStart {
                        label: "laptop".into(),
                        kind: "owner".into(),
                        grants: grant.clone(),
                    },
                )
                .await
                .unwrap(),
            );
            assert!(call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge: other,
                    response: response.clone(),
                    vouch: None,
                }
            )
            .await
            .is_err());

            let PasskeyResponse::Registered { cred_id, .. } = call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge: challenge.clone(),
                    response: response.clone(),
                    vouch: None,
                },
            )
            .await
            .unwrap() else {
                panic!()
            };
            // Single use: replaying the same finish never registers twice.
            assert!(call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge,
                    response,
                    vouch: None,
                }
            )
            .await
            .is_err());

            // Verified but NOT trusted: no credential and no grant until local approval.
            assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM credentials"), 0);
            assert_eq!(
                count(dir.path(), "SELECT COUNT(*) FROM pending_credentials"),
                1
            );
            let PasskeyResponse::Approved { .. } =
                call(&fixture, PasskeyRequest::Approve { cred_id })
                    .await
                    .unwrap()
            else {
                panic!()
            };
            assert_eq!(
                count(
                    dir.path(),
                    "SELECT COUNT(*) FROM credentials WHERE auth = 'passkey'"
                ),
                1
            );
            assert_eq!(
                count(
                    dir.path(),
                    "SELECT COUNT(*) FROM grants WHERE cap = 'session.start'"
                ),
                1
            );
            assert_eq!(
                count(dir.path(), "SELECT COUNT(*) FROM pending_credentials"),
                0
            );
        });
    }

    #[test]
    fn expired_ceremonies_and_remote_callers_are_refused() {
        let name = "passkey::expired_ceremonies_and_remote_callers_are_refused";
        if !isolated(name) {
            return;
        }
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        rt().block_on(async {
            let (challenge, options) = started(
                call(
                    &fixture,
                    PasskeyRequest::EnrollStart {
                        label: "laptop".into(),
                        kind: "owner".into(),
                        grants: vec!["session.start".into()],
                    },
                )
                .await
                .unwrap(),
            );
            let response = register(&mut SoftPasskey::new(true), ORIGIN, options).unwrap();
            test_clock::set(now_secs() + authz::passkey::ENROLL_TTL_SECS + 1);
            let error = call(
                &fixture,
                PasskeyRequest::EnrollFinish {
                    challenge,
                    response,
                    vouch: None,
                },
            )
            .await
            .unwrap_err();
            assert!(error.contains("expired"), "{error}");
            test_clock::set(0);
        });
        // Local-only operations refuse a caller without socket trust, and no
        // request field can claim it.
        let settings = crate::commands::local_host::settings::Settings::load(dir.path()).unwrap();
        for request in [
            PasskeyRequest::Pending,
            PasskeyRequest::Approve {
                cred_id: "x".into(),
            },
            PasskeyRequest::EnrollStart {
                label: "l".into(),
                kind: "owner".into(),
                grants: vec!["session.start".into()],
            },
        ] {
            let error = authz::passkey::handle(dir.path(), &settings, request, None, now_secs())
                .unwrap_err();
            assert!(format!("{error:#}").contains("local socket"));
        }
        assert!(serde_json::from_value::<PasskeyRequest>(
            json!({"op": "approve", "cred_id": "x", "is_local": true})
        )
        .is_err());
    }

    #[test]
    fn delegation_is_bound_to_the_pending_request_and_the_parents_authority() {
        let name = "passkey::delegation_is_bound_to_the_pending_request_and_the_parents_authority";
        if !isolated(name) {
            return;
        }
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let pubkey = serde_json::from_str::<Value>(FIXTURE).unwrap()["pub"]
            .as_str()
            .unwrap()
            .to_owned();
        rt().block_on(async {
            let mut auth = SoftPasskey::new(true);
            approved(&fixture, &mut auth, &["session.start"]).await;
            let dlg = "00112233445566778899aabbccddeeff";
            let delegate = |caps: &[&str], dlg: &str| PasskeyRequest::DelegateStart {
                browser_pub: pubkey.clone(), dlg: dlg.into(), caps: caps.iter().map(|c| c.to_string()).collect(), nonce: "0123456789abcdef0123456789abcdef".to_string() };
            // Not a P-256 point / bad id: refused before any challenge exists.
            assert!(call(&fixture, PasskeyRequest::DelegateStart {
                browser_pub: "AAAA".into(), dlg: dlg.into(), caps: vec!["session.start".into()], nonce: "0123456789abcdef0123456789abcdef".to_string() }).await.is_err());
            assert!(call(&fixture, delegate(&["session.start"], "short")).await.is_err());

            // Scope above the parent's grants: the signed-off ceremony issues nothing.
            let (challenge, options) = started(call(&fixture, delegate(&["session.reopen"], dlg)).await.unwrap());
            let response = assert_(&mut auth, ORIGIN, options).unwrap();
            let error = call(&fixture, PasskeyRequest::DelegateFinish { challenge, response }).await.unwrap_err();
            assert!(error.contains("exceeds"), "{error}");
            assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM delegations"), 0);

            // Wrong credential: an authenticator that holds no approved passkey cannot answer.
            let (_, options) = started(call(&fixture, delegate(&["session.start"], dlg)).await.unwrap());
            assert!(assert_(&mut SoftPasskey::new(true), ORIGIN, options).is_err());

            // Wrong origin on a real assertion.
            let (challenge, options) = started(call(&fixture, delegate(&["session.start"], dlg)).await.unwrap());
            {
                let response = assert_(&mut auth, "http://localhost:9999", options).unwrap();
                assert!(call(&fixture, PasskeyRequest::DelegateFinish { challenge, response }).await.is_err());
            }

            // Good ceremony; the issued scope is exactly what was stored at start.
            let (challenge, options) = started(call(&fixture, delegate(&["session.start"], dlg)).await.unwrap());
            let response = assert_(&mut auth, ORIGIN, options).unwrap();
            let PasskeyResponse::Delegated { not_after, .. } = call(&fixture, PasskeyRequest::DelegateFinish {
                challenge: challenge.clone(), response: response.clone(),
            }).await.unwrap() else { panic!() };
            assert!(not_after - now_secs() <= 24 * 3600 && not_after - now_secs() > 24 * 3600 - 60);
            assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM delegations WHERE caps = 'session.start' AND alg = 'es256'"), 1);
            // Replaying the finish, or the assertion under a fresh challenge, issues nothing.
            assert!(call(&fixture, PasskeyRequest::DelegateFinish { challenge, response: response.clone() }).await.is_err());
            let (fresh, _) = started(call(&fixture, delegate(&["session.start"], "ffeeddccbbaa99887766554433221100")).await.unwrap());
            assert!(call(&fixture, PasskeyRequest::DelegateFinish { challenge: fresh, response }).await.is_err());
            assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM delegations"), 1);
            // The id is taken for good.
            assert!(call(&fixture, delegate(&["session.start"], dlg)).await.is_err());
        });
    }

    /// A delegated (browser) key: the delegation is the request narrowed to the
    /// parent's ceiling, and every frame is checked against the parent's grants
    /// for THAT session as they are now.
    #[test]
    fn a_delegated_browser_key_writes_and_shares_only_where_its_parent_is_granted() {
        let name =
            "passkey::a_delegated_browser_key_writes_and_shares_only_where_its_parent_is_granted";
        if !isolated(name) {
            return;
        }
        use super::authz::envelope::{key_id, DOMAIN_OP};
        use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
        configure();
        const A: &str = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";
        const B: &str = "5e1aeb93-5aba-419e-8856-a7c721406279";
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        rt().block_on(async {
            let mut auth = SoftPasskey::new(true);
            let parent = approved(
                &fixture,
                &mut auth,
                &["ceiling:session.write", &format!("session.write:{A}"), &format!("session.manage_sharing:{A}")],
            ).await;
            // The browser's key, generated like WebCrypto would (P-256, raw SEC1 public key).
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
            let public = pair.public_key().as_ref().to_vec();
            let kid = key_id(&public);
            let dlg = "00112233445566778899aabbccddeeff";
            // Ask for more than the parent holds: session.start is outside its ceiling.
            let (challenge, options) = started(call(&fixture, PasskeyRequest::DelegateStart {
                browser_pub: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &public),
                dlg: dlg.into(),
                caps: vec!["session.start".into(), "session.write".into(), "session.manage_sharing".into()],
                nonce: "0123456789abcdef0123456789abcdef".into(),
            }).await.unwrap());
            let response = assert_(&mut auth, ORIGIN, options).unwrap();
            let PasskeyResponse::Delegated { statement, .. } = call(&fixture, PasskeyRequest::DelegateFinish { challenge, response }).await.unwrap() else { panic!() };
            let done = statement_payload(&serde_json::to_value(&statement).unwrap());
            assert_eq!(done["caps"], json!(["session.manage_sharing", "session.write"]), "narrowed, never widened");
            assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM delegations WHERE caps = 'session.manage_sharing,session.write'"), 1);

            let mut store = Store::open(dir.path()).unwrap();
            for id in [A, B] {
                store.insert_session(&authz::sessions::LocalSession {
                    session_id: id.into(), agent: conversations_core::AgentKind::Claude, cwd: "/w".into(),
                    resume_id: None, launcher: conversations_core::AgentLauncher::default(), adk_statusline: false,
                }, "signed_start").unwrap();
            }
            drop(store);
            let host = fixture.host(fixture.server());
            let mut terminals = Vec::new();
            for id in [A, B] {
                let (commands, rx) = mpsc::channel(8);
                host.sessions.lock().unwrap().insert(id.into(), Session {
                    screen: Some(Screen::new(80, 24)),
                    readiness: Default::default(),
            local_input_bytes: 0, local_input_dropped_chunks: 0,
                    registration: Registration {
                        launch: Launch { id: id.into(), agent: conversations_core::AgentKind::Claude, cwd: "/w".into(), command: vec!["claude".into()] },
                        workspace: None, surface: None, local_terminal: true, reconnecting: false, file_receiver: false, transfer_gate: false, effect_gate: true,
                        in_place: false,
                    },
                    resume_published: None, opencode_title_published: None, commands, pid: None,
                });
                terminals.push(rx);
            }
            let host_id = authz::host_id(dir.path()).unwrap();
            let now = now_secs();
            let mut counter = 0u64;
            let mut sign = |op: &str, session: &str, args: String, life: i64| {
                counter += 1;
                let payload = format!(
                    r#"{{"v":1,"typ":"op","aud":"{host_id}","kid":"{kid}","dlg":"{dlg}","op":"{op}","sess":"{session}","jti":"{counter:032x}","iat":{now},"exp":{},"args":{args}}}"#,
                    now + life
                );
                let mut message = DOMAIN_OP.to_vec();
                message.extend_from_slice(payload.as_bytes());
                SignedFrame {
                    payload: payload.into_bytes(),
                    sig: pair.sign(&rng, &message).unwrap().as_ref().to_vec(),
                    kid: kid.clone(),
                }
            };
            let input = |session: &str, seq: u64| format!(r#"{{"session":"{session}","ch":"0123456789abcdef0123456789abcdef","seq":{seq},"b64":"aGk="}}"#);
            // Granted session: typed. Ungranted session on the same delegation: refused.
            let first = sign("session.input", A, input(A, 1), 20);
            assert_eq!(execute_signed(&host, first.clone()).await.0, conversations_core::host_relay::OpStatus::Executed);
            // The identical delegated frame is not typed again, in this process...
            assert_ne!(execute_signed(&host, first.clone()).await.0, conversations_core::host_relay::OpStatus::Executed);
            // ...nor by a fresh host on the same database within the frame's validity.
            let restarted = fixture.host(fixture.server());
            let (commands, mut restarted_rx) = mpsc::channel(8);
            restarted.sessions.lock().unwrap().insert(A.into(), Session {
                screen: Some(Screen::new(80, 24)),
                readiness: Default::default(),
            local_input_bytes: 0, local_input_dropped_chunks: 0,
                registration: Registration {
                    launch: Launch { id: A.into(), agent: conversations_core::AgentKind::Claude, cwd: "/w".into(), command: vec!["claude".into()] },
                    workspace: None, surface: None, local_terminal: true, reconnecting: false, file_receiver: false, transfer_gate: false, effect_gate: true,
                    in_place: false,
                },
                resume_published: None, opencode_title_published: None, commands, pid: None,
            });
            assert_ne!(execute_signed(&restarted, first).await.0, conversations_core::host_relay::OpStatus::Executed, "delegated frame replayed after restart");
            assert!(restarted_rx.try_recv().is_err(), "delegated frame typed again after restart");
            assert_eq!(execute_signed(&host, sign("session.input", B, input(B, 1), 20)).await.0, conversations_core::host_relay::OpStatus::Refused);
            // The delegation never had session.start, whatever the browser signs.
            let start = sign("session.start", B, format!(r#"{{"session":"{B}","agent":"claude"}}"#), 60);
            assert_eq!(execute_signed(&host, start).await.0, conversations_core::host_relay::OpStatus::Refused);
            // Sharing A to a locally approved guest works; B does not (no management of B).
            let guest = authz::envelope::test_signing::Signer::new();
            authz::enroll_from_cli(dir.path(), "owner", "ed25519", &base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &guest.public), "g", &["ceiling:session.write".into()]).unwrap();
            let share = |session: &str| format!(r#"{{"session":"{session}","recipient":"{}","caps":["session.write"]}}"#, guest.kid);
            assert_eq!(execute_signed(&host, sign("grant.add", A, share(A), 60)).await.0, conversations_core::host_relay::OpStatus::Executed);
            assert_eq!(execute_signed(&host, sign("grant.add", B, share(B), 60)).await.0, conversations_core::host_relay::OpStatus::Refused);
            assert_eq!(count(dir.path(), &format!("SELECT COUNT(*) FROM grants WHERE cred_id = '{}' AND session_id = '{A}' AND cap = 'session.write'", guest.kid)), 1);
            assert_eq!(count(dir.path(), &format!("SELECT COUNT(*) FROM grants WHERE cred_id = '{}' AND session_id = '{B}'", guest.kid)), 0);
            // Shrinking the PARENT stops the delegated key at its next frame.
            Store::open(dir.path()).unwrap().connection().execute(
                "UPDATE grants SET revoked_at = 1 WHERE cred_id = ?1 AND cap = 'session.write'", [&parent]).unwrap();
            assert_eq!(execute_signed(&host, sign("session.input", A, input(A, 2), 20)).await.0, conversations_core::host_relay::OpStatus::Refused);
            Store::open(dir.path()).unwrap().connection().execute(
                "UPDATE credentials SET revoked_at = 1 WHERE cred_id = ?1", [&parent]).unwrap();
            assert_eq!(execute_signed(&host, sign("grant.add", A, share(A), 60)).await.0, conversations_core::host_relay::OpStatus::Refused);
            let typed: Vec<_> = std::iter::from_fn(|| terminals[0].try_recv().ok()).collect();
            assert_eq!(typed.len(), 1, "only the first frame was ever typed: {typed:?}");
            assert!(terminals[1].try_recv().is_err());
        });
    }

    fn fixture_frame(name: &str) -> String {
        let frames: Value = serde_json::from_str::<Value>(FIXTURE).unwrap();
        let frame = &frames["frames"][name];
        json!({"type": "signed_op", "p": frame["p"], "s": frame["s"], "k": frame["k"]}).to_string()
    }

    #[test]
    fn chrome_signed_delegated_start_is_admitted_once_and_rechecked_each_time() {
        let name =
            "passkey::chrome_signed_delegated_start_is_admitted_once_and_rechecked_each_time";
        if !isolated(name) {
            return;
        }
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let fixed: Value = serde_json::from_str(FIXTURE).unwrap();
        let host_id = Store::open(dir.path())
            .unwrap()
            .install_test_identity(&[7u8; 32])
            .unwrap();
        assert_eq!(host_id, fixed["host"].as_str().unwrap());
        // Frames were signed for iat 1_800_000_000, valid 60s.
        test_clock::set(1_800_000_010);
        let pubkey = fixed["pub"].as_str().unwrap().to_owned();
        let dlg = fixed["dlg"].as_str().unwrap().to_owned();
        rt().block_on(async {
            let mut auth = SoftPasskey::new(true);
            approved(&fixture, &mut auth, &["session.start"]).await;
            let (challenge, options) = started(call(&fixture, PasskeyRequest::DelegateStart {
                browser_pub: pubkey, dlg: dlg.clone(), caps: vec!["session.start".into()], nonce: "0123456789abcdef0123456789abcdef".to_string() }).await.unwrap());
            let response = assert_(&mut auth, ORIGIN, options).unwrap();
            call(&fixture, PasskeyRequest::DelegateFinish { challenge, response }).await.unwrap();

            // Accepted once; the duplicate, the unenrolled direct key, an unknown
            // delegation and an operation outside the delegation's scope do nothing.
            fixture.run_private(&[
                fixture_frame("start1"), fixture_frame("start1"),
                fixture_frame("direct"), fixture_frame("other_dlg"), fixture_frame("reopen1"),
            ]).await.unwrap();
            assert_eq!(fixture.launches(), 1, "{}", fixture.launch_log());
            // Journal attributes the operation to the browser key AND its parent.
            assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM op_journal WHERE dlg IS NOT NULL AND parent_cred IS NOT NULL AND kid <> parent_cred AND cap = 'session.start'"), 1);
            fixture.run_private(&[fixture_frame("start1")]).await.unwrap();
            assert_eq!(fixture.launches(), 1);

            // Each authority input, withdrawn alone, stops the next operation.
            let spoil = |sql: &str| {
                Store::open(dir.path()).unwrap().connection().execute(sql, []).unwrap();
            };
            for (undo, redo) in [
                ("UPDATE grants SET revoked_at = 1", "UPDATE grants SET revoked_at = NULL"),
                ("UPDATE credentials SET revoked_at = 1", "UPDATE credentials SET revoked_at = NULL"),
                ("UPDATE delegations SET revoked_at = 1", "UPDATE delegations SET revoked_at = NULL"),
                ("UPDATE delegations SET not_after = 1", "UPDATE delegations SET not_after = 4000000000"),
                ("UPDATE credentials SET ceiling = 'session.reopen'", "UPDATE credentials SET ceiling = 'session.start'"),
            ] {
                spoil(undo);
                fixture.run_private(&[fixture_frame("start3")]).await.unwrap();
                assert_eq!(fixture.launches(), 1, "{undo} did not stop the operation");
                spoil(redo);
            }
            // With nothing withdrawn the same frame is accepted: each stop above was that one cause.
            fixture.run_private(&[fixture_frame("start3")]).await.unwrap();
            assert_eq!(fixture.launches(), 2, "{}", fixture.launch_log());
        });
    }

    /// A stand-in server for the relay: serves `events` on the stream, records
    /// every reply the host posts back.
    async fn serve_relay(
        fixture: &Fixture,
        host_id: &str,
        events: Vec<Value>,
    ) -> Arc<Mutex<Vec<Value>>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", fixture.port))
            .await
            .unwrap();
        let answers = Arc::new(Mutex::new(Vec::new()));
        let seen = answers.clone();
        let stream_path = format!("GET /api/host-relay/{host_id}/stream");
        let reply_path = format!("POST /api/host-relay/{host_id}/reply");
        let challenge_path = format!("POST /api/host-relay/{host_id}/challenge");
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (events, seen) = (events.clone(), seen.clone());
                let (stream_path, reply_path) = (stream_path.clone(), reply_path.clone());
                let challenge_path = challenge_path.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head, body) = loop {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        raw.extend_from_slice(&chunk[..n]);
                        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..i]).to_string();
                            let len = head
                                .to_ascii_lowercase()
                                .lines()
                                .find_map(|l| {
                                    l.strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if raw.len() >= i + 4 + len {
                                break (head, raw[i + 4..i + 4 + len].to_vec());
                            }
                        }
                    };
                    if head.starts_with(&challenge_path) {
                        let payload = r#"{"nonce":"stub-nonce"}"#;
                        let _ = socket
                            .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).as_bytes())
                            .await;
                    } else if head.starts_with(&stream_path) {
                        let mut text = String::from(
                            "data: {\"type\":\"attached\",\"reply_token\":\"stub\"}\n\n",
                        );
                        for event in &events {
                            text.push_str(&format!("data: {event}\n\n"));
                        }
                        let _ = socket
                            .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\n\n{text}", text.len() + 1).as_bytes())
                            .await;
                    } else if head.starts_with(&reply_path) {
                        seen.lock()
                            .unwrap()
                            .push(serde_json::from_slice(&body).unwrap());
                        let _ = socket
                            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                            .await;
                    }
                });
            }
        });
        answers
    }

    /// The status inside a host-signed op statement (signature checks are the
    /// browser verifier's job and are covered by its own tests).
    fn op_status(answer: &Value) -> String {
        statement_payload(&answer["reply"]["statement"])["status"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn statement_payload(statement: &Value) -> Value {
        let bytes = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            statement["p"].as_str().unwrap(),
        )
        .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn envelope(id: &str, request: Value) -> Value {
        json!({"id": id, "request": request})
    }

    fn fixture_request(name: &str) -> Value {
        let frames: Value = serde_json::from_str(FIXTURE).unwrap();
        let frame = &frames["frames"][name];
        json!({"type": "signed_op", "p": frame["p"], "s": frame["s"], "k": frame["k"]})
    }

    async fn relayed_replies(
        fixture: &Fixture,
        dir: &Path,
        events: Vec<Value>,
        expect: usize,
    ) -> Vec<Value> {
        let host_id = authz::host_id(dir).unwrap();
        let answers = serve_relay(fixture, &host_id, events).await;
        let host = Arc::new(fixture.host(fixture.server()));
        // Simulate the wrapper registering so an admitted start completes.
        let simulator = {
            let host = host.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    let ids: Vec<String> = host.pending.lock().unwrap().keys().cloned().collect();
                    for id in ids {
                        if let Some(tx) = host.pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(());
                        }
                    }
                }
            })
        };
        let result = super::relay::consume(&host, &host_id).await;
        assert_eq!(result.unwrap_err().to_string(), "host relay stream ended");
        for _ in 0..200 {
            if answers.lock().unwrap().len() >= expect {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        simulator.abort();
        let replies = answers.lock().unwrap().clone();
        replies
    }

    #[test]
    fn browser_flow_relays_ceremonies_and_reaches_the_real_dispatcher_only_privately() {
        let name = "passkey::browser_flow_relays_ceremonies_and_reaches_the_real_dispatcher_only_privately";
        if !isolated(name) {
            return;
        }
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let fixed: Value = serde_json::from_str(FIXTURE).unwrap();
        Store::open(dir.path())
            .unwrap()
            .install_test_identity(&[7u8; 32])
            .unwrap();
        test_clock::set(1_800_000_010);
        let pubkey = fixed["pub"].as_str().unwrap().to_owned();
        let dlg = fixed["dlg"].as_str().unwrap().to_owned();
        rt().block_on(async {
            let host = fixture.host(fixture.server());
            let mut auth = SoftPasskey::new(true);

            // Registration starts LOCALLY; only its completion is relayed.
            let (challenge, options) = started(
                call(&fixture, PasskeyRequest::EnrollStart {
                    label: "laptop".into(), kind: "owner".into(), grants: vec!["session.start".into()],
                }).await.unwrap(),
            );
            let response = register(&mut auth, ORIGIN, options).unwrap();
            let reply = super::relay::respond(&host, conversations_core::host_relay::RelayRequest::EnrollFinish {
                challenge, response, nonce: "0123456789abcdef0123456789abcdef".into(),
                vouch: None,
            }).await;
            let conversations_core::host_relay::RelayReply::Registered { statement } = reply else { panic!("{reply:?}") };
            let cred_id = statement_payload(&serde_json::to_value(&statement).unwrap())["cred_id"].as_str().unwrap().to_owned();
            // Relayed completion creates no authority: an operation is still refused.
            assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM credentials"), 0);
            // Only local approval makes it permanent.
            call(&fixture, PasskeyRequest::Approve { cred_id }).await.unwrap();

            // Delegation, entirely through relay replies.
            let reply = super::relay::respond(&host, conversations_core::host_relay::RelayRequest::DelegateStart {
                browser_pub: pubkey, dlg: dlg.clone(), caps: vec!["session.start".into()], nonce: "0123456789abcdef0123456789abcdef".to_string() }).await;
            let conversations_core::host_relay::RelayReply::Started { challenge, options, .. } = reply else { panic!("{reply:?}") };
            let response = assert_(&mut auth, ORIGIN, options).unwrap();
            let reply = super::relay::respond(&host, conversations_core::host_relay::RelayRequest::DelegateFinish {
                challenge, response,
            }).await;
            assert!(matches!(reply, conversations_core::host_relay::RelayReply::Delegated { .. }), "{reply:?}");

            // The Chrome-signed start over the real stream/answer protocol: the
            // relay answers with a signed refusal (a start is private content),
            // and requests outside the closed request set get no answer at all.
            let replies = relayed_replies(&fixture, dir.path(), vec![
                envelope("r1", fixture_request("start1")),
                envelope("r7", json!({"type": "approve", "cred_id": "x"})),
                envelope("r8", json!({"type": "enroll_start", "label": "x", "kind": "owner", "grants": ["session.start"]})),
            ], 1).await;
            let answer = replies.iter().find(|r| r["id"] == "r1")
                .unwrap_or_else(|| panic!("no answer for r1: {replies:?}"));
            assert_eq!(answer["reply"]["type"], "op");
            assert_eq!(op_status(answer), "refused");
            assert!(replies.iter().all(|r| r["id"] != "r7" && r["id"] != "r8"));
            assert_eq!(fixture.launches(), 0, "{}", fixture.launch_log());
            // Answers carry status only: no path, id or prompt material.
            let text = serde_json::to_string(&replies).unwrap();
            assert!(!text.contains(&dir.path().to_string_lossy().to_string()));

            // The same Chrome-signed frames as the private channel hands them
            // over once decrypted: the refused start was not spent, so it runs once.
            let tampered = {
                let mut request = fixture_request("start2");
                let mut payload = request["p"].as_str().unwrap().to_owned();
                payload.replace_range(10..11, if payload.as_bytes()[10] == b'A' { "B" } else { "A" });
                request["p"] = payload.into();
                request.to_string()
            };
            let outcomes = fixture.run_private(&[
                fixture_frame("start1"), fixture_frame("start1"), fixture_frame("direct"),
                fixture_frame("other_dlg"), fixture_frame("reopen1"), tampered,
            ]).await.unwrap();
            use conversations_core::host_relay::OpStatus;
            assert_eq!(
                outcomes.iter().map(|o| o.0).collect::<Vec<_>>(),
                [OpStatus::Executed, OpStatus::Replayed, OpStatus::Refused, OpStatus::Refused, OpStatus::Refused, OpStatus::Refused],
                "{outcomes:?}"
            );
            assert_eq!(fixture.launches(), 1, "{}", fixture.launch_log());
            assert_eq!(count(dir.path(), "SELECT COUNT(*) FROM credentials"), 1);
        });
    }

    #[test]
    fn operations_with_wrong_audience_or_expiry_or_unknown_key_are_refused() {
        let name = "passkey::operations_with_wrong_audience_or_expiry_or_unknown_key_are_refused";
        if !isolated(name) {
            return;
        }
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        // A host whose identity is NOT the one the fixture was signed for, and no
        // keys at all. On the private channel, so the verifier decides (the
        // relay would refuse a start before verifying it).
        use conversations_core::host_relay::OpStatus;
        rt().block_on(async {
            let outcomes = fixture
                .run_private(&[fixture_frame("start1")])
                .await
                .unwrap();
            assert_eq!(outcomes[0].0, OpStatus::Refused, "{outcomes:?}");
            assert_eq!(fixture.launches(), 0);
        });
        // Right host, but the frame's validity window has passed.
        let dir2 = tempfile::tempdir().unwrap();
        let fixture2 = Fixture::new(dir2.path());
        Store::open(dir2.path())
            .unwrap()
            .install_test_identity(&[7u8; 32])
            .unwrap();
        test_clock::set(1_800_000_000 + 10_000);
        rt().block_on(async {
            let outcomes = fixture2
                .run_private(&[fixture_frame("start1")])
                .await
                .unwrap();
            assert_eq!(outcomes[0].0, OpStatus::Refused, "{outcomes:?}");
            assert_eq!(fixture2.launches(), 0);
        });
    }

    const APP_FIXTURE: &str = include_str!("authz/fixtures/app_signed_ops.json");

    #[test]
    fn operations_signed_by_the_apps_own_browser_code_are_verified_by_the_real_host() {
        let name =
            "passkey::operations_signed_by_the_apps_own_browser_code_are_verified_by_the_real_host";
        if !isolated(name) {
            return;
        }
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let app: Value = serde_json::from_str(APP_FIXTURE).unwrap();
        assert_eq!(app["extractable"], false);
        let host_id = Store::open(dir.path())
            .unwrap()
            .install_test_identity(&[7u8; 32])
            .unwrap();
        assert_eq!(host_id, app["host"].as_str().unwrap());
        test_clock::set(1_800_000_010);
        let request = |frame: &str| {
            let frame = &app["frames"][frame];
            json!({"type": "signed_op", "p": frame["p"], "s": frame["s"], "k": frame["k"]})
        };
        rt().block_on(async {
            let host = fixture.host(fixture.server());
            let mut auth = SoftPasskey::new(true);
            approved(&fixture, &mut auth, &["session.start", "session.reopen"]).await;
            let reply = super::relay::respond(
                &host,
                conversations_core::host_relay::RelayRequest::DelegateStart {
                    browser_pub: app["pub"].as_str().unwrap().to_owned(),
                    dlg: app["dlg"].as_str().unwrap().to_owned(),
                    caps: vec!["session.start".into(), "session.reopen".into()],
                    nonce: "0123456789abcdef0123456789abcdef".to_string(),
                },
            )
            .await;
            let conversations_core::host_relay::RelayReply::Started {
                challenge, options, ..
            } = reply
            else {
                panic!("{reply:?}")
            };
            let response = assert_(&mut auth, ORIGIN, options).unwrap();
            let reply = super::relay::respond(
                &host,
                conversations_core::host_relay::RelayRequest::DelegateFinish {
                    challenge,
                    response,
                },
            )
            .await;
            assert!(
                matches!(
                    reply,
                    conversations_core::host_relay::RelayReply::Delegated { .. }
                ),
                "{reply:?}"
            );

            // The relay refuses the app's start (private content) without spending it.
            let replies = relayed_replies(
                &fixture,
                dir.path(),
                vec![envelope("a0", request("start_a"))],
                1,
            )
            .await;
            assert_eq!(op_status(&replies[0]), "refused", "{replies:?}");
            assert_eq!(fixture.launches(), 0);
            // On the private channel, after decryption, the real verifier accepts
            // each. start_b was signed after a page reload, by the same persisted key.
            let outcomes = fixture
                .run_private(&[
                    request("start_a").to_string(),
                    request("reopen_a").to_string(),
                    request("start_b").to_string(),
                ])
                .await
                .unwrap();
            assert_eq!(
                outcomes.iter().map(|o| o.0).collect::<Vec<_>>(),
                [conversations_core::host_relay::OpStatus::Executed; 3],
                "{outcomes:?}"
            );
            assert_eq!(fixture.launches(), 3, "{}", fixture.launch_log());
        });
    }

    const PR_SETTINGS_FIXTURE: &str =
        include_str!("../../../../../frontend/src/lib/fixtures/app_signed_pr_settings_ops.json");

    /// The PR and settings operations as the SPA's production signing code makes
    /// them (WebCrypto P-256 under vitest, not a browser) pass the real host: a
    /// passkey-parented delegation made through the relay request handler, the
    /// relay's refusal of each private operation, then the dispatch the private
    /// channel makes once a request is decrypted, the recording `gh`, and the
    /// local settings file. The channel itself is not exercised here.
    #[test]
    fn operations_signed_by_the_apps_own_code_for_pr_and_settings_are_verified_by_the_real_host() {
        let name = "passkey::operations_signed_by_the_apps_own_code_for_pr_and_settings_are_verified_by_the_real_host";
        if !isolated(name) {
            return;
        }
        use crate::commands::local_host::settings::Settings;
        use conversations_core::host_relay::{RelayReply, RelayRequest};
        let calls = super::write::install_recording_gh();
        let (central, pushed) = super::write::central_stub();
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let app: Value = serde_json::from_str(PR_SETTINGS_FIXTURE).unwrap();
        let host_id = Store::open(dir.path())
            .unwrap()
            .install_test_identity(&[7u8; 32])
            .unwrap();
        assert_eq!(host_id, app["host"].as_str().unwrap());
        test_clock::set(1_800_000_010);
        let session = app["session"].as_str().unwrap().to_owned();
        Store::open(dir.path())
            .unwrap()
            .insert_session(
                &authz::sessions::LocalSession {
                    session_id: session.clone(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/work".into(),
                    resume_id: None,
                    launcher: conversations_core::AgentLauncher::default(),
                    adk_statusline: false,
                },
                "signed_start",
            )
            .unwrap();
        // PR refresh now follows session.write; the fixture's browser
        // delegation gets write on the session it refreshes so pr.sync
        // admits without a standalone pr_sync grant.
        let caps: Vec<String> = vec![
            "session.pr_comment".into(),
            format!("session.write:{}", app["session"].as_str().unwrap()),
            "host.settings_read".into(),
            "host.settings_write".into(),
        ];
        rt().block_on(async {
            let mut host = fixture.host(fixture.server());
            host.server = central;
            let mut auth = SoftPasskey::new(true);
            let caps_ref: Vec<&str> = caps.iter().map(String::as_str).collect();
            approved(&fixture, &mut auth, &caps_ref).await;
            let reply = super::relay::respond(
                &host,
                RelayRequest::DelegateStart {
                    browser_pub: app["pub"].as_str().unwrap().to_owned(),
                    dlg: app["dlg"].as_str().unwrap().to_owned(),
                    caps: caps.clone(),
                    nonce: "0123456789abcdef0123456789abcdef".to_string(),
                },
            )
            .await;
            let RelayReply::Started {
                challenge, options, ..
            } = reply
            else {
                panic!("{reply:?}")
            };
            let response = assert_(&mut auth, ORIGIN, options).unwrap();
            let reply = super::relay::respond(
                &host,
                RelayRequest::DelegateFinish {
                    challenge,
                    response,
                },
            )
            .await;
            assert!(matches!(reply, RelayReply::Delegated { .. }), "{reply:?}");

            let send = |name: &str| {
                let frame = &app["frames"][name];
                RelayRequest::SignedOp {
                    p: frame["p"].as_str().unwrap().into(),
                    s: frame["s"].as_str().unwrap().into(),
                    k: frame["k"].as_str().unwrap().into(),
                }
            };
            let status = |reply: &RelayReply| match reply {
                RelayReply::Op { statement } => op_status(
                    &json!({"reply": {"statement": serde_json::to_value(statement).unwrap()}}),
                ),
                other => panic!("{other:?}"),
            };
            let private = |name: &str| {
                let frame = &app["frames"][name];
                SignedFrame::from_wire(
                    frame["p"].as_str().unwrap(),
                    frame["s"].as_str().unwrap(),
                    frame["k"].as_str().unwrap(),
                )
                .unwrap()
            };
            use conversations_core::host_relay::OpStatus;

            // A PR comment is private content: the relay refuses it unspent and
            // calls no `gh`; the private channel carries the very same frame.
            assert_eq!(
                status(&super::relay::respond(&host, send("pr_comment")).await),
                "refused"
            );
            assert!(super::write::gh_calls(&calls).is_empty());
            let (commented, said) = decrypted_channel_request(&host, private("pr_comment")).await;
            assert_eq!(commented, OpStatus::Executed);
            assert!(
                said.is_none(),
                "successful local refresh needs no withholding reason: {said:?}"
            );
            let made = super::write::gh_calls(&calls);
            let thread: Vec<_> = made
                .iter()
                .filter(|c| {
                    c.first().map(String::as_str) == Some("api")
                        && c.get(1).map(String::as_str) == Some("--method")
                })
                .collect();
            assert_eq!(thread.len(), 1, "{made:?}");
            assert_eq!(thread[0][3], "repos/octo/repo/pulls/7/comments/99/replies");
            assert_eq!(thread[0][5], "body=Looks good.\n-- thanks `x` $(y)");

            // Explicit sync refreshes the private host cache; the relay still
            // refuses the operation before any GitHub read.
            let before_sync = super::write::gh_calls(&calls).len();
            assert_eq!(
                status(&super::relay::respond(&host, send("pr_sync")).await),
                "refused"
            );
            assert_eq!(super::write::gh_calls(&calls).len(), before_sync);
            let (synced, said) = decrypted_channel_request(&host, private("pr_sync")).await;
            assert_eq!(synced, OpStatus::Executed, "{said:?}");
            assert_eq!(super::write::gh_calls(&calls).len(), before_sync + 3);
            let (cache, _, _) =
                crate::commands::local_host::pr_metadata::chunk(&host.dir, &session, 0, None)
                    .unwrap();
            let cache: Value = serde_json::from_slice(
                &base64::engine::general_purpose::STANDARD
                    .decode(cache)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(cache["prs"][0]["owner"], "octo");
            assert_eq!(cache["prs"][0]["number"], 7);
            assert_eq!(cache["prs"][0]["status"]["title"], "t");
            assert_eq!(cache["comments"], json!([]));
            // The session is private: the signed sync and comment leave no PR
            // content (and no PR read) at the central server.
            let central_pr = pushed
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.contains("/prs"))
                .cloned()
                .collect::<Vec<_>>();
            assert!(central_pr.is_empty(), "{central_pr:?}");

            // Settings are private content: the relay refuses the update and
            // the private channel carries the very same signed frame.
            assert_eq!(
                status(&super::relay::respond(&host, send("settings_update")).await),
                "refused"
            );
            let frame = &app["frames"]["settings_update"];
            let update = SignedFrame::from_wire(
                frame["p"].as_str().unwrap(),
                frame["s"].as_str().unwrap(),
                frame["k"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(
                super::execute_signed(&host, update).await.0,
                conversations_core::host_relay::OpStatus::Executed
            );
            let saved = Settings::load(dir.path()).unwrap();
            assert!(saved.launch.claude_skip_permissions && saved.launch.adk_statusline);
            assert_eq!(
                saved.launch.codex_sandbox_mode.as_deref(),
                Some("read-only")
            );
            assert_eq!(
                saved.launch.default_launcher,
                conversations_core::AgentLauncher::Adk
            );

            // Settings are read only on the private channel, never over the relay.
            assert_eq!(
                status(&super::relay::respond(&host, send("settings_get")).await),
                "refused"
            );
            let frame = &app["frames"]["settings_get"];
            let frame = SignedFrame::from_wire(
                frame["p"].as_str().unwrap(),
                frame["s"].as_str().unwrap(),
                frame["k"].as_str().unwrap(),
            )
            .unwrap();
            let launch = super::private_channel::channel_settings(&host.dir, &frame).unwrap();
            assert_eq!(
                launch["claude_skip_permissions"],
                app["launch"]["claude_skip_permissions"]
            );
            assert_eq!(
                launch["default_launcher"],
                app["launch"]["default_launcher"]
            );
            // The very same signed frames again: nothing repeats.
            assert_eq!(
                decrypted_channel_request(&host, private("pr_comment"))
                    .await
                    .0,
                OpStatus::Replayed
            );
            assert_eq!(
                super::write::gh_calls(&calls)
                    .iter()
                    .filter(|c| c.first().map(String::as_str) == Some("api")
                        && c.get(1).map(String::as_str) == Some("--method"))
                    .count(),
                1
            );
        });
    }

    /// A hostile relay swaps the browser's key for its own in `delegate_start`.
    /// The host cannot know what the browser intended, so it still opens the
    /// ceremony, but its SIGNED offer names the swapped key. An honest browser
    /// compares the offer with its own request before any passkey prompt (see
    /// `hostStatements.test.ts`, which runs the production verifier on offers
    /// this issuer produced). Before the signed offer existed, the same sequence
    /// ended with the attacker's key delegated and nothing for the browser to
    /// compare (recorded RED in evidence.md).
    #[test]
    fn a_relay_swapped_key_is_visible_in_the_host_signed_offer() {
        let name = "passkey::a_relay_swapped_key_is_visible_in_the_host_signed_offer";
        if !isolated(name) {
            return;
        }
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let honest: Value = serde_json::from_str(APP_FIXTURE).unwrap();
        let attacker: Value = serde_json::from_str(FIXTURE).unwrap();
        rt().block_on(async {
            let host = fixture.host(fixture.server());
            let mut auth = SoftPasskey::new(true);
            approved(&fixture, &mut auth, &["session.start", "session.reopen"]).await;
            let reply = super::relay::respond(
                &host,
                conversations_core::host_relay::RelayRequest::DelegateStart {
                    browser_pub: attacker["pub"].as_str().unwrap().to_owned(),
                    dlg: "00112233445566778899aabbccddeeff".into(),
                    caps: vec!["session.start".into(), "session.reopen".into()],
                    nonce: "0123456789abcdef0123456789abcdef".into(),
                },
            )
            .await;
            let conversations_core::host_relay::RelayReply::Started {
                challenge,
                options,
                statement,
            } = reply
            else {
                panic!("{reply:?}")
            };
            let offer = statement_payload(&serde_json::to_value(&statement).unwrap());
            assert_eq!(offer["browser_pub"], attacker["pub"]);
            assert_ne!(
                offer["browser_pub"], honest["pub"],
                "the honest key is not what the host offered"
            );
            assert_eq!(
                offer["webauthn_challenge"],
                options["publicKey"]["challenge"]
            );
            assert_eq!(offer["ceremony"], challenge.as_str());
            // Completing the ceremony without verifying delegates the swapped key.
            let response = assert_(&mut auth, ORIGIN, options).unwrap();
            let reply = super::relay::respond(
                &host,
                conversations_core::host_relay::RelayRequest::DelegateFinish {
                    challenge,
                    response,
                },
            )
            .await;
            let conversations_core::host_relay::RelayReply::Delegated { statement } = reply else {
                panic!("{reply:?}")
            };
            let done = statement_payload(&serde_json::to_value(&statement).unwrap());
            assert_eq!(done["typ"], "delegate_done");
            assert_eq!(done["browser_kid"], offer["browser_kid"]);
        });
    }

    const HOST_STATEMENTS_PATH: &str =
        "../../../../../frontend/src/lib/fixtures/host_statements.json";
    const HOST_STATEMENTS: &str =
        include_str!("../../../../../frontend/src/lib/fixtures/host_statements.json");

    fn b64u(bytes: &[u8]) -> String {
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
    }

    fn b64u_decode(text: &str) -> Vec<u8> {
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, text).unwrap()
    }

    /// Verify a host statement the way the issuer defines it: Ed25519 over
    /// `HOST_DOMAIN || payload` with the host identity key.
    fn verify_host_statement(host_pub: &[u8], statement: &Value) -> Value {
        let payload = b64u_decode(statement["p"].as_str().unwrap());
        let mut message = conversations_core::host_relay::HOST_DOMAIN.to_vec();
        message.extend_from_slice(&payload);
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, host_pub)
            .verify(&message, &b64u_decode(statement["s"].as_str().unwrap()))
            .expect("host signature verifies");
        serde_json::from_slice(&payload).unwrap()
    }

    /// Regenerates the committed fixture with the REAL host pipeline (relay
    /// respond -> passkey delegate start/finish -> attest). Run with
    /// `REGEN_HOST_FIXTURES=1` and `--ignored`; normal runs only validate the
    /// committed file in the separate behavioral tests below.
    #[test]
    #[ignore = "manual fixture generator; requires REGEN_HOST_FIXTURES=1"]
    fn regenerate_host_statement_fixture() {
        let name = "passkey::regenerate_host_statement_fixture";
        assert_eq!(
            std::env::var("REGEN_HOST_FIXTURES").as_deref(),
            Ok("1"),
            "manual fixture generation requires REGEN_HOST_FIXTURES=1"
        );
        if !isolated(name) {
            return;
        }
        use conversations_core::host_relay::{RelayReply, RelayRequest};
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let honest: Value = serde_json::from_str(APP_FIXTURE).unwrap();
        let attacker: Value = serde_json::from_str(FIXTURE).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.install_test_identity(&[7u8; 32]).unwrap();
        let host_pub = store.host_public_key().unwrap();
        drop(store);
        test_clock::set(1_800_000_010);
        let nonce = "0123456789abcdef0123456789abcdef";
        let dlg = "00112233445566778899aabbccddeeff";
        let caps = vec!["session.reopen".to_string(), "session.start".to_string()];
        let pub_a = honest["pub"].as_str().unwrap().to_owned();
        let out = rt().block_on(async {
            let host = fixture.host(fixture.server());
            let mut auth = SoftPasskey::new(true);
            approved(&fixture, &mut auth, &["session.start", "session.reopen"]).await;
            let start = |pubkey: String, dlg: &str, caps: Vec<String>, nonce: &str| {
                let request = RelayRequest::DelegateStart {
                    browser_pub: pubkey,
                    dlg: dlg.into(),
                    caps,
                    nonce: nonce.into(),
                };
                let host = &host;
                async move {
                    match super::relay::respond(host, request).await {
                        RelayReply::Started {
                            challenge,
                            options,
                            statement,
                        } => json!({
                            "challenge": challenge, "options": options, "offer": statement,
                        }),
                        other => panic!("{other:?}"),
                    }
                }
            };
            let swapped_pub = start(
                attacker["pub"].as_str().unwrap().to_owned(),
                dlg,
                caps.clone(),
                nonce,
            )
            .await;
            let swapped_caps = start(pub_a.clone(), dlg, vec!["session.start".into()], nonce).await;
            let swapped_dlg = start(
                pub_a.clone(),
                "ffeeddccbbaa99887766554433221100",
                caps.clone(),
                nonce,
            )
            .await;
            let swapped_nonce = start(
                pub_a.clone(),
                dlg,
                caps.clone(),
                "ffffffffffffffffffffffffffffffff",
            )
            .await;
            let mut honest_run = start(pub_a.clone(), dlg, caps.clone(), nonce).await;
            let response = assert_(&mut auth, ORIGIN, honest_run["options"].clone()).unwrap();
            let done = super::relay::respond(
                &host,
                RelayRequest::DelegateFinish {
                    challenge: honest_run["challenge"].as_str().unwrap().to_owned(),
                    response,
                },
            )
            .await;
            let RelayReply::Delegated { statement } = done else {
                panic!("{done:?}")
            };
            honest_run["done"] = serde_json::to_value(statement).unwrap();
            (
                honest_run,
                swapped_pub,
                swapped_caps,
                swapped_dlg,
                swapped_nonce,
            )
        });
        let (honest_run, swapped_pub, swapped_caps, swapped_dlg, swapped_nonce) = out;

        // Statements no honest host would issue, signed by real keys.
        let mut store = Store::open(dir.path()).unwrap();
        let mut bad_expiry = verify_host_statement(&host_pub, &honest_run["offer"]);
        bad_expiry["not_after"] = json!(bad_expiry["not_after"].as_i64().unwrap() + 86_400 * 30);
        let bad_expiry =
            authz::attest::sign_payload(&mut store, &serde_json::to_vec(&bad_expiry).unwrap())
                .unwrap();
        // A different host (own key, own id).
        let other_dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(other_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut other = Store::open(other_dir.path()).unwrap();
        other.install_test_identity(&[8u8; 32]).unwrap();
        let other_pub = other.host_public_key().unwrap();
        let other_id = other.host_id().unwrap();
        let honest_payload =
            serde_json::to_vec(&verify_host_statement(&host_pub, &honest_run["offer"])).unwrap();
        let other_signs_honest_payload =
            authz::attest::sign_payload(&mut other, &honest_payload).unwrap();
        let mut own = verify_host_statement(&host_pub, &honest_run["offer"]);
        own["host"] = json!(other_id);
        let other_own_offer =
            authz::attest::sign_payload(&mut other, &serde_json::to_vec(&own).unwrap()).unwrap();
        drop(store);

        // Operation outcomes bound to exact operation bytes.
        let frame = &honest["frames"]["start_a"];
        let op_bytes = b64u_decode(frame["p"].as_str().unwrap());
        let other_bytes = b64u_decode(honest["frames"]["start_b"]["p"].as_str().unwrap());
        let kid = honest["kid"].as_str().unwrap();
        let result = |bytes: &[u8], status: &str, detail: Option<&str>| {
            serde_json::to_value(
                authz::attest::op_result(dir.path(), kid, bytes, status, detail).unwrap(),
            )
            .unwrap()
        };
        let doc = json!({
            "_comment": "Generated by the real host pipeline (daemon_authz_tests.rs::passkey::regenerate_host_statement_fixture, REGEN_HOST_FIXTURES=1). Host identity seed [7;32]; clock 1800000010. Chrome-key request: browser_pub/dlg/caps/nonce below.",
            "host": {"descriptor": format!("chd1.{}", b64u(&host_pub)), "id": authz::host_id(dir.path()).unwrap()},
            "other_host": {"descriptor": format!("chd1.{}", b64u(&other_pub)), "id": other_id},
            "now": 1_800_000_010,
            "request": {"browser_pub": pub_a, "kid": kid, "dlg": dlg, "caps": caps, "nonce": nonce},
            "honest": honest_run,
            "swapped_pub": swapped_pub, "swapped_caps": swapped_caps,
            "swapped_dlg": swapped_dlg, "swapped_nonce": swapped_nonce,
            "bad_expiry": bad_expiry,
            "other_signs_honest_payload": other_signs_honest_payload,
            "other_own_offer": other_own_offer,
            "op": {
                "payload": frame["p"], "other_payload": honest["frames"]["start_b"]["p"], "kid": kid,
                "executed": result(&op_bytes, "executed", None),
                "replayed": result(&op_bytes, "replayed", Some("already done; not repeated")),
                "failed": result(&op_bytes, "failed", Some("failed earlier; not repeated")),
                "uncertain": result(&op_bytes, "uncertain", Some("an earlier attempt did not finish; not repeated")),
                "refused": result(&op_bytes, "refused", Some("no current grant")),
                "executed_for_other_op": result(&other_bytes, "executed", None),
            },
        });
        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/commands/local_host")
            .join(HOST_STATEMENTS_PATH);
        std::fs::write(&target, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    }

    const HOST_ENROLLMENT_PATH: &str =
        "../../../../../frontend/src/lib/fixtures/host_enrollment.json";
    const HOST_ENROLLMENT: &str =
        include_str!("../../../../../frontend/src/lib/fixtures/host_enrollment.json");
    const ENROLL_NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn raw_of(response: &Value) -> Value {
        json!({
            "rawId": response["rawId"],
            "attestationObject": response["response"]["attestationObject"],
            "clientDataJSON": response["response"]["clientDataJSON"],
        })
    }

    fn digest_of(raw: &Value) -> String {
        authz::attest::registration_digest(
            &b64u_decode(raw["rawId"].as_str().unwrap()),
            &b64u_decode(raw["attestationObject"].as_str().unwrap()),
            &b64u_decode(raw["clientDataJSON"].as_str().unwrap()),
        )
    }

    /// Copy of a host's state (same identity, same pending ceremony) in a new dir.
    fn clone_host(from: &Path) -> (tempfile::TempDir, Fixture) {
        let to = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(to.path());
        let target = to.path().join("authz.db");
        let _ = std::fs::remove_file(&target);
        Store::open(from)
            .unwrap()
            .connection()
            .execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])
            .unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        (to, fixture)
    }

    async fn finish_via_relay(
        fixture: &Fixture,
        challenge: &str,
        response: &Value,
        nonce: &str,
    ) -> Result<Value, String> {
        use conversations_core::host_relay::{RelayReply, RelayRequest};
        let host = fixture.host(fixture.server());
        match super::relay::respond(
            &host,
            RelayRequest::EnrollFinish {
                challenge: challenge.into(),
                response: response.clone(),
                nonce: nonce.into(),
                vouch: None,
            },
        )
        .await
        {
            RelayReply::Registered { statement } => Ok(serde_json::to_value(statement).unwrap()),
            RelayReply::Error { message } => Err(message),
            other => panic!("{other:?}"),
        }
    }

    async fn start_enroll(fixture: &Fixture) -> (String, Value) {
        started(
            call(
                fixture,
                PasskeyRequest::EnrollStart {
                    label: "laptop".into(),
                    kind: "owner".into(),
                    grants: vec!["session.start".into()],
                },
            )
            .await
            .unwrap(),
        )
    }

    /// Regenerates `host_enrollment.json` with the REAL host: one honest
    /// registration, a valid registration an attacker substitutes for the same
    /// ceremony, one that keeps the honest raw credential id, and the host-signed
    /// completions for each. Run with `REGEN_HOST_FIXTURES=1` and `--ignored`.
    #[test]
    #[ignore = "manual fixture generator; requires REGEN_HOST_FIXTURES=1"]
    fn regenerate_host_enrollment_fixture() {
        let name = "passkey::regenerate_host_enrollment_fixture";
        assert_eq!(
            std::env::var("REGEN_HOST_FIXTURES").as_deref(),
            Ok("1"),
            "manual fixture generation requires REGEN_HOST_FIXTURES=1"
        );
        if !isolated(name) {
            return;
        }
        configure();
        test_clock::set(1_800_000_010);
        let out = rt().block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let fixture = Fixture::new(dir.path());
            let mut store = Store::open(dir.path()).unwrap();
            store.install_test_identity(&[7u8; 32]).unwrap();
            let host_pub = store.host_public_key().unwrap();
            drop(store);
            let (challenge, options) = start_enroll(&fixture).await;
            let mut honest_auth = SoftPasskey::new(true);
            let mut attacker_auth = SoftPasskey::new(true);
            let honest = register(&mut honest_auth, ORIGIN, options.clone()).unwrap();
            let attacker = register(&mut attacker_auth, ORIGIN, options.clone()).unwrap();

            // The attacker keeps the honest raw credential id but uses its own
            // key: the attestation object is rewritten to carry the honest id.
            let honest_id = b64u_decode(honest["rawId"].as_str().unwrap());
            let attacker_id = b64u_decode(attacker["rawId"].as_str().unwrap());
            assert_eq!(
                honest_id.len(),
                attacker_id.len(),
                "fixture needs equal id lengths"
            );
            // A self-made `none` attestation (nothing to sign, so the attacker can
            // write it by hand): the attacker's authData with the honest id swapped in.
            let original = b64u_decode(attacker["response"]["attestationObject"].as_str().unwrap());
            let key = b"authData";
            let at = original
                .windows(key.len())
                .position(|w| w == key)
                .expect("authData key")
                + key.len();
            assert_eq!(
                original[at], 0x58,
                "authData is a one-byte-length byte string"
            );
            let len = original[at + 1] as usize;
            let mut auth_data = original[at + 2..at + 2 + len].to_vec();
            let id_at = auth_data
                .windows(attacker_id.len())
                .position(|w| w == attacker_id.as_slice())
                .expect("credential id inside authData");
            auth_data[id_at..id_at + attacker_id.len()].copy_from_slice(&honest_id);
            let mut attestation = vec![0xa3, 0x63];
            attestation.extend_from_slice(b"fmt");
            attestation.push(0x64);
            attestation.extend_from_slice(b"none");
            attestation.push(0x67);
            attestation.extend_from_slice(b"attStmt");
            attestation.push(0xa0);
            attestation.push(0x68);
            attestation.extend_from_slice(b"authData");
            attestation.extend_from_slice(&[0x58, len as u8]);
            attestation.extend_from_slice(&auth_data);
            let mut same_raw = attacker.clone();
            same_raw["id"] = honest["id"].clone();
            same_raw["rawId"] = honest["rawId"].clone();
            same_raw["response"]["attestationObject"] = b64u(&attestation).into();

            let finish = |response: Value, nonce: &'static str| {
                let (copy, fx) = clone_host(dir.path());
                let challenge = challenge.clone();
                async move {
                    let result = finish_via_relay(&fx, &challenge, &response, nonce).await;
                    drop(copy);
                    result
                }
            };
            let s_honest = finish(honest.clone(), ENROLL_NONCE).await.unwrap();
            let s_attacker = finish(attacker.clone(), ENROLL_NONCE).await.unwrap();
            let s_same_raw = finish(same_raw.clone(), ENROLL_NONCE).await;
            let s_other_nonce = finish(honest.clone(), "ffffffffffffffffffffffffffffffff")
                .await
                .unwrap();

            // Another ceremony on the same host, and another host entirely.
            let (c2, o2) = start_enroll(&fixture).await;
            let mut auth2 = SoftPasskey::new(true);
            let honest2 = register(&mut auth2, ORIGIN, o2).unwrap();
            let s_other_ceremony = finish_via_relay(&fixture, &c2, &honest2, ENROLL_NONCE)
                .await
                .unwrap();
            let dir3 = tempfile::tempdir().unwrap();
            let fixture3 = Fixture::new(dir3.path());
            let mut store3 = Store::open(dir3.path()).unwrap();
            store3.install_test_identity(&[9u8; 32]).unwrap();
            let other_pub = store3.host_public_key().unwrap();
            drop(store3);
            let (c3, o3) = start_enroll(&fixture3).await;
            let mut auth3 = SoftPasskey::new(true);
            let honest3 = register(&mut auth3, ORIGIN, o3).unwrap();
            let s_other_host = finish_via_relay(&fixture3, &c3, &honest3, ENROLL_NONCE)
                .await
                .unwrap();

            json!({
                "host_pub": b64u(&host_pub),
                "other_host_pub": b64u(&other_pub),
                "challenge": challenge,
                "nonce": ENROLL_NONCE,
                "options": options,
                "honest": { "response": honest, "raw": raw_of(&honest) },
                "attacker": { "response": attacker, "raw": raw_of(&attacker) },
                "same_raw_id": {
                    "response": same_raw,
                    "raw": raw_of(&same_raw),
                    "statement": s_same_raw.clone().ok(),
                    "host_refused": s_same_raw.err(),
                },
                "statements": {
                    "honest": s_honest,
                    "attacker": s_attacker,
                    "other_nonce": s_other_nonce,
                    "other_ceremony": s_other_ceremony,
                    "other_host": s_other_host,
                },
            })
        });
        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/commands/local_host")
            .join(HOST_ENROLLMENT_PATH);
        std::fs::write(&target, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
    }

    /// The committed fixture is what the production host signs: every statement
    /// verifies under the host key, binds the digest of exactly the response the
    /// host finished with, and the substituted responses really differ from the
    /// honest one (so a browser comparing digests can tell).
    #[test]
    fn committed_enrollment_statements_bind_exactly_the_verified_response() {
        let fixed: Value = serde_json::from_str(HOST_ENROLLMENT).unwrap();
        let host_pub = b64u_decode(fixed["host_pub"].as_str().unwrap());
        let other_pub = b64u_decode(fixed["other_host_pub"].as_str().unwrap());
        let check = |key: &[u8], statement: &Value| verify_host_statement(key, statement);
        let honest = check(&host_pub, &fixed["statements"]["honest"]);
        let attacker = check(&host_pub, &fixed["statements"]["attacker"]);
        assert_eq!(honest["typ"], "enroll_done");
        assert_eq!(honest["nonce"], fixed["nonce"]);
        assert_eq!(honest["ceremony"], fixed["challenge"]);
        assert_eq!(
            honest["response_sha256"],
            digest_of(&fixed["honest"]["raw"])
        );
        assert_eq!(
            attacker["response_sha256"],
            digest_of(&fixed["attacker"]["raw"])
        );
        assert_ne!(honest["response_sha256"], attacker["response_sha256"]);
        assert_ne!(honest["cred_id"], attacker["cred_id"]);
        // The honest id is the SHA-256 of the raw id the browser sees.
        assert_eq!(
            honest["cred_id"],
            b64u(
                ring::digest::digest(
                    &ring::digest::SHA256,
                    &b64u_decode(fixed["honest"]["raw"]["rawId"].as_str().unwrap())
                )
                .as_ref()
            )
        );
        // Same raw credential id, different key: the host accepts it (the id check
        // alone cannot tell) and signs a DIFFERENT digest.
        let same = &fixed["same_raw_id"];
        assert!(same["host_refused"].is_null(), "{}", same["host_refused"]);
        let same_payload = check(&host_pub, &same["statement"]);
        assert_eq!(same_payload["cred_id"], honest["cred_id"]);
        assert_ne!(same_payload["response_sha256"], honest["response_sha256"]);
        assert_eq!(same_payload["response_sha256"], digest_of(&same["raw"]));
        assert_ne!(
            fixed["honest"]["raw"]["attestationObject"],
            same["raw"]["attestationObject"]
        );
        // Wrong nonce / other ceremony are real, validly signed statements.
        assert_ne!(
            check(&host_pub, &fixed["statements"]["other_nonce"])["nonce"],
            fixed["nonce"]
        );
        assert_ne!(
            check(&host_pub, &fixed["statements"]["other_ceremony"])["ceremony"],
            fixed["challenge"]
        );
        // Another host signs under its own key only.
        let foreign = check(&other_pub, &fixed["statements"]["other_host"]);
        assert_ne!(foreign["host"], honest["host"]);
    }

    #[test]
    fn committed_host_statements_verify_under_the_hosts_key_and_bind_what_they_claim() {
        let fixture: Value = serde_json::from_str(HOST_STATEMENTS).unwrap();
        let host_pub = b64u_decode(
            fixture["host"]["descriptor"]
                .as_str()
                .unwrap()
                .strip_prefix("chd1.")
                .unwrap(),
        );
        // The id is the hash of the key the descriptor carries.
        assert_eq!(
            fixture["host"]["id"].as_str().unwrap(),
            b64u(ring::digest::digest(&ring::digest::SHA256, &host_pub).as_ref())
        );
        let request = &fixture["request"];
        let offer = verify_host_statement(&host_pub, &fixture["honest"]["offer"]);
        assert_eq!(offer["typ"], "delegate_offer");
        assert_eq!(offer["host"], fixture["host"]["id"]);
        assert_eq!(offer["browser_pub"], request["browser_pub"]);
        assert_eq!(offer["dlg"], request["dlg"]);
        assert_eq!(offer["nonce"], request["nonce"]);
        assert_eq!(offer["caps"], request["caps"]);
        assert_eq!(offer["ceremony"], fixture["honest"]["challenge"]);
        assert_eq!(
            offer["webauthn_challenge"],
            fixture["honest"]["options"]["publicKey"]["challenge"]
        );
        assert_eq!(
            offer["not_after"].as_i64().unwrap() - offer["issued_at"].as_i64().unwrap(),
            86_400
        );
        let done = verify_host_statement(&host_pub, &fixture["honest"]["done"]);
        assert_eq!(done["typ"], "delegate_done");
        assert_eq!(done["nonce"], request["nonce"]);
        assert_eq!(done["not_after"], offer["not_after"]);
        // What the host signed for each relay substitution names the substituted value.
        assert_ne!(
            verify_host_statement(&host_pub, &fixture["swapped_pub"]["offer"])["browser_pub"],
            request["browser_pub"]
        );
        assert_ne!(
            verify_host_statement(&host_pub, &fixture["swapped_caps"]["offer"])["caps"],
            request["caps"]
        );
        assert_ne!(
            verify_host_statement(&host_pub, &fixture["swapped_dlg"]["offer"])["dlg"],
            request["dlg"]
        );
        assert_ne!(
            verify_host_statement(&host_pub, &fixture["swapped_nonce"]["offer"])["nonce"],
            request["nonce"]
        );
        // Operation outcomes are bound to the hash of the exact operation bytes.
        let op = &fixture["op"];
        let sha = |text: &str| {
            b64u(ring::digest::digest(&ring::digest::SHA256, &b64u_decode(text)).as_ref())
        };
        for status in ["executed", "replayed", "failed", "uncertain", "refused"] {
            let result = verify_host_statement(&host_pub, &op[status]);
            assert_eq!(result["typ"], "op_result");
            assert_eq!(result["status"], status);
            assert_eq!(
                result["payload_sha256"],
                sha(op["payload"].as_str().unwrap())
            );
        }
        assert_eq!(
            verify_host_statement(&host_pub, &op["executed_for_other_op"])["payload_sha256"],
            sha(op["other_payload"].as_str().unwrap())
        );
    }

    #[test]
    fn a_replayed_operation_keeps_its_original_outcome() {
        let name = "passkey::a_replayed_operation_keeps_its_original_outcome";
        if !isolated(name) {
            return;
        }
        configure();
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let app: Value = serde_json::from_str(APP_FIXTURE).unwrap();
        Store::open(dir.path())
            .unwrap()
            .install_test_identity(&[7u8; 32])
            .unwrap();
        test_clock::set(1_800_000_010);
        rt().block_on(async {
            let host = fixture.host(fixture.server());
            let mut auth = SoftPasskey::new(true);
            approved(&fixture, &mut auth, &["session.start", "session.reopen"]).await;
            let reply = super::relay::respond(
                &host,
                conversations_core::host_relay::RelayRequest::DelegateStart {
                    browser_pub: app["pub"].as_str().unwrap().to_owned(),
                    dlg: app["dlg"].as_str().unwrap().to_owned(),
                    caps: vec!["session.start".into(), "session.reopen".into()],
                    nonce: "0123456789abcdef0123456789abcdef".into(),
                },
            )
            .await;
            let conversations_core::host_relay::RelayReply::Started {
                challenge, options, ..
            } = reply
            else {
                panic!("{reply:?}")
            };
            let response = assert_(&mut auth, ORIGIN, options).unwrap();
            super::relay::respond(
                &host,
                conversations_core::host_relay::RelayRequest::DelegateFinish {
                    challenge,
                    response,
                },
            )
            .await;
            let frame = &app["frames"]["start_a"];
            let sent = || conversations_core::host_relay::RelayRequest::SignedOp {
                p: frame["p"].as_str().unwrap().into(),
                s: frame["s"].as_str().unwrap().into(),
                k: frame["k"].as_str().unwrap().into(),
            };
            let status = |reply: conversations_core::host_relay::RelayReply| {
                let conversations_core::host_relay::RelayReply::Op { statement } = reply else {
                    panic!("{reply:?}")
                };
                statement_payload(&serde_json::to_value(statement).unwrap())["status"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            // Journal states a replay can meet; nothing may turn a non-success into one.
            let simulator = {
                let pending = &host.pending;
                async {
                    loop {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        let ids: Vec<String> = pending.lock().unwrap().keys().cloned().collect();
                        for id in ids {
                            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(());
                            }
                        }
                    }
                }
            };
            // The journal state is the host's, whatever carries the frame: the
            // relay refuses this start unspent, the private channel admits it.
            let private = || {
                let frame = SignedFrame::from_wire(
                    frame["p"].as_str().unwrap(),
                    frame["s"].as_str().unwrap(),
                    frame["k"].as_str().unwrap(),
                )
                .unwrap();
                let host = &host;
                async move {
                    let (outcome, _) = decrypted_channel_request(host, frame).await;
                    serde_json::to_value(outcome)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned()
                }
            };
            let flow = async {
                assert_eq!(
                    status(super::relay::respond(&host, sent()).await),
                    "refused"
                );
                assert_eq!(private().await, "executed");
                for (state, expected) in [
                    ("done", "replayed"),
                    ("failed", "failed"),
                    ("uncertain", "uncertain"),
                    ("dispatching", "uncertain"),
                    ("admitted", "uncertain"),
                ] {
                    Store::open(dir.path())
                        .unwrap()
                        .connection()
                        .execute("UPDATE op_journal SET state = ?1", [state])
                        .unwrap();
                    assert_eq!(private().await, expected, "journal state {state}");
                }
            };
            tokio::select! { _ = simulator => unreachable!(), _ = flow => {} }
        });
    }
}

/// Signed writes: grants, input, replies and control, observed as what actually
/// arrives at the (fake) terminal behind a session. Raw ed25519 keys stand in
/// for the delegated browser key; delegation and passkey rules have their own
/// tests in `mod passkey`.
mod write {
    use super::*;
    use crate::commands::local_host::authz::{
        self,
        envelope::test_signing::Signer,
        store::{now_secs, Store},
        SignedFrame,
    };
    use base64::engine::general_purpose::STANDARD as B64;
    use conversations_core::host_relay::OpStatus;

    const A: &str = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";
    const B: &str = "5e1aeb93-5aba-419e-8856-a7c721406279";
    const CH: &str = "0123456789abcdef0123456789abcdef";

    struct World {
        _dir: tempfile::TempDir,
        fixture: Fixture,
        host: Host,
        host_id: String,
        jti: std::cell::Cell<u64>,
        seq: std::cell::Cell<u64>,
    }

    /// Terminals behind sessions A and B, as `(commands rx)`.
    struct Terminals {
        a: mpsc::Receiver<ServerMessage>,
        b: mpsc::Receiver<ServerMessage>,
    }

    impl Terminals {
        fn drain(rx: &mut mpsc::Receiver<ServerMessage>) -> Vec<ServerMessage> {
            let mut out = Vec::new();
            while let Ok(message) = rx.try_recv() {
                out.push(message);
            }
            out
        }
    }

    fn world() -> (World, Terminals) {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let host = fixture.host(fixture.server());
        let host_id = authz::host_id(dir.path()).unwrap();
        // Both sessions are known to this host (as a signed start/adoption would leave them).
        let mut store = Store::open(dir.path()).unwrap();
        for id in [A, B] {
            store
                .insert_session(
                    &authz::sessions::LocalSession {
                        session_id: id.into(),
                        agent: conversations_core::AgentKind::Claude,
                        cwd: "/work".into(),
                        resume_id: None,
                        launcher: conversations_core::AgentLauncher::default(),
                        adk_statusline: false,
                    },
                    "signed_start",
                )
                .unwrap();
        }
        let (a, b) = register(&host);
        (
            World {
                _dir: dir,
                fixture,
                host,
                host_id,
                jti: std::cell::Cell::new(1),
                seq: std::cell::Cell::new(now_secs() as u64 * 1000),
            },
            Terminals { a, b },
        )
    }

    /// Terminals behind both sessions, registered on `host` like live wrappers.
    fn register(host: &Host) -> (mpsc::Receiver<ServerMessage>, mpsc::Receiver<ServerMessage>) {
        let mut terminals = Vec::new();
        for id in [A, B] {
            let (commands, rx) = mpsc::channel(64);
            host.sessions.lock().unwrap().insert(
                id.into(),
                Session {
                    screen: Some(Screen::new(80, 24)),
                    readiness: Default::default(),
                    local_input_bytes: 0,
                    local_input_dropped_chunks: 0,
                    registration: Registration {
                        launch: Launch {
                            id: id.into(),
                            agent: conversations_core::AgentKind::Claude,
                            cwd: "/work".into(),
                            command: vec!["claude".into()],
                        },
                        workspace: None,
                        surface: None,
                        local_terminal: true,
                        reconnecting: false,
                        file_receiver: true,
                        transfer_gate: true,
                        effect_gate: true,
                        in_place: false,
                    },
                    resume_published: None,
                    opencode_title_published: None,
                    commands,
                    pid: None,
                },
            );
            terminals.push(rx);
        }
        let b = terminals.pop().unwrap();
        let a = terminals.pop().unwrap();
        (a, b)
    }

    impl World {
        /// A quick host restart: a FRESH `Host` (empty memory) on the SAME
        /// authz database and with the same sessions' terminals re-attached.
        fn restart(&mut self) -> Terminals {
            self.host = self.fixture.host(self.fixture.server());
            let (a, b) = register(&self.host);
            Terminals { a, b }
        }

        fn dir(&self) -> &Path {
            &self.fixture.dir
        }

        fn enroll(&self, kind: &str, signer: &Signer, grants: &[String]) -> Result<()> {
            authz::enroll_from_cli(
                self.dir(),
                kind,
                "ed25519",
                &B64.encode(&signer.public),
                "t",
                grants,
            )
            .map(|_| ())
        }

        fn next_jti(&self) -> String {
            self.jti.set(self.jti.get() + 1);
            format!("{:032x}", self.jti.get())
        }

        fn next_seq(&self) -> u64 {
            self.seq.set(self.seq.get() + 1);
            self.seq.get()
        }

        fn frame(
            &self,
            signer: &Signer,
            op: &str,
            session: &str,
            args: &str,
            life: i64,
        ) -> SignedFrame {
            let now = now_secs();
            let payload = signer.op_payload(
                &self.host_id,
                op,
                session,
                args,
                &self.next_jti(),
                now,
                now + life,
            );
            signer.sign_bytes(payload.as_bytes())
        }

        /// The frame as its assigned transport hands it to the host ([`deliver_signed`]).
        async fn send(&self, frame: SignedFrame) -> (OpStatus, Option<String>) {
            deliver_signed(&self.host, frame).await
        }

        async fn grant_add(
            &self,
            owner: &Signer,
            session: &str,
            recipient: &Signer,
            caps: &str,
        ) -> (OpStatus, Option<String>) {
            let args = format!(
                r#"{{"session":"{session}","recipient":"{}","caps":{caps}}}"#,
                recipient.kid
            );
            self.send(self.frame(owner, "grant.add", session, &args, 60))
                .await
        }

        async fn input(
            &self,
            signer: &Signer,
            session: &str,
            text: &str,
        ) -> (OpStatus, Option<String>) {
            let args = format!(
                r#"{{"session":"{session}","ch":"{CH}","seq":{},"b64":"{}"}}"#,
                self.next_seq(),
                B64.encode(text)
            );
            self.send(self.frame(signer, "session.input", session, &args, 20))
                .await
        }
    }

    fn ok(result: &(OpStatus, Option<String>)) -> bool {
        matches!(result.0, OpStatus::Executed)
    }

    fn refused(result: &(OpStatus, Option<String>)) -> bool {
        matches!(result.0, OpStatus::Refused)
    }

    fn inputs(messages: Vec<ServerMessage>) -> Vec<String> {
        messages
            .into_iter()
            .filter_map(|m| match m {
                ServerMessage::Input { b64, .. } => {
                    Some(String::from_utf8(B64.decode(b64).unwrap()).unwrap())
                }
                _ => None,
            })
            .collect()
    }

    const IMG: &[u8] = b"\x89PNG-test-image";
    const TRANSFER: &str = "0123456789abcdef0123456789abcdef";

    /// Paths of every bracketed paste (`ESC[200~ path ESC[201~`) in terminal input.
    fn pasted_paths(input: &[u8]) -> Vec<String> {
        String::from_utf8_lossy(input)
            .split("\x1b[201~")
            .filter_map(|p| p.rsplit_once("\x1b[200~").map(|(_, path)| path.to_owned()))
            .collect()
    }

    /// The bytes of every image the agent was handed by path, in order.
    fn pasted_images(input: &[u8]) -> Vec<Vec<u8>> {
        pasted_paths(input)
            .iter()
            .filter(|p| {
                p.rsplit('/')
                    .next()
                    .is_some_and(|n| n.starts_with("pasted-image-"))
            })
            .map(|p| std::fs::read(p).unwrap())
            .collect()
    }

    fn sha(bytes: &[u8]) -> String {
        crate::commands::local_host::transfers::sha256_hex(bytes)
    }

    fn authorizations(messages: Vec<ServerMessage>) -> Vec<(String, TransferKind)> {
        messages
            .into_iter()
            .filter_map(|m| match m {
                ServerMessage::TransferAuthorized { transfer, kind, .. } => Some((transfer, kind)),
                _ => None,
            })
            .collect()
    }

    use crate::commands::local_host::transfers::TransferKind;

    impl World {
        async fn paste(
            &self,
            signer: &Signer,
            session: &str,
            transfer: &str,
            mime: &str,
            bytes: &[u8],
        ) -> (OpStatus, Option<String>) {
            let args = format!(
                r#"{{"session":"{session}","transfer":"{transfer}","mime":"{mime}","size":{},"sha256":"{}"}}"#,
                bytes.len(),
                sha(bytes)
            );
            self.send(self.frame(signer, "session.paste_image", session, &args, 60))
                .await
        }

        async fn drop_files(
            &self,
            signer: &Signer,
            session: &str,
            transfer: &str,
            files: &[(&str, &[u8])],
        ) -> (OpStatus, Option<String>) {
            let list: Vec<String> = files
                .iter()
                .map(|(n, b)| {
                    format!(
                        r#"{{"name":"{n}","size":{},"sha256":"{}"}}"#,
                        b.len(),
                        sha(b)
                    )
                })
                .collect();
            let args = format!(
                r#"{{"session":"{session}","transfer":"{transfer}","files":[{}]}}"#,
                list.join(",")
            );
            self.send(self.frame(signer, "session.send_files", session, &args, 60))
                .await
        }
    }

    #[test]
    fn a_signed_clipboard_and_file_authorization_reaches_only_its_terminal_once() {
        if !isolated(
            "write::a_signed_clipboard_and_file_authorization_reaches_only_its_terminal_once",
        ) {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let owner = Signer::new();
            let guest = Signer::new();
            let stranger = Signer::new();
            w.enroll("owner", &owner, &[format!("session.manage_sharing:{A}")])
                .unwrap();
            w.enroll("owner", &guest, &["ceiling:session.write".into()])
                .unwrap();
            w.enroll("owner", &stranger, &["ceiling:session.write".into()])
                .unwrap();
            assert!(ok(&w
                .grant_add(&owner, A, &guest, r#"["session.write"]"#)
                .await));

            // No grant (stranger), no key (unknown): zero effects.
            assert!(refused(
                &w.paste(&stranger, A, TRANSFER, "image/png", IMG).await
            ));
            let unknown = Signer::new();
            assert!(refused(
                &w.paste(&unknown, A, TRANSFER, "image/png", IMG).await
            ));
            assert!(authorizations(Terminals::drain(&mut t.a)).is_empty());

            // The granted guest: exactly one authorization, for A's terminal only.
            let first = w.paste(&guest, A, TRANSFER, "image/png", IMG).await;
            assert!(ok(&first), "{first:?}");
            let got = authorizations(Terminals::drain(&mut t.a));
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].0, TRANSFER);
            assert!(
                matches!(&got[0].1, TransferKind::Image { mime, size, sha256 }
                if mime == "image/png" && *size == IMG.len() as u64 && *sha256 == sha(IMG))
            );
            assert!(authorizations(Terminals::drain(&mut t.b)).is_empty());

            // The same transfer id again (a new signature, so not a frame replay):
            // refused, nothing delivered.
            let again = w.paste(&guest, A, TRANSFER, "image/png", IMG).await;
            assert!(!ok(&again), "{again:?}");
            assert!(authorizations(Terminals::drain(&mut t.a)).is_empty());

            // Another session: no grant there.
            assert!(refused(
                &w.paste(
                    &guest,
                    B,
                    "fedcba9876543210fedcba9876543210",
                    "image/png",
                    IMG
                )
                .await
            ));
            assert!(authorizations(Terminals::drain(&mut t.b)).is_empty());

            // Files: exact names/sizes/hashes reach the terminal in order.
            let files: [(&str, &[u8]); 2] = [("a.txt", b"alpha"), ("b.png", b"beta")];
            let sent = w
                .drop_files(&guest, A, "00112233445566778899aabbccddeeff", &files)
                .await;
            assert!(ok(&sent), "{sent:?}");
            let got = authorizations(Terminals::drain(&mut t.a));
            assert!(matches!(&got[0].1, TransferKind::Files { files: f }
                if f.len() == 2 && f[0].name == "a.txt" && f[1].sha256 == sha(b"beta")));
        });
    }
    #[test]
    fn the_signed_permit_bounds_the_staging_and_pty_effect() {
        if !isolated("write::the_signed_permit_bounds_the_staging_and_pty_effect") {
            return;
        }
        use crate::commands::live::interactive::Shutdown;
        use crate::commands::live::terminal_files::{stage_and_paste_image, Deadline, FileStage};
        struct Recording(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Recording {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let written = Arc::new(Mutex::new(Vec::new()));
        let pty: Arc<Mutex<Box<dyn std::io::Write + Send>>> =
            Arc::new(Mutex::new(Box::new(Recording(written.clone()))));
        let stage = Arc::new(Mutex::new(FileStage::new()));
        let shutdown = Shutdown::new();
        let apply = |permit: Deadline, mime: &'static str| {
            runtime().block_on(stage_and_paste_image(
                &stage, &pty, b"\x89PNG", mime, permit, &shutdown,
            ))
        };

        let late = Deadline::from_claim(crate::commands::local_host::transfers::Claim {
            left: Duration::from_millis(100),
            permit: Default::default(),
        });
        std::thread::sleep(Duration::from_millis(300));
        apply(late, "image/png");
        assert!(
            written.lock().unwrap().is_empty(),
            "an expired permit pasted"
        );

        // A type with no staged extension is refused, never staged under a guessed name.
        apply(
            Deadline::from_claim(crate::commands::local_host::transfers::Claim {
                left: Duration::from_secs(20),
                permit: Default::default(),
            }),
            "image/tiff",
        );
        assert!(written.lock().unwrap().is_empty());

        apply(
            Deadline::from_claim(crate::commands::local_host::transfers::Claim {
                left: Duration::from_secs(20),
                permit: Default::default(),
            }),
            "image/png",
        );
        let pasted = written.lock().unwrap().clone();
        assert_eq!(pasted_images(&pasted), vec![b"\x89PNG".to_vec()]);
        assert!(!pasted.contains(&0x16));
    }

    // ---- Phase 2.5: signed GitHub and host settings -------------------------

    /// A recording `gh` on the child's PATH (one directory per call, one file per
    /// argument, so bodies with newlines survive) that answers the reads `gh` is
    /// asked for with minimal valid JSON.
    pub(super) fn install_recording_gh() -> std::path::PathBuf {
        let tools = std::path::PathBuf::from(std::env::var_os("PATH").unwrap());
        let script = r#"#!/bin/sh
d="${0%/*}/calls/$$"
/bin/mkdir -p "$d"
i=0
for a in "$@"; do printf '%s' "$a" > "$d/$i"; i=$((i+1)); done
case "$*" in
  *"state,isDraft"*) printf '%s' '{"state":"OPEN","isDraft":false,"title":"t","mergedAt":null,"number":7}' ;;
  *"comments,reviews"*) printf '%s' '{"comments":[],"reviews":[]}' ;;
  *graphql*) printf '%s' '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]}}}}}' ;;
esac
exit 0
"#;
        let path = tools.join("gh");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        tools.join("calls")
    }

    /// Every `gh` invocation so far, as argument vectors, oldest first is not
    /// promised: callers compare sets or single calls.
    pub(super) fn gh_calls(calls: &Path) -> Vec<Vec<String>> {
        let Ok(entries) = std::fs::read_dir(calls) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let mut args = Vec::new();
            let mut i = 0;
            while let Ok(text) = std::fs::read_to_string(entry.path().join(i.to_string())) {
                args.push(text);
                i += 1;
            }
            out.push(args);
        }
        out
    }

    /// The publication snapshot [`central_stub`] reports for every session.
    pub(super) const CENTRAL_SNAPSHOT: &str = r#"{"rev":1,"next_seq":4}"#;
    /// The one PR link [`central_stub`] lists for every session.
    const CENTRAL_LINK: &str =
        r#"{"owner":"octo","repo":"repo","number":7,"url":"https://github.com/octo/repo/pull/7"}"#;

    /// A stand-in for the central server: answers every request 200 and records
    /// the request line, so a test sees exactly what the host sent (and asked for).
    /// `GET /api/sessions/{id}/prs` reports the session published at
    /// [`CENTRAL_SNAPSHOT`], linking [`CENTRAL_LINK`]; everything else gets `{}`.
    pub(super) fn central_stub() -> (String, Arc<Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let record = record.clone();
                std::thread::spawn(move || {
                    let mut request = Vec::new();
                    let mut byte = [0u8; 1];
                    while !request.ends_with(b"\r\n\r\n") {
                        if stream.read_exact(&mut byte).is_err() {
                            return;
                        }
                        request.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&request).to_string();
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; length];
                    let _ = stream.read_exact(&mut body);
                    let line = head.lines().next().unwrap_or("").to_owned();
                    record
                        .lock()
                        .unwrap()
                        .push(format!("{line} {}", String::from_utf8_lossy(&body)));
                    let reply = if line.starts_with("GET ") && line.contains("/prs ") {
                        format!(r#"{{"prs":[{CENTRAL_LINK}],"publication":{CENTRAL_SNAPSHOT}}}"#)
                    } else {
                        "{}".to_owned()
                    };
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                            reply.len()
                        )
                        .as_bytes(),
                    );
                });
            }
        });
        (base, seen)
    }

    impl World {
        fn pr_frame(
            &self,
            signer: &Signer,
            op: &str,
            session: &str,
            extra: &str,
            life: i64,
        ) -> SignedFrame {
            let args = format!(
                r#"{{"session":"{session}","owner":"octo","repo":"repo","number":7{extra}}}"#
            );
            self.frame(signer, op, session, &args, life)
        }
    }

    fn tampered(frame: &SignedFrame, from: &str, to: &str) -> SignedFrame {
        let text = String::from_utf8(frame.payload.clone()).unwrap();
        assert!(text.contains(from), "{text}");
        SignedFrame {
            payload: text.replacen(from, to, 1).into_bytes(),
            sig: frame.sig.clone(),
            kid: frame.kid.clone(),
        }
    }

    #[test]
    fn signed_github_operations_run_gh_once_exactly_as_signed_and_nothing_else_does() {
        if !isolated(
            "write::signed_github_operations_run_gh_once_exactly_as_signed_and_nothing_else_does",
        ) {
            return;
        }
        let calls = install_recording_gh();
        let (central, requests) = central_stub();
        runtime().block_on(async {
            let (mut w, _t) = world();
            w.host.server = central;
            let owner = Signer::new();
            let sync_only = Signer::new();
            let guest = Signer::new();
            let other_session = Signer::new();
            w.enroll(
                "owner",
                &owner,
                &[format!("session.pr_comment:{A}"), format!("session.pr_sync:{A}")],
            )
            .unwrap();
            w.enroll("owner", &sync_only, &[format!("session.pr_sync:{A}")]).unwrap();
            w.enroll("owner", &other_session, &[format!("session.pr_comment:{B}")]).unwrap();
            // A recipient can hold write, never the PR capabilities: the ceiling
            // cannot contain them for an agent key and an owner key must be
            // approved for them locally.
            w.enroll("owner", &guest, &[format!("session.write:{A}")]).unwrap();
            let agent = Signer::new();
            assert!(
                w.enroll("agent", &agent, &[format!("session.pr_comment:{A}")]).is_err(),
                "an agent key must not be approvable for PR capabilities"
            );
            assert!(
                w.enroll("owner", &Signer::new(), &["session.pr_comment".to_string()]).is_ok(),
                "an owner key may hold the capability host-wide"
            );

            let body = r#", "body":"-x --body=evil\n$(touch pwned) `id` ; rm -rf /""#;
            let comment = w.pr_frame(&owner, "pr.comment", A, body, 60);

            // 1. ALLOW: exactly one gh call, with exactly the signed arguments.
            let commented = w.send(comment.clone()).await;
            assert!(ok(&commented), "{commented:?}");
            assert!(commented.1.is_none(),"local refresh succeeded: {commented:?}");
            let made = gh_calls(&calls);
            let posts: Vec<_> = made.iter().filter(|c| c.first().map(String::as_str) == Some("pr") && c.get(1).map(String::as_str) == Some("comment")).collect();
            assert_eq!(posts.len(), 1, "{made:?}");
            assert_eq!(
                posts[0],
                &vec![
                    "pr".to_owned(),
                    "comment".into(),
                    "7".into(),
                    "--repo=octo/repo".into(),
                    "--body=-x --body=evil\n$(touch pwned) `id` ; rm -rf /".into(),
                ]
            );
            assert!(!w.dir().join("pwned").exists());
            let posted_so_far = made.len();

            // 2. REPLAY: the identical operation does nothing again.
            assert_eq!(w.send(comment.clone()).await.0, OpStatus::Replayed);
            assert_eq!(gh_calls(&calls).len(), posted_so_far, "replay ran gh");

            // 3. TAMPER: any byte of repo, target number, thread or body changes the
            //    signature input, so zero gh calls.
            let fresh = w.pr_frame(&owner, "pr.comment", A, body, 60);
            for (from, to) in [
                (r#""repo":"repo""#, r#""repo":"evil""#),
                (r#""owner":"octo""#, r#""owner":"mallory""#),
                (r#""number":7"#, r#""number":8"#),
                ("touch pwned", "touch pwnd"),
            ] {
                assert!(refused(&w.send(tampered(&fresh, from, to)).await), "{from}");
            }
            assert_eq!(gh_calls(&calls).len(), posted_so_far, "a mutated frame ran gh");

            // 4. WRONG SESSION / PRINCIPAL / CAPABILITY.
            let on_b = w.pr_frame(&owner, "pr.comment", B, body, 60);
            assert!(refused(&w.send(on_b).await), "owner has no grant on B");
            let wrong_session_signer = w.pr_frame(&other_session, "pr.comment", A, body, 60);
            assert!(refused(&w.send(wrong_session_signer).await));
            let not_comment = w.pr_frame(&sync_only, "pr.comment", A, body, 60);
            assert!(refused(&w.send(not_comment).await), "sync-only key commented");
            let recipient = w.pr_frame(&guest, "pr.comment", A, body, 60);
            assert!(refused(&w.send(recipient).await), "a session.write recipient commented");
            // PR refresh now follows session.write: a holder of only the
            // legacy standalone pr_sync cap no longer authorizes pr.sync on
            // its own, while a session.write recipient may refresh the
            // session's linked PRs.
            assert!(refused(&w.send(w.pr_frame(&guest, "pr.sync", B, "", 60)).await), "a recipient writer on A refreshed B");
            let reader = Signer::new();
            w.enroll("owner", &reader, &[format!("history.read:{A}")]).unwrap();
            assert!(refused(&w.send(w.pr_frame(&reader, "pr.sync", A, "", 60)).await), "a read-only recipient refreshed PR metadata");
            let sync_only_pr_sync = w.pr_frame(&sync_only, "pr.sync", A, "", 60);
            assert!(
                refused(&w.send(sync_only_pr_sync).await),
                "standalone pr_sync credential still authorized pr.sync"
            );
            let stranger = Signer::new();
            assert!(refused(&w.send(w.pr_frame(&stranger, "pr.comment", A, body, 60)).await));
            assert_eq!(gh_calls(&calls).len(), posted_so_far);

            // 5. EXPIRY: a correctly signed but expired operation.
            let now = now_secs();
            let args = format!(r#"{{"session":"{A}","owner":"octo","repo":"repo","number":7{body}}}"#);
            let stale = owner.sign_bytes(
                owner
                    .op_payload(&w.host_id, "pr.comment", A, &args, &w.next_jti(), now - 100, now - 40)
                    .as_bytes(),
            );
            assert!(refused(&w.send(stale).await));
            assert_eq!(gh_calls(&calls).len(), posted_so_far);

            // 6. INJECTION SHAPES are refused by the verifier before any effect.
            for (owner_name, repo_name, number) in [
                ("--help", "repo", "7"),
                ("octo", "--web", "7"),
                ("octo", "..", "7"),
                ("oc/to", "repo", "7"),
                ("octo", "re po", "7"),
                ("octo", "repo", "0"),
                ("octo", "repo", "-3"),
            ] {
                let args = format!(
                    r#"{{"session":"{A}","owner":"{owner_name}","repo":"{repo_name}","number":{number}{body}}}"#
                );
                assert!(refused(&w.send(w.frame(&owner, "pr.comment", A, &args, 60)).await), "{owner_name}/{repo_name}#{number}");
            }
            // Unknown or extra fields at any depth, and a session-less comment.
            let extra = w.pr_frame(&owner, "pr.comment", A, &format!(r#"{body},"cmd":"x""#), 60);
            assert!(refused(&w.send(extra).await));
            assert!(refused(&w.send(w.frame(&owner, "pr.comment", "host", &format!(r#"{{"owner":"octo","repo":"repo","number":7{body}}}"#), 60)).await));
            assert_eq!(gh_calls(&calls).len(), posted_so_far);

            // 7. Explicit PRIVATE sync refreshes only the host cache. The
            //    signer here holds session.write on A, which (post-rule)
            //    admits pr.sync.
            let sync=w.pr_frame(&guest,"pr.sync",A,"",60);
            let private_sync=w.send(sync).await;
            assert_eq!(private_sync.0,OpStatus::Executed,"{private_sync:?}");
            assert_eq!(authz::pr_targets(w.dir(),A).unwrap(),vec![("octo".to_owned(),"repo".to_owned(),7)]);
            assert_eq!(gh_calls(&calls).len(),posted_so_far+3);
            let posted_so_far=gh_calls(&calls).len();
            let (cache,_,_)=crate::commands::local_host::pr_metadata::chunk(w.dir(),A,0,None).unwrap();
            let cache:Value=serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(cache).unwrap()).unwrap();
            assert_eq!(cache["prs"][0]["owner"],"octo");assert_eq!(cache["prs"][0]["number"],7);
            assert_eq!(cache["prs"][0]["status"]["title"],"t");
            assert_eq!(cache["comments"],json!([]));
            let private: Vec<String> = requests.lock().unwrap().iter().filter(|r| r.contains("/prs")).cloned().collect();
            assert!(private.is_empty(), "private PR content reached the server: {private:?}");

            //    Sharing A does not publish every PR the host may read: while no
            //    message the server holds links this one, the sync reports why
            //    and sends no body, although the central list claims the link.
            crate::commands::local_host::pr_cache_tests::set_local_publication(w.dir(), A, "publishing", 1, &w.host.server);
            let sync = w.pr_frame(&guest, "pr.sync", A, "", 60);
            let unlinked = w.send(sync).await;
            assert_eq!(unlinked.0, OpStatus::Failed, "{unlinked:?}");
            assert!(unlinked.1.as_deref().is_some_and(|d| d.contains("recorded") && d.contains("not linked")), "{unlinked:?}");
            assert_eq!(gh_calls(&calls).len(), posted_so_far, "an unlinked PR was read");
            assert!(!requests.lock().unwrap().iter().any(|r| r.starts_with("POST ") && r.contains("/prs")), "an unlinked PR cache reached the server");

            //    Once a message linking it is published, a sync reads that PR
            //    only, publishes it under the central snapshot read before gh,
            //    and the target is remembered for the local poll. Nothing asked
            //    the server for a list of work.
            crate::commands::local_host::pr_cache_tests::published_link(w.dir(), A);
            let sync = w.pr_frame(&guest, "pr.sync", A, "", 60);
            assert!(ok(&w.send(sync).await));
            let after_sync = gh_calls(&calls);
            assert!(after_sync.len() > posted_so_far);
            assert!(after_sync.iter().all(|c| {
                c.first().map(String::as_str) != Some("pr")
                    || c.get(1).map(String::as_str) != Some("view")
                    || c.contains(&"7".to_owned())
            }));
            let pushed: Vec<String> = requests.lock().unwrap().clone();
            for path in ["status", "comments"] {
                let push = pushed.iter().find(|r| r.starts_with(&format!("POST /api/sessions/{A}/prs/{path} "))).unwrap_or_else(|| panic!("{path}: {pushed:?}"));
                let body: Value = serde_json::from_str(push.split_once("HTTP/1.1 ").unwrap().1).unwrap();
                assert_eq!(body["publication"], serde_json::from_str::<Value>(CENTRAL_SNAPSHOT).unwrap(), "{push}");
            }
            assert!(
                pushed.iter().all(|r| !r.contains("/prs/watch/targets") && !r.contains("/prs/sync/stream") && !r.contains("/prs/reply/stream")),
                "the host asked the server for work: {pushed:?}"
            );
            assert_eq!(authz::pr_targets(w.dir(), A).unwrap(), vec![("octo".to_owned(), "repo".to_owned(), 7)]);

            // 8. POLL: reads only what a signed operation recorded, only for live
            //    sessions. A target the server would offer is never read.
            let before_poll = gh_calls(&calls).len();
            crate::commands::local_host::daemon::pr_poll_once(&w.host).await;
            assert_eq!(gh_calls(&calls).len(), before_poll, "manual refresh must postpone immediate background gh");
            rusqlite::Connection::open(w.dir().join("authz.db")).unwrap().execute("UPDATE session_pr_targets SET next_poll_at=0 WHERE session_id=?1", [A]).unwrap();
            Store::open(w.dir()).unwrap().record_pr_target(B, "bee", "hive", 3, None, now_secs()).unwrap();
            w.host.sessions.lock().unwrap().remove(B);
            crate::commands::local_host::daemon::pr_poll_once(&w.host).await;
            let polled = gh_calls(&calls);
            assert!(polled.len() > before_poll, "the recorded target of a live session was not polled");
            assert!(
                polled.iter().all(|c| !c.iter().any(|a| a == "bee/hive" || a == "owner=bee" || a == "repo=hive" || a.starts_with("--repo=mallory") || a == "mallory/repo" || a == "evil/repo" || a == "octo/evil")),
                "a target nobody signed for (or of a session not running) was read: {polled:?}"
            );
            assert!(
                requests.lock().unwrap().iter().all(|r| !r.contains("watch/targets")),
                "the poll asked the server for its target list"
            );
        });
    }

    /// Replace the launch block through the real browser→relay→host path (a raw
    /// signed frame standing in for the browser key) and read it back.
    async fn relay_settings(
        w: &World,
        frame: SignedFrame,
    ) -> conversations_core::host_relay::RelayReply {
        use conversations_core::host_relay::RelayRequest;
        let b =
            |x: &[u8]| base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, x);
        super::relay::respond(
            &w.host,
            RelayRequest::SignedOp {
                p: b(&frame.payload),
                s: b(&frame.sig),
                k: frame.kid.clone(),
            },
        )
        .await
    }

    fn launch_args(skip: bool, sandbox: &str) -> String {
        format!(
            r#"{{"launch":{{"claude_skip_permissions":{skip},"opencode_auto":true,"codex_sandbox_mode":{sandbox},"codex_approval_policy":null,"default_launcher":"adk","adk_statusline":true}}}}"#
        )
    }

    #[test]
    fn prompts_keystrokes_and_settings_are_refused_where_the_server_can_read_them() {
        if !isolated(
            "write::prompts_keystrokes_and_settings_are_refused_where_the_server_can_read_them",
        ) {
            return;
        }
        runtime().block_on(async {
            let (w, _t) = world();
            let owner = Signer::new();
            w.enroll(
                "owner",
                &owner,
                &[format!("session.write:{A}"), "host.settings_write".into()],
            )
            .unwrap();
            let input = format!(r#"{{"session":"{A}","ch":"{CH}","seq":1,"b64":"bHM="}}"#);
            for frame in [
                w.frame(
                    &owner,
                    "session.reply",
                    A,
                    &format!(r#"{{"session":"{A}","text":"secret"}}"#),
                    60,
                ),
                w.frame(&owner, "session.input", A, &input, 20),
                w.frame(
                    &owner,
                    "settings.update",
                    "host",
                    &launch_args(true, "null"),
                    60,
                ),
                w.frame(&owner, "session.start", A, r#"{"cwd":"/private/project","prompt":"private start"}"#, 60),
                w.frame(&owner, "session.reopen", A, r#"{"session":"private resume"}"#, 60),
                w.frame(&owner, "pr.sync", A, r#"{"session":"private target","owner":"private-owner","repo":"private-repo","number":7}"#, 60),
                w.frame(&owner, "pr.comment", A, r#"{"session":"private target","owner":"private-owner","repo":"private-repo","number":7,"body":"private comment"}"#, 60),
                w.frame(&owner, "session.paste_image", A, r#"{"session":"private image","id":"private-id"}"#, 60),
                w.frame(&owner, "session.send_files", A, r#"{"session":"private files","files":[]}"#, 60),
                w.frame(&owner, "session.end", A, &format!(r#"{{"session":"{A}"}}"#), 60),
                w.frame(&owner, "session.resize", A, &format!(r#"{{"session":"{A}","cols":80,"rows":24}}"#), 20),
                w.frame(&owner, "session.repaint", A, &format!(r#"{{"session":"{A}"}}"#), 20),
                w.frame(&owner, "grant.add", A, r#"{"recipient":"approved-key","caps":["history.read"]}"#, 60),
                w.frame(&owner, "grant.remove", A, r#"{"recipient":"approved-key","caps":["history.read"]}"#, 60),
                w.frame(&owner, "publication.share", A, &format!(r#"{{"session":"{A}","audience":{{"visibility":"organization"}}}}"#), 60),
                w.frame(&owner, "publication.unshare", A, &format!(r#"{{"session":"{A}"}}"#), 60),
                w.frame(&owner, "publication.status", A, &format!(r#"{{"session":"{A}"}}"#), 60),
            ] {
                // The relay uses this entry point.
                let (status, detail) = super::dispatch_signed(&w.host, frame.clone()).await;
                assert_eq!(status, OpStatus::Refused);
                assert!(detail.unwrap().contains("only on a private connection"));
                // Refused before admission: the frame was not spent, so the
                // channel can still carry it.
                assert_ne!(w.send(frame).await.0, OpStatus::Replayed);
            }
        });
    }

    #[test]
    fn signed_settings_persist_locally_drive_launches_and_cannot_touch_trust_config() {
        if !isolated(
            "write::signed_settings_persist_locally_drive_launches_and_cannot_touch_trust_config",
        ) {
            return;
        }
        use crate::commands::local_host::settings::Settings;
        use conversations_core::host_relay::RelayReply;
        runtime().block_on(async {
            let (w, _t) = world();
            let path = Settings::path().unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                r#"{"cmux":{"auto_wrap":true},"launch":{},"webauthn":{"rp_id":"conversations.example","origins":["https://conversations.example"]}}"#,
            )
            .unwrap();
            let trust_before = Settings::load(w.dir()).unwrap().webauthn;
            let owner = Signer::new();
            let reader = Signer::new();
            let guest = Signer::new();
            w.enroll("owner", &owner, &["host.settings_read".into(), "host.settings_write".into()]).unwrap();
            w.enroll("owner", &reader, &["host.settings_read".into()]).unwrap();
            w.enroll("owner", &guest, &[format!("session.write:{A}")]).unwrap();
            assert!(
                w.enroll("agent", &Signer::new(), &["host.settings_write".into()]).is_err(),
                "an agent key must not be approvable for settings"
            );
            assert!(
                w.enroll("owner", &Signer::new(), &[format!("host.settings_write:{A}")]).is_err(),
                "settings authority is host-wide only"
            );

            let get = |who: &Signer| w.frame(who, "settings.get", "host", "{}", 60);
            let update = |who: &Signer, args: &str| w.frame(who, "settings.update", "host", args, 60);
            let launch_of = |frame: SignedFrame| -> serde_json::Value {
                super::private_channel::channel_settings(w.dir(), &frame).expect("settings refused")
            };
            let file_bytes = || std::fs::read(&path).unwrap();

            // READ only on the private channel: the relay refuses it, the
            // channel answers with the flags only.
            assert!(matches!(relay_settings(&w, get(&owner)).await, RelayReply::Op { .. }));
            assert!(refused(&w.send(get(&owner)).await), "settings were answered off the channel");
            let initial = launch_of(get(&owner));
            assert_eq!(initial["claude_skip_permissions"], false);
            assert!(initial.as_object().unwrap().keys().all(|k| matches!(k.as_str(),
                "claude_skip_permissions" | "opencode_auto" | "codex_sandbox_mode" | "codex_approval_policy" | "default_launcher" | "adk_statusline")));

            // UNAUTHORIZED principals: zero mutation, no data.
            let untouched = file_bytes();
            for who in [&reader, &guest, &Signer::new()] {
                assert!(refused(&w.send(update(who, &launch_args(true, "null"))).await));
            }
            assert!(super::private_channel::channel_settings(w.dir(), &get(&guest)).is_none(), "a recipient read settings");
            assert_eq!(file_bytes(), untouched, "an unauthorized writer changed the file");

            // WRITE as the owner: persisted in the local file, trust config intact.
            // The relay (readable by the central server) refuses the write
            // untouched; the private channel carries it.
            let write = update(&owner, &launch_args(true, r#""read-only""#));
            assert!(matches!(relay_settings(&w, write.clone()).await, RelayReply::Op { .. }));
            assert_eq!(file_bytes(), untouched, "the relay wrote settings");
            assert_eq!(w.send(write.clone()).await.0, OpStatus::Executed);
            let saved = Settings::load(w.dir()).unwrap();
            assert!(saved.launch.claude_skip_permissions && saved.launch.opencode_auto && saved.launch.adk_statusline);
            assert_eq!(saved.launch.codex_sandbox_mode.as_deref(), Some("read-only"));
            assert_eq!(saved.launch.default_launcher, conversations_core::AgentLauncher::Adk);
            assert_eq!(saved.webauthn.rp_id, trust_before.rp_id);
            assert_eq!(saved.webauthn.origins, trust_before.origins);
            assert!(saved.cmux.auto_wrap, "an unrelated local setting was dropped");
            let reread = launch_of(get(&reader));
            assert_eq!(reread["claude_skip_permissions"], true);

            // REPLAY: the same signed update does not write again.
            let once = file_bytes();
            std::fs::write(&path, String::from_utf8(once.clone()).unwrap().replace("\"opencode_auto\": true", "\"opencode_auto\": false")).unwrap();
            let tampered_file = file_bytes();
            assert_eq!(w.send(write).await.0, OpStatus::Replayed);
            assert_eq!(file_bytes(), tampered_file, "a replayed update rewrote the file");
            std::fs::write(&path, &once).unwrap();

            // TRUST CONFIG and anything else: not representable, zero mutation.
            let before = file_bytes();
            for args in [
                r#"{"launch":{"claude_skip_permissions":false,"opencode_auto":false,"codex_sandbox_mode":null,"codex_approval_policy":null,"default_launcher":"opencode","adk_statusline":false},"webauthn":{"rp_id":"evil.example","origins":["https://evil.example"]}}"#,
                r#"{"launch":{"claude_skip_permissions":false,"opencode_auto":false,"codex_sandbox_mode":null,"codex_approval_policy":null,"default_launcher":"opencode","adk_statusline":false,"origins":["https://evil.example"]}}"#,
                r#"{"launch":{"claude_skip_permissions":false,"opencode_auto":false,"codex_sandbox_mode":"--yolo","codex_approval_policy":null,"default_launcher":"opencode","adk_statusline":false}}"#,
                r#"{"launch":{"claude_skip_permissions":false,"opencode_auto":false,"codex_approval_policy":null,"default_launcher":"opencode","adk_statusline":false}}"#,
                r#"{"launch":{"claude_skip_permissions":"yes","opencode_auto":false,"codex_sandbox_mode":null,"codex_approval_policy":null,"default_launcher":"opencode","adk_statusline":false}}"#,
                r#"{"credentials":[{"kind":"owner"}]}"#,
            ] {
                assert!(refused(&w.send(update(&owner, args)).await), "{args}");
            }
            assert!(refused(&w.send(w.frame(&owner, "settings.update", A, &launch_args(false, "null"), 60)).await), "a settings update scoped to a session");
            assert!(refused(&w.send(w.frame(&owner, "settings.get", A, "{}", 60)).await));
            assert_eq!(file_bytes(), before, "a refused update changed the file");
            assert_eq!(Settings::load(w.dir()).unwrap().webauthn.rp_id, trust_before.rp_id);

            // LAUNCH uses the local flags: start and reopen are built from this
            // file, and nothing central is consulted.
            use crate::commands::local_host::authz::envelope::Operation;
            let settings = Settings::load(w.dir()).unwrap();
            let (_, command) = authz::dispatch::start_command(
                &settings,
                &Operation::SessionStart {
                    session: B.into(),
                    agent: conversations_core::AgentKind::Claude,
                    launcher: None,
                },
            )
            .unwrap();
            let crate::commands::live::host::ControlCommand::StartNew { agent_args, .. } = command else { panic!() };
            assert_eq!(agent_args, vec!["--dangerously-skip-permissions".to_owned()]);
            let record = authz::lookup(w.dir(), A).unwrap().unwrap();
            let crate::commands::live::host::ControlCommand::Reopen { agent_args, .. } =
                authz::dispatch::reopen_command(&settings, &record).unwrap() else { panic!() };
            assert_eq!(agent_args, vec!["--dangerously-skip-permissions".to_owned()]);
        });
    }

    // ---- Phase 2.3: signed agent messaging and same-machine handoff ---------

    type Rewrite = Box<dyn Fn(&mut serde_json::Value) + Send>;

    #[derive(Default)]
    struct Relay {
        routes: Mutex<HashMap<String, Arc<Host>>>,
        mutate: Mutex<Option<Rewrite>>,
        seen: Mutex<Vec<serde_json::Value>>,
        /// Swallow the request (the host never sees it) / swallow the answer.
        drop_request: std::sync::atomic::AtomicBool,
        drop_reply: std::sync::atomic::AtomicBool,
        /// Every request path the central stand-in saw, and the (hostile) answer
        /// it gives to the legacy identity lookup.
        paths: Mutex<Vec<String>>,
        identity: Mutex<serde_json::Value>,
        /// Every genuine host answer it relayed, and a captured answer it hands
        /// back instead of asking any host (replay / swap).
        replies: Mutex<Vec<Vec<u8>>>,
        answer: Mutex<Option<Vec<u8>>>,
    }

    /// An untrusted central relay stand-in: plaintext HTTP in front of the real
    /// `relay::respond` of whichever host the route table names. It can rewrite
    /// request bytes and route to any host, like a hostile server.
    async fn relay_stub(relay: Arc<Relay>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let relay = relay.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if stream.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; length];
                    let _ = stream.read_exact(&mut body).await;
                    relay.paths.lock().unwrap().push(path.clone());
                    if path.ends_with("/identity") {
                        let payload = relay.identity.lock().unwrap().to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        return;
                    }
                    let mut value: serde_json::Value =
                        serde_json::from_slice(&body).unwrap_or_default();
                    relay.seen.lock().unwrap().push(value.clone());
                    if let Some(edit) = relay.mutate.lock().unwrap().as_ref() {
                        edit(&mut value);
                    }
                    let host_id = path.split('/').nth(3).unwrap_or("").to_owned();
                    let host = relay.routes.lock().unwrap().get(&host_id).cloned();
                    let lose = |flag: &std::sync::atomic::AtomicBool| {
                        flag.load(std::sync::atomic::Ordering::SeqCst)
                    };
                    let answer = match (host, serde_json::from_value(value)) {
                        (Some(_), Ok(_)) if lose(&relay.drop_request) => None,
                        (Some(_), Ok(_)) if relay.answer.lock().unwrap().is_some() => {
                            relay.answer.lock().unwrap().clone()
                        }
                        (Some(host), Ok(request)) => {
                            // Routed exactly as the production relay routes it.
                            let reply = match request {
                                conversations_core::host_relay::RelayRequest::PeerOffer {
                                    p,
                                    s,
                                    k,
                                    sdp,
                                } => super::private_channel::offer(&host, &p, &s, &k, sdp).await,
                                request => super::relay::respond(&host, request).await,
                            };
                            let bytes = serde_json::to_vec(&reply).unwrap();
                            relay.replies.lock().unwrap().push(bytes.clone());
                            (!lose(&relay.drop_reply)).then_some(bytes)
                        }
                        _ => None,
                    };
                    let (status, payload) = match answer {
                        Some(bytes) => ("200 OK", bytes),
                        None => ("404 Not Found", b"{}".to_vec()),
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(&payload).await;
                });
            }
        });
        base
    }

    /// Serve `host` on `<dir>/host.sock` the way the daemon does.
    fn serve_socket(dir: &Path, host: Arc<Host>) {
        let listener = tokio::net::UnixListener::bind(dir.join("host.sock")).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(handle_client(host.clone(), stream));
            }
        });
    }

    fn arc_host(w: &World) -> (Arc<Host>, Terminals) {
        let host = Arc::new(w.fixture.host(w.fixture.server()));
        let (a, b) = register(&host);
        (host, Terminals { a, b })
    }

    fn inbox_of(dir: &Path, session: &str) -> Vec<authz::store::InboxRow> {
        Store::open(dir)
            .unwrap()
            .inbox_deliverable(session, now_secs(), 50)
            .unwrap()
    }

    fn approve_agent(receiver: &Path, sender: &Path, sessions: &[&str]) -> String {
        let (public, kid) = crate::commands::local_host::agent_link::agent_public(sender).unwrap();
        let grants: Vec<String> = sessions
            .iter()
            .map(|s| format!("session.write:{s}"))
            .collect();
        authz::enroll_from_cli(receiver, "agent", "ed25519", &public, "sender", &grants).unwrap();
        kid
    }

    fn send_body(target: &str, text: &str, idem: &str) -> serde_json::Value {
        json!({"recipient_session_id": target, "text": text, "idempotency_key": idem, "reply_to": null})
    }

    #[test]
    fn signed_agent_messages_travel_from_the_mcp_request_to_the_receiving_hosts_inbox_and_nowhere_else(
    ) {
        if !isolated("write::signed_agent_messages_travel_from_the_mcp_request_to_the_receiving_hosts_inbox_and_nowhere_else") {
            return;
        }
        use crate::commands::local_host::agent_link;
        use crate::commands::messaging::MessagingClient;
        runtime().block_on(async {
            let (receiver, _unused) = world();
            let (rhost, mut rterm) = arc_host(&receiver);
            let (sender, _unused2) = world();
            let (shost, mut sterm) = arc_host(&sender);
            serve_socket(sender.dir(), shost.clone());
            // A wrapper "connecting" for every terminal the sender host launches.
            {
                let shost = shost.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        let ids: Vec<String> = shost.pending.lock().unwrap().keys().cloned().collect();
                        for id in ids {
                            if let Some(tx) = shost.pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(());
                            }
                        }
                    }
                });
            }
            let (rogue, _unused3) = world();
            let (rogue_host, mut rogue_term) = arc_host(&rogue);
            loopback_private_channel(receiver.dir());

            let relay = Arc::new(Relay {
                routes: Mutex::new(HashMap::from([(receiver.host_id.clone(), rhost.clone())])),
                ..Default::default()
            });
            let central = relay_stub(relay.clone()).await;
            // Sender B (a registered session of the sending host) messages A on
            // the receiving host. A cookie is all central ever sees.
            let client = MessagingClient::with_cookie(&central, B, Some("conversations.sid=x".into()))
                .unwrap()
                .with_local_dir(sender.dir().to_path_buf());
            let recv_client = MessagingClient::with_cookie(&central, A, None)
                .unwrap()
                .with_local_dir(receiver.dir().to_path_buf());
            let send = |text: &str, idem: &str, target: &str| {
                let client = client.clone();
                let body = send_body(target, text, idem);
                async move { client.request(reqwest::Method::POST, &["messages"], Some(body)).await }
            };
            let requests = || relay.seen.lock().unwrap().len();

            // 0. Not paired: no destination, no network traffic at all. Discovery
            //    (central peers) cannot supply one.
            let unpaired = send("hi", "k0", A).await.unwrap_err().to_string();
            assert!(unpaired.contains("not paired"), "{unpaired}");
            assert_eq!(requests(), 0);

            // Pair the recipient from ITS descriptor (local, by hand).
            let descriptor = authz::attest::descriptor(receiver.dir()).unwrap();
            agent_link::pair_peer(sender.dir(), &descriptor, A, "receiver A").unwrap();
            agent_link::pair_peer(sender.dir(), &descriptor, B, "receiver B").unwrap();

            // 1. Unknown key: the receiver never approved this machine's agent key.
            // The host refuses the unknown key when the private connection is
            // offered; the full error chain carries its refusal.
            let denied = format!("{:#}", send("hi", "k1", A).await.unwrap_err());
            assert!(denied.contains("refused"), "{denied}");
            assert!(inbox_of(receiver.dir(), A).is_empty());

            // 2. Approve ONCE, granting session A only.
            let kid = approve_agent(receiver.dir(), sender.dir(), &[A]);
            let sent = send("please look at the failing test", "k2", A).await.map_err(|e| format!("{e:#}")).unwrap();
            assert_eq!(sent["status"], "queued");
            let message_id = sent["id"].as_str().unwrap().to_owned();
            let rows = inbox_of(receiver.dir(), A);
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].text, "please look at the failing test");
            assert_eq!((rows[0].source_session.as_str(), rows[0].kid.as_str()), (B, kid.as_str()));
            // Queueing is NOT terminal input.
            assert!(Terminals::drain(&mut rterm.a).is_empty());
            // A retry of the same send is idempotent.
            send("please look at the failing test", "k2", A).await.unwrap();
            assert_eq!(inbox_of(receiver.dir(), A).len(), 1);

            // 3. Receiver-side tool: reads the host's own inbox, honest states.
            let got = recv_client
                .request(reqwest::Method::POST, &["inbox"], Some(json!({"wait_seconds": 0})))
                .await
                .unwrap();
            assert_eq!(got["messages"][0]["id"], message_id.as_str());
            assert_eq!(got["messages"][0]["sender_session_id"], B);
            let status = recv_client.request(reqwest::Method::GET, &["messages", &message_id], None).await.unwrap();
            assert_eq!((status["status"].as_str(), status["direction"].as_str()), (Some("received"), Some("received")), "{status}");
            recv_client
                .request(reqwest::Method::POST, &["inbox"], Some(json!({"acknowledge": [message_id]})))
                .await
                .unwrap();
            let status = recv_client.request(reqwest::Method::GET, &["messages", &message_id], None).await.unwrap();
            assert_eq!(status["status"], "acknowledged");

            // 4. Session B was not granted: refused, nothing stored, no terminal write.
            let b = send("and B?", "k3", B).await.unwrap_err().to_string();
            assert!(b.contains("refused"), "{b}");
            assert!(inbox_of(receiver.dir(), B).is_empty());
            assert!(Terminals::drain(&mut rterm.b).is_empty());

            // 5. Decrypted component signature proof (NOT a relay rewriting
            //    encrypted traffic, which it cannot read): the actual stored,
            //    admitted envelope with its text, target or source changed under
            //    the ORIGINAL signature is refused, so nothing is stored or typed.
            let admitted = Store::open(sender.dir()).unwrap().send_get(B, A, "k2").unwrap().unwrap();
            let before = inbox_of(receiver.dir(), A).len() + message_count(receiver.dir());
            for (from, to) in [("failing", "passing"), ("\"sess\":\"3f1d", "\"sess\":\"5e1a"), (B, A)] {
                let original = String::from_utf8(admitted.payload.clone()).unwrap();
                let tampered = original.replace(from, to);
                assert_ne!(tampered, original, "{from} is not in the envelope");
                let (status, detail) = deliver_signed(
                    &rhost,
                    SignedFrame { payload: tampered.into_bytes(), sig: admitted.sig.clone(), kid: admitted.kid.clone() },
                )
                .await;
                assert_eq!(
                    (status, detail.as_deref()),
                    (OpStatus::Refused, Some("signature does not verify")),
                    "{from}"
                );
            }
            assert_eq!(inbox_of(receiver.dir(), A).len() + message_count(receiver.dir()), before);
            assert!(Terminals::drain(&mut rterm.a).is_empty());

            // 6. Replay of the exact stored, admitted bytes and of an expired
            //    envelope, on the same decrypted component seam (the central
            //    stand-in only sees signaling, never these bytes).
            send("replay me", "k4", A).await.unwrap();
            let admitted = Store::open(sender.dir()).unwrap().send_get(B, A, "k4").unwrap().unwrap();
            let rows_before = message_count(receiver.dir());
            let (replayed, detail) = deliver_signed(
                &rhost,
                SignedFrame { payload: admitted.payload, sig: admitted.sig, kid: admitted.kid },
            )
            .await;
            assert_eq!(replayed, OpStatus::Replayed, "{detail:?}");
            assert_eq!(message_count(receiver.dir()), rows_before, "a replayed frame stored again");
            let stale = prep(sender.dir(), "late", "k5", now_secs() - 1000);
            let (status, detail) = deliver_signed(
                &rhost,
                SignedFrame { payload: stale.payload, sig: stale.sig, kid: stale.kid },
            )
            .await;
            assert_eq!(status, OpStatus::Refused, "{detail:?}");
            assert!(detail.as_deref().is_some_and(|d| d.contains("expired")), "{detail:?}");
            assert_eq!(message_count(receiver.dir()), rows_before);

            // 7. A wrong host injected centrally: the frame is audience-bound to the
            //    PINNED host, so a rogue host (even one that approved the same key)
            //    stores nothing and runs nothing, and the sender rejects its answer.
            let rogue_kid = approve_agent(rogue.dir(), sender.dir(), &[A]);
            assert_eq!(rogue_kid, kid);
            relay.routes.lock().unwrap().insert(receiver.host_id.clone(), rogue_host.clone());
            let wrong = format!("{:#}", send("private", "k6", A).await.unwrap_err());
            assert!(wrong.contains("refused") || wrong.contains("not signed"), "{wrong}");
            assert!(inbox_of(rogue.dir(), A).is_empty());
            assert_eq!(rogue.fixture.launches(), 0);
            assert!(Terminals::drain(&mut rogue_term.a).is_empty());
            relay.routes.lock().unwrap().insert(receiver.host_id.clone(), rhost.clone());

            // 8. Revoked row: a grant removed directly in the database stops both
            //    new messages and one already queued (never handed to the consumer).
            let queued = send("queued before revoke", "k7", A).await.unwrap();
            assert!(inbox_of(receiver.dir(), A).iter().any(|r| r.message_id == queued["id"].as_str().unwrap()));
            Store::open(receiver.dir()).unwrap().connection()
                .execute("UPDATE grants SET revoked_at = 1 WHERE cred_id = ?1", [&kid]).unwrap();
            assert!(inbox_of(receiver.dir(), A).is_empty(), "a queued message outlived its grant");
            assert!(send("after revoke", "k8", A).await.is_err());

            // 9. No remote session creation or handoff: an approved AGENT key's
            //    start is refused and launches nothing; so is an agent.message that
            //    smuggles handoff fields.
            let agent = Signer::new();
            receiver.enroll("agent", &agent, &[format!("session.write:{A}")]).unwrap();
            assert!(refused(&receiver.send(receiver.frame(&agent, "session.start", A, &format!(r#"{{"session":"{A}","agent":"zsh"}}"#), 60)).await));
            assert!(refused(&receiver.send(receiver.frame(&agent, "session.reopen", A, &format!(r#"{{"session":"{A}"}}"#), 60)).await));
            let extra = format!(r#"{{"session":"{A}","source":"{B}","text":"x","reply_to":null,"handoff":{{"brief":"x"}}}}"#);
            assert!(refused(&receiver.send(receiver.frame(&agent, "agent.message", A, &extra, 60)).await));
            assert_eq!(receiver.fixture.launches(), 0);
            // Per-key ceiling is non-delegating: an agent key cannot share anything.
            let share = receiver.grant_add(&agent, A, &Signer::new(), "session.write").await;
            assert!(!ok(&share));
            assert!(receiver.enroll("agent", &Signer::new(), &["session.manage_sharing".into()]).is_err());
            assert!(receiver.enroll("agent", &Signer::new(), &["session.write".into()]).is_err(), "no host-wide agent grants");

            // 10. Genuine same-machine handoff: the tool asks the owner-only host
            //     socket; the child starts from LOCAL settings. Retry is idempotent.
            crate::commands::local_host::settings::replace_launch(
                sender.dir(),
                crate::commands::local_host::settings::LaunchSettings {
                    claude_skip_permissions: true,
                    ..Default::default()
                },
            ).unwrap();
            let handoff = client
                .request(reqwest::Method::POST, &["handoffs"], Some(json!({"brief": "continue the migration", "idempotency_key": "h1", "agent": "claude", "model": "opus", "variant": "high"})))
                .await
                .unwrap();
            assert_eq!(handoff["status"], "started", "{handoff}");
            assert_eq!(sender.fixture.launches(), 1, "{}", sender.fixture.launch_log());
            let log = sender.fixture.launch_log();
            assert!(log.contains("continue the migration") && log.contains("--dangerously-skip-permissions"), "{log}");
            let again = client
                .request(reqwest::Method::POST, &["handoffs"], Some(json!({"brief": "continue the migration", "idempotency_key": "h1", "agent": "claude", "model": "opus", "variant": "high"})))
                .await
                .unwrap();
            assert_eq!(again["session_id"], handoff["session_id"]);
            assert_eq!(handoff["model"], "opus");
            assert!(log.contains("--model") && log.contains("opus") && log.contains("--effort") && log.contains("high"), "{log}");
            assert!(client.request(reqwest::Method::POST, &["handoffs"], Some(json!({"brief":"continue the migration", "idempotency_key":"h1", "agent":"claude", "model":"sonnet", "variant":"high"}))).await.is_err());
            assert!(client.request(reqwest::Method::POST, &["handoffs"], Some(json!({"brief":"x", "idempotency_key":"invalid-model", "agent":"claude", "model":"--evil"}))).await.is_err());
            assert!(client.request(reqwest::Method::POST, &["end"], Some(json!({"session_id": B}))).await.is_err(), "an unrelated local session is not a handoff child");
            let ended = client.request(reqwest::Method::POST, &["end"], Some(json!({"session_id":handoff["session_id"]}))).await.unwrap();
            assert!(matches!(ended["status"].as_str(), Some("ending" | "not_running")), "{ended}");
            assert!(ended["host_notified"].is_boolean());

            assert_eq!(sender.fixture.launches(), 1);
            let conflict = client
                .request(reqwest::Method::POST, &["handoffs"], Some(json!({"brief": "something else", "idempotency_key": "h1"})))
                .await;
            assert!(conflict.is_err());
            // A session the host does not run cannot hand off.
            let stranger = MessagingClient::with_cookie(&central, "9b2f6a40-2a43-4a54-8fd0-69f3a1b5fa55", None)
                .unwrap()
                .with_local_dir(sender.dir().to_path_buf());
            assert!(stranger
                .request(reqwest::Method::POST, &["handoffs"], Some(json!({"brief": "x", "idempotency_key": "h2"})))
                .await
                .is_err());
            assert_eq!(sender.fixture.launches(), 1);
            let _ = (&mut sterm.a, &mut sterm.b);
        });
    }

    fn prep(
        dir: &Path,
        text: &str,
        idem: &str,
        now: i64,
    ) -> crate::commands::local_host::agent_link::Prepared {
        use crate::commands::local_host::agent_link::{begin_send, Begin};
        match begin_send(dir, B, A, text, None, idem, now).unwrap() {
            Begin::Post(prepared) => prepared,
            _ => panic!("expected a fresh envelope"),
        }
    }

    fn message_count(dir: &Path) -> usize {
        Store::open(dir)
            .unwrap()
            .connection()
            .query_row("SELECT COUNT(*) FROM agent_inbox", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap() as usize
    }

    fn reply_status(reply: &conversations_core::host_relay::RelayReply) -> String {
        let value = serde_json::to_value(reply).unwrap();
        let bytes = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            value["statement"]["p"]
                .as_str()
                .unwrap_or_else(|| panic!("{value}")),
        )
        .unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["status"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn retries_reuse_the_stored_envelope_but_expiry_requires_explicit_status_or_a_new_key() {
        if !isolated("write::retries_reuse_the_stored_envelope_but_expiry_requires_explicit_status_or_a_new_key") {
            return;
        }
        use crate::commands::local_host::agent_link;
        use crate::commands::messaging::MessagingClient;
        use std::sync::atomic::Ordering::SeqCst;
        runtime().block_on(async {
            let (receiver, _a) = world();
            let (rhost, mut rterm) = arc_host(&receiver);
            let (sender, _b) = world();
            let (shost, _st) = arc_host(&sender);
            serve_socket(sender.dir(), shost.clone());
            let relay = Arc::new(Relay {
                routes: Mutex::new(HashMap::from([(receiver.host_id.clone(), rhost.clone())])),
                ..Default::default()
            });
            let central = relay_stub(relay.clone()).await;
            let client = || {
                MessagingClient::with_cookie(&central, B, Some("conversations.sid=x".into()))
                    .unwrap()
                    .with_local_dir(sender.dir().to_path_buf())
            };
            let send = |text: &str, idem: &str, target: &str, reply: Option<&str>| {
                let client = client();
                let mut body = send_body(target, text, idem);
                body["reply_to"] = json!(reply);
                async move {
                    client
                        .request(reqwest::Method::POST, &["messages"], Some(body))
                        .await
                        .map_err(|e| format!("{e:#}"))
                }
            };
            let descriptor = authz::attest::descriptor(receiver.dir()).unwrap();
            agent_link::pair_peer(sender.dir(), &descriptor, A, "a").unwrap();
            agent_link::pair_peer(sender.dir(), &descriptor, B, "b").unwrap();
            approve_agent(receiver.dir(), sender.dir(), &[A]);
            let lose = lose_verified_reply_marker(sender.dir());
            let row = |idem: &str| {
                Store::open(sender.dir())
                    .unwrap()
                    .send_get(B, A, idem)
                    .unwrap()
                    .unwrap()
            };

            // 1. The verified answer is lost after the host admitted the message
            //    over the private channel (the hook consumed its marker). A retry
            //    two seconds later reuses the EXACT stored bytes (a fresh signature
            //    would be a payload mismatch) and stays one message.
            std::fs::write(&lose, b"").unwrap();
            assert!(send("one", "k1", A, None).await.is_err());
            assert!(!lose.exists(), "no verified reply reached the sender");
            assert_eq!(message_count(receiver.dir()), 1);
            let first = row("k1");
            assert_eq!(
                first.state, "prepared",
                "an unanswered send must not claim success"
            );
            tokio::time::sleep(Duration::from_millis(2200)).await;
            let again = send("one", "k1", A, None).await.unwrap();
            assert_eq!(again["status"], "queued");
            assert_eq!(
                row("k1").payload,
                first.payload,
                "the envelope was re-signed"
            );
            assert_eq!(
                message_count(receiver.dir()),
                1,
                "the retry doubled the message"
            );
            assert!(Terminals::drain(&mut rterm.a).is_empty());

            // 2. A conflicting request under the same logical key is refused, with
            //    nothing sent: different text, reply, target, (source is fixed by the
            //    bound session).
            let sent_before = relay.seen.lock().unwrap().len();
            for (text, target, reply) in [
                ("two", A, None),
                ("one", A, Some("00000000000000000000000000000001")),
                ("one", B, None),
            ] {
                let target_is_new_row = target == B;
                let result = send(text, "k1", target, reply).await;
                if target_is_new_row {
                    // Another target is another logical send (its own row): it is
                    // refused by the recipient (no grant on B), never merged.
                    assert!(result.is_err());
                } else {
                    assert!(result.unwrap_err().contains("different message"));
                }
            }
            assert_eq!(message_count(receiver.dir()), 1);
            let _ = sent_before;

            // 3. A fresh client (source host restart) asks again: answered from the
            //    stored state with NO new network traffic.
            let seen = relay.seen.lock().unwrap().len();
            let restarted = send("one", "k1", A, None).await.unwrap();
            assert_eq!(restarted["status"], "queued");
            assert_eq!(relay.seen.lock().unwrap().len(), seen);

            // 4. An unanswered send expires after the receiver ALREADY queued it.
            //    Retrying does not query or resend; only explicit status recovers
            //    the receiver's record, after which retries return that known state.
            std::fs::write(&lose, b"").unwrap();
            assert!(send("lost", "k2", A, None).await.is_err());
            assert!(!lose.exists(), "no verified reply reached the sender");
            assert_eq!(message_count(receiver.dir()), 2);
            let real = now_secs();
            authz::store::test_clock::set(real + 120);
            let expired = row("k2");
            let seen = relay.paths.lock().unwrap().len();
            let unknown = send("lost", "k2", A, None).await.unwrap_err();
            assert!(
                unknown.contains("delivery unknown") && unknown.contains(&expired.message_id),
                "{unknown}"
            );
            assert_eq!(
                relay.paths.lock().unwrap().len(),
                seen,
                "expired send made a network request"
            );
            let unchanged = row("k2");
            assert_eq!(
                (&unchanged.payload, &unchanged.sig, &unchanged.message_id),
                (&expired.payload, &expired.sig, &expired.message_id)
            );
            assert_eq!(
                status_of(&client(), &expired.message_id).await.unwrap()["status"],
                "queued"
            );
            let seen = relay.paths.lock().unwrap().len();
            let recovered = send("lost", "k2", A, None).await.unwrap();
            assert_eq!(relay.paths.lock().unwrap().len(), seen);
            assert_eq!(recovered["status"], "queued");
            assert_eq!(
                message_count(receiver.dir()),
                2,
                "recovery created a duplicate"
            );
            // The receiver's honest state is what is reported: once its agent has
            // fetched the message, a recovery says so (never a sender-side guess).
            let fetched = MessagingClient::with_cookie(&central, A, None)
                .unwrap()
                .with_local_dir(receiver.dir().to_path_buf());
            fetched
                .request(
                    reqwest::Method::POST,
                    &["inbox"],
                    Some(json!({"wait_seconds": 0})),
                )
                .await
                .unwrap();
            let state = Store::open(receiver.dir())
                .unwrap()
                .inbox_state(&row("k2").kid, A, &stored_jti(sender.dir(), "k2"))
                .unwrap();
            assert_eq!(state, "received");

            // 5. Expiry where the receiver NEVER got it: even an explicit `absent`
            //    answer does not renew the old send. A deliberate new key sends
            //    once; the old bytes remain unchanged and cannot be admitted.
            relay.drop_request.store(true, SeqCst);
            assert!(send("never", "k3", A, None).await.is_err());
            relay.drop_request.store(false, SeqCst);
            let dead = row("k3");
            assert_eq!(message_count(receiver.dir()), 2);
            authz::store::test_clock::set(real + 300);
            let before = message_count(receiver.dir());
            let seen = relay.paths.lock().unwrap().len();
            let unknown = send("never", "k3", A, None).await.unwrap_err();
            assert!(
                unknown.contains("delivery unknown") && unknown.contains(&dead.message_id),
                "{unknown}"
            );
            assert_eq!(relay.paths.lock().unwrap().len(), seen);
            assert_eq!(
                status_of(&client(), &dead.message_id).await.unwrap()["status"],
                "absent"
            );
            let seen = relay.paths.lock().unwrap().len();
            let unknown = send("never", "k3", A, None).await.unwrap_err();
            assert!(
                unknown.contains("delivery unknown") && unknown.contains(&dead.message_id),
                "{unknown}"
            );
            assert_eq!(relay.paths.lock().unwrap().len(), seen);
            let unchanged = row("k3");
            assert_eq!(
                (&unchanged.payload, &unchanged.sig, &unchanged.message_id),
                (&dead.payload, &dead.sig, &dead.message_id)
            );
            assert_eq!(message_count(receiver.dir()), before);
            let done = send("never", "k3-new", A, None).await.unwrap();
            assert_eq!(done["status"], "queued");
            assert_ne!(row("k3-new").message_id, dead.message_id);
            assert_eq!(message_count(receiver.dir()), before + 1);
            // The dead envelope's exact stored bytes, delivered behind an
            // already-decrypted private channel (the relay would refuse any
            // agent message): still refused, since it expired.
            let (late, detail) = deliver_signed(
                &rhost,
                SignedFrame {
                    payload: dead.payload.clone(),
                    sig: dead.sig.clone(),
                    kid: dead.kid.clone(),
                },
            )
            .await;
            assert_eq!(
                late,
                conversations_core::host_relay::OpStatus::Refused,
                "{detail:?}"
            );
            assert_ne!(
                detail.as_deref(),
                Some("this operation is accepted only on a private connection")
            );
            assert_eq!(message_count(receiver.dir()), before + 1);
            assert!(
                Terminals::drain(&mut rterm.a).is_empty(),
                "queueing must never type"
            );
        });
    }

    // ---- AS5/AS6: signer-namespaced message identity and sender receipts ----

    /// Post one signed frame through the host's real relay entry point.
    async fn post(
        host: &Arc<Host>,
        frame: &SignedFrame,
    ) -> conversations_core::host_relay::RelayReply {
        let b =
            |x: &[u8]| base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, x);
        super::relay::respond(
            host,
            conversations_core::host_relay::RelayRequest::SignedOp {
                p: b(&frame.payload),
                s: b(&frame.sig),
                k: frame.kid.clone(),
            },
        )
        .await
    }

    fn reply_detail(reply: &conversations_core::host_relay::RelayReply) -> String {
        let value = serde_json::to_value(reply).unwrap();
        let bytes = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            value["statement"]["p"].as_str().unwrap(),
        )
        .unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["detail"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }

    /// Ask the host to type message `id` into `session`'s guarded input, exactly
    /// as a delivery consumer does, and return what reached the terminal.
    async fn peer_input(
        host: &Arc<Host>,
        terminal: &mut mpsc::Receiver<ServerMessage>,
        session: &str,
        id: &str,
    ) -> Option<String> {
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        tokio::spawn(handle_client(host.clone(), server));
        write_message(
            &mut client,
            &ClientMessage::PeerInputRequest {
                id: session.into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                native_session: uuid::Uuid::new_v4().to_string(),
                message_id: id.into(),
            },
        )
        .await
        .unwrap();
        let typed = tokio::select! {
            typed = terminal.recv() => typed,
            answer = read_message::<_, ServerMessage>(&mut client) => match answer.unwrap() {
                ServerMessage::PeerInputResult { written, .. } => {
                    assert!(!written);
                    return None;
                }
                other => panic!("{other:?}"),
            },
        };
        let Some(ServerMessage::PeerInput {
            text, request_id, ..
        }) = typed
        else {
            panic!("{typed:?}")
        };
        let waiting = host
            .peer_inputs
            .lock()
            .unwrap()
            .remove(&request_id)
            .unwrap();
        waiting.1.send(true).unwrap();
        match read_message::<_, ServerMessage>(&mut client).await.unwrap() {
            ServerMessage::PeerInputResult { written, .. } => assert!(written),
            other => panic!("{other:?}"),
        }
        Some(text)
    }

    /// Loopback private-channel candidates in the isolated settings file, and
    /// the one-shot marker of the existing debug hook that discards the next
    /// VERIFIED channel reply (the recipient host has acted; the sender never
    /// learns how). Returns the marker path: create it to lose the next reply.
    fn lose_verified_reply_marker(dir: &Path) -> PathBuf {
        loopback_private_channel(dir);
        let markers = dir.join("lost-reply");
        std::fs::create_dir_all(&markers).unwrap();
        std::env::set_var("CONVERSATIONS_TEST_PEER_DROP_REPLY", &markers);
        markers.join("drop")
    }

    /// Loopback private-channel candidates, set in the isolated settings file
    /// (never a personal one).
    fn loopback_private_channel(dir: &Path) {
        use crate::commands::local_host::settings::Settings;
        assert!(
            std::env::var_os("CONVERSATIONS_ISOLATED_TEST").is_some(),
            "needs isolated()"
        );
        assert!(std::env::var_os("CONVERSATIONS_HOST_SETTINGS_FILE").is_some());
        let mut settings = Settings::load(dir).unwrap();
        settings.transport.candidate_ips = vec![std::net::Ipv4Addr::LOCALHOST.into()];
        settings.save().unwrap();
    }

    /// Two machines, each with a served host socket and a real host behind one
    /// untrusted relay. The sender pins the receiver for A; the receiver pins the
    /// sender for B (so A can reply). The receiver approves the sender's agent key
    /// for A; the reverse approval is left to each test.
    struct Machines {
        sender: World,
        shost: Arc<Host>,
        sterm: Terminals,
        receiver: World,
        rhost: Arc<Host>,
        rterm: Terminals,
        relay: Arc<Relay>,
        central: String,
    }

    impl Machines {
        async fn new() -> Self {
            use crate::commands::local_host::{agent_link, settings::Settings};
            // Both hosts offer loopback candidates, set through the settings
            // file `isolated()` points this test process at, never a personal one.
            assert!(
                std::env::var_os("CONVERSATIONS_ISOLATED_TEST").is_some(),
                "Machines needs isolated()"
            );
            assert!(std::env::var_os("CONVERSATIONS_HOST_SETTINGS_FILE").is_some());
            let (receiver, _a) = world();
            let mut settings = Settings::load(receiver.dir()).unwrap();
            settings.transport.candidate_ips = vec![std::net::Ipv4Addr::LOCALHOST.into()];
            settings.save().unwrap();
            let (rhost, rterm) = arc_host(&receiver);
            serve_socket(receiver.dir(), rhost.clone());
            let (sender, _b) = world();
            let (shost, sterm) = arc_host(&sender);
            serve_socket(sender.dir(), shost.clone());
            let relay = Arc::new(Relay {
                routes: Mutex::new(HashMap::from([
                    (receiver.host_id.clone(), rhost.clone()),
                    (sender.host_id.clone(), shost.clone()),
                ])),
                ..Default::default()
            });
            let central = relay_stub(relay.clone()).await;
            let to_receiver = authz::attest::descriptor(receiver.dir()).unwrap();
            agent_link::pair_peer(sender.dir(), &to_receiver, A, "receiver A").unwrap();
            let to_sender = authz::attest::descriptor(sender.dir()).unwrap();
            agent_link::pair_peer(receiver.dir(), &to_sender, B, "sender B").unwrap();
            agent_link::pair_peer(receiver.dir(), &to_sender, A, "sender A").unwrap();
            approve_agent(receiver.dir(), sender.dir(), &[A]);
            Self {
                sender,
                shost,
                sterm,
                receiver,
                rhost,
                rterm,
                relay,
                central,
            }
        }

        fn client(&self, dir: &Path, session: &str) -> crate::commands::messaging::MessagingClient {
            crate::commands::messaging::MessagingClient::with_cookie(&self.central, session, None)
                .unwrap()
                .with_local_dir(dir.to_path_buf())
        }

        /// Sender session B -> receiver session A.
        fn sender_b(&self) -> crate::commands::messaging::MessagingClient {
            self.client(self.sender.dir(), B)
        }

        /// Receiver session A (inbox, explicit ack, replies to B).
        fn at_a(&self) -> crate::commands::messaging::MessagingClient {
            self.client(self.receiver.dir(), A)
        }
    }

    async fn send_as(
        client: &crate::commands::messaging::MessagingClient,
        target: &str,
        text: &str,
        idem: &str,
        reply_to: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let mut body = send_body(target, text, idem);
        body["reply_to"] = json!(reply_to);
        client
            .request(reqwest::Method::POST, &["messages"], Some(body))
            .await
            .map_err(|e| format!("{e:#}"))
    }

    async fn status_of(
        client: &crate::commands::messaging::MessagingClient,
        id: &str,
    ) -> Result<serde_json::Value, String> {
        client
            .request(reqwest::Method::GET, &["messages", id], None)
            .await
            .map_err(|e| format!("{e:#}"))
    }

    async fn fetch(
        client: &crate::commands::messaging::MessagingClient,
        ack: &[&str],
    ) -> Vec<String> {
        let got = client
            .request(
                reqwest::Method::POST,
                &["inbox"],
                Some(json!({"acknowledge": ack, "wait_seconds": 0})),
            )
            .await
            .unwrap();
        got["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The operation id (`jti`) inside a sender's stored envelope.
    fn stored_jti(dir: &Path, idem: &str) -> String {
        let row = Store::open(dir)
            .unwrap()
            .send_get(B, A, idem)
            .unwrap()
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&row.payload).unwrap()["jti"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn two_signers_reusing_one_operation_id_for_disjoint_sessions_both_deliver_and_a_changed_body_is_refused(
    ) {
        if !isolated("write::two_signers_reusing_one_operation_id_for_disjoint_sessions_both_deliver_and_a_changed_body_is_refused") {
            return;
        }
        runtime().block_on(async {
            // Agent messages and status questions are channel-only: delivered as
            // the private channel delivers them once decrypted.
            use conversations_core::host_relay::OpStatus;
            let (receiver, _t) = world();
            let (rhost, mut rterm) = arc_host(&receiver);
            let (alice, bob) = (Signer::new(), Signer::new());
            receiver.enroll("agent", &alice, &[format!("session.write:{A}")]).unwrap();
            receiver.enroll("agent", &bob, &[format!("session.write:{B}")]).unwrap();
            let jti = "0123456789abcdef0123456789abcdef";
            let source = "9b2f6a40-2a43-4a54-8fd0-69f3a1b5fa55";
            let message = |signer: &Signer, session: &str, text: &str| {
                let now = now_secs();
                let args = format!(
                    r#"{{"session":"{session}","source":"{source}","source_host":"{}","text":"{text}","reply_to":null}}"#,
                    signer.kid
                );
                signer.sign_bytes(
                    signer
                        .op_payload(&receiver.host_id, "agent.message", session, &args, jti, now, now + 60)
                        .as_bytes(),
                )
            };
            let (for_a, for_b) = (message(&alice, A, "for A from alice"), message(&bob, B, "for B from bob"));
            assert_eq!(deliver_signed(&rhost, for_a.clone()).await.0, OpStatus::Executed);
            assert_eq!(deliver_signed(&rhost, for_b.clone()).await.0, OpStatus::Executed);
            let (a, b) = (inbox_of(receiver.dir(), A), inbox_of(receiver.dir(), B));
            assert_eq!(a.len(), 1, "alice's message: {a:?}");
            assert_eq!(b.len(), 1, "bob's message was silently dropped: {b:?}");
            assert_eq!((a[0].text.as_str(), a[0].kid.as_str()), ("for A from alice", alice.kid.as_str()));
            assert_eq!((b[0].text.as_str(), b[0].kid.as_str()), ("for B from bob", bob.kid.as_str()));
            assert_ne!(a[0].message_id, b[0].message_id, "one id for two signers' messages");

            // Retrying each exact frame is a replay: no second row, no new effect.
            assert_eq!(deliver_signed(&rhost, for_a.clone()).await.0, OpStatus::Replayed);
            assert_eq!(deliver_signed(&rhost, for_b.clone()).await.0, OpStatus::Replayed);
            assert_eq!(message_count(receiver.dir()), 2);

            // The same signer reusing its operation id for another body is refused;
            // the admitted message is untouched.
            let changed = message(&alice, A, "rewritten by alice");
            assert_eq!(deliver_signed(&rhost, changed.clone()).await.0, OpStatus::Refused);
            assert_eq!(message_count(receiver.dir()), 2);
            assert_eq!(inbox_of(receiver.dir(), A)[0].text, "for A from alice");

            // The terminal effect: each session types exactly its own sender's text.
            let typed_a = peer_input(&rhost, &mut rterm.a, A, &a[0].message_id).await.unwrap();
            assert!(typed_a.contains("for A from alice") && !typed_a.contains("bob"), "{typed_a}");
            let typed_b = peer_input(&rhost, &mut rterm.b, B, &b[0].message_id).await.unwrap();
            assert!(typed_b.contains("for B from bob") && !typed_b.contains("alice"), "{typed_b}");
            assert!(peer_input(&rhost, &mut rterm.a, A, &b[0].message_id).await.is_none());
            assert!(Terminals::drain(&mut rterm.a).is_empty() && Terminals::drain(&mut rterm.b).is_empty());

            // Each signer can ask about its own operation and nobody else's.
            let ask = |signer: &Signer, session: &str| {
                let now = now_secs();
                let args = format!(r#"{{"session":"{session}","message":"{jti}"}}"#);
                let jti2 = format!("{:032x}", rand_u128());
                signer.sign_bytes(
                    signer
                        .op_payload(&receiver.host_id, "agent.status", session, &args, &jti2, now, now + 60)
                        .as_bytes(),
                )
            };
            assert_eq!(deliver_signed(&rhost, ask(&alice, A)).await.1.as_deref(), Some("queued"));
            assert_eq!(deliver_signed(&rhost, ask(&bob, B)).await.1.as_deref(), Some("queued"));
            let carol = Signer::new();
            receiver.enroll("agent", &carol, &[format!("session.write:{A}")]).unwrap();
            assert_eq!(deliver_signed(&rhost, ask(&carol, A)).await.1.as_deref(), Some("absent"));
        });
    }

    #[test]
    fn valid_native_agent_operations_are_refused_on_the_plaintext_relay_with_no_effect_but_admitted_on_the_channel(
    ) {
        if !isolated("write::valid_native_agent_operations_are_refused_on_the_plaintext_relay_with_no_effect_but_admitted_on_the_channel") {
            return;
        }
        runtime().block_on(async {
            use conversations_core::host_relay::OpStatus;
            let (receiver, _t) = world();
            let (rhost, _rterm) = arc_host(&receiver);
            let alice = Signer::new();
            receiver.enroll("agent", &alice, &[format!("session.write:{A}")]).unwrap();
            let source = "9b2f6a40-2a43-4a54-8fd0-69f3a1b5fa55";
            let frame = |op: &str, args: &str, jti: &str| {
                let now = now_secs();
                alice.sign_bytes(
                    alice
                        .op_payload(&receiver.host_id, op, A, args, jti, now, now + 60)
                        .as_bytes(),
                )
            };
            let message_args = format!(
                r#"{{"session":"{A}","source":"{source}","source_host":"{}","text":"sealed text","reply_to":null}}"#,
                alice.kid
            );
            let relayed_jti = format!("{:032x}", rand_u128());

            // The plaintext relay refuses a VALID, granted agent message and
            // status question: signed refusal, no inbox row, no journal entry.
            let relayed = post(&rhost, &frame("agent.message", &message_args, &relayed_jti)).await;
            assert_eq!(reply_status(&relayed), "refused");
            assert_eq!(
                reply_detail(&relayed),
                "this operation is accepted only on a private connection"
            );
            let status_args = format!(r#"{{"session":"{A}","message":"{relayed_jti}"}}"#);
            let asked = post(
                &rhost,
                &frame("agent.status", &status_args, &format!("{:032x}", rand_u128())),
            )
            .await;
            assert_eq!(reply_status(&asked), "refused");
            assert_eq!(message_count(receiver.dir()), 0, "the relay admitted a message");

            // Positive control: the SAME relay-refused frame is well formed and
            // granted, so the decrypted channel entry point admits it once.
            let (status, _) = super::decrypted_channel_request(
                &rhost,
                frame("agent.message", &message_args, &relayed_jti),
            )
            .await;
            assert_eq!(status, OpStatus::Executed);
            assert_eq!(message_count(receiver.dir()), 1);
            assert_eq!(inbox_of(receiver.dir(), A)[0].text, "sealed text");
            let (status, detail) = super::decrypted_channel_request(
                &rhost,
                frame("agent.status", &status_args, &format!("{:032x}", rand_u128())),
            )
            .await;
            assert_eq!(status, OpStatus::Executed);
            assert_eq!(detail.as_deref(), Some("queued"));
        });
    }

    fn rand_u128() -> u128 {
        uuid::Uuid::new_v4().as_u128()
    }

    #[test]
    fn the_original_sender_observes_queued_received_and_acknowledged_from_the_recipient_host_across_restart(
    ) {
        if !isolated("write::the_original_sender_observes_queued_received_and_acknowledged_from_the_recipient_host_across_restart") {
            return;
        }
        use std::sync::atomic::Ordering::SeqCst;
        runtime().block_on(async {
            let m = Machines::new().await;
            let sent = send_as(&m.sender_b(), A, "please review", "k1", None)
                .await
                .unwrap();
            assert_eq!(sent["status"], "queued");
            let id = sent["id"].as_str().unwrap().to_owned();

            let status = status_of(&m.sender_b(), &id).await.unwrap();
            assert_eq!(
                (status["status"].as_str(), status["stale"].as_bool()),
                (Some("queued"), Some(false)),
                "{status}"
            );

            // The recipient's agent fetches it: the id it sees is the sender's id.
            assert_eq!(fetch(&m.at_a(), &[]).await, vec![id.clone()]);
            let status = status_of(&m.sender_b(), &id).await.unwrap();
            assert_eq!(status["status"], "received", "{status}");
            assert_eq!(status["stale"], false);

            // Explicit acknowledgement on the recipient is what the sender sees next.
            fetch(&m.at_a(), &[&id]).await;
            let status = status_of(&m.sender_b(), &id).await.unwrap();
            assert_eq!(status["status"], "acknowledged", "{status}");

            // Restart of the sending process with the recipient unreachable: the
            // last AUTHENTICATED state is reported, marked stale, never upgraded.
            m.relay.drop_request.store(true, SeqCst);
            let stale = status_of(&m.sender_b(), &id).await.unwrap();
            assert_eq!(stale["status"], "acknowledged", "{stale}");
            assert_eq!(stale["stale"], true);
            assert!(
                stale["unavailable"].as_str().is_some_and(|r| !r.is_empty()),
                "{stale}"
            );
            m.relay.drop_request.store(false, SeqCst);

            // Another local session on the sending machine cannot inspect B's send.
            assert!(status_of(&m.client(m.sender.dir(), A), &id).await.is_err());
            // The recipient's own view is its inbox record.
            let theirs = status_of(&m.at_a(), &id).await.unwrap();
            assert_eq!(
                (theirs["status"].as_str(), theirs["direction"].as_str()),
                (Some("acknowledged"), Some("received")),
                "{theirs}"
            );
            assert_eq!(message_count(m.receiver.dir()), 1);
            let _ = (&m.shost, &m.sterm, &m.rterm);
        });
    }

    #[test]
    fn a_reply_acknowledges_the_original_message_only_after_the_destination_admits_it() {
        if !isolated(
            "write::a_reply_acknowledges_the_original_message_only_after_the_destination_admits_it",
        ) {
            return;
        }
        runtime().block_on(async {
            let m = Machines::new().await;
            let id = send_as(&m.sender_b(), A, "what broke?", "k1", None)
                .await
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(fetch(&m.at_a(), &[]).await, vec![id.clone()]);
            let state = || async {
                status_of(&m.sender_b(), &id).await.unwrap()["status"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };

            // The sender's host has not approved A's key: the reply is refused at
            // its destination, so nothing is acknowledged.
            let refused = send_as(&m.at_a(), B, "the parser", "r1", Some(&id)).await;
            assert!(refused.is_err(), "{refused:?}");
            assert_eq!(state().await, "received");

            // A reply_to that is not this session's incoming message from that
            // recipient never leaves the machine.
            let seen = m.relay.seen.lock().unwrap().len();
            for (client, target, reply) in [
                (m.at_a(), A, id.clone()),                      // wrong recipient
                (m.client(m.receiver.dir(), B), B, id.clone()), // another local session
                (m.at_a(), B, "00000000000000000000000000000001".to_owned()), // unknown message
            ] {
                let wrong = send_as(&client, target, "not a real reply", "bad", Some(&reply)).await;
                assert!(wrong.is_err(), "{wrong:?}");
            }
            assert_eq!(
                m.relay.seen.lock().unwrap().len(),
                seen,
                "an invalid reply was sent"
            );
            assert_eq!(state().await, "received");

            // Approved now. The reply's answer is lost: admitted at the destination
            // but unconfirmed here, so still no acknowledgement.
            approve_agent(m.sender.dir(), m.receiver.dir(), &[B]);
            let lose = lose_verified_reply_marker(m.receiver.dir());
            std::fs::write(&lose, b"").unwrap();
            assert!(send_as(&m.at_a(), B, "the parser", "r2", Some(&id))
                .await
                .is_err());
            assert!(!lose.exists(), "no verified reply reached the replier");
            // Admitted at the destination over the private channel.
            assert_eq!(inbox_of(m.sender.dir(), B).len(), 1);
            assert_eq!(
                state().await,
                "received",
                "acknowledged before the reply was confirmed"
            );

            // Retrying the same reply confirms admission: now the original is
            // acknowledged on the recipient's host, and the sender reads that.
            let reply = send_as(&m.at_a(), B, "the parser", "r2", Some(&id))
                .await
                .unwrap();
            assert_eq!(reply["status"], "queued");
            assert_eq!(state().await, "acknowledged");
            let replies = inbox_of(m.sender.dir(), B);
            assert_eq!(replies.len(), 1, "{replies:?}");
            assert_eq!(
                (replies[0].text.as_str(), replies[0].reply_to.as_deref()),
                ("the parser", Some(id.as_str()))
            );
            assert_eq!(
                fetch(&m.sender_b(), &[]).await,
                vec![reply["id"].as_str().unwrap().to_owned()]
            );
            // Explicit acknowledgement still works for the reply itself.
            fetch(&m.sender_b(), &[reply["id"].as_str().unwrap()]).await;
            assert_eq!(
                status_of(&m.at_a(), reply["id"].as_str().unwrap())
                    .await
                    .unwrap()["status"],
                "acknowledged"
            );
            let _ = (&m.shost, &m.sterm, &m.rhost, &m.rterm);
        });
    }

    #[test]
    fn a_hostile_relay_cannot_forge_swap_or_replay_the_recipient_hosts_status() {
        if !isolated(
            "write::a_hostile_relay_cannot_forge_swap_or_replay_the_recipient_hosts_status",
        ) {
            return;
        }
        runtime().block_on(async {
            let m = Machines::new().await;
            let id = |v: serde_json::Value| v["id"].as_str().unwrap().to_owned();
            let first = id(send_as(&m.sender_b(), A, "first", "k1", None)
                .await
                .unwrap());
            let second = id(send_as(&m.sender_b(), A, "second", "k2", None)
                .await
                .unwrap());
            fetch(&m.at_a(), &[]).await;
            fetch(&m.at_a(), &[&second]).await;
            // Genuine answers over the private channel: first is received, second
            // acknowledged. The central stand-in only carries SIGNALING, so what
            // it captures are the hosts' private-connection answers, not the
            // signed status outcomes (those travel encrypted). Forgery, swap and
            // replay of signed application outcomes are proven against the real
            // verifier in `private_peer::tests`; here they are signaling attacks.
            assert_eq!(
                status_of(&m.sender_b(), &first).await.unwrap()["status"],
                "received"
            );
            let first_answer = m.relay.replies.lock().unwrap().last().cloned().unwrap();
            assert_eq!(
                status_of(&m.sender_b(), &second).await.unwrap()["status"],
                "acknowledged"
            );
            let second_answer = m.relay.replies.lock().unwrap().last().cloned().unwrap();
            fetch(&m.at_a(), &[&first]).await; // first is now acknowledged on the recipient

            let rejected = |status: serde_json::Value, what: &str| {
                assert_eq!(status["stale"], true, "{what}: {status}");
                assert_eq!(
                    status["status"], "received",
                    "{what} changed the state: {status}"
                );
            };
            // Signaling replay: an earlier genuine connection answer.
            *m.relay.answer.lock().unwrap() = Some(first_answer);
            rejected(status_of(&m.sender_b(), &first).await.unwrap(), "replay");
            // Signaling swap: the connection answer for another message.
            *m.relay.answer.lock().unwrap() = Some(second_answer);
            rejected(status_of(&m.sender_b(), &first).await.unwrap(), "swap");
            *m.relay.answer.lock().unwrap() = None;
            // Signaling modification: the signed connection offer is changed in
            // transit, so the recipient refuses it. Requests without a signed
            // payload (the challenge) pass unchanged.
            *m.relay.mutate.lock().unwrap() = Some(Box::new(|value| {
                let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
                let Some(p) = value["p"].as_str() else {
                    return;
                };
                let bytes = base64::Engine::decode(&b64, p).unwrap();
                let text = String::from_utf8(bytes)
                    .unwrap()
                    .replace("\"exp\":", "\"exp\":1");
                value["p"] = json!(base64::Engine::encode(&b64, text.as_bytes()));
            }));
            rejected(
                status_of(&m.sender_b(), &first).await.unwrap(),
                "modified request",
            );
            *m.relay.mutate.lock().unwrap() = None;
            // Forged host: another host answers the connection (signed with its
            // own key), refused against the pinned key.
            let (rogue, _t) = world();
            let (rogue_host, _rt) = arc_host(&rogue);
            approve_agent(rogue.dir(), m.sender.dir(), &[A]);
            m.relay
                .routes
                .lock()
                .unwrap()
                .insert(m.receiver.host_id.clone(), rogue_host);
            rejected(
                status_of(&m.sender_b(), &first).await.unwrap(),
                "forged host",
            );
            m.relay
                .routes
                .lock()
                .unwrap()
                .insert(m.receiver.host_id.clone(), m.rhost.clone());

            // Honest path again: the real state.
            let fresh = status_of(&m.sender_b(), &first).await.unwrap();
            assert_eq!(
                (fresh["status"].as_str(), fresh["stale"].as_bool()),
                (Some("acknowledged"), Some(false)),
                "{fresh}"
            );

            // Another key approved for the same session cannot inspect B's message.
            let mallory = Signer::new();
            m.receiver
                .enroll("agent", &mallory, &[format!("session.write:{A}")])
                .unwrap();
            let jti = stored_jti(m.sender.dir(), "k1");
            let probe = m.receiver.frame(
                &mallory,
                "agent.status",
                A,
                &format!(r#"{{"session":"{A}","message":"{jti}"}}"#),
                60,
            );
            // Asked behind an already-decrypted private channel (the relay
            // refuses status questions): the host answers for mallory's own
            // namespace only.
            let (status, detail) = deliver_signed(&m.rhost, probe).await;
            assert_eq!(
                (status, detail.as_deref()),
                (OpStatus::Executed, Some("absent"))
            );
            let _ = (&m.shost, &m.sterm, &m.rterm);
        });
    }

    #[test]
    fn a_reply_whose_original_cannot_be_acknowledged_records_neither_and_its_retry_completes_both()
    {
        if !isolated("write::a_reply_whose_original_cannot_be_acknowledged_records_neither_and_its_retry_completes_both") {
            return;
        }
        runtime().block_on(async {
            let m = Machines::new().await;
            let id = send_as(&m.sender_b(), A, "what broke?", "k1", None)
                .await
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(fetch(&m.at_a(), &[]).await, vec![id.clone()]);
            approve_agent(m.sender.dir(), m.receiver.dir(), &[B]);

            // The replying host fails between confirming the reply and
            // acknowledging the original (a crash stand-in at that write).
            let fail = "CREATE TRIGGER fail_ack BEFORE UPDATE OF state ON agent_inbox
                        WHEN NEW.state = 'acknowledged'
                        BEGIN SELECT RAISE(ABORT, 'injected failure'); END;";
            Store::open(m.receiver.dir())
                .unwrap()
                .connection()
                .execute_batch(fail)
                .unwrap();
            let failed = send_as(&m.at_a(), B, "the parser", "r1", Some(&id)).await;
            assert!(failed.is_err(), "{failed:?}");

            // Reopened: the reply's confirmation was not kept without the ack it
            // implies, so the retry still has work to do.
            let reply = Store::open(m.receiver.dir())
                .unwrap()
                .send_get(A, B, "r1")
                .unwrap()
                .unwrap();
            assert_eq!(
                reply.state, "prepared",
                "reply recorded without its acknowledgement"
            );
            assert_eq!(
                status_of(&m.sender_b(), &id).await.unwrap()["status"],
                "received"
            );

            Store::open(m.receiver.dir())
                .unwrap()
                .connection()
                .execute_batch("DROP TRIGGER fail_ack;")
                .unwrap();
            let retried = send_as(&m.at_a(), B, "the parser", "r1", Some(&id))
                .await
                .unwrap();
            assert_eq!(retried["status"], "queued");
            assert_eq!(
                status_of(&m.sender_b(), &id).await.unwrap()["status"],
                "acknowledged"
            );
            assert_eq!(
                inbox_of(m.sender.dir(), B).len(),
                1,
                "the reply was admitted twice"
            );
            let _ = (&m.shost, &m.sterm, &m.rhost, &m.rterm, &m.relay);
        });
    }

    #[test]
    fn a_delayed_older_status_answer_never_moves_the_senders_state_back() {
        if !isolated("write::a_delayed_older_status_answer_never_moves_the_senders_state_back") {
            return;
        }
        use crate::commands::local_host::agent_link::{self as link, Lookup};
        runtime().block_on(async {
            let m = Machines::new().await;
            let id = send_as(&m.sender_b(), A, "hello", "k1", None)
                .await
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            // Two concurrent status requests; the recipient host answers each
            // genuinely, but the earlier answer arrives last.
            let ask = |now| {
                let Some(Lookup::Sent {
                    row,
                    query: Some(q),
                }) = link::lookup(m.sender.dir(), B, &id, now).unwrap()
                else {
                    panic!("not a pinned sent message")
                };
                (
                    *row,
                    SignedFrame {
                        payload: q.payload,
                        sig: q.sig,
                        kid: q.kid,
                    },
                )
            };
            let t = authz::store::now_secs();
            let (early_row, early_query) = ask(t);
            // Component ordering proof: the recipient host's genuine outcomes,
            // taken behind an already-decrypted private channel (the relay
            // refuses status questions). Not a network signed-outcome replay.
            let (status, early) = deliver_signed(&m.rhost, early_query).await;
            assert_eq!(status, OpStatus::Executed, "{early:?}");
            let early = early.unwrap();
            assert_eq!(early, "queued");
            fetch(&m.at_a(), &[]).await;
            fetch(&m.at_a(), &[&id]).await;
            let (late_row, late_query) = ask(t);
            let (status, late) = deliver_signed(&m.rhost, late_query).await;
            assert_eq!(status, OpStatus::Executed, "{late:?}");
            let late = late.unwrap();
            assert_eq!(late, "acknowledged");

            let done = link::after_query(
                m.sender.dir(),
                late_row,
                Ok(("executed".into(), Some(late))),
                t + 2,
            )
            .unwrap();
            assert_eq!(
                (done.state.as_str(), done.checked_at),
                ("acknowledged", Some(t + 2))
            );
            let slow = link::after_query(
                m.sender.dir(),
                early_row,
                Ok(("executed".into(), Some(early))),
                t + 3,
            )
            .unwrap();
            assert_eq!(
                (slow.state.as_str(), slow.checked_at, slow.unavailable),
                ("acknowledged", Some(t + 2), None),
                "a slower answer reported a state the sender no longer holds"
            );
            let stored = Store::open(m.sender.dir())
                .unwrap()
                .send_get(B, A, "k1")
                .unwrap()
                .unwrap();
            assert_eq!(
                (stored.state.as_str(), stored.checked_at),
                ("acknowledged", Some(t + 2))
            );
            let _ = (&m.shost, &m.sterm, &m.rterm, &m.relay, &m.receiver);
        });
    }

    #[test]
    fn explicit_status_recovers_lost_answers_while_expired_sends_require_a_new_key() {
        if !isolated(
            "write::explicit_status_recovers_lost_answers_while_expired_sends_require_a_new_key",
        ) {
            return;
        }
        use std::sync::atomic::Ordering::SeqCst;
        runtime().block_on(async {
            let mut m = Machines::new().await;
            // Admitted, verified answer lost. The sender's status asks the
            // recipient host and learns the truth; the later retry needs no
            // traffic and adds nothing.
            let lose = lose_verified_reply_marker(m.sender.dir());
            std::fs::write(&lose, b"").unwrap();
            assert!(send_as(&m.sender_b(), A, "lost answer", "k1", None)
                .await
                .is_err());
            assert!(!lose.exists(), "no verified reply reached the sender");
            assert_eq!(message_count(m.receiver.dir()), 1);
            let id = Store::open(m.sender.dir())
                .unwrap()
                .send_get(B, A, "k1")
                .unwrap()
                .unwrap()
                .message_id;
            let status = status_of(&m.sender_b(), &id).await.unwrap();
            assert_eq!(
                (status["status"].as_str(), status["stale"].as_bool()),
                (Some("queued"), Some(false)),
                "{status}"
            );
            let seen = m.relay.seen.lock().unwrap().len();
            assert_eq!(
                send_as(&m.sender_b(), A, "lost answer", "k1", None)
                    .await
                    .unwrap()["status"],
                "queued"
            );
            assert_eq!(m.relay.seen.lock().unwrap().len(), seen);

            // Typed once into the recipient's terminal, then acknowledged.
            let typed = peer_input(&m.rhost, &mut m.rterm.a, A, &id).await.unwrap();
            assert!(typed.contains("lost answer"), "{typed}");
            fetch(&m.at_a(), &[&id]).await;
            assert!(
                peer_input(&m.rhost, &mut m.rterm.a, A, &id).await.is_none(),
                "typed twice"
            );

            // Long after the envelope expired, a restarted sender still reads the
            // recipient's true state, and the retry does not resend.
            let real = now_secs();
            authz::store::test_clock::set(real + 120);
            assert_eq!(
                status_of(&m.sender_b(), &id).await.unwrap()["status"],
                "acknowledged"
            );
            assert_eq!(
                send_as(&m.sender_b(), A, "lost answer", "k1", None)
                    .await
                    .unwrap()["status"],
                "acknowledged"
            );
            assert_eq!(message_count(m.receiver.dir()), 1);

            // Never delivered and expired: neither retry nor an explicit absent
            // status renews it. Only a deliberate new key creates a new send.
            m.relay.drop_request.store(true, SeqCst);
            assert!(send_as(&m.sender_b(), A, "never arrived", "k2", None)
                .await
                .is_err());
            m.relay.drop_request.store(false, SeqCst);
            let dead_row = Store::open(m.sender.dir())
                .unwrap()
                .send_get(B, A, "k2")
                .unwrap()
                .unwrap();
            let dead = dead_row.message_id.clone();
            authz::store::test_clock::set(real + 300);
            let seen = m.relay.paths.lock().unwrap().len();
            let unknown = send_as(&m.sender_b(), A, "never arrived", "k2", None)
                .await
                .unwrap_err();
            assert!(
                unknown.contains("delivery unknown") && unknown.contains(&dead),
                "{unknown}"
            );
            assert_eq!(m.relay.paths.lock().unwrap().len(), seen);
            let absent = status_of(&m.sender_b(), &dead).await.unwrap();
            assert_eq!(
                (absent["status"].as_str(), absent["stale"].as_bool()),
                (Some("absent"), Some(false)),
                "{absent}"
            );
            let seen = m.relay.paths.lock().unwrap().len();
            let unknown = send_as(&m.sender_b(), A, "never arrived", "k2", None)
                .await
                .unwrap_err();
            assert!(
                unknown.contains("delivery unknown") && unknown.contains(&dead),
                "{unknown}"
            );
            assert_eq!(m.relay.paths.lock().unwrap().len(), seen);
            let unchanged = Store::open(m.sender.dir())
                .unwrap()
                .send_get(B, A, "k2")
                .unwrap()
                .unwrap();
            assert_eq!(
                (&unchanged.payload, &unchanged.sig, &unchanged.message_id),
                (&dead_row.payload, &dead_row.sig, &dead_row.message_id)
            );
            assert_eq!(message_count(m.receiver.dir()), 1);
            let resent = send_as(&m.sender_b(), A, "never arrived", "k2-new", None)
                .await
                .unwrap();
            assert_eq!(resent["status"], "queued");
            let new_id = resent["id"].as_str().unwrap().to_owned();
            assert_ne!(new_id, dead);
            assert_eq!(
                status_of(&m.sender_b(), &new_id).await.unwrap()["status"],
                "queued"
            );
            assert_eq!(message_count(m.receiver.dir()), 2);
            let typed = peer_input(&m.rhost, &mut m.rterm.a, A, &new_id)
                .await
                .unwrap();
            assert!(typed.contains("never arrived"));
            assert!(Terminals::drain(&mut m.rterm.a).is_empty());
            let _ = (&m.shost, &m.sterm);
        });
    }

    #[test]
    fn central_identity_cannot_steer_native_binding_and_local_binding_is_immutable() {
        if !isolated(
            "write::central_identity_cannot_steer_native_binding_and_local_binding_is_immutable",
        ) {
            return;
        }
        use crate::commands::messaging::MessagingClient;
        runtime().block_on(async {
            let (sender, _t) = world();
            let (shost, _st) = arc_host(&sender);
            serve_socket(sender.dir(), shost.clone());
            let relay = Arc::new(Relay {
                routes: Mutex::new(HashMap::new()),
                ..Default::default()
            });
            let central = relay_stub(relay.clone()).await;
            let client = |session: &str| {
                MessagingClient::with_cookie(&central, session, None).unwrap().with_local_dir(sender.dir().to_path_buf())
            };
            let attacker = "11111111-2222-4333-8444-555555555555";
            *relay.identity.lock().unwrap() = json!({"agent": "codex", "agent_session_id": A});

            assert_eq!(client(B).native_session_id().await.unwrap(), B);
            assert_eq!(client(B).session_metadata().await.unwrap()["agent"], "claude");

            shost.sessions.lock().unwrap().get_mut(B).unwrap().registration.launch.agent = conversations_core::AgentKind::Codex;
            let unbound = client(B).native_session_id().await.unwrap_err().to_string();
            assert!(unbound.contains("not registered locally"), "{unbound}");
            assert!(client(B).bind_private_codex_native("not-a-uuid").await.is_err());

            client(B).bind_private_codex_native(attacker).await.unwrap();
            assert_eq!(client(B).native_session_id().await.unwrap(), attacker);
            *relay.identity.lock().unwrap() = json!({"agent": "codex", "agent_session_id": "99999999-2222-4333-8444-555555555555"});
            assert_eq!(client(B).native_session_id().await.unwrap(), attacker);
            assert!(client(B).bind_private_codex_native("99999999-2222-4333-8444-555555555555").await.is_err(), "a bound native id was retargeted");
            shost.sessions.lock().unwrap().get_mut(A).unwrap().registration.launch.agent = conversations_core::AgentKind::Codex;
            assert!(client(A).bind_private_codex_native(attacker).await.is_err());
            let stranger = client("9b2f6a40-2a43-4a54-8fd0-69f3a1b5fa55");
            assert!(stranger.native_session_id().await.is_err());
            assert!(
                relay.paths.lock().unwrap().iter().all(|p| !p.ends_with("/identity")),
                "{:?}", relay.paths.lock().unwrap()
            );
        });
    }

    #[test]
    fn the_real_server_relays_signed_messages_across_accounts_and_only_a_proven_host_can_receive() {
        if !isolated("write::the_real_server_relays_signed_messages_across_accounts_and_only_a_proven_host_can_receive") {
            return;
        }
        use crate::commands::local_host::agent_link;
        use crate::commands::messaging::MessagingClient;
        runtime().block_on(async {
            // ---- the REAL server: router, auth, sessions, relay, SQLite ----
            let db = tempfile::tempdir().unwrap();
            let repo = conversations_server::SqliteRepo::connect(&format!("sqlite://{}?mode=rwc", db.path().join("c.sqlite").display()))
                .await
                .unwrap();
            let config = conversations_server::Config {
                auth_enabled: true,
                password_auth_enabled: true,
                cookie_secure: false,
                ..conversations_server::Config::default()
            };
            let state = conversations_server::AppState::new(Arc::new(repo), Arc::new(config));
            let app = conversations_server::build_router_with_auth(state).await.unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let central = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let http = reqwest::Client::new();
            let signup = |email: &'static str| {
                let (http, central) = (http.clone(), central.clone());
                async move {
                    let response = http
                        .post(format!("{central}/auth/register"))
                        .json(&json!({"email": email, "password": "test-password-123", "display_name": email}))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(response.status(), 201);
                    response.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_owned()
                }
            };
            let (alice, bob, mallory) = (signup("alice@x.test").await, signup("bob@x.test").await, signup("mallory@x.test").await);

            // ---- Bob's host: real relay consumer on the real server, own account ----
            let (receiver, _t) = world();
            let mut bob_host = receiver.fixture.host(central.clone());
            bob_host.cookie = Some(bob.clone());
            let bob_host = Arc::new(bob_host);
            let (mut bob_term, _bob_b) = register(&bob_host);
            let consumer = {
                let (host, id) = (bob_host.clone(), receiver.host_id.clone());
                tokio::spawn(async move { super::relay::consume(&host, &id).await })
            };
            let online = || {
                let (http, central, id, bob) = (http.clone(), central.clone(), receiver.host_id.clone(), bob.clone());
                async move {
                    http.get(format!("{central}/api/host-relay/{id}/status")).header("cookie", bob).send().await.unwrap()
                        .json::<serde_json::Value>().await.unwrap()["online"] == true
                }
            };
            for _ in 0..100 {
                if online().await { break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(online().await, "the host did not attach with its key proof");

            // ---- Alice's machine: a different account, a real sending client ----
            let (sender, _t2) = world();
            let (shost, _st) = arc_host(&sender);
            serve_socket(sender.dir(), shost.clone());
            let client_as = |dir: &Path, cookie: &str| {
                MessagingClient::with_cookie(&central, B, Some(cookie.to_owned())).unwrap().with_local_dir(dir.to_path_buf())
            };
            let send = |client: MessagingClient, target: &'static str, text: &'static str, idem: &'static str| async move {
                client.request(reqwest::Method::POST, &["messages"], Some(send_body(target, text, idem))).await.map_err(|e| format!("{e:#}"))
            };
            let descriptor = authz::attest::descriptor(receiver.dir()).unwrap();
            agent_link::pair_peer(sender.dir(), &descriptor, A, "bob A").unwrap();
            agent_link::pair_peer(sender.dir(), &descriptor, B, "bob B").unwrap();

            // Not approved yet: Bob's host refuses; nothing lands.
            let denied = send(client_as(sender.dir(), &alice), A, "hi bob", "k1").await.unwrap_err();
            assert!(denied.contains("refused"), "{denied}");
            assert_eq!(message_count(receiver.dir()), 0);

            // Bob approves Alice's agent key for session A only.
            approve_agent(receiver.dir(), sender.dir(), &[A]);
            let sent = send(client_as(sender.dir(), &alice), A, "hi bob", "k2").await.unwrap();
            assert_eq!(sent["status"], "queued");
            let rows = inbox_of(receiver.dir(), A);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].text, "hi bob");
            assert_eq!(message_count(receiver.dir()), 1, "delivered exactly once");
            // Outside the grant (B): refused. Another account's cookie is no substitute.
            assert!(send(client_as(sender.dir(), &alice), B, "and B", "k3").await.is_err());
            assert_eq!(inbox_of(receiver.dir(), B).len(), 0);

            // A key Bob never approved (another machine) cannot deliver, whatever cookie it has.
            let (eve, _t3) = world();
            let (ehost, _se) = arc_host(&eve);
            serve_socket(eve.dir(), ehost.clone());
            agent_link::pair_peer(eve.dir(), &descriptor, A, "bob A").unwrap();
            assert!(send(client_as(eve.dir(), &mallory), A, "let me in", "k4").await.is_err());
            assert!(send(client_as(eve.dir(), &bob), A, "even with bob's cookie", "k5").await.is_err());
            assert_eq!(message_count(receiver.dir()), 1);

            // ---- forged subscriber / forged reply by another ordinary account ----
            let nonce = http.post(format!("{central}/api/host-relay/{}/challenge", receiver.host_id)).header("cookie", &mallory)
                .send().await.unwrap().json::<serde_json::Value>().await.unwrap()["nonce"].as_str().unwrap().to_owned();
            let (mal_pub, mal_sig) = authz::attest::attach_proof(eve.dir(), &nonce).unwrap();
            let claim = http.get(format!("{central}/api/host-relay/{}/stream?nonce={nonce}&pub={mal_pub}&sig={mal_sig}", receiver.host_id))
                .header("cookie", &mallory).send().await.unwrap();
            assert_eq!(claim.status(), 400, "another account claimed Bob's host id");
            let forged = http.post(format!("{central}/api/host-relay/{}/reply", receiver.host_id)).header("cookie", &mallory)
                .header("x-relay-token", "guess").json(&json!({"id": "00", "reply": {"type": "error", "message": "x"}})).send().await.unwrap();
            assert_eq!(forged.status(), 400);
            // Bob's stream is still the proven one and still delivers.
            send(client_as(sender.dir(), &alice), A, "still works", "k6").await.unwrap();
            assert_eq!(message_count(receiver.dir()), 2);
            assert!(Terminals::drain(&mut bob_term).is_empty());

            // ---- a prompt is private: even Bob's own key and cookie cannot send it over the relay ----
            let owner = Signer::new();
            authz::enroll_from_cli(receiver.dir(), "owner", "ed25519", &B64.encode(&owner.public), "bob", &[format!("session.write:{A}")]).unwrap();
            let frame = receiver.frame(&owner, "session.reply", A, &format!(r#"{{"session":"{A}","text":"from bob's browser"}}"#), 60);
            let b = |x: &[u8]| base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, x);
            let reply: conversations_core::host_relay::RelayReply = http
                .post(format!("{central}/api/host-relay/{}/request", receiver.host_id)).header("cookie", &bob)
                .json(&conversations_core::host_relay::RelayRequest::SignedOp { p: b(&frame.payload), s: b(&frame.sig), k: frame.kid.clone() })
                .send().await.unwrap().json().await.unwrap();
            assert_eq!(reply_status(&reply), "refused");
            assert!(Terminals::drain(&mut bob_term).is_empty(), "a relayed prompt reached the terminal");
            consumer.abort();
        });
    }

    #[test]
    fn the_host_socket_never_types_caller_supplied_text_and_withholds_revoked_messages() {
        if !isolated("write::the_host_socket_never_types_caller_supplied_text_and_withholds_revoked_messages") {
            return;
        }
        use crate::commands::local_host::agent_link;
        runtime().block_on(async {
            let (receiver, _t) = world();
            let (rhost, mut rterm) = arc_host(&receiver);
            let (sender, _t2) = world();
            let kid = approve_agent(receiver.dir(), sender.dir(), &[A]);
            let descriptor = authz::attest::descriptor(receiver.dir()).unwrap();
            agent_link::pair_peer(sender.dir(), &descriptor, A, "r").unwrap();
            // Each sender's stored, signed envelope, delivered as the private
            // channel delivers it once decrypted (agent messages are channel-only).
            let deliver = |idem: &str, text: &str| {
                let p = prep(sender.dir(), text, idem, now_secs());
                let host = rhost.clone();
                async move {
                    let (status, _) = deliver_signed(
                        &host,
                        SignedFrame {
                            payload: p.payload.clone(),
                            sig: p.sig.clone(),
                            kid: p.kid.clone(),
                        },
                    )
                    .await;
                    assert_eq!(status, conversations_core::host_relay::OpStatus::Executed);
                    p.message_id
                }
            };
            let first = deliver("m1", "do the thing").await;

            // The guarded-input request names a message id; the host builds the text.
            let native = uuid::Uuid::new_v4().to_string();
            let ask = |session: &str, id: &str| {
                let host = rhost.clone();
                let (session, id, native) = (session.to_owned(), id.to_owned(), native.clone());
                async move {
                    let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
                    tokio::spawn(handle_client(host, server));
                    write_message(
                        &mut client,
                        &ClientMessage::PeerInputRequest {
                            id: session,
                            request_id: uuid::Uuid::new_v4().to_string(),
                            native_session: native,
                            message_id: id,
                        },
                    )
                    .await
                    .unwrap();
                    client
                }
            };
            let mut socket = ask(A, &first).await;
            let typed = tokio::time::timeout(Duration::from_secs(3), rterm.a.recv())
                .await
                .unwrap()
                .unwrap();
            let ServerMessage::PeerInput {
                text, request_id, ..
            } = typed
            else {
                panic!("{typed:?}")
            };
            assert!(
                text.contains("do the thing") && text.contains(&first),
                "{text}"
            );
            let waiting = rhost
                .peer_inputs
                .lock()
                .unwrap()
                .remove(&request_id)
                .unwrap();
            waiting.1.send(true).unwrap();
            match read_message::<_, ServerMessage>(&mut socket).await.unwrap() {
                ServerMessage::PeerInputResult { written, .. } => assert!(written),
                other => panic!("{other:?}"),
            }

            // Unknown id, another session's id, and a revoked sender's message: nothing typed.
            let other_session = deliver("m2", "for A").await;
            for (session, id) in [
                (A, "00000000000000000000000000000000".to_owned()),
                (B, other_session.clone()),
            ] {
                let mut socket = ask(session, &id).await;
                match read_message::<_, ServerMessage>(&mut socket).await.unwrap() {
                    ServerMessage::PeerInputResult { written, .. } => assert!(!written),
                    other => panic!("{other:?}"),
                }
            }
            Store::open(receiver.dir())
                .unwrap()
                .connection()
                .execute(
                    "UPDATE grants SET revoked_at = 1 WHERE cred_id = ?1",
                    [&kid],
                )
                .unwrap();
            let mut socket = ask(A, &other_session).await;
            match read_message::<_, ServerMessage>(&mut socket).await.unwrap() {
                ServerMessage::PeerInputResult { written, .. } => assert!(!written),
                other => panic!("{other:?}"),
            }
            assert!(
                Terminals::drain(&mut rterm.a).is_empty(),
                "text reached the terminal after revocation or for a foreign id"
            );
        });
    }

    #[test]
    fn malformed_or_oversized_transfer_authorizations_are_refused_before_any_effect() {
        if !isolated(
            "write::malformed_or_oversized_transfer_authorizations_are_refused_before_any_effect",
        ) {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let owner = Signer::new();
            w.enroll("owner", &owner, &[format!("session.write:{A}")])
                .unwrap();
            let id = |n: u8| format!("{:032x}", 0x1000 + n as u128);
            // Wrong type, zero and oversized sizes, bad hash, bad transfer id.
            assert!(!ok(&w.paste(&owner, A, &id(1), "text/html", IMG).await));
            assert!(!ok(&w.paste(&owner, A, &id(2), "image/png", b"").await));
            let args = format!(
                r#"{{"session":"{A}","transfer":"{}","mime":"image/png","size":{},"sha256":"{}"}}"#,
                id(3),
                10 * 1024 * 1024,
                sha(IMG)
            );
            assert!(!ok(&w.send(w.frame(&owner, "session.paste_image", A, &args, 60)).await));
            let args = format!(
                r#"{{"session":"{A}","transfer":"{}","mime":"image/png","size":1,"sha256":"XYZ"}}"#,
                id(4)
            );
            assert!(!ok(&w.send(w.frame(&owner, "session.paste_image", A, &args, 60)).await));
            assert!(!ok(&w.paste(&owner, A, "short", "image/png", IMG).await));
            // Path-escaping, empty and too-many files.
            assert!(!ok(&w.drop_files(&owner, A, &id(5), &[("../x", b"1")]).await));
            assert!(!ok(&w.drop_files(&owner, A, &id(6), &[]).await));
            let many: Vec<(&str, &[u8])> = (0..17).map(|_| ("f", b"1".as_slice())).collect();
            assert!(!ok(&w.drop_files(&owner, A, &id(7), &many).await));
            // Unknown extra field in the signed arguments.
            let args = format!(
                r#"{{"session":"{A}","transfer":"{}","mime":"image/png","size":1,"sha256":"{}","path":"/etc"}}"#,
                id(8),
                sha(IMG)
            );
            assert!(!ok(&w.send(w.frame(&owner, "session.paste_image", A, &args, 60)).await));
            assert!(authorizations(Terminals::drain(&mut t.a)).is_empty());
            // And one good one still works afterwards.
            assert!(ok(&w.paste(&owner, A, &id(9), "image/png", IMG).await));
            assert_eq!(authorizations(Terminals::drain(&mut t.a)).len(), 1);
        });
    }

    #[test]
    fn a_terminal_without_the_transfer_gate_is_never_authorized_for_transfers() {
        if !isolated(
            "write::a_terminal_without_the_transfer_gate_is_never_authorized_for_transfers",
        ) {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let owner = Signer::new();
            w.enroll("owner", &owner, &[format!("session.write:{A}")])
                .unwrap();
            w.host
                .sessions
                .lock()
                .unwrap()
                .get_mut(A)
                .unwrap()
                .registration
                .transfer_gate = false;
            let result = w.paste(&owner, A, TRANSFER, "image/png", IMG).await;
            assert!(!ok(&result), "{result:?}");
            assert!(authorizations(Terminals::drain(&mut t.a)).is_empty());
        });
    }

    #[test]
    fn an_owner_shares_one_session_and_the_recipient_writes_only_there() {
        if !isolated("write::an_owner_shares_one_session_and_the_recipient_writes_only_there") {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let owner = Signer::new();
            let guest = Signer::new();
            w.enroll("owner", &owner, &[format!("session.manage_sharing:{A}")])
                .unwrap();
            // The guest was approved locally with only a CEILING: no session yet.
            w.enroll("owner", &guest, &["ceiling:session.write".into()])
                .unwrap();

            // Before any grant: nothing the guest signs has an effect.
            assert!(refused(&w.input(&guest, A, "early").await));
            assert!(Terminals::drain(&mut t.a).is_empty());

            assert!(ok(&w
                .grant_add(&owner, A, &guest, r#"["session.write"]"#)
                .await));
            // Applies without re-enrolling the guest.
            assert!(ok(&w.input(&guest, A, "hello ").await));
            assert_eq!(inputs(Terminals::drain(&mut t.a)), vec!["hello "]);
            // Another session on the same host: no authority, no effect.
            assert!(refused(&w.input(&guest, B, "nope").await));
            assert!(Terminals::drain(&mut t.b).is_empty());
            // A signed message goes through the journaled path with its Enter.
            let args = format!(r#"{{"session":"{A}","text":"status please"}}"#);
            assert!(ok(&w
                .send(w.frame(&guest, "session.reply", A, &args, 60))
                .await));
            assert_eq!(
                inputs(Terminals::drain(&mut t.a)),
                vec!["status please", "\r"]
            );
            // The owner (manage only) cannot type: sharing is not write.
            assert!(refused(&w.input(&owner, A, "x").await));
        });
    }

    #[test]
    fn sharing_cannot_exceed_what_the_recipient_was_approved_for_or_the_actors_session() {
        if !isolated("write::sharing_cannot_exceed_what_the_recipient_was_approved_for_or_the_actors_session") {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let owner = Signer::new();
            let guest = Signer::new();
            let no_ceiling = Signer::new();
            let stranger = Signer::new();
            w.enroll("owner", &owner, &[format!("session.manage_sharing:{A}")])
                .unwrap();
            w.enroll("owner", &guest, &["ceiling:session.write".into()])
                .unwrap();
            w.enroll("owner", &no_ceiling, &["session.start".to_string()])
                .unwrap();

            // Not the session the actor manages.
            assert!(refused(
                &w.grant_add(&owner, B, &guest, r#"["session.write"]"#).await
            ));
            // Recipient never approved for write: the frame cannot raise its ceiling.
            let denied = w
                .grant_add(&owner, A, &no_ceiling, r#"["session.write"]"#)
                .await;
            assert!(refused(&denied), "{denied:?}");
            // Unknown recipient (not locally approved).
            assert!(refused(
                &w.grant_add(&owner, A, &stranger, r#"["session.write"]"#)
                    .await
            ));
            // Non-transferable capabilities fail verification outright.
            for caps in [
                r#"["session.manage_sharing"]"#,
                r#"["session.start"]"#,
                r#"["session.reopen"]"#,
                r#"["session.write","session.write"]"#,
                "[]",
            ] {
                let denied = w.grant_add(&owner, A, &guest, caps).await;
                assert!(refused(&denied), "{caps}: {denied:?}");
            }
            // A recipient holding write cannot share, start or reopen.
            assert!(ok(&w
                .grant_add(&owner, A, &guest, r#"["session.write"]"#)
                .await));
            assert!(refused(
                &w.grant_add(&guest, A, &no_ceiling, r#"["session.write"]"#)
                    .await
            ));
            let start = w.frame(
                &guest,
                "session.start",
                B,
                &format!(r#"{{"session":"{B}","agent":"claude"}}"#),
                60,
            );
            assert!(refused(&w.send(start).await));
            // Unknown session: the owner may manage A on paper but this id is not local.
            let ghost = "7a4b1c20-0000-4000-8000-000000000001";
            let owner2 = Signer::new();
            w.enroll("owner", &owner2, &["session.manage_sharing".into()])
                .unwrap();
            let denied = w
                .grant_add(&owner2, ghost, &guest, r#"["session.write"]"#)
                .await;
            assert!(
                refused(&denied) && denied.1.as_deref() == Some("unknown local session"),
                "{denied:?}"
            );
            assert!(refused(&w.input(&guest, ghost, "x").await));
            assert!(Terminals::drain(&mut t.a).is_empty() && Terminals::drain(&mut t.b).is_empty());
        });
    }

    #[test]
    fn agent_keys_hold_only_explicit_session_scopes() {
        if !isolated("write::agent_keys_hold_only_explicit_session_scopes") {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let agent = Signer::new();
            // Enrollment refuses host-wide write, sharing management and creation.
            for grant in ["session.write", "session.manage_sharing", "session.start", "session.reopen", &format!("session.manage_sharing:{A}")] {
                assert!(w.enroll("agent", &agent, &[grant.into()]).is_err(), "{grant}");
            }
            w.enroll("agent", &agent, &[format!("session.write:{A}")]).unwrap();
            assert!(ok(&w.input(&agent, A, "mine").await));
            assert!(refused(&w.input(&agent, B, "other").await));
            let start = w.frame(&agent, "session.start", B, &format!(r#"{{"session":"{B}","agent":"claude"}}"#), 60);
            assert!(refused(&w.send(start).await));
            let reopen = w.frame(&agent, "session.reopen", A, &format!(r#"{{"session":"{A}"}}"#), 60);
            assert!(refused(&w.send(reopen).await));
            assert_eq!(inputs(Terminals::drain(&mut t.a)), vec!["mine"]);
            assert!(Terminals::drain(&mut t.b).is_empty());
            // Even a host-wide row planted directly in the database does not
            // widen an agent: the admission query never reads it.
            Store::open(w.dir()).unwrap().connection().execute(
                "INSERT INTO grants(cred_id, session_id, cap, granted_at) VALUES(?1, '*', 'session.write', 1)",
                [&agent.kid],
            ).unwrap();
            assert!(refused(&w.input(&agent, B, "still no").await));
        });
    }

    #[test]
    fn a_grant_the_owner_shares_reaches_an_agent_key_only_within_its_ceiling() {
        if !isolated("write::a_grant_the_owner_shares_reaches_an_agent_key_only_within_its_ceiling")
        {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let owner = Signer::new();
            let agent = Signer::new();
            w.enroll("owner", &owner, &["session.manage_sharing".into()])
                .unwrap();
            w.enroll("agent", &agent, &["ceiling:session.write".into()])
                .unwrap();
            assert!(ok(&w
                .grant_add(&owner, B, &agent, r#"["session.write"]"#)
                .await));
            assert!(ok(&w.input(&agent, B, "b only").await));
            assert!(refused(&w.input(&agent, A, "not a").await));
            assert_eq!(inputs(Terminals::drain(&mut t.b)), vec!["b only"]);
            assert!(Terminals::drain(&mut t.a).is_empty());
        });
    }

    #[test]
    fn input_is_checked_against_current_authority_on_every_frame_and_cannot_be_replayed() {
        if !isolated("write::input_is_checked_against_current_authority_on_every_frame_and_cannot_be_replayed") {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let guest = Signer::new();
            w.enroll("owner", &guest, &[format!("session.write:{A}")])
                .unwrap();
            let first = w.frame(
                &guest,
                "session.input",
                A,
                &format!(
                    r#"{{"session":"{A}","ch":"{CH}","seq":{},"b64":"{}"}}"#,
                    w.next_seq(),
                    B64.encode("one")
                ),
                20,
            );
            assert!(ok(&w.send(first.clone()).await));
            // The same bytes again: same jti and seq. Not typed twice.
            assert!(!ok(&w.send(first).await));
            // An older sequence number is refused even with a fresh signature.
            let stale = w.frame(
                &guest,
                "session.input",
                A,
                &format!(
                    r#"{{"session":"{A}","ch":"{CH}","seq":1,"b64":"{}"}}"#,
                    B64.encode("old")
                ),
                20,
            );
            let stale = w.send(stale).await;
            assert!(
                refused(&stale) && stale.1.as_deref() == Some("stale frame on this channel"),
                "{stale:?}"
            );
            assert_eq!(inputs(Terminals::drain(&mut t.a)), vec!["one"]);
            // Stream frames cannot be long-lived leases.
            let lease = w.frame(
                &guest,
                "session.input",
                A,
                &format!(
                    r#"{{"session":"{A}","ch":"{CH}","seq":{},"b64":"{}"}}"#,
                    w.next_seq(),
                    B64.encode("lease")
                ),
                120,
            );
            assert!(refused(&w.send(lease).await));
            // Removing the grant stops the VERY NEXT frame: no signed lease survives.
            Store::open(w.dir())
                .unwrap()
                .connection()
                .execute(
                    "UPDATE grants SET revoked_at = 1 WHERE cred_id = ?1",
                    [&guest.kid],
                )
                .unwrap();
            assert!(refused(&w.input(&guest, A, "after revoke").await));
            assert!(Terminals::drain(&mut t.a).is_empty());
        });
    }

    #[test]
    fn resize_repaint_and_end_are_signed_and_unsigned_control_frames_do_nothing() {
        if !isolated(
            "write::resize_repaint_and_end_are_signed_and_unsigned_control_frames_do_nothing",
        ) {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            // Legacy unsigned frames must never become terminal authority.
            let frames = vec![
                json!({"type":"resize","session_id":A,"cols":100,"rows":30}).to_string(),
                json!({"type":"repaint","session_id":A}).to_string(),
                json!({"type":"end","session_id":A}).to_string(),
            ];
            w.fixture.run_control_on(&w.host, &frames).await.unwrap();
            assert!(
                Terminals::drain(&mut t.a).is_empty(),
                "unsigned control must not reach the terminal"
            );
            assert!(w.host.sessions.lock().unwrap().contains_key(A));

            let guest = Signer::new();
            w.enroll("owner", &guest, &[format!("session.write:{A}")])
                .unwrap();
            let resize = w.frame(
                &guest,
                "session.resize",
                A,
                &format!(
                    r#"{{"session":"{A}","ch":"{CH}","seq":{},"cols":100,"rows":30}}"#,
                    w.next_seq()
                ),
                20,
            );
            assert!(ok(&w.send(resize).await));
            let repaint = w.frame(
                &guest,
                "session.repaint",
                A,
                &format!(r#"{{"session":"{A}"}}"#),
                20,
            );
            assert!(ok(&w.send(repaint).await));
            let bad = w.frame(
                &guest,
                "session.resize",
                A,
                &format!(
                    r#"{{"session":"{A}","ch":"{CH}","seq":{},"cols":0,"rows":30}}"#,
                    w.next_seq()
                ),
                20,
            );
            assert!(refused(&w.send(bad).await));
            let seen = Terminals::drain(&mut t.a);
            assert!(
                matches!(
                    seen.as_slice(),
                    [
                        ServerMessage::Resize {
                            cols: 100,
                            rows: 30
                        },
                        ServerMessage::Repaint
                    ]
                ),
                "{seen:?}"
            );
            // End is journaled and delivered once; a replay of the frame does not end twice.
            let end = w.frame(
                &guest,
                "session.end",
                A,
                &format!(r#"{{"session":"{A}"}}"#),
                60,
            );
            assert!(ok(&w.send(end.clone()).await));
            let ended = Terminals::drain(&mut t.a);
            assert!(
                matches!(ended.as_slice(), [ServerMessage::End]),
                "{ended:?}"
            );
            assert_eq!(w.send(end).await.0, OpStatus::Replayed);
            assert!(Terminals::drain(&mut t.a).is_empty());
        });
    }

    #[test]
    fn a_signed_input_frame_is_never_delivered_twice_even_across_a_host_restart() {
        if !isolated(
            "write::a_signed_input_frame_is_never_delivered_twice_even_across_a_host_restart",
        ) {
            return;
        }
        runtime().block_on(async {
            let (mut w, mut t) = world();
            let guest = Signer::new();
            w.enroll("owner", &guest, &[format!("session.write:{A}")])
                .unwrap();
            let args = format!(
                r#"{{"session":"{A}","ch":"{CH}","seq":{},"b64":"{}"}}"#,
                w.next_seq(),
                B64.encode("rm -rf /tmp/x\r")
            );
            let frame = w.frame(&guest, "session.input", A, &args, 20);
            assert!(ok(&w.send(frame.clone()).await));
            assert_eq!(inputs(Terminals::drain(&mut t.a)), vec!["rm -rf /tmp/x\r"]);
            // Same process, same bytes.
            assert!(!ok(&w.send(frame.clone()).await), "replayed in-process");
            assert!(Terminals::drain(&mut t.a).is_empty());
            // A fresh host on the same database, within the frame's validity.
            let mut t2 = w.restart();
            let again = w.send(frame).await;
            assert!(!ok(&again), "replayed after restart: {again:?}");
            assert!(
                inputs(Terminals::drain(&mut t2.a)).is_empty(),
                "the identical frame was typed again after a host restart"
            );
        });
    }

    #[test]
    fn two_callers_with_the_same_key_and_unrelated_stream_ops_never_discard_each_others_input() {
        if !isolated(
            "write::two_callers_with_the_same_key_and_unrelated_stream_ops_never_discard_each_others_input",
        ) {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let guest = Signer::new();
            w.enroll("owner", &guest, &[format!("session.write:{A}")])
                .unwrap();
            let tab1 = "1111111111111111aaaaaaaaaaaaaaaa";
            let tab2 = "2222222222222222bbbbbbbbbbbbbbbb";
            let typed = |ch: &str, seq: u64, text: &str| {
                let args = format!(
                    r#"{{"session":"{A}","ch":"{ch}","seq":{seq},"b64":"{}"}}"#,
                    B64.encode(text)
                );
                w.frame(&guest, "session.input", A, &args, 20)
            };
            // Two browser tabs share one signing key and both start counting at 1.
            assert!(ok(&w.send(typed(tab1, 1, "a1")).await));
            assert!(ok(&w.send(typed(tab2, 1, "b1")).await));
            assert!(ok(&w.send(typed(tab1, 2, "a2")).await));
            assert!(ok(&w.send(typed(tab2, 2, "b2")).await));
            assert_eq!(
                inputs(Terminals::drain(&mut t.a)),
                vec!["a1", "b1", "a2", "b2"]
            );
            // Within one channel an older frame arriving late is still dropped.
            let late = w.send(typed(tab1, 2, "a2 again")).await;
            assert!(refused(&late), "{late:?}");
            assert!(Terminals::drain(&mut t.a).is_empty());

            // A resize whose sequence number is far ahead, a repaint, and input
            // with a small one are unrelated streams: all are delivered, in any
            // order they happen to arrive.
            let resize = |seq: u64, cols: u16| {
                let args = format!(
                    r#"{{"session":"{A}","ch":"{tab1}","seq":{seq},"cols":{cols},"rows":30}}"#
                );
                w.frame(&guest, "session.resize", A, &args, 20)
            };
            let repaint = w.frame(
                &guest,
                "session.repaint",
                A,
                &format!(r#"{{"session":"{A}"}}"#),
                20,
            );
            let (r, i, p) = tokio::join!(
                w.send(resize(1_000_000, 120)),
                w.send(typed(tab1, 3, "after resize")),
                w.send(repaint),
            );
            assert!(ok(&r) && ok(&i) && ok(&p), "{r:?} {i:?} {p:?}");
            let seen = Terminals::drain(&mut t.a);
            assert_eq!(seen.len(), 3, "{seen:?}");
            assert!(seen
                .iter()
                .any(|m| matches!(m, ServerMessage::Resize { cols: 120, .. })));
            assert!(seen.iter().any(|m| matches!(m, ServerMessage::Repaint)));
            assert_eq!(inputs(seen), vec!["after resize"]);
            // An older resize that is delayed past a newer one is dropped (latest wins).
            assert!(refused(&w.send(resize(5, 40)).await));
            assert!(Terminals::drain(&mut t.a).is_empty());
        });
    }

    #[test]
    fn a_denied_frame_is_consumed_and_spent_rows_are_pruned() {
        if !isolated("write::a_denied_frame_is_consumed_and_spent_rows_are_pruned") {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let owner = Signer::new();
            let guest = Signer::new();
            w.enroll("owner", &owner, &[format!("session.manage_sharing:{A}")])
                .unwrap();
            w.enroll("owner", &guest, &["ceiling:session.write".into()])
                .unwrap();
            let args = format!(
                r#"{{"session":"{A}","ch":"{CH}","seq":1,"b64":"{}"}}"#,
                B64.encode("held back")
            );
            let frame = w.frame(&guest, "session.input", A, &args, 20);
            assert!(refused(&w.send(frame.clone()).await));
            // Granting afterwards must not revive a frame that was already refused.
            assert!(ok(&w
                .grant_add(&owner, A, &guest, r#"["session.write"]"#)
                .await));
            assert!(!ok(&w.send(frame).await));
            assert!(Terminals::drain(&mut t.a).is_empty());
            // Spent rows are only kept while the frame could still verify.
            let conn = Store::open(w.dir()).unwrap();
            conn.connection()
                .execute(
                    "INSERT INTO stream_frames(kid, jti, exp) VALUES(?1, 'old', 1)",
                    [&guest.kid],
                )
                .unwrap();
            drop(conn);
            assert!(ok(&w.input(&guest, A, "fresh").await));
            let left: i64 = Store::open(w.dir())
                .unwrap()
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM stream_frames WHERE exp = 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(left, 0);
            // A frame from a key this host does not know leaves no row behind.
            let stranger = Signer::new();
            assert!(refused(&w.input(&stranger, A, "x").await));
            let rows: i64 = Store::open(w.dir())
                .unwrap()
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM stream_frames WHERE kid = ?1",
                    [&stranger.kid],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(rows, 0);
        });
    }

    /// Cost of the durable per-frame consumption, through the production path.
    /// Manual probe: `STREAM_COST_OUT=<file>` and `--ignored`; writes microseconds
    /// per frame. This is not a behavioral acceptance test.
    #[test]
    #[ignore = "manual timing probe; requires STREAM_COST_OUT=<file>"]
    fn stream_frame_durability_cost() {
        assert!(
            std::env::var_os("STREAM_COST_OUT").is_some_and(|path| !path.is_empty()),
            "manual timing probe requires STREAM_COST_OUT=<file>"
        );
        if !isolated("write::stream_frame_durability_cost") {
            return;
        }
        runtime().block_on(async {
            let (w, mut t) = world();
            let guest = Signer::new();
            w.enroll("owner", &guest, &[format!("session.write:{A}")])
                .unwrap();
            // Keep the terminal queue from filling: drain as we go.
            let n = 300u32;
            let started = std::time::Instant::now();
            for i in 0..n {
                let _ = w.input(&guest, A, "x").await;
                if i % 32 == 0 {
                    // queue capacity is 64
                    Terminals::drain(&mut t.a);
                }
            }
            let per = started.elapsed().as_micros() / u128::from(n);
            let line = format!("stream frame (verify+durable consume+deliver): {per} us/frame");
            if let Some(out) = std::env::var_os("STREAM_COST_OUT") {
                std::fs::write(out, &line).unwrap();
            }
            println!("{line}");
        });
    }

    /// Revocation cutover at the real effect boundaries (production host,
    /// production wrapper, a recording agent on a real PTY).
    mod revocation {
        include!("daemon_revocation_tests.rs");
    }
}

/// Host lifecycle: what a launch leaves behind when its outcome is unknown, when
/// it definitely never happened, and when a terminal registers with no prior
/// local record. No test here seeds the session table: the records come from the
/// production start and registration paths.
mod lifecycle {
    use super::*;
    use crate::commands::local_host::authz::{
        self, envelope::test_signing::Signer, store::now_secs,
    };
    use crate::commands::local_host::protocol::{
        write_message, ClientMessage, Launch, Registration,
    };
    use conversations_core::host_relay::OpStatus;

    const SESSION: &str = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";

    fn frame(
        signer: &Signer,
        host_id: &str,
        op: &str,
        session: &str,
        jti: &str,
        args: &str,
    ) -> authz::SignedFrame {
        let now = now_secs();
        signer.sign_bytes(
            signer
                .op_payload(host_id, op, session, args, jti, now, now + 60)
                .as_bytes(),
        )
    }

    fn start(signer: &Signer, host_id: &str, jti: &str) -> authz::SignedFrame {
        let now = now_secs();
        signer.sign_bytes(
            signer
                .start_payload(host_id, SESSION, "claude", jti, now, now + 60)
                .as_bytes(),
        )
    }

    fn reopen(signer: &Signer, host_id: &str, jti: &str) -> authz::SignedFrame {
        let now = now_secs();
        signer.sign_bytes(
            signer
                .reopen_payload(host_id, SESSION, jti, now, now + 60)
                .as_bytes(),
        )
    }

    /// An executable standing in for `conversations`: it records that it
    /// was really spawned (pid and argv), and never registers anything.
    fn recording_executable(dir: &Path) -> PathBuf {
        let path = dir.join("recording-wrapper");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$$ $*\" >> '{}'\nexec /bin/sleep 60\n",
                dir.join("spawns").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn spawns(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("spawns"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Targeted cleanup: only the processes this test spawned.
    struct Reaper(PathBuf);
    impl Drop for Reaper {
        fn drop(&mut self) {
            for line in spawns(&self.0) {
                if let Some(pid) = line.split(' ').next().and_then(|p| p.parse::<i32>().ok()) {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
    }

    fn journal(dir: &Path, jti: &str) -> (String, Option<String>) {
        rusqlite::Connection::open(dir.join("authz.db"))
            .unwrap()
            .query_row(
                "SELECT state, result FROM op_journal WHERE jti = ?1",
                [jti],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn host_with(
        fixture: &Fixture,
        cmux: bool,
        executable: Option<PathBuf>,
        deadline_ms: u64,
    ) -> Arc<Host> {
        let mut host = if cmux {
            fixture.host(fixture.server())
        } else {
            fixture.host_without_cmux()
        };
        host.tuning = LaunchTuning {
            registration_deadline: Duration::from_millis(deadline_ms),
            executable,
        };
        let host = Arc::new(host);
        let _ = std::fs::remove_file(fixture.dir.join("host.sock"));
        let listener = tokio::net::UnixListener::bind(fixture.dir.join("host.sock")).unwrap();
        let served = host.clone();
        let fixture_dir = fixture.dir.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (served, errors) = (served.clone(), fixture_dir.join("handle_client.err"));
                tokio::spawn(async move {
                    if let Err(error) = handle_client(served, stream).await {
                        let _ = std::fs::write(errors, format!("{error:#}"));
                    }
                });
            }
        });
        host
    }

    /// A wrapper's registration over the real host socket. Held open: the
    /// session lives as long as the returned stream.
    async fn register(
        dir: &Path,
        agent: conversations_core::AgentKind,
        cwd: &Path,
        command: Vec<String>,
    ) -> tokio::net::UnixStream {
        let mut socket = tokio::net::UnixStream::connect(dir.join("host.sock"))
            .await
            .unwrap();
        write_message(
            &mut socket,
            &ClientMessage::Register {
                registration: Registration {
                    launch: Launch {
                        id: SESSION.into(),
                        agent,
                        cwd: cwd.to_path_buf(),
                        command,
                    },
                    workspace: None,
                    surface: None,
                    local_terminal: false,
                    reconnecting: false,
                    file_receiver: false,
                    transfer_gate: false,
                    effect_gate: true,
                    in_place: false,
                },
            },
        )
        .await
        .unwrap();
        socket
    }

    async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
        for _ in 0..200 {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    fn setup(grants: &[&str]) -> (tempfile::TempDir, Fixture, Signer, String) {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(dir.path());
        let owner = Signer::new();
        let grants: Vec<String> = grants.iter().map(|g| g.to_string()).collect();
        let (_, host_id) = authz::enroll_from_cli(
            dir.path(),
            "owner",
            "ed25519",
            &B64s(&owner.public),
            "t",
            &grants,
        )
        .unwrap();
        (dir, fixture, owner, host_id)
    }

    #[allow(non_snake_case)]
    fn B64s(bytes: &[u8]) -> String {
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
    }

    fn assert_uncertain_then_never_duplicated(cmux: bool) {
        runtime().block_on(async {
            let (dir, fixture, owner, host_id) = setup(&["session.start", "session.reopen"]);
            let executable = recording_executable(dir.path());
            let _reaper = Reaper(dir.path().to_path_buf());
            let host = host_with(&fixture, cmux, Some(executable.clone()), 300);

            // The launch acts (a real child, or a cmux RPC), but no wrapper registers
            // within the deadline.
            let first = start(&owner, &host_id, &format!("{:032x}", 1));
            let (status, _) = execute_signed(&host, first.clone()).await;
            assert_eq!(
                status,
                OpStatus::Uncertain,
                "an unconfirmed launch is not a failure"
            );
            let dispatched = |dir: &Path| {
                if cmux {
                    fixture.launches()
                } else {
                    spawns(dir).len()
                }
            };
            wait_until("the launch to be observed", || dispatched(dir.path()) == 1).await;
            if cmux {
                let log = fixture.launch_log();
                assert!(
                    log.contains(SESSION) && log.contains("wrap"),
                    "cmux was not told to run the wrapper for the session: {log}"
                );
            } else {
                let line = &spawns(dir.path())[0];
                assert!(
                    line.contains(&format!("--session-id {SESSION}")) && line.contains(" wrap "),
                    "{line}"
                );
            }
            assert_eq!(journal(dir.path(), &format!("{:032x}", 1)).0, "uncertain");
            let record = authz::lookup(dir.path(), SESSION)
                .unwrap()
                .expect("the local record was deleted although a child may be running");
            assert_eq!(record.agent, conversations_core::AgentKind::Claude);

            // Redelivery of the same signed frame, a FRESH signed start for the same
            // UUID, and a restart of the host all stay at one effect.
            assert_eq!(execute_signed(&host, first).await.0, OpStatus::Uncertain);
            let fresh =
                execute_signed(&host, start(&owner, &host_id, &format!("{:032x}", 2))).await;
            assert_eq!(
                fresh.0,
                OpStatus::Refused,
                "a second signed start spawned a duplicate"
            );
            let restarted = host_with(&fixture, cmux, Some(executable), 300);
            authz::recover(dir.path()).unwrap();
            assert_eq!(
                execute_signed(&restarted, start(&owner, &host_id, &format!("{:032x}", 3)))
                    .await
                    .0,
                OpStatus::Refused
            );
            let reopened =
                execute_signed(&restarted, reopen(&owner, &host_id, &format!("{:032x}", 4))).await;
            assert_eq!(
                reopened.0,
                OpStatus::Refused,
                "a reopen must not paper over an unknown start"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(dispatched(dir.path()), 1, "a duplicate launch happened");
            assert!(
                !authz::restorable(dir.path(), SESSION),
                "an unknown start must not auto-restore"
            );

            // The late wrapper that matches the record settles the start.
            let command = vec!["claude".into(), "--session-id".into(), SESSION.into()];
            let _socket = register(
                dir.path(),
                conversations_core::AgentKind::Claude,
                &fixture.dir,
                command,
            )
            .await;
            wait_until("the late registration to settle the start", || {
                journal(dir.path(), &format!("{:032x}", 1)).0 == "done"
            })
            .await;
            assert_eq!(
                journal(dir.path(), &format!("{:032x}", 1)).1.as_deref(),
                Some("registered_after_deadline")
            );
            assert!(authz::restorable(dir.path(), SESSION));
        });
    }

    #[test]
    fn an_unconfirmed_normal_launch_is_uncertain_keeps_its_record_and_is_never_duplicated() {
        if !isolated("lifecycle::an_unconfirmed_normal_launch_is_uncertain_keeps_its_record_and_is_never_duplicated") {
            return;
        }
        assert_uncertain_then_never_duplicated(false);
    }

    #[test]
    fn an_unconfirmed_cmux_launch_is_uncertain_keeps_its_record_and_is_never_duplicated() {
        if !isolated("lifecycle::an_unconfirmed_cmux_launch_is_uncertain_keeps_its_record_and_is_never_duplicated") {
            return;
        }
        assert_uncertain_then_never_duplicated(true);
    }

    #[test]
    fn a_registration_for_another_agent_or_place_never_settles_or_rebinds_the_record() {
        if !isolated("lifecycle::a_registration_for_another_agent_or_place_never_settles_or_rebinds_the_record") {
            return;
        }
        runtime().block_on(async {
            let (dir, fixture, owner, host_id) = setup(&["session.start"]);
            let executable = recording_executable(dir.path());
            let _reaper = Reaper(dir.path().to_path_buf());
            let host = host_with(&fixture, false, Some(executable), 200);
            assert_eq!(
                execute_signed(&host, start(&owner, &host_id, &format!("{:032x}", 1)))
                    .await
                    .0,
                OpStatus::Uncertain
            );
            // Wrong agent: refused outright, the record keeps its agent, nothing settles.
            let mut wrong = register(
                dir.path(),
                conversations_core::AgentKind::Codex,
                &fixture.dir,
                vec!["codex".into()],
            )
            .await;
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert_eq!(
                authz::lookup(dir.path(), SESSION).unwrap().unwrap().agent,
                conversations_core::AgentKind::Claude
            );
            assert_eq!(journal(dir.path(), &format!("{:032x}", 1)).0, "uncertain");
            assert!(host.sessions.lock().unwrap().is_empty());
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut wrong).await;
            // Right agent, different directory: connects, but cannot settle an unknown start.
            let elsewhere = tempfile::tempdir().unwrap();
            let _socket = register(
                dir.path(),
                conversations_core::AgentKind::Claude,
                elsewhere.path(),
                vec!["claude".into()],
            )
            .await;
            wait_until("the registration", || {
                host.sessions.lock().unwrap().contains_key(SESSION)
            })
            .await;
            assert_eq!(journal(dir.path(), &format!("{:032x}", 1)).0, "uncertain");
        });
    }

    /// How the launch wait and the wrapper's connection interleave, made
    /// deterministic by driving `start_signed` and `dispatch::finish` (the two
    /// halves of `execute_signed`) by hand around a real socket registration.
    #[derive(Clone, Copy, PartialEq)]
    enum LateWrapper {
        /// Connects after the wait gave up but BEFORE its outcome is recorded.
        BeforeFinish,
        /// The host dies after the wait; the wrapper reconnects to its successor.
        AfterCrash,
        /// Wrong agent, then wrong directory: neither may settle the start.
        Mismatched,
    }

    fn late_wrapper(case: LateWrapper) {
        runtime().block_on(async {
            let (dir, fixture, owner, host_id) = setup(&["session.start"]);
            let executable = recording_executable(dir.path());
            let _reaper = Reaper(dir.path().to_path_buf());
            let jti = format!("{:032x}", 1);
            let first = start(&owner, &host_id, &jti);
            let host = host_with(&fixture, false, Some(executable.clone()), 200);
            let op = match authz::dispatch::decide(dir.path(), &first, now_secs()).unwrap() {
                authz::dispatch::Decision::Execute(op) => op,
                other => panic!("{other:?}"),
            };
            let waited = start_signed(&host, &op).await;
            assert!(
                waited.as_ref().is_err_and(authz::dispatch::is_uncertain),
                "{waited:?}"
            );
            assert_eq!(journal(dir.path(), &jti).0, "dispatching");
            wait_until("the spawn", || spawns(dir.path()).len() == 1).await;
            let outcome = waited.map_err(|error| authz::dispatch::outcome_error(&error));

            let host = if case == LateWrapper::AfterCrash {
                authz::recover(dir.path()).unwrap();
                assert_eq!(journal(dir.path(), &jti).0, "uncertain");
                host_with(&fixture, false, Some(executable), 200)
            } else {
                host
            };
            let command = vec!["claude".into(), "--session-id".into(), SESSION.into()];
            let _held;
            if case == LateWrapper::Mismatched {
                let mut wrong = register(
                    dir.path(),
                    conversations_core::AgentKind::Codex,
                    &fixture.dir,
                    vec!["codex".into()],
                )
                .await;
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert!(host.sessions.lock().unwrap().is_empty());
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut wrong).await;
                let elsewhere = tempfile::tempdir().unwrap();
                _held = register(
                    dir.path(),
                    conversations_core::AgentKind::Claude,
                    elsewhere.path(),
                    command,
                )
                .await;
                wait_until("the misplaced wrapper to connect", || {
                    host.sessions.lock().unwrap().contains_key(SESSION)
                })
                .await;
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert_eq!(journal(dir.path(), &jti).0, "dispatching");
            } else {
                _held = register(
                    dir.path(),
                    conversations_core::AgentKind::Claude,
                    &fixture.dir,
                    command,
                )
                .await;
                wait_until("the wrapper to connect", || {
                    host.sessions.lock().unwrap().contains_key(SESSION)
                })
                .await;
                wait_until("the connection to settle the start", || {
                    journal(dir.path(), &jti).0 == "done"
                })
                .await;
            }
            if case != LateWrapper::AfterCrash {
                // The outcome the wait produced is recorded only now.
                authz::dispatch::finish(dir.path(), &op, &outcome).unwrap();
            }
            let (state, result) = journal(dir.path(), &jti);
            match case {
                LateWrapper::BeforeFinish => {
                    assert_eq!(
                        (state.as_str(), result.as_deref()),
                        ("done", Some("started")),
                        "the late outcome regressed a confirmed start"
                    );
                    assert!(authz::restorable(dir.path(), SESSION));
                }
                LateWrapper::AfterCrash => {
                    assert_eq!(
                        (state.as_str(), result.as_deref()),
                        ("done", Some("registered_after_deadline"))
                    );
                }
                LateWrapper::Mismatched => {
                    assert_eq!(state, "uncertain");
                    assert!(!authz::restorable(dir.path(), SESSION));
                    assert_eq!(
                        authz::lookup(dir.path(), SESSION).unwrap().unwrap().agent,
                        conversations_core::AgentKind::Claude
                    );
                }
            }
            // Neither redelivery nor a fresh signed start ever spawns again.
            let replayed = execute_signed(&host, first).await.0;
            assert_eq!(
                replayed,
                if case == LateWrapper::Mismatched {
                    OpStatus::Uncertain
                } else {
                    OpStatus::Replayed
                }
            );
            let fresh =
                execute_signed(&host, start(&owner, &host_id, &format!("{:032x}", 2))).await;
            assert_eq!(fresh.0, OpStatus::Refused);
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(spawns(dir.path()).len(), 1, "a duplicate launch happened");
        });
    }

    #[test]
    fn a_wrapper_connecting_before_the_timed_out_outcome_is_recorded_settles_the_start() {
        if !isolated("lifecycle::a_wrapper_connecting_before_the_timed_out_outcome_is_recorded_settles_the_start") {
            return;
        }
        late_wrapper(LateWrapper::BeforeFinish);
    }

    #[test]
    fn a_wrapper_reconnecting_after_a_host_crash_settles_the_recovered_start() {
        if !isolated(
            "lifecycle::a_wrapper_reconnecting_after_a_host_crash_settles_the_recovered_start",
        ) {
            return;
        }
        late_wrapper(LateWrapper::AfterCrash);
    }

    #[test]
    fn a_mismatched_wrapper_connecting_before_the_outcome_is_recorded_settles_nothing() {
        if !isolated("lifecycle::a_mismatched_wrapper_connecting_before_the_outcome_is_recorded_settles_nothing") {
            return;
        }
        late_wrapper(LateWrapper::Mismatched);
    }

    /// A refused registration writes nothing: not the agent, and not a native id
    /// taken from its argv, whether the record has no native id yet or another one.
    #[test]
    fn a_refused_registration_leaves_the_canonical_record_untouched() {
        if !isolated("lifecycle::a_refused_registration_leaves_the_canonical_record_untouched") {
            return;
        }
        use conversations_core::{AgentKind, AgentLauncher};
        let (dir, _fixture, _owner, _host_id) = setup(&["session.start"]);
        let cwd = dir.path().to_string_lossy().into_owned();
        let claude_argv = vec!["claude".into(), "--session-id".into(), SESSION.into()];
        let record = |agent, resume_id: Option<&str>| authz::sessions::LocalSession {
            session_id: SESSION.into(),
            agent,
            cwd: cwd.clone(),
            resume_id: resume_id.map(str::to_owned),
            launcher: AgentLauncher::default(),
            adk_statusline: false,
        };

        // Another agent's record without a native id yet.
        assert!(authz::record_started(dir.path(), &record(AgentKind::Codex, None)).unwrap());
        assert!(
            authz::register_launch(dir.path(), SESSION, AgentKind::Claude, &cwd, &claude_argv)
                .is_err()
        );
        assert_eq!(
            authz::lookup(dir.path(), SESSION).unwrap().unwrap(),
            record(AgentKind::Codex, None),
            "a refused registration bound its argv's native id"
        );
        authz::forget_started(dir.path(), SESSION).unwrap();

        // The same agent, already bound to another native session.
        let bound = record(
            AgentKind::Claude,
            Some("0b7c3d1e-0000-4000-8000-000000000001"),
        );
        assert!(authz::record_started(dir.path(), &bound).unwrap());
        assert!(
            authz::register_launch(dir.path(), SESSION, AgentKind::Claude, &cwd, &claude_argv)
                .is_err()
        );
        assert_eq!(authz::lookup(dir.path(), SESSION).unwrap().unwrap(), bound);
        authz::forget_started(dir.path(), SESSION).unwrap();

        // The matching agent binds the proven id once; repeating it changes nothing.
        assert!(authz::record_started(dir.path(), &record(AgentKind::Claude, None)).unwrap());
        for _ in 0..2 {
            authz::register_launch(dir.path(), SESSION, AgentKind::Claude, &cwd, &claude_argv)
                .unwrap();
            assert_eq!(
                authz::lookup(dir.path(), SESSION).unwrap().unwrap(),
                record(AgentKind::Claude, Some(SESSION))
            );
        }
    }

    #[test]
    fn a_definite_preflight_failure_never_blocks_the_corrected_start_or_a_later_reopen() {
        if !isolated("lifecycle::a_definite_preflight_failure_never_blocks_the_corrected_start_or_a_later_reopen") {
            return;
        }
        runtime().block_on(async {
            let (dir, fixture, owner, host_id) = setup(&["session.start", "session.reopen"]);
            let executable = recording_executable(dir.path());
            let _reaper = Reaper(dir.path().to_path_buf());

            // The executable cannot be spawned: nothing started, so this is a plain
            // failure that leaves no record behind.
            let broken = host_with(
                &fixture,
                false,
                Some(dir.path().join("does-not-exist")),
                300,
            );
            let (status, _) =
                execute_signed(&broken, start(&owner, &host_id, &format!("{:032x}", 1))).await;
            assert_eq!(status, OpStatus::Failed);
            assert_eq!(journal(dir.path(), &format!("{:032x}", 1)).0, "failed");
            assert!(authz::lookup(dir.path(), SESSION).unwrap().is_none());
            assert!(spawns(dir.path()).is_empty());

            // Corrected start, same UUID: runs once, and the wrapper registers.
            let host = host_with(&fixture, false, Some(executable.clone()), 5_000);
            let second = start(&owner, &host_id, &format!("{:032x}", 2));
            let started = {
                let host = host.clone();
                tokio::spawn(async move { execute_signed(&host, second).await })
            };
            wait_until("the spawn", || spawns(dir.path()).len() == 1).await;
            let command = vec!["claude".into(), "--session-id".into(), SESSION.into()];
            let wrapper = register(
                dir.path(),
                conversations_core::AgentKind::Claude,
                &fixture.dir,
                command.clone(),
            )
            .await;
            assert_eq!(started.await.unwrap().0, OpStatus::Executed);
            assert_eq!(journal(dir.path(), &format!("{:032x}", 2)).0, "done");
            assert!(
                authz::restorable(dir.path(), SESSION),
                "the old failed start still blocks restore"
            );

            // The wrapper goes away; a restarted host reopens from the canonical record.
            drop(wrapper);
            let restarted = host_with(&fixture, false, Some(executable), 5_000);
            let reopened = {
                let host = restarted.clone();
                let frame = reopen(&owner, &host_id, &format!("{:032x}", 3));
                tokio::spawn(async move { execute_signed(&host, frame).await })
            };
            wait_until("the reopen spawn", || spawns(dir.path()).len() == 2).await;
            let _again = register(
                dir.path(),
                conversations_core::AgentKind::Claude,
                &fixture.dir,
                command,
            )
            .await;
            let (status, detail) = reopened.await.unwrap();
            assert_eq!(
                status,
                OpStatus::Executed,
                "{detail:?}: a failed pre-effect start blocked the reopen"
            );
            assert!(
                spawns(dir.path())[1].contains(&format!("--resume {SESSION}")),
                "{:?}",
                spawns(dir.path())
            );
        });
    }

    #[test]
    fn a_locally_started_cmux_session_gets_its_record_and_signed_writes_work_without_a_restart() {
        if !isolated("lifecycle::a_locally_started_cmux_session_gets_its_record_and_signed_writes_work_without_a_restart") {
            return;
        }
        runtime().block_on(async {
            let (dir, fixture, owner, host_id) = setup(&[&format!("session.write:{SESSION}")]);
            let host = host_with(&fixture, true, None, 5_000);
            assert!(authz::lookup(dir.path(), SESSION).unwrap().is_none());
            // An unknown session is refused until a terminal for it registers.
            let args = format!(r#"{{"session":"{SESSION}","text":"hello"}}"#);
            let before = execute_signed(
                &host,
                frame(
                    &owner,
                    &host_id,
                    "session.reply",
                    SESSION,
                    &format!("{:032x}", 1),
                    &args,
                ),
            )
            .await;
            assert_eq!(before.0, OpStatus::Refused);

            // The owner's local socket starts it (no signed start, no DB seeding).
            let launch = Launch {
                id: SESSION.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: fixture.dir.clone(),
                command: vec!["claude".into()],
            };
            let mut start_socket = tokio::net::UnixStream::connect(dir.path().join("host.sock"))
                .await
                .unwrap();
            write_message(&mut start_socket, &ClientMessage::Start { launch })
                .await
                .unwrap();
            wait_until("cmux to be asked for the terminal", || {
                fixture.launches() == 1
            })
            .await;
            assert!(
                fixture.launch_log().contains("--session-id"),
                "{}",
                fixture.launch_log()
            );
            let command = vec!["claude".into(), "--session-id".into(), SESSION.into()];
            let mut terminal = register(
                dir.path(),
                conversations_core::AgentKind::Claude,
                &fixture.dir,
                command,
            )
            .await;
            let started: crate::commands::local_host::protocol::ServerMessage =
                crate::commands::local_host::protocol::read_message(&mut start_socket)
                    .await
                    .unwrap();
            assert!(
                matches!(
                    started,
                    crate::commands::local_host::protocol::ServerMessage::Started { .. }
                ),
                "{started:?}"
            );

            let record = authz::lookup(dir.path(), SESSION)
                .unwrap()
                .expect("registration created no local record");
            assert_eq!(record.agent, conversations_core::AgentKind::Claude);
            assert_eq!(
                record.resume_id.as_deref(),
                Some(SESSION),
                "the argv proves Claude's native id"
            );

            // The same signed write now goes through, and reaches the terminal's queue.
            let after = execute_signed(
                &host,
                frame(
                    &owner,
                    &host_id,
                    "session.reply",
                    SESSION,
                    &format!("{:032x}", 2),
                    &args,
                ),
            )
            .await;
            assert_eq!(after.0, OpStatus::Executed, "{after:?}");
            let delivered: crate::commands::local_host::protocol::ServerMessage =
                tokio::time::timeout(
                    Duration::from_secs(3),
                    crate::commands::local_host::protocol::read_message(&mut terminal),
                )
                .await
                .unwrap()
                .unwrap();
            let rendered = format!("{delivered:?}");
            assert!(
                rendered.contains("hello") || rendered.contains("aGVsbG8"),
                "{rendered}"
            );
        });
    }

    /// The normal backend (no cmux at all): a Codex terminal's native thread comes
    /// from its private app-server, an OpenCode terminal's from its bridge report.
    /// Both are bound in the host's own record without any checkpoint, the central
    /// server being unreachable, and an established binding is never retargeted.
    #[test]
    fn codex_and_opencode_native_identity_is_bound_locally_on_the_normal_backend() {
        if !isolated(
            "lifecycle::codex_and_opencode_native_identity_is_bound_locally_on_the_normal_backend",
        ) {
            return;
        }
        use conversations_core::AgentKind;
        static ENDPOINT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        static PROBE: CodexIdentityProbe = CodexIdentityProbe {
            endpoint: |_| Ok(ENDPOINT.get().unwrap().clone()),
            deadline: Duration::from_millis(800),
        };
        // macOS caps socket paths near 104 bytes: keep the endpoint short.
        let sockets = tempfile::tempdir().unwrap();
        ENDPOINT.set(sockets.path().join("p.sock")).unwrap();
        let root = "01a168c9-b038-70a3-9f0f-4d3aebae2d94";
        runtime().block_on(async {
            let (dir, fixture, _owner, _host_id) = setup(&["session.start"]);
            let host = Arc::new(fixture.host_without_cmux());
            let settings = crate::commands::local_host::settings::Settings::default();
            let registration = |id: &str, agent, command: &str| Registration {
                launch: Launch {
                    id: id.into(),
                    agent,
                    cwd: fixture.dir.clone(),
                    command: vec![command.into()],
                },
                workspace: None,
                surface: None,
                local_terminal: false,
                reconnecting: false,
                file_receiver: false,
                transfer_gate: false,
                effect_gate: true,
                in_place: false,
            };

            // Codex: nothing is guessed until the private app-server names its root thread.
            let codex = uuid::Uuid::new_v4().to_string();
            authz::register_launch(
                dir.path(),
                &codex,
                AgentKind::Codex,
                &fixture.dir.to_string_lossy(),
                &["codex".into()],
            )
            .unwrap();
            let local = authz::lookup(dir.path(), &codex).unwrap().unwrap();
            assert_eq!(local.resume_id, None);
            assert!(
                authz::dispatch::reopen_command(&settings, &local).is_err(),
                "a Conversations id was guessed as a Codex thread"
            );
            let rollout = fixture.dir.join("rollout.jsonl");
            std::fs::write(&rollout, "").unwrap();
            let endpoint = ENDPOINT.get().unwrap();
            let app_server = crate::commands::local_host::codex::tests::app_server(
                std::os::unix::net::UnixListener::bind(endpoint).unwrap(),
                vec![crate::commands::local_host::codex::tests::thread(
                    root,
                    Some(&rollout),
                    json!("vscode"),
                    None,
                    false,
                )],
            );
            let task = tokio::spawn(native_identity_loop(
                host.clone(),
                registration(&codex, AgentKind::Codex, "codex"),
                &PROBE,
            ));
            wait_until("the Codex thread to be bound locally", || {
                authz::lookup(dir.path(), &codex)
                    .unwrap()
                    .unwrap()
                    .resume_id
                    .as_deref()
                    == Some(root)
            })
            .await;
            let local = authz::lookup(dir.path(), &codex).unwrap().unwrap();
            assert!(authz::dispatch::reopen_command(&settings, &local).is_ok());
            assert!(
                authz::bind_native(dir.path(), &codex, "99999999-9999-4999-8999-999999999999")
                    .is_err(),
                "an established binding was retargeted"
            );
            assert_eq!(
                authz::lookup(dir.path(), &codex)
                    .unwrap()
                    .unwrap()
                    .resume_id
                    .as_deref(),
                Some(root)
            );
            task.abort();
            let _ = std::fs::remove_file(endpoint);
            let _ = tokio::task::spawn_blocking(move || app_server.join()).await;

            // OpenCode: the bridge's report for exactly this launch, nothing else.
            let opencode = uuid::Uuid::new_v4().to_string();
            authz::register_launch(
                dir.path(),
                &opencode,
                AgentKind::OpenCode,
                &fixture.dir.to_string_lossy(),
                &["opencode".into()],
            )
            .unwrap();
            let task = tokio::spawn(native_identity_loop(
                host.clone(),
                registration(&opencode, AgentKind::OpenCode, "opencode"),
                &PROBE,
            ));
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert_eq!(
                authz::lookup(dir.path(), &opencode)
                    .unwrap()
                    .unwrap()
                    .resume_id,
                None,
                "bound before the agent reported"
            );
            let report = dir.path().join("opencode").join(&opencode);
            std::fs::create_dir_all(&report).unwrap();
            std::fs::write(
                report.join("report.json"),
                json!({"launchID": "someone-else", "nativeID": "ses_wrong"}).to_string(),
            )
            .unwrap();
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert_eq!(
                authz::lookup(dir.path(), &opencode)
                    .unwrap()
                    .unwrap()
                    .resume_id,
                None,
                "another launch's report was accepted"
            );
            std::fs::write(
                report.join("report.json"),
                json!({"launchID": opencode, "nativeID": "ses_right", "title": "t"}).to_string(),
            )
            .unwrap();
            wait_until("the OpenCode session to be bound locally", || {
                authz::lookup(dir.path(), &opencode)
                    .unwrap()
                    .unwrap()
                    .resume_id
                    .as_deref()
                    == Some("ses_right")
            })
            .await;
            task.abort();
        });
    }
}
