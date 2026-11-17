//! Publication of locally canonical history, observed end to end: the host's
//! own `history.db` (written by the same `History::ingest` the tailers use),
//! the local publication state and outbox, the real publisher talking HTTP to
//! the REAL central router (auth on, SQLite) through a loopback proxy that can
//! go offline or lose a response, and the host's signed-operation dispatcher.
//! Every assertion reads what a central reader actually sees.
//! Signed controls enter the already-decrypted dispatcher seam; actual private
//! networking is covered separately by the Chromium transport/app journeys.

#[path = "publication_tests/pr_cache_journey.rs"]
mod pr_cache_journey;

use super::*;
use crate::commands::local_host::authz::{
    self,
    envelope::test_signing::Signer,
    store::{now_secs, Store},
    SignedFrame,
};
use crate::commands::local_host::history::{History, Source};
use crate::commands::local_host::publication::{self, Audience, Destination, State};
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64U};
use conversations_core::host_relay::OpStatus;
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ALICE: &str = "alice@x.test";
const BOB: &str = "bob@x.test";
const CAROL: &str = "carol@x.test";
const EVE: &str = "eve@x.test";
const S: &str = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";

/// Re-run `name` in a child with an isolated HOME, config and settings file.
fn isolated(name: &str) -> bool {
    crate::commands::local_host::test_support::isolated(
        &format!("commands::local_host::daemon::publication_tests::{name}"),
        &[],
    )
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

// ---------------------------------------------------------------- central ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Pass,
    /// Accept and close: the host sees a transport failure, nothing arrives.
    Offline,
}

/// Loopback proxy in front of the real server. It records every publication
/// request body and can lose the answer to the N-th delivered publication
/// request (the server applied it; the host never hears back).
type AnswerHook = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[derive(Clone)]
struct Proxy {
    url: String,
    mode: Arc<Mutex<Mode>>,
    lose_at: Arc<Mutex<Option<usize>>>,
    log: Arc<Mutex<Vec<Vec<u8>>>>,
    delivered: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Hold the answer to the N-th delivered publication request (the server
    /// applied it) until `release`: the host is still waiting for it.
    hold_at: Arc<Mutex<Option<usize>>>,
    holding: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    /// Runs with each delivered publication request body after the server
    /// answered it and before the host hears the answer.
    on_answer: Arc<Mutex<Option<AnswerHook>>>,
    /// Request line of every request of any kind, in any mode.
    requests: Arc<Mutex<Vec<String>>>,
}

impl Proxy {
    async fn start(upstream: String) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = Proxy {
            url: format!("http://{}", listener.local_addr().unwrap()),
            mode: Arc::new(Mutex::new(Mode::Pass)),
            lose_at: Arc::new(Mutex::new(None)),
            log: Arc::default(),
            delivered: Arc::default(),
            hold_at: Arc::default(),
            holding: Arc::default(),
            release: Arc::default(),
            on_answer: Arc::default(),
            requests: Arc::default(),
        };
        let upstream = upstream.trim_start_matches("http://").to_owned();
        let p = proxy.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let (p, upstream) = (p.clone(), upstream.clone());
                tokio::spawn(async move {
                    let _ = p.serve(socket, &upstream).await;
                });
            }
        });
        proxy
    }

    fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn lose_response_of(&self, index: usize) {
        *self.lose_at.lock().unwrap() = Some(index);
    }

    fn hold_response_of(&self, index: usize) {
        *self.hold_at.lock().unwrap() = Some(index);
    }

    /// Bodies of publication requests the proxy received (in any mode).
    fn attempts(&self) -> Vec<Vec<u8>> {
        self.log.lock().unwrap().clone()
    }

    /// Bodies the real server actually received.
    fn delivered(&self) -> Vec<Vec<u8>> {
        self.delivered.lock().unwrap().clone()
    }

    async fn serve(&self, mut socket: tokio::net::TcpStream, upstream: &str) -> Result<()> {
        loop {
            let Some((head, body)) = read_http(&mut socket).await? else {
                return Ok(());
            };
            let publication = head.starts_with(b"POST /api/publications ");
            self.requests.lock().unwrap().push(
                String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            );
            if publication {
                self.log.lock().unwrap().push(body.clone());
            }
            if *self.mode.lock().unwrap() == Mode::Offline {
                return Ok(());
            }
            let mut up = tokio::net::TcpStream::connect(upstream).await?;
            up.write_all(&head).await?;
            up.write_all(&body).await?;
            let (rhead, rbody) = read_http(&mut up)
                .await?
                .context("upstream closed without answering")?;
            if publication {
                let index = {
                    let mut delivered = self.delivered.lock().unwrap();
                    delivered.push(body.clone());
                    delivered.len() - 1
                };
                {
                    let mut lose_at = self.lose_at.lock().unwrap();
                    if *lose_at == Some(index) {
                        *lose_at = None;
                        return Ok(());
                    }
                }
                let hook = self.on_answer.lock().unwrap().clone();
                if let Some(hook) = hook {
                    hook(&body);
                }
                let hold = self.hold_at.lock().unwrap().take_if(|at| *at == index);
                if hold.is_some() {
                    self.holding.notify_one();
                    self.release.notified().await;
                }
            }
            socket.write_all(&rhead).await?;
            socket.write_all(&rbody).await?;
        }
    }
}

