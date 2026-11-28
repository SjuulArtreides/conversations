//! The opaque host relay over a real HTTP listener, with a fake host speaking
//! the stream/reply protocol. The server must carry requests, never mix hosts
//! or accounts, never retry, and bound everything. (Signature verification is
//! the host's job and is covered by the CLI tests.)

use std::sync::Arc;
use std::time::Duration;

use conversations_core::host_relay::{RelayEnvelope, MAX_REQUEST_BYTES};
use conversations_server::{build_router, AppState, Config, SqliteRepo};
use futures::StreamExt;
use serde_json::{json, Value};

/// A host's identity key: the id is the SHA-256 of the public key.
struct HostKey {
    pair: ring::signature::Ed25519KeyPair,
    public: String,
    id: String,
}

impl HostKey {
    fn new() -> Self {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
        use ring::signature::KeyPair;
        let pkcs8 =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public = pair.public_key().as_ref().to_vec();
        Self {
            id: B64.encode(ring::digest::digest(&ring::digest::SHA256, &public).as_ref()),
            public: B64.encode(&public),
            pair,
        }
    }

    fn sign(&self, nonce: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
        B64.encode(
            self.pair
                .sign(&conversations_core::host_relay::attach_message(
                    &self.id, nonce,
                ))
                .as_ref(),
        )
    }
}

struct Server {
    base: String,
    state: AppState,
    http: reqwest::Client,
    _db: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self._db);
    }
}

