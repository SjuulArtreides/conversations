//! Repository-level cascade proof for `delete_user` against both backends.
//! The SQLite path runs on every workspace test run; the Postgres path is
//! `#[ignore]`d so it runs only when a disposable database is wired up in
//! `PUBLICATION_TEST_DATABASE_URL`.

use std::sync::Arc;

use chrono::{Duration, Utc};
use conversations_core as core;
use conversations_server::store::{Repository, SqliteRepo, VISIBILITY_RESTRICTED};

fn sample_session(id: &str, owner: &str) -> core::Session {
    core::Session {
        id: id.to_string(),
        agent: core::AgentKind::Claude,
        title: format!("title {id} for {owner}"),
        project_path: None,
        cwd: None,
        git_branch: None,
        claude_version: None,
        started_at: None,
        ended_at: None,
        messages: Vec::new(),
        skipped_lines: 0,
    }
}

async fn exercise(repo: Arc<dyn Repository>) {
    repo.upsert_user("victim@example.com", Some("Victim"))
        .await
        .unwrap();
    repo.upsert_user("bystander@example.com", Some("Bystander"))
        .await
        .unwrap();
    repo.upsert_user("owner@example.com", Some("Owner"))
        .await
        .unwrap();

    // Cascading child rows: password, delegated-admin grant, container paths,
    // user settings, and a reset token.
    repo.set_password("victim@example.com", "pwd")
        .await
        .unwrap();
    repo.set_user_admin("victim@example.com", true, "admin@example.com")
        .await
        .unwrap();
    repo.set_container_paths("victim@example.com", &["/tmp/x".into()])
        .await
        .unwrap();
    repo.set_default_agent("victim@example.com", core::AgentKind::Codex)
        .await
        .unwrap();
    repo.create_password_reset_token(
        "victim@example.com",
        "admin@example.com",
        "tok-delete",
        "secret-hash",
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();

    // Share grants that must be scrubbed so a revoked account loses access
    // granted TO it.
    let shared = sample_session("sess-shared", "owner@example.com");
    repo.upsert_session(
        "owner@example.com",
        None,
        Some("owner@example.com"),
        &shared,
    )
    .await
    .unwrap();
    repo.set_sharing(
        "sess-shared",
        VISIBILITY_RESTRICTED,
        &[
            "victim@example.com".to_string(),
            "bystander@example.com".to_string(),
        ],
    )
    .await
    .unwrap();

    // Content the victim authored must survive with the owner_email tombstone.
    let authored = sample_session("sess-victim", "victim@example.com");
    repo.upsert_session(
        "victim@example.com",
        None,
        Some("victim@example.com"),
        &authored,
    )
    .await
    .unwrap();

    assert!(repo.delete_user("victim@example.com").await.unwrap());
    assert!(!repo.delete_user("victim@example.com").await.unwrap());

    // Cascading rows gone.
    assert!(repo
        .get_user_with_password("victim@example.com")
        .await
        .unwrap()
        .is_none());
    assert!(!repo.is_user_admin("victim@example.com").await.unwrap());
    assert!(repo
        .get_container_paths("victim@example.com")
        .await
        .unwrap()
        .is_empty());
    assert!(repo
        .get_password_reset_token("tok-delete", Utc::now())
        .await
        .unwrap()
        .is_none());

    // Share list drops the victim and keeps the bystander.
    let (_, emails) = repo.get_sharing("sess-shared").await.unwrap().unwrap();
    assert_eq!(emails, vec!["bystander@example.com".to_string()]);

    // Authored session stays with owner_email in place.
    let stored = repo
        .get_session(None, "sess-victim")
        .await
        .unwrap()
        .expect("authored session preserved");
    assert_eq!(stored.owner_email.as_deref(), Some("victim@example.com"));
    assert_eq!(stored.uploaded_by, "victim@example.com");

    let listed = repo.list_users().await.unwrap();
    assert!(!listed.iter().any(|u| u.email == "victim@example.com"));
    assert!(listed.iter().any(|u| u.email == "bystander@example.com"));
    assert!(listed.iter().any(|u| u.email == "owner@example.com"));
}

#[tokio::test]
async fn delete_user_cascades_and_preserves_authored_sqlite() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/delete-user-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.sqlite", uuid::Uuid::new_v4()));
    let repo = SqliteRepo::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    exercise(Arc::new(repo.clone())).await;
    repo.pool().close().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable empty Postgres database in PUBLICATION_TEST_DATABASE_URL"]
async fn delete_user_cascades_and_preserves_authored_postgres() {
    let url = std::env::var("PUBLICATION_TEST_DATABASE_URL").unwrap();
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("delete_user_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let url = format!("{url}?options=-csearch_path%3D{schema}");
    let repo = conversations_server::store::PostgresRepo::connect(&url)
        .await
        .unwrap();
    exercise(Arc::new(repo.clone())).await;
    repo.pool().close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
