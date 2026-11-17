//! Database-backed [`tower_sessions::SessionStore`] for persistent login
//! sessions.
//!
//! The auth slice originally used tower-sessions' in-memory store, which is
//! wiped on every pod restart (forced re-login). This module backs sessions
//! with the same database the repository already uses, so a login survives a
//! restart / redeploy.
//!
//! Because the data backend is chosen at runtime from `DATABASE_URL` (SQLite by
//! default, Postgres when configured), [`SessionBackend`] is an enum over the
//! two sqlx-backed stores from `tower-sessions-sqlx-store`. It implements
//! [`tower_sessions::SessionStore`] by delegating every method to whichever
//! inner store is active, so a single `SessionManagerLayer<SessionBackend>`
//! works for both backends.

use tower_sessions::session::{Id, Record};
use tower_sessions::session_store;
use tower_sessions::SessionStore;
use tower_sessions_sqlx_store::{PostgresStore, SqliteStore};

/// A persistent, database-backed session store, generic over the active
/// repository backend. Built by [`crate::store::Repository::session_store`].
#[derive(Clone, Debug)]
pub enum SessionBackend {
    /// SQLite-backed session store (the default backend).
    Sqlite(SqliteStore),
    /// Postgres-backed session store.
    Postgres(PostgresStore),
}

#[async_trait::async_trait]
impl SessionStore for SessionBackend {
    async fn create(&self, session_record: &mut Record) -> session_store::Result<()> {
        match self {
            SessionBackend::Sqlite(store) => store.create(session_record).await,
            SessionBackend::Postgres(store) => store.create(session_record).await,
        }
    }

    async fn save(&self, session_record: &Record) -> session_store::Result<()> {
        match self {
            SessionBackend::Sqlite(store) => store.save(session_record).await,
            SessionBackend::Postgres(store) => store.save(session_record).await,
        }
    }

    async fn load(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        match self {
            SessionBackend::Sqlite(store) => store.load(session_id).await,
            SessionBackend::Postgres(store) => store.load(session_id).await,
        }
    }

    async fn delete(&self, session_id: &Id) -> session_store::Result<()> {
        match self {
            SessionBackend::Sqlite(store) => store.delete(session_id).await,
            SessionBackend::Postgres(store) => store.delete(session_id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Repository, SqliteRepo};
    use time::OffsetDateTime;

    /// Sessions persist across a simulated pod restart: a record written through
    /// a `SessionBackend` built from one repo on a temp-file DB is still loadable
    /// from a *fresh* `SessionBackend` built on the same file (in-memory sqlite
    /// is per-connection, so a temp file is required to survive reconnection).
    #[tokio::test]
    async fn session_survives_restart() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("conv-session-{nanos}.sqlite"));
        let url = format!("sqlite://{}?mode=rwc", path.display());

        // First "boot": connect, build the session store, write a session.
        let repo = SqliteRepo::connect(&url).await.expect("connect + migrate");
        let store = repo.session_store().await.expect("session store");

        let mut record = Record {
            id: Id::default(),
            data: std::collections::HashMap::new(),
            expiry_date: OffsetDateTime::now_utc() + time::Duration::hours(1),
        };
        record
            .data
            .insert("email".to_string(), serde_json::json!("alice@example.com"));
        store.create(&mut record).await.expect("create session");
        store.save(&record).await.expect("save session");
        let id = record.id;
        drop(store);
        drop(repo);

        // Second "boot": fresh repo + store on the SAME file. The record must
        // still be loadable.
        let repo2 = SqliteRepo::connect(&url)
            .await
            .expect("reconnect + migrate");
        let store2 = repo2.session_store().await.expect("session store 2");
        let loaded = store2
            .load(&id)
            .await
            .expect("load")
            .expect("record present");
        assert_eq!(loaded.id, id);
        assert_eq!(
            loaded.data.get("email"),
            Some(&serde_json::json!("alice@example.com"))
        );

        drop(store2);
        drop(repo2);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}
