//! Opt-in real PostgreSQL substring-search regression. Uses a new synthetic database.
//! PG_SEARCH_TEST_DATABASE_URL must allow CREATE DATABASE; no existing data is touched.
use conversations_server::store::{PostgresRepo, Repository};
use serde_json::Value;
use sqlx::{postgres::PgConnectOptions, PgPool};
use std::{str::FromStr, time::Instant};

#[tokio::test]
#[ignore = "requires isolated PostgreSQL in PG_SEARCH_TEST_DATABASE_URL"]
async fn substring_search_keeps_acl_and_ranking_without_scanning_transcripts() {
    let url = std::env::var("PG_SEARCH_TEST_DATABASE_URL").unwrap();
    let options = PgConnectOptions::from_str(&url).unwrap();
    let admin = PgPool::connect_with(options.clone()).await.unwrap();
    let database = format!("search_regression_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.database(&database))
        .await
        .unwrap();
    exercise(&pool).await;
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
}

async fn exercise(pool: &PgPool) {
    // Upgrade a populated shipped schema, so index creation is measured against real rows.
    let mut baseline = sqlx::migrate!("./migrations/postgres");
    baseline.migrations.to_mut().retain(|m| m.version <= 42);
    baseline.run(pool).await.unwrap();
    let rows: i64 = std::env::var("PG_SEARCH_TEST_ROWS")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(416_548);
    assert!(rows >= 30_000);
    let started = Instant::now();
    sqlx::query("INSERT INTO sessions(id,title,uploaded_by,uploaded_at,owner_email,visibility) SELECT 'bulk-'||i,'Transcript '||i,'fixture',now(),'viewer@fixture','restricted' FROM generate_series(0,2999) i")
        .execute(pool).await.unwrap();
    sqlx::query("INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) SELECT 'message-'||i,'bulk-'||((i-1)%3000),'user','user',left(repeat('async fn handle_request(session, metadata) { let result = database.query(parameters).await; assert_eq!(result.status, completed); } '||md5(i::text)||md5((i+1)::text),20),2048)||CASE WHEN i=$1 THEN ' deep QuartzNeedle match' ELSE '' END,'{}',i FROM generate_series(1,$1) i")
        .bind(rows).execute(pool).await.unwrap();
    sqlx::raw_sql("INSERT INTO sessions(id,title,title_override,uploaded_by,uploaded_at,owner_email,visibility) VALUES
        ('exact','Old title','QuartzNeedle','fixture',now()-interval '1 year','viewer@fixture','restricted'),
        ('hidden','Hidden','QuartzNeedle','fixture',now(),'other@fixture','restricted'),
        ('shared','Shared QuartzNeedle',NULL,'fixture',now(),'other@fixture','restricted'),
        ('literal','Literal 100%_done',NULL,'fixture',now(),'viewer@fixture','restricted'),
        ('unicode','Unicode',NULL,'fixture',now(),'viewer@fixture','restricted');
        INSERT INTO session_shares(session_id,email) VALUES ('shared','viewer@fixture');
        INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) VALUES
        ('hidden-body','hidden','user','user','QuartzNeedle secret','{}',1),
        ('unicode-body','unicode','user','user','Äpfel CAFÉ 🦀 xy literal %_done','{}',1);
        ANALYZE messages; ANALYZE sessions;")
        .execute(pool).await.unwrap();
    let bytes: i64 = sqlx::query_scalar("SELECT sum(octet_length(text))::bigint FROM messages")
        .fetch_one(pool)
        .await
        .unwrap();
    eprintln!(
        "synthetic seed: {rows} bulk messages, {bytes} text bytes in {:?}",
        started.elapsed()
    );
    let repo = PostgresRepo::from_pool(pool.clone());
    let started = Instant::now();
    let migrating = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.migrate().await })
    };
    let observer = PgPool::connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30),async {
        loop {
            let building:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_progress_create_index WHERE datname=current_database())").fetch_one(&observer).await.unwrap();
            if building {break;}
            assert!(!migrating.is_finished(),"expected populated concurrent index build");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    let write_started = Instant::now();
    let mut writer = observer.begin().await.unwrap();
    sqlx::query("SET LOCAL statement_timeout='1s'")
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query("INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) VALUES('during-index','bulk-0','user','user','A write during index creation','{}',0)").execute(&mut *writer).await.unwrap();
    writer.commit().await.unwrap();
    eprintln!(
        "write during concurrent build: {:?}",
        write_started.elapsed()
    );
    migrating.await.unwrap().unwrap();
    eprintln!("populated migration: {:?}", started.elapsed());
    let index_bytes: i64 =
        sqlx::query_scalar("SELECT pg_relation_size('idx_message_search_cache_trgm')")
            .fetch_one(pool)
            .await
            .unwrap();
    eprintln!("trigram index bytes: {index_bytes}");
    observer.close().await;
    repo.migrate().await.unwrap(); // A normal restart never rebuilds the index.
    sqlx::query("ANALYZE messages").execute(pool).await.unwrap();
    let started = Instant::now();
    let results = repo
        .list_sessions(Some("viewer@fixture"), Some("QuartzNeedle"), None)
        .await
        .unwrap();
    eprintln!("repository search: {:?}", started.elapsed());
    assert_eq!(
        results.first().unwrap().id,
        "exact",
        "exact effective title must rank ahead of recency"
    );
    assert!(!results.iter().any(|s| s.id == "hidden"));
    assert!(results.iter().any(|s| s.id == "shared"));
    assert!(
        results
            .iter()
            .any(|s| s.id == format!("bulk-{}", (rows - 1) % 3000)),
        "match late in the full transcript must survive"
    );
    assert_eq!(results.len(), 3);

    // Explain the actual cached Repository statement, including the generic plan
    // PostgreSQL may reuse after repeated executions, not a copied approximation.
    let name: String = sqlx::query_scalar("SELECT name FROM pg_prepared_statements WHERE statement LIKE '%COALESCE(title_override, title) AS title%' AND statement LIKE '%m.lower_text%' AND statement NOT LIKE 'SELECT name%'")
        .fetch_one(pool).await.unwrap();
    assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    for mode in ["force_custom_plan", "force_generic_plan"] {
        sqlx::query(&format!("SET plan_cache_mode={mode}"))
            .execute(pool)
            .await
            .unwrap();
        let started = Instant::now();
        let repeated = repo
            .list_sessions(Some("viewer@fixture"), Some("QuartzNeedle"), None)
            .await
            .unwrap();
        eprintln!("repository {mode}: {:?}", started.elapsed());
        assert_eq!(
            repeated.iter().map(|r| &r.id).collect::<Vec<_>>(),
            results.iter().map(|r| &r.id).collect::<Vec<_>>()
        );
        let plan:Value=sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) EXECUTE {name}('viewer@fixture','viewer@fixture','%quartzneedle%','%quartzneedle%','%quartzneedle%','%quartzneedle%','%quartzneedle%','quartzneedle')")).fetch_one(pool).await.unwrap();
        eprintln!(
            "actual repository {mode} plan: {}",
            serde_json::to_string(&plan).unwrap()
        );
        if let Ok(path) = std::env::var("PG_SEARCH_TEST_PLAN_PATH") {
            std::fs::write(
                format!("{path}.{mode}.json"),
                serde_json::to_vec_pretty(&plan).unwrap(),
            )
            .unwrap();
        }
        let mut message_nodes = Vec::new();
        relations(&plan[0]["Plan"], "message_search_cache", &mut message_nodes);
        assert!(!message_nodes.is_empty());
        for node in &message_nodes {
            assert_ne!(
                node["Node Type"], "Seq Scan",
                "substring search must use an index at scale ({mode}): {node}"
            );
            assert_eq!(
                node["Actual Loops"].as_f64().unwrap(),
                1.0,
                "body match set must be evaluated once, not once per session ({mode})"
            );
            let blocks = node["Shared Hit Blocks"].as_u64().unwrap_or(0)
                + node["Shared Read Blocks"].as_u64().unwrap_or(0);
            assert!(blocks<1000,"selective body search touched {blocks} blocks instead of a bounded index match set ({mode})");
        }
    }
    sqlx::query("SET plan_cache_mode=auto")
        .execute(pool)
        .await
        .unwrap();
    // Preserve existing LIKE wildcard/escape behavior and short-query/Unicode semantics.
    for (needle, expected) in [
        ("CAFÉ", "unicode"),
        ("xy", "unicode"),
        (r"100\%\_done", "literal"),
        (r"\%\_done", "unicode"),
    ] {
        let found = repo
            .list_sessions(Some("viewer@fixture"), Some(needle), None)
            .await
            .unwrap();
        assert!(
            found.iter().any(|s| s.id == expected),
            "missing {expected} for {needle}"
        );
        assert!(!found.iter().any(|s| s.id == "hidden"));
    }
}