async fn server() -> Server {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "conv-relay-{}-{}.sqlite",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let repo = SqliteRepo::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    let state = AppState::new(
        Arc::new(repo),
        Arc::new(Config {
            auth_enabled: false,
            ..Config::default()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        base: format!("http://{addr}"),
        state,
        http: reqwest::Client::new(),
        _db: path,
    }
}

/// A fake host: an open SSE stream plus the ability to answer.
struct FakeHost {
    events: futures::stream::BoxStream<'static, reqwest::Result<axum::body::Bytes>>,
    buffer: String,
    token: String,
}

async fn challenge(server: &Server, key: &HostKey, user: &str) -> String {
    server
        .http
        .post(format!(
            "{}/api/host-relay/{}/challenge",
            server.base, key.id
        ))
        .header("x-demo-user", user)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["nonce"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn attach_response(
    server: &Server,
    key: &HostKey,
    user: &str,
    nonce: &str,
    sig: &str,
) -> reqwest::Response {
    server
        .http
        .get(format!(
            "{}/api/host-relay/{}/stream?nonce={nonce}&pub={}&sig={sig}",
            server.base, key.id, key.public
        ))
        .header("x-demo-user", user)
        .send()
        .await
        .unwrap()
}

impl FakeHost {
    async fn connect(server: &Server, key: &HostKey, user: &str) -> Self {
        let nonce = challenge(server, key, user).await;
        let response = attach_response(server, key, user, &nonce, &key.sign(&nonce)).await;
        assert_eq!(response.status(), 200);
        let mut host = Self {
            events: response.bytes_stream().boxed(),
            buffer: String::new(),
            token: String::new(),
        };
        let attached = host.next_data().await.expect("attached event");
        assert_eq!(attached["type"], "attached");
        host.token = attached["reply_token"].as_str().unwrap().to_owned();
        host
    }

    async fn next_data(&mut self) -> Option<Value> {
        loop {
            while let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                if let Some(data) = block.lines().find_map(|l| l.strip_prefix("data:")) {
                    return Some(serde_json::from_str(data.trim()).unwrap());
                }
            }
            let chunk = tokio::time::timeout(Duration::from_secs(3), self.events.next())
                .await
                .ok()??
                .ok()?;
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    async fn next_envelope(&mut self) -> RelayEnvelope {
        serde_json::from_value(self.next_data().await.expect("a relayed request")).unwrap()
    }
}

async fn browser(server: &Server, host: &str, user: &str, body: Value) -> reqwest::Response {
    server
        .http
        .post(format!("{}/api/host-relay/{host}/request", server.base))
        .header("x-demo-user", user)
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn answer(server: &Server, host: &str, token: &str, body: Value) -> u16 {
    server
        .http
        .post(format!("{}/api/host-relay/{host}/reply", server.base))
        .header("x-relay-token", token)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn signed() -> Value {
    json!({"type":"signed_op","p":"cA","s":"cw","k":"aw"})
}

#[tokio::test]
async fn a_request_reaches_the_host_and_its_cleaned_answer_returns() {
    let server = server().await;
    let key = HostKey::new();
    let mut host = FakeHost::connect(&server, &key, "me@x").await;
    let waiting = {
        let (http, base, id) = (server.http.clone(), server.base.clone(), key.id.clone());
        tokio::spawn(async move {
            http.post(format!("{base}/api/host-relay/{id}/request"))
                .header("x-demo-user", "me@x")
                .json(&signed())
                .send()
                .await
                .unwrap()
        })
    };
    let envelope = host.next_envelope().await;
    assert_eq!(
        serde_json::to_value(&envelope.request).unwrap(),
        signed(),
        "request modified in transit"
    );
    let long = "x".repeat(1000);
    let status = answer(
        &server,
        &key.id,
        &host.token,
        json!({"id": envelope.id, "reply": {"type":"op","statement":{"p": long.clone(), "s": "sig"}}}),
    )
    .await;
    assert_eq!(status, 204);
    let response = waiting.await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["statement"], json!({"p": long, "s": "sig"}));
    assert_eq!(
        answer(
            &server,
            &key.id,
            &host.token,
            json!({"id": envelope.id, "reply": {"type":"error","message":"late"}})
        )
        .await,
        400
    );
}

#[tokio::test]
async fn any_account_may_route_but_no_other_account_can_claim_or_answer_for_a_host() {
    let server = server().await;
    let (ka, kb) = (HostKey::new(), HostKey::new());
    let mut a = FakeHost::connect(&server, &ka, "bob@x").await;
    let mut b = FakeHost::connect(&server, &kb, "bob@x").await;

    // Alice (another account) routes an opaque request to Bob's host id.
    let to_a = {
        let (http, base, id) = (server.http.clone(), server.base.clone(), ka.id.clone());
        tokio::spawn(async move {
            http.post(format!("{base}/api/host-relay/{id}/request"))
                .header("x-demo-user", "alice@y")
                .json(&signed())
                .send()
                .await
                .unwrap()
        })
    };
    let envelope = a.next_envelope().await;
    assert!(b.next_data().await.is_none(), "host B saw host A's request");

    // Claiming the public id without the key fails: no signature, a signature
    // by another key, a replayed nonce, and a nonce never issued.
    let mallory = HostKey::new();
    let nonce = challenge(&server, &ka, "alice@y").await;
    assert_eq!(
        attach_response(&server, &ka, "alice@y", &nonce, &mallory.sign(&nonce))
            .await
            .status(),
        400
    );
    assert_eq!(
        attach_response(&server, &ka, "alice@y", &nonce, &ka.sign(&nonce))
            .await
            .status(),
        400,
        "a failed proof must burn its nonce"
    );
    assert_eq!(
        attach_response(
            &server,
            &ka,
            "alice@y",
            "f".repeat(64).as_str(),
            &ka.sign(&"f".repeat(64))
        )
        .await
        .status(),
        400
    );
    let proof_for_other = {
        let n = challenge(&server, &kb, "alice@y").await;
        (n.clone(), ka.sign(&n))
    };
    assert_eq!(
        attach_response(
            &server,
            &ka,
            "alice@y",
            &proof_for_other.0,
            &proof_for_other.1
        )
        .await
        .status(),
        400,
        "a nonce issued for another host"
    );
    // Presenting another key's public half under this id is rejected too.
    let n = challenge(&server, &ka, "alice@y").await;
    let spoof = server
        .http
        .get(format!(
            "{}/api/host-relay/{}/stream?nonce={n}&pub={}&sig={}",
            server.base,
            ka.id,
            mallory.public,
            mallory.sign(&n)
        ))
        .header("x-demo-user", "alice@y")
        .send()
        .await
        .unwrap();
    assert_eq!(spoof.status(), 400);

    // Nobody but the proven stream can answer: not another account, not with
    // another host's token, not with a guess.
    let forged = json!({"id": envelope.id, "reply": {"type":"op","statement":{"p":"a","s":"b"}}});
    assert_eq!(answer(&server, &ka.id, "", forged.clone()).await, 400);
    assert_eq!(answer(&server, &ka.id, &b.token, forged.clone()).await, 400);
    assert_eq!(answer(&server, &kb.id, &a.token, forged.clone()).await, 400);
    assert_eq!(answer(&server, &ka.id, "guess", forged.clone()).await, 400);
    assert_eq!(answer(&server, &ka.id, &a.token, forged).await, 204);
    assert_eq!(to_a.await.unwrap().status(), 200);
}

async fn attached(server: &Server, user: &str) -> Value {
    server
        .http
        .get(format!("{}/api/host-relay", server.base))
        .header("x-demo-user", user)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
}

#[tokio::test]
async fn an_account_lists_only_the_hosts_it_attached_with_their_proven_keys() {
    let server = server().await;
    let (mine, theirs) = (HostKey::new(), HostKey::new());
    assert_eq!(attached(&server, "bob@x").await, json!({ "hosts": [] }));

    let host = FakeHost::connect(&server, &mine, "bob@x").await;
    let _other = FakeHost::connect(&server, &theirs, "alice@y").await;
    assert_eq!(
        attached(&server, "bob@x").await,
        json!({ "hosts": [{ "id": mine.id, "key": mine.public }] })
    );
    assert_eq!(
        attached(&server, "alice@y").await,
        json!({ "hosts": [{ "id": theirs.id, "key": theirs.public }] })
    );

    // A host that detaches is no longer offered.
    drop(host);
    tokio::time::timeout(Duration::from_secs(3), async {
        while server.state.host_relay.is_online(&mine.id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the dropped stream detaches");
    assert_eq!(attached(&server, "bob@x").await, json!({ "hosts": [] }));
}

#[tokio::test]
async fn a_hosts_machine_card_is_listed_to_its_own_account_unchanged_and_a_malformed_one_is_refused(
) {
    let server = server().await;
    let key = HostKey::new();
    let attach = |card: String| {
        let server = &server;
        let key = &key;
        async move {
            let nonce = challenge(server, key, "bob@x").await;
            server
                .http
                .get(format!(
                    "{}/api/host-relay/{}/stream?nonce={nonce}&pub={}&sig={}&{card}",
                    server.base,
                    key.id,
                    key.public,
                    key.sign(&nonce)
                ))
                .header("x-demo-user", "bob@x")
                .send()
                .await
                .unwrap()
        }
    };
    assert_eq!(
        attach("card_p=not+base64url&card_s=x".into())
            .await
            .status(),
        400
    );
    let oversized = "a".repeat(2000);
    assert_eq!(
        attach(format!("card_p={oversized}&card_s=x"))
            .await
            .status(),
        400
    );

    let response = attach("card_p=eyJ2IjoxfQ&card_s=c2ln".into()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        attached(&server, "bob@x").await,
        json!({ "hosts": [{
            "id": key.id, "key": key.public,
            "card": { "p": "eyJ2IjoxfQ", "s": "c2ln" },
        }] })
    );
    assert_eq!(attached(&server, "alice@y").await, json!({ "hosts": [] }));
    drop(response);
}

#[tokio::test]
async fn offline_timeout_replacement_and_overload_fail_honestly() {
    let server = server().await;
    let key = HostKey::new();
    server
        .state
        .host_relay
        .set_timeout(Duration::from_millis(300));

    let response = browser(&server, &key.id, "me@x", signed()).await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.json::<Value>().await.unwrap()["code"],
        "host_offline"
    );

    let mut host = FakeHost::connect(&server, &key, "me@x").await;
    let response = browser(&server, &key.id, "me@x", signed()).await;
    assert_eq!(response.status(), 504);
    let stale = host.next_envelope().await;
    assert_eq!(
        answer(
            &server,
            &key.id,
            &host.token,
            json!({"id": stale.id, "reply": {"type":"op","statement":{"p":"a","s":"b"}}})
        )
        .await,
        400
    );

    server.state.host_relay.set_timeout(Duration::from_secs(5));
    let pending = {
        let (http, base, id) = (server.http.clone(), server.base.clone(), key.id.clone());
        tokio::spawn(async move {
            http.post(format!("{base}/api/host-relay/{id}/request"))
                .header("x-demo-user", "me@x")
                .json(&signed())
                .send()
                .await
                .unwrap()
        })
    };
    host.next_envelope().await;
    let _newer = FakeHost::connect(&server, &key, "me@x").await;
    assert_eq!(pending.await.unwrap().status(), 502);
    assert_eq!(host.next_data().await.unwrap()["type"], "superseded");

    // Overload: the per-requester cap stops one account first; distinct
    // requesters then fill the per-host cap.
    let mut handles = Vec::new();
    for n in 0..conversations_server::host_relay::MAX_PENDING {
        let (http, base, id) = (server.http.clone(), server.base.clone(), key.id.clone());
        let user = format!("user{n}@x");
        handles.push(tokio::spawn(async move {
            http.post(format!("{base}/api/host-relay/{id}/request"))
                .header("x-demo-user", user)
                .json(&signed())
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let overflow = browser(&server, &key.id, "late@x", signed()).await;
    assert_eq!(overflow.status(), 429);
}

#[tokio::test]
async fn one_requester_cannot_fill_a_hosts_queue() {
    let server = server().await;
    let key = HostKey::new();
    let _host = FakeHost::connect(&server, &key, "bob@x").await;
    let mut handles = Vec::new();
    for _ in 0..conversations_server::host_relay::MAX_PER_REQUESTER {
        let (http, base, id) = (server.http.clone(), server.base.clone(), key.id.clone());
        handles.push(tokio::spawn(async move {
            http.post(format!("{base}/api/host-relay/{id}/request"))
                .header("x-demo-user", "spam@x")
                .json(&signed())
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        browser(&server, &key.id, "spam@x", signed()).await.status(),
        429
    );
    // Another account still gets through to the queue (it waits for the host).
    let other = tokio::time::timeout(
        Duration::from_millis(500),
        browser(&server, &key.id, "alice@y", signed()),
    )
    .await;
    assert!(
        other.is_err(),
        "another requester was refused instead of queued"
    );
}

#[tokio::test]
async fn only_the_closed_request_set_with_bounded_size_is_accepted() {
    let server = server().await;
    let key = HostKey::new();
    let _host = FakeHost::connect(&server, &key, "me@x").await;
    for body in [
        json!({"type":"approve","cred_id":"x"}),
        json!({"type":"enroll_start","label":"l","kind":"owner","grants":["session.start"]}),
        json!({"type":"signed_op","p":"a","s":"b","k":"c","is_local":true}),
        json!({"type":"session_start","cwd":"/etc"}),
    ] {
        let response = browser(&server, &key.id, "me@x", body.clone()).await;
        assert!(
            response.status().is_client_error(),
            "{body} was accepted: {}",
            response.status()
        );
    }
    let huge = json!({"type":"signed_op","p":"x".repeat(MAX_REQUEST_BYTES),"s":"b","k":"c"});
    assert_eq!(browser(&server, &key.id, "me@x", huge).await.status(), 413);
    let response = server
        .http
        .post(format!("{}/api/host-relay/not-a-host/request", server.base))
        .json(&signed())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let status = server
        .http
        .get(format!("{}/api/host-relay/{}/status", server.base, key.id))
        .header("x-demo-user", "me@x")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(status["online"], true);
}

#[tokio::test]
async fn shutdown_ends_host_streams_and_pending_requests_and_refuses_reattachment() {
    let server = server().await;
    let key = HostKey::new();
    let mut host = FakeHost::connect(&server, &key, "me@x").await;
    let waiting = {
        let relay = server.state.host_relay.clone();
        let id = key.id.clone();
        tokio::spawn(async move {
            relay
                .submit("me@x", &id, serde_json::from_value(signed()).unwrap())
                .await
        })
    };
    host.next_envelope().await;
    server.state.host_relay.close_all();
    assert!(matches!(
        waiting.await.unwrap(),
        Err(conversations_server::host_relay::RelayError::HostDisconnected)
    ));
    assert!(
        host.next_data().await.is_none(),
        "shutdown left the SSE stream open"
    );
    assert!(!server.state.host_relay.is_online(&key.id));
    let nonce = challenge(&server, &key, "me@x").await;
    let response = attach_response(&server, &key, "me@x", &nonce, &key.sign(&nonce)).await;
    assert_eq!(response.status(), 503);
}
