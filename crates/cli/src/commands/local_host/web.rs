//! Owner-only loopback management. Remote directory entries never grant access.
#[path = "web_access.rs"]
mod access;
#[path = "web_attention.rs"]
mod attention;
#[path = "web_communications.rs"]
mod communications;
#[path = "web_devices.rs"]
mod devices;
#[path = "web_linear.rs"]
mod linear;
#[path = "web_local.rs"]
mod local;
#[path = "web_metadata.rs"]
mod metadata;
#[path = "web_passkeys.rs"]
mod passkeys;
#[path = "web_pr.rs"]
mod pr;
#[path = "web_projects.rs"]
mod projects;
#[cfg(test)]
#[path = "web_ui_tests.rs"]
mod ui_tests;
#[path = "web_transfers.rs"]
mod uploads;
include!(concat!(env!("OUT_DIR"), "/frontend_assets.rs"));
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use super::protocol::{
    read_message, write_message, ClientMessage, LocalTerminalInput, ServerMessage,
};
use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Path as RoutePath, Query, Request, State,
    },
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use futures::{SinkExt, StreamExt};
use ring::rand::SecureRandom;
use serde::Deserialize;
use serde_json::{json, Value};

use super::authz::{
    self,
    envelope::{key_id, Alg},
    store::Store,
};

pub(super) struct WebState {
    pub(super) dir: PathBuf,
    pub(super) server: String,
    /// The one-time bootstrap token awaiting exchange. The local owner rotates
    /// it over the host socket (`host web`); a successful unlock consumes it.
    pending_token: Mutex<Option<(String, String)>>,
    revoked: tokio::sync::watch::Sender<u64>,
    needs_setup: bool,
    pub(super) port: u16,
}

fn random_token() -> Result<String> {
    let mut bytes = [0; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("could not generate local management credential"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

// Separate from host authority migrations. The database uses the same private
// directory and file checks as the host identity database.
fn owner_database(dir: &Path) -> Result<rusqlite::Connection> {
    let db = authz::store::open_private(dir, "web-owner.db")?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS browser_owner(
            id INTEGER PRIMARY KEY CHECK(id = 1),
            cookie TEXT NOT NULL,
            setup_complete INTEGER NOT NULL DEFAULT 0
        );",
    )?;
    Ok(db)
}

impl WebState {
    fn new(dir: &Path, server: &str, port: u16) -> Result<Arc<Self>> {
        let db = owner_database(dir)?;
        db.execute(
            "INSERT OR IGNORE INTO browser_owner(id, cookie, setup_complete) VALUES(1, ?1, 0)",
            [random_token()?],
        )?;
        let setup_complete: bool = db.query_row(
            "SELECT setup_complete FROM browser_owner WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(Arc::new(Self {
            dir: dir.to_owned(),
            server: server.trim_end_matches('/').to_owned(),
            pending_token: Mutex::new(None),
            revoked: tokio::sync::watch::channel(0).0,
            needs_setup: !setup_complete,
            port,
        }))
    }

    /// A fresh one-time owner link. Its token travels only in the URL fragment
    /// handed to the local browser — never to logs, HTTP requests or the
    /// hosted application.
    pub(super) fn rotate_bootstrap_url(&self) -> Result<String> {
        let token = random_token()?;
        let url = format!("http://localhost:{}/#token={token}", self.port);
        let mut pending = self.pending_token.lock().unwrap_or_else(|e| e.into_inner());
        let cookie = owner_database(&self.dir)?.query_row(
            "SELECT cookie FROM browser_owner WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        *pending = Some((token, cookie));
        Ok(url)
    }

    fn initial_setup_url(&self) -> Result<Option<String>> {
        self.needs_setup
            .then(|| self.rotate_bootstrap_url())
            .transpose()
    }

    pub(super) fn open_initial_setup(&self) -> Result<()> {
        if let Some(url) = self.initial_setup_url()? {
            open_local_management(&url)?;
        }
        Ok(())
    }

    // Serialize exchange and revocation. A failed write leaves the token usable.
    fn exchange_token(&self, presented: &str) -> Result<Option<String>> {
        let mut pending = self.pending_token.lock().unwrap_or_else(|e| e.into_inner());
        let Some((token, issued_cookie)) = pending.as_ref() else {
            return Ok(None);
        };
        if token != presented {
            return Ok(None);
        }
        let mut db = owner_database(&self.dir)?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let cookie: String =
            tx.query_row("SELECT cookie FROM browser_owner WHERE id = 1", [], |row| {
                row.get(0)
            })?;
        if &cookie != issued_cookie {
            return Ok(None);
        }
        tx.execute(
            "UPDATE browser_owner SET setup_complete = 1 WHERE id = 1",
            [],
        )?;
        tx.commit()?;
        pending.take();
        Ok(Some(cookie))
    }

    pub(super) fn reset_browser_access(&self) -> Result<()> {
        let mut pending = self.pending_token.lock().unwrap_or_else(|e| e.into_inner());
        owner_database(&self.dir)?.execute(
            "UPDATE browser_owner SET cookie = ?1, setup_complete = 0 WHERE id = 1",
            [random_token()?],
        )?;
        pending.take();
        self.revoked.send_modify(|generation| *generation += 1);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn for_test(dir: &Path, server: &str, port: u16, owner_cookie: &str) -> Arc<Self> {
        let state = Self::new(dir, server, port).unwrap();
        owner_database(dir)
            .unwrap()
            .execute(
                "UPDATE browser_owner SET cookie = ?1, setup_complete = 1 WHERE id = 1",
                [owner_cookie],
            )
            .unwrap();
        state
    }
}

/// The loopback management server: bound once, then served until the caller's
/// shutdown signal (standalone) or task abort (the host that owns it).
pub(super) struct WebServer {
    listener: tokio::net::TcpListener,
    state: Arc<WebState>,
}

impl WebServer {
    pub(super) async fn bind(dir: &Path, server: &str, port: u16) -> Result<Self> {
        anyhow::ensure!(!SPA_ASSETS.is_empty(), "This CLI has no shared frontend. Run npm ci && npm run build in frontend/, then rebuild the CLI.");
        authz::ensure_dir(dir)?;
        Store::open(dir)?;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .with_context(|| {
                format!("binding local management server on localhost:{port} (is another process already serving it?)")
            })?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            state: WebState::new(dir, server, port)?,
        })
    }

    pub(super) fn state(&self) -> Arc<WebState> {
        self.state.clone()
    }

    pub(super) fn port(&self) -> u16 {
        self.state.port
    }

    pub(super) async fn serve(
        self,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        axum::serve(self.listener, router(self.state))
            .with_graceful_shutdown(shutdown)
            .await?;
        Ok(())
    }
}

