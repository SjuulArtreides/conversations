//! Authenticated owner-bound client for MCP messaging tools.

use anyhow::{bail, Context, Result};
use reqwest::{Client, Method, Url};
use serde_json::{json, Value};

use crate::credentials;

#[derive(Clone)]
pub(crate) struct MessagingClient {
    http: Client,
    origin: Url,
    cookie: Option<String>,
    session: String,
    /// Runtime directory of the local host whose authority database holds this
    /// machine's signing key, pinned recipients and admitted inbox.
    local_dir: std::path::PathBuf,
}

impl MessagingClient {
    /// Identity of THIS session from the local host: its live registration (agent
    /// kind) and the native id bound locally. The server's copy of this mapping is
    /// never consulted: it can differ from what is really running here, and it
    /// would otherwise steer which native session receives a message.
    pub(crate) async fn session_metadata(&self) -> Result<Value> {
        let status = super::local_host::agent_link::registered_session(&self.local_dir, &self.session)
            .await?
            .with_context(|| {
                format!(
                    "session {} is not registered with the local host; start or reopen it in a wrapped terminal on this machine",
                    self.session
                )
            })?;
        anyhow::ensure!(status.native_identity_supported,
            "restart or upgrade the Conversations host onto the updated CLI, then reconnect the Conversations MCP; this host does not support native identity in status");
        Ok(json!({
            "agent": status.registration.launch.agent.as_str(),
            "agent_session_id": status.native_session_id,
        }))
    }

    pub(crate) async fn native_session_id(&self) -> Result<String> {
        let value = self.session_metadata().await?;
        if let Some(native) = value["agent_session_id"]
            .as_str()
            .filter(|id| !id.is_empty())
        {
            return Ok(native.into());
        }
        if value["agent"] == "claude" {
            return Ok(self.session.clone());
        }
        bail!("native session identity is not registered locally yet; open the agent in its wrapped Conversations terminal")
    }

    /// An explicit OpenCode TUI route can identify an otherwise unbound shell
    /// wrapper. The binding is recorded locally and is immutable.
    pub(crate) async fn bind_wrapped_native(&self, native: &str) -> Result<()> {
        anyhow::ensure!(
            !native.is_empty() && native.len() <= 256 && !native.chars().any(char::is_control),
            "invalid native session identity"
        );
        let metadata = self.session_metadata().await?;
        if let Some(existing) = metadata["agent_session_id"]
            .as_str()
            .filter(|id| !id.is_empty())
        {
            anyhow::ensure!(
                existing == native,
                "source already belongs to another native session"
            );
            return Ok(());
        }
        if metadata["agent"] != "zsh" {
            anyhow::ensure!(
                self.native_session_id().await? == native,
                "native session does not match source"
            );
            return Ok(());
        }
        anyhow::ensure!(
            std::env::var("CONVERSATIONS_WRAPPED").as_deref() == Ok(self.session_id()),
            "only the bound terminal can identify its shell agent session"
        );
        self.bind_native_locally(native).await
    }

    /// Caller must hold the source delivery lock and prove that this identity
    /// is the sole loaded thread on that source's owned private Codex endpoint.
    pub(crate) async fn bind_private_codex_native(&self, native: &str) -> Result<()> {
        anyhow::ensure!(
            uuid::Uuid::parse_str(native).is_ok(),
            "invalid private Codex thread identity"
        );
        let metadata = self.session_metadata().await?;
        anyhow::ensure!(
            metadata["agent"] == "codex",
            "private Codex endpoint belongs to a non-Codex source"
        );
        if let Some(existing) = metadata["agent_session_id"]
            .as_str()
            .filter(|id| !id.is_empty())
        {
            anyhow::ensure!(
                existing == native,
                "source already belongs to another native session"
            );
            return Ok(());
        }
        self.bind_native_locally(native).await
    }

    async fn bind_native_locally(&self, native: &str) -> Result<()> {
        let (dir, session, bound) = (
            self.local_dir.clone(),
            self.session.clone(),
            native.to_owned(),
        );
        tokio::task::spawn_blocking(move || {
            super::local_host::authz::ensure_dir(&dir)?;
            super::local_host::authz::store::Store::open(&dir)?.bind_native(&session, &bound)
        })
        .await??;
        anyhow::ensure!(
            self.native_session_id().await? == native,
            "native identity did not match after binding"
        );
        Ok(())
    }

    pub(crate) fn server(&self) -> &str {
        self.origin.as_str()
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session
    }

