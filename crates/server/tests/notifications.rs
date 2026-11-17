use conversations_core::{AgentKind, Session};
use conversations_server::store::{notifications::SharingUpdate, LiveShare, Repository};
use conversations_server::store::{PostgresRepo, SqliteRepo};
use uuid::Uuid;

async fn sharing_and_attention_journey(repo: &dyn Repository) {
    let id = format!("notifications-{}", Uuid::new_v4());
    let owner = format!("owner-{id}@test.invalid");
    let reader = format!("reader-{id}@test.invalid");
    let stranger = format!("stranger-{id}@test.invalid");
    let session = Session {
        id: id.clone(),
        agent: AgentKind::Claude,
        title: "Sharing context".into(),
        project_path: None,
        cwd: None,
        git_branch: None,
        claude_version: None,
        started_at: None,
        ended_at: None,
        messages: vec![],
        skipped_lines: 0,
    };
    repo.upsert_session(&owner, None, Some(&owner), &session)
        .await
        .unwrap();
    let update = SharingUpdate {
        sender: Some(owner.clone()),
        reason: Some("Please review the tenant setup".into()),
        visibility: Some("restricted".into()),
        emails: vec![reader.clone()],
        shares: vec![],
    };
    repo.update_sharing_context(&id, &update).await.unwrap();
    repo.update_sharing_context(&id, &update).await.unwrap();
    let inbox = repo.notifications(&reader).await.unwrap();
    assert_eq!(inbox.len(), 1, "unchanged saves do not notify twice");
    assert_eq!(inbox[0].message, "Please review the tenant setup");
    assert!(repo.notifications(&stranger).await.unwrap().is_empty());
    assert!(repo
        .acknowledge_notification(&inbox[0].id, &stranger, false)
        .await
        .is_err());
    repo.acknowledge_notification(&inbox[0].id, &reader, false)
        .await
        .unwrap();
    let inbox = repo.notifications(&reader).await.unwrap();
    assert!(inbox[0].read_at.is_some());
    assert_eq!(
        inbox[0].message, "Please review the tenant setup",
        "read notifications retain sharing context"
    );

    let changed = SharingUpdate {
        reason: Some("Review the identity mapping".into()),
        ..update
    };
    repo.update_sharing_context(&id, &changed).await.unwrap();
    let inbox = repo.notifications(&reader).await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].message, "Review the identity mapping");
    assert!(
        inbox[0].read_at.is_none(),
        "a changed reason creates fresh unread context"
    );
    let unchanged = SharingUpdate {
        reason: None,
        ..changed
    };
    repo.update_sharing_context(&id, &unchanged).await.unwrap();
    assert_eq!(
        repo.notifications(&reader).await.unwrap()[0].message,
        "Review the identity mapping",
        "legacy clients do not erase sharing context"
    );

    let recipients = repo.attention_recipients(&id, &reader).await.unwrap();
    assert_eq!(recipients.len(), 1);
    assert_eq!(recipients[0].email, owner);
    assert!(repo
        .request_attention(&id, &stranger, &owner, "Question")
        .await
        .is_err());
    assert!(repo
        .request_attention(&id, &reader, &owner, "  ")
        .await
        .is_err());
    repo.request_attention(&id, &reader, &owner, "Why this tenant?")
        .await
        .unwrap();
    repo.request_attention(&id, &reader, &owner, "And which region?")
        .await
        .unwrap();
    let inbox = repo.notifications(&owner).await.unwrap();
    assert_eq!(inbox.iter().filter(|n| n.resolved_at.is_none()).count(), 2);
    repo.acknowledge_notification(&inbox[0].id, &owner, false)
        .await
        .unwrap();
    assert_eq!(
        repo.notifications(&owner)
            .await
            .unwrap()
            .iter()
            .filter(|n| n.resolved_at.is_none())
            .count(),
        2,
        "reading must not clear attention"
    );
    repo.acknowledge_notification(&inbox[0].id, &owner, true)
        .await
        .unwrap();
    assert_eq!(
        repo.notifications(&owner)
            .await
            .unwrap()
            .iter()
            .filter(|n| n.resolved_at.is_none())
            .count(),
        1
    );
    repo.acknowledge_notification(&inbox[1].id, &owner, true)
        .await
        .unwrap();
    assert!(repo
        .notifications(&owner)
        .await
        .unwrap()
        .iter()
        .all(|n| n.resolved_at.is_some() && n.read_at.is_some()));

    let live_reader = format!("live-reader-{id}@test.invalid");
    let live_share = SharingUpdate {
        sender: Some(owner.clone()),
        reason: Some("Check the live terminal".into()),
        visibility: None,
        emails: vec![],
        shares: vec![LiveShare {
            email: live_reader.clone(),
            can_write: false,
            can_read: true,
            can_communicate: false,
        }],
    };
    repo.update_sharing_context(&id, &live_share).await.unwrap();
    let live_inbox = repo.notifications(&live_reader).await.unwrap();
    assert_eq!(live_inbox[0].message, "Check the live terminal");
    let invalid = SharingUpdate {
        reason: Some("x".repeat(2001)),
        ..live_share
    };
    assert!(repo.update_sharing_context(&id, &invalid).await.is_err());
    assert_eq!(
        repo.get_live_shares(&id).await.unwrap()[0].email,
        live_reader,
        "failed saves preserve existing access"
    );

    let communication = SharingUpdate {
        sender: Some(owner.clone()),
        reason: None,
        visibility: None,
        emails: vec![],
        shares: vec![LiveShare {
            email: stranger.clone(),
            can_write: false,
            can_read: false,
            can_communicate: true,
        }],
    };
    repo.update_sharing_context(&id, &communication)
        .await
        .unwrap();
    assert!(repo.notifications(&stranger).await.unwrap().is_empty());
    assert!(repo
        .request_attention(&id, &stranger, &owner, "No transcript access")
        .await
        .is_err());
    assert!(repo
        .request_attention(&id, &reader, &stranger, "No transcript access")
        .await
        .is_err());

    let revoked = SharingUpdate {
        sender: Some(owner.clone()),
        reason: None,
        visibility: Some("restricted".into()),
        emails: vec![],
        shares: vec![],
    };
    repo.update_sharing_context(&id, &revoked).await.unwrap();
    assert!(
        repo.notifications(&reader).await.unwrap().is_empty(),
        "revocation hides sharing context"
    );
    assert!(repo
        .acknowledge_notification(&inbox[0].id, &reader, true)
        .await
        .is_err());
    assert!(repo
        .request_attention(&id, &reader, &owner, "Revoked")
        .await
        .is_err());
    repo.delete_session(&id).await.unwrap();
    assert!(
        repo.notifications(&owner).await.unwrap().is_empty(),
        "session deletion removes inbox entries"
    );
}

