//! Opt-in PostgreSQL cache migration lifecycle checks against disposable databases.
use sqlx::Connection;
use sqlx::{
    migrate::{Migration, MigrationType, Migrator},
    postgres::PgConnectOptions,
    PgPool,
};
use std::str::FromStr;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL in PG_SEARCH_TEST_DATABASE_URL"]
async fn sqlx_no_transaction_copy_commits_batches_and_resumes_without_journaling_failure() {
    let options =
        PgConnectOptions::from_str(&std::env::var("PG_SEARCH_TEST_DATABASE_URL").unwrap()).unwrap();
    let admin = PgPool::connect_with(options.clone()).await.unwrap();
    let database = format!("cache_lifecycle_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPool::connect_with(options.clone().database(&database))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE copied(id integer PRIMARY KEY); CREATE TABLE fault(enabled bool); INSERT INTO fault VALUES(true)").execute(&pool).await.unwrap();
    let migration = Migration::new(
        1,
        "bounded copy".into(),
        MigrationType::Simple,
        r#"-- no-transaction
DO $copy$
BEGIN
 INSERT INTO copied VALUES(1) ON CONFLICT DO NOTHING;
 COMMIT;
 IF (SELECT enabled FROM fault) THEN RAISE EXCEPTION 'deliberate interruption'; END IF;
 INSERT INTO copied VALUES(2) ON CONFLICT DO NOTHING;
 COMMIT;
END $copy$;"#
            .into(),
        true,
    );
    let migrator = Migrator {
        migrations: vec![migration].into(),
        ..Migrator::DEFAULT
    };
    let error = migrator.run(&pool).await.unwrap_err();
    assert!(
        error.to_string().contains("deliberate interruption"),
        "{error}"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM copied")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    pool.close().await;
    let pool = PgPool::connect_with(options.database(&database))
        .await
        .unwrap();
    sqlx::query("UPDATE fault SET enabled=false")
        .execute(&pool)
        .await
        .unwrap();
    migrator.run(&pool).await.unwrap();
    migrator.run(&pool).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM copied")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL in PG_SEARCH_TEST_DATABASE_URL"]
async fn cached_search_tracks_atomic_text_changes_deletes_and_preserves_acl() {
    use conversations_server::store::{PostgresRepo, Repository};
    let options =
        PgConnectOptions::from_str(&std::env::var("PG_SEARCH_TEST_DATABASE_URL").unwrap()).unwrap();
    let admin = PgPool::connect_with(options.clone()).await.unwrap();
    let database = format!("cache_behavior_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPool::connect_with(options.database(&database))
        .await
        .unwrap();
    let mut baseline = sqlx::migrate!("./migrations/postgres");
    baseline.migrations.to_mut().retain(|m| m.version <= 46);
    baseline.run(&pool).await.unwrap();
    sqlx::raw_sql("INSERT INTO sessions(id,title,uploaded_at,uploaded_by,owner_email,visibility) VALUES('body','Body',now(),'fixture','viewer','restricted'),('exact','Local: codex',now(),'fixture','viewer','restricted'),('hidden','Hidden',now(),'fixture','other','restricted'); INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) VALUES('one','body','user','user','Mixed LOCAL: CODEX Äpfel %_literal','{}',0),('two','hidden','user','user','local: codex','{}',0);").execute(&pool).await.unwrap();
    let repo = PostgresRepo::from_pool(pool.clone());
    repo.migrate().await.unwrap();
    let cached: String = sqlx::query_scalar(
        "SELECT lower_text FROM message_search_cache WHERE session_id='body' AND uuid='one'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cached, "mixed local: codex äpfel %_literal");
    let results = repo
        .list_sessions(Some("viewer"), Some("Local: codex"), None)
        .await
        .unwrap();
    assert_eq!(
        results.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec!["exact", "body"]
    );
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE messages SET text='Replacement UNIQUE' WHERE session_id='body'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let inside: String =
        sqlx::query_scalar("SELECT lower_text FROM message_search_cache WHERE session_id='body'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(inside, "replacement unique");
    let outside: String =
        sqlx::query_scalar("SELECT lower_text FROM message_search_cache WHERE session_id='body'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outside, cached);
    tx.rollback().await.unwrap();
    sqlx::query("UPDATE messages SET text='Replacement UNIQUE' WHERE session_id='body'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        repo.list_sessions(Some("viewer"), Some("unique"), None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        repo.list_sessions(Some("viewer"), Some("Local: codex"), None)
            .await
            .unwrap()
            .len(),
        1
    );
    sqlx::query("DELETE FROM messages WHERE session_id='body'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM message_search_cache WHERE session_id='body'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    sqlx::query("INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) VALUES('new','body','user','user','new UNIQUE','{}',1)").execute(&pool).await.unwrap();
    assert_eq!(
        repo.list_sessions(Some("viewer"), Some("unique"), None)
            .await
            .unwrap()
            .len(),
        1
    );
    sqlx::query("DELETE FROM sessions WHERE id='body'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM message_search_cache WHERE session_id='body'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL in PG_SEARCH_TEST_DATABASE_URL"]
async fn interrupted_copy_revisits_locked_rows_and_keeps_concurrent_writer_values() {
    use conversations_server::store::PostgresRepo;
    let options =
        PgConnectOptions::from_str(&std::env::var("PG_SEARCH_TEST_DATABASE_URL").unwrap()).unwrap();
    let admin = PgPool::connect_with(options.clone()).await.unwrap();
    let database = format!("cache_copy_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
    let options = options.database(&database);
    let pool = PgPool::connect_with(options.clone()).await.unwrap();
    let mut baseline = sqlx::migrate!("./migrations/postgres");
    baseline.migrations.to_mut().retain(|m| m.version <= 46);
    baseline.run(&pool).await.unwrap();
    sqlx::raw_sql("INSERT INTO sessions(id,title,uploaded_by,uploaded_at) VALUES('fixture','Fixture','fixture',now()); INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) SELECT lpad(i::text,6,'0'),'fixture','user','user','Original '||i,'{}',i FROM generate_series(0,1023)i;").execute(&pool).await.unwrap();
    let mut setup = sqlx::migrate!("./migrations/postgres");
    setup.migrations.to_mut().retain(|m| m.version <= 47);
    setup.run(&pool).await.unwrap();
    let mut busy = pool.begin().await.unwrap();
    sqlx::query("SELECT uuid FROM messages WHERE uuid='000000' FOR UPDATE")
        .execute(&mut *busy)
        .await
        .unwrap();
    let mut migration_connection = sqlx::PgConnection::connect_with(&options).await.unwrap();
    let migrating = tokio::spawn(async move {
        let result = sqlx::migrate!("./migrations/postgres")
            .run_direct(&mut migration_connection)
            .await;
        // Model process termination after a failed startup, not a retained pool.
        migration_connection.close_hard().await.unwrap();
        result
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM message_search_cache")
                .fetch_one(&pool)
                .await
                .unwrap();
            if count == 1023 {
                break;
            }
            assert!(!migrating.is_finished());
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !migrating.is_finished(),
        "locked uncopied row must prevent completeness"
    );
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL statement_timeout='1s'")
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::raw_sql("UPDATE messages SET text='Concurrent FRESH' WHERE uuid='000001'; DELETE FROM messages WHERE uuid='000002'; INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) VALUES('new','fixture','user','user','Concurrent INSERT','{}',1024)").execute(&mut *writer).await.unwrap();
    writer.commit().await.unwrap();
    let cancelled:bool=sqlx::query_scalar("SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE datname=current_database() AND query LIKE '-- no-transaction%DO $copy$%' AND state='active'").fetch_one(&pool).await.unwrap();
    assert!(cancelled);
    assert!(migrating.await.unwrap().is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations WHERE version=48")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query("UPDATE messages SET text='Locked FRESH' WHERE uuid='000000'")
        .execute(&mut *busy)
        .await
        .unwrap();
    busy.commit().await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        PostgresRepo::from_pool(pool.clone()).migrate(),
    )
    .await
    .unwrap()
    .unwrap();
    let mismatches:i64=sqlx::query_scalar("SELECT count(*) FROM messages m LEFT JOIN message_search_cache c USING(session_id,uuid) WHERE c.uuid IS NULL OR c.lower_text<>LOWER(m.text) OR c.search_key<>TRANSLATE(LOWER(m.text),' :-/.','xqzvy')").fetch_one(&pool).await.unwrap();
    assert_eq!(
        mismatches, 0,
        "resumed copy must not overwrite concurrent updates or omit busy rows"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM message_search_cache")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1024
    );
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and explicitly authorized PG_SEARCH_OWNED_CORPUS_CSV"]
async fn owned_corpus_cached_repository_search_preserves_complete_results_and_uses_bounded_index() {
    use conversations_server::store::{PostgresRepo, Repository};
    use sqlx::Row;
    let csv = std::env::var("PG_SEARCH_OWNED_CORPUS_CSV").unwrap();
    let options =
        PgConnectOptions::from_str(&std::env::var("PG_SEARCH_TEST_DATABASE_URL").unwrap()).unwrap();
    let admin = PgPool::connect_with(options.clone()).await.unwrap();
    let database = format!("cache_owned_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.database(&database))
        .await
        .unwrap();
    let mut baseline = sqlx::migrate!("./migrations/postgres");
    baseline.migrations.to_mut().retain(|m| m.version <= 46);
    baseline.run(&pool).await.unwrap();
    // The mitigation does not use this expression index. Omit its maintenance
    // from fixture loading; retirement/recovery is covered separately.
    sqlx::raw_sql("DROP INDEX idx_messages_search_key_trgm; CREATE TABLE owned_messages(id bigint,session_id text,text text)").execute(&pool).await.unwrap();
    let copied = sqlx::raw_sql(&format!(
        "COPY owned_messages FROM '{}' WITH (FORMAT csv)",
        csv.replace('\'', "''")
    ))
    .execute(&pool)
    .await;
    assert!(
        copied.is_ok(),
        "owned corpus import failed (row contents intentionally not logged)"
    );
    let started = std::time::Instant::now();
    let seeded=sqlx::raw_sql("INSERT INTO sessions(id,title,uploaded_at,uploaded_by,owner_email,visibility) SELECT DISTINCT session_id,'Imported archive',now(),'fixture','viewer','restricted' FROM owned_messages; INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) SELECT id::text,session_id,'user','user',text,'{}',id FROM owned_messages; DROP TABLE owned_messages; UPDATE sessions SET owner_email='other' WHERE id=(SELECT min(session_id) FROM messages WHERE LOWER(text) LIKE '%local: codex%'); INSERT INTO sessions(id,title,uploaded_at,uploaded_by,owner_email,visibility) VALUES('exact-title','Local: codex',now()-interval '1 year','fixture','viewer','restricted'); ANALYZE sessions; ANALYZE messages;").execute(&pool).await;
    assert!(
        seeded.is_ok(),
        "owned corpus fixture setup failed (contents intentionally not logged)"
    );
    eprintln!("owned corpus repository load {:?}", started.elapsed());
    let fingerprint:String=sqlx::query_scalar("SELECT count(*)::text||'|'||md5(string_agg(uuid,',' ORDER BY uuid::bigint)) FROM messages WHERE LOWER(text) LIKE '%local: codex%'").fetch_one(&pool).await.unwrap();
    eprintln!("owned corpus body fingerprint {fingerprint}");
    let mut setup = sqlx::migrate!("./migrations/postgres");
    setup.migrations.to_mut().retain(|m| m.version <= 47);
    setup.run(&pool).await.unwrap();
    let repo = PostgresRepo::from_pool(pool.clone());
    repo.list_sessions(Some("viewer"), Some("Local: codex"), None)
        .await
        .unwrap();
    let (name,statement):(String,String)=sqlx::query_as("SELECT name,statement FROM pg_prepared_statements WHERE statement LIKE '%COALESCE(title_override, title) AS title%' AND statement LIKE '%m.lower_text%' AND statement NOT LIKE 'SELECT name%'").fetch_one(&pool).await.unwrap();
    let original = statement
        .replace("FROM message_search_cache m", "FROM messages m")
        .replace(
            "m.search_key LIKE TRANSLATE($6, ' :-/.', 'xqzvy') AND m.lower_text LIKE $6",
            "LOWER(m.text) LIKE $6",
        );
    assert!(!original.contains("search_key"));
    let started = std::time::Instant::now();
    let mut query = sqlx::query(&original).bind("viewer").bind("viewer");
    for _ in 0..5 {
        query = query.bind("%local: codex%");
    }
    let expected = query
        .bind("local: codex")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("id"))
        .collect::<Vec<_>>();
    eprintln!(
        "original full repository {:?}, {} visible sessions",
        started.elapsed(),
        expected.len()
    );
    let started = std::time::Instant::now();
    repo.migrate().await.unwrap();
    eprintln!("bounded owned corpus copy+index {:?}", started.elapsed());
    let mismatch:i64=sqlx::query_scalar("SELECT count(*) FROM messages m LEFT JOIN message_search_cache c USING(session_id,uuid) WHERE c.uuid IS NULL OR c.lower_text<>LOWER(m.text) OR c.search_key<>TRANSLATE(LOWER(m.text),' :-/.','xqzvy')").fetch_one(&pool).await.unwrap();
    assert_eq!(mismatch, 0);
    assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    for mode in ["force_custom_plan", "force_generic_plan"] {
        sqlx::query(&format!("SET plan_cache_mode={mode}"))
            .execute(&pool)
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let actual = repo
            .list_sessions(Some("viewer"), Some("Local: codex"), None)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect::<Vec<_>>();
        assert!(
            actual == expected,
            "complete ordered session IDs must match the original ACL/ranking query"
        );
        assert!(actual.first().is_some_and(|id| id == "exact-title"));
        eprintln!("owned corpus repository {mode}: {:?}", started.elapsed());
        let plan:serde_json::Value=sqlx::query_scalar(&format!("EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) EXECUTE {name}('viewer','viewer','%local: codex%','%local: codex%','%local: codex%','%local: codex%','%local: codex%','local: codex')")).fetch_one(&pool).await.unwrap();
        let encoded = serde_json::to_string(&plan).unwrap();
        assert!(
            encoded.contains("idx_message_search_cache_trgm"),
            "actual repository must use cached-key GIN"
        );
        if let Ok(dir) = std::env::var("PG_SEARCH_TEST_ARTIFACT_DIR") {
            std::fs::write(
                std::path::Path::new(&dir).join(format!("repository-cache-{mode}.json")),
                serde_json::to_vec_pretty(&plan).unwrap(),
            )
            .unwrap();
        }
        eprintln!(
            "actual repository {mode} execution_ms {}",
            plan[0]["Execution Time"]
        );
    }
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
}