pub async fn run(dir: &Path, server: &str, port: u16, reset_browser_access: bool) -> Result<()> {
    super::validate_server(server)?;
    let server = WebServer::bind(dir, server, port).await?;
    let url = format!("http://localhost:{}/", server.port());
    println!("Local host management: {url}");
    if reset_browser_access {
        server.state().reset_browser_access()?;
    }
    let bootstrap = server.state().rotate_bootstrap_url()?;
    open_local_management(&bootstrap)?;
    server
        .serve(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

/// Open the owner link in the local browser. The credential stays in the URL
/// fragment, out of logs, HTTP URLs and the hosted application.
pub(super) fn open_local_management(bootstrap_url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let opener = "/usr/bin/open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    let opener = std::env::var_os("CONVERSATIONS_BROWSER").unwrap_or_else(|| opener.into());
    std::process::Command::new(opener)
        .arg(bootstrap_url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("opening local management; set CONVERSATIONS_BROWSER to an installed browser launcher executable, then run `arrakis-conversations host web`")?;
    Ok(())
}

pub(super) fn router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/api/unlock", post(unlock))
        .route("/api/status", get(status))
        .route("/api/terminal/{id}", get(terminal))
        .route("/api/users", get(users))
        .route("/api/trust", post(trust))
        .route("/api/trust/upgrade", post(upgrade_owner))
        .route(
            "/api/app",
            get(|| async { Json(json!({"version":env!("CARGO_PKG_VERSION")})) }),
        )
        .route("/api/local", get(local::marker))
        .route("/api/local/sessions", get(local::list))
        .route(
            "/api/local/sessions/{id}/migration",
            get(local::migration_export),
        )
        .route("/api/local/migration", post(local::migration_import))
        .route("/api/local/sessions/{id}/prs", get(pr::list))
        .route("/api/local/sessions/{id}/prs/sync", post(pr::sync))
        .route("/api/local/sessions/{id}/prs/comment", post(pr::comment))
        .route("/api/local/sessions/{id}/prs/unlink", post(pr::unlink))
        .route(
            "/api/local/sessions/{id}/linear",
            get(linear::list).delete(linear::unlink),
        )
        .route("/api/local/sessions/{id}/history", get(local::history))
        .route("/api/local/sessions/{id}/fragment", get(local::fragment))
        .route("/api/local/sessions/{id}/search", get(local::search))
        .route("/api/local/sessions/{id}/reopen", post(local::reopen))
        .route("/api/local/sessions/{id}/reply", post(local::reply))
        .route("/api/projects", get(projects::list).post(projects::create))
        .route(
            "/api/projects/{id}",
            get(projects::detail)
                .put(projects::update)
                .delete(projects::delete),
        )
        .route(
            "/api/sessions/{id}/project",
            axum::routing::put(projects::assign).get(projects::membership),
        )
        .route("/api/llm/status", get(projects::llm_status))
        .route("/api/local/passkeys", post(passkeys::operate))
        .route("/api/devices/configure", post(devices::configure))
        .route("/api/devices/start", post(devices::start))
        .route("/api/devices/pending", get(devices::pending))
        .route("/api/devices/approve", post(devices::approve))
        .route("/api/devices/reject", post(devices::reject))
        .route("/api/local/start", post(local::start))
        .route(
            "/api/local/sessions/{id}/transfer",
            post(uploads::upload).layer(DefaultBodyLimit::max(uploads::MAX_UPLOAD_BODY)),
        )
        .route(
            "/api/local/sessions/{id}/communications",
            get(communications::list),
        )
        .route(
            "/api/local/sessions/{id}/attention",
            post(attention::request),
        )
        .route(
            "/api/local/sessions/{id}/attention-recipients",
            get(attention::recipients),
        )
        .route("/api/local/notifications", get(attention::list))
        .route(
            "/api/local/sessions/{id}/notifications/{notification}/read",
            post(attention::read),
        )
        .route(
            "/api/local/sessions/{id}/notifications/{notification}/resolve",
            post(attention::resolve),
        )
        .route("/api/local/sessions/{id}/rename", post(metadata::rename))
        .route("/api/local/sessions/{id}/publication", get(access::status))
        .route(
            "/api/local/sessions/{id}/publication/share",
            post(access::share),
        )
        .route(
            "/api/local/sessions/{id}/publication/unshare",
            post(access::unshare),
        )
        .route("/api/local/sessions/{id}/grants", get(access::grants))
        .route("/api/local/sessions/{id}/grants/add", post(access::add))
        .route(
            "/api/local/sessions/{id}/grants/remove",
            post(access::remove),
        )
        .route("/api/local/sessions/{id}/end", post(local::end))
        .route(
            "/api/local/preferences",
            get(local::preferences).put(local::save_preferences),
        )
        .route(
            "/api/local/settings",
            get(local::settings).post(local::save_settings),
        )
        .fallback(spa)
        .layer(DefaultBodyLimit::max(128 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}
fn has_owner_cookie(state: &WebState, headers: &axum::http::HeaderMap) -> bool {
    let expected = owner_database(&state.dir).and_then(|db| {
        Ok(
            db.query_row("SELECT cookie FROM browser_owner WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })?,
        )
    });
    let Ok(expected) = expected else {
        return false;
    };
    headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|cookies| {
            cookies
                .split(';')
                .any(|part| part.trim().strip_prefix("ac_local_owner=") == Some(expected.as_str()))
        })
}
async fn spa(request: Request) -> Response {
    let path = request.uri().path();
    if path.starts_with("/api/") {
        return error(StatusCode::NOT_FOUND, "Unknown local API.");
    }
    let asset = SPA_ASSETS
        .iter()
        .find(|(name, _)| *name == path)
        .or_else(|| {
            if path
                .rsplit('/')
                .next()
                .is_some_and(|name| !name.contains('.'))
            {
                SPA_ASSETS.iter().find(|(name, _)| *name == "/index.html")
            } else {
                None
            }
        });
    let Some((name, bytes)) = asset else {
        return error(StatusCode::NOT_FOUND, "Frontend asset not found.");
    };
    let mime = if name.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if name.ends_with(".js") {
        "text/javascript"
    } else if name.ends_with(".css") {
        "text/css"
    } else if name.ends_with(".svg") {
        "image/svg+xml"
    } else if name.ends_with(".png") {
        "image/png"
    } else if name.ends_with(".woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    };
    ([(header::CONTENT_TYPE, mime)], *bytes).into_response()
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
}

async fn guard(State(state): State<Arc<WebState>>, request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let hosts = [
        format!("localhost:{}", state.port),
        format!("127.0.0.1:{}", state.port),
    ];
    if !hosts.iter().any(|h| h == host) {
        return error(
            StatusCode::FORBIDDEN,
            "This page is available only on localhost.",
        );
    }
    if (request.method() != Method::GET && request.method() != Method::HEAD)
        || request.uri().path().starts_with("/api/terminal/")
    {
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok());
        if origin != Some(format!("http://{host}").as_str()) {
            return error(
                StatusCode::FORBIDDEN,
                "Open this action from the local management page.",
            );
        }
    }
    if request.uri().path().starts_with("/api/")
        && request.uri().path() != "/api/unlock"
        && request.uri().path() != "/api/local"
    {
        let authorized = has_owner_cookie(&state, request.headers());
        if !authorized {
            return error(
                StatusCode::UNAUTHORIZED,
                "Open this page using host web on your laptop.",
            );
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data: blob:; font-src 'self' data:; worker-src 'self' blob:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    response
}

async fn terminal(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if uuid::Uuid::parse_str(&id).is_err() {
        return error(StatusCode::BAD_REQUEST, "Invalid session.");
    }
    let mut revoked = state.revoked.subscribe();
    if !has_owner_cookie(&state, &headers) {
        return error(StatusCode::UNAUTHORIZED, "Owner access required.");
    }
    ws.max_message_size(128 * 1024)
        .on_upgrade(move |socket| async move {
            tokio::select! {
                _ = terminal_bridge(socket, state, id) => {},
                _ = revoked.changed() => {},
            }
        })
        .into_response()
}
async fn terminal_bridge(browser: WebSocket, state: Arc<WebState>, id: String) {
    let result: Result<()> = async {
        let mut host = tokio::net::UnixStream::connect(state.dir.join("host.sock")).await?;
        write_message(&mut host, &ClientMessage::LocalTerminal { id }).await?;
        let (mut reader, mut writer) = host.into_split();
        let (mut sender, mut receiver) = browser.split();
        // Separate futures preserve framing reads across the other direction's activity.
        let output = async {
            loop {
                let message: ServerMessage = read_message(&mut reader).await?;
                let failed = matches!(message, ServerMessage::Error { .. });
                sender
                    .send(Message::Text(serde_json::to_string(&message)?.into()))
                    .await?;
                if failed {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let input = async {
            while let Some(message) = receiver.next().await {
                match message? {
                    Message::Text(text) => {
                        let command: LocalTerminalInput = serde_json::from_str(&text)?;
                        write_message(&mut writer, &command).await?;
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                    _ => anyhow::bail!("unsupported terminal frame"),
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! { result = output => result, result = input => result }
    }
    .await;
    // Dropping the connection closes the browser; it explicitly offers reconnect.
    // Never log terminal input/output or connection errors containing payloads.
    let _ = result;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Unlock {
    token: String,
}
async fn unlock(State(state): State<Arc<WebState>>, Json(input): Json<Unlock>) -> Response {
    let owner_cookie = match state.exchange_token(&input.token) {
        Ok(Some(cookie)) => cookie,
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "This local unlock link is invalid or has already been used."),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Could not remember browser authorization. Retry after the local storage problem is resolved."),
    };
    let mut response = Json(json!({"ok":true})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "ac_local_owner={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000",
            owner_cookie
        ))
        .unwrap(),
    );
    response
}

fn local_status(state: &WebState) -> Result<Value> {
    let descriptor = authz::attest::descriptor(&state.dir)?;
    let credentials = super::access_metadata::list(&state.dir)?;
    Ok(json!({"descriptor":descriptor,"server":state.server,"credentials":credentials}))
}
async fn status(State(state): State<Arc<WebState>>) -> Response {
    match local_status(&state) {
        Ok(value) => Json(value).into_response(),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read this host's approved keys.",
        ),
    }
}

#[derive(Deserialize)]
struct Search {
    #[serde(default)]
    q: String,
}
async fn users(State(state): State<Arc<WebState>>, Query(search): Query<Search>) -> Response {
    let result: Result<Value> = async {
        anyhow::ensure!(search.q.len() <= 200, "search too long");
        let cookie = crate::credentials::cookie_for(&state.server).context("no saved login")?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        let mut url = reqwest::Url::parse(&format!("{}/api/user-keys", state.server))?;
        url.query_pairs_mut()
            .append_pair("q", &search.q)
            .append_pair("limit", "20");
        let response = client
            .get(url)
            .header(reqwest::header::COOKIE, cookie)
            .send()
            .await?
            .error_for_status()?;
        // Only public directory fields leave this process; the saved login never does.
        let directory: Directory = response.json().await?;
        Ok(serde_json::to_value(directory)?)
    }
    .await;
    match result { Ok(value) => Json(value).into_response(), Err(_) => error(StatusCode::BAD_GATEWAY,"Could not load published keys. Check your saved server login and that the server supports the key directory.") }
}
#[derive(Deserialize, serde::Serialize)]
struct Directory {
    users: Vec<DirectoryUser>,
}
#[derive(Deserialize, serde::Serialize)]
struct DirectoryUser {
    email: String,
    display_name: Option<String>,
    keys: Vec<DirectoryKey>,
}
#[derive(Deserialize, serde::Serialize)]
struct DirectoryKey {
    kid: String,
    alg: String,
    public_key: String,
    label: String,
    created_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Trust {
    kid: String,
    public_key: String,
    label: String,
    compared: bool,
    grants: Vec<String>,
}
async fn trust(State(state): State<Arc<WebState>>, Json(input): Json<Trust>) -> Response {
    let public = match STANDARD.decode(&input.public_key) {
        Ok(public) => public,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid public key."),
    };
    if !input.compared || !Alg::Es256.valid_public_key(&public) || key_id(&public) != input.kid {
        return error(
            StatusCode::BAD_REQUEST,
            "Compare this exact key's fingerprint with its owner before trusting it.",
        );
    }
    if input.grants.is_empty()
        || input
            .grants
            .iter()
            .any(|g| !is_enrollable_owner_cap(g.as_str()))
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Choose at least one supported permission.",
        );
    }
    match authz::enroll_from_cli(&state.dir,"owner","es256",&input.public_key,&input.label,&input.grants) {
        Ok((kid,_)) => Json(json!({"kid":kid})).into_response(),
        Err(e) if e.to_string().contains("already enrolled") => error(StatusCode::CONFLICT,"This key is already enrolled. Its existing permissions were not changed."),
        Err(_) => error(StatusCode::BAD_REQUEST,"Could not trust this key. Use a label of 1–64 printable characters and valid permissions."),
    }
}

/// Owner-held capabilities the local management UI may enroll or add to an
/// existing owner credential. `session.write`, `history.read` and the two
/// session-creating caps have always been accepted. The five owner-only caps
/// below belong to owner keys by design (`Cap::owner_only`) and are what the
/// browser's delegation needs for the PR rail, host settings and sharing to
/// work. A credential enrolled before this list grew can be upgraded via
/// `/api/trust/upgrade` without re-enrolling the passkey.
///
/// `session.pr_sync` is intentionally absent: PR refresh now follows
/// `session.write`, so new enrollments no longer hand out a standalone
/// refresh cap. Existing stored `session.pr_sync` rows remain parseable
/// for wire/storage compatibility and still gate PR commenting flows
/// that build on them, but we do not mint new ones here.
pub(super) fn is_enrollable_owner_cap(cap: &str) -> bool {
    matches!(
        cap,
        "history.read"
            | "session.write"
            | "session.start"
            | "session.reopen"
            | "session.pr_comment"
            | "session.manage_sharing"
            | "host.settings_read"
            | "host.settings_write"
            | "passkey.manage"
    )
}

/// Owner-only caps that this upgrade path may add to an existing credential.
/// The four caps the trust UI has always accepted are intentionally NOT in
/// this list: narrowing them would never be useful, and only additions are
/// allowed here. `session.pr_sync` is absent for the same reason as in
/// [`is_enrollable_owner_cap`]: the cap is obsolete.
pub(super) fn is_owner_only_upgrade_cap(cap: &str) -> bool {
    matches!(
        cap,
        "session.pr_comment"
            | "session.manage_sharing"
            | "host.settings_read"
            | "host.settings_write"
            | "passkey.manage"
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upgrade {
    kid: String,
    grants: Vec<String>,
}

/// Add owner-only capabilities (host.settings_*, session.pr_sync, …) to an
/// existing owner credential on this laptop. The guard middleware already
/// pins Origin and Host to the loopback management page and requires the
/// one-use owner cookie: this handler is the only way, from this page, to
/// ever broaden what an already-approved key may do, and only within the
/// explicit opt-in list above.
async fn upgrade_owner(State(state): State<Arc<WebState>>, Json(input): Json<Upgrade>) -> Response {
    if input.grants.is_empty()
        || input
            .grants
            .iter()
            .any(|g| !is_owner_only_upgrade_cap(g.as_str()))
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Choose at least one owner-only permission to add.",
        );
    }
    let caps: Vec<authz::envelope::Cap> = input
        .grants
        .iter()
        .filter_map(|g| authz::envelope::Cap::parse(g))
        .collect();
    let mut store = match Store::open(&state.dir) {
        Ok(store) => store,
        Err(_) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not open this host's approvals.",
            );
        }
    };
    match store.grant_owner_only_caps(&input.kid, &caps) {
        Ok(added) => {
            let added: Vec<&'static str> = added
                .into_iter()
                .map(authz::envelope::Cap::as_str)
                .collect();
            Json(json!({"kid": input.kid, "added": added})).into_response()
        }
        Err(e) if e.to_string().contains("no credential") => error(
            StatusCode::NOT_FOUND,
            "No approved key on this host has that id.",
        ),
        Err(e) if e.to_string().contains("owner credential") => error(
            StatusCode::CONFLICT,
            "These permissions belong to owner keys only.",
        ),
        Err(e) if e.to_string().contains("revoked") => error(
            StatusCode::CONFLICT,
            "This key is revoked; enroll a new one.",
        ),
        Err(_) => error(
            StatusCode::BAD_REQUEST,
            "Could not add these permissions. Review the owner-only permission list.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use ring::signature::KeyPair;
    use serde_json::{json, Value};
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _dir: tempfile::TempDir,
        state: Arc<WebState>,
        token: String,
        base: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        super::super::authz::host_id(dir.path()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = WebState::new(dir.path(), "https://example.com", port).unwrap();
        let bootstrap = state.rotate_bootstrap_url().unwrap();
        let token = bootstrap.rsplit("#token=").next().unwrap().to_owned();
        let app = router(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Fixture {
            _dir: dir,
            state,
            token,
            base: format!("http://127.0.0.1:{port}"),
            task,
        }
    }
    async fn cookie(f: &Fixture) -> String {
        let r = reqwest::Client::new()
            .post(format!("{}/api/unlock", f.base))
            .header("Origin", &f.base)
            .json(&json!({"token":f.token}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let cookie = r.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=31536000"));
        cookie.split(';').next().unwrap().to_owned()
    }
    #[tokio::test]
    async fn browser_authorization_survives_host_restart() {
        let mut f = fixture().await;
        let setup = f
            .state
            .initial_setup_url()
            .unwrap()
            .expect("first start opens setup");
        f.token = setup.rsplit("#token=").next().unwrap().to_owned();
        let c = cookie(&f).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let restarted = WebState::new(f._dir.path(), "https://example.com", port).unwrap();
        assert!(restarted.initial_setup_url().unwrap().is_none());
        let other = tempfile::tempdir().unwrap();
        std::fs::set_permissions(other.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let unrelated = WebState::new(other.path(), "https://example.com", port).unwrap();
        assert!(unrelated.needs_setup);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(&c).unwrap());
        assert!(!has_owner_cookie(&unrelated, &headers));
        let task = tokio::spawn(async move {
            axum::serve(listener, router(restarted)).await.unwrap();
        });
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://127.0.0.1:{port}/api/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = client
            .post(format!("http://127.0.0.1:{port}/api/unlock"))
            .header("Origin", format!("http://127.0.0.1:{port}"))
            .json(&json!({"token": f.token}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = client
            .get(format!("http://127.0.0.1:{port}/api/status"))
            .header("Cookie", c)
            .send()
            .await
            .unwrap();
        task.abort();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_failed_setup_write_leaves_the_owner_link_usable() {
        let f = fixture().await;
        let db = owner_database(f._dir.path()).unwrap();
        db.execute_batch("CREATE TRIGGER reject_setup BEFORE UPDATE ON browser_owner BEGIN SELECT RAISE(FAIL, 'temporary write failure'); END;").unwrap();
        let client = reqwest::Client::new();
        let attempt = || {
            client
                .post(format!("{}/api/unlock", f.base))
                .header("Origin", &f.base)
                .json(&json!({"token": f.token}))
        };
        assert_eq!(attempt().send().await.unwrap().status(), 500);
        db.execute_batch("DROP TRIGGER reject_setup").unwrap();
        assert_eq!(attempt().send().await.unwrap().status(), 200);
        assert_eq!(attempt().send().await.unwrap().status(), 401);
    }

    #[tokio::test]
    async fn reset_revokes_browser_access_pending_links_and_open_terminals() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let f = fixture().await;
        let old_cookie = cookie(&f).await;
        let other_state =
            WebState::new(f._dir.path(), "https://example.com", f.state.port).unwrap();
        let other_link = other_state.rotate_bootstrap_url().unwrap();
        let stale = f.state.rotate_bootstrap_url().unwrap();
        let stale = stale.rsplit("#token=").next().unwrap().to_owned();
        let listener = tokio::net::UnixListener::bind(f.state.dir.join("host.sock")).unwrap();
        let (ready, connected) = tokio::sync::oneshot::channel();
        let host = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _: ClientMessage = read_message(&mut socket).await.unwrap();
            ready.send(()).unwrap();
            assert!(read_message::<_, LocalTerminalInput>(&mut socket)
                .await
                .is_err());
        });
        let mut request = format!(
            "{}/api/terminal/00000000-0000-0000-0000-000000000001",
            f.base.replace("http:", "ws:")
        )
        .into_client_request()
        .unwrap();
        request
            .headers_mut()
            .insert("Origin", f.base.parse().unwrap());
        request
            .headers_mut()
            .insert("Cookie", old_cookie.parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        connected.await.unwrap();
        f.state.reset_browser_access().unwrap();
        assert!(other_state
            .exchange_token(other_link.rsplit("#token=").next().unwrap())
            .unwrap()
            .is_none());
        let closed = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap();
        assert!(matches!(
            closed,
            None | Some(Err(_)) | Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
        ));
        host.await.unwrap();
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{}/api/status", f.base))
            .header("Cookie", &old_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        let response = client
            .post(format!("{}/api/unlock", f.base))
            .header("Origin", &f.base)
            .json(&json!({"token": stale}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        let fresh = f.state.rotate_bootstrap_url().unwrap();
        let response = client
            .post(format!("{}/api/unlock", f.base))
            .header("Origin", &f.base)
            .json(&json!({"token": fresh.rsplit("#token=").next().unwrap()}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let fresh_cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        assert_eq!(
            client
                .get(format!("{}/api/status", f.base))
                .header("Cookie", fresh_cookie)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            client
                .get(format!("{}/api/status", f.base))
                .header("Cookie", &old_cookie)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }

    #[tokio::test]
    async fn local_management_requires_owner_cookie_host_and_origin() {
        let f = fixture().await;
        let client = reqwest::Client::new();
        let r = client
            .get(format!("{}/api/status", f.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        let r = client
            .post(format!("{}/api/unlock", f.base))
            .header("Origin", "https://evil.example")
            .json(&json!({"token":f.token}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = client
            .post(format!("{}/api/unlock", f.base))
            .header("Origin", &f.base)
            .json(&json!({"token":"wrong"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        let c = cookie(&f).await;
        let r = client
            .get(format!("{}/api/status", f.base))
            .header("Cookie", &c)
            .header("Host", "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = client
            .get(format!("{}/api/status", f.base))
            .header("Cookie", &c)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.json::<Value>().await.unwrap()["descriptor"]
            .as_str()
            .unwrap()
            .starts_with("chd1."));
        let r = client
            .post(format!("{}/api/trust", f.base))
            .header("Cookie", &c)
            .header("Origin", "https://evil.example")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }
    #[tokio::test]
    async fn a_bootstrap_token_is_single_use_and_the_owner_can_issue_a_fresh_one() {
        let f = fixture().await;
        let client = reqwest::Client::new();
        let unlock = |token: &str| {
            client
                .post(format!("{}/api/unlock", f.base))
                .header("Origin", &f.base)
                .json(&json!({"token":token}))
        };
        // Never issued.
        assert_eq!(unlock("not-issued").send().await.unwrap().status(), 401);
        let first = cookie(&f).await;
        // Already used.
        assert_eq!(unlock(&f.token).send().await.unwrap().status(), 401);
        // The local owner rotates the link over the host socket (`host web`)
        // and the fresh token works while the old cookie stays valid.
        let bootstrap = f.state.rotate_bootstrap_url().unwrap();
        let token = bootstrap.rsplit("#token=").next().unwrap().to_owned();
        assert_eq!(unlock(&token).send().await.unwrap().status(), 200);
        let r = client
            .get(format!("{}/api/status", f.base))
            .header("Cookie", &first)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    #[tokio::test]
    async fn trust_pins_exact_compared_key_and_never_broadens_existing_grants() {
        let f = fixture().await;
        let cookie = cookie(&f).await;
        let client = reqwest::Client::new();
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .unwrap();
        let key = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8.as_ref(),
            &rng,
        )
        .unwrap();
        let kid = super::super::authz::envelope::key_id(key.public_key().as_ref());
        let mut body = json!({"kid":kid,"public_key":STANDARD.encode(key.public_key().as_ref()),"label":"Alex · browser","compared":false,"grants":["history.read"]});
        let send = |v: Value| {
            client
                .post(format!("{}/api/trust", f.base))
                .header("Origin", &f.base)
                .header("Cookie", &cookie)
                .json(&v)
                .send()
        };
        assert_eq!(send(body.clone()).await.unwrap().status(), 400);
        body["compared"] = json!(true);
        body["kid"] = json!("different-key");
        assert_eq!(send(body.clone()).await.unwrap().status(), 400);
        body["kid"] = json!(kid);
        assert_eq!(send(body.clone()).await.unwrap().status(), 200);
        body["grants"] = json!(["history.read", "session.write"]);
        assert_eq!(send(body).await.unwrap().status(), 409);
        let store = super::super::authz::store::Store::open(&f.state.dir).unwrap();
        let caps: Vec<String> = store
            .connection()
            .prepare("SELECT cap FROM grants WHERE cred_id=?1")
            .unwrap()
            .query_map([kid], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(caps, vec!["history.read"]);
    }
    /// A new owner enrolment via the local trust UI must be able to pick the
    /// owner-only capabilities the browser's delegation asks for (PR refresh,
    /// PR comment, sharing management, host settings). Regression for owners
    /// who followed the first-time-setup instructions and ended up with only
    /// `history.read,session.write,session.start,session.reopen` — PR refresh
    /// then fails on the delegation path with "operation outside the
    /// delegation's scope" because the parent credential never held pr_sync.
    #[tokio::test]
    async fn trust_accepts_owner_only_caps_for_new_owner_credential() {
        let f = fixture().await;
        let cookie = cookie(&f).await;
        let client = reqwest::Client::new();
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .unwrap();
        let key = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8.as_ref(),
            &rng,
        )
        .unwrap();
        let kid = super::super::authz::envelope::key_id(key.public_key().as_ref());
        let body = json!({
            "kid": kid,
            "public_key": STANDARD.encode(key.public_key().as_ref()),
            "label": "Owner · laptop",
            "compared": true,
            "grants": [
                "history.read",
                "session.write",
                "session.start",
                "session.reopen",
                "session.pr_comment",
                "session.manage_sharing",
                "host.settings_read",
                "host.settings_write",
                "passkey.manage",
            ],
        });
        let response = client
            .post(format!("{}/api/trust", f.base))
            .header("Origin", &f.base)
            .header("Cookie", &cookie)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let store = super::super::authz::store::Store::open(&f.state.dir).unwrap();
        let mut caps: Vec<String> = store
            .connection()
            .prepare("SELECT cap FROM grants WHERE cred_id=?1 ORDER BY cap")
            .unwrap()
            .query_map([&kid], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        caps.sort();
        // PR refresh now follows session.write, so there is no standalone
        // session.pr_sync cap on a freshly enrolled owner.
        assert_eq!(
            caps,
            vec![
                "history.read",
                "host.settings_read",
                "host.settings_write",
                "passkey.manage",
                "session.manage_sharing",
                "session.pr_comment",
                "session.reopen",
                "session.start",
                "session.write",
            ]
        );
    }

    /// The trust endpoint refuses to mint the obsolete `session.pr_sync`
    /// cap: new credentials rely on `session.write` for PR refresh.
    #[tokio::test]
    async fn trust_refuses_to_mint_obsolete_pr_sync_cap() {
        let f = fixture().await;
        let cookie = cookie(&f).await;
        let client = reqwest::Client::new();
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .unwrap();
        let key = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8.as_ref(),
            &rng,
        )
        .unwrap();
        let kid = super::super::authz::envelope::key_id(key.public_key().as_ref());
        let body = json!({
            "kid": kid,
            "public_key": STANDARD.encode(key.public_key().as_ref()),
            "label": "Owner · laptop",
            "compared": true,
            "grants": [
                "history.read",
                "session.write",
                "session.pr_sync",
            ],
        });
        let response = client
            .post(format!("{}/api/trust", f.base))
            .header("Origin", &f.base)
            .header("Cookie", &cookie)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }

    /// The legacy-owner repair path. A credential enrolled before the trust
    /// allowlist grew holds only the historical four caps; adding
    /// `session.pr_comment` (etc.) through this endpoint must (a) require
    /// the owner cookie, (b) only accept owner-only caps, (c) land the
    /// cap in both `grants` (host-wide) and the credential's `ceiling` so a
    /// fresh browser delegation includes it. Agent credentials are refused.
    ///
    /// `session.pr_sync` is no longer an upgradable cap: PR refresh now
    /// follows `session.write`, which the legacy four-cap owner already
    /// holds. The test proves the upgrade endpoint refuses the obsolete
    /// name.
    #[tokio::test]
    async fn upgrade_adds_owner_only_caps_only_to_owner_credential_and_requires_cookie() {
        use super::super::authz;
        let f = fixture().await;
        let client = reqwest::Client::new();
        // A legacy owner with exactly the four caps the LIVE EVIDENCE shows.
        let rng = ring::rand::SystemRandom::new();
        let owner_pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let owner_pair = ring::signature::Ed25519KeyPair::from_pkcs8(owner_pkcs8.as_ref()).unwrap();
        let owner_pub = owner_pair.public_key().as_ref().to_vec();
        let (owner_kid, _) = authz::enroll_from_cli(
            &f.state.dir,
            "owner",
            "ed25519",
            &STANDARD.encode(&owner_pub),
            "legacy-owner",
            &[
                "history.read".into(),
                "session.write".into(),
                "session.start".into(),
                "session.reopen".into(),
            ],
        )
        .unwrap();
        // An agent key, which must never gain an owner-only cap.
        let agent_pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let agent_pair = ring::signature::Ed25519KeyPair::from_pkcs8(agent_pkcs8.as_ref()).unwrap();
        let agent_pub = agent_pair.public_key().as_ref().to_vec();
        let (agent_kid, _) = authz::enroll_from_cli(
            &f.state.dir,
            "agent",
            "ed25519",
            &STANDARD.encode(&agent_pub),
            "agent",
            &[format!(
                "session.write:{}",
                uuid::Uuid::new_v4().hyphenated()
            )],
        )
        .unwrap();

        // No owner cookie → UNAUTHORIZED.
        let r = client
            .post(format!("{}/api/trust/upgrade", f.base))
            .header("Origin", &f.base)
            .json(&json!({"kid": owner_kid, "grants": ["session.pr_comment"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);

        let cookie = cookie(&f).await;

        // Unknown cap → BAD_REQUEST, nothing added.
        let r = client
            .post(format!("{}/api/trust/upgrade", f.base))
            .header("Origin", &f.base)
            .header("Cookie", &cookie)
            .json(&json!({"kid": owner_kid, "grants": ["session.write"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);

        // The obsolete standalone pr_sync cap is no longer upgradable: PR
        // refresh follows session.write, which the legacy owner already
        // holds.
        let r = client
            .post(format!("{}/api/trust/upgrade", f.base))
            .header("Origin", &f.base)
            .header("Cookie", &cookie)
            .json(&json!({"kid": owner_kid, "grants": ["session.pr_sync"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);

        // Agent credential → refused.
        let r = client
            .post(format!("{}/api/trust/upgrade", f.base))
            .header("Origin", &f.base)
            .header("Cookie", &cookie)
            .json(&json!({"kid": agent_kid, "grants": ["passkey.manage"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 409);

        // Owner credential is upgraded exactly with the requested owner-only caps.
        let r = client
            .post(format!("{}/api/trust/upgrade", f.base))
            .header("Origin", &f.base)
            .header("Cookie", &cookie)
            .json(&json!({
                "kid": owner_kid,
                "grants": ["session.pr_comment", "session.manage_sharing", "passkey.manage"],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        let added: Vec<String> = body["added"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert!(added.contains(&"session.pr_comment".into()));
        assert!(added.contains(&"session.manage_sharing".into()));

        let store = super::super::authz::store::Store::open(&f.state.dir).unwrap();
        let grants: Vec<(String, String)> = store
            .connection()
            .prepare("SELECT cap, session_id FROM grants WHERE cred_id=?1 ORDER BY cap")
            .unwrap()
            .query_map([&owner_kid], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(grants.contains(&("session.pr_comment".into(), "*".into())));
        assert!(grants.contains(&("session.manage_sharing".into(), "*".into())));
        let ceiling: String = store
            .connection()
            .query_row(
                "SELECT ceiling FROM credentials WHERE cred_id=?1",
                [&owner_kid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(ceiling.contains("session.pr_comment"));
        assert!(ceiling.contains("session.manage_sharing"));
        assert!(ceiling.contains("passkey.manage"));
        assert!(grants.contains(&("passkey.manage".into(), "*".into())));

        // Re-running is a no-op: nothing new is added.
        let r = client
            .post(format!("{}/api/trust/upgrade", f.base))
            .header("Origin", &f.base)
            .header("Cookie", &cookie)
            .json(&json!({
                "kid": owner_kid,
                "grants": ["session.pr_comment"],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["added"], json!(Vec::<String>::new()));
    }

    #[tokio::test]
    async fn terminal_upgrade_requires_owner_and_exact_origin() {
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue};
        let f = fixture().await;
        let cookie = cookie(&f).await;
        let url = format!(
            "{}/api/terminal/00000000-0000-0000-0000-000000000001",
            f.base.replace("http:", "ws:")
        );
        for (origin, credential, expected) in [
            (f.base.as_str(), "", 401),
            ("https://evil.example", cookie.as_str(), 403),
            ("", cookie.as_str(), 403),
        ] {
            let mut request = url.clone().into_client_request().unwrap();
            request
                .headers_mut()
                .insert("Origin", HeaderValue::from_str(origin).unwrap());
            request
                .headers_mut()
                .insert("Cookie", HeaderValue::from_str(credential).unwrap());
            match tokio_tungstenite::connect_async(request).await {
                Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                    assert_eq!(response.status().as_u16(), expected)
                }
                _ => panic!("unauthorized terminal upgrade was not refused"),
            }
        }
    }
    #[tokio::test]
    async fn terminal_websocket_roundtrips_directly_to_owner_socket() {
        use tokio_tungstenite::tungstenite::{
            client::IntoClientRequest, http::HeaderValue, Message as WsMessage,
        };
        let f = fixture().await;
        let cookie = cookie(&f).await;
        let listener = tokio::net::UnixListener::bind(f.state.dir.join("host.sock")).unwrap();
        let host = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert!(matches!(
                read_message::<_, ClientMessage>(&mut socket).await.unwrap(),
                ClientMessage::LocalTerminal { .. }
            ));
            let frame = super::super::protocol::Frame {
                cols: 80,
                rows: 24,
                b64: STANDARD.encode(b"prompt"),
                kind: conversations_core::terminal::PtyFrameKind::Snapshot,
            };
            write_message(&mut socket, &ServerMessage::TerminalFrame { frame })
                .await
                .unwrap();
            let input: LocalTerminalInput = read_message(&mut socket).await.unwrap();
            match input {
                LocalTerminalInput::Input { b64 } => {
                    assert_eq!(STANDARD.decode(b64).unwrap(), b"hello\r\x1b\x03")
                }
                _ => panic!("wrong input"),
            }
            assert!(matches!(
                read_message::<_, LocalTerminalInput>(&mut socket)
                    .await
                    .unwrap(),
                LocalTerminalInput::Resize {
                    cols: 120,
                    rows: 40
                }
            ));
        });
        let mut request = format!(
            "{}/api/terminal/00000000-0000-0000-0000-000000000001",
            f.base.replace("http:", "ws:")
        )
        .into_client_request()
        .unwrap();
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_str(&f.base).unwrap());
        request
            .headers_mut()
            .insert("Cookie", HeaderValue::from_str(&cookie).unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let message = ws.next().await.unwrap().unwrap();
        let frame: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        assert_eq!(
            STANDARD
                .decode(frame["frame"]["b64"].as_str().unwrap())
                .unwrap(),
            b"prompt"
        );
        ws.send(WsMessage::Text(
            serde_json::to_string(&LocalTerminalInput::Input {
                b64: STANDARD.encode(b"hello\r\x1b\x03"),
            })
            .unwrap(),
        ))
        .await
        .unwrap();
        ws.send(WsMessage::Text(
            serde_json::to_string(&LocalTerminalInput::Resize {
                cols: 120,
                rows: 40,
            })
            .unwrap(),
        ))
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), host)
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
                .await
                .is_ok()
        );
    }
    #[tokio::test]
    async fn shared_spa_owner_api_lists_pages_and_never_serves_html_for_api_errors() {
        let f = fixture().await;
        let client = reqwest::Client::new();
        let marker: Value = client
            .get(format!("{}/api/local", f.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(marker, json!({"local":true,"authenticated":false}));
        assert_eq!(
            client
                .get(format!("{}/api/local/sessions", f.base))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let cookie = cookie(&f).await;
        let mut store = Store::open(&f.state.dir).unwrap();
        for suffix in [1, 2] {
            store
                .insert_session(
                    &super::super::authz::sessions::LocalSession {
                        session_id: format!("00000000-0000-0000-0000-{suffix:012}"),
                        agent: conversations_core::AgentKind::Codex,
                        cwd: "/work/project".into(),
                        resume_id: None,
                        launcher: Default::default(),
                        adk_statusline: false,
                    },
                    "adopted",
                )
                .unwrap();
        }
        let first: Value = client
            .get(format!("{}/api/local/sessions?limit=1", f.base))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(first["kind"], "sessions");
        assert_eq!(first["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(first["sessions"][0]["running"], false);
        let mut next = reqwest::Url::parse(&format!("{}/api/local/sessions", f.base)).unwrap();
        next.query_pairs_mut()
            .append_pair("after", first["next"].as_str().unwrap())
            .append_pair("limit", "1");
        let second: Value = client
            .get(next)
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(first["sessions"][0]["id"], second["sessions"][0]["id"]);
        assert!(second["next"].is_null());
        let history: Value = client
            .get(format!(
                "{}/api/local/sessions/{}/history?digest=true",
                f.base,
                first["sessions"][0]["id"].as_str().unwrap()
            ))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(history, json!({"kind":"history","items":[],"next":null}));
        let active: Value = client
            .get(format!("{}/api/local/sessions?running_only=true", f.base))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(active, json!({"kind":"sessions","sessions":[],"next":null}));
        let missing = client
            .get(format!("{}/api/unknown", f.base))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
        assert!(missing.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("application/json"));
        if !SPA_ASSETS.is_empty() {
            let page = client
                .get(format!("{}/private/example", f.base))
                .send()
                .await
                .unwrap();
            assert_eq!(page.status(), 200);
            assert_eq!(
                page.bytes().await.unwrap().as_ref(),
                SPA_ASSETS
                    .iter()
                    .find(|(name, _)| *name == "/index.html")
                    .unwrap()
                    .1
            );
        }
    }
    #[tokio::test]
    async fn shared_spa_start_reopen_and_reply_stay_on_owner_socket() {
        use super::super::protocol::LocalAction;
        let f = fixture().await;
        let cookie = cookie(&f).await;
        let listener = tokio::net::UnixListener::bind(f.state.dir.join("host.sock")).unwrap();
        let host = tokio::spawn(async move {
            for expected in ["start", "reopen", "reply", "end"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let message: ClientMessage = read_message(&mut socket).await.unwrap();
                let action = match message {
                    ClientMessage::LocalAction { action } => action,
                    _ => panic!("not an owner action"),
                };
                match (expected, action) {
                    (
                        "start",
                        LocalAction::Start {
                            agent: conversations_core::AgentKind::Codex,
                            ..
                        },
                    ) => {}
                    ("reopen", LocalAction::Reopen { .. }) => {}
                    ("end", LocalAction::End { .. }) => {}
                    ("reply", LocalAction::Reply { text, .. }) => assert_eq!(text, "hello"),
                    _ => panic!("wrong action"),
                }
                write_message(&mut socket, &ServerMessage::Started { id: "test".into() })
                    .await
                    .unwrap();
            }
        });
        let id = "00000000-0000-0000-0000-000000000001";
        for (path, body) in [
            (
                "/api/local/start".to_owned(),
                json!({"session":id,"agent":"codex"}),
            ),
            (format!("/api/local/sessions/{id}/reopen"), json!({})),
            (
                format!("/api/local/sessions/{id}/reply"),
                json!({"text":"hello"}),
            ),
            (format!("/api/local/sessions/{id}/end"), json!({})),
        ] {
            let response: Value = reqwest::Client::new()
                .post(format!("{}{path}", f.base))
                .header("Cookie", &cookie)
                .header("Origin", &f.base)
                .json(&body)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(response["status"], "executed");
        }
        host.await.unwrap();
    }
    #[tokio::test]
    async fn local_preferences_persist_without_changing_launch_settings() {
        if !super::super::test_support::isolated("commands::local_host::web::tests::local_preferences_persist_without_changing_launch_settings", &[]) {return;}
        let f = fixture().await;
        let cookie = cookie(&f).await;
        let mut settings = super::super::settings::Settings::default();
        settings.launch.claude_skip_permissions = true;
        settings.save().unwrap();
        let prefs = json!({"container_paths":["/work"],"default_agent":"codex","inline_code_colors":{"palette":[1,7],"rgb":[1193046]}});
        let response = reqwest::Client::new()
            .put(format!("{}/api/local/preferences", f.base))
            .header("Cookie", &cookie)
            .header("Origin", &f.base)
            .json(&prefs)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let read: Value = reqwest::Client::new()
            .get(format!("{}/api/local/preferences", f.base))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(read, prefs);
        let saved = super::super::settings::Settings::load(&f.state.dir).unwrap();
        assert!(saved.launch.claude_skip_permissions);
        assert_eq!(saved.ui.default_agent, conversations_core::AgentKind::Codex);
    }
    #[tokio::test]
    async fn owner_communication_endpoint_requires_cookie_and_known_session() {
        let f = fixture().await;
        let client = reqwest::Client::new();
        let cookie = cookie(&f).await;
        let id = uuid::Uuid::new_v4().to_string();
        Store::open(&f.state.dir)
            .unwrap()
            .insert_session(
                &super::super::authz::sessions::LocalSession {
                    session_id: id.clone(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/fixture".into(),
                    resume_id: None,
                    launcher: Default::default(),
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        let url = format!("{}/api/local/sessions/{id}/communications?limit=32", f.base);
        assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
        let response = client
            .get(&url)
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let value: Value = response.json().await.unwrap();
        assert_eq!(value["kind"], "communications");
        assert_eq!(value["exchanges"], json!([]));
        let missing = client
            .get(format!(
                "{}/api/local/sessions/{}/communications",
                f.base,
                uuid::Uuid::new_v4()
            ))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 400);
    }

    #[tokio::test]
    async fn owner_attention_is_durable_idempotent_and_never_enters_the_agent_inbox() {
        let f = fixture().await;
        let client = reqwest::Client::new();
        let cookie = cookie(&f).await;
        let id = uuid::Uuid::new_v4().to_string();
        let mut store = Store::open(&f.state.dir).unwrap();
        store
            .insert_session(
                &super::super::authz::sessions::LocalSession {
                    session_id: id.clone(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/tmp".into(),
                    resume_id: None,
                    launcher: Default::default(),
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        let kid = store
            .enroll(
                authz::store::Kind::Owner,
                Alg::Ed25519,
                &[49; 32],
                "Alex",
                &[authz::store::Grant::parse(&format!("history.read:{id}")).unwrap()],
            )
            .unwrap();
        let url = format!("{}/api/local/sessions/{id}/attention", f.base);
        let request = json!({"recipient":{"kind":"credential","kid":kid},"message":"Please inspect the result","idempotency_key":uuid::Uuid::new_v4().to_string()});
        assert_eq!(
            client
                .post(&url)
                .header("Origin", &f.base)
                .json(&request)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .post(&url)
                .header("Cookie", &cookie)
                .header("Origin", "https://evil.example")
                .json(&request)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            client
                .get(format!("{}/api/local/notifications", f.base))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        for _ in 0..2 {
            let response = client
                .post(&url)
                .header("Cookie", &cookie)
                .header("Origin", &f.base)
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.json::<Value>().await.unwrap()["status"],
                "executed"
            );
        }
        let conn = rusqlite::Connection::open(f.state.dir.join("authz.db")).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM human_notifications", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM agent_inbox", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let mut changed = request.clone();
        changed["message"] = json!("Different body");
        let result: Value = client
            .post(&url)
            .header("Cookie", &cookie)
            .header("Origin", &f.base)
            .json(&changed)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["status"], "refused");
    }

    #[tokio::test]
    async fn owner_pr_cache_reads_and_unlinks_without_remote_requests() {
        use super::super::{authz::sessions::LocalSession, pr_metadata};
        let f = fixture().await;
        let id = uuid::Uuid::new_v4().to_string();
        Store::open(&f.state.dir)
            .unwrap()
            .insert_session(
                &LocalSession {
                    session_id: id.clone(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/tmp".into(),
                    resume_id: None,
                    launcher: Default::default(),
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        let status = crate::commands::gh::PrStatus {
            state: "OPEN".into(),
            merged: false,
            draft: false,
            title: "Local cached PR".into(),
            review_decision: None,
        };
        pr_metadata::save(
            &f.state.dir,
            &id,
            "octo",
            "repo",
            7,
            Some(&status),
            &[],
            true,
        )
        .unwrap();
        let client = reqwest::Client::new();
        let cookie = cookie(&f).await;
        let url = format!("{}/api/local/sessions/{id}/prs", f.base);
        assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
        let value: Value = client
            .get(&url)
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["kind"], "pr_cache");
        let body: Value =
            serde_json::from_slice(&STANDARD.decode(value["b64"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(body["prs"][0]["status"]["title"], "Local cached PR");
        let target = json!({"owner":"octo","repo":"repo","number":7});
        assert_eq!(
            client
                .post(format!("{url}/sync"))
                .header("Cookie", &cookie)
                .header("Origin", "https://evil.example")
                .json(&target)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let value: Value = client
            .post(format!("{url}/unlink"))
            .header("Cookie", &cookie)
            .header("Origin", &f.base)
            .json(&target)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["status"], "executed");
        let value: Value = client
            .get(&url)
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(
                &STANDARD.decode(value["b64"].as_str().unwrap()).unwrap()
            )
            .unwrap(),
            json!({"prs":[],"comments":[]})
        );
    }

    #[tokio::test]
    async fn owner_access_keeps_publication_explicit_and_grants_within_ceiling() {
        use super::super::authz::{
            sessions::LocalSession,
            store::{Grant, Kind},
        };
        use conversations_core::{AgentKind, AgentLauncher};
        let f = fixture().await;
        let client = reqwest::Client::new();
        let cookie = cookie(&f).await;
        let id = uuid::Uuid::new_v4().to_string();
        let mut store = Store::open(&f.state.dir).unwrap();
        store
            .insert_session(
                &LocalSession {
                    session_id: id.clone(),
                    agent: AgentKind::Claude,
                    cwd: "/tmp".into(),
                    resume_id: Some(id.clone()),
                    launcher: AgentLauncher::default(),
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        let kid = store
            .enroll(
                Kind::Owner,
                Alg::Ed25519,
                &[42; 32],
                "Reader",
                &[Grant::parse("ceiling:history.read").unwrap()],
            )
            .unwrap();
        let base = format!("{}/api/local/sessions/{id}", f.base);
        let response = client
            .get(format!("{base}/publication"))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let value: Value = response.json().await.unwrap();
        let detail: Value = serde_json::from_str(value["detail"].as_str().unwrap()).unwrap();
        assert_eq!(detail["state"], "private");
        for (cap, expected) in [
            ("history.read", "executed"),
            ("session.write", "refused"),
            ("session.manage_sharing", "refused"),
        ] {
            let value: Value = client
                .post(format!("{base}/grants/add"))
                .header("Cookie", &cookie)
                .header("Origin", &f.base)
                .json(&json!({"recipient":kid,"caps":[cap]}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(value["status"], expected, "{value}");
        }
        let host_reader = store
            .enroll(
                Kind::Owner,
                Alg::Ed25519,
                &[43; 32],
                "Host reader",
                &[Grant::parse("history.read").unwrap()],
            )
            .unwrap();
        let value: Value = client
            .post(format!("{base}/grants/remove"))
            .header("Cookie", &cookie)
            .header("Origin", &f.base)
            .json(&json!({"recipient":host_reader,"caps":["history.read"]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["status"], "refused");
        assert!(value["detail"].as_str().unwrap().contains("host-wide"));
        let grants: Value = client
            .get(format!("{base}/grants"))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let reader = grants["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["kid"] == kid)
            .unwrap();
        assert_eq!(
            reader["grants"],
            json!([{"cap":"history.read","session":id}])
        );
        assert!(reader.get("public_key").is_none());
        let request = || {
            client
                .post(format!("{base}/grants/remove"))
                .json(&json!({"recipient":kid,"caps":["history.read"]}))
        };
        assert_eq!(
            request()
                .header("Origin", &f.base)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            request()
                .header("Cookie", &cookie)
                .header("Origin", "https://evil.example")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let value: Value = request()
            .header("Cookie", &cookie)
            .header("Origin", &f.base)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["status"], "executed");
        assert!(value["detail"]
            .as_str()
            .unwrap()
            .contains("interruption unavailable"));
        let mut history = super::super::history::History::open(&f.state.dir).unwrap();
        let message = conversations_core::parse_line(&json!({"type":"user","uuid":"local-test-turn","sessionId":id,
            "timestamp":"2026-11-02T10:00:00Z","message":{"role":"user","content":"Isolated publication fixture"}})).unwrap();
        history
            .ingest(
                &id,
                &super::super::history::Source {
                    agent: AgentKind::Claude,
                    native_id: id.clone(),
                    origin: "fixture".into(),
                },
                &[message],
            )
            .unwrap();
        for (path, body, phase) in [
            (
                "share",
                json!({"audience":{"visibility":"restricted","emails":["reader@example.com"]}}),
                "syncing",
            ),
            ("unshare", json!({}), "deletion_pending"),
        ] {
            let value: Value = client
                .post(format!("{base}/publication/{path}"))
                .header("Cookie", &cookie)
                .header("Origin", &f.base)
                .json(&body)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(value["status"], "executed", "{value}");
            assert_eq!(value["detail"], phase);
        }
        assert_eq!(history.messages(&id, 10).unwrap().len(), 1);
        assert_eq!(
            super::super::publication::status(&f.state.dir, &id)
                .unwrap()
                .state,
            super::super::publication::State::DeletionPending
        );
    }
    #[tokio::test]
    async fn local_upload_requires_owner_and_forwards_exact_unicode_files_with_receipt() {
        let f = fixture().await;
        let client = reqwest::Client::new();
        let id = "00000000-0000-0000-0000-000000000001";
        let url = format!("{}/api/local/sessions/{id}/transfer", f.base);
        let body = json!({"transfer":"0123456789abcdef0123456789abcdef","content":{"kind":"files","files":[{"name":"Résumé.txt","b64":STANDARD.encode(vec![b'x';256*1024])}]}});
        assert_eq!(
            client
                .post(&url)
                .header("Origin", &f.base)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let cookie = cookie(&f).await;
        assert_eq!(
            client
                .post(&url)
                .header("Cookie", &cookie)
                .header("Origin", "https://evil.example")
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let listener = tokio::net::UnixListener::bind(f.state.dir.join("host.sock")).unwrap();
        let host = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request: Value = read_message(&mut socket).await.unwrap();
            assert_eq!(request["type"], "local_transfer");
            assert_eq!(request["session"], id);
            assert_eq!(request["content"]["files"][0]["name"], "Résumé.txt");
            let bytes = STANDARD
                .decode(request["content"]["files"][0]["b64"].as_str().unwrap())
                .unwrap();
            assert_eq!(bytes.len(), 256 * 1024);
            assert!(bytes.iter().all(|b| *b == b'x'));
            write_message(
                &mut socket,
                &json!({"type":"local_transfer_result","status":"delivered"}),
            )
            .await
            .unwrap();
        });
        let response = client
            .post(&url)
            .header("Cookie", &cookie)
            .header("Origin", &f.base)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"status":"delivered"})
        );
        host.await.unwrap();
    }
}
