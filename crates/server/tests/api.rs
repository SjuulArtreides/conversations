//! Integration test for the JSON API: upload -> idempotent re-upload -> list ->
//! get -> healthz -> 404. Drives the real router via `tower::ServiceExt::oneshot`
//! against a temp-file SQLite database (in-memory sqlite is per-connection, so a
//! temp file is used to survive across pool connections + migrations).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use conversations_server::{build_router, build_router_with_auth, AppState, Config, SqliteRepo};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
const SAMPLE: &str = concat!(
    r#"{"type":"ai-title","aiTitle":"Test session","sessionId":"it-sess-1"}"#,
    "\n",
    r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","sessionId":"it-sess-1","gitBranch":"main","cwd":"/tmp/proj","message":{"role":"user","content":"hello there"}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-06-05T10:00:02.000Z","sessionId":"it-sess-1","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"hi back"}]}}"#,
    "\n",
);

struct TempDb {
    path: std::path::PathBuf,
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
    }
}

async fn test_state() -> (AppState, TempDb) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // Unique temp file per test; nanos + a process-wide counter avoids
    // collisions across runs AND between tests that start in the same nanosecond.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("conv-it-{nanos}-{n}.sqlite"));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let repo = SqliteRepo::connect(&url).await.expect("connect + migrate");
    let state = AppState::new(
        Arc::new(repo),
        Arc::new(Config {
            auth_enabled: false,
            ..Config::default()
        }),
    );
    (state, TempDb { path })
}

/// Like [`test_state`] but with auth ON and password-only (no Google). Proves
/// real identities + gating work without any OIDC/network configuration.
async fn test_state_password_auth() -> (AppState, TempDb) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("conv-it-auth-{nanos}-{n}.sqlite"));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let repo = SqliteRepo::connect(&url).await.expect("connect + migrate");
    let config = Config {
        auth_enabled: true,
        password_auth_enabled: true,
        initial_admin_emails: vec![
            "admin@example.com".to_string(),
            "unclaimed-admin@example.com".to_string(),
        ],
        ..Config::default()
    };
    let state = AppState::new(Arc::new(repo), Arc::new(config));
    (state, TempDb { path })
}

/// Extract the session cookie (`name=value`) from a response's `Set-Cookie`.
fn session_cookie(resp: &axum::response::Response) -> String {
    let raw = resp
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .expect("set-cookie present")
        .to_str()
        .unwrap();
    raw.split(';').next().unwrap().to_string()
}

fn upload_body() -> Body {
    let session = conversations_core::parse_session_str(Some("it-sess-1"), SAMPLE).unwrap();
    let req = json!({
        "uploaded_by": "tester@example.com",
        "source_host": "ci-host",
        "session": session,
    });
    Body::from(serde_json::to_vec(&req).unwrap())
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn app_info_exposes_running_version() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    let resp = app.oneshot(get("/api/app")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["version"], json!(env!("CARGO_PKG_VERSION")));
}

#[tokio::test]
async fn upload_idempotent_list_get_health_404() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // First upload -> 201 created
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["created"], json!(true));
    assert_eq!(body["id"], json!("it-sess-1"));
    assert_eq!(body["message_count"], json!(2));

    // Re-upload same id -> 200, created=false, same message_count (idempotent)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["created"], json!(false));
    assert_eq!(body["message_count"], json!(2));

    // List -> contains the session
    let resp = app.clone().oneshot(get("/api/sessions")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], json!("it-sess-1"));
    assert_eq!(sessions[0]["uploaded_by"], json!("tester@example.com"));

    // Get -> full messages
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(body["messages"][0]["role"], json!("user"));

    // Healthz
    let resp = app.clone().oneshot(get("/healthz")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], json!("ok"));

    // Unknown id -> 404
    let resp = app.oneshot(get("/api/sessions/nope")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn completed_codex_session_get_reconstructs_tool_blocks() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);
    let transcript = concat!(
        r#"{"type":"session_meta","payload":{"id":"codex-tools","cwd":"/work"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"custom_tool_call","id":"item-1","call_id":"custom-1","name":"apply_patch","input":"raw patch"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","id":"item-2","call_id":"custom-1","output":"Done!"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"function_call","id":"item-3","call_id":"function-1","name":"shell","arguments":"{\"cmd\":\"pwd\"}"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"function_call_output","id":"item-4","call_id":"function-1","output":"/work"}}"#,
        "\n",
    );
    let session = conversations_core::parse_session_str(None, transcript).unwrap();
    let body = json!({
        "uploaded_by": "tester@example.com",
        "source_host": "ci-host",
        "session": session,
    });

    let response = app
        .clone()
        .oneshot(json_req("POST", "/api/sessions", &body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = app.oneshot(get("/api/sessions/codex-tools")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["blocks"][0]["type"], "tool_use");
    assert_eq!(messages[0]["blocks"][0]["id"], "custom-1");
    assert_eq!(messages[0]["blocks"][0]["input"], "raw patch");
    assert_eq!(messages[1]["blocks"][0]["type"], "tool_result");
    assert_eq!(messages[1]["blocks"][0]["tool_use_id"], "custom-1");
    assert_eq!(messages[1]["blocks"][0]["content"], "Done!");
    assert_eq!(messages[2]["blocks"][0]["type"], "tool_use");
    assert_eq!(messages[2]["blocks"][0]["input"]["cmd"], "pwd");
    assert_eq!(messages[3]["blocks"][0]["type"], "tool_result");
    assert_eq!(messages[3]["blocks"][0]["content"], "/work");
}

/// Renaming a session stores a DB-only override: GET reflects it, a re-upload
/// (which re-parses the transcript title) preserves it, and a blank title
/// clears it back to the parsed title. Exercises `PUT /api/sessions/{id}/title`.
#[tokio::test]
async fn rename_session_overrides_title_without_touching_transcript() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Upload, parsed title = "Test session".
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();

    let rename = |title: Value| {
        app.clone().oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/it-sess-1/title")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "title": title })).unwrap(),
                ))
                .unwrap(),
        )
    };

    // Set the override; response echoes the effective title.
    let resp = rename(json!("My rename")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["title"], json!("My rename"));

    // GET reflects the override.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1"))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["title"], json!("My rename"));

    // Re-upload re-parses "Test session" but must not wipe the override.
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1"))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["title"], json!("My rename"));

    // Blank title clears the override → reverts to the parsed title.
    let resp = rename(json!("   ")).await.unwrap();
    assert_eq!(body_json(resp).await["title"], json!("Test session"));

    // Renaming a non-existent session → 404.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/nope/title")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "title": "x" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A live session launched from a known repo, whose title carries a
/// `pull request #N` shorthand, auto-links that PR while still live — and the
/// link surfaces in the `/api/live` payload so the SPA can match it in search.
/// Exercises the full chain: remote_url on register → repo context →
/// title-driven `detect_and_link_prs` → `linked_prs` on the live view.
#[tokio::test]
async fn live_title_autolinks_pr_from_repo_context() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());

    // Seed a live session with the launch repo's origin remote (the central
    // registration endpoint is retired).
    state
        .repo
        .create_live_session(&conversations_server::store::NewLiveSession {
            id: "live-pr-1".into(),
            session_id: "live-pr-1".into(),
            agent: conversations_core::AgentKind::Claude,
            title: None,
            project_path: None,
            cwd: None,
            git_branch: None,
            owner_email: None,
            source_host: None,
            remote_url: Some("git@github.com:Example-Org/product-demo.git".into()),
            mode: "view".into(),
        })
        .await
        .unwrap();

    // The title arrives on its own endpoint, carrying the PR shorthand.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/live/live-pr-1/title")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "title": "Review pull request #409" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["updated"], json!(true));

    // The live listing exposes the auto-linked PR for the SPA to match.
    let resp = app.clone().oneshot(get("/api/live")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let live = &body["live_sessions"][0];
    assert_eq!(live["id"], json!("live-pr-1"));
    let prs = live["linked_prs"].as_array().expect("linked_prs array");
    assert_eq!(prs.len(), 1, "expected one auto-linked PR, got {prs:?}");
    assert_eq!(prs[0]["owner"], json!("Example-Org"));
    assert_eq!(prs[0]["repo"], json!("product-demo"));
    assert_eq!(prs[0]["number"], json!(409));
    assert_eq!(
        prs[0]["url"],
        json!("https://github.com/Example-Org/product-demo/pull/409")
    );
}
#[tokio::test]
async fn live_session_is_listed_and_renamable() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());

    // Seed a live session (no title yet; the central registration endpoint is
    // retired, so tests seed the row directly).
    seed_live(&state, "live-rn-1").await;

    // It shows up in the main session list, flagged live.
    let resp = app.clone().oneshot(get("/api/sessions")).await.unwrap();
    let body = body_json(resp).await;
    let row = body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == json!("live-rn-1"))
        .expect("live session listed");
    assert_eq!(row["is_live"], json!(true));

    // Rename it via the stored-session endpoint — now finds the eager row.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/live-rn-1/title")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "title": "My live rename" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["title"], json!("My live rename"));

    // The override is reflected on the session.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/live-rn-1"))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["title"], json!("My live rename"));

    // Later automatic (CLI-pushed) titles must not clobber the manual rename,
    // in either the live list or the session detail.
    let effective_titles = |app: axum::Router, expected_override: Option<&'static str>| async move {
        let resp = app.clone().oneshot(get("/api/live")).await.unwrap();
        let live = body_json(resp).await;
        let row = live["live_sessions"]
            .as_array()
            .expect("live list has live_sessions array")
            .iter()
            .find(|s| s["id"] == json!("live-rn-1"))
            .expect("live session in live list");
        assert_eq!(
            row.get("title_override"),
            Some(&json!(expected_override)),
            "clients must distinguish an explicit override even when it equals an automatic title"
        );
        let live_title = row["title"].clone();
        let resp = app.oneshot(get("/api/sessions/live-rn-1")).await.unwrap();
        (live_title, body_json(resp).await["title"].clone())
    };
    let post_auto_title = |app: axum::Router, title: &'static str| async move {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/live/live-rn-1/title")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "title": title })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        body_json(resp).await
    };

    assert_eq!(
        post_auto_title(app.clone(), "Automatic title one").await["updated"],
        json!(true)
    );
    post_auto_title(app.clone(), "My live rename").await;
    effective_titles(app.clone(), Some("My live rename")).await;
    assert_eq!(
        post_auto_title(app.clone(), "Automatic title two").await["updated"],
        json!(true)
    );
    let (live_title, session_title) = effective_titles(app.clone(), Some("My live rename")).await;
    assert_eq!(live_title, json!("My live rename"));
    assert_eq!(session_title, json!("My live rename"));

    // Clearing the manual override reveals the newest automatic title.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/live-rn-1/title")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "title": "" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let (live_title, session_title) = effective_titles(app, None).await;
    assert_eq!(live_title, json!("Automatic title two"));
    assert_eq!(session_title, json!("Automatic title two"));
}