    pub(crate) fn new(server: &str, session: &str) -> Result<Self> {
        Self::with_cookie(server, session, credentials::cookie_for(server))
    }

    pub(crate) fn with_cookie(server: &str, session: &str, cookie: Option<String>) -> Result<Self> {
        let origin = validated_origin(server)?;
        if session.is_empty() {
            bail!("a Conversations session ID is required");
        }
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(35))
            .user_agent(concat!("conversations/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            origin,
            cookie,
            session: session.into(),
            // runtime_for checks the kernel-locked per-server host record before falling back to local_dir().
            local_dir: super::local_host::agent_link::runtime_for(server),
        })
    }

    pub(crate) fn with_local_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.local_dir = dir;
        self
    }

    pub(crate) fn session_url(&self, session: &str) -> Result<String> {
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid origin"))?
            .extend(["sessions", session]);
        Ok(url.into())
    }

    /// Messaging authority is local. Sending signs for a recipient this machine
    /// paired by hand, receiving reads the host's own inbox of verified messages,
    /// and a handoff goes over the owner-only host socket. None of these consults
    /// a central inbox, handoff row or account membership. Only read-only peer
    /// discovery (`GET peers`) still goes to the server; it is untrusted and never
    /// chooses a destination or implies a grant. Native identity is local only.
    pub(crate) async fn request(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Value>,
    ) -> Result<Value> {
        match (method.as_str(), segments) {
            ("POST", ["messages"]) => return self.send_signed(body).await,
            ("POST", ["inbox"]) => return self.local_inbox(body).await,
            ("GET", ["messages", id]) => return self.message_status(id).await,
            ("POST", ["end"]) => return self.local_end(body).await,
            ("POST", ["handoffs"]) => return self.local_handoff(body).await,
            (_, ["handoffs", ..]) => {
                bail!("handoffs are local to this machine and no longer use the server")
            }
            ("GET", ["peers"]) => {}
            _ => bail!("this messaging request is not available"),
        }
        self.central_request(method, segments, body).await
    }

    async fn send_signed(&self, body: Option<Value>) -> Result<Value> {
        use super::local_host::agent_link as link;
        let body = body.context("missing message body")?;
        let get = |k: &str| body[k].as_str().map(str::to_owned);
        let target = get("recipient_session_id").context("recipient_session_id is required")?;
        let text = get("text").context("text is required")?;
        let idem = get("idempotency_key").context("idempotency_key is required")?;
        let reply_to = get("reply_to");
        anyhow::ensure!(
            link::source_registered(&self.local_dir, &self.session).await?,
            "this session is not registered with the local host; messages are signed only for managed sessions"
        );
        self.route_to_paired_machine(&target).await;
        let now = super::local_host::authz::store::now_secs;
        let begin = link::begin_send(
            &self.local_dir,
            &self.session,
            &target,
            &text,
            reply_to.as_deref(),
            &idem,
            now(),
        )?;
        match begin {
            link::Begin::Done(row) => Ok(json!({
                "id": row.message_id, "status": row.state,
                "recipient_session_id": target, "note": state_note(&row.state),
            })),
            link::Begin::Post(prepared) => {
                let (status, detail) = self.exchange(&prepared).await.context(
                    "no signed answer: the message may or may not have been admitted; retry with the same idempotency_key",
                )?;
                match status.as_str() {
                    "executed" | "replayed" => {
                        let state = link::mark_queued(
                            &self.local_dir,
                            &self.session,
                            &target,
                            &idem,
                            &prepared.message_id,
                            now(),
                        )?;
                        Ok(json!({
                            "id": prepared.message_id, "status": state,
                            "recipient_session_id": target, "note": state_note(&state),
                        }))
                    }
                    other => bail!(
                        "the recipient host answered `{other}`{}",
                        detail.map(|d| format!(": {d}")).unwrap_or_default()
                    ),
                }
            }
        }
    }

    /// A recipient not pinned here is routed to the paired machine the server
    /// says runs it. The claim chooses only among machines the owner paired, so
    /// a false one reaches another of the owner's machines, which refuses a
    /// session it does not have. Best effort: without a route the send reports
    /// the recipient as not paired.
    async fn route_to_paired_machine(&self, target: &str) {
        use super::local_host::authz::{self, store::Store};
        let dir = self.local_dir.clone();
        let target_owned = target.to_owned();
        let pinned = tokio::task::spawn_blocking(move || -> Result<bool> {
            authz::ensure_dir(&dir)?;
            let store = Store::open(&dir)?;
            Ok(store.target(&target_owned)?.is_some() || store.paired_machines()?.is_empty())
        })
        .await;
        if !matches!(pinned, Ok(Ok(false))) {
            return;
        }
        let Ok(mut url) = self.origin.join("api/sessions/") else {
            return;
        };
        url.path_segments_mut()
            .map(|mut s| {
                s.pop_if_empty().push(target);
            })
            .ok();
        let mut request = self.http.get(url);
        if let Some(cookie) = &self.cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        let Ok(response) = request.send().await else {
            return;
        };
        let Ok(session) = response.json::<Value>().await else {
            return;
        };
        let Some(host_id) = session["publication"]["host_id"]
            .as_str()
            .map(str::to_owned)
        else {
            return;
        };
        let (dir, target) = (self.local_dir.clone(), target.to_owned());
        let _ = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut store = Store::open(&dir)?;
            if let Some(machine) = store.paired_machine(&host_id)? {
                store.target_add(&target, &machine.host_id, &machine.host_key, &machine.label)?;
            }
            Ok(())
        })
        .await;
    }

    /// Send one prepared (already signed, already stored) request to its pinned
    /// host on a private channel (central carries only signaling) and return
    /// the host's signed `(status, detail)` for exactly it.
    async fn exchange(
        &self,
        prepared: &super::local_host::agent_link::Prepared,
    ) -> Result<(String, Option<String>)> {
        use super::local_host::{private_peer, settings::Settings};
        let transport = Settings::load(&self.local_dir)?.transport;
        let relay = private_peer::Relay {
            http: &self.http,
            origin: &self.origin,
            cookie: self.cookie.as_deref(),
        };
        private_peer::exchange(&relay, &self.local_dir, prepared, &transport).await
    }

    /// Re-checked by native delivery immediately before it hands a message to
    /// the agent: the message is still undelivered here and its sender still
    /// holds a current grant (a revoked one is marked `revoked` and withheld).
    pub(crate) fn still_deliverable(&self, message_id: &str) -> Result<bool> {
        use super::local_host::authz::{self, store::now_secs};
        authz::ensure_dir(&self.local_dir)?;
        Ok(authz::store::Store::open(&self.local_dir)?
            .inbox_message(&self.session, message_id, now_secs())?
            .is_some())
    }

    async fn local_inbox(&self, body: Option<Value>) -> Result<Value> {
        use super::local_host::authz::{self, store::now_secs};
        let body = body.unwrap_or_else(|| json!({}));
        let ack: Vec<String> = body["acknowledge"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let wait = body["wait_seconds"].as_u64().unwrap_or(0).min(25);
        authz::ensure_dir(&self.local_dir)?;
        let mut store = authz::store::Store::open(&self.local_dir)?;
        if !ack.is_empty() {
            store.inbox_mark(&self.session, &ack, "acknowledged", now_secs())?;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);
        loop {
            let rows = store.inbox_deliverable(&self.session, now_secs(), 50)?;
            if !rows.is_empty() || std::time::Instant::now() >= deadline {
                let ids: Vec<String> = rows.iter().map(|r| r.message_id.clone()).collect();
                store.inbox_mark(&self.session, &ids, "received", now_secs())?;
                let messages: Vec<_> = rows.into_iter().map(|r| r.into_agent_message()).collect();
                return Ok(json!({ "messages": messages }));
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    /// A received message: this host's inbox record. A sent message: a fresh
    /// signed question to the recipient host pinned for it, whose signed answer
    /// must name exactly that question. Without one, the last state that host
    /// vouched for is reported as stale, with the reason.
    async fn message_status(&self, id: &str) -> Result<Value> {
        use super::local_host::{agent_link as link, authz::store::now_secs};
        match link::lookup(&self.local_dir, &self.session, id, now_secs())? {
            None => bail!("unknown message"),
            Some(link::Lookup::Received { state, source }) => Ok(json!({
                "id": id, "status": state, "direction": "received", "session_id": source,
            })),
            Some(link::Lookup::Sent { row, query }) => {
                let target = row.target.clone();
                let answer = match &query {
                    Some(query) => self.exchange(query).await,
                    None => Err(anyhow::anyhow!(
                        "the recipient is no longer paired with the host this message went to"
                    )),
                };
                let status = link::after_query(&self.local_dir, *row, answer, now_secs())?;
                Ok(json!({
                    "id": id, "status": status.state, "direction": "sent", "session_id": target,
                    "stale": status.unavailable.is_some(), "unavailable": status.unavailable,
                    "checked_at": status.checked_at.map(link::iso),
                    "note": state_note(&status.state),
                }))
            }
        }
    }

    async fn local_handoff(&self, body: Option<Value>) -> Result<Value> {
        let body = body.context("missing handoff body")?;
        let agent = match body["agent"].as_str() {
            None => None,
            Some(name) => Some(
                serde_json::from_value::<conversations_core::AgentKind>(json!(name))
                    .context("unknown agent")?,
            ),
        };
        super::local_host::agent_link::local_handoff(
            &self.local_dir,
            &self.session,
            body["idempotency_key"]
                .as_str()
                .context("idempotency_key is required")?,
            body["brief"].as_str().context("brief is required")?,
            agent,
            body["model"].as_str().map(str::to_owned),
            body["variant"].as_str().map(str::to_owned),
        )
        .await
    }

    async fn local_end(&self, body: Option<Value>) -> Result<Value> {
        use super::local_host::protocol::{ClientMessage, ServerMessage};
        let body = body.context("missing end body")?;
        let session_id = body["session_id"]
            .as_str()
            .context("session_id is required")?;
        match super::local_host::request(
            &self.local_dir,
            ClientMessage::EndHandoff {
                source: self.session.clone(),
                session_id: session_id.into(),
            },
        )
        .await?
        {
            ServerMessage::EndHandoffResult {
                status,
                host_notified,
            } => Ok(json!({"session_id":session_id,"status":status,"host_notified":host_notified})),
            ServerMessage::Error { message } => bail!("{message}"),
            _ => bail!("unexpected local end response"),
        }
    }

    async fn central_request(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Value>,
    ) -> Result<Value> {
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid server origin"))?
            .extend(["api", "agent-messaging", "sessions", &self.session])
            .extend(segments);
        let mut request = self.http.request(method, url);
        if let Some(cookie) = &self.cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .context("Conversations messaging request failed")?;
        let status = response.status();
        let text = response
            .text()
            .await
            .context("Conversations response could not be read")?;
        let body: Value = match serde_json::from_str(&text) {
            Ok(body) => body,
            Err(_) if matches!(status.as_u16(), 404 | 405) => bail!(
                "Conversations HTTP {}: this server does not support the operation; update the Conversations server",
                status.as_u16()
            ),
            // axum rejects an unknown request field with plain text.
            Err(_) if status.as_u16() == 422 && text.contains("unknown field") => bail!(
                "Conversations HTTP 422: this server does not accept every argument this CLI sent; update the Conversations server, or retry without the newer arguments such as model"
            ),
            Err(_) if !status.is_success() => bail!(
                "Conversations HTTP {}: {}",
                status.as_u16(),
                text.chars().take(300).collect::<String>()
            ),
            Err(error) => {
                return Err(error).context("Conversations returned a non-JSON response")
            }
        };
        if !status.is_success() {
            bail!(
                "Conversations HTTP {}: {}",
                status.as_u16(),
                body["error"].as_str().unwrap_or("request rejected")
            );
        }
        Ok(body)
    }
}

fn state_note(state: &str) -> &'static str {
    match state {
        "queued" => "Admitted into the recipient host's local inbox after signature, host and grant checks. Not proof the agent consumed it or completed any work.",
        "received" => "The recipient host reports its agent fetched the message. Not proof of handling.",
        "acknowledged" => "The recipient host reports the message was acknowledged by its agent or answered by a reply. Not proof of completion.",
        "absent" => "The recipient host holds no such message from this machine. Retry messages_send with the same idempotency_key.",
        "unconfirmed" => "The recipient host never confirmed admission. Retry messages_send with the same idempotency_key.",
        _ => "",
    }
}

fn validated_origin(server: &str) -> Result<Url> {
    let url = Url::parse(server)?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("messaging requires an HTTPS server or an HTTP loopback origin");
    }
    Ok(url)
}

pub(crate) fn read_cookie_file(path: &std::path::Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        bail!("cookie file must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("cookie file must be owner-only");
        }
    }
    let cookie = std::fs::read_to_string(path)?.trim().to_owned();
    if !cookie.starts_with("conversations.sid=") || cookie.contains(['\n', '\r', ';']) {
        bail!("cookie file must contain one Conversations session cookie");
    }
    Ok(cookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_require_a_safe_origin() {
        for url in [
            "http://other.example",
            "https://user:password@example.com",
            "https://example.com/api",
            "https://example.com?x=1",
            "https://example.com#x",
        ] {
            assert!(validated_origin(url).is_err(), "{url}");
        }
        for url in [
            "https://example.com",
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
            "http://localhost:8080",
        ] {
            assert!(validated_origin(url).is_ok(), "{url}");
        }
    }

    // Run the fixture in a child process so HOME and the stale wrapper path are
    // isolated without changing environment variables in parallel test threads.
    #[test]
    fn managed_client_uses_per_server_runtime() {
        const CHILD: &str = "CONVERSATIONS_MESSAGING_RUNTIME_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "commands::messaging::tests::managed_client_uses_per_server_runtime",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("HOME", home.path())
                .env("XDG_DATA_HOME", home.path().join("data"))
                .env(
                    "CONVERSATIONS_HOST_DIR",
                    home.path().join("missing-wrapper-runtime"),
                )
                .status()
                .unwrap();
            assert!(
                status.success(),
                "isolated messaging fixture failed: {status}"
            );
            return;
        }

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            use crate::commands::host_owner::HostOwner;
            use crate::commands::local_host::authz::store::Store;
            use crate::commands::local_host::protocol::{
                read_message, write_message, ClientMessage, Launch, Registration, ServerMessage,
                SessionStatus,
            };
            use crate::commands::local_host::Backend;
            use conversations_core::AgentKind;

            let runtime = tempfile::tempdir().unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let server = format!("https://{}.example.test", uuid::Uuid::new_v4());
            let source = uuid::Uuid::new_v4().to_string();
            let native = uuid::Uuid::new_v4().to_string();
            let owner = HostOwner::acquire(&server, Some(runtime.path())).unwrap();
            Store::open(runtime.path())
                .unwrap()
                .bind_native(&source, &native)
                .unwrap();
            let listener =
                tokio::net::UnixListener::bind(runtime.path().join("host.sock")).unwrap();
            let fixture_source = source.clone();
            let fixture_native = native.clone();
            let responder = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    assert!(matches!(
                        read_message::<_, ClientMessage>(&mut socket).await.unwrap(),
                        ClientMessage::Status
                    ));
                    write_message(
                        &mut socket,
                        &ServerMessage::Status {
                            backend: Backend::Normal,
                            sessions: vec![SessionStatus {
                                native_session_id: Some(fixture_native.clone()),
                                native_identity_supported: true,
                                readiness: None,
                                registration: Registration {
                                    launch: Launch {
                                        id: fixture_source.clone(),
                                        agent: AgentKind::Codex,
                                        cwd: "/fixture".into(),
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
                                local_input_bytes: 0,
                                local_input_dropped_chunks: 0,
                            }],
                            in_place_sessions: true,
                        },
                    )
                    .await
                    .unwrap();
                }
            });
            let client = MessagingClient::with_cookie(&server, &source, None).unwrap();
            let metadata = client
                .session_metadata()
                .await
                .expect("registered source must be found via the active per-server host");
            assert_eq!(
                metadata,
                json!({"agent": "codex", "agent_session_id": native})
            );
            // Keep the socket responding after the owner is dropped: selecting
            // its stale record must produce a successful response and fail this test.

            // Another server and a stopped owner must not borrow this host.
            let other = format!("https://{}.example.test", uuid::Uuid::new_v4());
            let error = MessagingClient::with_cookie(&other, &source, None)
                .unwrap()
                .session_metadata()
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains("local Conversations host is not reachable"));
            drop(owner);
            let error = MessagingClient::with_cookie(&server, &source, None)
                .unwrap()
                .session_metadata()
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains("local Conversations host is not reachable"));
            responder.abort();
        });
    }

    async fn rejection(response: String, path: &'static str, body: Value) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let client = MessagingClient::with_cookie(&origin, "source", None).unwrap();
        client
            .central_request(Method::POST, &[path], Some(body))
            .await
            .unwrap_err()
            .to_string()
    }

    fn plain_text_422(body: &str) -> String {
        format!("HTTP/1.1 422 Unprocessable Entity\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
    }

    // MCP identity must remain usable after a newer host migrates its database.
    #[tokio::test]
    async fn session_identity_survives_a_newer_host_database() {
        use crate::commands::local_host::protocol::{read_message, write_message};
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        drop(crate::commands::local_host::authz::store::Store::open(dir.path()).unwrap());
        let db = rusqlite::Connection::open(dir.path().join("authz.db")).unwrap();
        db.pragma_update(None, "user_version", 999).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let native = uuid::Uuid::new_v4().to_string();
        let listener = tokio::net::UnixListener::bind(dir.path().join("host.sock")).unwrap();
        let identity = native.clone();
        let source = session.clone();
        let responder = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request: Value = read_message(&mut socket).await.unwrap();
            assert_eq!(request, json!({"type": "status"}));
            write_message(&mut socket, &json!({
                "type": "status", "backend": "normal", "sessions": [{
                    "launch": {"id": source, "agent": "codex", "cwd": "/fixture", "command": []},
                    "workspace": null, "surface": null, "local_terminal": true,
                    "reconnecting": false, "file_receiver": false,
                    "transfer_gate": false, "effect_gate": false,
                    "native_identity_supported": true,
                    "native_session_id": identity
                }]
            })).await.unwrap();
        });
        let client = MessagingClient::with_cookie("http://127.0.0.1:1", &session, None)
            .unwrap()
            .with_local_dir(dir.path().to_owned());
        let metadata = client.session_metadata().await.unwrap();
        assert_eq!(
            metadata,
            json!({"agent": "codex", "agent_session_id": native})
        );
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            999
        );
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn legacy_host_identity_is_rejected_without_a_native_id_fallback() {
        use crate::commands::local_host::protocol::{read_message, write_message};
        for agent in ["claude", "codex", "opencode"] {
            let dir = tempfile::tempdir().unwrap();
            let source = uuid::Uuid::new_v4().to_string();
            let session = source.clone();
            let listener = tokio::net::UnixListener::bind(dir.path().join("host.sock")).unwrap();
            let responder = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let _: Value = read_message(&mut socket).await.unwrap();
                write_message(&mut socket, &json!({
                    "type": "status", "backend": "normal", "sessions": [{
                        "launch": {"id": session, "agent": agent, "cwd": "/fixture", "command": []},
                        "workspace": null, "surface": null, "local_terminal": true,
                        "reconnecting": false, "file_receiver": false,
                        "transfer_gate": false, "effect_gate": false
                    }]
                })).await.unwrap();
            });
            let client = MessagingClient::with_cookie("http://127.0.0.1:1", &source, None)
                .unwrap()
                .with_local_dir(dir.path().to_owned());
            let error = client.native_session_id().await.unwrap_err().to_string();
            assert!(
                error.contains("restart or upgrade the Conversations host"),
                "{agent}: {error}"
            );
            responder.await.unwrap();
        }
    }

    #[tokio::test]
    async fn host_status_error_is_preserved() {
        use crate::commands::local_host::protocol::{read_message, write_message};
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::UnixListener::bind(dir.path().join("host.sock")).unwrap();
        let responder = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _: Value = read_message(&mut socket).await.unwrap();
            write_message(
                &mut socket,
                &json!({"type": "error", "message": "could not read host status: schema 999"}),
            )
            .await
            .unwrap();
        });
        let client = MessagingClient::with_cookie("http://127.0.0.1:1", "source", None)
            .unwrap()
            .with_local_dir(dir.path().to_owned());
        let error = client.session_metadata().await.unwrap_err().to_string();
        assert_eq!(error, "could not read host status: schema 999");
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn an_older_server_without_the_route_says_to_update_it() {
        let error = rejection(
            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
            "end",
            json!({"session_id":"child"}),
        )
        .await;
        assert!(error.contains("update the Conversations server"), "{error}");
        assert!(!error.contains("non-JSON"), "{error}");
    }

    #[tokio::test]
    async fn an_older_server_rejecting_model_says_what_to_do() {
        let model = json!({"model":"anthropic/claude-opus-5-5"});
        let error = rejection(
            plain_text_422(
                "Failed to deserialize the JSON body into the target type: unknown field `model`",
            ),
            "handoffs",
            model.clone(),
        )
        .await;
        assert!(error.contains("update the Conversations server"), "{error}");
        assert!(
            error.contains("without the newer arguments such as model"),
            "{error}"
        );
        // Any other plain-text rejection keeps its own reason.
        let error = rejection(
            plain_text_422("Failed to deserialize the JSON body into the target type: agent: unknown variant `zsh2`"),
            "handoffs",
            model,
        )
        .await;
        assert!(error.contains("unknown variant `zsh2`"), "{error}");
        assert!(
            !error.contains("update the Conversations server"),
            "{error}"
        );
    }
}
