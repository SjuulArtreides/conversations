//! Project views must not leak restricted member sessions, on both backends.
//! The SQLite path runs on every workspace test run; the Postgres path is
//! `#[ignore]`d so it runs only when a disposable database is wired up in
//! `PUBLICATION_TEST_DATABASE_URL`.

use std::collections::HashSet;
use std::sync::Arc;

use conversations_core as core;
use conversations_server::store::{
    LiveShare, NewLiveSession, Repository, SqliteRepo, VISIBILITY_ORGANIZATION,
    VISIBILITY_RESTRICTED,
};

fn sample_session(id: &str) -> core::Session {
    core::Session {
        id: id.to_string(),
        agent: core::AgentKind::Claude,
        title: format!("title {id}"),
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

async fn visible_members(
    repo: &dyn Repository,
    viewer: Option<&str>,
    project: &str,
) -> (HashSet<String>, usize) {
    let detail = repo.get_project(viewer, project).await.unwrap().unwrap();
    let ids = detail.sessions.iter().map(|s| s.id.clone()).collect();
    let listed = repo.list_projects_detailed(viewer).await.unwrap();
    let row = listed.iter().find(|p| p.id == project).unwrap();
    assert_eq!(
        row.session_count, detail.project.session_count,
        "list and detail counts must agree for {viewer:?}"
    );
    (ids, detail.project.session_count)
}

fn set(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

async fn exercise(repo: Arc<dyn Repository>) {
    let repo = repo.as_ref();
    let project = repo.create_project(None, "P", None).await.unwrap();

    // Org-wide member.
    repo.upsert_session("u@x", None, None, &sample_session("pub"))
        .await
        .unwrap();
    repo.set_sharing("pub", VISIBILITY_ORGANIZATION, &[])
        .await
        .unwrap();
    // Restricted member owned by alice, person-shared with bob.
    repo.upsert_session("u@x", None, Some("alice@x"), &sample_session("priv"))
        .await
        .unwrap();
    repo.set_sharing("priv", VISIBILITY_RESTRICTED, &["bob@x".to_string()])
        .await
        .unwrap();
    // Restricted live member owned by alice: dave may read it, erin may only
    // communicate with it (no transcript access).
    repo.upsert_session("u@x", None, Some("alice@x"), &sample_session("live"))
        .await
        .unwrap();
    repo.set_sharing("live", VISIBILITY_RESTRICTED, &[])
        .await
        .unwrap();
    repo.create_live_session(&NewLiveSession {
        id: "live".into(),
        session_id: "live".into(),
        agent: core::AgentKind::Claude,
        title: None,
        project_path: None,
        cwd: None,
        git_branch: None,
        owner_email: Some("alice@x".into()),
        source_host: None,
        remote_url: None,
        mode: "view".into(),
    })
    .await
    .unwrap();
    repo.set_live_shares(
        "live",
        &[
            LiveShare {
                email: "dave@x".into(),
                can_read: true,
                can_write: false,
                can_communicate: false,
            },
            LiveShare {
                email: "erin@x".into(),
                can_read: false,
                can_write: false,
                can_communicate: true,
            },
        ],
    )
    .await
    .unwrap();
    for id in ["pub", "priv", "live"] {
        repo.set_session_project(id, Some(&project.id))
            .await
            .unwrap();
    }

    let cases: [(Option<&str>, &[&str]); 6] = [
        (None, &["pub", "priv", "live"]),
        (Some("alice@x"), &["pub", "priv", "live"]),
        (Some("bob@x"), &["pub", "priv"]),
        (Some("dave@x"), &["pub", "live"]),
        (Some("erin@x"), &["pub"]),
        (Some("carol@x"), &["pub"]),
    ];
    for (viewer, expected) in cases {
        let (ids, count) = visible_members(repo, viewer, &project.id).await;
        assert_eq!(ids, set(expected), "members for {viewer:?}");
        assert_eq!(count, expected.len(), "count for {viewer:?}");
    }

    assert!(repo.get_project(None, "nope").await.unwrap().is_none());
    assert!(repo
        .get_project(Some("carol@x"), "nope")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn project_views_filter_restricted_members_sqlite() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp/project-visibility-tests");
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
async fn project_views_filter_restricted_members_postgres() {
    let url = std::env::var("PUBLICATION_TEST_DATABASE_URL").unwrap();
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("project_visibility_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    // `public` stays on the path so the database-wide pg_trgm operators resolve.
    let url = format!("{url}?options=-csearch_path%3D{schema}%2Cpublic");
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
