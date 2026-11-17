//! Upgrade a shipped schema rather than testing only an empty database.
use conversations_server::SqliteRepo;
use sqlx::sqlite::SqlitePoolOptions;

#[tokio::test]
async fn main_handoff_model_schema_upgrades_without_rewriting_history() {
    // Main 440ba09 ends at SQLite 41 (handoff_model). Keep the actual migration
    // bytes/checksums; only withhold this branch's later publications migration.
    let mut main = sqlx::migrate!("./migrations/sqlite");
    main.migrations
        .to_mut()
        .retain(|migration| migration.version <= 41);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    main.run(&pool).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO projects(id,name,created_at,updated_at)
         VALUES('project','Upgrade fixture','2026-11-05','2026-11-05');
         INSERT INTO sessions(id,title,uploaded_by,uploaded_at,project_id)
         VALUES('source','Source','fixture','2026-11-05','project'),
               ('child','Child','fixture','2026-11-05','project');
         INSERT INTO agent_handoffs(session_id,source_session_id,project_id,agent,brief,
             idempotency_key,status,created_at,model,variant)
         VALUES('child','source','project','opencode','Retain this request','once',
             'requested','2026-11-05','provider/model','high');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let before: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(before.last().unwrap().0, 41);
    let repo = SqliteRepo::from_pool(pool.clone());
    repo.migrate().await.unwrap();
    repo.migrate().await.unwrap(); // Restart must not repeat or rewrite migrations.
    let after: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version,checksum FROM _sqlx_migrations WHERE version <= 41 ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(after, before);
    let handoff: (String, String, String, String, String) = sqlx::query_as(
        "SELECT source_session_id,idempotency_key,status,model,variant
         FROM agent_handoffs WHERE session_id='child'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        handoff,
        (
            "source".into(),
            "once".into(),
            "requested".into(),
            "provider/model".into(),
            "high".into()
        )
    );
    let publications: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM publications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        publications, 0,
        "migration must not publish existing sessions"
    );
    let applied: bool = sqlx::query_scalar("SELECT success FROM _sqlx_migrations WHERE version=42")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(applied);
}