#[tokio::test]
async fn sharing_endpoints_and_reupload_preserves_sharing() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Upload the sample session.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Set sharing -> restricted + [a@x].
    let body = Body::from(
        serde_json::to_vec(&json!({ "visibility": "restricted", "emails": ["a@x"] })).unwrap(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/it-sess-1/sharing")
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["visibility"], json!("restricted"));
    assert_eq!(body["emails"], json!(["a@x"]));

    // Re-upload the same id -> sharing must be preserved, message_count stable.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["message_count"], json!(2));

    // Get reflects the preserved sharing (auth off => no read enforcement).
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["visibility"], json!("restricted"));
    assert_eq!(body["shared_emails"], json!(["a@x"]));
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);

    // Invalid visibility -> 400.
    let body =
        Body::from(serde_json::to_vec(&json!({ "visibility": "public", "emails": [] })).unwrap());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/it-sess-1/sharing")
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Unknown session -> 404.
    let body = Body::from(
        serde_json::to_vec(&json!({ "visibility": "organization", "emails": [] })).unwrap(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/does-not-exist/sharing")
                .header("content-type", "application/json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn readyz_reports_ready() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    let resp = app.oneshot(get("/readyz")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], json!("ready"));
}

#[tokio::test]
async fn readyz_reports_unready_once_shutting_down() {
    let (state, _tmp) = test_state().await;
    // Simulate receipt of a termination signal.
    state.begin_shutdown();
    let app = build_router(state);

    // Readiness flips to 503 even though the DB is perfectly healthy, so the
    // Service drains this pod out of rotation during graceful shutdown.
    let resp = app.clone().oneshot(get("/readyz")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(body["status"], json!("shutting_down"));

    // Liveness stays green — a draining pod must not be restarted by the
    // liveness probe before it finishes shutting down.
    let resp = app.oneshot(get("/healthz")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], json!("ok"));
}

#[tokio::test]
async fn search_ranks_exact_effective_titles_globally_before_live_and_recency() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());
    for (id, title, year) in [
        ("exact-old", "Original title", 2020),
        ("partial-new", "Needle later work", 2025),
        ("partial-live", "Needle live work", 2024),
        ("literal-exact", "Rate 100%_done", 2021),
        ("literal-partial", "Rate 100%_done followup", 2026),
    ] {
        let mut session = conversations_core::parse_session_str(Some(id), SAMPLE).unwrap();
        session.id = id.into();
        session.title = title.into();
        session.started_at = Some(format!("{year}-01-01T00:00:00Z").parse().unwrap());
        let response = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/api/sessions",
                &json!({"uploaded_by":"rank-test@example.com","session":session}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    // Effective manual title, not the original parsed title, defines exactness.
    let response = app
        .clone()
        .oneshot(json_req(
            "PUT",
            "/api/sessions/exact-old/title",
            &json!({"title":" Needle "}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    seed_live(&state, "partial-live").await;
    async fn ids(app: &axum::Router, query: &str) -> Vec<String> {
        let response = app
            .clone()
            .oneshot(get(&format!("/api/sessions{query}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect()
    }
    assert_eq!(
        ids(&app, "?q=%20nEeDlE%20").await,
        ["exact-old", "partial-live", "partial-new"]
    );
    assert_eq!(
        ids(&app, "?q=Needle").await,
        ["exact-old", "partial-live", "partial-new"]
    );
    assert_eq!(
        ids(&app, "?q=Rate%20100%25_done").await,
        ["literal-exact", "literal-partial"]
    );
    assert_eq!(ids(&app, "?q=").await, ids(&app, "").await);
    assert_eq!(ids(&app, "?q=%20%20").await, ids(&app, "").await);
    assert_eq!(ids(&app, "").await[0], "partial-live");
}

#[tokio::test]
async fn search_is_case_insensitive() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Upload the sample (title "Test session", message text "hello there").
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Uppercase query matches the mixed-case title.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions?q=TEST"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);

    // Uppercase query matches lowercase message text.
    let resp = app.oneshot(get("/api/sessions?q=HELLO")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn search_matches_pr_url() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Upload a session carrying a repo link with a PR url (#742).
    let session = conversations_core::parse_session_str(Some("pr-sess-1"), SAMPLE).unwrap();
    let body = json!({
        "uploaded_by": "alice@example.com",
        "source_host": "ci",
        "session": session,
        "repos": [
            {"repo": "app", "branch": "feat/x",
             "pr_url": "https://github.com/org/app/pull/742",
             "local_path": "/home/alice/app"}
        ]
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The bare PR number finds the session via its pr_url.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions?q=742"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);

    // A leading '#' is stripped so "#742" matches the same link.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions?q=%23742"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);

    // A non-matching PR number returns nothing.
    let resp = app.oneshot(get("/api/sessions?q=999")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["sessions"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn smtp_signup_requires_email_proof_and_consumes_it_once() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use conversations_server::config::SignupEmailConfig;

    let (mut state, _db) = test_state_password_auth().await;
    Arc::make_mut(&mut state.config).signup_email = SignupEmailConfig {
        smtp_url: Some("smtps://sender:password@localhost".into()),
        from: Some("sender@example.com".into()),
        public_url: Some("https://example.com".into()),
    };
    let app = build_router_with_auth(state.clone()).await.unwrap();
    let config = body_json(app.clone().oneshot(get("/api/auth/config")).await.unwrap()).await;
    assert_eq!(config["signup_verification_enabled"], true);

    let post = |path, body: Value| {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let email = "signup@example.com";
    let password = "signup-pass";
    let credentials = json!({ "email": email, "password": password });
    let bypass = app
        .clone()
        .oneshot(post("/auth/register", credentials.clone()))
        .await
        .unwrap();
    assert_eq!(bypass.status(), StatusCode::NOT_FOUND);
    let login = app
        .clone()
        .oneshot(post("/auth/login", credentials.clone()))
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::UNAUTHORIZED);

    // Seed the digest of the token that would have been emailed by the relay.
    let token = uuid::Uuid::new_v4().simple().to_string();
    let digest = URL_SAFE_NO_PAD.encode(ring::digest::digest(
        &ring::digest::SHA256,
        token.as_bytes(),
    ));
    let now = chrono::Utc::now();
    assert!(state
        .repo
        .reserve_signup_token(email, &digest, now, now + chrono::Duration::minutes(30))
        .await
        .unwrap());
    assert!(!state
        .repo
        .reserve_signup_token(
            email,
            "replacement",
            now,
            now + chrono::Duration::minutes(30)
        )
        .await
        .unwrap());
    let expired = uuid::Uuid::new_v4().simple().to_string();
    let expired_digest = URL_SAFE_NO_PAD.encode(ring::digest::digest(
        &ring::digest::SHA256,
        expired.as_bytes(),
    ));
    assert!(state
        .repo
        .reserve_signup_token(
            "expired@example.com",
            &expired_digest,
            now - chrono::Duration::minutes(31),
            now - chrono::Duration::minutes(1)
        )
        .await
        .unwrap());
    let rejected = app
        .clone()
        .oneshot(post(
            "/auth/signup-email/complete",
            json!({ "token": expired, "password": password }),
        ))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

    let complete = json!({ "token": token, "password": password, "display_name": "Verified" });
    let first = app
        .clone()
        .oneshot(post("/auth/signup-email/complete", complete.clone()))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let second = app
        .clone()
        .oneshot(post("/auth/signup-email/complete", complete))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::BAD_REQUEST);

    let login = app
        .clone()
        .oneshot(post("/auth/login", credentials))
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = session_cookie(&login);
    let protected = app
        .oneshot(
            Request::builder()
                .uri("/api/sessions")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(protected.status(), StatusCode::OK);
}

#[tokio::test]
async fn password_auth_register_login_and_gating() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state)
        .await
        .expect("build auth router");

    // Auth-config probe (no session): password on, google off, no user.
    let resp = app.clone().oneshot(get("/api/auth/config")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["password_enabled"], json!(true));
    assert_eq!(body["google_enabled"], json!(false));
    assert_eq!(body["user"], Value::Null);

    // Protected endpoint without a session -> 401.
    let resp = app.clone().oneshot(get("/api/sessions")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Register (no email verification) -> 201 + a session cookie.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "email": "Dev@Example.com",
                        "password": "hunter2pass",
                        "display_name": "Dev"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let cookie = session_cookie(&resp);
    let body = body_json(resp).await;
    // Email is normalized to lowercase.
    assert_eq!(body["email"], json!("dev@example.com"));

    // With the session cookie, the protected endpoint is reachable.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/sessions")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Registering the same email again -> 409 conflict.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "email": "dev@example.com",
                        "password": "anotherpass"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Wrong password -> 401.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "email": "dev@example.com",
                        "password": "wrong"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Correct password (case-insensitive email) -> 200 + a fresh cookie.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "email": "DEV@example.com",
                        "password": "hunter2pass"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = session_cookie(&resp);

    // The logged-in identity is reported by /api/auth/config.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/auth/config")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["user"]["email"], json!("dev@example.com"));
}

#[tokio::test]
async fn admin_password_reset_links_are_authorized_expiring_and_single_use() {
    let (state, _tmp) = test_state_password_auth().await;
    state
        .repo
        .upsert_user("admin@example.com", Some("Admin"))
        .await
        .unwrap();
    let admin_hash = bcrypt::hash("admin-password", bcrypt::DEFAULT_COST).unwrap();
    state
        .repo
        .set_password("admin@example.com", &admin_hash)
        .await
        .unwrap();
    let app = build_router_with_auth(state.clone())
        .await
        .expect("build auth router");

    let target_session = register_cookie(&app, "target@example.com").await;
    let non_admin = register_cookie(&app, "viewer@example.com").await;

    // An allowlisted but unprovisioned address cannot be claimed through the
    // app's unverified self-registration flow.
    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/auth/register",
            &json!({
                "email": "unclaimed-admin@example.com",
                "password": "attacker-password"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/auth/login",
            &json!({
                "email": "admin@example.com",
                "password": "admin-password"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let admin = session_cookie(&resp);

    // A delegated admin grant also cannot turn an unverified directory row
    // into a claimable privileged account.
    state
        .repo
        .upsert_user("delegated-no-password@example.com", None)
        .await
        .unwrap();
    state
        .repo
        .set_user_admin(
            "delegated-no-password@example.com",
            true,
            "admin@example.com",
        )
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/auth/register",
            &json!({
                "email": "delegated-no-password@example.com",
                "password": "attacker-password"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let create_link = |cookie: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/admin/password-reset-links")
            .header("content-type", "application/json")
            .header("cookie", cookie)
            .body(Body::from(
                serde_json::to_vec(&json!({ "email": "target@example.com" })).unwrap(),
            ))
            .unwrap()
    };

    // A signed-in non-admin cannot issue reset links.
    let resp = app.clone().oneshot(create_link(&non_admin)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // The account directory is administrator-only too.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/admin/users")
                .header("cookie", &non_admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // The configured initial admin identity is visible to the SPA.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/config")
                .header("cookie", &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["user"]["is_admin"], json!(true));

    // The initial admin can promote an existing user.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/admins")
                .header("content-type", "application/json")
                .header("cookie", &admin)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "email": "viewer@example.com" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The promoted user gains real administrator authority immediately.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/auth/config")
                .header("cookie", &non_admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["user"]["is_admin"], json!(true));
    let resp = app.clone().oneshot(create_link(&non_admin)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Admins can see the complete account directory, including more users than
    // the interactive search endpoint's 50-result cap, with effective roles.
    for index in 0..51 {
        state
            .repo
            .upsert_user(&format!("bulk-{index:02}@example.com"), None)
            .await
            .unwrap();
    }
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/admin/users")
                .header("cookie", &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let users = body_json(resp).await["users"].as_array().unwrap().clone();
    assert_eq!(users.len(), 55);
    assert!(users.iter().any(|entry| {
        entry["email"] == "admin@example.com"
            && entry["display_name"] == "Admin"
            && entry["is_admin"] == json!(true)
            && entry["is_initial"] == json!(true)
    }));
    assert!(users.iter().any(|entry| {
        entry["email"] == "viewer@example.com"
            && entry["is_admin"] == json!(true)
            && entry["is_initial"] == json!(false)
    }));
    assert!(users
        .iter()
        .any(|entry| entry["email"] == "bulk-50@example.com"));

    // Both bootstrap and delegated admins are listed with their source.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/admin/admins")
                .header("cookie", &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let admins = body_json(resp).await["admins"].as_array().unwrap().clone();
    assert!(admins
        .iter()
        .any(|entry| entry["email"] == "admin@example.com" && entry["is_initial"] == json!(true)));
    assert!(
        admins
            .iter()
            .any(|entry| entry["email"] == "viewer@example.com"
                && entry["is_initial"] == json!(false))
    );

    // Bootstrap administrators cannot be revoked through the application.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/admin/admins/admin%40example.com")
                .header("cookie", &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Revoking a delegated admin removes the authority immediately.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/admin/admins/viewer%40example.com")
                .header("cookie", &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/admin/users")
                .header("cookie", &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let users = body_json(resp).await["users"].as_array().unwrap().clone();
    assert!(users.iter().any(|entry| {
        entry["email"] == "viewer@example.com" && entry["is_admin"] == json!(false)
    }));
    let resp = app.clone().oneshot(create_link(&non_admin)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Issuing a newer link invalidates the older one.
    let resp = app.clone().oneshot(create_link(&admin)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let first_path = body_json(resp).await["reset_path"]
        .as_str()
        .unwrap()
        .to_string();
    let first_token = first_path.split("token=").nth(1).unwrap().to_string();

    let resp = app.clone().oneshot(create_link(&admin)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let second_path = body_json(resp).await["reset_path"]
        .as_str()
        .unwrap()
        .to_string();
    let second_token = second_path.split("token=").nth(1).unwrap().to_string();

    let reset = |token: &str, password: &str| {
        Request::builder()
            .method("POST")
            .uri("/auth/password-reset")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "token": token, "password": password })).unwrap(),
            ))
            .unwrap()
    };

    let resp = app
        .clone()
        .oneshot(reset(&first_token, "new-password"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = app
        .clone()
        .oneshot(reset(&second_token, "new-password"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Resetting the password invalidates every session authenticated with the
    // account's previous authentication version.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/sessions")
                .header("cookie", &target_session)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // A consumed link cannot change the password again.
    let resp = app
        .clone()
        .oneshot(reset(&second_token, "another-password"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let login = |password: &str| {
        Request::builder()
            .method("POST")
            .uri("/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "email": "target@example.com",
                    "password": password
                }))
                .unwrap(),
            ))
            .unwrap()
    };
    let resp = app.clone().oneshot(login("hunter2pass")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = app.clone().oneshot(login("new-password")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // An otherwise well-formed token that is already expired is also rejected.
    let expired_id = uuid::Uuid::new_v4().to_string();
    let expired_secret = uuid::Uuid::new_v4().to_string();
    let expired_hash = bcrypt::hash(&expired_secret, bcrypt::DEFAULT_COST).unwrap();
    state
        .repo
        .create_password_reset_token(
            "target@example.com",
            "admin@example.com",
            &expired_id,
            &expired_hash,
            chrono::Utc::now() - chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
    let expired_token = format!("{expired_id}.{expired_secret}");
    let resp = app
        .clone()
        .oneshot(reset(&expired_token, "expired-password"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_delete_user_requires_admin_and_blocks_lockout() {
    let (state, _tmp) = test_state_password_auth().await;
    state
        .repo
        .upsert_user("admin@example.com", Some("Admin"))
        .await
        .unwrap();
    let admin_hash = bcrypt::hash("admin-password", bcrypt::DEFAULT_COST).unwrap();
    state
        .repo
        .set_password("admin@example.com", &admin_hash)
        .await
        .unwrap();
    let app = build_router_with_auth(state.clone())
        .await
        .expect("build auth router");

    // Log the initial admin in.
    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/auth/login",
            &json!({ "email": "admin@example.com", "password": "admin-password" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let admin = session_cookie(&resp);

    // Register two bystanders, one of whom we will promote and then demote
    // across the test, and one plain user that is the real delete target.
    let viewer = register_cookie(&app, "viewer@example.com").await;
    let _target = register_cookie(&app, "target@example.com").await;

    let delete = |cookie: &str, email: &str| {
        Request::builder()
            .method("DELETE")
            .uri(format!("/api/admin/users/{}", email.replace('@', "%40")))
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    };

    // Non-admins cannot delete.
    let resp = app
        .clone()
        .oneshot(delete(&viewer, "target@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Initial administrators are refused through the app.
    let resp = app
        .clone()
        .oneshot(delete(&admin, "admin@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A separately-signed-in admin still cannot delete their own account.
    state
        .repo
        .upsert_user("second-admin@example.com", None)
        .await
        .unwrap();
    let hash = bcrypt::hash("second-password", bcrypt::DEFAULT_COST).unwrap();
    state
        .repo
        .set_password("second-admin@example.com", &hash)
        .await
        .unwrap();
    state
        .repo
        .set_user_admin("second-admin@example.com", true, "admin@example.com")
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/auth/login",
            &json!({
                "email": "second-admin@example.com",
                "password": "second-password"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let second_admin = session_cookie(&resp);
    let resp = app
        .clone()
        .oneshot(delete(&second_admin, "second-admin@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Missing users are 404.
    let resp = app
        .clone()
        .oneshot(delete(&admin, "ghost@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // The happy path: an ordinary user is removed.
    let resp = app
        .clone()
        .oneshot(delete(&admin, "target@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // A repeat delete is now 404.
    let resp = app
        .clone()
        .oneshot(delete(&admin, "target@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // The deleted user disappears from the directory listing.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/admin/users")
                .header("cookie", &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await["users"].as_array().unwrap().clone();
    assert!(!listed
        .iter()
        .any(|entry| entry["email"] == "target@example.com"));

    // Deleting the delegated admin is allowed — two bootstrap admins remain.
    let resp = app
        .clone()
        .oneshot(delete(&admin, "second-admin@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Last-effective-admin guard: construct a state whose only effective
    // admin is a single delegated grant, then try to delete that user.
    let (lone_state, _lone_tmp) = {
        let state = test_state_no_initial_admins().await;
        state
            .0
            .repo
            .upsert_user("sole@example.com", None)
            .await
            .unwrap();
        let hash = bcrypt::hash("sole-password", bcrypt::DEFAULT_COST).unwrap();
        state
            .0
            .repo
            .set_password("sole@example.com", &hash)
            .await
            .unwrap();
        state
            .0
            .repo
            .set_user_admin("sole@example.com", true, "bootstrap@example.com")
            .await
            .unwrap();
        state
    };
    let lone_app = build_router_with_auth(lone_state.clone())
        .await
        .expect("build auth router");
    let resp = lone_app
        .clone()
        .oneshot(json_req(
            "POST",
            "/auth/login",
            &json!({ "email": "sole@example.com", "password": "sole-password" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sole = session_cookie(&resp);
    // sole@ cannot delete themselves (also the only effective admin).
    let resp = lone_app
        .clone()
        .oneshot(delete(&sole, "sole@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Make a second user and promote them, so the guard is purely "last admin"
    // rather than "self-delete" (which fires first).
    lone_state
        .repo
        .upsert_user("other-admin@example.com", None)
        .await
        .unwrap();
    let hash2 = bcrypt::hash("other-password", bcrypt::DEFAULT_COST).unwrap();
    lone_state
        .repo
        .set_password("other-admin@example.com", &hash2)
        .await
        .unwrap();
    lone_state
        .repo
        .set_user_admin("other-admin@example.com", true, "sole@example.com")
        .await
        .unwrap();
    // sole@ can now delete the second admin — one effective admin remains.
    let resp = lone_app
        .clone()
        .oneshot(delete(&sole, "other-admin@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // But sole@ remains the last admin and still cannot self-delete.
    let resp = lone_app
        .clone()
        .oneshot(delete(&sole, "sole@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Password-auth fixture with no initial admins configured, so admin count is
/// entirely driven by the `user_admins` table.
async fn test_state_no_initial_admins() -> (AppState, TempDb) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("conv-it-del-user-{nanos}-{n}.sqlite"));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let repo = SqliteRepo::connect(&url).await.expect("connect + migrate");
    let config = Config {
        auth_enabled: true,
        password_auth_enabled: true,
        initial_admin_emails: Vec::new(),
        ..Config::default()
    };
    let state = AppState::new(Arc::new(repo), Arc::new(config));
    (state, TempDb { path })
}

#[tokio::test]
async fn repos_upload_merge_get_and_edit() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Upload with two repo links carrying the uploader's local paths.
    let session = conversations_core::parse_session_str(Some("it-sess-1"), SAMPLE).unwrap();
    let body = json!({
        "uploaded_by": "alice@example.com",
        "source_host": "ci",
        "session": session,
        "repos": [
            {"repo": "app", "remote_url": "git@github.com:org/app.git",
             "branch": "main", "local_path": "/home/alice/app"},
            {"repo": "infra", "branch": "feat/x", "pr_url": "https://gh/pr/1",
             "local_path": "/home/alice/infra"}
        ]
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // GET reflects both repos, with the uploader's path recorded.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1"))
        .await
        .unwrap();
    let bdy = body_json(resp).await;
    let repos = bdy["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 2);
    let app_repo = repos.iter().find(|r| r["repo"] == "app").unwrap();
    assert_eq!(app_repo["branch"], json!("main"));
    assert_eq!(app_repo["remote_url"], json!("git@github.com:org/app.git"));
    assert_eq!(
        app_repo["paths"][0]["user_email"],
        json!("alice@example.com")
    );
    assert_eq!(app_repo["paths"][0]["local_path"], json!("/home/alice/app"));

    // A second uploader (bob) re-uploads with his own path for `app` and a new
    // repo `docs`. Merge is additive: infra survives, app gains bob's path.
    let body = json!({
        "uploaded_by": "bob@example.com",
        "session": conversations_core::parse_session_str(Some("it-sess-1"), SAMPLE).unwrap(),
        "repos": [
            {"repo": "app", "branch": "main", "local_path": "/Users/bob/app"},
            {"repo": "docs", "branch": "main", "local_path": "/Users/bob/docs"}
        ]
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1"))
        .await
        .unwrap();
    let bdy = body_json(resp).await;
    let repos = bdy["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 3, "infra preserved, docs added");
    let app_repo = repos.iter().find(|r| r["repo"] == "app").unwrap();
    // app keeps its remote (not clobbered by bob's null) and now has 2 paths.
    assert_eq!(app_repo["remote_url"], json!("git@github.com:org/app.git"));
    assert_eq!(app_repo["paths"].as_array().unwrap().len(), 2);

    // Web edit: replace the canonical set with just `app` (remote/PR updated).
    let body = json!({"repos": [
        {"repo": "app", "remote_url": "git@github.com:org/app.git",
         "branch": "main", "pr_url": "https://gh/pr/9"}
    ]});
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/it-sess-1/repos")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bdy = body_json(resp).await;
    let repos = bdy["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["pr_url"], json!("https://gh/pr/9"));
    // Surviving repo keeps both users' paths.
    assert_eq!(repos[0]["paths"].as_array().unwrap().len(), 2);

    // Edit on a missing session -> 404.
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/nope/repos")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"repos": []})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn users_directory_endpoint() {
    let (state, _tmp) = test_state().await;
    // Seed two users directly through the repo.
    state
        .repo
        .upsert_user("alice@example.com", Some("Alice Smith"))
        .await
        .unwrap();
    state
        .repo
        .upsert_user("bob@example.com", Some("Bob Jones"))
        .await
        .unwrap();

    let app = build_router(state);

    // q matches alice by email substring.
    let resp = app
        .clone()
        .oneshot(get("/api/users?q=alice"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let users = body["users"].as_array().unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["email"], json!("alice@example.com"));

    // No q returns both (ordered by email).
    let resp = app.oneshot(get("/api/users")).await.unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["users"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn delete_session_without_auth() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Upload, then delete (auth off ⇒ allowed), then it's gone.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/it-sess-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app.oneshot(get("/api/sessions/it-sess-1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn upload_strips_nul_from_transcript_and_metadata() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    let mut session = conversations_core::parse_session_str(Some("nul-up-1"), SAMPLE).unwrap();
    session.id = "nul-up-1".to_string();
    session.title = "ti\u{0000}tle".to_string();
    session.cwd = Some("/tmp/pr\u{0000}oj".to_string());
    session.messages[0].text = "he\u{0000}llo".to_string();
    // `raw` is JSONB: the scrub has to reach nested values and object keys, not
    // just the top level. Blocks are rebuilt from `raw` on read, so a clean
    // `content` array is what makes the rendered transcript clean.
    session.messages[0].raw = json!({
        "content": [{
            "type": "tool_result",
            "tool_use_id": "t1",
            "content": "read bin\u{0000}ary",
        }],
        "meta": { "nested": ["de\u{0000}ep", 3] },
    });
    session.messages[0]
        .raw
        .as_object_mut()
        .unwrap()
        .insert("k\u{0000}ey".into(), json!("v"));

    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/api/sessions",
            &json!({
                "uploaded_by": "tester@example.com",
                "source_host": "ci\u{0000}-host",
                "session": session,
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body = body_json(
        app.clone()
            .oneshot(get("/api/sessions/nul-up-1"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["title"], json!("title"));
    assert_eq!(body["cwd"], json!("/tmp/proj"));
    assert_eq!(body["source_host"], json!("ci-host"));
    let msg = &body["messages"][0];
    assert_eq!(msg["text"], json!("hello"));
    assert_eq!(msg["raw"]["content"][0]["content"], json!("read binary"));
    assert_eq!(msg["raw"]["meta"]["nested"][0], json!("deep"));
    assert_eq!(
        msg["raw"]["key"],
        json!("v"),
        "NUL stripped from object keys"
    );
    // Blocks are reconstructed from the scrubbed `raw`, so what the UI renders
    // is clean too.
    assert_eq!(msg["blocks"][0]["content"], json!("read binary"));
}

// ---------------------------------------------------------------------------
// Projects + chats (slice 5a)
// ---------------------------------------------------------------------------

/// Build a GET request for `uri`.
fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

/// Build a JSON-body request for `method`/`uri`.
fn json_req(method: &str, uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

/// Upload the sample session under a given id (auth off).
async fn upload_sample(app: &axum::Router, id: &str) {
    let session = conversations_core::parse_session_str(Some(id), SAMPLE).unwrap();
    // Rewrite the embedded session id so the row keys on `id`.
    let mut session = session;
    session.id = id.to_string();
    let body = json!({
        "uploaded_by": "tester@example.com",
        "source_host": "ci",
        "session": session,
    });
    let resp = app
        .clone()
        .oneshot(json_req("POST", "/api/sessions", &body))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::CREATED || resp.status() == StatusCode::OK,
        "upload {id} failed: {}",
        resp.status()
    );
}

#[tokio::test]
async fn project_crud_and_membership() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Empty list.
    let resp = app.clone().oneshot(get("/api/projects")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["projects"].as_array().unwrap().len(), 0);

    // Create -> 201.
    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/api/projects",
            &json!({ "name": "Alpha" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let proj = body_json(resp).await;
    let pid = proj["id"].as_str().unwrap().to_string();
    assert_eq!(proj["name"], json!("Alpha"));
    assert_eq!(proj["session_count"], json!(0));

    // List -> contains it.
    let resp = app.clone().oneshot(get("/api/projects")).await.unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["projects"].as_array().unwrap().len(), 1);

    // Assign a session to the project.
    upload_sample(&app, "it-sess-1").await;
    let resp = app
        .clone()
        .oneshot(json_req(
            "PUT",
            "/api/sessions/it-sess-1/project",
            &json!({ "project_id": pid }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET session reflects project_id.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["project_id"], json!(pid));

    // GET project -> member session present.
    let resp = app
        .clone()
        .oneshot(get(&format!("/api/projects/{pid}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["project"]["session_count"], json!(1));
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);

    // Clear the assignment (null).
    let resp = app
        .clone()
        .oneshot(json_req(
            "PUT",
            "/api/sessions/it-sess-1/project",
            &json!({ "project_id": Value::Null }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["project_id"], Value::Null);

    // Unknown project -> 400.
    let resp = app
        .clone()
        .oneshot(json_req(
            "PUT",
            "/api/sessions/it-sess-1/project",
            &json!({ "project_id": "missing" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Unknown session -> 404.
    let resp = app
        .clone()
        .oneshot(json_req(
            "PUT",
            "/api/sessions/ghost/project",
            &json!({ "project_id": pid }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Rename.
    let resp = app
        .clone()
        .oneshot(json_req(
            "PUT",
            &format!("/api/projects/{pid}"),
            &json!({ "name": "Beta" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["name"], json!("Beta"));

    // Rename a missing project -> 404.
    let resp = app
        .clone()
        .oneshot(json_req(
            "PUT",
            "/api/projects/nope",
            &json!({ "name": "X" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Delete -> 204, then gone.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/projects/{pid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app
        .clone()
        .oneshot(get(&format!("/api/projects/{pid}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn session_and_project_chats_create_list_get() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    upload_sample(&app, "it-sess-1").await;

    // Create a session chat.
    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/api/sessions/it-sess-1/chats",
            &json!({ "title": "Q about X" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let chat = body_json(resp).await;
    let chat_id = chat["id"].as_str().unwrap().to_string();
    assert_eq!(chat["session_id"], json!("it-sess-1"));
    assert_eq!(chat["title"], json!("Q about X"));
    assert_eq!(chat["shared"], json!(false));

    // List session chats.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1/chats"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["chats"].as_array().unwrap().len(), 1);

    // Session chats for a missing session -> 404.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/ghost/chats"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // GET the chat -> includes (empty) messages.
    let resp = app
        .clone()
        .oneshot(get(&format!("/api/chats/{chat_id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["chat"]["id"], json!(chat_id));
    assert_eq!(body["messages"].as_array().unwrap().len(), 0);

    // Now a project chat.
    let resp = app
        .clone()
        .oneshot(json_req("POST", "/api/projects", &json!({ "name": "P" })))
        .await
        .unwrap();
    let proj = body_json(resp).await;
    let pid = proj["id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            &format!("/api/projects/{pid}/chats"),
            &json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let pchat = body_json(resp).await;
    assert_eq!(pchat["project_id"], json!(pid));
    assert_eq!(pchat["title"], Value::Null);

    let resp = app
        .clone()
        .oneshot(get(&format!("/api/projects/{pid}/chats")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["chats"].as_array().unwrap().len(), 1);

    // Chats for a missing project -> 404.
    let resp = app
        .clone()
        .oneshot(get("/api/projects/nope/chats"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Unknown chat -> 404.
    let resp = app.clone().oneshot(get("/api/chats/nope")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_chat_message_503_when_llm_disabled() {
    // The default test config has no OPENROUTER_API_KEY, so `state.llm` is None.
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());

    upload_sample(&app, "it-sess-1").await;

    // Create a session chat to post into.
    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/api/sessions/it-sess-1/chats",
            &json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let chat_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    // Posting a message returns 503 (LLM not configured) and persists nothing.
    let resp = app
        .clone()
        .oneshot(json_req(
            "POST",
            &format!("/api/chats/{chat_id}/messages"),
            &json!({ "content": "what happened here?" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // The 503 short-circuits before persisting the user message.
    let with_messages = state
        .repo
        .get_chat_with_messages(&chat_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(with_messages.messages.len(), 0);
}

#[tokio::test]
async fn post_chat_message_authz_when_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state)
        .await
        .expect("build auth router");

    let register = |email: &str| {
        let body =
            serde_json::to_vec(&json!({ "email": email, "password": "hunter2pass" })).unwrap();
        Request::builder()
            .method("POST")
            .uri("/auth/register")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    // Owner registers + uploads a session + creates a chat.
    let resp = app
        .clone()
        .oneshot(register("dev@example.com"))
        .await
        .unwrap();
    let dev = session_cookie(&resp);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/chats")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(Body::from(serde_json::to_vec(&json!({})).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let chat_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let post_msg = |cookie: &str, chat: &str| {
        Request::builder()
            .method("POST")
            .uri(format!("/api/chats/{chat}/messages"))
            .header("content-type", "application/json")
            .header("cookie", cookie)
            .body(Body::from(
                serde_json::to_vec(&json!({ "content": "hi" })).unwrap(),
            ))
            .unwrap()
    };

    // A non-owner posting to the owner's chat -> 403 (owner-only write).
    let resp = app
        .clone()
        .oneshot(register("eve@example.com"))
        .await
        .unwrap();
    let eve = session_cookie(&resp);
    let resp = app.clone().oneshot(post_msg(&eve, &chat_id)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Absent chat -> 404.
    let resp = app.clone().oneshot(post_msg(&dev, "nope")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // The owner posting hits the LLM guard (no key configured) -> 503, NOT 403.
    // This proves the owner passes authz before the LLM check.
    let resp = app.clone().oneshot(post_msg(&dev, &chat_id)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn chat_read_authz_and_owner_only_mutations_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state)
        .await
        .expect("build auth router");

    let register = |email: &str| {
        let body =
            serde_json::to_vec(&json!({ "email": email, "password": "hunter2pass" })).unwrap();
        Request::builder()
            .method("POST")
            .uri("/auth/register")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    // Owner registers + uploads an organization-visible session so eve can view
    // the parent.
    let resp = app
        .clone()
        .oneshot(register("dev@example.com"))
        .await
        .unwrap();
    let dev = session_cookie(&resp);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    // Make the session organization-visible so a non-owner can view the parent.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/it-sess-1/sharing")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "visibility": "organization", "emails": [] }))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Owner creates a (private) chat.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/chats")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(Body::from(serde_json::to_vec(&json!({})).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let chat_id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let eve_get = |cookie: &str| {
        Request::builder()
            .uri(format!("/api/chats/{chat_id}"))
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    };

    // Owner can read.
    let resp = app.clone().oneshot(eve_get(&dev)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A different user cannot read the private chat -> 404 (hidden).
    let resp = app
        .clone()
        .oneshot(register("eve@example.com"))
        .await
        .unwrap();
    let eve = session_cookie(&resp);
    let resp = app.clone().oneshot(eve_get(&eve)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // eve cannot toggle shared (owner only) -> 403.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/chats/{chat_id}"))
                .header("content-type", "application/json")
                .header("cookie", &eve)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "shared": true })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Owner toggles shared = true.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/chats/{chat_id}"))
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "shared": true, "title": "Shared" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["shared"], json!(true));
    assert_eq!(body["title"], json!("Shared"));

    // Now eve (who can view the org-visible parent session) can read it.
    let resp = app.clone().oneshot(eve_get(&eve)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // eve still cannot delete (owner only) -> 403.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/chats/{chat_id}"))
                .header("cookie", &eve)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Owner deletes -> 204, then gone.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/chats/{chat_id}"))
                .header("cookie", &dev)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app.clone().oneshot(eve_get(&dev)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn project_mutations_owner_only_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state)
        .await
        .expect("build auth router");

    let register = |email: &str| {
        let body =
            serde_json::to_vec(&json!({ "email": email, "password": "hunter2pass" })).unwrap();
        Request::builder()
            .method("POST")
            .uri("/auth/register")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    let resp = app
        .clone()
        .oneshot(register("dev@example.com"))
        .await
        .unwrap();
    let dev = session_cookie(&resp);
    let resp = app
        .clone()
        .oneshot(register("eve@example.com"))
        .await
        .unwrap();
    let eve = session_cookie(&resp);

    // dev creates a project.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/projects")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "name": "Owned" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let pid = body_json(resp).await["id"].as_str().unwrap().to_string();

    // eve cannot rename -> 403.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/projects/{pid}"))
                .header("content-type", "application/json")
                .header("cookie", &eve)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "name": "Hijack" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // eve cannot delete -> 403.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/projects/{pid}"))
                .header("cookie", &eve)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // dev (owner) can delete -> 204.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/projects/{pid}"))
                .header("cookie", &dev)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_session_owner_only_when_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state)
        .await
        .expect("build auth router");

    let register = |email: &str| {
        let body =
            serde_json::to_vec(&json!({ "email": email, "password": "hunter2pass" })).unwrap();
        Request::builder()
            .method("POST")
            .uri("/auth/register")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    // Owner registers and uploads → the session's owner_email is the owner.
    let resp = app
        .clone()
        .oneshot(register("dev@example.com"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let dev = session_cookie(&resp);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // `uploaded_by` is recorded as the authenticated email, NOT the wire
    // `user@host` value (`tester@example.com`) the upload body carried.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/sessions/it-sess-1")
                .header("cookie", &dev)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["uploaded_by"], json!("dev@example.com"));
    assert_eq!(body["owner_email"], json!("dev@example.com"));

    // Deleting a session that doesn't exist → 404.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/does-not-exist")
                .header("cookie", &dev)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // A different user cannot delete the owner's session → 403.
    let resp = app
        .clone()
        .oneshot(register("eve@example.com"))
        .await
        .unwrap();
    let eve = session_cookie(&resp);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/it-sess-1")
                .header("cookie", &eve)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // The owner can delete it → 204, then it's gone.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/it-sess-1")
                .header("cookie", &dev)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/sessions/it-sess-1")
                .header("cookie", &dev)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `set_sharing` + `set_repos` are owner-only when auth is on (regression for the
/// access-control gap where any authenticated user could flip any session to
/// `organization` visibility or rewrite its repo links).
#[tokio::test]
async fn set_sharing_and_set_repos_owner_only_when_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state)
        .await
        .expect("build auth router");

    let register = |email: &str| {
        let body =
            serde_json::to_vec(&json!({ "email": email, "password": "hunter2pass" })).unwrap();
        Request::builder()
            .method("POST")
            .uri("/auth/register")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    // Owner registers + uploads (owner_email = dev@example.com).
    let resp = app
        .clone()
        .oneshot(register("dev@example.com"))
        .await
        .unwrap();
    let dev = session_cookie(&resp);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // A different authenticated user.
    let resp = app
        .clone()
        .oneshot(register("eve@example.com"))
        .await
        .unwrap();
    let eve = session_cookie(&resp);

    // Eve cannot change the owner's sharing → 403 (the headline privilege-
    // escalation: flipping a private session to organization-visible).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/it-sess-1/sharing")
                .header("content-type", "application/json")
                .header("cookie", &eve)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "visibility": "organization", "emails": [] }))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Eve cannot rewrite the owner's repo links → 403.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/it-sess-1/repos")
                .header("content-type", "application/json")
                .header("cookie", &eve)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "repos": [] })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // The owner CAN change sharing → 200, and the PUT echoes the new visibility
    // (confirming Eve's attempts above genuinely did not take effect).
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/sessions/it-sess-1/sharing")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "visibility": "organization", "emails": [] }))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["visibility"], json!("organization"));
}

/// Live-session author-owned sharing + per-viewer authz (auth on).
///
/// Covers: author registers a live session; non-author can't see/manage it
/// (404); author PUTs a read share + a read+write share; the read-sharee can
/// view (GET stream replay 200) but can't reply (403); the rw-sharee can reply
/// (200, delivered:false with no CLI attached); a stranger gets 404 on stream;
/// the share-management routes are author-only; and `GET /api/live` filters to
/// author+sharees and stamps the per-viewer flags.
#[tokio::test]
async fn live_sharing_and_authz_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state.clone())
        .await
        .expect("build auth router");

    let register = |email: &str| {
        let body =
            serde_json::to_vec(&json!({ "email": email, "password": "hunter2pass" })).unwrap();
        Request::builder()
            .method("POST")
            .uri("/auth/register")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    // Register three identities.
    let resp = app.clone().oneshot(register("author@x")).await.unwrap();
    let author = session_cookie(&resp);
    let resp = app.clone().oneshot(register("reader@x")).await.unwrap();
    let reader = session_cookie(&resp);
    let resp = app.clone().oneshot(register("writer@x")).await.unwrap();
    let writer = session_cookie(&resp);
    let resp = app.clone().oneshot(register("stranger@x")).await.unwrap();
    let stranger = session_cookie(&resp);

    // Author owns an interactive live session (seeded; the central
    // registration endpoint is retired).
    seed_live_as(&state, Some("author@x"), "live-x", "interactive").await;

    // Non-author can't read sharing (author-only) -> 403.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/live/live-x/sharing")
                .header("cookie", &reader)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Author sets the share list: reader (read) + writer (read+write).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/live/live-x/sharing")
                .header("content-type", "application/json")
                .header("cookie", &author)
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "shares": [
                            { "email": "reader@x", "can_write": false },
                            { "email": "writer@x", "can_write": true },
                        ]
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let shares = body["shares"].as_array().unwrap();
    assert_eq!(shares.len(), 2);
    // The write share grants interactive access (replies + keyboard).
    let writer_share = shares
        .iter()
        .find(|s| s["email"] == json!("writer@x"))
        .expect("writer share present");
    assert_eq!(writer_share["can_write"], json!(true));
    // can_control is gone from the wire entirely.
    assert!(writer_share.get("can_control").is_none());
    // The read share is not a writer.
    let reader_share = shares
        .iter()
        .find(|s| s["email"] == json!("reader@x"))
        .expect("reader share present");
    assert_eq!(reader_share["can_write"], json!(false));

    // Author GET sharing returns the same list.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/live/live-x/sharing")
                .header("cookie", &author)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Reader can view the stream (replay then live) -> 200.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/live/live-x/stream")
                .header("cookie", &reader)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Stranger can't view -> 404 (existence hidden).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/live/live-x/stream")
                .header("cookie", &stranger)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // The body-less outline stream is gated exactly like the full stream.
    for (cookie, expected) in [
        (&reader, StatusCode::OK),
        (&stranger, StatusCode::NOT_FOUND),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/live/live-x/stream?bodies=false")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), expected);
    }

    // The retired unsigned resize route is unavailable to every role.
    // Browser resize now goes through the signed host control path.
    let resize = |cookie: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/live/live-x/resize")
            .header("content-type", "application/json")
            .header("cookie", cookie)
            .body(Body::from(
                serde_json::to_vec(&json!({ "cols": 72, "rows": 20 })).unwrap(),
            ))
            .unwrap()
    };
    for cookie in [&reader, &writer, &author] {
        let resp = app.clone().oneshot(resize(cookie)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // GET /api/live filters + stamps per-viewer flags.
    let list_for = |cookie: &str| {
        Request::builder()
            .uri("/api/live")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    };

    // Author: sees it, viewer_is_author=true, can_write=true.
    let resp = app.clone().oneshot(list_for(&author)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let items = body["live_sessions"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["viewer_is_author"], json!(true));
    assert_eq!(items[0]["viewer_can_write"], json!(true));
    // viewer_can_control is gone from the wire entirely.
    assert!(items[0].get("viewer_can_control").is_none());
    // The flattened LiveSession fields are still present.
    assert_eq!(items[0]["id"], json!("live-x"));
    assert_eq!(items[0]["mode"], json!("interactive"));
    // Central attachment no longer exists, so the presence signal is always
    // false: a host's terminals live behind its own signed, end-to-end
    // channels, which the server cannot observe.
    assert_eq!(items[0]["cli_attached"], json!(false));

    // Reader: sees it, not author, can't write.
    let resp = app.clone().oneshot(list_for(&reader)).await.unwrap();
    let body = body_json(resp).await;
    let items = body["live_sessions"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["viewer_is_author"], json!(false));
    assert_eq!(items[0]["viewer_can_write"], json!(false));

    // Writer: sees it, not author, can write (which now includes keyboard).
    let resp = app.clone().oneshot(list_for(&writer)).await.unwrap();
    let body = body_json(resp).await;
    let items = body["live_sessions"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["viewer_is_author"], json!(false));
    assert_eq!(items[0]["viewer_can_write"], json!(true));

    // Stranger: empty list.
    let resp = app.clone().oneshot(list_for(&stranger)).await.unwrap();
    let body = body_json(resp).await;
    assert!(body["live_sessions"].as_array().unwrap().is_empty());

    // Non-author can't manage shares (PUT) -> 403.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/live/live-x/sharing")
                .header("content-type", "application/json")
                .header("cookie", &writer)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "shares": [] })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Sharing on an absent live session -> 404.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/live/ghost/sharing")
                .header("cookie", &author)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Old unsigned control endpoints are retired even for an authenticated owner.
#[tokio::test]
async fn legacy_control_routes_are_not_mounted() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state.clone()).await.unwrap();
    let owner = register_cookie(&app, "retired-control@x").await;
    seed_live_as(
        &state,
        Some("retired-control@x"),
        "retired-control",
        "interactive",
    )
    .await;
    for (method, path) in [
        ("GET", "/api/live/control/stream"),
        ("GET", "/api/live/control/available"),
        ("POST", "/api/live/start-new"),
        ("POST", "/api/sessions/retired-control/reopen"),
        ("POST", "/api/live/retired-control/repaint"),
        ("POST", "/api/live/retired-control/resize"),
        // The central live mirror and every central control/byte channel are
        // retired too: nothing the server stores or relays may reach a host
        // machine's process, terminal or arguments.
        ("POST", "/api/live"),
        ("POST", "/api/live/retired-control/events"),
        ("POST", "/api/live/retired-control/complete"),
        ("POST", "/api/live/retired-control/agent-session-id"),
        ("POST", "/api/live/retired-control/heartbeat"),
        ("POST", "/api/live/retired-control/replies"),
        ("GET", "/api/live/retired-control/replies/stream"),
        ("GET", "/api/live/retired-control/pty/ws"),
        ("GET", "/api/live/retired-control/host/ws"),
        ("GET", "/api/live/retired-control/text/stream"),
        ("POST", "/api/live/retired-control/clipboard"),
        ("GET", "/api/live/retired-control/clipboard/stream"),
        ("POST", "/api/live/retired-control/files"),
        ("GET", "/api/live/retired-control/files/stream"),
        ("GET", "/api/live/retired-control/files/t-1/preflight"),
        ("POST", "/api/live/retired-control/files/t-1/result"),
        // Central agent-messaging and PR relays: local, signed channels only.
        (
            "GET",
            "/api/agent-messaging/sessions/retired-control/identity",
        ),
        (
            "POST",
            "/api/agent-messaging/sessions/retired-control/messages",
        ),
        (
            "POST",
            "/api/agent-messaging/sessions/retired-control/inbox",
        ),
        (
            "GET",
            "/api/agent-messaging/sessions/retired-control/messages/m-1",
        ),
        (
            "POST",
            "/api/agent-messaging/sessions/retired-control/handoffs",
        ),
        (
            "POST",
            "/api/agent-messaging/sessions/retired-control/handoffs/c-1/claim",
        ),
        (
            "POST",
            "/api/agent-messaging/sessions/retired-control/handoffs/c-1/result",
        ),
        ("POST", "/api/agent-messaging/sessions/retired-control/end"),
        ("POST", "/api/sessions/retired-control/prs/reply"),
        ("GET", "/api/sessions/retired-control/prs/reply/stream"),
        ("POST", "/api/sessions/retired-control/prs/sync"),
        ("GET", "/api/prs/sync/stream"),
        ("GET", "/api/prs/sync/available"),
        ("GET", "/api/prs/watch/targets"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("cookie", &owner)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "retired route {method} {path} returned {}",
            response.status()
        );
    }
}
#[tokio::test]
async fn session_prs_link_list_delete() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Upload the session first.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // GET /api/sessions/it-sess-1/prs -> initially empty.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1/prs"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["prs"], json!([]));

    // POST /api/sessions/it-sess-1/prs with a valid PR URL.
    let link_body = Body::from(
        serde_json::to_vec(&json!({
            "url": "https://github.com/org/myrepo/pull/42"
        }))
        .unwrap(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs")
                .header("content-type", "application/json")
                .body(link_body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["owner"], json!("org"));
    assert_eq!(body["repo"], json!("myrepo"));
    assert_eq!(body["number"], json!(42));
    assert_eq!(body["url"], json!("https://github.com/org/myrepo/pull/42"));
    // The server never fetches status; a freshly-linked PR has no cache.
    assert_eq!(body["status"], json!(null));
    assert_eq!(body["status_fetched_at"], json!(null));

    // GET /api/sessions/it-sess-1/prs -> now lists the linked PR.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1/prs"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let prs = body["prs"].as_array().unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0]["number"], json!(42));
    assert_eq!(prs[0]["status"], json!(null));
    assert_eq!(prs[0]["status_fetched_at"], json!(null));

    // POST the same PR again -> idempotent (still only 1 in the list).
    let link_body = Body::from(
        serde_json::to_vec(&json!({
            "url": "https://github.com/org/myrepo/pull/42"
        }))
        .unwrap(),
    );
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs")
                .header("content-type", "application/json")
                .body(link_body)
                .unwrap(),
        )
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1/prs"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(
        body["prs"].as_array().unwrap().len(),
        1,
        "idempotent link must not create duplicates"
    );

    // DELETE /api/sessions/it-sess-1/prs?owner=org&repo=myrepo&number=42 -> 204.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/it-sess-1/prs?owner=org&repo=myrepo&number=42")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // GET after DELETE -> empty again.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1/prs"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["prs"], json!([]));

    // DELETE a non-existent PR -> 404.
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/it-sess-1/prs?owner=org&repo=myrepo&number=999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Linear refs in a session's messages are auto-detected on upload and surfaced
/// by `GET /api/sessions/{id}/linear-docs`; a ref can then be unlinked by URL.
#[tokio::test]
async fn session_linear_docs_autodetect_list_delete() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // A transcript that mentions a Linear document and a Linear issue (with a
    // trailing title slug — canonicalised to `…/issue/ENG-7`).
    let sample = concat!(
        r#"{"type":"ai-title","aiTitle":"Linear session","sessionId":"it-lin-1"}"#,
        "\n",
        r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","sessionId":"it-lin-1","gitBranch":"main","cwd":"/tmp/proj","message":{"role":"user","content":"spec at https://linear.app/example/document/spec-1 tracked by https://linear.app/example/issue/ENG-7/fix-it"}}"#,
        "\n",
    );
    let session = conversations_core::parse_session_str(Some("it-lin-1"), sample).unwrap();
    let upload = json!({
        "uploaded_by": "tester@example.com",
        "source_host": "ci-host",
        "session": session,
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&upload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // GET lists both auto-detected refs (the issue is canonicalised, slug dropped).
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-lin-1/linear-docs"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let docs = body["linear_docs"].as_array().unwrap();
    assert_eq!(docs.len(), 2, "both refs auto-detected: {docs:?}");
    let urls: Vec<&str> = docs.iter().map(|d| d["url"].as_str().unwrap()).collect();
    assert!(urls.contains(&"https://linear.app/example/document/spec-1"));
    assert!(urls.contains(&"https://linear.app/example/issue/ENG-7"));
    let issue = docs
        .iter()
        .find(|d| d["kind"] == "issue")
        .expect("issue present");
    assert_eq!(issue["identifier"], json!("ENG-7"));
    assert_eq!(issue["workspace"], json!("example"));

    // DELETE one by its canonical URL -> 204, then it's gone.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/it-lin-1/linear-docs?url=https://linear.app/example/document/spec-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-lin-1/linear-docs"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let docs = body["linear_docs"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(
        docs[0]["url"],
        json!("https://linear.app/example/issue/ENG-7")
    );

    // DELETE a non-existent ref -> 404.
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/it-lin-1/linear-docs?url=https://linear.app/example/document/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Previously saved escaped variants must display and unlink as one clean ref.
#[tokio::test]
async fn session_linear_docs_normalizes_saved_escaped_variants() {
    use conversations_server::store::SessionLinearDoc;
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());
    let sample = concat!(
        r#"{"type":"user","uuid":"u1","sessionId":"it-lin-escaped","timestamp":"2026-06-05T10:00:00.000Z","message":{"role":"user","content":"Linear references"}}"#,
        "\n",
    );
    let session = conversations_core::parse_session_str(Some("it-lin-escaped"), sample).unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(
                        &json!({"session": session, "uploaded_by": "tester@example.com"}),
                    )
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    // Seed historical rows directly: new detection must not create these anymore.
    let docs: Vec<_> = ["", r"\\\", r"\n", r"\\\n\n", r".\\\", r".\\\\n\\\\n"]
        .into_iter()
        .map(|suffix| SessionLinearDoc {
            url: format!("https://linear.app/example/issue/ENG-2422{suffix}"),
            workspace: "example".into(),
            kind: "issue".into(),
            identifier: format!("ENG-2422{suffix}"),
        })
        .collect();
    state
        .repo
        .auto_link_session_linear_docs("it-lin-escaped", &docs)
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(get("/api/sessions/it-lin-escaped/linear-docs"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["linear_docs"],
        json!([{
            "url": "https://linear.app/example/issue/ENG-2422", "workspace": "example", "kind": "issue", "identifier": "ENG-2422"
        }])
    );
    let response = app.clone().oneshot(Request::builder().method("DELETE")
        .uri("/api/sessions/it-lin-escaped/linear-docs?url=https://linear.app/example/issue/ENG-2422")
        .body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(state
        .repo
        .list_session_linear_docs("it-lin-escaped")
        .await
        .unwrap()
        .is_empty());
    let response = app
        .oneshot(get("/api/sessions/it-lin-escaped/linear-docs"))
        .await
        .unwrap();
    assert_eq!(body_json(response).await["linear_docs"], json!([]));
}

/// A ref deleted *before* the first list must stay gone: upload stamps the
/// session scanned, so the GET-time backfill never re-detects (resurrects) it
/// from the original transcript. Regression guard for the DELETE-durability
/// edge case CodeRabbit flagged.
#[tokio::test]
async fn session_linear_docs_delete_before_first_list_is_durable() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    let sample = concat!(
        r#"{"type":"ai-title","aiTitle":"Linear session","sessionId":"it-lin-2"}"#,
        "\n",
        r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","sessionId":"it-lin-2","gitBranch":"main","cwd":"/tmp/proj","message":{"role":"user","content":"spec at https://linear.app/example/document/spec-1 tracked by https://linear.app/example/issue/ENG-7/fix-it"}}"#,
        "\n",
    );
    let session = conversations_core::parse_session_str(Some("it-lin-2"), sample).unwrap();
    let upload = json!({
        "uploaded_by": "tester@example.com",
        "source_host": "ci-host",
        "session": session,
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&upload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // DELETE one ref *before* ever listing (so no backfill has run yet).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/sessions/it-lin-2/linear-docs?url=https://linear.app/example/document/spec-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // The first GET (which triggers backfill) must NOT resurrect the deleted ref.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-lin-2/linear-docs"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let docs = body["linear_docs"].as_array().unwrap();
    let urls: Vec<&str> = docs.iter().map(|d| d["url"].as_str().unwrap()).collect();
    assert!(
        !urls.contains(&"https://linear.app/example/document/spec-1"),
        "deleted ref must not be resurrected by backfill: {docs:?}"
    );
    assert!(urls.contains(&"https://linear.app/example/issue/ENG-7"));
}

#[tokio::test]
async fn session_pr_status_push_then_cached() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Upload the session.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Link a PR.
    let link_body = Body::from(
        serde_json::to_vec(&json!({ "url": "https://github.com/org/myrepo/pull/42" })).unwrap(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs")
                .header("content-type", "application/json")
                .body(link_body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // POST status to an UNLINKED PR -> 404.
    let status_body = Body::from(
        serde_json::to_vec(&json!({
            "owner": "org",
            "repo": "myrepo",
            "number": 999,
            "status": { "state": "open", "merged": false, "draft": false, "title": "ghost" }
        }))
        .unwrap(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs/status")
                .header("content-type", "application/json")
                .body(status_body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // POST status for the linked PR -> 204.
    let status_body = Body::from(
        serde_json::to_vec(&json!({
            "owner": "org",
            "repo": "myrepo",
            "number": 42,
            "status": { "state": "open", "merged": false, "draft": true, "title": "Add X", "review_decision": "CHANGES_REQUESTED" }
        }))
        .unwrap(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs/status")
                .header("content-type", "application/json")
                .body(status_body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // GET -> now shows the cached status + a non-null fetched_at.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1/prs"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let prs = body["prs"].as_array().unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0]["status"]["state"], json!("open"));
    assert_eq!(prs[0]["status"]["merged"], json!(false));
    assert_eq!(prs[0]["status"]["draft"], json!(true));
    assert_eq!(prs[0]["status"]["title"], json!("Add X"));
    assert_eq!(
        prs[0]["status"]["review_decision"],
        json!("CHANGES_REQUESTED")
    );
    assert!(
        prs[0]["status_fetched_at"].is_string(),
        "status_fetched_at must be a non-null RFC3339 string after a push"
    );

    // POST status to an unknown session -> 404.
    let status_body = Body::from(
        serde_json::to_vec(&json!({
            "owner": "org", "repo": "myrepo", "number": 42,
            "status": { "state": "open", "merged": false, "draft": false, "title": "x" }
        }))
        .unwrap(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/ghost/prs/status")
                .header("content-type", "application/json")
                .body(status_body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Authz: with auth ON, a non-viewer cannot push PR status to a restricted
/// session — the view gate (matching the link/unlink routes) returns 404 so a
/// non-viewer never learns the session exists.
#[tokio::test]
async fn session_pr_status_push_authz_when_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state)
        .await
        .expect("build auth router");

    let register = |email: &str| {
        let body =
            serde_json::to_vec(&json!({ "email": email, "password": "hunter2pass" })).unwrap();
        Request::builder()
            .method("POST")
            .uri("/auth/register")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    // Owner registers, uploads a (restricted) session, links a PR.
    let resp = app
        .clone()
        .oneshot(register("dev@example.com"))
        .await
        .unwrap();
    let dev = session_cookie(&resp);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "url": "https://github.com/org/myrepo/pull/42" }))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let push = |cookie: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/sessions/it-sess-1/prs/status")
            .header("content-type", "application/json")
            .header("cookie", cookie)
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "owner": "org", "repo": "myrepo", "number": 42,
                    "status": { "state": "open", "merged": false, "draft": false, "title": "X" }
                }))
                .unwrap(),
            ))
            .unwrap()
    };

    // A non-viewer of the restricted session -> 404 (view gate).
    let resp = app
        .clone()
        .oneshot(register("eve@example.com"))
        .await
        .unwrap();
    let eve = session_cookie(&resp);
    let resp = app.clone().oneshot(push(&eve)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // The owner can push -> 204.
    let resp = app.oneshot(push(&dev)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn session_prs_invalid_url_is_400() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Upload the session.
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();

    // A non-PR GitHub URL -> 400.
    let link_body = Body::from(
        serde_json::to_vec(&json!({
            "url": "https://github.com/org/repo/issues/5"
        }))
        .unwrap(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs")
                .header("content-type", "application/json")
                .body(link_body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Garbage string -> 400.
    let link_body = Body::from(serde_json::to_vec(&json!({ "url": "not a url at all" })).unwrap());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs")
                .header("content-type", "application/json")
                .body(link_body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn session_prs_unknown_session_is_404() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // GET on unknown session -> 404.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/ghost-session/prs"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // POST on unknown session -> 404.
    let link_body = Body::from(
        serde_json::to_vec(&json!({
            "url": "https://github.com/org/repo/pull/1"
        }))
        .unwrap(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/ghost-session/prs")
                .header("content-type", "application/json")
                .body(link_body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A session JSONL transcript whose message text mentions a full PR URL plus a
/// `PR #N` reference, used to exercise PR auto-detection. The repo is supplied
/// separately at upload time so number-only refs resolve.
fn pr_mention_sample(id: &str) -> String {
    format!(
        concat!(
            r#"{{"type":"ai-title","aiTitle":"PR talk","sessionId":"{id}"}}"#,
            "\n",
            r#"{{"type":"user","uuid":"pu1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","sessionId":"{id}","gitBranch":"main","cwd":"/tmp/proj","message":{{"role":"user","content":"please look at PR #193 thanks"}}}}"#,
            "\n",
            r#"{{"type":"assistant","uuid":"pa1","parentUuid":"pu1","timestamp":"2026-06-05T10:00:02.000Z","sessionId":"{id}","message":{{"role":"assistant","model":"claude-opus-4-8","content":[{{"type":"text","text":"done, see https://github.com/acme/widgets/pull/200"}}]}}}}"#,
            "\n",
        ),
        id = id
    )
}
#[tokio::test]
async fn pr_autodetect_ignores_tool_output_on_upload_and_backfill() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());
    let noise = "Memory search: https://github.com/Example-Org/dotfiles-template/pull/32";
    let mut session =
        conversations_core::parse_session_str(Some("pr-noise"), &pr_mention_sample("pr-noise"))
            .unwrap();
    session.messages.truncate(1);
    let message = &mut session.messages[0];
    message.blocks = vec![
        conversations_core::ContentBlock::Text {
            text: "See https://github.com/acme/widgets/pull/200".into(),
        },
        conversations_core::ContentBlock::ToolResult {
            tool_use_id: Some("memory-search".into()),
            content: noise.into(),
        },
    ];
    message
        .blocks
        .push(conversations_core::ContentBlock::Thinking {
            thinking: "Old example: https://github.com/acme/widgets/pull/999".into(),
        });
    message.kind = conversations_core::MessageKind::Mixed;
    message.raw = json!({ "role": "user", "content": message.blocks });
    message.text = format!("See https://github.com/acme/widgets/pull/200\n{noise}\nOld example: https://github.com/acme/widgets/pull/999");

    for path in ["upload", "backfill"] {
        let sid = format!("pr-noise-{path}");
        session.id = sid.clone();
        match path {
            "upload" => {
                let response = app
                    .clone()
                    .oneshot(json_req(
                        "POST",
                        "/api/sessions",
                        &json!({
                            "uploaded_by": "tester@x", "session": session, "repos": []
                        }),
                    ))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::CREATED);
            }
            _ => {
                state
                    .repo
                    .upsert_session("tester@x", None, None, &session)
                    .await
                    .unwrap();
            }
        }
        if path != "backfill" {
            let prs = state.repo.list_session_prs(&sid).await.unwrap();
            assert_eq!(prs.len(), 1, "{path}: check ingestion before backfill");
            assert_eq!(prs[0].url, "https://github.com/acme/widgets/pull/200");
        }
        let response = app
            .clone()
            .oneshot(get(&format!("/api/sessions/{sid}/prs")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let prs = body["prs"].as_array().unwrap();
        assert_eq!(
            prs.len(),
            1,
            "{path}: tool output must not create a PR link: {prs:?}"
        );
        assert_eq!(prs[0]["url"], "https://github.com/acme/widgets/pull/200");
    }
}

/// Auto-detect: an UPLOAD whose session resolves to exactly one repo and whose
/// messages mention `PR #193` + a full PR URL links both at ingest, flagged
/// `detected: true`. A subsequently mentioned number that matches a manual link
/// must NOT be downgraded.
#[tokio::test]
async fn pr_autodetect_on_upload_and_backfill() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());

    // Pre-create a MANUAL link for acme/widgets#193 (detected = false). The
    // upload below also mentions PR #193 — auto-link must not clobber it.
    // Seed the session row first via the store so the FK is satisfied.
    let sid = "it-pr-auto";
    let session =
        conversations_core::parse_session_str(Some(sid), &pr_mention_sample(sid)).unwrap();
    state
        .repo
        .upsert_session("tester@x", None, Some("tester@x"), &session)
        .await
        .unwrap();
    state
        .repo
        .link_session_pr(
            sid,
            &conversations_server::store::SessionPr {
                url: "https://github.com/acme/widgets/pull/193".into(),
                owner: "acme".into(),
                repo: "widgets".into(),
                number: 193,
            },
        )
        .await
        .unwrap();

    // Now UPLOAD the same id with a repo link resolving to a single
    // (owner, repo) = (acme, widgets), exercising ingest-time detection.
    let body = json!({
        "uploaded_by": "tester@x",
        "session": conversations_core::parse_session_str(Some(sid), &pr_mention_sample(sid)).unwrap(),
        "repos": [
            {"repo": "widgets", "remote_url": "git@github.com:acme/widgets.git", "branch": "main"}
        ]
    });
    let resp = app
        .clone()
        .oneshot(json_req("POST", "/api/sessions", &body))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::OK || resp.status() == StatusCode::CREATED,
        "upload failed: {}",
        resp.status()
    );

    // GET the PR list — backfill also runs (first GET), but ingest already
    // linked. Expect TWO PRs: #193 (manual, detected=false) + #200 (auto, URL).
    let resp = app
        .clone()
        .oneshot(get(&format!("/api/sessions/{sid}/prs")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let prs = body["prs"].as_array().unwrap();
    assert_eq!(prs.len(), 2, "expected #193 + #200, got {prs:?}");

    let pr193 = prs.iter().find(|p| p["number"] == 193).unwrap();
    assert_eq!(
        pr193["detected"],
        json!(false),
        "manual #193 must NOT be downgraded to detected"
    );
    let pr200 = prs.iter().find(|p| p["number"] == 200).unwrap();
    assert_eq!(pr200["detected"], json!(true), "auto-linked #200 (URL)");
    assert_eq!(pr200["owner"], json!("acme"));
    assert_eq!(pr200["repo"], json!("widgets"));
    assert_eq!(
        pr200["url"],
        json!("https://github.com/acme/widgets/pull/200")
    );
}

/// Fix 1 — a bare `PR #N` in a session whose only repo is a monorepo CONTAINER
/// (CLI-flagged `is_container`) must NOT link: the number refers to a sub-repo
/// PR, not a non-existent top-level one. The container repo is excluded from
/// single-repo PR-context resolution, so there is no context and `#N` is skipped.
#[tokio::test]
async fn pr_autodetect_skips_bare_ref_in_container_repo() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());

    let sid = "it-pr-container";
    let jsonl = format!(
        concat!(
            r#"{{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","sessionId":"{id}","gitBranch":"main","cwd":"/tmp/mono","message":{{"role":"user","content":"check PR #205 please"}}}}"#,
            "\n"
        ),
        id = sid
    );
    let body = json!({
        "uploaded_by": "tester@x",
        "session": conversations_core::parse_session_str(Some(sid), &jsonl).unwrap(),
        "repos": [
            {
                "repo": "example-workspace",
                "remote_url": "git@github.com:Example-Org/example-workspace.git",
                "branch": "main",
                "is_container": true
            }
        ]
    });
    let resp = app
        .clone()
        .oneshot(json_req("POST", "/api/sessions", &body))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::OK || resp.status() == StatusCode::CREATED,
        "upload failed: {}",
        resp.status()
    );

    let resp = app
        .clone()
        .oneshot(get(&format!("/api/sessions/{sid}/prs")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let prs = body["prs"].as_array().unwrap();
    assert!(
        prs.is_empty(),
        "bare `PR #205` in a container repo must not link, got {prs:?}"
    );
}

/// Fix 2 — a full PR URL whose owner is NOT one of the session's repo owners is
/// not linked (e.g. a test-fixture / doc-example URL quoted in a transcript),
/// while a same-org URL for a different repo still links.
#[tokio::test]
async fn pr_autodetect_gates_full_url_by_owner() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());

    let sid = "it-pr-offorg";
    let jsonl = format!(
        concat!(
            r#"{{"type":"assistant","uuid":"a1","parentUuid":null,"timestamp":"2026-06-05T10:00:00.000Z","sessionId":"{id}","message":{{"role":"assistant","model":"claude-opus-4-8","content":[{{"type":"text","text":"fixture https://github.com/acme/widgets/pull/42 ; real https://github.com/Example-Org/example-deploy/pull/739"}}]}}}}"#,
            "\n"
        ),
        id = sid
    );
    let body = json!({
        "uploaded_by": "tester@x",
        "session": conversations_core::parse_session_str(Some(sid), &jsonl).unwrap(),
        "repos": [
            {
                "repo": "example-platform",
                "remote_url": "git@github.com:Example-Org/example-platform.git",
                "branch": "main"
            }
        ]
    });
    let resp = app
        .clone()
        .oneshot(json_req("POST", "/api/sessions", &body))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::OK || resp.status() == StatusCode::CREATED,
        "upload failed: {}",
        resp.status()
    );

    let resp = app
        .clone()
        .oneshot(get(&format!("/api/sessions/{sid}/prs")))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let prs = body["prs"].as_array().unwrap();
    assert_eq!(
        prs.len(),
        1,
        "only the same-org URL should link, got {prs:?}"
    );
    assert_eq!(prs[0]["owner"], json!("Example-Org"));
    assert_eq!(prs[0]["repo"], json!("example-deploy"));
    assert_eq!(prs[0]["number"], json!(739));
}

/// Auto-detect backfill: a session whose messages already mention PR refs but
/// that has NEVER been PR-scanned (no ingest-time detection ran) gets them
/// linked on the FIRST PR-list GET, then never re-scanned.
#[tokio::test]
async fn pr_autodetect_backfill_first_get_only() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());

    // Seed a session + repo link directly through the store (NO ingest-time
    // detection), so backfill is the only thing that can link the refs.
    let sid = "it-pr-backfill";
    let session =
        conversations_core::parse_session_str(Some(sid), &pr_mention_sample(sid)).unwrap();
    state
        .repo
        .upsert_session("tester@x", None, Some("tester@x"), &session)
        .await
        .unwrap();
    state
        .repo
        .set_repos(
            sid,
            &[conversations_server::store::RepoLinkInput {
                repo: "widgets".into(),
                remote_url: Some("https://github.com/acme/widgets".into()),
                branch: "main".into(),
                pr_url: None,
            }],
        )
        .await
        .unwrap();

    // Nothing linked yet (no ingest ran, no GET yet).
    assert!(state.repo.list_session_prs(sid).await.unwrap().is_empty());
    assert!(state.repo.pr_scanned_at(sid).await.unwrap().is_none());

    // First GET → backfill links #193 (via repo_ctx) + #200 (URL), and stamps.
    let resp = app
        .clone()
        .oneshot(get(&format!("/api/sessions/{sid}/prs")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let prs = body["prs"].as_array().unwrap();
    assert_eq!(prs.len(), 2, "backfill must link both refs, got {prs:?}");
    assert!(prs.iter().all(|p| p["detected"] == json!(true)));
    assert!(
        state.repo.pr_scanned_at(sid).await.unwrap().is_some(),
        "backfill must stamp pr_scanned_at"
    );
}

// ---------------------------------------------------------------------------
// /api/llm/models response parse (unit test — no network)
// ---------------------------------------------------------------------------

#[test]
fn llm_models_response_parse() {
    // Sample body matching OpenRouter's /models response schema.
    let body = r#"{
        "data": [
            {"id": "anthropic/claude-sonnet-4", "name": "Claude Sonnet 4", "context_length": 200000},
            {"id": "openai/gpt-4o", "name": "GPT-4o", "context_length": 128000},
            {"id": "unknown-model"}
        ]
    }"#;

    #[derive(serde::Deserialize)]
    struct OrModelsResponse {
        #[serde(default)]
        data: Vec<OrModel>,
    }
    #[derive(serde::Deserialize)]
    struct OrModel {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        context_length: Option<u64>,
    }

    let parsed: OrModelsResponse = serde_json::from_str(body).unwrap();
    assert_eq!(parsed.data.len(), 3);
    assert_eq!(parsed.data[0].id, "anthropic/claude-sonnet-4");
    assert_eq!(parsed.data[0].name, "Claude Sonnet 4");
    assert_eq!(parsed.data[0].context_length, Some(200_000));
    // Third entry has no name/context_length -> defaults.
    assert_eq!(parsed.data[2].id, "unknown-model");
    assert!(parsed.data[2].name.is_empty());
    assert!(parsed.data[2].context_length.is_none());
}

/// Comments push (204) then list returns them sorted; the CLI re-push of the
/// same comment_id dedupes. Auth-off path, mirroring `session_pr_status_*`.
#[tokio::test]
async fn session_pr_comments_push_then_listed() {
    let (state, _tmp) = test_state().await;
    let mut change_events = state.live.subscribe("it-sess-1");
    let app = build_router(state);

    // Upload the session.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Initially empty.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1/prs/comments"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["comments"].as_array().unwrap().is_empty());

    // Push two comments out of order -> 204.
    let push_body = Body::from(
        serde_json::to_vec(&json!({
            "owner": "org",
            "repo": "myrepo",
            "number": 42,
            "comments": [
                {
                    "comment_id": "200", "kind": "review", "author": "bob",
                    "body": "second", "html_url": "https://github.com/org/myrepo/pull/42#c200",
                    "created_at": "2024-01-02T00:00:00Z"
                },
                {
                    "comment_id": "100", "kind": "issue", "author": "alice",
                    "body": "first", "html_url": "https://github.com/org/myrepo/pull/42#c100",
                    "created_at": "2024-01-01T00:00:00Z"
                }
            ]
        }))
        .unwrap(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs/comments")
                .header("content-type", "application/json")
                .body(push_body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(matches!(
        change_events.try_recv(),
        Ok(conversations_server::live::LiveEvent::PrCommentsChanged)
    ));

    // GET -> sorted by created_at ascending, with ingested_at stamped.
    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1/prs/comments"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let comments = body["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 2);
    assert_eq!(comments[0]["comment_id"], json!("100"));
    assert_eq!(comments[0]["kind"], json!("issue"));
    assert_eq!(comments[0]["author"], json!("alice"));
    assert_eq!(comments[0]["body"], json!("first"));
    assert_eq!(comments[0]["owner"], json!("org"));
    assert_eq!(comments[0]["number"], json!(42));
    assert!(comments[0]["ingested_at"].is_string());
    assert_eq!(comments[1]["comment_id"], json!("200"));
    let cursor = comments[0]["revision"].as_i64().unwrap();

    // An unchanged CLI snapshot must not produce a new change for the viewer.
    let unchanged = Body::from(
        serde_json::to_vec(&json!({
            "owner": "org", "repo": "myrepo", "number": 42,
            "comments": [{
                "comment_id": "100", "kind": "issue", "author": "alice",
                "body": "first", "html_url": "https://github.com/org/myrepo/pull/42#c100",
                "created_at": "2024-01-01T00:00:00Z"
            }]
        }))
        .unwrap(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs/comments")
                .header("content-type", "application/json")
                .body(unchanged)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(matches!(
        change_events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/sessions/it-sess-1/prs/comments?since={cursor}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(body_json(resp).await["comments"]
        .as_array()
        .unwrap()
        .is_empty());

    // Re-push the same comment_id with a new body -> dedupes + refreshes.
    let push_body = Body::from(
        serde_json::to_vec(&json!({
            "owner": "org", "repo": "myrepo", "number": 42,
            "comments": [{
                "comment_id": "100", "kind": "issue", "author": "alice",
                "body": "first-edited", "html_url": "https://github.com/org/myrepo/pull/42#c100",
                "created_at": "2024-01-01T00:00:00Z"
            }]
        }))
        .unwrap(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions/it-sess-1/prs/comments")
                .header("content-type", "application/json")
                .body(push_body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(get("/api/sessions/it-sess-1/prs/comments"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    let comments = body["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 2, "re-push of same comment_id must dedupe");
    assert_eq!(comments[0]["body"], json!("first-edited"));

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/sessions/it-sess-1/prs/comments?since={cursor}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let changed = body_json(resp).await;
    assert_eq!(changed["comments"].as_array().unwrap().len(), 1);
    assert_eq!(changed["comments"][0]["body"], "first-edited");

    let resp = app
        .oneshot(get("/api/sessions/it-sess-1/prs/comments/preview"))
        .await
        .unwrap();
    let previews = body_json(resp).await;
    assert_eq!(previews["comments"].as_array().unwrap().len(), 2);
}

/// Ingest defense: the SPA renders `html_url` as a link, so a pushed comment
/// carrying a non-http(s) URL (`javascript:`, `data:`, relative) must be
/// rejected with 400 instead of being planted into the cache.
#[tokio::test]
async fn session_pr_comments_rejects_non_http_html_url() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // Upload the session.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    for bad_url in [
        "javascript:alert(document.domain)",
        "data:text/html,<script>1</script>",
        "/relative/path",
    ] {
        let push_body = Body::from(
            serde_json::to_vec(&json!({
                "owner": "org", "repo": "myrepo", "number": 42,
                "comments": [{
                    "comment_id": "1", "kind": "issue", "author": "mallory",
                    "body": "pwn", "html_url": bad_url,
                    "created_at": "2024-01-01T00:00:00Z"
                }]
            }))
            .unwrap(),
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sessions/it-sess-1/prs/comments")
                    .header("content-type", "application/json")
                    .body(push_body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "html_url {bad_url:?} must be rejected"
        );
    }

    // Nothing was stored.
    let resp = app
        .oneshot(get("/api/sessions/it-sess-1/prs/comments"))
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert!(body["comments"].as_array().unwrap().is_empty());
}

/// Authz: with auth ON, a non-viewer cannot push or list PR comments on a
/// restricted session — the view gate returns 404 (mirrors the status push).
#[tokio::test]
async fn session_pr_comments_view_gated_when_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state)
        .await
        .expect("build auth router");

    let register = |email: &str| {
        let body =
            serde_json::to_vec(&json!({ "email": email, "password": "hunter2pass" })).unwrap();
        Request::builder()
            .method("POST")
            .uri("/auth/register")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    // Owner registers + uploads a (restricted) session.
    let resp = app
        .clone()
        .oneshot(register("dev@example.com"))
        .await
        .unwrap();
    let dev = session_cookie(&resp);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header("cookie", &dev)
                .body(upload_body())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let push = |cookie: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/sessions/it-sess-1/prs/comments")
            .header("content-type", "application/json")
            .header("cookie", cookie)
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "owner": "org", "repo": "myrepo", "number": 42,
                    "comments": [{
                        "comment_id": "1", "kind": "issue", "author": "a",
                        "body": "hi", "html_url": "https://x", "created_at": "2024-01-01T00:00:00Z"
                    }]
                }))
                .unwrap(),
            ))
            .unwrap()
    };
    let list = |cookie: &str| {
        Request::builder()
            .uri("/api/sessions/it-sess-1/prs/comments")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap()
    };

    // Non-viewer -> 404 on both push and list (view gate hides existence).
    let resp = app
        .clone()
        .oneshot(register("eve@example.com"))
        .await
        .unwrap();
    let eve = session_cookie(&resp);
    let resp = app.clone().oneshot(push(&eve)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app.clone().oneshot(list(&eve)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    for suffix in ["?since=0", "/preview"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/it-sess-1/prs/comments{suffix}"))
                    .header("cookie", &eve)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // Owner can push -> 204, and list -> 200.
    let resp = app.clone().oneshot(push(&dev)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app.oneshot(list(&dev)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
#[tokio::test]
async fn user_settings_get_empty_and_put_normalizes() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // GET with no viewer -> empty list, 200 (never errors).
    let resp = app
        .clone()
        .oneshot(get("/api/user/settings"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["container_paths"], json!([]));
    assert_eq!(body["default_agent"], json!("claude"));
    assert!(body.get("codex_sandbox_mode").is_none());
    assert!(body.get("codex_approval_policy").is_none());
    assert_eq!(body["opencode_auto"], json!(false));
    assert_eq!(body["adk_reopen"], json!(false));
    assert_eq!(body["adk_statusline"], json!(false));

    // PUT (auth off) with a viewer via X-Demo-User; trailing slash + dup + blank.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/user/settings")
                .header("content-type", "application/json")
                .header("x-demo-user", "alice@x")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "default_agent": "codex",
                        "container_paths": [
                            "/Users/alice/example/",
                            "/Users/alice/example",
                            "   ",
                            "/Users/alice/other"
                        ],
                        "codex_sandbox_mode": "workspace-write",
                        "codex_approval_policy": "on-request",
                        "opencode_auto": true,
                        "adk_reopen": true,
                        "adk_statusline": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["container_paths"],
        json!(["/Users/alice/example", "/Users/alice/other"])
    );
    assert_eq!(body["default_agent"], json!("codex"));
    assert_eq!(body["codex_sandbox_mode"], json!("workspace-write"));
    assert_eq!(body["codex_approval_policy"], json!("on-request"));
    assert_eq!(body["opencode_auto"], json!(true));
    assert_eq!(body["adk_reopen"], json!(true));
    assert_eq!(body["adk_statusline"], json!(true));

    // GET back (same viewer) reflects the stored normalized set.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/user/settings")
                .header("x-demo-user", "alice@x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(
        body["container_paths"],
        json!(["/Users/alice/example", "/Users/alice/other"])
    );
    assert_eq!(body["default_agent"], json!("codex"));
    assert_eq!(body["codex_sandbox_mode"], json!("workspace-write"));
    assert_eq!(body["codex_approval_policy"], json!("on-request"));
    assert_eq!(body["opencode_auto"], json!(true));
    assert_eq!(body["adk_reopen"], json!(true));
    assert_eq!(body["adk_statusline"], json!(true));
}

#[tokio::test]
async fn user_settings_rejects_invalid_codex_launch_settings() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/user/settings")
                .header("content-type", "application/json")
                .header("x-demo-user", "alice@x")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "container_paths": [],
                        "default_agent": "codex",
                        "codex_sandbox_mode": "unsafe"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// PUT stores the opaque inline-code-colour blob and GET echoes it back
/// verbatim; omitting it on a later PUT clears it.
#[tokio::test]
async fn user_settings_inline_code_colors_roundtrip() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state);

    // No viewer -> the field is omitted entirely (serde skips None).
    let body = body_json(
        app.clone()
            .oneshot(get("/api/user/settings"))
            .await
            .unwrap(),
    )
    .await;
    assert!(body.get("inline_code_colors").is_none());

    // PUT a colour config for a demo viewer.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/user/settings")
                .header("content-type", "application/json")
                .header("x-demo-user", "alice@x")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "container_paths": [],
                        "default_agent": "claude",
                        "inline_code_colors": { "palette": [], "rgb": [11647993] }
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["inline_code_colors"],
        json!({ "palette": [], "rgb": [11647993] })
    );

    // GET back reflects it.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/user/settings")
                .header("x-demo-user", "alice@x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(
        body["inline_code_colors"],
        json!({ "palette": [], "rgb": [11647993] })
    );

    // PUT again WITHOUT the field clears it (the field is then omitted on GET).
    app.clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/user/settings")
                .header("content-type", "application/json")
                .header("x-demo-user", "alice@x")
                .body(Body::from(
                    serde_json::to_vec(
                        &json!({ "container_paths": [], "default_agent": "claude" }),
                    )
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/user/settings")
                .header("x-demo-user", "alice@x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert!(body.get("inline_code_colors").is_none());
}

/// PUT settings without an authenticated viewer (auth ON, no cookie) -> 401.
#[tokio::test]
async fn user_settings_put_unauthenticated_is_401() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state)
        .await
        .expect("build auth router");

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/user/settings")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "container_paths": ["/x"] })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A session with 2 repos where 1 is the uploader's container: at upload the
/// container is excluded from PR-ctx, so a bare "PR #N" resolves to the OTHER
/// repo and links.
#[tokio::test]
async fn container_excluded_from_pr_ctx_lets_bare_ref_resolve() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());

    // Uploader marks /home/dev/mono as a container repo.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/user/settings")
                .header("content-type", "application/json")
                .header("x-demo-user", "dev@x")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "container_paths": ["/home/dev/mono"] })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Upload a session that mentions bare "PR #193" with TWO repos: the monorepo
    // container and one work sub-repo (acme/widgets).
    let sid = "it-container-ctx";
    let body = json!({
        "uploaded_by": "dev@x",
        "session": conversations_core::parse_session_str(Some(sid), &pr_mention_sample(sid)).unwrap(),
        "repos": [
            {"repo": "mono", "remote_url": "git@github.com:acme/mono.git",
             "branch": "main", "local_path": "/home/dev/mono"},
            {"repo": "widgets", "remote_url": "git@github.com:acme/widgets.git",
             "branch": "main", "local_path": "/home/dev/mono/widgets"}
        ]
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header("x-demo-user", "dev@x")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // PR list: #193 resolves to the single non-container repo (acme/widgets),
    // plus #200 from the full URL in the assistant message.
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{sid}/prs"))
                .header("x-demo-user", "dev@x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let prs = body["prs"].as_array().unwrap();
    let pr193 = prs.iter().find(|p| p["number"] == 193).unwrap();
    assert_eq!(pr193["owner"], json!("acme"));
    assert_eq!(pr193["repo"], json!("widgets"));
}

/// With BOTH repos being containers (no work sub-repo left), the bare "PR #N"
/// stays unresolved (ctx None) — only the full URL links.
#[tokio::test]
async fn all_containers_leave_pr_ctx_none() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());

    // Mark both repo roots as containers.
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/user/settings")
                .header("content-type", "application/json")
                .header("x-demo-user", "dev@x")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "container_paths": ["/home/dev/a", "/home/dev/b"]
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let sid = "it-all-containers";
    let body = json!({
        "uploaded_by": "dev@x",
        "session": conversations_core::parse_session_str(Some(sid), &pr_mention_sample(sid)).unwrap(),
        "repos": [
            {"repo": "a", "remote_url": "git@github.com:acme/a.git",
             "branch": "main", "local_path": "/home/dev/a"},
            {"repo": "b", "remote_url": "git@github.com:acme/b.git",
             "branch": "main", "local_path": "/home/dev/b"}
        ]
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header("x-demo-user", "dev@x")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{sid}/prs"))
                .header("x-demo-user", "dev@x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_json(resp).await;
    let prs = body["prs"].as_array().unwrap();
    // No bare "PR #193" (ctx None ⇒ unresolved); only the full URL #200 links.
    assert!(
        prs.iter().all(|p| p["number"] != 193),
        "bare ref must NOT resolve when every repo is a container: {prs:?}"
    );
    assert!(prs.iter().any(|p| p["number"] == 200), "URL #200 links");
}

/// GET /api/sessions/{id} exposes a per-viewer `is_container` flag on each repo:
/// true for the link whose viewer path matches a container path, false otherwise.
#[tokio::test]
async fn session_get_exposes_is_container_flag() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());

    // Viewer marks /home/dev/mono as a container.
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/user/settings")
                .header("content-type", "application/json")
                .header("x-demo-user", "dev@x")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "container_paths": ["/home/dev/mono"] })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let sid = "it-iscontainer";
    let mut session = conversations_core::parse_session_str(Some(sid), SAMPLE).unwrap();
    // SAMPLE embeds sessionId="it-sess-1"; force the row key to `sid`.
    session.id = sid.to_string();
    let body = json!({
        "uploaded_by": "dev@x",
        "session": session,
        "repos": [
            {"repo": "mono", "branch": "main", "local_path": "/home/dev/mono"},
            {"repo": "widgets", "branch": "main", "local_path": "/home/dev/mono/widgets"}
        ]
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sessions")
                .header("content-type", "application/json")
                .header("x-demo-user", "dev@x")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // GET as the same viewer: mono is a container, widgets is not.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{sid}"))
                .header("x-demo-user", "dev@x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let repos = body["repos"].as_array().unwrap();
    let mono = repos.iter().find(|r| r["repo"] == "mono").unwrap();
    let widgets = repos.iter().find(|r| r["repo"] == "widgets").unwrap();
    assert_eq!(mono["is_container"], json!(true));
    assert_eq!(widgets["is_container"], json!(false));

    // Make the session org-visible so a different viewer can read it.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/sessions/{sid}/sharing"))
                .header("content-type", "application/json")
                .header("x-demo-user", "dev@x")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "visibility": "organization" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET as a DIFFERENT viewer (no matching container path): all false.
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{sid}"))
                .header("x-demo-user", "other@x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let repos = body["repos"].as_array().unwrap();
    assert!(repos.iter().all(|r| r["is_container"] == json!(false)));
}
async fn seed_live(state: &AppState, id: &str) {
    seed_live_as(state, None, id, "view").await;
}

/// `owner` is the row's owner_email (author-gated routes check it); `mode` is
/// the legacy `"view"`/`"interactive"` interaction flag old rows carry.
async fn seed_live_as(state: &AppState, owner: Option<&str>, id: &str, mode: &str) {
    state
        .repo
        .create_live_session(&conversations_server::store::NewLiveSession {
            id: id.into(),
            session_id: id.into(),
            agent: conversations_core::AgentKind::Claude,
            title: None,
            project_path: None,
            cwd: None,
            git_branch: None,
            owner_email: owner.map(str::to_owned),
            source_host: None,
            remote_url: None,
            mode: mode.into(),
        })
        .await
        .unwrap();
}
#[tokio::test]
async fn live_sse_refuses_cross_origin_when_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state.clone())
        .await
        .expect("build auth router");
    let owner = register_cookie(&app, "owner@x").await;
    seed_live_as(&state, Some("owner@x"), "sse-1", "interactive").await;

    async fn get_stream(
        app: &axum::Router,
        owner: &str,
        origin: Option<&str>,
        host: &str,
    ) -> axum::response::Response {
        let mut builder = Request::builder()
            .method("GET")
            .uri("/api/live/sse-1/stream")
            .header("host", host)
            .header("cookie", owner);
        if let Some(origin) = origin {
            builder = builder.header("origin", origin);
        }
        app.clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    // A hostile page's cross-origin EventSource is refused before the stream.
    let resp = get_stream(
        &app,
        &owner,
        Some("http://evil.example"),
        "conversations.example",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Same-origin (the SPA) streams.
    let resp = get_stream(
        &app,
        &owner,
        Some("http://conversations.example"),
        "conversations.example",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default(),
        "text/event-stream"
    );

    // No Origin header (the CLI) streams.
    let resp = get_stream(&app, &owner, None, "conversations.example").await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// The SPA document response carries the Content-Security-Policy (validated
/// against the real Vite build); JSON API responses do not.
#[tokio::test]
async fn spa_document_carries_csp_header() {
    let dir = std::env::temp_dir().join(format!(
        "conv-csp-static-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("index.html"),
        "<!doctype html><html><body><div id=\"root\"></div></body></html>",
    )
    .unwrap();
    let url = format!("sqlite://{}/csp.db?mode=rwc", dir.display());
    let repo = SqliteRepo::connect(&url).await.expect("connect + migrate");
    let state = AppState::new(
        Arc::new(repo),
        Arc::new(Config {
            auth_enabled: false,
            static_dir: Some(dir.to_string_lossy().into_owned()),
            ..Config::default()
        }),
    );
    let app = build_router(state);

    // The SPA document carries the policy.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let csp = resp
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .expect("document responses carry a CSP");
    assert!(
        csp.contains("script-src 'self'"),
        "script-src is locked: {csp}"
    );
    assert!(csp.contains("object-src 'none'"), "object-src none: {csp}");
    assert!(
        csp.contains("frame-ancestors 'none'"),
        "framing is blocked: {csp}"
    );

    // JSON API responses do not carry it.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("content-security-policy").is_none());

    let _ = std::fs::remove_dir_all(&dir);
}
async fn register_cookie(app: &axum::Router, email: &str) -> String {
    let body = serde_json::to_vec(&json!({ "email": email, "password": "hunter2pass" })).unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    session_cookie(&resp)
}

/// Author sets the share list on a live session; asserts OK.
async fn set_live_sharing_as(app: &axum::Router, cookie: &str, id: &str, shares: Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/live/{id}/sharing"))
                .header("content-type", "application/json")
                .header("cookie", cookie)
                .body(Body::from(
                    serde_json::to_vec(&json!({ "shares": shares })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET a live SSE endpoint and return only the response status (the body is an
/// endless stream, so we never read it — `oneshot` resolves once the status +
/// headers are ready, which is all an authz check needs).
async fn live_get_status(app: &axum::Router, uri: &str, cookie: Option<&str>) -> StatusCode {
    let mut req = Request::builder().uri(uri);
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

/// The terminal-mirror egress (browser socket `/pty/ws` + text SSE `/text/stream`)
/// is VIEW-authorized (`authorize_live_view`), NOT author-only like the host
/// socket: the author AND any sharee — including a read-ONLY sharee — may watch,
/// while a stranger gets 404 (existence hidden) and an absent session 404s. This
/// is the "watching is not writing" boundary: seeing the mirror needs only read.
#[tokio::test]
async fn live_mirror_streams_are_view_authorized_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state.clone())
        .await
        .expect("build auth router");

    let author = register_cookie(&app, "author@x").await;
    let reader = register_cookie(&app, "reader@x").await;
    let stranger = register_cookie(&app, "stranger@x").await;

    seed_live_as(&state, Some("author@x"), "mir-x", "interactive").await;
    // reader is a READ-ONLY sharee (can_write: false).
    set_live_sharing_as(
        &app,
        &author,
        "mir-x",
        json!([{ "email": "reader@x", "can_write": false }]),
    )
    .await;

    // The transcript stream is the one remaining mirror, and it stays
    // VIEW-authorized: the author AND any sharee — including a read-ONLY
    // sharee — may watch, while a stranger gets 404 (existence hidden) and an
    // absent session 404s. This is the "watching is not writing" boundary.
    let uri = "/api/live/mir-x/stream";
    assert_eq!(
        live_get_status(&app, uri, Some(&author)).await,
        StatusCode::OK,
        "author must watch /stream"
    );
    assert_eq!(
        live_get_status(&app, uri, Some(&reader)).await,
        StatusCode::OK,
        "read-only sharee must watch /stream"
    );
    assert_eq!(
        live_get_status(&app, uri, Some(&stranger)).await,
        StatusCode::NOT_FOUND,
        "stranger must not see /stream"
    );
    assert_eq!(
        live_get_status(&app, "/api/live/ghost/stream", Some(&author)).await,
        StatusCode::NOT_FOUND,
        "absent session /stream must 404"
    );
}
#[tokio::test]
async fn dismiss_live_soft_closes_and_regrants_without_owner_end_auth_on() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state.clone())
        .await
        .expect("build auth router");

    let author = register_cookie(&app, "author@x").await;
    let reader = register_cookie(&app, "reader@x").await;

    seed_live_as(&state, Some("author@x"), "dis-x", "interactive").await;
    set_live_sharing_as(
        &app,
        &author,
        "dis-x",
        json!([{ "email": "reader@x", "can_write": false }]),
    )
    .await;

    // A read-only sharee dismisses the session -> 200, graduated (session_id).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/live/dis-x/dismiss")
                .header("cookie", &reader)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let session_id = body_json(resp).await["session_id"]
        .as_str()
        .expect("dismiss returns the graduated session_id")
        .to_string();

    // The dismissing sharee retains read access to the graduated session.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/sessions/{session_id}"))
                .header("cookie", &reader)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "dismiss must re-grant the sharee read access to the graduated session"
    );

    // And it left the live list for both the author and the sharee.
    for cookie in [&author, &reader] {
        let list = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/live")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_json(list).await;
        assert!(
            body["live_sessions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["id"] != json!("dis-x")),
            "dismissed session must leave the live list"
        );
    }
}
#[tokio::test]
async fn live_order_persists_partial_moves_and_rejects_stale_writes() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());
    for id in ["order-a", "order-b", "order-c"] {
        seed_live(&state, id).await;
    }
    let request = |method: &str, body: Value| {
        Request::builder()
            .method(method)
            .uri("/api/live/order")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            json!({"revision":0,"session_ids":["order-c","order-b","order-a"]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            json!({"revision":0,"session_ids":["order-a","order-b"]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let response = app
        .clone()
        .oneshot(request(
            "PUT",
            json!({"revision":1,"session_ids":["order-a","order-c"]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    seed_live(&state, "order-d").await;
    // Rebuild router against persisted storage; new registrations prepend, unaffected slots stay put.
    let response = build_router(state)
        .oneshot(request("GET", json!(null)))
        .await
        .unwrap();
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["revision"], 2);
    assert_eq!(
        body["session_ids"],
        json!(["order-d", "order-a", "order-b", "order-c"])
    );
}

#[tokio::test]
async fn live_order_auth_isolates_users_and_hides_inaccessible_ids() {
    let (state, _tmp) = test_state_password_auth().await;
    let app = build_router_with_auth(state.clone()).await.unwrap();
    let req = |method: &str, uri: &str, cookie: &str, body: Value| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("cookie", cookie)
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let mut cookies = Vec::new();
    for user in ["order-alice@example.com", "order-bob@example.com"] {
        let response = app
            .clone()
            .oneshot(req(
                "POST",
                "/auth/register",
                "",
                json!({"email":user,"password":"long-order-password"}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        cookies.push(session_cookie(&response));
    }
    for (index, id) in ["private-a", "private-b"].iter().enumerate() {
        let owner = ["order-alice@example.com", "order-bob@example.com"][index];
        seed_live_as(&state, Some(owner), id, "view").await;
    }
    let response = app
        .clone()
        .oneshot(req(
            "PUT",
            "/api/live/order",
            &cookies[0],
            json!({"revision":0,"session_ids":["private-b"]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(req(
            "PUT",
            "/api/live/order",
            &cookies[0],
            json!({"revision":0,"session_ids":["private-a"]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(req("GET", "/api/live/order", &cookies[1], json!(null)))
        .await
        .unwrap();
    let body = body_json(response).await;
    assert_eq!(body["revision"], 0);
    assert_eq!(body["session_ids"], json!(["private-b"]));
    // Graduating a session (a dismissal/completion on the host side) removes
    // it from the visible order.
    state.repo.complete_live_session("private-a").await.unwrap();
    let response = app
        .oneshot(req("GET", "/api/live/order", &cookies[0], json!(null)))
        .await
        .unwrap();
    assert_eq!(body_json(response).await["session_ids"], json!([]));
}

// ---------------------------------------------------------------------------
// Terminal file drops: browser upload -> host SSE frame -> explicit host ack
// ---------------------------------------------------------------------------

/// Incremental reader for an SSE response body, returning each `data:` payload
/// as JSON and ignoring keep-alive comments.
struct SseReader {
    body: Body,
    buf: String,
}

impl SseReader {
    fn new(resp: axum::response::Response) -> Self {
        Self {
            body: resp.into_body(),
            buf: String::new(),
        }
    }

    async fn next_json(&mut self, ms: u64) -> Value {
        tokio::time::timeout(Duration::from_millis(ms), async {
            loop {
                while let Some(end) = self.buf.find("\n\n") {
                    let event: String = self.buf.drain(..end + 2).collect();
                    if let Some(line) = event.lines().find(|l| l.starts_with("data:")) {
                        return serde_json::from_str(line["data:".len()..].trim()).unwrap();
                    }
                }
                let frame = self
                    .body
                    .frame()
                    .await
                    .expect("sse stream ended")
                    .expect("sse frame ok");
                if let Some(data) = frame.data_ref() {
                    self.buf.push_str(std::str::from_utf8(data).unwrap());
                }
            }
        })
        .await
        .expect("an sse event should arrive before the timeout")
    }
}

fn live_message(uuid: &str, role: &str, blocks: Value) -> Value {
    json!({
        "uuid": uuid,
        "parent_uuid": null,
        "role": role,
        "kind": "text",
        "model": null,
        "is_sidechain": false,
        "timestamp": "2026-10-24T10:00:00.000Z",
        "blocks": blocks,
        "text": format!("BODY-{uuid}"),
        "raw": { "content": format!("BODY-{uuid}") },
        "usage": null,
    })
}

/// A collapsed transcript subscribes with `?bodies=false`: it needs the count
/// and the latest visible role (for its label and the Live response pane) and
/// the side-channel frames, but must not download any message body — neither
/// the stored backlog nor new messages arriving on the live tail.
#[tokio::test]
async fn live_stream_without_bodies_sends_only_message_outline() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());
    seed_live(&state, "outline-1").await;
    for message in [
        live_message(
            "m0",
            "user",
            json!([{ "type": "text", "text": "bootstrap" }]),
        ),
        live_message(
            "m1",
            "assistant",
            json!([
                { "type": "tool_use", "id": "t1", "name": "Bash", "input": {} }
            ]),
        ),
        live_message(
            "m2",
            "user",
            json!([
                { "type": "tool_result", "tool_use_id": "t1", "content": "ok" }
            ]),
        ),
        live_message(
            "m3",
            "assistant",
            json!([{ "type": "thinking", "thinking": " " }]),
        ),
    ] {
        let message: conversations_core::Message = serde_json::from_value(message).unwrap();
        state
            .repo
            .append_live_message("outline-1", &message)
            .await
            .unwrap();
    }

    let resp = app
        .clone()
        .oneshot(get("/api/live/outline-1/stream?bodies=false"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut sse = SseReader::new(resp);
    let mut outline = Vec::new();
    for _ in 0..4 {
        outline.push(sse.next_json(2_000).await);
    }
    assert_eq!(
        outline,
        vec![
            json!({ "type": "message_outline", "seq": 0, "role": "user", "visible": true }),
            json!({ "type": "message_outline", "seq": 1, "role": "assistant", "visible": true }),
            json!({ "type": "message_outline", "seq": 2, "role": "user", "visible": false }),
            json!({ "type": "message_outline", "seq": 3, "role": "assistant", "visible": false }),
        ],
        "replay must carry only the outline of each stored message"
    );

    // The default stream is unchanged: it still replays full bodies.
    let resp = app
        .clone()
        .oneshot(get("/api/live/outline-1/stream"))
        .await
        .unwrap();
    let mut full = SseReader::new(resp);
    let first = full.next_json(2_000).await;
    assert_eq!(first["type"], json!("message"));
    assert_eq!(first["message"]["text"], json!("BODY-m0"));
}

#[tokio::test]
async fn default_adk_launcher_settings_roundtrip_and_validate() {
    let (state, _tmp) = test_state().await;
    let app = build_router(state.clone());
    let request = |method: &str, uri: &str, body: Value| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("x-demo-user", "owner@x")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    };
    let resp = app
        .clone()
        .oneshot(request(
            "PUT",
            "/api/user/settings",
            json!({
                "default_agent": "opencode", "default_launcher": "adk", "adk_statusline": true
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(request("GET", "/api/user/settings", json!({})))
        .await
        .unwrap();
    let saved = body_json(resp).await;
    assert_eq!(saved["default_agent"], "opencode");
    assert_eq!(saved["default_launcher"], "adk");
    let resp = app
        .clone()
        .oneshot(request(
            "PUT",
            "/api/user/settings",
            json!({
                "default_agent": "codex", "default_launcher": "adk"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = app
        .oneshot(request("GET", "/api/user/settings", json!({})))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["default_agent"], "opencode");
}
