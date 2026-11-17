//! The communication rail read model: owner-only, non-consuming, and never a
//! route to transcripts the viewer cannot already read.

use std::sync::Arc;

use axum::{body::Body, http::Request, Router};
use conversations_server::{build_router_with_auth, AppState, Config, Repository, SqliteRepo};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

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

const BASE: &str = "/api/agent-messaging/sessions";

fn rail(id: &str) -> String {
    format!("{BASE}/{id}/communications")
}

fn row<'a>(body: &'a Value, id: &str) -> Option<&'a Value> {
    body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == id)
}

async fn upload(repo: &Arc<dyn Repository>, id: &str, owner: &str) {
    let session = conversations_core::parse_session_str(
        None,
        &format!(
            r#"{{"type":"user","sessionId":"{id}","uuid":"u-{id}","message":{{"role":"user","content":"hello"}}}}"#
        ),
    )
    .unwrap();
    repo.upsert_session(owner, None, Some(owner), &session)
        .await
        .unwrap();
}

async fn exercise(repo: Arc<dyn Repository>) {
    let config = Config {
        auth_enabled: true,
        password_auth_enabled: true,
        cookie_secure: false,
        ..Config::default()
    };
    let state = AppState::new(repo.clone(), Arc::new(config));
    let app = build_router_with_auth(state).await.unwrap();
    let alice = login(&app, "alice@example.com").await;
    let bob = login(&app, "bob@example.com").await;
    let eve = login(&app, "eve@example.com").await;
    let mallory = login(&app, "mallory@example.com").await;

    let project = repo
        .create_project(Some("alice@example.com"), "shared", None)
        .await
        .unwrap();
    upload(&repo, "alice-session", "alice@example.com").await;
    upload(&repo, "eve-session", "eve@example.com").await;
    repo.create_live_session(&conversations_server::store::NewLiveSession {
        id: "bob-session".into(),
        session_id: "bob-session".into(),
        agent: conversations_core::AgentKind::Codex,
        title: Some("Bob's live task".into()),
        project_path: None,
        cwd: None,
        git_branch: None,
        owner_email: Some("bob@example.com".into()),
        source_host: None,
        remote_url: None,
        mode: "interactive".into(),
    })
    .await
    .unwrap();
    repo.set_session_project("alice-session", Some(&project.id))
        .await
        .unwrap();
    repo.set_session_project("bob-session", Some(&project.id))
        .await
        .unwrap();

    // Sharing a project alone grants no contact.
    let (_, before) = request(&app, &alice, "GET", &rail("alice-session"), Value::Null).await;
    assert!(row(&before, "bob-session").is_none(), "{before}");
    // Bob explicitly lets Alice's sessions contact his.
    repo.set_live_shares(
        "bob-session",
        &[conversations_server::store::LiveShare {
            email: "alice@example.com".into(),
            can_read: false,
            can_write: false,
            can_communicate: true,
        }],
    )
    .await
    .unwrap();

    // An eligible peer with no messages is listed without a conversation, and
    // unrelated sessions are not listed at all.
    let (status, initial) = request(&app, &alice, "GET", &rail("alice-session"), Value::Null).await;
    assert_eq!(status, 200, "{initial}");
    let bob_row = row(&initial, "bob-session").expect("shared peer is listed");
    assert_eq!(bob_row["title"], "Bob's live task");
    assert_eq!(bob_row["owner_email"], "bob@example.com");
    assert_eq!(bob_row["agent"], "codex");
    assert_eq!(bob_row["connected"], false);
    assert!(bob_row["conversation"].is_null());
    assert!(row(&initial, "eve-session").is_none());

    // Messaging authority does not confer transcript access: Alice may message
    // Bob's session but cannot read it, and the rail must say so.
    assert_eq!(
        request(
            &app,
            &alice,
            "GET",
            "/api/sessions/bob-session",
            Value::Null
        )
        .await
        .0,
        404
    );
    assert_eq!(bob_row["readable"], false);

    // Build a conversation: Alice → Bob (fetched by Bob, then replied to),
    // Alice → Bob (still queued), Bob → Alice (the reply, unacknowledged by Alice).
    // Central messaging routes are retired; seed the message rows directly
    // (the rail reads only stored rows).
    let seed_message = |_from: &str, to: &str, text: &str, key: &str, reply_to: Option<String>| {
        conversations_core::messaging::SendAgentMessage {
            recipient_session_id: to.to_string(),
            text: text.to_string(),
            idempotency_key: key.to_string(),
            reply_to,
        }
    };
    let first = repo
        .send_agent_message(
            "alice-session",
            &seed_message(
                "alice-session",
                "bob-session",
                "\n  Review retry handling  \nsecret-body-not-in-rail",
                "a1",
                None,
            ),
        )
        .await
        .unwrap();
    repo.receive_agent_messages("bob-session", &[])
        .await
        .unwrap();
    let reply = json!(repo
        .send_agent_message(
            "bob-session",
            &seed_message(
                "bob-session",
                "alice-session",
                "secret-reply",
                "b1",
                Some(first.id.clone()),
            ),
        )
        .await
        .unwrap());
    let second = repo
        .send_agent_message(
            "alice-session",
            &seed_message("alice-session", "bob-session", &"é".repeat(161), "a2", None),
        )
        .await
        .unwrap();
    let first = json!(first);
    let second = json!(second);

    repo.update_live_session_title("bob-session", "Updated live task")
        .await
        .unwrap();
    let (_, retitled) = request(&app, &alice, "GET", &rail("alice-session"), Value::Null).await;
    assert_eq!(
        row(&retitled, "bob-session").unwrap()["title"],
        "Updated live task"
    );
    repo.set_title_override("bob-session", Some("Renamed Bob task"))
        .await
        .unwrap();
    upload(&repo, "alice-other", "alice@example.com").await;
    let other_message = json!(repo
        .send_agent_message(
            "alice-session",
            &seed_message(
                "alice-session",
                "alice-other",
                "Only for the other session",
                "other-1",
                None
            ),
        )
        .await
        .unwrap());

    let (status, body) = request(&app, &alice, "GET", &rail("alice-session"), Value::Null).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        row(&body, "bob-session").unwrap()["title"],
        "Renamed Bob task"
    );
    let other_titles = row(&body, "alice-other").unwrap()["conversation"]["messages"]
        .as_array()
        .unwrap();
    assert_eq!(other_titles.len(), 1);
    assert_eq!(other_titles[0]["id"], other_message["id"]);
    let conversation = &row(&body, "bob-session").unwrap()["conversation"];
    assert_eq!(conversation["sent_count"], 2);
    assert_eq!(conversation["received_count"], 1);
    assert_eq!(conversation["last_message_at"], second["created_at"]);
    assert_eq!(conversation["last_direction"], "sent");
    // The latest sent message is only stored; the rail must not call it delivered.
    assert_eq!(conversation["last_sent_status"], "queued");
    assert_eq!(conversation["unacknowledged_incoming"], 1);
    let messages = conversation["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["id"], first["id"]);
    assert_eq!(messages[0]["title"], "Review retry handling");
    assert_eq!(messages[0]["direction"], "sent");
    assert_eq!(messages[1]["title"], "secret-reply");
    assert_eq!(messages[1]["direction"], "received");
    assert_eq!(messages[2]["id"], second["id"]);
    assert_eq!(messages[2]["title"], format!("{}…", "é".repeat(160)));
    assert!(
        !body.to_string().contains("secret-body-not-in-rail"),
        "{body}"
    );

    // Reading the rail never consumes Alice's inbox.
    let still_queued = repo
        .get_agent_message("alice-session", reply["id"].as_str().unwrap())
        .await
        .unwrap()
        .expect("reply row exists");
    assert_eq!(
        still_queued.status,
        conversations_core::messaging::AgentMessageStatus::Queued
    );

    // Bob's view of the same conversation is mirrored from his own session.
    let (_, bob_view) = request(&app, &bob, "GET", &rail("bob-session"), Value::Null).await;
    let mirrored = &row(&bob_view, "alice-session").unwrap()["conversation"];
    assert_eq!(mirrored["sent_count"], 1);
    assert_eq!(mirrored["received_count"], 2);
    assert_eq!(mirrored["last_direction"], "received");
    assert_eq!(mirrored["last_sent_status"], "queued");
    assert_eq!(mirrored["unacknowledged_incoming"], 1);

    // Only the session owner sees its communications — not a project peer, a
    // read+write sharee, a communication-only sharee, or a stranger.
    assert_eq!(
        request(
            &app,
            &alice,
            "PUT",
            "/api/live/alice-session/sharing",
            json!({"shares":[
                {"email":"eve@example.com","can_write":true},
                {"email":"mallory@example.com","communication_only":true}
            ]}),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        request(
            &app,
            &eve,
            "GET",
            "/api/sessions/alice-session",
            Value::Null
        )
        .await
        .0,
        200,
        "eve can read the transcript"
    );
    for viewer in [&bob, &eve, &mallory] {
        let (status, denied) =
            request(&app, viewer, "GET", &rail("alice-session"), Value::Null).await;
        assert_eq!(status, 403, "{denied}");
        assert!(!denied.to_string().contains("bob-session"));
    }

    // A transcript link appears only with real read access: a communication-only
    // share from Bob keeps it hidden, a read share reveals it.
    for (share, readable) in [
        (
            json!({"email":"alice@example.com","communication_only":true}),
            false,
        ),
        (
            json!({"email":"alice@example.com","can_read":true,"can_communicate":true}),
            true,
        ),
    ] {
        assert_eq!(
            request(
                &app,
                &bob,
                "PUT",
                "/api/live/bob-session/sharing",
                json!({"shares":[share]}),
            )
            .await
            .0,
            200
        );
        let transcript = request(
            &app,
            &alice,
            "GET",
            "/api/sessions/bob-session",
            Value::Null,
        )
        .await
        .0;
        assert_eq!(transcript == 200, readable);
        let (_, body) = request(&app, &alice, "GET", &rail("alice-session"), Value::Null).await;
        assert_eq!(row(&body, "bob-session").unwrap()["readable"], readable);
    }

    // Revoking the authorizing share hides the peer and its history, even
    // though the stored messages remain.
    assert_eq!(
        request(
            &app,
            &bob,
            "PUT",
            "/api/live/bob-session/sharing",
            json!({"shares":[]}),
        )
        .await
        .0,
        200
    );
    repo.set_session_project("bob-session", None).await.unwrap();
    let (_, revoked) = request(&app, &alice, "GET", &rail("alice-session"), Value::Null).await;
    assert!(row(&revoked, "bob-session").is_none(), "{revoked}");
}

#[tokio::test]
async fn communication_rail_sqlite() {
    let repo: Arc<dyn Repository> = Arc::new(SqliteRepo::connect("sqlite::memory:").await.unwrap());
    exercise(repo).await;
}

#[tokio::test]
#[ignore = "requires a disposable empty Postgres database in MESSAGING_TEST_DATABASE_URL"]
async fn communication_rail_postgres() {
    let url = std::env::var("MESSAGING_TEST_DATABASE_URL").unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("comm_rail_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let repo = conversations_server::store::PostgresRepo::connect(&format!(
        "{url}?options=-csearch_path%3D{schema}"
    ))
    .await
    .unwrap();
    exercise(Arc::new(repo)).await;
}