fn relations<'a>(node: &'a Value, relation: &str, out: &mut Vec<&'a Value>) {
    if node["Relation Name"] == relation {
        out.push(node);
    }
    if let Some(children) = node["Plans"].as_array() {
        for child in children {
            relations(child, relation, out);
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL in PG_SEARCH_TEST_DATABASE_URL"]
async fn interrupted_concurrent_index_fails_visibly_and_recovers_without_ledger_edits() {
    let options =
        PgConnectOptions::from_str(&std::env::var("PG_SEARCH_TEST_DATABASE_URL").unwrap()).unwrap();
    let admin = PgPool::connect_with(options.clone()).await.unwrap();
    let database = format!("search_recovery_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
    let options = options.database(&database);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .unwrap();
    let observer = PgPool::connect_with(options.clone()).await.unwrap();
    let mut baseline = sqlx::migrate!("./migrations/postgres");
    baseline.migrations.to_mut().retain(|m| m.version <= 48);
    baseline.run(&pool).await.unwrap();
    sqlx::query("INSERT INTO sessions(id,title,uploaded_by,uploaded_at) VALUES('fixture','Fixture','fixture',now())").execute(&pool).await.unwrap();
    // An old writer makes CREATE INDEX CONCURRENTLY wait after catalog registration.
    let mut old_writer = observer.begin().await.unwrap();
    sqlx::query("INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) VALUES('uncommitted','fixture','user','user','uncommitted','{}',1)").execute(&mut *old_writer).await.unwrap();
    let migrating = {
        let repo = PostgresRepo::from_pool(pool.clone());
        tokio::spawn(async move { repo.migrate().await })
    };
    let pid=tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {
            let pid:Option<i32>=sqlx::query_scalar("SELECT pid FROM pg_stat_progress_create_index WHERE datname=current_database() AND phase LIKE 'waiting for writers%'").fetch_optional(&observer).await.unwrap();
            if let Some(pid)=pid {break pid;}
            assert!(!migrating.is_finished(),"build must wait on the fixture writer");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    let cancelled: bool = sqlx::query_scalar("SELECT pg_cancel_backend($1)")
        .bind(pid)
        .fetch_one(&observer)
        .await
        .unwrap();
    assert!(cancelled);
    assert!(migrating.await.unwrap().is_err());
    old_writer.rollback().await.unwrap();
    assert!(!index_valid(&observer).await);
    assert_eq!(
        migration_rows(&observer).await,
        0,
        "sqlx must not mark the failed nontransactional migration applied"
    );
    pool.close().await; // A crashed/restarted process also releases the migrator advisory lock.
    for interrupted_after_build in [false, true] {
        let retry = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        let error = PostgresRepo::from_pool(retry.clone())
            .migrate()
            .await
            .unwrap_err();
        assert!(
            format!("{error:?}").contains("already exists"),
            "unexpected error: {error:?}"
        );
        assert_eq!(migration_rows(&observer).await, 0);
        retry.close().await;
        // Exactly the documented operator repair: no edits to _sqlx_migrations.
        sqlx::query("DROP INDEX CONCURRENTLY idx_message_search_cache_trgm")
            .execute(&observer)
            .await
            .unwrap();
        let recovered = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        PostgresRepo::from_pool(recovered.clone())
            .migrate()
            .await
            .unwrap();
        assert!(index_valid(&observer).await);
        assert_eq!(migration_rows(&observer).await, 1);
        PostgresRepo::from_pool(recovered.clone())
            .migrate()
            .await
            .unwrap();
        recovered.close().await;
        if !interrupted_after_build {
            // Simulate death after index commit but before SQLx's success ledger insert.
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version>=49")
                .execute(&observer)
                .await
                .unwrap();
        }
    }
    // A crash after the old index was dropped but before migration 50 was
    // journaled can retry the DROP without rebuilding the replacement.
    let replacement_oid: i64 =
        sqlx::query_scalar("SELECT 'idx_message_search_cache_trgm'::regclass::oid::bigint")
            .fetch_one(&observer)
            .await
            .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version=50")
        .execute(&observer)
        .await
        .unwrap();
    PostgresRepo::from_pool(observer.clone())
        .migrate()
        .await
        .unwrap();
    let after_oid: i64 =
        sqlx::query_scalar("SELECT 'idx_message_search_cache_trgm'::regclass::oid::bigint")
            .fetch_one(&observer)
            .await
            .unwrap();
    assert_eq!(replacement_oid, after_oid);
    // Older images do not recognize the new ledger entry: deploy a compatible
    // image or roll forward, rather than assuming an index-only change permits rollback.
    assert!(matches!(
        baseline.run(&observer).await,
        Err(sqlx::migrate::MigrateError::VersionMissing(49))
    ));
    observer.close().await;
    sqlx::query(&format!("DROP DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
}
async fn index_valid(pool: &PgPool) -> bool {
    sqlx::query_scalar(
        "SELECT indisvalid FROM pg_index WHERE indexrelid='idx_message_search_cache_trgm'::regclass",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}
async fn migration_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version=49")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL in PG_SEARCH_TEST_DATABASE_URL"]
async fn common_phrase_search_does_not_recheck_thousands_of_large_bodies() {
    let options =
        PgConnectOptions::from_str(&std::env::var("PG_SEARCH_TEST_DATABASE_URL").unwrap()).unwrap();
    let admin = PgPool::connect_with(options.clone()).await.unwrap();
    let database = format!("search_phrase_{}", uuid::Uuid::new_v4().simple());
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
    baseline.migrations.to_mut().retain(|m| m.version <= 44);
    baseline.run(&pool).await.unwrap();
    let started = Instant::now();
    sqlx::query("INSERT INTO sessions(id,title,uploaded_by,uploaded_at,owner_email,visibility) SELECT 'phrase-'||i,'Transcript '||i,'fixture',now(),'viewer@fixture','restricted' FROM generate_series(0,2999) i").execute(&pool).await.unwrap();
    // Both common words appear in every 64-KiB body, but only one contains the phrase.
    sqlx::query("INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) SELECT 'message-'||i,'phrase-'||((i-1)%3000),'user','user','local '||left(repeat('async fn metadata() { handle_request(parameters).await; } '||md5(i::text),1000),65536)||' codex'||CASE WHEN i=12000 THEN ' Local: codex' ELSE '' END,'{}',i FROM generate_series(1,12000) i").execute(&pool).await.unwrap();
    sqlx::raw_sql("INSERT INTO sessions(id,title,uploaded_by,uploaded_at,owner_email,visibility) VALUES ('phrase-exact','Local: codex','fixture',now()-interval '1 year','viewer@fixture','restricted'),('phrase-hidden','Local: codex','fixture',now(),'other@fixture','restricted'),('phrase-collision','Collision','fixture',now(),'viewer@fixture','restricted');
    INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) VALUES ('hidden','phrase-hidden','user','user','Local: codex','{}',1),('collision','phrase-collision','user','user','localqxcodex','{}',1);
    ANALYZE messages; ANALYZE sessions;").execute(&pool).await.unwrap();
    // Match production's ~3% lossy candidates, rather than making a scan of
    // the entire table an artificially attractive alternative to the old GIN.
    sqlx::query("INSERT INTO messages(uuid,session_id,role,kind,text,raw,ord) SELECT 'background-'||i,'phrase-'||((i-1)%3000),'user','user','unrelated archive metadata '||md5(i::text),'{}',12000+i FROM generate_series(1,404548) i").execute(&pool).await.unwrap();
    sqlx::query("ANALYZE messages")
        .execute(&pool)
        .await
        .unwrap();
    eprintln!("common phrase seed/index44: {:?}", started.elapsed());
    let repo = PostgresRepo::from_pool(pool.clone());
    let started = Instant::now();
    repo.migrate().await.unwrap();
    eprintln!("common phrase upgrade: {:?}", started.elapsed());
    sqlx::query("ANALYZE messages")
        .execute(&pool)
        .await
        .unwrap();
    for mode in ["force_custom_plan", "force_generic_plan"] {
        sqlx::query(&format!("SET plan_cache_mode={mode}"))
            .execute(&pool)
            .await
            .unwrap();
        let started = Instant::now();
        let results = repo
            .list_sessions(Some("viewer@fixture"), Some("Local: codex"), None)
            .await
            .unwrap();
        eprintln!("common phrase {mode}: {:?}", started.elapsed());
        assert_eq!(
            results.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["phrase-exact", "phrase-2999"]
        );
        let name:String=sqlx::query_scalar("SELECT name FROM pg_prepared_statements WHERE statement LIKE '%COALESCE(title_override, title) AS title%' AND statement LIKE '%m.lower_text%' AND statement NOT LIKE 'SELECT name%'").fetch_one(&pool).await.unwrap();
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        let plan:Value=sqlx::query_scalar(&format!("EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) EXECUTE {name}('viewer@fixture','viewer@fixture','%local: codex%','%local: codex%','%local: codex%','%local: codex%','%local: codex%','local: codex')")).fetch_one(&pool).await.unwrap();
        eprintln!("common phrase {mode} plan: {plan}");
        if let Ok(path) = std::env::var("PG_SEARCH_TEST_PLAN_PATH") {
            std::fs::write(
                format!("{path}.phrase.{mode}.json"),
                serde_json::to_vec_pretty(&plan).unwrap(),
            )
            .unwrap();
        }
        let mut nodes = Vec::new();
        relations(&plan[0]["Plan"], "message_search_cache", &mut nodes);
        assert!(!nodes.is_empty());
        for node in nodes {
            let rejected = node["Rows Removed by Index Recheck"].as_u64().unwrap_or(0)
                + node["Rows Removed by Filter"].as_u64().unwrap_or(0);
            assert!(
                rejected < 100,
                "phrase candidate index rechecked {rejected} unrelated large bodies ({mode})"
            );
            let blocks = node["Shared Hit Blocks"].as_u64().unwrap_or(0)
                + node["Shared Read Blocks"].as_u64().unwrap_or(0);
            assert!(
                blocks < 1000,
                "phrase search touched {blocks} message/TOAST blocks ({mode})"
            );
        }
    }
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {database}"))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL in PG_SEARCH_TEST_DATABASE_URL"]
async fn separator_candidate_preserves_like_escapes_unicode_and_collisions() {
    let pool = PgPool::connect(&std::env::var("PG_SEARCH_TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    let alphabet = [
        'a', 'x', 'q', 'z', 'v', 'y', ' ', ':', '-', '/', '.', '%', '_', '\\', 'Ä', '🦀',
    ];
    let mut texts = vec![
        "local: codex".to_owned(),
        "localqxcodex".into(),
        "LOCAL: CODEX".into(),
        "Äpfel: CAFÉ".into(),
        "local:\ncodex".into(),
    ];
    let mut patterns = vec![
        "%local: codex%".to_owned(),
        "%localqxcodex%".into(),
        r"%local\:\ codex%".into(),
        "%äpfel: café%".into(),
        "%".into(),
        "_".into(),
        String::new(),
    ];
    let escape = |c: char| {
        if matches!(c, '%' | '_' | '\\') {
            format!("\\{c}")
        } else {
            c.to_lowercase().to_string()
        }
    };
    for a in alphabet {
        for b in alphabet {
            texts.push(format!("{a}{b}"));
            for gap in ["", "_", "%"] {
                patterns.push(format!("%{}{gap}{}%", escape(a), escape(b)));
            }
        }
    }
    let (lost,collisions):(i64,i64)=sqlx::query_as("SELECT count(*) FILTER (WHERE exact AND NOT candidate),count(*) FILTER (WHERE candidate AND NOT exact) FROM (SELECT LOWER(t) LIKE p AS exact,TRANSLATE(LOWER(t),' :-/.','xqzvy') LIKE TRANSLATE(p,' :-/.','xqzvy') AS candidate FROM unnest($1::text[]) t CROSS JOIN unnest($2::text[]) p) checks").bind(&texts).bind(&patterns).fetch_one(&pool).await.unwrap();
    assert_eq!(
        lost, 0,
        "candidate predicate must never remove a valid LIKE match"
    );
    assert!(
        collisions > 0,
        "noninjective key must remain guarded by original LIKE predicate"
    );
    eprintln!(
        "LIKE equivalence: {} text/pattern pairs, {collisions} collisions, no lost matches",
        texts.len() * patterns.len()
    );
}