/// One HTTP/1.1 message with a Content-Length body (both directions here).
async fn read_http(socket: &mut tokio::net::TcpStream) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        if socket.read(&mut byte).await? == 0 {
            return Ok(None);
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let length = text
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map(|v| v.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    socket.read_exact(&mut body).await?;
    Ok(Some((head, body)))
}

struct Central {
    url: String,
    proxy: Proxy,
    http: reqwest::Client,
    cookies: HashMap<&'static str, String>,
    _db: tempfile::TempDir,
}

impl Central {
    async fn start() -> Self {
        Self::start_with(true).await
    }

    /// The real router; with `auth` off it has no accounts and anyone reads.
    async fn start_with(auth: bool) -> Self {
        let db = tempfile::tempdir().unwrap();
        let repo = conversations_server::SqliteRepo::connect(&format!(
            "sqlite://{}?mode=rwc",
            db.path().join("central.sqlite").display()
        ))
        .await
        .unwrap();
        let config = conversations_server::Config {
            auth_enabled: auth,
            password_auth_enabled: auth,
            cookie_secure: false,
            ..conversations_server::Config::default()
        };
        let state = conversations_server::AppState::new(Arc::new(repo), Arc::new(config));
        // As the server's `main` composes it for each mode.
        let app = if auth {
            conversations_server::build_router_with_auth(state)
                .await
                .unwrap()
        } else {
            conversations_server::build_router(state)
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let mut cookies = HashMap::new();
        for email in [ALICE, BOB, CAROL, EVE].into_iter().filter(|_| auth) {
            let response = http
                .post(format!("{url}/auth/register"))
                .json(&json!({"email": email, "password": "test-password-123", "display_name": email}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 201);
            let cookie = response.headers()["set-cookie"]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            cookies.insert(email, cookie);
        }
        let proxy = Proxy::start(url.clone()).await;
        Central {
            url,
            proxy,
            http,
            cookies,
            _db: db,
        }
    }

    /// The URL the host publishes to (through the proxy).
    fn server(&self) -> &str {
        &self.proxy.url
    }

    async fn get(&self, who: &str, path: &str) -> (u16, Value) {
        let response = self
            .http
            .get(format!("{}{path}", self.url))
            .header("cookie", self.cookies.get(who).map_or("", String::as_str))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, who: &str, path: &str, body: Vec<u8>) -> (u16, Value) {
        let response = self
            .http
            .post(format!("{}{path}", self.url))
            .header("cookie", self.cookies.get(who).map_or("", String::as_str))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// Message texts `who` reads for `session`, or `None` when it cannot read it.
    async fn texts(&self, who: &str, session: &str) -> Option<Vec<String>> {
        let (status, body) = self.get(who, &format!("/api/sessions/{session}")).await;
        (status == 200).then(|| {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["text"].as_str().unwrap().to_owned())
                .collect()
        })
    }

    async fn search(&self, who: &str, term: &str) -> Vec<String> {
        let (status, body) = self.get(who, &format!("/api/sessions?q={term}")).await;
        assert_eq!(status, 200, "{body}");
        body["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect()
    }

    async fn prs(&self, who: &str, session: &str) -> String {
        self.get(who, &format!("/api/sessions/{session}/prs"))
            .await
            .1
            .to_string()
    }

    fn destination<'a>(&'a self, who: &str) -> Destination<'a> {
        Destination {
            server: self.server(),
            client: &self.http,
            cookie: self.cookies.get(who).map(String::as_str),
        }
    }
}

// ------------------------------------------------------------------- host ----

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn source() -> Source {
    Source {
        agent: conversations_core::AgentKind::Claude,
        native_id: "native-1".into(),
        origin: "/projects/x/native-1.jsonl".into(),
    }
}

fn user(uuid: &str, text: &str) -> conversations_core::Message {
    conversations_core::parse_line(&json!({"type":"user","uuid":uuid,"sessionId":"s",
        "timestamp":"2026-11-02T10:00:00.000Z","message":{"role":"user","content":text}}))
    .unwrap()
}

/// What the agent's own transcript tailer does: persist into `history.db`.
fn ingest(dir: &Path, session: &str, messages: &[conversations_core::Message]) {
    History::open(dir)
        .unwrap()
        .ingest(session, &source(), messages)
        .unwrap();
}

fn local_count(dir: &Path, session: &str) -> usize {
    History::open(dir)
        .unwrap()
        .messages(session, 100_000)
        .unwrap()
        .len()
}

fn org() -> Audience {
    Audience::Organization {}
}

fn people(emails: &[&str]) -> Audience {
    Audience::Restricted {
        emails: emails.iter().map(|e| e.to_string()).collect(),
        write_pending: Vec::new(),
    }
}

/// Run the publisher until it has nothing left to send (or the network fails).
async fn pump(dir: &Path, central: &Central, who: &str) -> publication::Pumped {
    publication::pump(dir, &central.destination(who))
        .await
        .unwrap()
}

fn state(dir: &Path, session: &str) -> State {
    publication::status(dir, session).unwrap().state
}

// ----------------------------------------------------------------- proofs ----

#[test]
fn private_history_is_absent_until_shared_then_past_future_and_updates_publish() {
    if !isolated("private_history_is_absent_until_shared_then_past_future_and_updates_publish") {
        return;
    }
    runtime().block_on(async {
        let central = Central::start().await;
        let host = private_dir();
        let dir = host.path();
        ingest(
            dir,
            S,
            &[
                user("m1", "private-marker one"),
                user(
                    "m2",
                    "draft-needle see https://github.com/acme/widgets/pull/42",
                ),
                user("m3", "third"),
            ],
        );
        // Ordinary tailing and running the publisher publish nothing.
        assert_eq!(pump(dir, &central, ALICE).await.sent, 0);
        assert_eq!(state(dir, S), State::Private);
        assert!(central.proxy.attempts().is_empty());
        assert_eq!(central.texts(ALICE, S).await, None);
        assert!(central.search(ALICE, "private-marker").await.is_empty());

        // Explicit share to Bob: past history arrives in order; Eve sees nothing.
        let shared =
            publication::share(dir, S, people(&[BOB]), Some(ALICE), central.server()).unwrap();
        assert_eq!(shared.state, State::Syncing);
        pump(dir, &central, ALICE).await;
        assert_eq!(state(dir, S), State::Shared);
        assert_eq!(
            central.texts(BOB, S).await.unwrap(),
            vec![
                "private-marker one",
                "draft-needle see https://github.com/acme/widgets/pull/42",
                "third"
            ]
        );
        assert_eq!(central.search(BOB, "private-marker").await, vec![S]);
        assert!(central.prs(BOB, S).await.contains("pull/42"));
        assert_eq!(central.texts(EVE, S).await, None);
        assert!(central.search(EVE, "private-marker").await.is_empty());

        // Future history and a same-UUID update follow the share.
        ingest(
            dir,
            S,
            &[
                user(
                    "m2",
                    "updated-needle see https://github.com/acme/widgets/pull/43",
                ),
                user("m4", "fourth"),
            ],
        );
        assert_eq!(state(dir, S), State::Syncing);
        pump(dir, &central, ALICE).await;
        assert_eq!(state(dir, S), State::Shared);
        assert_eq!(
            central.texts(BOB, S).await.unwrap(),
            vec![
                "private-marker one",
                "updated-needle see https://github.com/acme/widgets/pull/43",
                "third",
                "fourth"
            ]
        );
        assert_eq!(central.search(BOB, "updated-needle").await, vec![S]);
        assert!(central.search(BOB, "draft-needle").await.is_empty());
        let prs = central.prs(BOB, S).await;
        assert!(prs.contains("pull/43") && !prs.contains("pull/42"), "{prs}");

        // The host goes offline: the named reader still reads.
        central.proxy.set(Mode::Offline);
        assert_eq!(central.texts(BOB, S).await.unwrap().len(), 4);

        // A new organization audience (a newer revision) reaches any member.
        central.proxy.set(Mode::Pass);
        publication::share(dir, S, org(), Some(ALICE), central.server()).unwrap();
        pump(dir, &central, ALICE).await;
        assert_eq!(state(dir, S), State::Shared);
        assert_eq!(central.texts(CAROL, S).await.unwrap().len(), 4);
        central.proxy.set(Mode::Offline);
        assert_eq!(central.texts(EVE, S).await.unwrap().len(), 4);
        assert_eq!(local_count(dir, S), 4);
    });
}

#[test]
fn lost_responses_and_restarts_resend_identical_bytes_without_holes() {
    if !isolated("lost_responses_and_restarts_resend_identical_bytes_without_holes") {
        return;
    }
    runtime().block_on(async {
        let central = Central::start().await;
        let host = private_dir();
        let dir = host.path();
        // More than two chunks' worth (500 messages per chunk).
        let first: Vec<_> = (0..1100)
            .map(|i| user(&format!("m{i:05}"), &format!("message {i}")))
            .collect();
        ingest(dir, S, &first);
        publication::share(dir, S, people(&[BOB]), Some(ALICE), central.server()).unwrap();

        // Share (0) and chunk 0 are answered; chunk 1 reaches the server but
        // its answer is lost.
        central.proxy.lose_response_of(2);
        let attempt = pump(dir, &central, ALICE).await;
        assert_eq!(
            attempt,
            publication::Pumped {
                busy: false,
                sent: 2,
                transport_errors: 1
            }
        );
        let lost = central.proxy.delivered()[2].clone();
        assert_eq!(state(dir, S), State::Syncing);

        // History keeps changing while the snapshot is paged: an update to a
        // published message, one to a message inside the frozen (lost) chunk,
        // and a new message at the end.
        ingest(
            dir,
            S,
            &[
                user("m00003", "message 3 edited"),
                user("m00700", "message 700 edited"),
                user("m99999", "tail"),
            ],
        );

        // A restart: nothing in memory, a new client; progress comes from disk.
        let restarted = Destination {
            server: central.server(),
            client: &reqwest::Client::new(),
            cookie: Some(&central.cookies[ALICE]),
        };
        publication::pump(dir, &restarted).await.unwrap();
        let attempts = central.proxy.attempts();
        let index = attempts.iter().position(|b| b == &lost).unwrap();
        assert_eq!(
            attempts[index + 1],
            lost,
            "the retry was not byte-identical"
        );
        assert_eq!(state(dir, S), State::Shared);

        let texts = central.texts(BOB, S).await.unwrap();
        let mut expected: Vec<String> = (0..1100).map(|i| format!("message {i}")).collect();
        expected[3] = "message 3 edited".into();
        expected[700] = "message 700 edited".into();
        expected.push("tail".into());
        assert_eq!(
            texts, expected,
            "projection has holes, duplicates or reordering"
        );

        // The server still refuses altered duplicates, bad digests and gaps.
        let request: Value = serde_json::from_slice(&lost).unwrap();
        let op: Value =
            serde_json::from_slice(&B64U.decode(request["op"].as_str().unwrap()).unwrap()).unwrap();
        if op["action"]["type"] == "append" {
            let mut store = Store::open(dir).unwrap();
            let sign = |store: &mut Store, op: &Value, chunk: Option<&[u8]>| {
                let op_bytes = serde_json::to_vec(op).unwrap();
                let sig = store
                    .host_sign(&conversations_core::publication::signed_message(&op_bytes))
                    .unwrap();
                let mut body = json!({
                    "host_key": B64U.encode(store.host_public_key().unwrap()),
                    "op": B64U.encode(&op_bytes),
                    "sig": B64U.encode(sig),
                });
                if let Some(chunk) = chunk {
                    body["chunk"] = json!(B64U.encode(chunk));
                }
                serde_json::to_vec(&body).unwrap()
            };
            let altered = br#"{"messages":[]}"#.to_vec();
            let digest = B64U.encode(ring::digest::digest(&ring::digest::SHA256, &altered));
            let mut altered_op = op.clone();
            altered_op["action"]["chunk_sha256"] = json!(digest);
            let (status, _) = central
                .post(
                    ALICE,
                    "/api/publications",
                    sign(&mut store, &altered_op, Some(&altered)),
                )
                .await;
            assert!(
                status == 400 || status == 409,
                "altered duplicate: {status}"
            );
            let (status, _) = central
                .post(
                    ALICE,
                    "/api/publications",
                    sign(&mut store, &op, Some(b"{\"messages\":[]}x")),
                )
                .await;
            assert_eq!(status, 400, "digest mismatch");
            let mut gap = op.clone();
            gap["action"]["seq"] = json!(op["action"]["seq"].as_u64().unwrap() + 50);
            let (status, _) = central
                .post(
                    ALICE,
                    "/api/publications",
                    sign(&mut store, &gap, Some(&altered)),
                )
                .await;
            assert!(status == 400 || status == 409, "sequence gap: {status}");
        }
        assert_eq!(central.texts(BOB, S).await.unwrap(), expected);
    });
}

#[test]
fn offline_unshare_is_pending_then_deletes_and_old_uploads_cannot_resurrect() {
    if !isolated("offline_unshare_is_pending_then_deletes_and_old_uploads_cannot_resurrect") {
        return;
    }
    runtime().block_on(async {
        let central = Central::start().await;
        let host = private_dir();
        let dir = host.path();
        ingest(
            dir,
            S,
            &[
                user("m1", "secret-needle https://github.com/acme/widgets/pull/7"),
                user("m2", "two"),
            ],
        );
        publication::share(dir, S, people(&[BOB]), Some(ALICE), central.server()).unwrap();
        pump(dir, &central, ALICE).await;
        assert_eq!(central.texts(BOB, S).await.unwrap().len(), 2);
        let old_uploads = central.proxy.delivered();

        // Offline unshare: pending, and nothing more is uploaded.
        central.proxy.set(Mode::Offline);
        let pending = publication::unshare(dir, S).unwrap();
        assert_eq!(pending.state, State::DeletionPending);
        ingest(dir, S, &[user("m3", "written while pending")]);
        let before = central.proxy.attempts().len();
        let attempt = pump(dir, &central, ALICE).await;
        assert!(attempt.transport_errors > 0);
        for body in &central.proxy.attempts()[before..] {
            let request: Value = serde_json::from_slice(body).unwrap();
            let op: Value =
                serde_json::from_slice(&B64U.decode(request["op"].as_str().unwrap()).unwrap())
                    .unwrap();
            assert_eq!(
                op["action"]["type"], "unshare",
                "uploaded while unsharing: {op}"
            );
        }
        assert_eq!(state(dir, S), State::DeletionPending);

        // Reconnect: the tombstone lands; content, search and links are gone.
        central.proxy.set(Mode::Pass);
        pump(dir, &central, ALICE).await;
        assert_eq!(state(dir, S), State::Private);
        assert_eq!(central.texts(BOB, S).await, None);
        assert_eq!(central.texts(ALICE, S).await, None);
        assert!(central.search(BOB, "secret-needle").await.is_empty());
        assert!(central.search(ALICE, "secret-needle").await.is_empty());
        assert!(!central.prs(ALICE, S).await.contains("pull/7"));

        // Old queued uploads (share and chunks) cannot bring it back.
        for body in old_uploads {
            let (status, _) = central.post(ALICE, "/api/publications", body).await;
            assert_eq!(status, 409, "an old upload was accepted after unshare");
        }
        assert_eq!(central.texts(BOB, S).await, None);
        assert!(central.search(BOB, "secret-needle").await.is_empty());

        // Local history survived; re-sharing at a newer revision republishes it all.
        assert_eq!(local_count(dir, S), 3);
        let again =
            publication::share(dir, S, people(&[BOB]), Some(ALICE), central.server()).unwrap();
        assert!(again.rev > pending.rev);
        pump(dir, &central, ALICE).await;
        assert_eq!(state(dir, S), State::Shared);
        assert_eq!(
            central.texts(BOB, S).await.unwrap(),
            vec![
                "secret-needle https://github.com/acme/widgets/pull/7",
                "two",
                "written while pending"
            ]
        );
    });
}

#[test]
fn existing_unmanaged_central_history_is_never_adopted_or_deleted() {
    if !isolated("existing_unmanaged_central_history_is_never_adopted_or_deleted") {
        return;
    }
    runtime().block_on(async {
        let central = Central::start().await;
        let host = private_dir();
        let dir = host.path();
        // Alice's legacy upload under the same id, shared with nobody.
        let line = json!({"type": "user", "sessionId": S, "uuid": "l1",
            "message": {"role": "user", "content": "legacy-needle original"}});
        let legacy = json!({
            "uploaded_by": ALICE, "source_host": null,
            "session": conversations_core::parse_session_str(None, &line.to_string()).unwrap()
        });
        let (status, body) = central
            .post(ALICE, "/api/sessions", serde_json::to_vec(&legacy).unwrap())
            .await;
        assert!(status == 200 || status == 201, "{status} {body}");
        let (status, before) = central.get(ALICE, &format!("/api/sessions/{S}")).await;
        assert_eq!(status, 200);
        ingest(dir, S, &[user("m1", "local-only-needle")]);

        publication::share(dir, S, org(), Some(ALICE), central.server()).unwrap();
        pump(dir, &central, ALICE).await;
        let shared = publication::status(dir, S).unwrap();
        assert_eq!(shared.state, State::Error, "{shared:?}");
        assert!(shared.error.is_some());
        publication::unshare(dir, S).unwrap();
        pump(dir, &central, ALICE).await;
        // The rejected share never claimed this id. Unshare acknowledges that
        // there is no managed publication to delete, leaving legacy history alone.
        let unshared = publication::status(dir, S).unwrap();
        assert_eq!(unshared.state, State::Private, "{unshared:?}");
        assert!(unshared.error.is_none());

        let (status, after) = central.get(ALICE, &format!("/api/sessions/{S}")).await;
        assert_eq!(status, 200, "unshare deleted the unmanaged session");
        assert_eq!(
            central.texts(ALICE, S).await,
            Some(vec!["legacy-needle original".to_owned()])
        );
        assert_eq!(after["messages"], before["messages"]);
        assert_eq!(after["visibility"], before["visibility"]);
        assert_eq!(after["title"], before["title"]);
        assert_eq!(central.texts(BOB, S).await, None);
        assert!(central.search(ALICE, "local-only-needle").await.is_empty());
        assert_eq!(local_count(dir, S), 1);
    });
}

#[test]
fn the_owner_is_bound_locally_and_another_accounts_login_publishes_nothing() {
    if !isolated("the_owner_is_bound_locally_and_another_accounts_login_publishes_nothing") {
        return;
    }
    runtime().block_on(async {
        let central = Central::start().await;
        let host = private_dir();
        let dir = host.path();
        ingest(dir, S, &[user("m1", "owner-bound-needle")]);
        publication::share(dir, S, org(), Some(ALICE), central.server()).unwrap();
        // The host is logged in as Eve: the server refuses Alice's signed owner.
        pump(dir, &central, EVE).await;
        let status = publication::status(dir, S).unwrap();
        assert_eq!(status.state, State::Error, "{status:?}");
        assert!(central.search(EVE, "owner-bound-needle").await.is_empty());
        assert_eq!(central.texts(EVE, S).await, None);
        // A different destination than the one bound at share time: nothing sent.
        let other = Destination {
            server: &central.url,
            client: &central.http,
            cookie: Some(&central.cookies[ALICE]),
        };
        publication::share(dir, S, org(), Some(ALICE), central.server()).unwrap();
        let sent = central.proxy.attempts().len();
        assert_eq!(publication::pump(dir, &other).await.unwrap().sent, 0);
        assert_eq!(central.proxy.attempts().len(), sent);
        assert_eq!(central.texts(ALICE, S).await, None);
    });
}

/// The signed operation inside a recorded publication request.
fn op_of(body: &[u8]) -> Value {
    let request: Value = serde_json::from_slice(body).unwrap();
    serde_json::from_slice(&B64U.decode(request["op"].as_str().unwrap()).unwrap()).unwrap()
}

#[test]
fn a_bound_publication_moves_only_after_its_old_copy_is_deleted() {
    if !isolated("a_bound_publication_moves_only_after_its_old_copy_is_deleted") {
        return;
    }
    runtime().block_on(async {
        let (a, b) = (Central::start().await, Central::start().await);
        let host = private_dir();
        let dir = host.path();
        ingest(
            dir,
            S,
            &[user("m1", "moving-needle one"), user("m2", "two")],
        );
        publication::share(dir, S, people(&[BOB]), Some(ALICE), a.server()).unwrap();
        pump(dir, &a, ALICE).await;
        assert_eq!(state(dir, S), State::Shared);
        let bound = publication::status(dir, S).unwrap();

        // Asking to publish the live copy elsewhere, or as another account.
        let elsewhere = publication::share(dir, S, people(&[BOB]), Some(ALICE), b.server());
        let as_carol = publication::share(dir, S, people(&[BOB]), Some(CAROL), a.server());
        // Then the user unshares and both publishers run.
        let pending = publication::unshare(dir, S).unwrap();
        pump(dir, &a, ALICE).await;
        pump(dir, &b, ALICE).await;
        assert_eq!(
            b.texts(BOB, S).await,
            None,
            "unshared history is readable on the second server"
        );
        assert_eq!(
            a.texts(BOB, S).await,
            None,
            "the first server's copy was abandoned instead of deleted"
        );
        assert!(a.search(BOB, "moving-needle").await.is_empty());
        for (attempt, what) in [(elsewhere, "server"), (as_carol, "owner")] {
            let error = format!("{:#}", attempt.expect_err(what));
            assert!(
                error.contains(a.server()) && error.contains("unshare"),
                "{what}: {error}"
            );
        }
        assert_eq!(pending.server.as_deref(), Some(a.server()));
        assert_eq!(pending.rev, bound.rev + 1);

        // While deletion is pending the binding is still refused.
        let (s2, s2_text) = ("9a1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b22", "pending-needle");
        ingest(dir, s2, &[user("p1", s2_text)]);
        publication::share(dir, s2, org(), Some(ALICE), a.server()).unwrap();
        pump(dir, &a, ALICE).await;
        a.proxy.set(Mode::Offline);
        publication::unshare(dir, s2).unwrap();
        pump(dir, &a, ALICE).await;
        assert_eq!(state(dir, s2), State::DeletionPending);
        let error = publication::share(dir, s2, org(), Some(ALICE), b.server()).unwrap_err();
        assert!(format!("{error:#}").contains("pending"), "{error:#}");
        assert_eq!(state(dir, s2), State::DeletionPending);
        a.proxy.set(Mode::Pass);
        pump(dir, &a, ALICE).await;
        assert_eq!(state(dir, s2), State::Private);
        assert!(a.search(CAROL, s2_text).await.is_empty());

        // Once the old copy is acknowledged deleted, the move is a new share.
        let before = a.proxy.attempts().len();
        let moved = publication::share(dir, S, people(&[BOB]), Some(ALICE), b.server()).unwrap();
        assert!(moved.rev > pending.rev);
        pump(dir, &a, ALICE).await;
        pump(dir, &b, ALICE).await;
        assert_eq!(state(dir, S), State::Shared);
        assert_eq!(
            b.texts(BOB, S).await.unwrap(),
            vec!["moving-needle one", "two"]
        );
        assert_eq!(a.texts(BOB, S).await, None);
        assert_eq!(
            a.proxy.attempts().len(),
            before,
            "the old server was sent the move"
        );
        assert_eq!(local_count(dir, S), 2);
    });
}

#[test]
fn a_pump_for_the_old_server_never_sends_requests_for_the_new_one() {
    if !isolated("a_pump_for_the_old_server_never_sends_requests_for_the_new_one") {
        return;
    }
    runtime().block_on(async {
        let (a, b) = (Central::start().await, Central::start().await);
        let host = private_dir();
        let dir = host.path();
        ingest(dir, S, &[user("m1", "race-needle one"), user("m2", "two")]);
        publication::share(dir, S, people(&[BOB]), Some(ALICE), a.server()).unwrap();
        // The old pump's append (request 1) is applied on A but its answer
        // is still in flight while the session is unshared there. The
        // directory-wide pump lock makes a second pump step aside (busy, no
        // requests) and the deletion waits for the first pump.
        a.proxy.hold_response_of(1);
        let old_pump = pump(dir, &a, ALICE);
        let meanwhile = async {
            a.proxy.holding.notified().await;
            publication::unshare(dir, S).unwrap();
            let sent_before = a.proxy.attempts().len();
            let second = pump(dir, &a, ALICE).await;
            assert!(
                second.busy && second.sent == 0 && second.transport_errors == 0,
                "{second:?}"
            );
            assert_eq!(
                a.proxy.attempts().len(),
                sent_before,
                "a busy pump sent requests"
            );
            assert_eq!(state(dir, S), State::DeletionPending);
            a.proxy.release.notify_one();
        };
        tokio::join!(old_pump, meanwhile);

        // With the first pump finished, the persisted deletion completes on A
        // and only then is the session moved to B.
        pump(dir, &a, ALICE).await;
        assert_eq!(state(dir, S), State::Private);
        assert_eq!(a.texts(BOB, S).await, None, "the deleted copy came back");
        let moved = publication::share(dir, S, people(&[BOB]), Some(ALICE), b.server()).unwrap();

        // The pumps for A were serialized and settled before the move. A pump
        // still pointed at A after the move sends nothing for the new binding,
        // settles nothing, and A is never sent the new revision.
        let sent_to_a = a.proxy.attempts().len();
        let stale = pump(dir, &a, ALICE).await;
        assert!(
            !stale.busy && stale.sent == 0 && stale.transport_errors == 0,
            "{stale:?}"
        );
        assert_eq!(
            a.proxy.attempts().len(),
            sent_to_a,
            "a pump for the old server sent requests after the move"
        );
        let status = publication::status(dir, S).unwrap();
        assert_eq!(
            (status.state, status.rev, status.published),
            (State::Syncing, moved.rev, 0),
            "{status:?}"
        );
        assert_eq!(status.server.as_deref(), Some(b.server()));
        let to_a: Vec<Value> = a.proxy.attempts().iter().map(|b| op_of(b)).collect();
        assert!(
            to_a.iter().all(|op| op["rev"].as_u64() < Some(moved.rev)),
            "the old pump sent the new revision to the old server: {to_a:?}"
        );
        assert_eq!(a.texts(BOB, S).await, None, "the deleted copy came back");
        assert!(a.search(BOB, "race-needle").await.is_empty());

        // The new server gets it, retried byte for byte after a lost answer.
        b.proxy.lose_response_of(0);
        assert_eq!(pump(dir, &b, ALICE).await.transport_errors, 1);
        pump(dir, &b, ALICE).await;
        let to_b = b.proxy.attempts();
        assert_eq!(to_b[0], to_b[1], "the retry was not byte-identical");
        assert!(to_b
            .iter()
            .all(|body| op_of(body)["rev"].as_u64() == Some(moved.rev)));
        assert_eq!(state(dir, S), State::Shared);
        assert_eq!(
            b.texts(BOB, S).await.unwrap(),
            vec!["race-needle one", "two"]
        );
        assert_eq!(a.texts(BOB, S).await, None);
    });
}

#[test]
fn a_session_whose_history_never_stops_changing_cannot_starve_a_pending_deletion() {
    if !isolated("a_session_whose_history_never_stops_changing_cannot_starve_a_pending_deletion") {
        return;
    }
    runtime().block_on(async {
        let central = Central::start().await;
        let host = private_dir();
        let dir = host.path().to_path_buf();
        let (busy, quiet) = (S, "9a1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b33");
        ingest(&dir, busy, &[user("b0", "busy start")]);
        ingest(&dir, quiet, &[user("q1", "quiet-needle")]);
        for session in [busy, quiet] {
            publication::share(&dir, session, org(), Some(ALICE), central.server()).unwrap();
        }
        pump(&dir, &central, ALICE).await;
        assert_eq!(state(&dir, busy), State::Shared);
        assert_eq!(state(&dir, quiet), State::Shared);

        // Every answered append of the busy session changes a published
        // message and adds a new one, so its backlog is never empty. The cap
        // only bounds a publisher that would otherwise drain it forever.
        const CAP: usize = 200;
        let growths = Arc::new(Mutex::new(0usize));
        let hook = {
            let (dir, growths) = (dir.clone(), growths.clone());
            move |body: &[u8]| {
                let op = op_of(body);
                if op["session"] != busy || op["action"]["type"] != "append" {
                    return;
                }
                let mut n = growths.lock().unwrap();
                if *n < CAP {
                    *n += 1;
                    ingest(
                        &dir,
                        busy,
                        &[
                            user("b0", &format!("busy start, edit {n}")),
                            user(&format!("b{n:04}"), "more"),
                        ],
                    );
                }
            }
        };
        *central.proxy.on_answer.lock().unwrap() = Some(Arc::new(hook));
        ingest(&dir, busy, &[user("b9999", "backlog")]);
        publication::unshare(&dir, quiet).unwrap();
        assert_eq!(state(&dir, quiet), State::DeletionPending);

        let before = central.proxy.delivered().len();
        pump(&dir, &central, ALICE).await;
        let grown = *growths.lock().unwrap();
        assert!(
            grown < CAP,
            "one pump drained the busy session {grown} times"
        );
        assert_eq!(state(&dir, quiet), State::Private);
        assert!(central.search(CAROL, "quiet-needle").await.is_empty());
        assert_eq!(central.texts(CAROL, quiet).await, None);
        let sent: Vec<Value> = central.proxy.delivered()[before..]
            .iter()
            .map(|b| op_of(b))
            .collect();
        assert_eq!(
            sent[0]["action"]["type"], "unshare",
            "the pending deletion waited behind other work: {sent:?}"
        );
        // The busy session still progresses, a bounded amount per pass.
        assert_eq!(state(&dir, busy), State::Syncing);
        assert!(grown > 0);
        let published = publication::status(&dir, busy).unwrap().published;
        pump(&dir, &central, ALICE).await;
        assert!(publication::status(&dir, busy).unwrap().published >= published);
        assert!(*growths.lock().unwrap() > grown, "the busy session stalled");
    });
}

#[test]
fn an_audience_too_large_to_sign_stops_only_its_own_session_visibly() {
    if !isolated("an_audience_too_large_to_sign_stops_only_its_own_session_visibly") {
        return;
    }
    runtime().block_on(async {
        let central = Central::start().await;
        let host = private_dir();
        let dir = host.path();
        let other = "9a1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b44";
        ingest(dir, S, &[user("m1", "crowded-needle")]);
        ingest(dir, other, &[user("o1", "other-needle")]);
        // Within the person limit, but longer than one signed operation holds.
        let crowd: Vec<String> = (0..conversations_core::publication::MAX_AUDIENCE_EMAILS)
            .map(|i| format!("{}{i:03}@x.test", "p".repeat(90)))
            .collect();
        let crowd: Vec<&str> = crowd.iter().map(String::as_str).collect();
        publication::share(dir, S, people(&crowd), Some(ALICE), central.server()).unwrap();
        publication::share(dir, other, people(&[BOB]), Some(ALICE), central.server()).unwrap();

        pump(dir, &central, ALICE).await;
        let status = publication::status(dir, S).unwrap();
        assert_eq!(status.state, State::Error, "{status:?}");
        assert!(status.error.unwrap().contains("audience"));
        assert_eq!(central.texts(ALICE, S).await, None);
        assert_eq!(state(dir, other), State::Shared);
        assert_eq!(
            central.texts(BOB, other).await.unwrap(),
            vec!["other-needle"]
        );
        // A smaller audience is a new revision that publishes.
        publication::share(dir, S, people(&[BOB]), Some(ALICE), central.server()).unwrap();
        pump(dir, &central, ALICE).await;
        assert_eq!(central.texts(BOB, S).await.unwrap(), vec!["crowded-needle"]);
    });
}

#[test]
fn oversized_or_unreadable_history_blocks_visibly_instead_of_skipping() {
    if !isolated("oversized_or_unreadable_history_blocks_visibly_instead_of_skipping") {
        return;
    }
    runtime().block_on(async {
        let central = Central::start().await;
        let host = private_dir();
        let dir = host.path();
        let huge = "x".repeat(conversations_core::publication::MAX_CHUNK_BYTES + 10);
        ingest(
            dir,
            S,
            &[user("m1", "before"), user("m2", &huge), user("m3", "after")],
        );
        publication::share(dir, S, org(), Some(ALICE), central.server()).unwrap();
        pump(dir, &central, ALICE).await;
        let status = publication::status(dir, S).unwrap();
        assert_eq!(status.state, State::Error, "{status:?}");
        assert!(status.error.unwrap().contains("m2"));
        // Nothing after the blocked message was published around it.
        let texts = central.texts(BOB, S).await.unwrap_or_default();
        assert!(!texts.iter().any(|t| t == "after"), "{texts:?}");
    });
}

// --------------------------------------------------- signed browser intent ----

fn host_on(dir: &Path, server: &str, cookie: Option<String>) -> Host {
    Host {
        dir: dir.to_path_buf(),
        cwd: dir.to_path_buf(),
        server: server.to_owned(),
        client: reqwest::Client::new(),
        cookie,
        availability: crate::commands::live::availability::AvailabilitySignal::new(),
        backend: Backend::Normal,
        cmux: None,
        sessions: Mutex::new(HashMap::new()),
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
    }
}

/// A live terminal for `S` on `host`, as a connected wrapper leaves it.
fn attach_terminal(host: &Host) -> mpsc::Receiver<ServerMessage> {
    let (commands, rx) = mpsc::channel(64);
    host.sessions.lock().unwrap().insert(
        S.into(),
        Session {
            screen: Some(Screen::new(80, 24)),
            readiness: Default::default(),
            local_input_bytes: 0,
            local_input_dropped_chunks: 0,
            registration: Registration {
                launch: Launch {
                    id: S.into(),
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
            },
            resume_published: None,
            opencode_title_published: None,
            commands,
            pid: None,
        },
    );
    rx
}

fn grants(dir: &Path) -> i64 {
    Store::open(dir)
        .unwrap()
        .connection()
        .query_row("SELECT COUNT(*) FROM grants", [], |r| r.get(0))
        .unwrap()
}

struct Signed<'a> {
    dir: &'a Path,
    host_id: String,
    jti: std::cell::Cell<u64>,
}

impl Signed<'_> {
    fn frame(&self, signer: &Signer, op: &str, args: &str) -> SignedFrame {
        self.jti.set(self.jti.get() + 1);
        let now = now_secs();
        let payload = signer.op_payload(
            &self.host_id,
            op,
            S,
            args,
            &format!("{:032x}", self.jti.get()),
            now,
            now + 60,
        );
        signer.sign_bytes(payload.as_bytes())
    }

    fn enroll(&self, kind: &str, signer: &Signer, grants: &[String]) {
        authz::enroll_from_cli(
            self.dir,
            kind,
            "ed25519",
            &B64.encode(&signer.public),
            "t",
            grants,
        )
        .unwrap();
    }
}

#[test]
fn signed_publication_status_preserves_the_hosts_actual_audience_and_pending_state() {
    if !isolated("signed_publication_status_preserves_the_hosts_actual_audience_and_pending_state")
    {
        return;
    }
    runtime().block_on(async {
        let tmp = private_dir();
        let dir = tmp.path();
        Store::open(dir)
            .unwrap()
            .insert_session(
                &authz::sessions::LocalSession {
                    session_id: S.into(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/private/status-fixture".into(),
                    resume_id: None,
                    launcher: conversations_core::AgentLauncher::default(),
                    adk_statusline: false,
                },
                "signed_start",
            )
            .unwrap();
        ingest(dir, S, &[user("status-m1", "local status fixture")]);
        let host = host_on(dir, "http://127.0.0.1:1", None);
        let signed = Signed {
            dir,
            host_id: authz::host_id(dir).unwrap(),
            jti: std::cell::Cell::new(0),
        };
        let manager = Signer::new();
        signed.enroll("owner", &manager, &[format!("session.manage_sharing:{S}")]);
        for audience in [Audience::Organization {}, people(&[BOB])] {
            publication::share(dir, S, audience.clone(), Some(ALICE), &host.server).unwrap();
            let frame = signed.frame(
                &manager,
                "publication.status",
                &format!(r#"{{"session":"{S}"}}"#),
            );
            // Already-decrypted dispatcher seam; the browser's actual encrypted
            // transport is independently exercised by private_transport.rs.
            let (outcome, detail) = execute_signed(&host, frame).await;
            assert_eq!(outcome, OpStatus::Executed, "{detail:?}");
            let detail: Value = serde_json::from_str(&detail.unwrap())
                .expect("publication status omitted its authoritative audience");
            assert_eq!(detail["state"], "syncing");
            assert_eq!(detail["audience"], serde_json::to_value(audience).unwrap());
            assert_eq!(detail["published"], 0);
            assert_eq!(detail["pending"], 1);
            assert!(detail["rev"].as_u64().unwrap() > 0);
        }
        publication::unshare(dir, S).unwrap();
        let (outcome, detail) = execute_signed(
            &host,
            signed.frame(
                &manager,
                "publication.status",
                &format!(r#"{{"session":"{S}"}}"#),
            ),
        )
        .await;
        assert_eq!(outcome, OpStatus::Executed);
        let detail: Value = serde_json::from_str(&detail.unwrap()).unwrap();
        assert_eq!(detail["state"], "deletion_pending");
        assert!(detail["audience"].is_null());
        assert_eq!(
            local_count(dir, S),
            1,
            "status/unshare destroyed canonical history"
        );
    });
}

#[test]
fn only_a_locally_authorized_sharing_key_can_publish_or_unshare_and_reading_grants_nothing() {
    if !isolated(
        "only_a_locally_authorized_sharing_key_can_publish_or_unshare_and_reading_grants_nothing",
    ) {
        return;
    }
    runtime().block_on(async {
        let central = Central::start().await;
        let tmp = private_dir();
        let dir = tmp.path();
        // The account this host publishes as is local configuration.
        std::fs::write(
            std::env::var("CONVERSATIONS_HOST_SETTINGS_FILE").unwrap(),
            json!({"publication": {"account": ALICE}}).to_string(),
        )
        .unwrap();
        let host_id = authz::host_id(dir).unwrap();
        Store::open(dir)
            .unwrap()
            .insert_session(
                &authz::sessions::LocalSession {
                    session_id: S.into(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/work".into(),
                    resume_id: None,
                    launcher: conversations_core::AgentLauncher::default(),
                    adk_statusline: false,
                },
                "signed_start",
            )
            .unwrap();
        ingest(dir, S, &[user("m1", "signed-needle")]);
        // Eve's login on the host: whatever the central account claims, only
        // the signature decides. Publication still uses the locally bound owner.
        let host = host_on(dir, central.server(), Some(central.cookies[ALICE].clone()));
        let mut terminal = attach_terminal(&host);
        let signed = Signed {
            dir,
            host_id,
            jti: std::cell::Cell::new(0),
        };
        let manager = Signer::new();
        let writer = Signer::new();
        let agent = Signer::new();
        let stranger = Signer::new();
        signed.enroll(
            "owner",
            &manager,
            &[
                format!("session.manage_sharing:{S}"),
                format!("history.read:{S}"),
            ],
        );
        signed.enroll(
            "owner",
            &writer,
            &[format!("session.write:{S}"), format!("history.read:{S}")],
        );
        signed.enroll("agent", &agent, &[format!("session.write:{S}")]);
        let grants_before = grants(dir);
        let share_args = format!(
            r#"{{"session":"{S}","audience":{{"visibility":"restricted","emails":["{BOB}"]}}}}"#
        );
        let unshare_args = format!(r#"{{"session":"{S}"}}"#);

        for signer in [&writer, &agent, &stranger] {
            let (status, detail) = execute_signed(
                &host,
                signed.frame(signer, "publication.share", &share_args),
            )
            .await;
            assert_eq!(status, OpStatus::Refused, "{detail:?}");
        }
        pump(dir, &central, ALICE).await;
        assert_eq!(state(dir, S), State::Private);
        assert_eq!(central.texts(BOB, S).await, None);
        assert!(central.proxy.attempts().is_empty());

        let (status, detail) = execute_signed(
            &host,
            signed.frame(&manager, "publication.share", &share_args),
        )
        .await;
        assert_eq!(status, OpStatus::Executed, "{detail:?}");
        let (status, detail) = execute_signed(
            &host,
            signed.frame(&manager, "publication.status", &unshare_args),
        )
        .await;
        assert_eq!(status, OpStatus::Executed, "{detail:?}");
        let detail: Value = serde_json::from_str(&detail.unwrap()).unwrap();
        assert_eq!(detail["state"], "syncing");
        assert_eq!(
            detail["audience"],
            json!({"visibility":"restricted","emails":[BOB]})
        );
        pump(dir, &central, ALICE).await;
        assert_eq!(central.texts(BOB, S).await.unwrap(), vec!["signed-needle"]);
        // Reading is not execution: no host grant appeared for anyone.
        assert_eq!(grants(dir), grants_before);

        for signer in [&writer, &agent, &stranger] {
            let (status, _) = execute_signed(
                &host,
                signed.frame(signer, "publication.unshare", &unshare_args),
            )
            .await;
            assert_eq!(status, OpStatus::Refused);
        }
        pump(dir, &central, ALICE).await;
        assert_eq!(central.texts(BOB, S).await.unwrap().len(), 1);

        let (status, detail) = execute_signed(
            &host,
            signed.frame(&manager, "publication.unshare", &unshare_args),
        )
        .await;
        assert_eq!(status, OpStatus::Executed, "{detail:?}");
        pump(dir, &central, ALICE).await;
        assert_eq!(central.texts(BOB, S).await, None);
        assert_eq!(state(dir, S), State::Private);
        // The agent and its terminal were not touched; local history remains.
        assert!(host.sessions.lock().unwrap().contains_key(S));
        assert!(
            terminal.try_recv().is_err(),
            "the terminal received a command"
        );
        assert_eq!(local_count(dir, S), 1);
    });
}

// ------------------------------------------------------- canonical follow ----

/// A recording stand-in for `claude`: it records how it was run, then appends
/// to its own transcript in Claude's on-disk layout over time, without being
/// told where the host keeps anything.
const RECORDING_CLAUDE: &str = r#"#!/bin/sh
id="$1"; runs="$2"; go="$3"
echo "$$ $*" >> "$runs"
dir="$HOME/.claude/projects/$(pwd -P | /usr/bin/tr './' '--')"
/bin/mkdir -p "$dir"
line() {
  printf '%s\n' '{"type":"user","uuid":"'"$id-$1"'","sessionId":"'"$id"'","timestamp":"2026-11-02T10:00:0'"$1"'.000Z","message":{"role":"user","content":"'"$2"'"}}' >> "$dir/$id.jsonl"
}
i=1
while [ $i -le 6 ]; do line $i "offline growth step$i"; /bin/sleep 0.3; i=$((i+1)); done
while [ ! -e "$go" ]; do /bin/sleep 0.1; done
line 7 "after share step7"
exec /bin/sleep 600
"#;

/// A test-owned agent in its own process group. Dropping it, on success or on
/// a failed assertion, kills that group and reaps the child, so nothing it
/// started outlives the test or holds the test runner's output open.
struct OwnedAgent(std::process::Child);

impl OwnedAgent {
    fn spawn(command: &mut std::process::Command) -> Self {
        use std::os::unix::process::CommandExt;
        let child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        Self(child)
    }

    fn running(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}

impl Drop for OwnedAgent {
    fn drop(&mut self) {
        // The group id is this child's own pid (`process_group(0)`).
        unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

async fn eventually<T>(what: &str, mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn local_texts(dir: &Path, session: &str) -> Vec<String> {
    History::open(dir)
        .and_then(|history| history.messages(session, 1000))
        .unwrap_or_default()
        .into_iter()
        .map(|message| message.text)
        .collect()
}

#[test]
fn a_growing_transcript_is_followed_locally_while_central_is_down_and_leaves_only_on_share() {
    if !isolated(
        "a_growing_transcript_is_followed_locally_while_central_is_down_and_leaves_only_on_share",
    ) {
        return;
    }
    runtime().block_on(async {
        let central = Central::start().await;
        central.proxy.set(Mode::Offline);
        let tmp = private_dir();
        let dir = tmp.path();
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let work = home.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let work = work.canonicalize().unwrap();
        let (stub, runs, go) = (
            home.join("recording-claude"),
            home.join("claude.runs"),
            home.join("go"),
        );
        std::fs::write(&stub, RECORDING_CLAUDE).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();

        // The canonical local record a registration leaves: the native binding.
        Store::open(dir)
            .unwrap()
            .insert_session(
                &authz::sessions::LocalSession {
                    session_id: S.into(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: work.to_string_lossy().into_owned(),
                    resume_id: Some(S.into()),
                    launcher: conversations_core::AgentLauncher::default(),
                    adk_statusline: false,
                },
                "signed_start",
            )
            .unwrap();
        let host = Arc::new(host_on(
            dir,
            central.server(),
            Some(central.cookies[ALICE].clone()),
        ));
        let registration = Registration {
            launch: Launch {
                id: S.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: work.clone(),
                command: vec![stub.to_string_lossy().into_owned()],
            },
            workspace: None,
            surface: None,
            local_terminal: true,
            reconnecting: false,
            file_receiver: true,
            transfer_gate: true,
            effect_gate: true,
        };
        let follow = tokio::spawn(transcript_loop(host.clone(), registration));
        let mut agent = OwnedAgent::spawn(
            std::process::Command::new(&stub)
                .arg(S)
                .arg(&runs)
                .arg(&go)
                .current_dir(&work),
        );

        // Central is down the whole time the transcript grows.
        let grown = eventually("the transcript to grow into local history", async || {
            let texts = local_texts(dir, S);
            (texts.len() >= 6).then_some(texts)
        })
        .await;
        assert!(
            std::fs::read_to_string(&runs).is_ok_and(|r| r.contains(S)),
            "the recording agent did not run"
        );
        assert_eq!(
            grown,
            (1..=6)
                .map(|i| format!("offline growth step{i}"))
                .collect::<Vec<_>>()
        );
        let history = History::open(dir).unwrap();
        assert_eq!(
            history
                .search("offline growth", Some(S), 100)
                .unwrap()
                .len(),
            6
        );
        assert_eq!(history.search("step6", None, 100).unwrap().len(), 1);
        let uploads = |central: &Central| {
            central
                .proxy
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|line| line.contains("/api/live") || line.contains("/api/sessions"))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(uploads(&central), Vec::<String>::new());

        // Central returns and the daemon's publisher runs: still private.
        central.proxy.set(Mode::Pass);
        let publisher = tokio::spawn(publication::run_loop(
            dir.to_path_buf(),
            central.server().to_owned(),
            reqwest::Client::new(),
            Some(central.cookies[ALICE].clone()),
        ));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(state(dir, S), State::Private);
        assert_eq!(central.texts(ALICE, S).await, None);
        assert!(central.search(ALICE, "offline").await.is_empty());
        assert!(central.proxy.attempts().is_empty());
        assert_eq!(uploads(&central), Vec::<String>::new());

        // An explicit share publishes the history captured offline, then follows.
        publication::share(dir, S, people(&[BOB]), Some(ALICE), central.server()).unwrap();
        let published = eventually("Bob to read the shared history", async || {
            central.texts(BOB, S).await.filter(|texts| texts.len() >= 6)
        })
        .await;
        assert_eq!(published, grown);
        std::fs::write(&go, "").unwrap();
        let followed = eventually("the next message to follow the share", async || {
            central.texts(BOB, S).await.filter(|texts| texts.len() >= 7)
        })
        .await;
        assert_eq!(
            followed.last().map(String::as_str),
            Some("after share step7")
        );
        assert_eq!(central.texts(EVE, S).await, None);
        assert_eq!(state(dir, S), State::Shared);
        assert_eq!(uploads(&central), Vec::<String>::new());

        // Stopping the publisher and the follower leaves the agent and history.
        publisher.abort();
        follow.abort();
        assert!(agent.running(), "the agent stopped");
        assert_eq!(local_texts(dir, S).len(), 7);
    });
}

#[test]
fn with_central_auth_off_a_signed_local_intent_publishes_without_an_owner() {
    if !isolated("with_central_auth_off_a_signed_local_intent_publishes_without_an_owner") {
        return;
    }
    runtime().block_on(async {
        let central = Central::start_with(false).await;
        // No publication account: nothing invents one.
        std::fs::write(
            std::env::var("CONVERSATIONS_HOST_SETTINGS_FILE").unwrap(),
            "{}",
        )
        .unwrap();
        let legacy = "9b2f8f4e-7a51-4c5e-8d0a-3e6f1c2d4b77";
        let upload = json!({"uploaded_by": "laptop-user", "source_host": null,
            "session": conversations_core::parse_session_str(None,
                &json!({"type":"user","uuid":"l1","sessionId":legacy,
                        "message":{"role":"user","content":"legacy-authoff"}}).to_string()).unwrap()});
        let (status, body) = central
            .post("anyone", "/api/sessions", serde_json::to_vec(&upload).unwrap())
            .await;
        assert_eq!(status, 201, "{body}");

        let tmp = private_dir();
        let dir = tmp.path();
        let host_id = authz::host_id(dir).unwrap();
        Store::open(dir)
            .unwrap()
            .insert_session(
                &authz::sessions::LocalSession {
                    session_id: S.into(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/work".into(),
                    resume_id: None,
                    launcher: conversations_core::AgentLauncher::default(),
                    adk_statusline: false,
                },
                "signed_start",
            )
            .unwrap();
        ingest(dir, S, &[user("m1", "authoff-needle")]);
        let host = host_on(dir, central.server(), None);
        let signed = Signed {
            dir,
            host_id,
            jti: std::cell::Cell::new(0),
        };
        let manager = Signer::new();
        let stranger = Signer::new();
        signed.enroll("owner", &manager, &[format!("session.manage_sharing:{S}")]);
        let share = format!(r#"{{"session":"{S}","audience":{{"visibility":"organization"}}}}"#);
        let session_only = format!(r#"{{"session":"{S}"}}"#);

        // Central auth being off authorizes nothing locally.
        let (status, _) =
            execute_signed(&host, signed.frame(&stranger, "publication.share", &share)).await;
        assert_eq!(status, OpStatus::Refused);
        pump(dir, &central, "anyone").await;
        assert!(central.proxy.attempts().is_empty());
        assert_eq!(central.texts("anyone", S).await, None);

        let (status, detail) =
            execute_signed(&host, signed.frame(&manager, "publication.share", &share)).await;
        assert_eq!(status, OpStatus::Executed, "{detail:?}");
        pump(dir, &central, "anyone").await;
        assert_eq!(state(dir, S), State::Shared);
        assert_eq!(
            central.texts("anyone", S).await.unwrap(),
            vec!["authoff-needle"]
        );
        // Every signed operation names no owner.
        for body in central.proxy.delivered() {
            let request: Value = serde_json::from_slice(&body).unwrap();
            let op: Value = serde_json::from_slice(
                &B64U.decode(request["op"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(op["owner"], Value::Null, "{op}");
        }

        let (status, _) =
            execute_signed(&host, signed.frame(&stranger, "publication.unshare", &session_only))
                .await;
        assert_eq!(status, OpStatus::Refused);
        let (status, _) =
            execute_signed(&host, signed.frame(&manager, "publication.unshare", &session_only))
                .await;
        assert_eq!(status, OpStatus::Executed);
        pump(dir, &central, "anyone").await;
        assert_eq!(central.texts("anyone", S).await, None);
        assert_eq!(state(dir, S), State::Private);
        assert_eq!(local_count(dir, S), 1);
        assert_eq!(
            central.texts("anyone", legacy).await.unwrap(),
            vec!["legacy-authoff"]
        );
    });
}