#[tokio::test]
async fn sqlite_sharing_and_attention() {
    let path = std::path::Path::new("tmp").join(format!("notifications-{}.db", Uuid::new_v4()));
    std::fs::create_dir_all("tmp").unwrap();
    let repo = SqliteRepo::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sharing_and_attention_journey(&repo).await;
    drop(repo);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable Postgres database in NOTIFICATIONS_TEST_POSTGRES_URL"]
async fn postgres_sharing_and_attention() {
    let url = std::env::var("NOTIFICATIONS_TEST_POSTGRES_URL")
        .expect("Postgres notification tests require NOTIFICATIONS_TEST_POSTGRES_URL");
    let repo = PostgresRepo::connect(&url).await.unwrap();
    sharing_and_attention_journey(&repo).await;
}

#[tokio::test]
async fn notification_api_enforces_identity_and_accepts_legacy_sharing() {
    use axum::{body::Body, http::Request};
    use conversations_server::{build_router, AppState, Config};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;
    let repo = Arc::new(SqliteRepo::connect("sqlite::memory:").await.unwrap());
    let session: Session =
        serde_json::from_value(json!({"id":"api-notifications", "title":"API", "messages":[]}))
            .unwrap();
    repo.upsert_session("owner@test", None, Some("owner@test"), &session)
        .await
        .unwrap();
    let app = build_router(AppState::new(
        repo,
        Arc::new(Config {
            auth_enabled: false,
            ..Config::default()
        }),
    ));
    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        user: Option<&str>,
        body: Value,
    ) -> (u16, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(user) = user {
            request = request.header("x-demo-user", user);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
    let path = "/api/sessions/api-notifications/sharing";
    assert_eq!(
        call(
            &app,
            "PUT",
            path,
            Some("stranger@test"),
            json!({"visibility":"restricted","emails":["stranger@test"]})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        call(
            &app,
            "PUT",
            path,
            Some("owner@test"),
            json!({"visibility":"restricted","emails":["reader@test"]})
        )
        .await
        .0,
        200
    );
    let (status, inbox) = call(
        &app,
        "GET",
        "/api/notifications",
        Some("reader@test"),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(inbox[0]["message"], "");
    assert_eq!(
        call(&app, "GET", "/api/notifications", None, Value::Null)
            .await
            .1,
        json!([])
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/sessions/api-notifications/attention",
            None,
            json!({"recipient_email":"owner@test","message":"Question"})
        )
        .await
        .0,
        401
    );
    let id = inbox[0]["id"].as_str().unwrap();
    assert_eq!(
        call(
            &app,
            "POST",
            &format!("/api/notifications/{id}/read"),
            Some("stranger@test"),
            Value::Null
        )
        .await
        .0,
        404
    );
    assert_eq!(
        call(
            &app,
            "POST",
            &format!("/api/notifications/{id}/read"),
            Some("reader@test"),
            Value::Null
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/sessions/api-notifications/attention",
            Some("reader@test"),
            json!({"recipient_email":"owner@test","message":"Question"})
        )
        .await
        .0,
        200
    );
    let inbox = call(
        &app,
        "GET",
        "/api/notifications",
        Some("owner@test"),
        Value::Null,
    )
    .await
    .1;
    assert_eq!(inbox[0]["message"], "Question");
    let id = inbox[0]["id"].as_str().unwrap();
    assert_eq!(
        call(
            &app,
            "POST",
            &format!("/api/notifications/{id}/resolve"),
            Some("owner@test"),
            Value::Null
        )
        .await
        .0,
        200
    );
    assert!(call(
        &app,
        "GET",
        "/api/notifications",
        Some("owner@test"),
        Value::Null
    )
    .await
    .1[0]["resolved_at"]
        .is_string());
}
