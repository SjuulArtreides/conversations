//! Host-signed publication through the real authenticated router.
//!
//! Each scenario asserts the observable effect (reads, search, PR/Linear links,
//! and the rows left in the database), not just the status code.

use std::sync::Arc;

use axum::{body::Body, http::Request, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use conversations_core::publication::signed_message;
use conversations_server::store::{ChatParent, SessionSummary, SourceContext};
use conversations_server::{
    build_router, build_router_with_auth, AppState, Config, Repository, SqliteRepo,
};
use http_body_util::BodyExt;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Value};
use tower::ServiceExt;

const ALICE: &str = "alice@example.com";

async fn request(
    app: &Router,
    cookie: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .header("cookie", cookie)
                .body(if body.is_null() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn login(app: &Router, email: &str) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": email, "password": "test-password-123", "display_name": email})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

/// A publishing host: an Ed25519 key whose id is the key's SHA-256.
struct Host(Ed25519KeyPair);

impl Host {
    fn new() -> Self {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        Self(Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap())
    }

    fn id(&self) -> String {
        B64.encode(ring::digest::digest(
            &ring::digest::SHA256,
            self.0.public_key().as_ref(),
        ))
    }

    /// Sign `op` exactly as serialized and wrap it in a request.
    fn sign(&self, op: &Value, chunk: Option<&[u8]>) -> Value {
        let op_bytes = serde_json::to_vec(op).unwrap();
        let sig = self.0.sign(&signed_message(&op_bytes));
        let mut body = json!({
            "host_key": B64.encode(self.0.public_key().as_ref()),
            "op": B64.encode(&op_bytes),
            "sig": B64.encode(sig.as_ref()),
        });
        if let Some(chunk) = chunk {
            body["chunk"] = json!(B64.encode(chunk));
        }
        body
    }

    fn op(&self, session: &str, owner: &str, rev: u64, action: Value) -> Value {
        json!({"v": 1, "host": self.id(), "session": session, "owner": owner, "rev": rev, "action": action})
    }

    fn share(&self, session: &str, rev: u64, audience: Value) -> Value {
        let action = json!({"type": "share", "audience": audience, "agent": "claude", "title": "Widget fix"});
        self.sign(&self.op(session, ALICE, rev, action), None)
    }

    fn append(&self, session: &str, rev: u64, seq: u64, messages: &[(&str, &str)]) -> Value {
        let chunk =
            serde_json::to_vec(&json!({"messages": parse(session, messages).messages})).unwrap();
        let digest = B64.encode(ring::digest::digest(&ring::digest::SHA256, &chunk));
        let action = json!({"type": "append", "seq": seq, "chunk_sha256": digest});
        self.sign(&self.op(session, ALICE, rev, action), Some(&chunk))
    }

    fn unshare(&self, session: &str, rev: u64) -> Value {
        self.sign(
            &self.op(session, ALICE, rev, json!({"type": "unshare"})),
            None,
        )
    }
}

fn parse(session: &str, messages: &[(&str, &str)]) -> conversations_core::Session {
    let lines: Vec<String> = messages
        .iter()
        .map(|(uuid, text)| {
            json!({"type": "user", "sessionId": session, "uuid": uuid,
                   "message": {"role": "user", "content": text}})
            .to_string()
        })
        .collect();
    conversations_core::parse_session_str(None, &lines.join("\n")).unwrap()
}

fn restricted(emails: &[&str]) -> Value {
    json!({"visibility": "restricted", "emails": emails})
}

/// Direct row counts, so a deletion is proven at the persistence boundary.
enum Probe {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

impl Probe {
    async fn count(&self, table: &str, session: &str) -> i64 {
        let column = if table == "sessions" {
            "id"
        } else {
            "session_id"
        };
        match self {
            Probe::Sqlite(pool) => {
                sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?"))
                    .bind(session)
                    .fetch_one(pool)
                    .await
                    .unwrap()
            }
            Probe::Postgres(pool) => {
                sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE {column} = $1"))
                    .bind(session)
                    .fetch_one(pool)
                    .await
                    .unwrap()
            }
        }
    }

    /// `(host_id, owner_email, max_rev, state, next_seq)` of the ledger row.
    async fn ledger(&self, session: &str) -> (String, Option<String>, i64, String, i64) {
        let sql = "SELECT host_id, owner_email, max_rev, state, next_seq FROM publications WHERE session_id = ";
        match self {
            Probe::Sqlite(pool) => sqlx::query_as(&format!("{sql}?"))
                .bind(session)
                .fetch_one(pool)
                .await
                .unwrap(),
            Probe::Postgres(pool) => sqlx::query_as(&format!("{sql}$1"))
                .bind(session)
                .fetch_one(pool)
                .await
                .unwrap(),
        }
    }

    /// A reader's `write_pending_key` share flag (0/1 on sqlite, bool on
    /// postgres, coerced to the same scale).
    async fn share_flag(&self, session: &str, email: &str) -> i64 {
        let sql = "SELECT write_pending_key FROM session_shares WHERE session_id = ";
        match self {
            Probe::Sqlite(pool) => {
                sqlx::query_scalar::<sqlx::Sqlite, i64>(&format!("{sql}? AND email = ?"))
                    .bind(session)
                    .bind(email)
                    .fetch_one(pool)
                    .await
                    .unwrap()
            }
            Probe::Postgres(pool) => {
                sqlx::query_scalar::<sqlx::Postgres, bool>(&format!("{sql}$1 AND email = $2"))
                    .bind(session)
                    .bind(email)
                    .fetch_one(pool)
                    .await
                    .unwrap()
                    .into()
            }
        }
    }
}

const CONTENT_TABLES: [&str; 11] = [
    "sessions",
    "session_notifications",
    "session_repos",
    "messages",
    "session_shares",
    "session_prs",
    "session_linear_docs",
    "session_summaries",
    "chats",
    "publication_chunks",
    "publication_message_refs",
];

/// Every revision-less legacy or derived writer refuses a managed session and
/// leaves no derived row: title, repos, project, PR link/status/comments,
/// summary and session chat.
async fn derived_writes_refused(
    app: &Router,
    owner: &str,
    repo: &Arc<dyn Repository>,
    probe: &Probe,
    session: &str,
    project: &str,
) {
    let base = format!("/api/sessions/{session}");
    let exists = probe.count("sessions", session).await == 1;
    let title_before = request(app, owner, "GET", &base, Value::Null).await.1["title"].clone();
    let pr = json!({"owner": "acme", "repo": "widgets", "number": 42});
    let mut status = pr.clone();
    status["status"] = json!({"state": "open", "merged": false, "draft": false, "title": "t"});
    let mut comments = pr;
    comments["comments"] = json!([]);
    for (method, path, body) in [
        ("PUT", format!("{base}/title"), json!({"title": "renamed"})),
        (
            "PUT",
            format!("{base}/repos"),
            json!({"repos": [{"repo": "acme/widgets", "branch": "main"}]}),
        ),
        (
            "PUT",
            format!("{base}/project"),
            json!({"project_id": project}),
        ),
        (
            "POST",
            format!("{base}/prs"),
            json!({"url": "https://github.com/acme/widgets/pull/43"}),
        ),
        ("POST", format!("{base}/prs/status"), status),
        ("POST", format!("{base}/prs/comments"), comments),
    ] {
        let (code, body) = request(app, owner, method, &path, body).await;
        // With no projection the route may already answer 404; either way
        // nothing is written.
        let refused = if exists { vec![409] } else { vec![404, 409] };
        assert!(refused.contains(&code), "{method} {path}: {code} {body}");
    }
    let summary = SessionSummary {
        intent: "derived-needle".into(),
        invariants: vec![],
        summary: "derived".into(),
        model: "test".into(),
        generated_by: Some(ALICE.into()),
        generated_at: chrono::Utc::now(),
    };
    // A revision-less (unmanaged) context never writes into a managed session.
    assert!(repo
        .upsert_summary(SourceContext::Unmanaged, session, &summary)
        .await
        .is_err());
    let chat = ChatParent::Session(session.to_owned());
    assert!(repo
        .create_chat(&chat, SourceContext::Unmanaged, Some(ALICE), Some("q"))
        .await
        .is_err());
    let title_after = request(app, owner, "GET", &base, Value::Null).await.1["title"].clone();
    assert_eq!(title_after, title_before);
    for table in [
        "session_summaries",
        "chats",
        "session_repos",
        "session_pr_comment_revisions",
    ] {
        assert_eq!(probe.count(table, session).await, 0, "{table}");
    }
    assert!(
        !request(app, owner, "GET", &format!("{base}/prs"), Value::Null)
            .await
            .1
            .to_string()
            .contains("pull/43")
    );
}

async fn search(app: &Router, cookie: &str, term: &str) -> Vec<String> {
    let (status, body) = request(
        app,
        cookie,
        "GET",
        &format!("/api/sessions?q={term}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn message_texts(app: &Router, cookie: &str, session: &str) -> Option<Vec<String>> {
    let (status, body) = request(
        app,
        cookie,
        "GET",
        &format!("/api/sessions/{session}"),
        Value::Null,
    )
    .await;
    (status == 200).then(|| {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["text"].as_str().unwrap().to_owned())
            .collect()
    })
}

/// Seed a live row directly (the central registration endpoint is retired).
async fn seed_live(repo: &Arc<dyn Repository>, owner: &str, id: &str) {
    repo.create_live_session(&conversations_server::store::NewLiveSession {
        id: id.into(),
        session_id: id.into(),
        agent: conversations_core::AgentKind::Claude,
        title: Some("Live".into()),
        project_path: None,
        cwd: None,
        git_branch: None,
        owner_email: Some(owner.into()),
        source_host: None,
        remote_url: None,
        mode: "interactive".into(),
    })
    .await
    .unwrap();
}

async fn exercise(repo: Arc<dyn Repository>, probe: Probe) {
    let config = Config {
        auth_enabled: true,
        password_auth_enabled: true,
        cookie_secure: false,
        ..Config::default()
    };
    let app = build_router_with_auth(AppState::new(repo.clone(), Arc::new(config)))
        .await
        .unwrap();
    let alice = login(&app, ALICE).await;
    let bob = login(&app, "bob@example.com").await;
    let eve = login(&app, "eve@example.com").await;
    let project = repo
        .create_project(Some(ALICE), "acme", None)
        .await
        .unwrap();
    let project = project.id.as_str();
    let host = Host::new();
    let other_host = Host::new();
    let publish = |cookie: &str, body: Value| {
        let app = app.clone();
        let cookie = cookie.to_owned();
        async move { request(&app, &cookie, "POST", "/api/publications", body).await }
    };
    let s = uuid::Uuid::new_v4().to_string();

    // Existing unmanaged data: an uploaded session and a live session.
    let legacy = uuid::Uuid::new_v4().to_string();
    let upload = |id: &str, text: &str| json!({"uploaded_by": ALICE, "source_host": null, "session": parse(id, &[("legacy-1", text)])});
    assert_eq!(
        request(
            &app,
            &alice,
            "POST",
            "/api/sessions",
            upload(&legacy, "legacy-needle")
        )
        .await
        .0,
        201
    );
    let live = uuid::Uuid::new_v4().to_string();
    let register = |id: &str| json!({"session_id": id, "title": "Live", "agent": "claude", "mode": "interactive"});
    seed_live(&repo, "bob@example.com", &live).await;

    // A managed id cannot adopt an uploaded or a live id; both stay intact.
    for id in [&legacy, &live] {
        let (status, _) = publish(&alice, host.share(id, 1, restricted(&[]))).await;
        assert_eq!(status, 409, "share must not adopt existing id {id}");
        assert_eq!(probe.count("publications", id).await, 0);
    }
    assert_eq!(
        message_texts(&app, &alice, &legacy).await.unwrap(),
        vec!["legacy-needle"]
    );
    assert_eq!(probe.count("live_sessions", &live).await, 1);
    // A live row whose transcript was deleted still owns its id.
    let orphan = uuid::Uuid::new_v4().to_string();
    seed_live(&repo, "bob@example.com", &orphan).await;
    let deleted = request(
        &app,
        &bob,
        "DELETE",
        &format!("/api/sessions/{orphan}"),
        Value::Null,
    )
    .await
    .0;
    assert!(deleted < 300, "{deleted}");
    assert_eq!(probe.count("sessions", &orphan).await, 0);
    assert_eq!(
        publish(&alice, host.share(&orphan, 1, restricted(&[])))
            .await
            .0,
        409
    );
    assert_eq!(probe.count("publications", &orphan).await, 0);
    assert_eq!(probe.count("live_sessions", &orphan).await, 1);

    // The first publication cannot be claimed by a replaying account.
    let first = host.share(&s, 1, restricted(&["bob@example.com"]));
    assert_eq!(publish(&eve, first.clone()).await.0, 403);
    assert_eq!(probe.count("publications", &s).await, 0);
    assert_eq!(probe.count("sessions", &s).await, 0);
    // Content cannot arrive before a share.
    assert_eq!(
        publish(&alice, host.append(&s, 1, 0, &[("m1", "early")]))
            .await
            .0,
        409
    );
    assert_eq!(probe.count("publications", &s).await, 0);

    // An unshare that arrives before its share was ever accepted is
    // acknowledged as deleted (the publisher's state machine converges), but
    // it records NOTHING: a tombstone exists only for content that actually
    // existed, so a stranger's unshare cannot claim an id and lock out its
    // real publisher. A delayed/replayed share of a never-shared id can still
    // land — replay protection for genuinely deleted content comes from the
    // real tombstone below.
    let early = uuid::Uuid::new_v4().to_string();
    let (status, ack) = publish(&alice, host.unshare(&early, 2)).await;
    assert_eq!(
        (status, ack["state"].clone(), ack["replayed"].clone()),
        (200, json!("deleted"), json!(false)),
        "{ack}"
    );
    assert_eq!(probe.count("publications", &early).await, 0);
    assert_eq!(probe.count("sessions", &early).await, 0);
    // A retry is answered the same way, still without any state.
    let (status, ack) = publish(&alice, host.unshare(&early, 2)).await;
    assert_eq!(
        (status, ack["state"].clone()),
        (200, json!("deleted")),
        "{ack}"
    );
    assert_eq!(probe.count("publications", &early).await, 0);
    // Even on an id that has unmanaged legacy data: acknowledged, nothing
    // created, legacy untouched.
    let (status, _) = publish(&alice, host.unshare(&legacy, 1)).await;
    assert_eq!(status, 200);
    assert_eq!(probe.count("publications", &legacy).await, 0);
    assert_eq!(
        message_texts(&app, &alice, &legacy).await.unwrap(),
        vec!["legacy-needle"]
    );

    // Squatting defence: a stranger's unshare of an id they never published
    // claims nothing; the real owner can still make the first share.
    let claimed = uuid::Uuid::new_v4().to_string();
    let squat_host = Host::new();
    let squat = squat_host.sign(
        &squat_host.op(&claimed, "eve@example.com", 7, json!({"type": "unshare"})),
        None,
    );
    let (status, ack) = publish(&eve, squat).await;
    assert_eq!(
        (status, ack["state"].clone()),
        (200, json!("deleted")),
        "{ack}"
    );
    assert_eq!(probe.count("publications", &claimed).await, 0);
    let (status, ack) = publish(&alice, host.share(&claimed, 3, restricted(&[]))).await;
    assert_eq!(
        (status, ack["state"].clone()),
        (200, json!("active")),
        "{ack}"
    );
    assert_eq!(
        probe.ledger(&claimed).await,
        (host.id(), Some(ALICE.into()), 3, "active".into(), 0)
    );

    // The delayed share after an acknowledged unshare is a normal first
    // share; the publisher's following unshare then creates a REAL tombstone,
    // which refuses the older replayed share and chunk.
    let (status, ack) = publish(&alice, host.share(&early, 1, restricted(&[]))).await;
    assert_eq!(
        (status, ack["state"].clone()),
        (200, json!("active")),
        "{ack}"
    );
    let (status, ack) = publish(&alice, host.unshare(&early, 2)).await;
    assert_eq!(
        (status, ack["state"].clone()),
        (200, json!("deleted")),
        "{ack}"
    );
    assert_eq!(
        publish(&alice, host.share(&early, 1, restricted(&[])))
            .await
            .0,
        409
    );
    assert_eq!(
        publish(&alice, host.append(&early, 1, 0, &[("e1", "late")]))
            .await
            .0,
        409
    );
    assert_eq!(probe.count("sessions", &early).await, 0);
    assert_eq!(
        probe.ledger(&early).await,
        (host.id(), Some(ALICE.into()), 2, "deleted".into(), 0)
    );

    let (status, ack) = publish(&alice, first.clone()).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(
        ack,
        json!({"session": s, "rev": 1, "state": "active", "next_seq": 0, "replayed": false})
    );
    assert_eq!(publish(&alice, first.clone()).await.1["replayed"], true);

    // Existing history.
    let existing = [
        (
            "m1",
            "existing-needle https://github.com/acme/widgets/pull/42",
        ),
        ("m2", "tracked in https://linear.app/acme/issue/ENG-12"),
    ];
    let (status, ack) = publish(&alice, host.append(&s, 1, 0, &existing)).await;
    assert_eq!((status, ack["next_seq"].clone()), (200, json!(1)), "{ack}");

    // The named reader can read and search it; another account cannot.
    assert_eq!(message_texts(&app, &bob, &s).await.unwrap().len(), 2);
    assert_eq!(search(&app, &bob, "existing-needle").await, vec![s.clone()]);
    let (_, prs) = request(
        &app,
        &bob,
        "GET",
        &format!("/api/sessions/{s}/prs"),
        Value::Null,
    )
    .await;
    assert!(
        prs.to_string()
            .contains("https://github.com/acme/widgets/pull/42"),
        "{prs}"
    );
    let (_, docs) = request(
        &app,
        &bob,
        "GET",
        &format!("/api/sessions/{s}/linear-docs"),
        Value::Null,
    )
    .await;
    assert!(docs.to_string().contains("ENG-12"), "{docs}");
    assert_eq!(message_texts(&app, &eve, &s).await, None);
    assert!(search(&app, &eve, "existing-needle").await.is_empty());

    // Future history: appended exactly once, ordered, idempotent.
    let future = host.append(&s, 1, 1, &[("m3", "future-needle")]);
    assert_eq!(publish(&alice, future.clone()).await.1["next_seq"], 2);
    let (status, ack) = publish(&alice, future).await;
    assert_eq!((status, ack["replayed"].clone()), (200, json!(true)));
    assert_eq!(
        publish(&alice, host.append(&s, 1, 1, &[("m3", "changed")]))
            .await
            .0,
        409
    );
    assert_eq!(
        publish(&alice, host.append(&s, 1, 3, &[("m9", "hole")]))
            .await
            .0,
        409
    );
    // A sequence whose next cursor would not fit the column is malformed.
    let overflow = host.append(&s, 1, i64::MAX as u64, &[("m9", "overflow")]);
    assert_eq!(publish(&alice, overflow).await.0, 400);
    assert_eq!(probe.ledger(&s).await.4, 2);
    assert_eq!(
        message_texts(&app, &bob, &s).await.unwrap()[2..],
        ["future-needle".to_owned()]
    );

    // A failing chunk leaves no content and does not advance the cursor.
    let failing = host.append(&s, 1, 2, &[("m4", "atomic-needle"), ("m4", "duplicate")]);
    assert_eq!(publish(&alice, failing).await.0, 409);
    assert_eq!(probe.count("messages", &s).await, 3);
    assert_eq!(probe.count("publication_chunks", &s).await, 2);
    assert_eq!(probe.ledger(&s).await.4, 2);
    assert!(search(&app, &alice, "atomic-needle").await.is_empty());
    assert_eq!(
        publish(&alice, host.append(&s, 1, 2, &[("m4", "atomic-needle")]))
            .await
            .1["next_seq"],
        3
    );

    // The canonical history updates a message in place under its uuid: a newer
    // chunk replaces its text and links at the same position without adding a
    // row. A uuid used by another session is a different message.
    let update = [
        (
            "m1",
            "updated-needle https://github.com/acme/widgets/pull/77",
        ),
        ("legacy-1", "own-copy"),
    ];
    let (status, ack) = publish(&alice, host.append(&s, 1, 3, &update)).await;
    assert_eq!((status, ack["next_seq"].clone()), (200, json!(4)), "{ack}");
    let texts = message_texts(&app, &bob, &s).await.unwrap();
    assert_eq!(
        texts,
        [
            update[0].1,
            existing[1].1,
            "future-needle",
            "atomic-needle",
            "own-copy"
        ]
    );
    assert!(search(&app, &bob, "existing-needle").await.is_empty());
    assert_eq!(search(&app, &bob, "updated-needle").await, vec![s.clone()]);
    let (_, listed) = request(
        &app,
        &bob,
        "GET",
        "/api/sessions?q=updated-needle",
        Value::Null,
    )
    .await;
    assert_eq!(listed["sessions"][0]["message_count"], 5);
    let (_, prs) = request(
        &app,
        &bob,
        "GET",
        &format!("/api/sessions/{s}/prs"),
        Value::Null,
    )
    .await;
    assert!(prs.to_string().contains("acme/widgets/pull/77"), "{prs}");
    assert!(!prs.to_string().contains("acme/widgets/pull/42"), "{prs}");
    let (_, docs) = request(
        &app,
        &bob,
        "GET",
        &format!("/api/sessions/{s}/linear-docs"),
        Value::Null,
    )
    .await;
    assert!(docs.to_string().contains("ENG-12"), "{docs}");
    assert_eq!(
        message_texts(&app, &alice, &legacy).await.unwrap(),
        vec!["legacy-needle"]
    );
    // Replaying older chunks is a no-op; different bytes at an old seq conflict.
    for old in [
        host.append(&s, 1, 0, &existing),
        host.append(&s, 1, 1, &[("m3", "future-needle")]),
    ] {
        assert_eq!(publish(&alice, old).await.1["replayed"], true);
    }
    assert_eq!(
        publish(&alice, host.append(&s, 1, 0, &[("m1", "revert")]))
            .await
            .0,
        409
    );
    assert_eq!(message_texts(&app, &bob, &s).await.unwrap(), texts);
    assert!(search(&app, &bob, "existing-needle").await.is_empty());

    // Neither another host nor another account can write or delete.
    let impostor = other_host.sign(
        &other_host.op(&s, ALICE, 5, json!({"type": "unshare"})),
        None,
    );
    assert_eq!(publish(&alice, impostor).await.0, 403);
    let borrowed = other_host.sign(&host.op(&s, ALICE, 5, json!({"type": "unshare"})), None);
    assert_eq!(publish(&alice, borrowed).await.0, 403);
    let eve_op = host.sign(
        &host.op(&s, "eve@example.com", 5, json!({"type": "unshare"})),
        None,
    );
    assert_eq!(publish(&eve, eve_op).await.0, 403);
    let signed_unshare = host.unshare(&s, 2);
    assert_eq!(publish(&eve, signed_unshare.clone()).await.0, 403);
    assert_eq!(publish(&bob, signed_unshare.clone()).await.0, 403);
    let mut tampered = host.unshare(&s, 2);
    tampered["op"] = json!(
        B64.encode(serde_json::to_vec(&host.op(&s, ALICE, 9, json!({"type": "unshare"}))).unwrap())
    );
    assert_eq!(publish(&alice, tampered).await.0, 403);
    assert_eq!(probe.count("messages", &s).await, 5);
    assert_eq!(
        probe.ledger(&s).await,
        (host.id(), Some(ALICE.into()), 1, "active".into(), 4)
    );

    // Legacy routes cannot touch a managed session.
    assert_eq!(
        request(
            &app,
            &alice,
            "POST",
            "/api/sessions",
            upload(&s, "overwrite")
        )
        .await
        .0,
        409
    );
    let organization = json!({"visibility": "organization"});
    assert_eq!(
        request(
            &app,
            &alice,
            "PUT",
            &format!("/api/sessions/{s}/sharing"),
            organization
        )
        .await
        .0,
        409
    );
    assert_eq!(
        request(
            &app,
            &alice,
            "DELETE",
            &format!("/api/sessions/{s}"),
            Value::Null
        )
        .await
        .0,
        409
    );
    // The legacy registration route is not even mounted (405).
    assert_eq!(
        request(&app, &alice, "POST", "/api/live", register(&s))
            .await
            .0,
        405
    );
    assert_eq!(message_texts(&app, &bob, &s).await.unwrap().len(), 5);
    assert_eq!(message_texts(&app, &eve, &s).await, None);
    assert_eq!(probe.count("live_sessions", &s).await, 0);

    // A reader's attention request is persisted with the published content.
    let attention = json!({"recipient_email": ALICE, "message": "please look"});
    let path = format!("/api/sessions/{s}/attention");
    assert_eq!(request(&app, &bob, "POST", &path, attention).await.0, 200);
    assert_eq!(probe.count("session_notifications", &s).await, 1);
    // Revision-less derived writes fail closed for a managed session.
    derived_writes_refused(&app, &alice, &repo, &probe, &s, project).await;

    // Unshare removes the central copy and everything derived from it.
    let (status, ack) = publish(&alice, signed_unshare.clone()).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(
        ack,
        json!({"session": s, "rev": 2, "state": "deleted", "next_seq": 4, "replayed": false})
    );
    for table in CONTENT_TABLES {
        assert_eq!(probe.count(table, &s).await, 0, "{table} still holds {s}");
    }
    assert_eq!(
        probe.ledger(&s).await,
        (host.id(), Some(ALICE.into()), 2, "deleted".into(), 4)
    );
    for cookie in [&alice, &bob] {
        assert_eq!(message_texts(&app, cookie, &s).await, None);
        assert!(search(&app, cookie, "existing-needle").await.is_empty());
        assert!(search(&app, cookie, "future-needle").await.is_empty());
    }
    let (_, prs) = request(
        &app,
        &bob,
        "GET",
        &format!("/api/sessions/{s}/prs"),
        Value::Null,
    )
    .await;
    assert!(!prs.to_string().contains("acme/widgets"), "{prs}");
    assert_eq!(publish(&alice, signed_unshare).await.1["replayed"], true);

    // Stale operations cannot resurrect it, through any route.
    for stale in [
        first,
        host.append(&s, 1, 0, &existing),
        host.append(&s, 2, 0, &existing),
        host.share(&s, 2, restricted(&["bob@example.com"])),
    ] {
        assert_eq!(publish(&alice, stale).await.0, 409);
    }
    assert_eq!(
        request(
            &app,
            &alice,
            "POST",
            "/api/sessions",
            upload(&s, "resurrect")
        )
        .await
        .0,
        409
    );
    // The legacy registration route is not even mounted (405).
    assert_eq!(
        request(&app, &alice, "POST", "/api/live", register(&s))
            .await
            .0,
        405
    );
    let link = json!({"url": "https://github.com/acme/widgets/pull/43"});
    assert_ne!(
        request(
            &app,
            &alice,
            "POST",
            &format!("/api/sessions/{s}/prs"),
            link
        )
        .await
        .0,
        200
    );
    let foreign = other_host.sign(
        &other_host.op(
            &s,
            ALICE,
            7,
            json!({"type": "share", "audience": restricted(&[]), "agent": "claude", "title": "x"}),
        ),
        None,
    );
    assert_eq!(publish(&alice, foreign).await.0, 403);
    let eve_share = host.sign(
        &host.op(
            &s,
            "eve@example.com",
            7,
            json!({"type": "share", "audience": restricted(&[]), "agent": "claude", "title": "x"}),
        ),
        None,
    );
    assert_eq!(publish(&eve, eve_share).await.0, 403);
    for table in CONTENT_TABLES {
        assert_eq!(probe.count(table, &s).await, 0, "{table} resurrected {s}");
    }
    assert_eq!(probe.count("live_sessions", &s).await, 0);
    assert_eq!(
        probe.ledger(&s).await,
        (host.id(), Some(ALICE.into()), 2, "deleted".into(), 4)
    );

    // A derived write delayed across the tombstone cannot land either.
    derived_writes_refused(&app, &alice, &repo, &probe, &s, project).await;

    // Re-share at a strictly newer revision, now organization-wide.
    assert_eq!(
        publish(
            &alice,
            host.share(&s, 3, json!({"visibility": "organization"}))
        )
        .await
        .1["next_seq"],
        0
    );
    assert_eq!(
        publish(&alice, host.append(&s, 3, 0, &[("m5", "reshared-needle")]))
            .await
            .1["next_seq"],
        1
    );
    let carol = login(&app, "carol@example.com").await;
    for cookie in [&carol, &eve] {
        assert_eq!(
            message_texts(&app, cookie, &s).await.unwrap(),
            vec!["reshared-needle"]
        );
    }
    assert!(search(&app, &carol, "existing-needle").await.is_empty());
    // ...nor in the newer share's projection.
    derived_writes_refused(&app, &alice, &repo, &probe, &s, project).await;
    assert_eq!(
        publish(&alice, host.share(&s, 2, restricted(&[]))).await.0,
        409
    );

    // The legacy data was never adopted or deleted.
    assert_eq!(
        message_texts(&app, &alice, &legacy).await.unwrap(),
        vec!["legacy-needle"]
    );
    assert_eq!(probe.count("publications", &legacy).await, 0);
    assert_eq!(probe.count("live_sessions", &live).await, 1);
}

/// A synthetic OpenRouter. Each completion request is handed to the test with
/// the prompt it carried, after the server has read its source context, and
/// is answered only when the test releases it: a deterministic barrier between
/// a derived job's context read and its final write.
struct Llm {
    url: String,
    calls: tokio::sync::mpsc::UnboundedReceiver<LlmCall>,
}

struct LlmCall {
    prompt: String,
    reply: tokio::sync::oneshot::Sender<String>,
}

impl LlmCall {
    fn answer(self, text: &str) {
        self.reply.send(text.to_owned()).unwrap();
    }
}

impl Llm {
    async fn start() -> Self {
        use axum::response::IntoResponse;
        let (tx, calls) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().route(
            "/chat/completions",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let tx = tx.clone();
                async move {
                    let (reply, answer) = tokio::sync::oneshot::channel();
                    let prompt = body["messages"].to_string();
                    tx.send(LlmCall { prompt, reply }).unwrap();
                    let text: String = answer.await.unwrap();
                    if body["stream"] == true {
                        let chunk = json!({"choices": [{"delta": {"content": text}}]});
                        (
                            [("content-type", "text/event-stream")],
                            format!("data: {chunk}\n\ndata: [DONE]\n\n"),
                        )
                            .into_response()
                    } else {
                        axum::Json(json!({"choices": [{"message": {"content": text}}]}))
                            .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, calls }
    }

    async fn next(&mut self) -> LlmCall {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.calls.recv())
            .await
            .expect("no model request arrived")
            .unwrap()
    }

    /// No request reached the model.
    fn idle(&mut self) -> bool {
        self.calls.try_recv().is_err()
    }
}

fn summary_reply(intent: &str) -> String {
    json!({"intent": intent, "invariants": [], "summary": "recap"}).to_string()
}

/// Rows of `table` whose `column` is `value`.
async fn count_where(probe: &Probe, table: &str, column: &str, value: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} = ");
    match probe {
        Probe::Sqlite(pool) => sqlx::query_scalar(&format!("{sql}?"))
            .bind(value)
            .fetch_one(pool)
            .await
            .unwrap(),
        Probe::Postgres(pool) => sqlx::query_scalar(&format!("{sql}$1"))
            .bind(value)
            .fetch_one(pool)
            .await
            .unwrap(),
    }
}

/// The PR comment cache revision counter of a session, if any.
async fn comment_revision(probe: &Probe, session: &str) -> Option<i64> {
    let sql = "SELECT revision FROM session_pr_comment_revisions WHERE session_id = ";
    match probe {
        Probe::Sqlite(pool) => sqlx::query_scalar(&format!("{sql}?"))
            .bind(session)
            .fetch_optional(pool)
            .await
            .unwrap(),
        Probe::Postgres(pool) => sqlx::query_scalar(&format!("{sql}$1"))
            .bind(session)
            .fetch_optional(pool)
            .await
            .unwrap(),
    }
}

/// Every table a derived feature writes for a session.
const DERIVED_TABLES: [&str; 6] = [
    "session_summaries",
    "chats",
    "session_prs",
    "session_linear_docs",
    "session_pr_comments",
    "session_pr_comment_revisions",
];

/// Published summaries, session chat and PR caches, each pinned to the
/// publication snapshot `(rev, next_seq)` read with its source context.
async fn derived_features(repo: Arc<dyn Repository>, probe: Probe) {
    let mut llm = Llm::start().await;
    let config = Config {
        auth_enabled: true,
        password_auth_enabled: true,
        cookie_secure: false,
        openrouter_api_key: Some("synthetic".into()),
        openrouter_base_url: llm.url.clone(),
        ..Config::default()
    };
    let app = build_router_with_auth(AppState::new(repo.clone(), Arc::new(config)))
        .await
        .unwrap();
    let alice = login(&app, ALICE).await;
    let bob = login(&app, "bob@example.com").await;
    let eve = login(&app, "eve@example.com").await;
    let host = Host::new();
    let publish = |body: Value| {
        let app = app.clone();
        let cookie = alice.clone();
        async move {
            let (status, ack) = request(&app, &cookie, "POST", "/api/publications", body).await;
            assert_eq!(status, 200, "{ack}");
            ack
        }
    };
    // A request held by the model fixture runs concurrently with the test.
    let spawn = |cookie: &str, path: String, body: Value| {
        let app = app.clone();
        let cookie = cookie.to_owned();
        tokio::spawn(async move { request(&app, &cookie, "POST", &path, body).await })
    };
    let summarize =
        |cookie: &str, id: &str| spawn(cookie, format!("/api/sessions/{id}/summary"), Value::Null);
    // The chat reply is an SSE body: return it raw.
    let ask = |cookie: &str, chat: &str, content: &str| {
        let app = app.clone();
        let cookie = cookie.to_owned();
        let path = format!("/api/chats/{chat}/messages");
        let body = json!({"content": content}).to_string();
        tokio::spawn(async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .header("cookie", cookie)
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status().as_u16();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        })
    };
    let get = |cookie: &str, path: String| {
        let app = app.clone();
        let cookie = cookie.to_owned();
        async move { request(&app, &cookie, "GET", &path, Value::Null).await }
    };
    let post = |cookie: &str, path: String, body: Value| {
        let app = app.clone();
        let cookie = cookie.to_owned();
        async move { request(&app, &cookie, "POST", &path, body).await.0 }
    };
    let status_body = |number: i64, title: &str, publication: &Value| {
        let mut body = json!({"owner": "acme", "repo": "widgets", "number": number,
            "status": {"state": "open", "merged": false, "draft": false, "title": title}});
        if !publication.is_null() {
            body["publication"] = publication.clone();
        }
        body
    };
    let comments_body = |number: i64, text: &str, publication: &Value| {
        let mut body = json!({"owner": "acme", "repo": "widgets", "number": number,
        "comments": [
            {"comment_id": format!("{text}-1"), "kind": "issue", "author": "octo", "body": text,
             "html_url": "https://github.com/acme/widgets/pull/1", "created_at": "2026-11-03T00:00:00Z"},
            {"comment_id": format!("{text}-2"), "kind": "issue", "author": "octo", "body": text,
             "html_url": "https://github.com/acme/widgets/pull/1", "created_at": "2026-11-03T00:00:01Z"}
        ]});
        if !publication.is_null() {
            body["publication"] = publication.clone();
        }
        body
    };
    let chat_texts = |cookie: &str, chat: &str| {
        let get = get(cookie, format!("/api/chats/{chat}"));
        async move {
            let (status, body) = get.await;
            (status == 200).then(|| {
                body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["content"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
        }
    };

    let s = uuid::Uuid::new_v4().to_string();
    let base = format!("/api/sessions/{s}");
    publish(host.share(&s, 1, restricted(&["bob@example.com"]))).await;
    publish(host.append(
        &s,
        1,
        0,
        &[("m1", "alpha-needle https://github.com/acme/widgets/pull/42")],
    ))
    .await;

    // Same revision: the owner and a named reader use the derived features.
    let job = summarize(&alice, &s);
    let call = llm.next().await;
    assert!(call.prompt.contains("alpha-needle"), "{}", call.prompt);
    call.answer(&summary_reply("summary-needle"));
    let (status, body) = job.await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        get(&bob, base.clone()).await.1["summary"]["intent"],
        "summary-needle"
    );
    let (status, chat) = request(
        &app,
        &bob,
        "POST",
        &format!("{base}/chats"),
        json!({"title": "q"}),
    )
    .await;
    assert_eq!(status, 201, "{chat}");
    let chat = chat["id"].as_str().unwrap().to_owned();
    let job = ask(&bob, &chat, "what changed?");
    let call = llm.next().await;
    assert!(call.prompt.contains("alpha-needle"), "{}", call.prompt);
    call.answer("chat-needle");
    let (status, sse) = job.await.unwrap();
    assert!(status == 200 && sse.contains(r#""type":"done""#), "{sse}");
    assert_eq!(
        chat_texts(&bob, &chat).await.unwrap(),
        ["what changed?", "chat-needle"]
    );
    let (_, prs) = get(&bob, format!("{base}/prs")).await;
    let snapshot = prs["publication"].clone();
    assert_eq!(snapshot, json!({"rev": 1, "next_seq": 1}), "{prs}");
    assert_eq!(
        post(
            &bob,
            format!("{base}/prs/status"),
            status_body(42, "first", &snapshot)
        )
        .await,
        204
    );
    assert_eq!(
        post(
            &bob,
            format!("{base}/prs/comments"),
            comments_body(42, "comment-needle", &snapshot)
        )
        .await,
        204
    );
    let (_, prs) = get(&bob, format!("{base}/prs")).await;
    assert_eq!(prs["prs"][0]["status"]["title"], "first", "{prs}");
    let (_, comments) = get(&bob, format!("{base}/prs/comments")).await;
    assert_eq!(
        comments["comments"].as_array().unwrap().len(),
        2,
        "{comments}"
    );
    // Revision-less pushes stay refused for a managed session.
    assert_eq!(
        post(
            &bob,
            format!("{base}/prs/status"),
            status_body(42, "revisionless", &Value::Null)
        )
        .await,
        409
    );
    assert_eq!(
        post(
            &bob,
            format!("{base}/prs/comments"),
            comments_body(42, "revisionless", &Value::Null)
        )
        .await,
        409
    );

    // A denied reader reaches neither the context nor the results, and no
    // request of theirs reaches the model.
    assert_eq!(get(&eve, base.clone()).await.0, 404);
    assert_eq!(summarize(&eve, &s).await.unwrap().0, 404);
    assert_eq!(post(&eve, format!("{base}/chats"), json!({})).await, 404);
    assert_ne!(ask(&eve, &chat, "leak?").await.unwrap().0, 200);
    assert_eq!(chat_texts(&eve, &chat).await, None);
    assert_eq!(get(&eve, format!("{base}/prs")).await.0, 404);
    assert_eq!(get(&eve, format!("{base}/prs/comments")).await.0, 404);
    assert_eq!(
        post(
            &eve,
            format!("{base}/prs/status"),
            status_body(42, "eve", &snapshot)
        )
        .await,
        404
    );
    assert!(llm.idle());
    assert_eq!(chat_texts(&bob, &chat).await.unwrap().len(), 2);

    // The same revision moves on while a job is held: a newer chunk updates
    // m1 in place (dropping its PR link). The old snapshot no longer matches.
    let job = summarize(&alice, &s);
    let held = llm.next().await;
    publish(host.append(&s, 1, 1, &[("m1", "beta-needle")])).await;
    held.answer(&summary_reply("stale-summary-needle"));
    assert_eq!(job.await.unwrap().0, 409);
    assert_eq!(
        get(&bob, base.clone()).await.1["summary"]["intent"],
        "summary-needle"
    );
    let job = ask(&bob, &chat, "and now?");
    let held = llm.next().await;
    assert!(held.prompt.contains("beta-needle"), "{}", held.prompt);
    publish(host.append(
        &s,
        1,
        2,
        &[("m2", "gamma-needle https://github.com/acme/widgets/pull/77")],
    ))
    .await;
    held.answer("stale-chat-needle");
    let (_, sse) = job.await.unwrap();
    assert!(sse.contains(r#""type":"error""#), "{sse}");
    let texts = chat_texts(&bob, &chat).await.unwrap();
    assert!(!texts.iter().any(|t| t == "stale-chat-needle"), "{texts:?}");
    let revision = comment_revision(&probe, &s).await;
    assert_eq!(
        post(
            &bob,
            format!("{base}/prs/status"),
            status_body(77, "stale", &snapshot)
        )
        .await,
        409
    );
    assert_eq!(
        post(
            &bob,
            format!("{base}/prs/comments"),
            comments_body(77, "stale-comment", &snapshot)
        )
        .await,
        409
    );
    // The refused write left nothing behind, not even a counter bump.
    assert_eq!(comment_revision(&probe, &s).await, revision);
    assert_eq!(
        count_where(&probe, "session_pr_comments", "session_id", &s).await,
        2
    );
    let (_, prs) = get(&bob, format!("{base}/prs")).await;
    let current = prs["publication"].clone();
    assert_eq!(current, json!({"rev": 1, "next_seq": 3}), "{prs}");
    assert!(!prs.to_string().contains("pull/42"), "{prs}");
    assert_eq!(prs["prs"][0]["status"], Value::Null, "{prs}");
    assert_eq!(
        post(
            &bob,
            format!("{base}/prs/status"),
            status_body(77, "second", &current)
        )
        .await,
        204
    );
    // A PR the published history does not link has no cache.
    assert_eq!(
        post(
            &bob,
            format!("{base}/prs/comments"),
            comments_body(42, "unlinked", &current)
        )
        .await,
        404
    );

    // Unshare and re-share at a newer revision between an old job's context
    // read and its write.
    let summary_job = summarize(&alice, &s);
    let summary_call = llm.next().await;
    let chat_job = ask(&bob, &chat, "late?");
    let chat_call = llm.next().await;
    publish(host.unshare(&s, 2)).await;
    for table in CONTENT_TABLES.iter().chain(&DERIVED_TABLES) {
        assert_eq!(probe.count(table, &s).await, 0, "{table} still holds {s}");
    }
    assert_eq!(
        count_where(&probe, "chat_messages", "chat_id", &chat).await,
        0
    );
    publish(host.share(&s, 3, json!({"visibility": "organization"}))).await;
    publish(host.append(
        &s,
        3,
        0,
        &[("m5", "delta-needle https://github.com/acme/widgets/pull/77")],
    ))
    .await;
    // The new projection reaches the old snapshot's cursor: only `rev` differs.
    publish(host.append(&s, 3, 1, &[("m6", "epsilon")])).await;
    publish(host.append(&s, 3, 2, &[("m7", "zeta")])).await;
    summary_call.answer(&summary_reply("stale-reshare-needle"));
    chat_call.answer("stale-reshare-chat");
    assert_eq!(summary_job.await.unwrap().0, 409);
    let (_, sse) = chat_job.await.unwrap();
    assert!(!sse.contains(r#""type":"done""#), "{sse}");
    for (path, body) in [
        (
            format!("{base}/prs/status"),
            status_body(77, "stale", &current),
        ),
        (
            format!("{base}/prs/comments"),
            comments_body(77, "stale", &current),
        ),
        (
            format!("{base}/prs/status"),
            status_body(77, "stale", &Value::Null),
        ),
        (
            format!("{base}/prs/comments"),
            comments_body(77, "stale", &Value::Null),
        ),
    ] {
        assert_eq!(post(&alice, path, body).await, 409);
    }
    for table in [
        "session_summaries",
        "chats",
        "session_pr_comments",
        "session_pr_comment_revisions",
    ] {
        assert_eq!(
            probe.count(table, &s).await,
            0,
            "{table} took a stale write"
        );
    }
    assert_eq!(
        count_where(&probe, "chat_messages", "chat_id", &chat).await,
        0
    );
    let carol = login(&app, "carol@example.com").await;
    assert!(search(&app, &carol, "stale-reshare-needle")
        .await
        .is_empty());
    let detail = get(&carol, base.clone()).await.1;
    assert_eq!(detail["summary"], Value::Null, "{detail}");
    // A job at the new revision works.
    let (_, prs) = get(&carol, format!("{base}/prs")).await;
    let fresh = prs["publication"].clone();
    assert_eq!(fresh, json!({"rev": 3, "next_seq": 3}), "{prs}");
    assert_eq!(prs["prs"][0]["status"], Value::Null, "{prs}");
    assert_eq!(
        post(
            &carol,
            format!("{base}/prs/status"),
            status_body(77, "fresh", &fresh)
        )
        .await,
        204
    );
    let job = summarize(&carol, &s);
    let call = llm.next().await;
    assert!(call.prompt.contains("delta-needle"), "{}", call.prompt);
    assert!(!call.prompt.contains("beta-needle"), "{}", call.prompt);
    call.answer(&summary_reply("fresh-needle"));
    assert_eq!(job.await.unwrap().0, 200);
    assert_eq!(
        get(&carol, base.clone()).await.1["summary"]["intent"],
        "fresh-needle"
    );

    // After unshare nothing derived survives, and no stale or revision-less
    // write passes the tombstone.
    publish(host.unshare(&s, 4)).await;
    for table in CONTENT_TABLES.iter().chain(&DERIVED_TABLES) {
        assert_eq!(probe.count(table, &s).await, 0, "{table} still holds {s}");
    }
    assert_eq!(summarize(&alice, &s).await.unwrap().0, 404);
    assert_eq!(post(&alice, format!("{base}/chats"), json!({})).await, 404);
    for publication in [&fresh, &Value::Null] {
        assert_ne!(
            post(
                &alice,
                format!("{base}/prs/status"),
                status_body(77, "late", publication)
            )
            .await,
            204
        );
        assert_ne!(
            post(
                &alice,
                format!("{base}/prs/comments"),
                comments_body(77, "late", publication)
            )
            .await,
            204
        );
    }
    assert!(llm.idle());
    for table in CONTENT_TABLES.iter().chain(&DERIVED_TABLES) {
        assert_eq!(probe.count(table, &s).await, 0, "{table} resurrected {s}");
    }

    // Unmanaged history keeps its existing behavior, without any snapshot.
    let legacy = uuid::Uuid::new_v4().to_string();
    let base = format!("/api/sessions/{legacy}");
    let session = parse(
        &legacy,
        &[("l1", "legacy-needle https://github.com/acme/widgets/pull/9")],
    );
    assert_eq!(
        post(
            &alice,
            "/api/sessions".into(),
            json!({"uploaded_by": ALICE, "source_host": null, "session": session})
        )
        .await,
        201
    );
    let job = summarize(&alice, &legacy);
    let call = llm.next().await;
    assert!(call.prompt.contains("legacy-needle"), "{}", call.prompt);
    call.answer(&summary_reply("legacy-summary"));
    assert_eq!(job.await.unwrap().0, 200);
    assert_eq!(
        get(&alice, base.clone()).await.1["summary"]["intent"],
        "legacy-summary"
    );
    let (status, chat) = request(&app, &alice, "POST", &format!("{base}/chats"), json!({})).await;
    assert_eq!(status, 201, "{chat}");
    let chat = chat["id"].as_str().unwrap().to_owned();
    let job = ask(&alice, &chat, "legacy?");
    llm.next().await.answer("legacy-chat");
    assert!(job.await.unwrap().1.contains(r#""type":"done""#));
    assert_eq!(
        chat_texts(&alice, &chat).await.unwrap(),
        ["legacy?", "legacy-chat"]
    );
    let (_, prs) = get(&alice, format!("{base}/prs")).await;
    assert_eq!(prs["publication"], Value::Null, "{prs}");
    assert!(prs.to_string().contains("pull/9"), "{prs}");
    assert_eq!(
        post(
            &alice,
            format!("{base}/prs/status"),
            status_body(9, "legacy", &Value::Null)
        )
        .await,
        204
    );
    assert_eq!(
        post(
            &alice,
            format!("{base}/prs/comments"),
            comments_body(9, "legacy", &Value::Null)
        )
        .await,
        204
    );
    // A snapshot cannot make an unmanaged session look managed.
    let revision = comment_revision(&probe, &legacy).await;
    assert_eq!(
        post(
            &alice,
            format!("{base}/prs/status"),
            status_body(9, "claimed", &fresh)
        )
        .await,
        409
    );
    assert_eq!(
        post(
            &alice,
            format!("{base}/prs/comments"),
            comments_body(9, "claimed", &fresh)
        )
        .await,
        409
    );
    assert_eq!(comment_revision(&probe, &legacy).await, revision);
    let (_, prs) = get(&alice, format!("{base}/prs")).await;
    assert_eq!(prs["prs"][0]["status"]["title"], "legacy", "{prs}");
    assert_eq!(
        count_where(&probe, "session_pr_comments", "session_id", &legacy).await,
        2
    );
    assert!(llm.idle());
}

/// A publishing host can name an
/// audience member as intended to type into the session through it once their
/// key is trusted. Until then that member reads the published copy like any
/// other sharee — the session tells THEM so (and nobody else), and an intent
/// naming a non-reader is refused.
async fn write_pending_key(repo: Arc<dyn Repository>, probe: Probe) {
    let config = Config {
        auth_enabled: true,
        password_auth_enabled: true,
        cookie_secure: false,
        ..Config::default()
    };
    let app = build_router_with_auth(AppState::new(repo, Arc::new(config)))
        .await
        .unwrap();
    let alice = login(&app, ALICE).await;
    let bob = login(&app, "bob@example.com").await;
    let carol = login(&app, "carol@example.com").await;
    let host = Host::new();
    let s = uuid::Uuid::new_v4().to_string();
    let publish = |body: Value| {
        let app = app.clone();
        let cookie = alice.clone();
        async move { request(&app, &cookie, "POST", "/api/publications", body).await }
    };

    // Share with two readers; Bob is also named as a pending writer.
    let audience = json!({
        "visibility": "restricted",
        "emails": ["bob@example.com", "carol@example.com"],
        "write_pending": ["bob@example.com"],
    });
    assert_eq!(publish(host.share(&s, 1, audience)).await.0, 200);
    assert_eq!(
        publish(host.append(&s, 1, 0, &[("m1", "published line")]))
            .await
            .0,
        200
    );

    // The intent is persisted on exactly Bob's share row (boundary proof).
    assert_eq!(
        probe.share_flag(&s, "bob@example.com").await,
        1,
        "bob's row carries the pending-write intent"
    );
    assert_eq!(
        probe.share_flag(&s, "carol@example.com").await,
        0,
        "carol is an ordinary reader"
    );

    // The pending writer is told; the plain reader and the owner are not.
    let (status, bob_body) = request(
        &app,
        &bob,
        "GET",
        &format!("/api/sessions/{s}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(bob_body["viewer_write_pending_key"], json!(true));
    let (_, carol_body) = request(
        &app,
        &carol,
        "GET",
        &format!("/api/sessions/{s}"),
        Value::Null,
    )
    .await;
    assert_eq!(carol_body["viewer_write_pending_key"], json!(false));
    let (_, alice_body) = request(
        &app,
        &alice,
        "GET",
        &format!("/api/sessions/{s}"),
        Value::Null,
    )
    .await;
    assert_eq!(alice_body["viewer_write_pending_key"], json!(false));

    // A pending-write intent outside the audience is refused and changes
    // nothing: the ledger stays at revision 1.
    let bad = json!({
        "visibility": "restricted",
        "emails": ["bob@example.com"],
        "write_pending": ["eve@example.com"],
    });
    let (status, _) = publish(host.share(&s, 2, bad)).await;
    assert_eq!(status, 400, "intent outside the audience must be refused");
    let (_, bob_body) = request(
        &app,
        &bob,
        "GET",
        &format!("/api/sessions/{s}"),
        Value::Null,
    )
    .await;
    assert_eq!(bob_body["viewer_write_pending_key"], json!(true));
    assert_eq!(probe.ledger(&s).await.3, "active");
}

#[tokio::test]
async fn write_pending_key_sqlite() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/publication-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.sqlite", uuid::Uuid::new_v4()));
    let repo = SqliteRepo::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    write_pending_key(Arc::new(repo.clone()), Probe::Sqlite(repo.pool().clone())).await;
    repo.pool().close().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable empty Postgres database in PUBLICATION_TEST_DATABASE_URL"]
async fn write_pending_key_postgres() {
    let (repo, probe) = postgres().await;
    write_pending_key(repo, probe).await;
}

#[tokio::test]
async fn host_signed_publication_sqlite() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/publication-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.sqlite", uuid::Uuid::new_v4()));
    let repo = SqliteRepo::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    exercise(Arc::new(repo.clone()), Probe::Sqlite(repo.pool().clone())).await;
    repo.pool().close().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn published_derived_features_sqlite() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/publication-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.sqlite", uuid::Uuid::new_v4()));
    let repo = SqliteRepo::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    derived_features(Arc::new(repo.clone()), Probe::Sqlite(repo.pool().clone())).await;
    repo.pool().close().await;
    std::fs::remove_file(path).unwrap();
}

/// A repository in a fresh schema of the disposable database named by
/// `PUBLICATION_TEST_DATABASE_URL`, plus a probe pool on the same schema.
async fn postgres() -> (Arc<dyn Repository>, Probe) {
    let url = std::env::var("PUBLICATION_TEST_DATABASE_URL").unwrap();
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("publication_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    let url = format!("{url}?options=-csearch_path%3D{schema}");
    let repo = conversations_server::store::PostgresRepo::connect(&url)
        .await
        .unwrap();
    let probe = sqlx::PgPool::connect(&url).await.unwrap();
    (Arc::new(repo), Probe::Postgres(probe))
}

#[tokio::test]
#[ignore = "requires a disposable empty Postgres database in PUBLICATION_TEST_DATABASE_URL"]
async fn host_signed_publication_postgres() {
    let (repo, probe) = postgres().await;
    exercise(repo, probe).await;
}

#[tokio::test]
#[ignore = "requires a disposable empty Postgres database in PUBLICATION_TEST_DATABASE_URL"]
async fn published_derived_features_postgres() {
    let (repo, probe) = postgres().await;
    derived_features(repo, probe).await;
}

/// A reader learns from the session itself whether it is a managed
/// publication, so the UI never offers the legacy sharing writer for it. Only
/// the owner learns which host publishes it.
async fn managed_marker(repo: Arc<dyn Repository>) {
    let config = Config {
        auth_enabled: true,
        password_auth_enabled: true,
        cookie_secure: false,
        ..Config::default()
    };
    let app = build_router_with_auth(AppState::new(repo, Arc::new(config)))
        .await
        .unwrap();
    let alice = login(&app, ALICE).await;
    let bob = login(&app, "bob@example.com").await;
    let host = Host::new();
    let s = uuid::Uuid::new_v4().to_string();
    let legacy = uuid::Uuid::new_v4().to_string();
    let upload = json!({"uploaded_by": ALICE, "source_host": null,
        "session": parse(&legacy, &[("legacy-1", "legacy")])});
    assert_eq!(
        request(&app, &alice, "POST", "/api/sessions", upload)
            .await
            .0,
        201
    );
    let publish = |body: Value| request(&app, &alice, "POST", "/api/publications", body);
    assert_eq!(
        publish(host.share(&s, 2, restricted(&["bob@example.com"])))
            .await
            .0,
        200
    );
    assert_eq!(
        publish(host.append(&s, 2, 0, &[("m1", "hello")])).await.0,
        200
    );

    let (status, owner_view) = request(
        &app,
        &alice,
        "GET",
        &format!("/api/sessions/{s}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{owner_view}");
    assert_eq!(
        owner_view["publication"],
        json!({"state": "active", "rev": 2, "host_id": host.id()})
    );
    let (status, reader_view) = request(
        &app,
        &bob,
        "GET",
        &format!("/api/sessions/{s}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{reader_view}");
    assert_eq!(
        reader_view["publication"],
        json!({"state": "active", "rev": 2})
    );

    let (_, legacy_view) = request(
        &app,
        &alice,
        "GET",
        &format!("/api/sessions/{legacy}"),
        Value::Null,
    )
    .await;
    assert_eq!(legacy_view["publication"], Value::Null);
    assert_eq!(legacy_view["messages"][0]["text"], "legacy");

    // The legacy sharing writer still refuses the managed session.
    let (status, _) = request(
        &app,
        &alice,
        "PUT",
        &format!("/api/sessions/{s}/sharing"),
        json!({"visibility": "organization", "emails": []}),
    )
    .await;
    assert_eq!(status, 409);
}

/// With auth off there is no owner: every reader learns the publishing host
/// (so a browser that paired it can offer the signed controls), and the
/// legacy sharing writer still refuses the managed session.
async fn managed_marker_auth_off(repo: Arc<dyn Repository>) {
    let app = build_router(AppState::new(
        repo,
        Arc::new(Config {
            auth_enabled: false,
            ..Config::default()
        }),
    ));
    let host = Host::new();
    let s = uuid::Uuid::new_v4().to_string();
    let anonymous_share = |rev: u64, action: Value, chunk: Option<&[u8]>| {
        let op = json!({"v": 1, "host": host.id(), "session": s, "owner": null, "rev": rev, "action": action});
        host.sign(&op, chunk)
    };
    let share = json!({"type": "share", "audience": {"visibility": "organization"},
        "agent": "claude", "title": "Widget fix"});
    let (status, body) = request(
        &app,
        "",
        "POST",
        "/api/publications",
        anonymous_share(1, share, None),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, view) = request(&app, "", "GET", &format!("/api/sessions/{s}"), Value::Null).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(
        view["publication"],
        json!({"state": "active", "rev": 1, "host_id": host.id()})
    );
    let (status, _) = request(
        &app,
        "",
        "PUT",
        &format!("/api/sessions/{s}/sharing"),
        json!({"visibility": "restricted", "emails": []}),
    )
    .await;
    assert_eq!(status, 409);
}

#[tokio::test]
async fn managed_marker_auth_off_sqlite() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/publication-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.sqlite", uuid::Uuid::new_v4()));
    let repo = SqliteRepo::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    managed_marker_auth_off(Arc::new(repo.clone())).await;
    repo.pool().close().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn managed_marker_sqlite() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/publication-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.sqlite", uuid::Uuid::new_v4()));
    let repo = SqliteRepo::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    managed_marker(Arc::new(repo.clone())).await;
    repo.pool().close().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable empty Postgres database in PUBLICATION_TEST_DATABASE_URL"]
async fn managed_marker_postgres() {
    let url = std::env::var("PUBLICATION_TEST_DATABASE_URL").unwrap();
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("publication_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    let url = format!("{url}?options=-csearch_path%3D{schema}");
    let repo = conversations_server::store::PostgresRepo::connect(&url)
        .await
        .unwrap();
    managed_marker(Arc::new(repo)).await;
}
