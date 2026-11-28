//! SQLite implementation of [`Repository`] backed by an sqlx `SqlitePool`.
//!
//! All SQL uses sqlx's runtime query API (`sqlx::query` / `query_as`) rather
//! than the compile-time `query!` macros, so the crate builds without a live
//! database / `DATABASE_URL` at build time.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use conversations_core as core;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, Result};

use super::nul::{scrub_opt, scrub_session, scrub_str};
use super::publication::SourceContext;
use super::session::SessionBackend;
use super::{
    validate_visibility, Chat, ChatMessage, ChatParent, ChatWithMessages, LiveSession, LiveShare,
    LiveStoredMessage, NewLiveSession, PasswordResetToken, PrWatchTarget, Project, ProjectCount,
    ProjectDetail, RepoLinkInput, RepoPath, Repository, SessionLinearDoc, SessionPr,
    SessionPrWithStatus, SessionSummary, StoredPrComment, StoredRepoLink, StoredSession,
    StoredSummary, UpsertOutcome, User, LIVE_STATUS_ABANDONED, LIVE_STATUS_GRADUATED,
    LIVE_STATUS_LIVE, USER_SEARCH_MAX_LIMIT, VISIBILITY_ORGANIZATION, VISIBILITY_RESTRICTED,
};
use uuid::Uuid;

/// SQLite-backed repository.
#[derive(Clone)]
pub struct SqliteRepo {
    pool: SqlitePool,
}

fn stored_pr_comment(r: &SqliteRow) -> StoredPrComment {
    StoredPrComment {
        owner: r.get("owner"),
        repo: r.get("repo"),
        number: r.get("number"),
        comment_id: r.get("comment_id"),
        kind: r.get("kind"),
        author: r.get("author"),
        body: r.get("body"),
        html_url: r.get("html_url"),
        created_at: r.get("created_at"),
        path: r.get("path"),
        line: r.get("line"),
        diff_hunk: r.get("diff_hunk"),
        resolved: r.get::<Option<bool>, _>("resolved"),
        ingested_at: str_to_ts(r.get("ingested_at")).unwrap_or_else(Utc::now),
        revision: r.get("revision"),
    }
}

impl SqliteRepo {
    /// Connect to `database_url`, run migrations, and return the repo.
    ///
    /// The connection is configured for safe concurrent access: WAL journaling
    /// (readers don't block the writer), `synchronous = NORMAL` (durable under
    /// WAL, much faster than FULL), a 5s `busy_timeout` so a brief write lock
    /// retries instead of surfacing `SQLITE_BUSY` as a 500, and per-connection
    /// foreign-key enforcement.
    pub async fn connect(database_url: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(database_url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;
        let repo = SqliteRepo { pool };
        repo.migrate().await?;
        Ok(repo)
    }

    /// Build a repo from an already-constructed pool (used in tests).
    pub fn from_pool(pool: SqlitePool) -> Self {
        SqliteRepo { pool }
    }

    /// Run the embedded migrations and enable FK enforcement.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::query("PRAGMA foreign_keys = ON;")
            .execute(&self.pool)
            .await?;
        sqlx::migrate!("./migrations/sqlite")
            .run(&self.pool)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
        Ok(())
    }

    /// Borrow the underlying pool.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// Render an optional timestamp as RFC3339 for storage.
fn ts_to_str(ts: &Option<DateTime<Utc>>) -> Option<String> {
    ts.map(|t| t.to_rfc3339())
}

/// Parse an RFC3339 string column back into a UTC timestamp.
fn str_to_ts(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|t| t.with_timezone(&Utc))
}

#[async_trait]
impl Repository for SqliteRepo {
    async fn live_order(&self, viewer: &str) -> Result<super::LiveOrder> {
        let row = sqlx::query("SELECT revision, session_ids FROM live_tab_order WHERE viewer = ?")
            .bind(viewer)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(super::LiveOrder {
                revision: row.try_get("revision")?,
                session_ids: serde_json::from_str(row.try_get::<&str, _>("session_ids")?)?,
            }),
            None => Ok(super::LiveOrder::default()),
        }
    }

    async fn set_live_order(&self, viewer: &str, revision: i64, ids: &[String]) -> Result<bool> {
        let result = sqlx::query("INSERT INTO live_tab_order (viewer, revision, session_ids) VALUES (?, 1, ?) ON CONFLICT(viewer) DO UPDATE SET revision = live_tab_order.revision + 1, session_ids = excluded.session_ids WHERE live_tab_order.revision = ?")
            .bind(viewer).bind(serde_json::to_string(ids)?).bind(revision).execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    async fn health(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    async fn upsert_session(
        &self,
        uploaded_by: &str,
        source_host: Option<&str>,
        owner_email: Option<&str>,
        session: &core::Session,
    ) -> Result<UpsertOutcome> {
        // Scrubbed on both backends so the same upload stores the same bytes
        // everywhere: SQLite accepts a NUL but truncates at it in LIKE/substr,
        // which would silently break search. See `store::nul`.
        let session = scrub_session(session);
        let session = session.as_ref();
        let uploaded_by = scrub_str(uploaded_by);
        let uploaded_by = uploaded_by.as_ref();
        let source_host = scrub_opt(source_host);
        let source_host = source_host.as_deref();
        let owner_email = scrub_opt(owner_email);
        let owner_email = owner_email.as_deref();

        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, session.id.as_str());

        // Did this session already exist?
        let existed: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = ?")
            .bind(&session.id)
            .fetch_optional(&mut *tx)
            .await?;
        let created = existed.is_none();

        let message_count = session.message_count();
        let uploaded_at = Utc::now().to_rfc3339();

        // Upsert the session row. On CREATE owner_email/visibility are set from
        // the insert values; on RE-UPLOAD (ON CONFLICT) those columns are
        // deliberately NOT touched, so sharing settings are preserved.
        sqlx::query(
            r#"
            INSERT INTO sessions
                (id, agent, title, project_path, cwd, git_branch, claude_version,
                 started_at, ended_at, message_count, skipped_lines,
                 uploaded_by, uploaded_at, source_host, owner_email, visibility)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'restricted')
            ON CONFLICT(id) DO UPDATE SET
                agent          = excluded.agent,
                title          = excluded.title,
                project_path   = excluded.project_path,
                cwd            = excluded.cwd,
                git_branch     = excluded.git_branch,
                claude_version = excluded.claude_version,
                started_at     = excluded.started_at,
                ended_at       = excluded.ended_at,
                message_count  = excluded.message_count,
                skipped_lines  = excluded.skipped_lines,
                uploaded_by    = excluded.uploaded_by,
                uploaded_at    = excluded.uploaded_at,
                source_host    = excluded.source_host
            "#,
        )
        .bind(&session.id)
        .bind(session.agent.as_str())
        .bind(&session.title)
        .bind(&session.project_path)
        .bind(&session.cwd)
        .bind(&session.git_branch)
        .bind(&session.claude_version)
        .bind(ts_to_str(&session.started_at))
        .bind(ts_to_str(&session.ended_at))
        .bind(message_count as i64)
        .bind(session.skipped_lines as i64)
        .bind(uploaded_by)
        .bind(&uploaded_at)
        .bind(source_host)
        .bind(owner_email)
        .execute(&mut *tx)
        .await?;

        // Full replace of messages.
        sqlx::query("DELETE FROM messages WHERE session_id = ?")
            .bind(&session.id)
            .execute(&mut *tx)
            .await?;

        for (ord, msg) in session.messages.iter().enumerate() {
            let role = serde_json::to_value(msg.role)?
                .as_str()
                .unwrap_or("user")
                .to_string();
            let kind = serde_json::to_value(msg.kind)?
                .as_str()
                .unwrap_or("mixed")
                .to_string();
            let raw = serde_json::to_string(&msg.raw)?;
            let usage = msg.usage.as_ref().map(serde_json::to_string).transpose()?;

            sqlx::query(
                r#"
                INSERT INTO messages
                    (uuid, session_id, parent_uuid, role, kind, model,
                     is_sidechain, timestamp, text, raw, usage, ord)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(&msg.uuid)
            .bind(&session.id)
            .bind(&msg.parent_uuid)
            .bind(&role)
            .bind(&kind)
            .bind(&msg.model)
            .bind(msg.is_sidechain as i64)
            .bind(ts_to_str(&msg.timestamp))
            .bind(&msg.text)
            .bind(&raw)
            .bind(&usage)
            .bind(ord as i64)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        Ok(UpsertOutcome {
            created,
            message_count,
        })
    }

    async fn list_sessions(
        &self,
        viewer: Option<&str>,
        q: Option<&str>,
        project: Option<&str>,
    ) -> Result<Vec<StoredSummary>> {
        // Build the query dynamically with sequential positional placeholders,
        // binding values in the same order the clauses are appended.
        let q = q.map(str::trim).filter(|query| !query.is_empty());
        let mut sql = String::from(
            r#"
            SELECT id, COALESCE(title_override, title) AS title, project_path,
                   agent, git_branch, message_count,
                   started_at, ended_at, uploaded_by, uploaded_at, source_host,
                   owner_email, visibility,
                   EXISTS (SELECT 1 FROM live_sessions l
                           WHERE l.id = sessions.id AND l.status = 'live') AS is_live
            FROM sessions
            "#,
        );
        let mut conditions: Vec<String> = Vec::new();
        // Ordered bind values, matching the order of `?` placeholders below.
        let mut binds: Vec<String> = Vec::new();

        // Visibility enforcement: only applied when a viewer is provided.
        if let Some(email) = viewer {
            conditions.push(
                "(visibility = 'organization' OR owner_email = ? \
                 OR EXISTS (SELECT 1 FROM session_shares s \
                    WHERE s.session_id = sessions.id AND s.email = ?) OR EXISTS (SELECT 1 FROM live_session_shares ls WHERE ls.live_id = sessions.id AND ls.communication_only = 0 AND ls.email = ?))"
                    .to_string(),
            );
            binds.push(email.to_string());
            binds.push(email.to_string());
            binds.push(email.to_string());
        }

        if let Some(needle) = q {
            // Lowercase both the pattern and each column so search is
            // case-insensitive and portable (the Postgres backend does the
            // same). The same pattern is used for the title/uploader/project/
            // message clauses; push it four times so each `?` has its own
            // bound value, then a fifth PR-url pattern.
            let pattern = format!("%{}%", needle.to_lowercase());
            // PR refs also match against the stored repo `pr_url`; strip a
            // leading '#' so both "#123" and "123" find a `…/pull/123` link.
            let pr_pattern = format!("%{}%", needle.trim_start_matches('#').to_lowercase());
            let mut or_clauses = vec![
                "LOWER(COALESCE(title_override, title)) LIKE ?".to_string(),
                "LOWER(IFNULL(uploaded_by, '')) LIKE ?".to_string(),
                "LOWER(IFNULL(project_path, '')) LIKE ?".to_string(),
                "EXISTS (SELECT 1 FROM messages m \
                    WHERE m.session_id = sessions.id AND LOWER(m.text) LIKE ?)"
                    .to_string(),
                "EXISTS (SELECT 1 FROM session_repos r \
                    WHERE r.session_id = sessions.id AND LOWER(IFNULL(r.pr_url, '')) LIKE ?)"
                    .to_string(),
            ];
            for _ in 0..4 {
                binds.push(pattern.clone());
            }
            binds.push(pr_pattern);

            // If the query is a PR reference (URL or owner/repo#N), also match
            // sessions linked to that PR via the dedicated `session_prs` table —
            // text/pr_url search alone never touches it. `number` is bound as
            // TEXT (cast for portability).
            if let Some((owner, repo, number)) = crate::github::parse_pr_query(needle) {
                or_clauses.push(
                    "EXISTS (SELECT 1 FROM session_prs p \
                     WHERE p.session_id = sessions.id \
                       AND LOWER(p.owner) = ? AND LOWER(p.repo) = ? \
                       AND CAST(p.number AS TEXT) = ?)"
                        .to_string(),
                );
                binds.push(owner.to_lowercase());
                binds.push(repo.to_lowercase());
                binds.push(number.to_string());
            }

            conditions.push(format!("({})", or_clauses.join(" OR ")));
        }
        if let Some(p) = project {
            conditions.push("project_path = ?".to_string());
            binds.push(p.to_string());
        }

        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        // Rank the complete result set before callers display it. Effective
        // title equality is literal, so SQL wildcard characters carry no ranking authority.
        sql.push_str(" ORDER BY ");
        if let Some(needle) = q {
            sql.push_str(
                "CASE WHEN LOWER(TRIM(COALESCE(title_override, title))) = ? THEN 0 ELSE 1 END, ",
            );
            binds.push(needle.to_lowercase());
        }
        sql.push_str("is_live DESC, started_at DESC, uploaded_at DESC");

        let mut query = sqlx::query(&sql);
        for b in &binds {
            query = query.bind(b);
        }

        let rows = query.fetch_all(&self.pool).await?;
        let mut summaries: Vec<StoredSummary> = rows.iter().map(summary_from_row).collect();

        // Batch-load repos for all listed sessions (avoids N+1).
        let ids: Vec<String> = summaries.iter().map(|s| s.id.clone()).collect();
        let mut by_session: std::collections::HashMap<String, Vec<StoredRepoLink>> = self
            .get_repos_for_sessions(&ids)
            .await?
            .into_iter()
            .collect();
        for s in &mut summaries {
            if let Some(repos) = by_session.remove(&s.id) {
                s.repos = repos;
            }
        }
        Ok(summaries)
    }

    async fn get_session(&self, viewer: Option<&str>, id: &str) -> Result<Option<StoredSession>> {
        let row = sqlx::query(
            r#"
            SELECT id, COALESCE(title_override, title) AS title, project_path,
                   cwd, agent, agent_session_id, git_branch, claude_version,
                   started_at, ended_at, message_count, skipped_lines,
                   uploaded_by, uploaded_at, source_host, owner_email, visibility,
                   project_id
            FROM sessions WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        // Load the share list for this session.
        let share_rows = sqlx::query("SELECT email FROM session_shares WHERE session_id = ?")
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
        let shared_emails: Vec<String> = share_rows.iter().map(|r| r.get("email")).collect();

        // Visibility enforcement: a restricted viewer who is neither owner nor
        // in the share list cannot see the session (return None so handler 404s).
        let visibility: String = row.get("visibility");
        let owner_email: Option<String> = row.get("owner_email");
        // Whether THIS viewer was named by the publishing host as intended to
        // write into the session once their key is trusted there.
        let viewer_write_pending_key = match viewer {
            Some(email) => sqlx::query_scalar::<sqlx::Sqlite, i64>(
                "SELECT write_pending_key FROM session_shares WHERE session_id = ? AND email = ?",
            )
            .bind(id)
            .bind(email)
            .fetch_optional(&self.pool)
            .await?
            .map(|v| v != 0)
            .unwrap_or(false),
            None => false,
        };
        if let Some(email) = viewer {
            let person_read = sqlx::query("SELECT 1 FROM live_session_shares WHERE live_id = ? AND email = ? AND communication_only = 0")
                .bind(id).bind(email).fetch_optional(&self.pool).await?.is_some();
            let visible = visibility == VISIBILITY_ORGANIZATION
                || owner_email.as_deref() == Some(email)
                || shared_emails.iter().any(|e| e == email)
                || person_read;
            if !visible {
                return Ok(None);
            }
        }

        let msg_rows = sqlx::query(
            r#"
            SELECT uuid, parent_uuid, role, kind, model, is_sidechain,
                   timestamp, text, raw, usage
            FROM messages WHERE session_id = ? ORDER BY ord ASC
            "#,
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;

        let mut messages = Vec::with_capacity(msg_rows.len());
        for m in &msg_rows {
            messages.push(message_from_row(m)?);
        }

        let repos = self.get_repos(id).await?;
        let summary = self.get_summary(id).await?;

        Ok(Some(StoredSession {
            id: row.get("id"),
            agent: super::parse_agent(row.try_get::<String, _>("agent").ok().as_deref()),
            agent_session_id: row.get("agent_session_id"),
            title: row.get("title"),
            project_path: row.get("project_path"),
            cwd: row.get("cwd"),
            git_branch: row.get("git_branch"),
            claude_version: row.get("claude_version"),
            started_at: str_to_ts(row.get("started_at")),
            ended_at: str_to_ts(row.get("ended_at")),
            message_count: row.get::<i64, _>("message_count") as usize,
            skipped_lines: row.get::<i64, _>("skipped_lines") as usize,
            uploaded_by: row.get("uploaded_by"),
            uploaded_at: str_to_ts(row.get("uploaded_at")).unwrap_or_else(Utc::now),
            source_host: row.get("source_host"),
            owner_email,
            visibility,
            project_id: row.get("project_id"),
            shared_emails,
            viewer_write_pending_key,
            repos,
            summary,
            messages,
        }))
    }

    async fn get_session_owner(&self, id: &str) -> Result<Option<Option<String>>> {
        let row = sqlx::query("SELECT owner_email FROM sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<Option<String>, _>("owner_email")))
    }

    async fn set_title_override(&self, id: &str, title: Option<&str>) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, id);
        let result = sqlx::query("UPDATE sessions SET title_override = ? WHERE id = ?")
            .bind(title)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    async fn get_title_override(&self, id: &str) -> Result<Option<String>> {
        let row: Option<Option<String>> =
            sqlx::query_scalar("SELECT title_override FROM sessions WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.flatten())
    }

    async fn delete_session(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, id);
        // `session_prs` no longer carries an ON DELETE CASCADE FK (relaxed in
        // migration 0016 so PRs can be linked to a live session), so delete its
        // rows explicitly. Other child rows (messages, shares, repos, per-user
        // paths, pr_comments) still cascade via their FK.
        sqlx::query("DELETE FROM session_prs WHERE session_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // `session_linear_docs` likewise has no cascade FK (see migration 0021).
        sqlx::query("DELETE FROM session_linear_docs WHERE session_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn list_projects(&self) -> Result<Vec<ProjectCount>> {
        let rows = sqlx::query(
            r#"
            SELECT project_path, COUNT(*) AS session_count
            FROM sessions
            WHERE project_path IS NOT NULL AND project_path <> ''
            GROUP BY project_path
            ORDER BY session_count DESC, project_path ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| ProjectCount {
                project_path: r.get("project_path"),
                session_count: r.get::<i64, _>("session_count") as usize,
            })
            .collect())
    }

    async fn upsert_user(&self, email: &str, display_name: Option<&str>) -> Result<User> {
        let now = Utc::now().to_rfc3339();
        // On conflict, refresh last_seen_at and only overwrite display_name when
        // a new (non-null) value is supplied.
        sqlx::query(
            r#"
            INSERT INTO users (email, display_name, created_at, last_seen_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(email) DO UPDATE SET
                display_name = COALESCE(excluded.display_name, users.display_name),
                last_seen_at = excluded.last_seen_at
            "#,
        )
        .bind(email)
        .bind(display_name)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query(
            "SELECT email, display_name, created_at, last_seen_at FROM users WHERE email = ?",
        )
        .bind(email)
        .fetch_one(&self.pool)
        .await?;
        Ok(user_from_row(&row))
    }

    async fn list_users(&self) -> Result<Vec<User>> {
        let rows = sqlx::query(
            "SELECT email, display_name, created_at, last_seen_at FROM users ORDER BY email ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(user_from_row).collect())
    }

    async fn delete_user(&self, email: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM session_shares WHERE email = ?")
            .bind(email)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM live_session_shares WHERE email = ?")
            .bind(email)
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query("DELETE FROM users WHERE email = ?")
            .bind(email)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(deleted > 0)
    }

    async fn browser_keys(&self, email: &str) -> Result<Vec<crate::user_keys::BrowserKey>> {
        let rows = sqlx::query("SELECT kid, public_key, label, created_at FROM browser_keys WHERE user_email = $1 ORDER BY created_at, kid")
            .bind(email).fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| crate::user_keys::BrowserKey {
                kid: r.get("kid"),
                alg: "es256".into(),
                public_key: r.get("public_key"),
                label: r.get("label"),
                created_at: str_to_ts(r.get("created_at")).unwrap_or_else(Utc::now),
            })
            .collect())
    }

    async fn save_browser_key_challenge(&self, c: &crate::user_keys::Challenge) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM browser_key_challenges WHERE expires_at < $1 OR user_email = $2")
            .bind(Utc::now().timestamp())
            .bind(&c.user_email)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO browser_key_challenges(challenge,user_email,payload,expires_at) VALUES($1,$2,$3,$4)")
            .bind(&c.challenge).bind(&c.user_email).bind(&c.payload).bind(c.expires_at).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn browser_key_challenge(
        &self,
        id: &str,
        email: &str,
        now: i64,
    ) -> Result<Option<crate::user_keys::Challenge>> {
        let row = sqlx::query("SELECT challenge,user_email,payload,expires_at FROM browser_key_challenges WHERE challenge=$1 AND user_email=$2 AND expires_at >= $3")
            .bind(id).bind(email).bind(now).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| crate::user_keys::Challenge {
            challenge: r.get("challenge"),
            user_email: r.get("user_email"),
            payload: r.get("payload"),
            expires_at: r.get("expires_at"),
        }))
    }

    async fn publish_browser_key(
        &self,
        c: &crate::user_keys::Challenge,
        key: &crate::user_keys::BrowserKey,
        now: i64,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let removed = sqlx::query("DELETE FROM browser_key_challenges WHERE challenge=$1 AND user_email=$2 AND payload=$3 AND expires_at >= $4")
            .bind(&c.challenge).bind(&c.user_email).bind(&c.payload).bind(now).execute(&mut *tx).await?.rows_affected();
        if removed != 1 {
            return Ok(false);
        }
        sqlx::query("INSERT INTO browser_keys(user_email,kid,public_key,label,created_at) VALUES($1,$2,$3,$4,$5) ON CONFLICT(user_email,kid) DO UPDATE SET label=excluded.label")
            .bind(&c.user_email).bind(&key.kid).bind(&key.public_key).bind(&key.label).bind(key.created_at.to_rfc3339()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn search_users(&self, q: Option<&str>, limit: i64) -> Result<Vec<User>> {
        let limit = limit.clamp(1, USER_SEARCH_MAX_LIMIT);
        let rows = match q {
            Some(needle) if !needle.is_empty() => {
                let pattern = format!("%{}%", needle.to_lowercase());
                sqlx::query(
                    r#"
                    SELECT email, display_name, created_at, last_seen_at
                    FROM users
                    WHERE LOWER(email) LIKE ?
                       OR LOWER(IFNULL(display_name, '')) LIKE ?
                    ORDER BY email ASC
                    LIMIT ?
                    "#,
                )
                .bind(&pattern)
                .bind(&pattern)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            _ => {
                sqlx::query(
                    r#"
                    SELECT email, display_name, created_at, last_seen_at
                    FROM users
                    ORDER BY email ASC
                    LIMIT ?
                    "#,
                )
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.iter().map(user_from_row).collect())
    }

    async fn get_user_with_password(&self, email: &str) -> Result<Option<(User, Option<String>)>> {
        let row = sqlx::query(
            "SELECT email, display_name, created_at, last_seen_at, password_hash \
             FROM users WHERE email = ?",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let hash: Option<String> = r.get("password_hash");
            (user_from_row(&r), hash)
        }))
    }

    async fn set_password(&self, email: &str, password_hash: &str) -> Result<()> {
        let res = sqlx::query("UPDATE users SET password_hash = ? WHERE email = ?")
            .bind(password_hash)
            .bind(email)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    async fn reserve_signup_token(
        &self,
        email: &str,
        token_hash: &str,
        now: chrono::DateTime<chrono::Utc>,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM signup_tokens WHERE expires_at <= ?")
            .bind(now.to_rfc3339())
            .execute(&mut *tx)
            .await?;
        let reserved = sqlx::query(
            "INSERT INTO signup_tokens (email, token_hash, sent_at, expires_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT (email) DO UPDATE SET
                 token_hash = excluded.token_hash,
                 sent_at = excluded.sent_at,
                 expires_at = excluded.expires_at
             WHERE signup_tokens.sent_at <= ?",
        )
        .bind(email)
        .bind(token_hash)
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .bind((now - chrono::Duration::minutes(1)).to_rfc3339())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(reserved.rows_affected() == 1)
    }

    async fn remove_signup_token(&self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM signup_tokens WHERE token_hash = ?")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn has_valid_signup_token(
        &self,
        token_hash: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        let found = sqlx::query_scalar::<_, i32>(
            "SELECT 1 FROM signup_tokens WHERE token_hash = ? AND expires_at > ?",
        )
        .bind(token_hash)
        .bind(now.to_rfc3339())
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    async fn complete_signup(
        &self,
        token_hash: &str,
        password_hash: &str,
        display_name: Option<&str>,
        reserved_admins: &[String],
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let email: Option<String> = sqlx::query_scalar(
            "DELETE FROM signup_tokens WHERE token_hash = ? AND expires_at > ? RETURNING email",
        )
        .bind(token_hash)
        .bind(now.to_rfc3339())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(email) = email else {
            return Ok(false);
        };
        if reserved_admins.contains(&email) {
            return Ok(false);
        }
        let created = sqlx::query(
            "INSERT INTO users (email, display_name, password_hash, created_at, last_seen_at)
             SELECT ?, ?, ?, ?, ?
             WHERE NOT EXISTS (SELECT 1 FROM user_admins WHERE user_email = ?)
             ON CONFLICT (email) DO UPDATE SET
                 password_hash = excluded.password_hash,
                 display_name = COALESCE(excluded.display_name, users.display_name),
                 last_seen_at = excluded.last_seen_at
             WHERE users.password_hash IS NULL
               AND NOT EXISTS (SELECT 1 FROM user_admins WHERE user_email = users.email)",
        )
        .bind(&email)
        .bind(display_name)
        .bind(password_hash)
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(&email)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(created.rows_affected() == 1)
    }

    async fn get_user_auth_version(&self, email: &str) -> Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT auth_version FROM users WHERE email = ?")
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(version,)| version))
    }

    async fn create_password_reset_token(
        &self,
        user_email: &str,
        created_by: &str,
        token_id: &str,
        secret_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM users WHERE email = ?")
            .bind(user_email)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::NotFound);
        }

        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE password_reset_tokens SET used_at = ? \
             WHERE user_email = ? AND used_at IS NULL",
        )
        .bind(&now)
        .bind(user_email)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO password_reset_tokens \
             (token_id, user_email, secret_hash, created_by, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(token_id)
        .bind(user_email)
        .bind(secret_hash)
        .bind(created_by)
        .bind(&now)
        .bind(expires_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_password_reset_token(
        &self,
        token_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<PasswordResetToken>> {
        let row = sqlx::query(
            "SELECT token_id, user_email, secret_hash, expires_at \
             FROM password_reset_tokens \
             WHERE token_id = ? AND used_at IS NULL AND expires_at > ?",
        )
        .bind(token_id)
        .bind(now.to_rfc3339())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| PasswordResetToken {
            token_id: r.get("token_id"),
            user_email: r.get("user_email"),
            secret_hash: r.get("secret_hash"),
            expires_at: str_to_ts(r.get("expires_at")).unwrap_or_else(Utc::now),
        }))
    }

    async fn consume_password_reset_token(
        &self,
        token_id: &str,
        password_hash: &str,
        used_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let used_at = used_at.to_rfc3339();
        let claimed = sqlx::query(
            "UPDATE password_reset_tokens SET used_at = ? \
             WHERE token_id = ? AND used_at IS NULL AND expires_at > ?",
        )
        .bind(&used_at)
        .bind(token_id)
        .bind(&used_at)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(false);
        }

        let changed = sqlx::query(
            "UPDATE users SET password_hash = ?, auth_version = auth_version + 1 WHERE email = \
             (SELECT user_email FROM password_reset_tokens WHERE token_id = ?)",
        )
        .bind(password_hash)
        .bind(token_id)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn is_user_admin(&self, email: &str) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM user_admins WHERE user_email = ?")
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    async fn list_admin_emails(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT user_email FROM user_admins ORDER BY user_email ASC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|row| row.get("user_email")).collect())
    }

    async fn set_user_admin(&self, email: &str, is_admin: bool, granted_by: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM users WHERE email = ?")
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::NotFound);
        }

        if is_admin {
            sqlx::query(
                "INSERT INTO user_admins (user_email, granted_by, granted_at) \
                 VALUES (?, ?, ?) \
                 ON CONFLICT(user_email) DO UPDATE SET \
                    granted_by = excluded.granted_by, \
                    granted_at = excluded.granted_at",
            )
            .bind(email)
            .bind(granted_by)
            .bind(Utc::now().to_rfc3339())
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query("DELETE FROM user_admins WHERE user_email = ?")
                .bind(email)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn set_sharing(
        &self,
        session_id: &str,
        visibility: &str,
        emails: &[String],
    ) -> Result<()> {
        validate_visibility(visibility)?;

        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, session_id);

        // 404 surface: caller checks existence first, but guard here too.
        let existed: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
        if existed.is_none() {
            return Err(AppError::NotFound);
        }

        sqlx::query("UPDATE sessions SET visibility = ? WHERE id = ?")
            .bind(visibility)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        // Replace the share list.
        sqlx::query("DELETE FROM session_shares WHERE session_id = ?")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        // For organization visibility we clear the share list (it is ignored).
        if visibility == VISIBILITY_RESTRICTED {
            for email in super::dedup_nonempty(emails) {
                sqlx::query(
                    "INSERT OR IGNORE INTO session_shares (session_id, email) VALUES (?, ?)",
                )
                .bind(session_id)
                .bind(&email)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }

    async fn get_sharing(&self, session_id: &str) -> Result<Option<(String, Vec<String>)>> {
        let row = sqlx::query("SELECT visibility FROM sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let visibility: String = row.get("visibility");

        let share_rows = sqlx::query("SELECT email FROM session_shares WHERE session_id = ?")
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?;
        let emails: Vec<String> = share_rows.iter().map(|r| r.get("email")).collect();
        Ok(Some((visibility, emails)))
    }

    async fn merge_repos(
        &self,
        session_id: &str,
        uploader: Option<&str>,
        links: &[core::RepoLink],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, session_id);
        for link in links {
            let repo = link.repo.trim();
            let branch = link.branch.trim();
            if repo.is_empty() || branch.is_empty() {
                continue;
            }
            // Upsert the canonical row; COALESCE keeps an existing non-null
            // remote/PR rather than letting a later null clobber it.
            sqlx::query(
                r#"
                INSERT INTO session_repos (session_id, repo, branch, remote_url, pr_url, is_container)
                VALUES (?, ?, ?, ?, ?, ?)
                ON CONFLICT(session_id, repo, branch) DO UPDATE SET
                    remote_url   = COALESCE(excluded.remote_url, session_repos.remote_url),
                    pr_url       = COALESCE(excluded.pr_url, session_repos.pr_url),
                    -- Sticky-true: once any uploader flags the repo a container,
                    -- a later flag-less push (older CLI) must not clear it.
                    is_container = MAX(session_repos.is_container, excluded.is_container)
                "#,
            )
            .bind(session_id)
            .bind(repo)
            .bind(branch)
            .bind(link.remote_url.as_deref())
            .bind(link.pr_url.as_deref())
            .bind(link.is_container)
            .execute(&mut *tx)
            .await?;

            // Record this user's local path, when both are known.
            if let (Some(user), Some(path)) = (uploader, link.local_path.as_deref()) {
                let path = path.trim();
                if !user.is_empty() && !path.is_empty() {
                    sqlx::query(
                        r#"
                        INSERT INTO session_repo_paths
                            (session_id, repo, branch, user_email, local_path)
                        VALUES (?, ?, ?, ?, ?)
                        ON CONFLICT(session_id, repo, branch, user_email) DO UPDATE SET
                            local_path = excluded.local_path
                        "#,
                    )
                    .bind(session_id)
                    .bind(repo)
                    .bind(branch)
                    .bind(user)
                    .bind(path)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn set_repos(&self, session_id: &str, links: &[RepoLinkInput]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, session_id);

        let existed: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
        if existed.is_none() {
            return Err(AppError::NotFound);
        }

        // Normalize + dedupe the desired (repo, branch) set.
        let keep = super::distinct_repo_links(links);

        // Drop canonical rows no longer present. Their paths cascade away via
        // the FK; surviving rows keep their per-user paths.
        let existing: Vec<(String, String)> =
            sqlx::query_as("SELECT repo, branch FROM session_repos WHERE session_id = ?")
                .bind(session_id)
                .fetch_all(&mut *tx)
                .await?;
        for (repo, branch) in &existing {
            let still = keep
                .iter()
                .any(|l| l.repo.trim() == repo && l.branch.trim() == branch);
            if !still {
                sqlx::query(
                    "DELETE FROM session_repos WHERE session_id = ? AND repo = ? AND branch = ?",
                )
                .bind(session_id)
                .bind(repo)
                .bind(branch)
                .execute(&mut *tx)
                .await?;
            }
        }

        // Upsert the surviving/new rows (replace remote/pr authoritatively).
        for l in &keep {
            sqlx::query(
                r#"
                INSERT INTO session_repos (session_id, repo, branch, remote_url, pr_url)
                VALUES (?, ?, ?, ?, ?)
                ON CONFLICT(session_id, repo, branch) DO UPDATE SET
                    remote_url = excluded.remote_url,
                    pr_url     = excluded.pr_url
                "#,
            )
            .bind(session_id)
            .bind(l.repo.trim())
            .bind(l.branch.trim())
            .bind(l.remote_url.as_deref())
            .bind(l.pr_url.as_deref())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn get_repos(&self, session_id: &str) -> Result<Vec<StoredRepoLink>> {
        let pairs = self
            .get_repos_for_sessions(std::slice::from_ref(&session_id.to_string()))
            .await?;
        Ok(pairs.into_iter().next().map(|(_, v)| v).unwrap_or_default())
    }

    async fn get_repos_for_sessions(
        &self,
        session_ids: &[String],
    ) -> Result<Vec<(String, Vec<StoredRepoLink>)>> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; session_ids.len()].join(", ");

        // Canonical rows.
        let repo_sql = format!(
            "SELECT session_id, repo, branch, remote_url, pr_url, is_container \
             FROM session_repos WHERE session_id IN ({placeholders}) \
             ORDER BY session_id, repo, branch"
        );
        let mut q = sqlx::query(&repo_sql);
        for id in session_ids {
            q = q.bind(id);
        }
        let repo_rows = q.fetch_all(&self.pool).await?;

        // Per-user paths.
        let path_sql = format!(
            "SELECT session_id, repo, branch, user_email, local_path \
             FROM session_repo_paths WHERE session_id IN ({placeholders}) \
             ORDER BY user_email"
        );
        let mut pq = sqlx::query(&path_sql);
        for id in session_ids {
            pq = pq.bind(id);
        }
        let path_rows = pq.fetch_all(&self.pool).await?;

        // Index paths by (session_id, repo, branch).
        let mut paths: std::collections::HashMap<(String, String, String), Vec<RepoPath>> =
            std::collections::HashMap::new();
        for r in &path_rows {
            let key = (
                r.get::<String, _>("session_id"),
                r.get::<String, _>("repo"),
                r.get::<String, _>("branch"),
            );
            paths.entry(key).or_default().push(RepoPath {
                user_email: r.get("user_email"),
                local_path: r.get("local_path"),
            });
        }

        // Assemble, preserving session order from `session_ids`.
        let mut by_session: std::collections::HashMap<String, Vec<StoredRepoLink>> =
            std::collections::HashMap::new();
        for r in &repo_rows {
            let session_id: String = r.get("session_id");
            let repo: String = r.get("repo");
            let branch: String = r.get("branch");
            let key = (session_id.clone(), repo.clone(), branch.clone());
            by_session
                .entry(session_id)
                .or_default()
                .push(StoredRepoLink {
                    repo,
                    remote_url: r.get("remote_url"),
                    branch,
                    pr_url: r.get("pr_url"),
                    paths: paths.remove(&key).unwrap_or_default(),
                    is_container: r.get("is_container"),
                });
        }

        let mut out = Vec::new();
        for id in session_ids {
            if let Some(links) = by_session.remove(id) {
                out.push((id.clone(), links));
            }
        }
        Ok(out)
    }

    async fn get_summary(&self, session_id: &str) -> Result<Option<SessionSummary>> {
        let row = sqlx::query(
            r#"
            SELECT intent, invariants, summary, model, generated_by, generated_at
            FROM session_summaries WHERE session_id = ?
            "#,
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let invariants_json: String = row.get("invariants");
        let invariants: Vec<String> = serde_json::from_str(&invariants_json)?;

        Ok(Some(SessionSummary {
            intent: row.get("intent"),
            invariants,
            summary: row.get("summary"),
            model: row.get("model"),
            generated_by: row.get("generated_by"),
            generated_at: str_to_ts(row.get("generated_at")).unwrap_or_else(Utc::now),
        }))
    }

    async fn upsert_summary(
        &self,
        source: SourceContext,
        session_id: &str,
        summary: &SessionSummary,
    ) -> Result<()> {
        let invariants_json = serde_json::to_string(&summary.invariants)?;
        let mut tx = self.pool.begin().await?;
        super::publication::fence_source!(tx, false, session_id, source);
        sqlx::query(
            r#"
            INSERT INTO session_summaries
                (session_id, intent, invariants, summary, model, generated_by, generated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(session_id) DO UPDATE SET
                intent       = excluded.intent,
                invariants   = excluded.invariants,
                summary      = excluded.summary,
                model        = excluded.model,
                generated_by = excluded.generated_by,
                generated_at = excluded.generated_at
            "#,
        )
        .bind(session_id)
        .bind(&summary.intent)
        .bind(&invariants_json)
        .bind(&summary.summary)
        .bind(&summary.model)
        .bind(summary.generated_by.as_deref())
        .bind(summary.generated_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn create_project(
        &self,
        owner_email: Option<&str>,
        name: &str,
        description: Option<&str>,
    ) -> Result<Project> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO projects (id, name, description, owner_email, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(name)
        .bind(description)
        .bind(owner_email)
        .bind(&now_str)
        .bind(&now_str)
        .execute(&self.pool)
        .await?;

        Ok(Project {
            id,
            name: name.to_string(),
            description: description.map(str::to_string),
            owner_email: owner_email.map(str::to_string),
            session_count: 0,
            created_at: now,
            updated_at: now,
        })
    }

    async fn list_projects_detailed(&self, viewer: Option<&str>) -> Result<Vec<Project>> {
        // Count only sessions this viewer may see (auth off ⇒ all members).
        // Same predicate as `list_sessions`, scoped to the `s` alias.
        let mut binds: Vec<String> = Vec::new();
        let count_filter = match viewer {
            Some(email) => {
                binds.push(email.to_string());
                binds.push(email.to_string());
                binds.push(email.to_string());
                " AND (s.visibility = 'organization' OR s.owner_email = ? \
                 OR EXISTS (SELECT 1 FROM session_shares ss \
                    WHERE ss.session_id = s.id AND ss.email = ?) OR EXISTS (SELECT 1 FROM live_session_shares lss WHERE lss.live_id = s.id AND lss.communication_only = 0 AND lss.email = ?))"
                    .to_string()
            }
            None => String::new(),
        };
        let count_sql = format!(
            r#"
            SELECT p.id, p.name, p.description, p.owner_email, p.created_at, p.updated_at,
                   (SELECT COUNT(*) FROM sessions s
                       WHERE s.project_id = p.id{count_filter}) AS session_count
            FROM projects p
            ORDER BY p.created_at DESC, p.id DESC
            "#
        );
        let mut query = sqlx::query(&count_sql);
        for bind in binds {
            query = query.bind(bind);
        }
        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows.iter().map(project_from_row).collect())
    }

    async fn get_project(&self, viewer: Option<&str>, id: &str) -> Result<Option<ProjectDetail>> {
        // session_count: visibility-filtered for a known viewer (same predicate
        // as `list_sessions`, scoped to the `s` alias), all members otherwise.
        let mut count_binds: Vec<String> = Vec::new();
        let count_filter = match viewer {
            Some(email) => {
                count_binds.push(email.to_string());
                count_binds.push(email.to_string());
                count_binds.push(email.to_string());
                " AND (s.visibility = 'organization' OR s.owner_email = ? \
                 OR EXISTS (SELECT 1 FROM session_shares ss \
                    WHERE ss.session_id = s.id AND ss.email = ?) OR EXISTS (SELECT 1 FROM live_session_shares lss WHERE lss.live_id = s.id AND lss.communication_only = 0 AND lss.email = ?))"
                    .to_string()
            }
            None => String::new(),
        };
        let count_sql = format!(
            r#"
            SELECT p.id, p.name, p.description, p.owner_email, p.created_at, p.updated_at,
                   (SELECT COUNT(*) FROM sessions s
                       WHERE s.project_id = p.id{count_filter}) AS session_count
            FROM projects p WHERE p.id = ?
            "#
        );
        let mut count_query = sqlx::query(&count_sql);
        for bind in count_binds {
            count_query = count_query.bind(bind);
        }
        let row = count_query.bind(id).fetch_optional(&self.pool).await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let project = project_from_row(&row);

        // Member summaries: same visibility filter as `list_sessions` (the
        // session list must not leak restricted sessions through the project
        // detail view either).
        let mut member_binds = vec![id.to_string()];
        let member_sql = match viewer {
            Some(email) => {
                member_binds.push(email.to_string());
                member_binds.push(email.to_string());
                member_binds.push(email.to_string());
                "SELECT id, COALESCE(title_override, title) AS title, project_path, \
                   agent, git_branch, message_count, started_at, ended_at, \
                   uploaded_by, uploaded_at, source_host, owner_email, visibility \
                 FROM sessions WHERE project_id = ? \
                 AND (visibility = 'organization' OR owner_email = ? \
                 OR EXISTS (SELECT 1 FROM session_shares s \
                    WHERE s.session_id = sessions.id AND s.email = ?) \
                 OR EXISTS (SELECT 1 FROM live_session_shares ls \
                    WHERE ls.live_id = sessions.id AND ls.communication_only = 0 AND ls.email = ?)) \
                 ORDER BY started_at DESC, uploaded_at DESC"
            }
            None => {
                "SELECT id, COALESCE(title_override, title) AS title, project_path, \
                   agent, git_branch, message_count, started_at, ended_at, \
                   uploaded_by, uploaded_at, source_host, owner_email, visibility \
                 FROM sessions WHERE project_id = ? \
                 ORDER BY started_at DESC, uploaded_at DESC"
            }
        };
        let mut member_query = sqlx::query(member_sql);
        for bind in member_binds {
            member_query = member_query.bind(bind);
        }
        let session_rows = member_query.fetch_all(&self.pool).await?;
        let mut sessions: Vec<StoredSummary> = session_rows.iter().map(summary_from_row).collect();

        let ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
        let mut by_session: std::collections::HashMap<String, Vec<StoredRepoLink>> = self
            .get_repos_for_sessions(&ids)
            .await?
            .into_iter()
            .collect();
        for s in &mut sessions {
            if let Some(repos) = by_session.remove(&s.id) {
                s.repos = repos;
            }
        }

        Ok(Some(ProjectDetail { project, sessions }))
    }

    async fn get_project_owner(&self, id: &str) -> Result<Option<Option<String>>> {
        let row = sqlx::query("SELECT owner_email FROM projects WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<Option<String>, _>("owner_email")))
    }

    async fn update_project(&self, id: &str, name: &str, description: Option<&str>) -> Result<()> {
        // `description` is always set to the provided value (passing `None`
        // clears it) — the edit form always sends both fields.
        let res = sqlx::query(
            "UPDATE projects SET name = ?, description = ?, updated_at = ? WHERE id = ?",
        )
        .bind(name)
        .bind(description)
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    async fn delete_project(&self, id: &str) -> Result<()> {
        // Member sessions' project_id is set NULL and project-chats cascade,
        // both via FK.
        sqlx::query("DELETE FROM projects WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_session_project(&self, session_id: &str, project_id: Option<&str>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, session_id);

        let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::NotFound);
        }

        if let Some(pid) = project_id {
            let proj: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM projects WHERE id = ?")
                .bind(pid)
                .fetch_optional(&mut *tx)
                .await?;
            if proj.is_none() {
                return Err(AppError::BadRequest(format!("unknown project {pid:?}")));
            }
        }

        sqlx::query("UPDATE sessions SET project_id = ? WHERE id = ?")
            .bind(project_id)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn create_chat(
        &self,
        parent: &ChatParent,
        source: SourceContext,
        owner_email: Option<&str>,
        title: Option<&str>,
    ) -> Result<Chat> {
        let (session_id, project_id): (Option<&str>, Option<&str>) = match parent {
            ChatParent::Session(id) => (Some(id), None),
            ChatParent::Project(id) => (None, Some(id)),
        };

        let mut tx = self.pool.begin().await?;
        match session_id {
            Some(id) => {
                super::publication::fence_source!(tx, false, id, source);
            }
            None => source.without_publication()?,
        }

        // Validate the parent exists (the CHECK only guards exactly-one-ness).
        let parent_exists: Option<(i64,)> = match parent {
            ChatParent::Session(id) => {
                sqlx::query_as("SELECT 1 FROM sessions WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?
            }
            ChatParent::Project(id) => {
                sqlx::query_as("SELECT 1 FROM projects WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?
            }
        };
        if parent_exists.is_none() {
            return Err(AppError::BadRequest("unknown chat parent".to_string()));
        }

        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO chats
                (id, session_id, project_id, owner_email, title, shared,
                 created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, 0, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(session_id)
        .bind(project_id)
        .bind(owner_email)
        .bind(title)
        .bind(&now_str)
        .bind(&now_str)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(Chat {
            id,
            session_id: session_id.map(str::to_string),
            project_id: project_id.map(str::to_string),
            owner_email: owner_email.map(str::to_string),
            title: title.map(str::to_string),
            shared: false,
            created_at: now,
            updated_at: now,
        })
    }

    async fn list_chats(&self, parent: &ChatParent, viewer: Option<&str>) -> Result<Vec<Chat>> {
        let (column, parent_id) = match parent {
            ChatParent::Session(id) => ("session_id", id),
            ChatParent::Project(id) => ("project_id", id),
        };

        let mut sql = format!(
            "SELECT id, session_id, project_id, owner_email, title, shared, \
             created_at, updated_at FROM chats WHERE {column} = ?"
        );
        if viewer.is_some() {
            sql.push_str(" AND (owner_email = ? OR shared = 1)");
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC");

        let mut query = sqlx::query(&sql).bind(parent_id);
        if let Some(email) = viewer {
            query = query.bind(email);
        }
        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows.iter().map(chat_from_row).collect())
    }

    async fn get_chat(&self, id: &str) -> Result<Option<Chat>> {
        let row = sqlx::query(
            "SELECT id, session_id, project_id, owner_email, title, shared, \
             created_at, updated_at FROM chats WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(chat_from_row))
    }

    async fn get_chat_with_messages(&self, id: &str) -> Result<Option<ChatWithMessages>> {
        let Some(chat) = self.get_chat(id).await? else {
            return Ok(None);
        };
        let rows = sqlx::query(
            "SELECT id, chat_id, role, content, created_at \
             FROM chat_messages WHERE chat_id = ? ORDER BY created_at ASC, id ASC",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        let messages = rows.iter().map(chat_message_from_row).collect();
        Ok(Some(ChatWithMessages { chat, messages }))
    }

    async fn add_chat_message(
        &self,
        chat_id: &str,
        source: SourceContext,
        role: &str,
        content: &str,
    ) -> Result<ChatMessage> {
        let mut tx = self.pool.begin().await?;

        let parent: Option<(Option<String>,)> =
            sqlx::query_as("SELECT session_id FROM chats WHERE id = ?")
                .bind(chat_id)
                .fetch_optional(&mut *tx)
                .await?;
        match parent {
            None => return Err(AppError::NotFound),
            Some((Some(session_id),)) => {
                super::publication::fence_source!(tx, false, session_id.as_str(), source);
            }
            Some((None,)) => source.without_publication()?,
        }
        // The chat may have gone with its session before the lock was taken.
        let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM chats WHERE id = ?")
            .bind(chat_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::NotFound);
        }

        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO chat_messages (id, chat_id, role, content, created_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(chat_id)
        .bind(role)
        .bind(content)
        .bind(&now_str)
        .execute(&mut *tx)
        .await?;

        sqlx::query("UPDATE chats SET updated_at = ? WHERE id = ?")
            .bind(&now_str)
            .bind(chat_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok(ChatMessage {
            id,
            chat_id: chat_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            created_at: now,
        })
    }

    async fn update_chat(&self, id: &str, shared: bool, title: Option<&str>) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let res = match title {
            Some(t) => {
                sqlx::query("UPDATE chats SET shared = ?, title = ?, updated_at = ? WHERE id = ?")
                    .bind(shared as i64)
                    .bind(t)
                    .bind(&now)
                    .bind(id)
                    .execute(&self.pool)
                    .await?
            }
            None => {
                sqlx::query("UPDATE chats SET shared = ?, updated_at = ? WHERE id = ?")
                    .bind(shared as i64)
                    .bind(&now)
                    .bind(id)
                    .execute(&self.pool)
                    .await?
            }
        };
        if res.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    async fn delete_chat(&self, id: &str) -> Result<()> {
        // Messages cascade via FK.
        sqlx::query("DELETE FROM chats WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn create_live_session(&self, ls: &NewLiveSession) -> Result<LiveSession> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let title = scrub_opt(ls.title.as_deref());
        let project_path = scrub_opt(ls.project_path.as_deref());
        let cwd = scrub_opt(ls.cwd.as_deref());
        let git_branch = scrub_opt(ls.git_branch.as_deref());
        let remote_url = scrub_opt(ls.remote_url.as_deref());
        let source_host = scrub_opt(ls.source_host.as_deref());

        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, ls.id.as_str(), ls.session_id.as_str());
        // Idempotent on `id` (== Claude session_id): re-registering the same
        // session resumes it instead of violating the PK. The original
        // `started_at` and `id` are preserved; `status` flips back to 'live',
        // `ended_at` clears, and the metadata is refreshed from the new values.
        sqlx::query(
            r#"
            INSERT INTO live_sessions
                (id, session_id, agent, title, project_path, cwd, git_branch,
                 owner_email, source_host, remote_url, status, mode, started_at, last_event_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'live', ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                status        = 'live',
                agent         = excluded.agent,
                mode          = excluded.mode,
                last_event_at = excluded.last_event_at,
                ended_at      = NULL,
                title         = excluded.title,
                project_path  = excluded.project_path,
                cwd           = excluded.cwd,
                git_branch    = excluded.git_branch,
                source_host   = excluded.source_host,
                remote_url    = excluded.remote_url,
                owner_email   = excluded.owner_email
            "#,
        )
        .bind(&ls.id)
        .bind(&ls.session_id)
        .bind(ls.agent.as_str())
        .bind(title.as_deref())
        .bind(project_path.as_deref())
        .bind(cwd.as_deref())
        .bind(git_branch.as_deref())
        .bind(&ls.owner_email)
        .bind(source_host.as_deref())
        .bind(remote_url.as_deref())
        .bind(&ls.mode)
        .bind(&now_str)
        .bind(&now_str)
        .execute(&mut *tx)
        .await?;

        // Eagerly create the metadata `sessions` row so a live session is a
        // first-class session from the start — rename/project/repos/sharing/PRs
        // (all FK'd to `sessions`) work live, not just after graduation.
        // DO NOTHING on conflict: a reopen of an existing stored session must
        // NOT clobber its stored metadata / title_override / visibility. The
        // content fields (message_count, ended_at, …) stay at these placeholder
        // values until `/complete` updates them via `upsert_session`.
        sqlx::query(
            r#"
            INSERT INTO sessions
                (id, agent, title, project_path, cwd, git_branch, started_at,
                 message_count, skipped_lines, uploaded_by, uploaded_at,
                 source_host, owner_email, visibility)
            VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0, ?, ?, ?, ?, 'restricted')
            ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(&ls.id)
        .bind(ls.agent.as_str())
        // `sessions.title` is NOT NULL; a live session may register without a
        // title yet (the CLI pushes a transcript-derived title later, which
        // also updates this column — see `update_live_session_title`).
        .bind(title.as_deref().unwrap_or("Live session"))
        .bind(project_path.as_deref())
        .bind(cwd.as_deref())
        .bind(git_branch.as_deref())
        .bind(&now_str)
        .bind(ls.owner_email.clone().unwrap_or_default())
        .bind(&now_str)
        .bind(source_host.as_deref())
        .bind(&ls.owner_email)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        self.get_live_session(&ls.id).await?.ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!("live session vanished after insert"))
        })
    }

    async fn append_live_message(
        &self,
        live_id: &str,
        msg: &core::Message,
    ) -> Result<Option<LiveStoredMessage>> {
        let mut tx = self.pool.begin().await?;

        // De-dup on (live_id, message_uuid): a re-push stores nothing.
        let dup: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM live_messages WHERE live_id = ? AND message_uuid = ?")
                .bind(live_id)
                .bind(&msg.uuid)
                .fetch_optional(&mut *tx)
                .await?;
        if dup.is_some() {
            return Ok(None);
        }

        // Next monotonic seq for this live session.
        let next_seq: i64 = sqlx::query_as::<_, (Option<i64>,)>(
            "SELECT MAX(seq) FROM live_messages WHERE live_id = ?",
        )
        .bind(live_id)
        .fetch_one(&mut *tx)
        .await?
        .0
        .map_or(0, |m| m + 1);

        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let payload = serde_json::to_string(msg)?;

        let inserted = sqlx::query(
            r#"
            INSERT INTO live_messages (id, live_id, seq, message_uuid, payload, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT (live_id, message_uuid) DO NOTHING
            "#,
        )
        .bind(&id)
        .bind(live_id)
        .bind(next_seq)
        .bind(&msg.uuid)
        .bind(&payload)
        .bind(&now_str)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        // Lost the de-dup race: a concurrent append already stored this
        // message_uuid that the pre-check SELECT above missed. Treat as a no-op
        // re-push rather than letting the UNIQUE violation surface as a 500.
        if inserted == 0 {
            return Ok(None);
        }

        // Bump last_event_at so the reaper sees activity.
        sqlx::query("UPDATE live_sessions SET last_event_at = ? WHERE id = ?")
            .bind(&now_str)
            .bind(live_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok(Some(LiveStoredMessage {
            seq: next_seq,
            message_uuid: msg.uuid.clone(),
            message: msg.clone(),
            created_at: now,
        }))
    }

    async fn get_live_messages(&self, live_id: &str) -> Result<Vec<LiveStoredMessage>> {
        let rows = sqlx::query(
            "SELECT seq, message_uuid, payload, created_at \
             FROM live_messages WHERE live_id = ? ORDER BY seq ASC",
        )
        .bind(live_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(live_message_from_row).collect()
    }

    async fn get_live_messages_since(
        &self,
        live_id: &str,
        after_seq: i64,
    ) -> Result<Vec<LiveStoredMessage>> {
        let rows = sqlx::query(
            "SELECT seq, message_uuid, payload, created_at \
             FROM live_messages WHERE live_id = ? AND seq > ? ORDER BY seq ASC",
        )
        .bind(live_id)
        .bind(after_seq)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(live_message_from_row).collect()
    }

    async fn list_live_sessions(
        &self,
        status: Option<&str>,
        viewer: Option<&str>,
    ) -> Result<Vec<LiveSession>> {
        let mut sql = String::from(
            "SELECT id, session_id, title, project_path, cwd, git_branch, owner_email, \
             agent, source_host, remote_url, status, mode, started_at, last_event_at, ended_at, \
             graduated_session_id FROM live_sessions",
        );
        // Ordered bind values, matching the order of `?` placeholders appended.
        let mut conditions: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        if let Some(s) = status {
            conditions.push("status = ?".to_string());
            binds.push(s.to_string());
        }
        // Visibility: author OR sharee. Live sessions are always private.
        if let Some(email) = viewer {
            conditions.push(
                "(owner_email = ? OR EXISTS (SELECT 1 FROM live_session_shares ls \
                 WHERE ls.live_id = live_sessions.id AND ls.communication_only = 0 AND ls.email = ?))"
                    .to_string(),
            );
            binds.push(email.to_string());
            binds.push(email.to_string());
        }
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        sql.push_str(" ORDER BY started_at DESC");
        let mut query = sqlx::query(&sql);
        for b in &binds {
            query = query.bind(b);
        }
        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows.iter().map(live_session_from_row).collect())
    }

    async fn set_live_shares(&self, live_id: &str, shares: &[LiveShare]) -> Result<()> {
        // Dedup by email (last write wins) so a duplicated email can't violate
        // the (live_id, email) PK and the final flag reflects the last entry.
        let mut by_email: std::collections::HashMap<String, (bool, bool, bool)> =
            std::collections::HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for s in shares {
            s.validate()?;
            let email = s.email.trim().to_lowercase();
            if email.is_empty() {
                continue;
            }
            if by_email
                .insert(email.clone(), (s.can_read, s.can_write, s.can_communicate))
                .is_none()
            {
                order.push(email);
            } else {
                // Already present: move to last-wins by leaving order; value updated.
            }
        }

        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM live_session_shares WHERE live_id = ?")
            .bind(live_id)
            .execute(&mut *tx)
            .await?;

        for email in &order {
            let (can_read, can_write, can_communicate) = by_email[email];
            sqlx::query(
                "INSERT OR REPLACE INTO live_session_shares \
                 (live_id, email, can_write, communication_only, can_communicate, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(live_id)
            .bind(email)
            .bind(can_write as i64)
            .bind(!can_read as i64)
            .bind(can_communicate as i64)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn get_live_shares(&self, live_id: &str) -> Result<Vec<LiveShare>> {
        let rows = sqlx::query(
            "SELECT email, can_write, communication_only, can_communicate FROM live_session_shares \
             WHERE live_id = ? ORDER BY email ASC",
        )
        .bind(live_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(live_share_from_row).collect())
    }

    async fn get_live_session_access(
        &self,
        id: &str,
    ) -> Result<Option<(Option<String>, Vec<LiveShare>)>> {
        let row = sqlx::query("SELECT owner_email FROM sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let owner: Option<String> = row.get("owner_email");
        let shares = self.get_live_shares(id).await?;
        Ok(Some((owner, shares)))
    }

    async fn get_live_session(&self, live_id: &str) -> Result<Option<LiveSession>> {
        let row = sqlx::query(
            "SELECT id, session_id, title, project_path, cwd, git_branch, owner_email, \
             agent, source_host, remote_url, status, mode, started_at, last_event_at, ended_at, \
             graduated_session_id FROM live_sessions WHERE id = ?",
        )
        .bind(live_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(live_session_from_row))
    }

    async fn get_live_session_owner(&self, live_id: &str) -> Result<Option<Option<String>>> {
        let row = sqlx::query("SELECT owner_email FROM sessions WHERE id = ?")
            .bind(live_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<Option<String>, _>("owner_email")))
    }

    async fn touch_live_session(&self, live_id: &str) -> Result<bool> {
        // Self-healing: a heartbeat for a session the reaper prematurely marked
        // `abandoned` (e.g. a >threshold heartbeat gap during a rolling deploy)
        // resurrects it to `live` so its mirror reappears. The `status IN`
        // predicate scopes the whole update to `live`/`abandoned` rows: a
        // terminal `graduated` row is never touched (no `last_event_at` bump, no
        // resurrection), so a late/stale heartbeat returns `false`.
        let res = sqlx::query(
            "UPDATE live_sessions \
             SET last_event_at = ?, \
                 status = CASE WHEN status = ? THEN ? ELSE status END \
             WHERE id = ? AND status IN (?, ?)",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(LIVE_STATUS_ABANDONED)
        .bind(LIVE_STATUS_LIVE)
        .bind(live_id)
        .bind(LIVE_STATUS_ABANDONED)
        .bind(LIVE_STATUS_LIVE)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn update_live_session_title(&self, live_id: &str, title: &str) -> Result<bool> {
        let title = scrub_str(title);
        let title = title.as_ref();

        let mut tx = self.pool.begin().await?;
        // Only a still-live row accepts a CLI title push. A late push for a
        // graduated/abandoned (or absent) session must NOT mutate the durable
        // `sessions.title` — bail before touching it when nothing matched.
        let res = sqlx::query("UPDATE live_sessions SET title = ? WHERE id = ? AND status = ?")
            .bind(title)
            .bind(live_id)
            .bind(LIVE_STATUS_LIVE)
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        // Keep the eager `sessions` row's title in sync with the CLI-pushed
        // title. A user rename lives in `title_override` and still wins
        // (reads COALESCE(title_override, title)), so this never clobbers it.
        sqlx::query("UPDATE sessions SET title = ? WHERE id = ?")
            .bind(title)
            .bind(live_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn set_live_agent_session_id(
        &self,
        live_id: &str,
        agent_session_id: &str,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        // Only a still-live row accepts the CLI's native-id push. A late push for
        // a graduated/abandoned (or absent) session must NOT mutate the durable
        // `sessions` row — bail before touching it when nothing matched.
        let res = sqlx::query(
            "UPDATE live_sessions SET agent_session_id = ? WHERE id = ? AND status = ?",
        )
        .bind(agent_session_id)
        .bind(live_id)
        .bind(LIVE_STATUS_LIVE)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        // Mirror onto the eager `sessions` row (keyed by the live id) so a later
        // `reopen` — which reads `sessions` — sees the resume target. Graduation
        // (`upsert_session`) preserves this column, so it survives the session
        // ending.
        sqlx::query("UPDATE sessions SET agent_session_id = ? WHERE id = ?")
            .bind(agent_session_id)
            .bind(live_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn complete_live_session(&self, live_id: &str) -> Result<Option<String>> {
        let Some(live) = self.get_live_session(live_id).await? else {
            return Ok(None);
        };

        // Assemble a core::Session from the stored live messages, in seq order.
        let stored = self.get_live_messages(live_id).await?;
        let messages: Vec<core::Message> = stored.into_iter().map(|m| m.message).collect();
        let (started_at, ended_at) = super::message_time_range(&messages);

        let session = core::Session {
            id: live.session_id.clone(),
            agent: live.agent,
            title: live
                .title
                .clone()
                .unwrap_or_else(|| live.session_id.clone()),
            project_path: live.project_path.clone(),
            cwd: live.cwd.clone(),
            git_branch: live.git_branch.clone(),
            claude_version: None,
            started_at,
            ended_at,
            messages,
            skipped_lines: 0,
        };

        // Reuse the existing upsert path so a graduated session is an ordinary
        // stored session (same sharing/summary/repo machinery + detail view).
        let uploaded_by = live.owner_email.as_deref().unwrap_or("live");
        self.upsert_session(
            uploaded_by,
            live.source_host.as_deref(),
            live.owner_email.as_deref(),
            &session,
        )
        .await?;

        sqlx::query(
            "UPDATE live_sessions \
             SET status = ?, graduated_session_id = ?, ended_at = ? WHERE id = ?",
        )
        .bind(LIVE_STATUS_GRADUATED)
        .bind(&session.id)
        .bind(Utc::now().to_rfc3339())
        .bind(live_id)
        .execute(&self.pool)
        .await?;

        Ok(Some(session.id))
    }

    async fn mark_abandoned_live_sessions(&self, older_than: DateTime<Utc>) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE live_sessions SET status = 'abandoned' \
             WHERE status = ? AND last_event_at < ?",
        )
        .bind(LIVE_STATUS_LIVE)
        .bind(older_than.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    async fn session_store(&self) -> Result<SessionBackend> {
        let store = tower_sessions_sqlx_store::SqliteStore::new(self.pool.clone());
        store
            .migrate()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
        Ok(SessionBackend::Sqlite(store))
    }

    async fn link_session_pr(&self, session_id: &str, pr: &SessionPr) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, session_id);
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO session_prs (session_id, url, owner, repo, number, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(session_id, owner, repo, number) DO NOTHING
            "#,
        )
        .bind(session_id)
        .bind(&pr.url)
        .bind(&pr.owner)
        .bind(&pr.repo)
        .bind(pr.number)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn list_session_prs(&self, session_id: &str) -> Result<Vec<SessionPrWithStatus>> {
        let rows = sqlx::query(
            "SELECT url, owner, repo, number, \
                    status_state, status_merged, status_draft, status_title, status_review_decision, status_fetched_at, \
                    detected \
             FROM session_prs \
             WHERE session_id = ? ORDER BY created_at ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| SessionPrWithStatus {
                url: r.get("url"),
                owner: r.get("owner"),
                repo: r.get("repo"),
                number: r.get::<i64, _>("number"),
                status_state: r.get("status_state"),
                // sqlite stores bools as INTEGER 0/1.
                status_merged: r.get::<Option<i64>, _>("status_merged").map(|v| v != 0),
                status_draft: r.get::<Option<i64>, _>("status_draft").map(|v| v != 0),
                status_title: r.get("status_title"),
                status_review_decision: r.get("status_review_decision"),
                status_fetched_at: str_to_ts(r.get("status_fetched_at")),
                detected: r.get::<i64, _>("detected") != 0,
            })
            .collect())
    }

    async fn list_open_pr_watch_targets(&self, owner: Option<&str>) -> Result<Vec<PrWatchTarget>> {
        // Scoped to CURRENTLY LIVE sessions (`live_sessions.status = 'live'`):
        // the proactive timer only polls sessions in progress, so a graduated /
        // abandoned session — including any stale or phantom link it picked up —
        // is never re-fetched forever. Open = not merged AND (state OPEN or
        // never synced/unknown). The owner param is bound twice: `? IS NULL`
        // short-circuits the filter when auth is off (owner = None), else it
        // scopes to that user's own live sessions.
        let rows = sqlx::query(
            "SELECT sp.session_id, sp.owner, sp.repo, sp.number, sp.url \
             FROM session_prs sp \
             JOIN live_sessions ls ON ls.id = sp.session_id AND ls.status = ? \
             WHERE COALESCE(sp.status_merged, 0) = 0 \
               AND (sp.status_state IS NULL OR UPPER(sp.status_state) = 'OPEN') \
               AND (? IS NULL OR ls.owner_email = ?) \
             ORDER BY sp.session_id ASC, sp.number ASC",
        )
        .bind(LIVE_STATUS_LIVE)
        .bind(owner)
        .bind(owner)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| PrWatchTarget {
                session_id: r.get("session_id"),
                owner: r.get("owner"),
                repo: r.get("repo"),
                number: r.get::<i64, _>("number"),
                url: r.get("url"),
            })
            .collect())
    }

    async fn auto_link_session_prs(&self, session_id: &str, prs: &[SessionPr]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, session_id);
        let now = Utc::now().to_rfc3339();
        let mut inserted = 0u64;
        for pr in prs {
            // detected = 1; never clobbers/downgrades an existing row.
            let res = sqlx::query(
                r#"
                INSERT INTO session_prs (session_id, url, owner, repo, number, created_at, detected)
                VALUES (?, ?, ?, ?, ?, ?, 1)
                ON CONFLICT(session_id, owner, repo, number) DO NOTHING
                "#,
            )
            .bind(session_id)
            .bind(&pr.url)
            .bind(&pr.owner)
            .bind(&pr.repo)
            .bind(pr.number)
            .bind(&now)
            .execute(&mut *tx)
            .await;
            // Best-effort: the insert now succeeds even for a session id with no
            // `sessions` row (e.g. a live, ungraduated session) since the FK was
            // dropped in migration 0016. Any residual error (e.g. a transient DB
            // failure) is swallowed so detection never fails the surrounding ingest.
            match res {
                // A conflict no-op reports 0 rows affected; only a genuine insert
                // counts toward the "new link" tally the caller notifies on.
                Ok(r) => inserted += r.rows_affected(),
                Err(e) => {
                    tracing::debug!(session_id, error = %e, "auto_link_session_prs insert skipped")
                }
            }
        }
        tx.commit().await?;
        Ok(inserted)
    }

    async fn pr_scanned_at(&self, session_id: &str) -> Result<Option<DateTime<Utc>>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT pr_scanned_at FROM sessions WHERE id = ?")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(ts,)| str_to_ts(ts)))
    }

    async fn set_pr_scanned_at(&self, session_id: &str, ts: DateTime<Utc>) -> Result<()> {
        sqlx::query("UPDATE sessions SET pr_scanned_at = ? WHERE id = ?")
            .bind(ts.to_rfc3339())
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn unlink_session_pr(
        &self,
        session_id: &str,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> Result<bool> {
        let res = sqlx::query(
            "DELETE FROM session_prs \
             WHERE session_id = ? AND owner = ? AND repo = ? AND number = ?",
        )
        .bind(session_id)
        .bind(owner)
        .bind(repo)
        .bind(number)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn auto_link_session_linear_docs(
        &self,
        session_id: &str,
        docs: &[SessionLinearDoc],
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, false, session_id);
        let now = Utc::now().to_rfc3339();
        let mut inserted = 0u64;
        for doc in docs {
            let res = sqlx::query(
                r#"
                INSERT INTO session_linear_docs
                    (session_id, url, workspace, kind, identifier, created_at)
                VALUES (?, ?, ?, ?, ?, ?)
                ON CONFLICT(session_id, url) DO NOTHING
                "#,
            )
            .bind(session_id)
            .bind(&doc.url)
            .bind(&doc.workspace)
            .bind(&doc.kind)
            .bind(&doc.identifier)
            .bind(&now)
            .execute(&mut *tx)
            .await;
            // Best-effort, mirroring auto_link_session_prs: the insert succeeds
            // even for a live (un-graduated) session id (no FK), and a residual
            // error is swallowed so detection never fails the surrounding ingest.
            match res {
                Ok(r) => inserted += r.rows_affected(),
                Err(e) => {
                    tracing::debug!(session_id, error = %e, "auto_link_session_linear_docs insert skipped")
                }
            }
        }
        tx.commit().await?;
        Ok(inserted)
    }

    async fn list_session_linear_docs(&self, session_id: &str) -> Result<Vec<SessionLinearDoc>> {
        let rows = sqlx::query(
            "SELECT url, workspace, kind, identifier \
             FROM session_linear_docs \
             WHERE session_id = ? ORDER BY created_at ASC, url ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| SessionLinearDoc {
                url: r.get("url"),
                workspace: r.get("workspace"),
                kind: r.get("kind"),
                identifier: r.get("identifier"),
            })
            .collect())
    }

    async fn unlink_session_linear_doc(&self, session_id: &str, url: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM session_linear_docs WHERE session_id = ? AND url = ?")
            .bind(session_id)
            .bind(url)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn linear_scanned_at(&self, session_id: &str) -> Result<Option<DateTime<Utc>>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT linear_scanned_at FROM sessions WHERE id = ?")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(ts,)| str_to_ts(ts)))
    }

    async fn set_linear_scanned_at(&self, session_id: &str, ts: DateTime<Utc>) -> Result<()> {
        sqlx::query("UPDATE sessions SET linear_scanned_at = ? WHERE id = ?")
            .bind(ts.to_rfc3339())
            .bind(session_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn set_session_pr_status(
        &self,
        source: SourceContext,
        session_id: &str,
        owner: &str,
        repo: &str,
        number: i64,
        status: &crate::github::PrStatus,
        fetched_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        super::publication::fence_source!(tx, false, session_id, source);
        let res = sqlx::query(
            "UPDATE session_prs SET \
                status_state = ?, status_merged = ?, status_draft = ?, \
                status_title = ?, status_review_decision = COALESCE(?, status_review_decision), status_fetched_at = ? \
             WHERE session_id = ? AND owner = ? AND repo = ? AND number = ?",
        )
        .bind(&status.state)
        .bind(status.merged as i64)
        .bind(status.draft as i64)
        .bind(&status.title)
        .bind(&status.review_decision)
        .bind(fetched_at.to_rfc3339())
        .bind(session_id)
        .bind(owner)
        .bind(repo)
        .bind(number)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res.rows_affected() > 0)
    }

    async fn upsert_pr_comments(
        &self,
        source: SourceContext,
        session_id: &str,
        owner: &str,
        repo: &str,
        number: i64,
        comments: &[crate::github::PrComment],
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        super::publication::fence_source!(tx, false, session_id, source);
        if matches!(source, SourceContext::Published(_)) {
            let linked: Option<(i64,)> = sqlx::query_as(
                "SELECT 1 FROM session_prs \
                 WHERE session_id = ? AND owner = ? AND repo = ? AND number = ?",
            )
            .bind(session_id)
            .bind(owner)
            .bind(repo)
            .bind(number)
            .fetch_optional(&mut *tx)
            .await?;
            if linked.is_none() {
                return Err(AppError::NotFound);
            }
        }
        let revision: i64 = sqlx::query_scalar(
            "INSERT INTO session_pr_comment_revisions (session_id, revision) VALUES (?, 1) \
             ON CONFLICT(session_id) DO UPDATE SET revision = session_pr_comment_revisions.revision + 1 \
             RETURNING revision",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;
        let stamp = Utc::now();
        let now = stamp.to_rfc3339();
        let mut changed = false;
        for c in comments {
            let updated = sqlx::query(
                r#"
                INSERT INTO session_pr_comments
                    (session_id, owner, repo, number, comment_id,
                     kind, author, body, html_url, created_at,
                     path, line, diff_hunk, resolved, ingested_at, revision)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(session_id, owner, repo, number, comment_id) DO UPDATE SET
                    kind = excluded.kind,
                    author = excluded.author,
                    body = excluded.body,
                    html_url = excluded.html_url,
                    created_at = excluded.created_at,
                    path = excluded.path,
                    line = excluded.line,
                    diff_hunk = excluded.diff_hunk,
                    resolved = excluded.resolved,
                    ingested_at = excluded.ingested_at,
                    revision = excluded.revision
                WHERE kind IS DISTINCT FROM excluded.kind
                   OR author IS DISTINCT FROM excluded.author
                   OR body IS DISTINCT FROM excluded.body
                   OR html_url IS DISTINCT FROM excluded.html_url
                   OR created_at IS DISTINCT FROM excluded.created_at
                   OR path IS DISTINCT FROM excluded.path
                   OR line IS DISTINCT FROM excluded.line
                   OR diff_hunk IS DISTINCT FROM excluded.diff_hunk
                   OR resolved IS DISTINCT FROM excluded.resolved
                RETURNING comment_id
                "#,
            )
            .bind(session_id)
            .bind(owner)
            .bind(repo)
            .bind(number)
            .bind(&c.comment_id)
            .bind(&c.kind)
            .bind(&c.author)
            .bind(&c.body)
            .bind(&c.html_url)
            .bind(&c.created_at)
            .bind(&c.path)
            .bind(c.line)
            .bind(&c.diff_hunk)
            .bind(c.resolved)
            .bind(&now)
            .bind(revision)
            .fetch_optional(&mut *tx)
            .await?;
            if updated.is_some() {
                changed = true;
            }
        }
        tx.commit().await?;
        Ok(changed)
    }

    async fn list_pr_comments(&self, session_id: &str) -> Result<Vec<StoredPrComment>> {
        let rows = sqlx::query(
            "SELECT owner, repo, number, comment_id, kind, author, body, \
                    html_url, created_at, path, line, diff_hunk, resolved, ingested_at, revision \
             FROM session_pr_comments \
             WHERE session_id = ? ORDER BY created_at ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(stored_pr_comment).collect())
    }

    async fn list_pr_comments_since(
        &self,
        session_id: &str,
        since: i64,
    ) -> Result<Vec<StoredPrComment>> {
        let rows = sqlx::query(
            "SELECT owner, repo, number, comment_id, kind, author, body, \
                    html_url, created_at, path, line, diff_hunk, resolved, ingested_at, revision \
             FROM session_pr_comments \
             WHERE session_id = ? AND revision > ? ORDER BY revision ASC",
        )
        .bind(session_id)
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(stored_pr_comment).collect())
    }

    async fn list_pr_comment_previews(&self, session_id: &str) -> Result<Vec<StoredPrComment>> {
        let rows = sqlx::query(
            "WITH ranked AS ( \
                SELECT owner, repo, number, comment_id, \
                       ROW_NUMBER() OVER (PARTITION BY owner, repo, number \
                           ORDER BY created_at DESC, comment_id DESC) AS rank \
                FROM session_pr_comments WHERE session_id = ? \
                    AND (kind <> 'review_comment' OR resolved IS NOT 1) \
                    AND NOT (kind IN ('review', 'issue') \
                        AND lower(trim(author)) IN ('coderabbitai', 'coderabbitai[bot]')) \
             ) SELECT c.owner, c.repo, c.number, c.comment_id, c.kind, c.author, \
                    substr(c.body, 1, 240) AS body, c.html_url, c.created_at, \
                     c.path, c.line, NULL AS diff_hunk, c.resolved, c.ingested_at, c.revision \
             FROM ranked r JOIN session_pr_comments c \
               ON c.session_id = ? AND c.owner = r.owner AND c.repo = r.repo \
               AND c.number = r.number AND c.comment_id = r.comment_id \
             WHERE r.rank <= 4 ORDER BY c.created_at ASC, c.comment_id ASC",
        )
        .bind(session_id)
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(stored_pr_comment).collect())
    }

    async fn get_container_paths(&self, email: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT path FROM user_container_paths WHERE user_email = ? ORDER BY path ASC",
        )
        .bind(email)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("path")).collect())
    }

    async fn set_container_paths(&self, email: &str, paths: &[String]) -> Result<()> {
        let normalized = super::normalize_container_paths(paths);

        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM user_container_paths WHERE user_email = ?")
            .bind(email)
            .execute(&mut *tx)
            .await?;

        for path in &normalized {
            sqlx::query(
                "INSERT OR IGNORE INTO user_container_paths (user_email, path) VALUES (?, ?)",
            )
            .bind(email)
            .bind(path)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn get_default_agent(&self, email: &str) -> Result<core::AgentKind> {
        let row = sqlx::query("SELECT default_agent FROM user_settings WHERE user_email = ?")
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;
        let agent = row
            .as_ref()
            .and_then(|r| r.try_get::<String, _>("default_agent").ok());
        Ok(super::parse_agent(agent.as_deref()))
    }

    async fn set_default_agent(&self, email: &str, agent: core::AgentKind) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_settings (user_email, default_agent) VALUES (?, ?) \
             ON CONFLICT(user_email) DO UPDATE SET default_agent = excluded.default_agent",
        )
        .bind(email)
        .bind(agent.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_inline_code_colors(&self, email: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT inline_code_colors FROM user_settings WHERE user_email = ?")
            .bind(email)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| {
            r.try_get::<Option<String>, _>("inline_code_colors")
                .ok()
                .flatten()
        }))
    }

    async fn get_codex_launch_settings(&self, email: &str) -> Result<super::CodexLaunchSettings> {
        let row = sqlx::query(
            "SELECT codex_sandbox_mode, codex_approval_policy \
             FROM user_settings WHERE user_email = ?",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .map(|r| super::CodexLaunchSettings {
                sandbox_mode: r
                    .try_get::<Option<String>, _>("codex_sandbox_mode")
                    .ok()
                    .flatten(),
                approval_policy: r
                    .try_get::<Option<String>, _>("codex_approval_policy")
                    .ok()
                    .flatten(),
            })
            .unwrap_or_default())
    }

    async fn get_claude_skip_permissions(&self, email: &str) -> Result<bool> {
        let value = sqlx::query_scalar::<_, bool>(
            "SELECT claude_skip_permissions FROM user_settings WHERE user_email = $1",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        Ok(value.unwrap_or(false))
    }

    async fn set_claude_skip_permissions(&self, email: &str, enabled: bool) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_settings (user_email, claude_skip_permissions) VALUES ($1, $2) \
             ON CONFLICT (user_email) DO UPDATE SET claude_skip_permissions = excluded.claude_skip_permissions",
        )
        .bind(email)
        .bind(enabled)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_opencode_launch_settings(
        &self,
        email: &str,
    ) -> Result<super::OpenCodeLaunchSettings> {
        let row = sqlx::query(
            "SELECT opencode_auto, adk_reopen, adk_statusline, default_launcher \
             FROM user_settings WHERE user_email = ?",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(super::OpenCodeLaunchSettings::default());
        };
        Ok(super::OpenCodeLaunchSettings {
            default_launcher: row
                .try_get::<String, _>("default_launcher")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_default(),
            auto: row.try_get::<bool, _>("opencode_auto").unwrap_or(false),
            adk_reopen: row.try_get::<bool, _>("adk_reopen").unwrap_or(false),
            adk_statusline: row.try_get::<bool, _>("adk_statusline").unwrap_or(false),
        })
    }

    async fn set_inline_code_colors(&self, email: &str, json: Option<&str>) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_settings (user_email, inline_code_colors) VALUES (?, ?) \
             ON CONFLICT(user_email) DO UPDATE SET inline_code_colors = excluded.inline_code_colors",
        )
        .bind(email)
        .bind(json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_codex_launch_settings(
        &self,
        email: &str,
        settings: &super::CodexLaunchSettings,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_settings \
                 (user_email, codex_sandbox_mode, codex_approval_policy) \
             VALUES (?, ?, ?) \
             ON CONFLICT(user_email) DO UPDATE SET \
                 codex_sandbox_mode = excluded.codex_sandbox_mode, \
                 codex_approval_policy = excluded.codex_approval_policy",
        )
        .bind(email)
        .bind(settings.sandbox_mode.as_deref())
        .bind(settings.approval_policy.as_deref())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_opencode_launch_settings(
        &self,
        email: &str,
        settings: super::OpenCodeLaunchSettings,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_settings \
                 (user_email, opencode_auto, adk_reopen, adk_statusline, default_launcher) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(user_email) DO UPDATE SET \
                 opencode_auto = excluded.opencode_auto, \
                 adk_reopen = excluded.adk_reopen, \
                 adk_statusline = excluded.adk_statusline, \
                 default_launcher = excluded.default_launcher",
        )
        .bind(email)
        .bind(settings.auto)
        .bind(settings.adk_reopen)
        .bind(settings.adk_statusline)
        .bind(settings.default_launcher.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn user_from_row(row: &SqliteRow) -> User {
    User {
        email: row.get("email"),
        display_name: row.get("display_name"),
        created_at: str_to_ts(row.get("created_at")).unwrap_or_else(Utc::now),
        last_seen_at: str_to_ts(row.get("last_seen_at")).unwrap_or_else(Utc::now),
    }
}

fn project_from_row(row: &SqliteRow) -> Project {
    Project {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        owner_email: row.get("owner_email"),
        session_count: row.get::<i64, _>("session_count") as usize,
        created_at: str_to_ts(row.get("created_at")).unwrap_or_else(Utc::now),
        updated_at: str_to_ts(row.get("updated_at")).unwrap_or_else(Utc::now),
    }
}

fn chat_from_row(row: &SqliteRow) -> Chat {
    Chat {
        id: row.get("id"),
        session_id: row.get("session_id"),
        project_id: row.get("project_id"),
        owner_email: row.get("owner_email"),
        title: row.get("title"),
        shared: row.get::<i64, _>("shared") != 0,
        created_at: str_to_ts(row.get("created_at")).unwrap_or_else(Utc::now),
        updated_at: str_to_ts(row.get("updated_at")).unwrap_or_else(Utc::now),
    }
}

fn chat_message_from_row(row: &SqliteRow) -> ChatMessage {
    ChatMessage {
        id: row.get("id"),
        chat_id: row.get("chat_id"),
        role: row.get("role"),
        content: row.get("content"),
        created_at: str_to_ts(row.get("created_at")).unwrap_or_else(Utc::now),
    }
}

fn summary_from_row(row: &SqliteRow) -> StoredSummary {
    StoredSummary {
        id: row.get("id"),
        agent: super::parse_agent(row.try_get::<String, _>("agent").ok().as_deref()),
        title: row.get("title"),
        project_path: row.get("project_path"),
        git_branch: row.get("git_branch"),
        message_count: row.get::<i64, _>("message_count") as usize,
        started_at: str_to_ts(row.get("started_at")),
        ended_at: str_to_ts(row.get("ended_at")),
        uploaded_by: row.get("uploaded_by"),
        uploaded_at: str_to_ts(row.get("uploaded_at")).unwrap_or_else(Utc::now),
        source_host: row.get("source_host"),
        owner_email: row.get("owner_email"),
        visibility: row.get("visibility"),
        // `is_live` is an EXISTS (0/1) only present on the list query; tolerate
        // its absence on other SELECTs that feed this mapper.
        is_live: row
            .try_get::<i64, _>("is_live")
            .map(|v| v != 0)
            .unwrap_or(false),
        repos: Vec::new(),
    }
}

fn live_session_from_row(row: &SqliteRow) -> LiveSession {
    LiveSession {
        id: row.get("id"),
        session_id: row.get("session_id"),
        agent: super::parse_agent(row.try_get::<String, _>("agent").ok().as_deref()),
        title: row.get("title"),
        project_path: row.get("project_path"),
        cwd: row.get("cwd"),
        git_branch: row.get("git_branch"),
        owner_email: row.get("owner_email"),
        source_host: row.get("source_host"),
        remote_url: row.get("remote_url"),
        status: row.get("status"),
        mode: row.get("mode"),
        started_at: str_to_ts(row.get("started_at")).unwrap_or_else(Utc::now),
        last_event_at: str_to_ts(row.get("last_event_at")).unwrap_or_else(Utc::now),
        ended_at: str_to_ts(row.get("ended_at")),
        graduated_session_id: row.get("graduated_session_id"),
    }
}

fn live_share_from_row(row: &SqliteRow) -> LiveShare {
    LiveShare {
        email: row.get("email"),
        can_write: row.get::<i64, _>("can_write") != 0,
        can_read: row.get::<i64, _>("communication_only") == 0,
        can_communicate: row.get::<i64, _>("can_communicate") != 0,
    }
}

fn live_message_from_row(row: &SqliteRow) -> Result<LiveStoredMessage> {
    let payload: String = row.get("payload");
    let message: core::Message = serde_json::from_str(&payload)?;
    Ok(LiveStoredMessage {
        seq: row.get("seq"),
        message_uuid: row.get("message_uuid"),
        message,
        created_at: str_to_ts(row.get("created_at")).unwrap_or_else(Utc::now),
    })
}

fn message_from_row(row: &SqliteRow) -> Result<core::Message> {
    let role_str: String = row.get("role");
    let kind_str: String = row.get("kind");
    let role: core::Role = serde_json::from_value(serde_json::Value::String(role_str))?;
    let kind: core::MessageKind = serde_json::from_value(serde_json::Value::String(kind_str))?;

    let raw_str: String = row.get("raw");
    let raw: serde_json::Value = serde_json::from_str(&raw_str)?;

    let usage = row
        .get::<Option<String>, _>("usage")
        .map(|s| serde_json::from_str::<core::Usage>(&s))
        .transpose()?;

    let blocks = core::blocks_from_raw_message(&raw);

    Ok(core::Message {
        uuid: row.get("uuid"),
        parent_uuid: row.get("parent_uuid"),
        role,
        kind,
        model: row.get("model"),
        is_sidechain: row.get::<i64, _>("is_sidechain") != 0,
        timestamp: str_to_ts(row.get("timestamp")),
        blocks,
        text: row.get("text"),
        raw,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{VISIBILITY_ORGANIZATION, VISIBILITY_RESTRICTED};

    /// A unique temp-file-backed SqliteRepo (in-memory sqlite is per-connection,
    /// so a temp file is used to survive across the pool's connections).
    struct TempRepo {
        repo: SqliteRepo,
        path: std::path::PathBuf,
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
        }
    }

    async fn temp_repo() -> TempRepo {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // A process-wide counter guarantees uniqueness even when two tests start
        // within the same nanosecond on different threads.
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("conv-store-{nanos}-{n}.sqlite"));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let repo = SqliteRepo::connect(&url).await.expect("connect + migrate");
        TempRepo { repo, path }
    }

    fn sample_session(id: &str) -> core::Session {
        core::Session {
            id: id.to_string(),
            agent: core::AgentKind::Claude,
            title: format!("title {id}"),
            project_path: Some("/tmp/proj".to_string()),
            cwd: Some("/tmp/proj".to_string()),
            git_branch: Some("main".to_string()),
            claude_version: None,
            started_at: None,
            ended_at: None,
            messages: vec![core::Message {
                uuid: format!("{id}-u1"),
                parent_uuid: None,
                role: core::Role::User,
                kind: core::MessageKind::Text,
                model: None,
                is_sidechain: false,
                timestamp: None,
                blocks: vec![core::ContentBlock::Text {
                    text: "hello".to_string(),
                }],
                text: "hello".to_string(),
                raw: serde_json::json!({ "content": "hello" }),
                usage: None,
            }],
            skipped_lines: 0,
        }
    }

    #[tokio::test]
    async fn reupload_preserves_sharing() {
        let t = temp_repo().await;
        let repo = &t.repo;
        let s = sample_session("sess-r");

        let out = repo
            .upsert_session("up@x", None, Some("owner@x"), &s)
            .await
            .unwrap();
        assert!(out.created);
        assert_eq!(out.message_count, 1);

        repo.set_sharing("sess-r", VISIBILITY_RESTRICTED, &["a@x".to_string()])
            .await
            .unwrap();

        // Re-upload the same id (different uploader) must not wipe sharing.
        let out = repo
            .upsert_session("other@x", None, Some("someone-else@x"), &s)
            .await
            .unwrap();
        assert!(!out.created);
        assert_eq!(out.message_count, 1);

        let (visibility, emails) = repo.get_sharing("sess-r").await.unwrap().unwrap();
        assert_eq!(visibility, VISIBILITY_RESTRICTED);
        assert_eq!(emails, vec!["a@x".to_string()]);

        // owner_email is preserved from the original create, not the re-upload.
        let stored = repo.get_session(None, "sess-r").await.unwrap().unwrap();
        assert_eq!(stored.owner_email.as_deref(), Some("owner@x"));
        assert_eq!(stored.message_count, 1);
    }

    #[tokio::test]
    async fn title_override_is_transparent_and_survives_reupload() {
        let t = temp_repo().await;
        let repo = &t.repo;
        let s = sample_session("sess-t");

        repo.upsert_session("u@x", None, Some("o@x"), &s)
            .await
            .unwrap();

        // Set the override; reads (get + list) return it transparently as `title`.
        assert!(repo
            .set_title_override("sess-t", Some("Renamed"))
            .await
            .unwrap());
        let stored = repo.get_session(None, "sess-t").await.unwrap().unwrap();
        assert_eq!(stored.title, "Renamed");
        let listed = repo.list_sessions(None, None, None).await.unwrap();
        assert_eq!(listed[0].title, "Renamed");
        let searched = repo
            .list_sessions(None, Some("renamed"), None)
            .await
            .unwrap();
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].id, "sess-t");

        // Re-upload (which re-sets `title` to the parsed value) must not wipe it.
        repo.upsert_session("u2@x", None, Some("o@x"), &s)
            .await
            .unwrap();
        let stored = repo.get_session(None, "sess-t").await.unwrap().unwrap();
        assert_eq!(stored.title, "Renamed");

        // Clearing reverts to the parsed transcript title.
        assert!(repo.set_title_override("sess-t", None).await.unwrap());
        let stored = repo.get_session(None, "sess-t").await.unwrap().unwrap();
        assert_eq!(stored.title, "title sess-t");

        // Unknown id → false, no write.
        assert!(!repo.set_title_override("nope", Some("x")).await.unwrap());
    }

    #[tokio::test]
    async fn enforcement_filters_by_viewer() {
        let t = temp_repo().await;
        let repo = &t.repo;

        // explicitly org-wide visible
        repo.upsert_session("u@x", None, None, &sample_session("org"))
            .await
            .unwrap();
        repo.set_sharing("org", VISIBILITY_ORGANIZATION, &[])
            .await
            .unwrap();
        // restricted, owned by alice, no shares.
        repo.upsert_session("u@x", None, Some("alice@x"), &sample_session("alice-priv"))
            .await
            .unwrap();
        repo.set_sharing("alice-priv", VISIBILITY_RESTRICTED, &[])
            .await
            .unwrap();
        // restricted, owned by bob, shared with alice.
        repo.upsert_session("u@x", None, Some("bob@x"), &sample_session("bob-shared"))
            .await
            .unwrap();
        repo.set_sharing(
            "bob-shared",
            VISIBILITY_RESTRICTED,
            &["alice@x".to_string()],
        )
        .await
        .unwrap();

        // alice sees org + her own + the one shared with her = 3.
        let alice = repo
            .list_sessions(Some("alice@x"), None, None)
            .await
            .unwrap();
        assert_eq!(alice.len(), 3);

        // carol sees only the org one = 1.
        let carol = repo
            .list_sessions(Some("carol@x"), None, None)
            .await
            .unwrap();
        assert_eq!(carol.len(), 1);
        assert_eq!(carol[0].id, "org");

        // No viewer => no filtering => all 3.
        let all = repo.list_sessions(None, None, None).await.unwrap();
        assert_eq!(all.len(), 3);

        // get_session honors the same rules.
        assert!(repo
            .get_session(Some("carol@x"), "org")
            .await
            .unwrap()
            .is_some());
        assert!(repo
            .get_session(Some("carol@x"), "alice-priv")
            .await
            .unwrap()
            .is_none());
        assert!(repo
            .get_session(Some("alice@x"), "bob-shared")
            .await
            .unwrap()
            .is_some());
        // No viewer can fetch a restricted session.
        assert!(repo
            .get_session(None, "alice-priv")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn search_matches_linked_pr() {
        let t = temp_repo().await;
        let repo = &t.repo;

        // A session with a linked PR but no PR text anywhere in its title/messages.
        repo.upsert_session("u@x", None, None, &sample_session("with-pr"))
            .await
            .unwrap();
        repo.link_session_pr(
            "with-pr",
            &SessionPr {
                url: "https://github.com/Example-Org/product-demo/pull/409".to_string(),
                owner: "Example-Org".to_string(),
                repo: "product-demo".to_string(),
                number: 409,
            },
        )
        .await
        .unwrap();
        // An unrelated session with no PR.
        repo.upsert_session("u@x", None, None, &sample_session("no-pr"))
            .await
            .unwrap();

        // Pasting the full PR URL finds the linked session.
        let by_url = repo
            .list_sessions(
                None,
                Some("https://github.com/Example-Org/product-demo/pull/409"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(by_url.len(), 1);
        assert_eq!(by_url[0].id, "with-pr");

        // The owner/repo#N shorthand works too, case-insensitively.
        let by_shorthand = repo
            .list_sessions(None, Some("example-org/product-demo#409"), None)
            .await
            .unwrap();
        assert_eq!(by_shorthand.len(), 1);
        assert_eq!(by_shorthand[0].id, "with-pr");

        // A different PR number does not match.
        let miss = repo
            .list_sessions(
                None,
                Some("https://github.com/Example-Org/product-demo/pull/410"),
                None,
            )
            .await
            .unwrap();
        assert!(miss.is_empty());
    }

    #[tokio::test]
    async fn linear_docs_link_list_unlink_and_scan() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, None, &sample_session("ld"))
            .await
            .unwrap();

        let doc = SessionLinearDoc {
            url: "https://linear.app/example/document/spec-1".to_string(),
            workspace: "example".to_string(),
            kind: "document".to_string(),
            identifier: "spec-1".to_string(),
        };
        let issue = SessionLinearDoc {
            url: "https://linear.app/example/issue/ENG-7".to_string(),
            workspace: "example".to_string(),
            kind: "issue".to_string(),
            identifier: "ENG-7".to_string(),
        };

        // First insert links both; re-inserting is an idempotent no-op.
        let inserted = repo
            .auto_link_session_linear_docs("ld", &[doc.clone(), issue.clone()])
            .await
            .unwrap();
        assert_eq!(inserted, 2);
        let again = repo
            .auto_link_session_linear_docs("ld", &[doc.clone(), issue.clone()])
            .await
            .unwrap();
        assert_eq!(again, 0, "re-link must not insert");

        // List is ordered by created_at (insertion order here).
        let listed = repo.list_session_linear_docs("ld").await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0], doc);
        assert_eq!(listed[1], issue);

        // Unlink one by canonical URL.
        assert!(repo
            .unlink_session_linear_doc("ld", &doc.url)
            .await
            .unwrap());
        assert!(
            !repo
                .unlink_session_linear_doc("ld", &doc.url)
                .await
                .unwrap(),
            "second unlink finds nothing"
        );
        let after = repo.list_session_linear_docs("ld").await.unwrap();
        assert_eq!(after, vec![issue]);

        // Scan stamp: None until set, then round-trips.
        assert!(repo.linear_scanned_at("ld").await.unwrap().is_none());
        let now = Utc::now();
        repo.set_linear_scanned_at("ld", now).await.unwrap();
        let got = repo.linear_scanned_at("ld").await.unwrap();
        assert!(got.is_some());
        assert!((got.unwrap() - now).num_seconds().abs() < 2);

        // delete_session clears linked Linear refs (no cascade FK).
        repo.delete_session("ld").await.unwrap();
        assert!(repo
            .list_session_linear_docs("ld")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn set_sharing_organization_clears_shares() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, Some("o@x"), &sample_session("s"))
            .await
            .unwrap();
        repo.set_sharing(
            "s",
            VISIBILITY_RESTRICTED,
            &["a@x".to_string(), "b@x".to_string()],
        )
        .await
        .unwrap();
        repo.set_sharing("s", VISIBILITY_ORGANIZATION, &["a@x".to_string()])
            .await
            .unwrap();
        let (visibility, emails) = repo.get_sharing("s").await.unwrap().unwrap();
        assert_eq!(visibility, VISIBILITY_ORGANIZATION);
        assert!(emails.is_empty());
    }

    #[tokio::test]
    async fn set_sharing_unknown_session_is_not_found() {
        let t = temp_repo().await;
        let err = t
            .repo
            .set_sharing("missing", VISIBILITY_RESTRICTED, &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound));
        assert!(t.repo.get_sharing("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn container_paths_roundtrip_and_normalization() {
        let t = temp_repo().await;
        let repo = &t.repo;
        // The FK references users(email), so create the user first.
        repo.upsert_user("alice@x", None).await.unwrap();

        // Empty when none set.
        assert!(repo
            .get_container_paths("alice@x")
            .await
            .unwrap()
            .is_empty());

        // Set with a trailing slash, a blank, and a duplicate-after-normalize.
        repo.set_container_paths(
            "alice@x",
            &[
                "/Users/alice/example/".to_string(),
                "/Users/alice/example".to_string(), // dup after trailing-slash strip
                "   ".to_string(),                  // blank
                "  /Users/alice/other  ".to_string(),
            ],
        )
        .await
        .unwrap();

        // Stored normalized, deduped, sorted ascending.
        let stored = repo.get_container_paths("alice@x").await.unwrap();
        assert_eq!(
            stored,
            vec![
                "/Users/alice/example".to_string(),
                "/Users/alice/other".to_string()
            ]
        );

        // PUT replaces the full set (delete-all-then-insert).
        repo.set_container_paths("alice@x", &["/new/root".to_string()])
            .await
            .unwrap();
        assert_eq!(
            repo.get_container_paths("alice@x").await.unwrap(),
            vec!["/new/root".to_string()]
        );

        // An empty set clears everything.
        repo.set_container_paths("alice@x", &[]).await.unwrap();
        assert!(repo
            .get_container_paths("alice@x")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn default_agent_roundtrip_supports_all_coding_agents() {
        let t = temp_repo().await;
        let repo = &t.repo;

        assert_eq!(
            repo.get_default_agent("alice@x").await.unwrap(),
            core::AgentKind::Claude
        );
        repo.upsert_user("alice@x", None).await.unwrap();
        repo.set_default_agent("alice@x", core::AgentKind::Codex)
            .await
            .unwrap();
        assert_eq!(
            repo.get_default_agent("alice@x").await.unwrap(),
            core::AgentKind::Codex
        );
        repo.set_default_agent("alice@x", core::AgentKind::OpenCode)
            .await
            .unwrap();
        assert_eq!(
            repo.get_default_agent("alice@x").await.unwrap(),
            core::AgentKind::OpenCode
        );
    }

    #[tokio::test]
    async fn codex_launch_settings_roundtrip_defaults_to_none() {
        let t = temp_repo().await;
        let repo = &t.repo;

        assert_eq!(
            repo.get_codex_launch_settings("alice@x").await.unwrap(),
            crate::store::CodexLaunchSettings::default()
        );

        repo.upsert_user("alice@x", None).await.unwrap();
        repo.set_codex_launch_settings(
            "alice@x",
            &crate::store::CodexLaunchSettings {
                sandbox_mode: Some("workspace-write".to_string()),
                approval_policy: Some("never".to_string()),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            repo.get_codex_launch_settings("alice@x").await.unwrap(),
            crate::store::CodexLaunchSettings {
                sandbox_mode: Some("workspace-write".to_string()),
                approval_policy: Some("never".to_string()),
            }
        );

        repo.set_codex_launch_settings("alice@x", &crate::store::CodexLaunchSettings::default())
            .await
            .unwrap();
        assert_eq!(
            repo.get_codex_launch_settings("alice@x").await.unwrap(),
            crate::store::CodexLaunchSettings::default()
        );
    }

    #[tokio::test]
    async fn opencode_launch_settings_roundtrip_defaults_to_disabled() {
        let t = temp_repo().await;
        let repo = &t.repo;

        assert_eq!(
            repo.get_opencode_launch_settings("alice@x").await.unwrap(),
            crate::store::OpenCodeLaunchSettings::default()
        );

        repo.upsert_user("alice@x", None).await.unwrap();
        repo.set_opencode_launch_settings(
            "alice@x",
            crate::store::OpenCodeLaunchSettings {
                default_launcher: core::AgentLauncher::default(),
                auto: true,
                adk_reopen: true,
                adk_statusline: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            repo.get_opencode_launch_settings("alice@x").await.unwrap(),
            crate::store::OpenCodeLaunchSettings {
                default_launcher: core::AgentLauncher::default(),
                auto: true,
                adk_reopen: true,
                adk_statusline: true,
            }
        );

        // Each flag is independent: the `adk code` launcher and its status strip
        // are separate opt-ins, and neither rides on `--auto`.
        repo.set_opencode_launch_settings(
            "alice@x",
            crate::store::OpenCodeLaunchSettings {
                default_launcher: core::AgentLauncher::default(),
                auto: false,
                adk_reopen: true,
                adk_statusline: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            repo.get_opencode_launch_settings("alice@x").await.unwrap(),
            crate::store::OpenCodeLaunchSettings {
                default_launcher: core::AgentLauncher::default(),
                auto: false,
                adk_reopen: true,
                adk_statusline: false,
            }
        );

        repo.set_opencode_launch_settings(
            "alice@x",
            crate::store::OpenCodeLaunchSettings::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            repo.get_opencode_launch_settings("alice@x").await.unwrap(),
            crate::store::OpenCodeLaunchSettings::default()
        );
    }

    #[tokio::test]
    async fn non_claude_session_agents_round_trip_through_get_and_list() {
        let t = temp_repo().await;
        let repo = &t.repo;

        let mut s = sample_session("codex-sess");
        s.agent = core::AgentKind::Codex;
        repo.upsert_session("u@x", None, Some("o@x"), &s)
            .await
            .unwrap();

        let stored = repo.get_session(None, "codex-sess").await.unwrap().unwrap();
        assert_eq!(stored.agent, core::AgentKind::Codex);

        let listed = repo.list_sessions(None, None, None).await.unwrap();
        let summary = listed.iter().find(|s| s.id == "codex-sess").unwrap();
        assert_eq!(summary.agent, core::AgentKind::Codex);

        let mut opencode = sample_session("opencode-sess");
        opencode.agent = core::AgentKind::OpenCode;
        repo.upsert_session("u@x", None, Some("o@x"), &opencode)
            .await
            .unwrap();
        let stored = repo
            .get_session(None, "opencode-sess")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.agent, core::AgentKind::OpenCode);
        let listed = repo.list_sessions(None, None, None).await.unwrap();
        let summary = listed.iter().find(|s| s.id == "opencode-sess").unwrap();
        assert_eq!(summary.agent, core::AgentKind::OpenCode);

        // A Claude session stored alongside keeps its own agent (no cross-talk).
        let claude = sample_session("claude-sess");
        repo.upsert_session("u@x", None, Some("o@x"), &claude)
            .await
            .unwrap();
        let stored = repo
            .get_session(None, "claude-sess")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.agent, core::AgentKind::Claude);
    }

    #[tokio::test]
    async fn non_claude_live_session_agents_round_trip() {
        let t = temp_repo().await;
        let repo = &t.repo;

        let mut nl = new_live("codex-live");
        nl.agent = core::AgentKind::Codex;
        repo.create_live_session(&nl).await.unwrap();

        let live = repo.get_live_session("codex-live").await.unwrap().unwrap();
        assert_eq!(live.agent, core::AgentKind::Codex);

        let listed = repo.list_live_sessions(Some("live"), None).await.unwrap();
        let found = listed.iter().find(|l| l.id == "codex-live").unwrap();
        assert_eq!(found.agent, core::AgentKind::Codex);

        let mut nl = new_live("opencode-live");
        nl.agent = core::AgentKind::OpenCode;
        repo.create_live_session(&nl).await.unwrap();
        let live = repo
            .get_live_session("opencode-live")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.agent, core::AgentKind::OpenCode);
    }

    #[tokio::test]
    async fn merge_repos_roundtrips_is_container_and_is_sticky() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, None, &sample_session("sc"))
            .await
            .unwrap();

        // First merge: a container repo + a normal sub-repo.
        repo.merge_repos(
            "sc",
            Some("u@x"),
            &[
                core::RepoLink {
                    repo: "monorepo".into(),
                    remote_url: Some("git@github.com:org/monorepo.git".into()),
                    branch: "main".into(),
                    pr_url: None,
                    local_path: Some("/w/monorepo".into()),
                    is_container: true,
                },
                core::RepoLink {
                    repo: "service".into(),
                    remote_url: Some("git@github.com:org/service.git".into()),
                    branch: "main".into(),
                    pr_url: None,
                    local_path: Some("/w/monorepo/service".into()),
                    is_container: false,
                },
            ],
        )
        .await
        .unwrap();

        let links = repo.get_repos("sc").await.unwrap();
        let mono = links.iter().find(|l| l.repo == "monorepo").unwrap();
        let svc = links.iter().find(|l| l.repo == "service").unwrap();
        assert!(mono.is_container, "container flag must round-trip");
        assert!(!svc.is_container, "sub-repo is not a container");

        // Sticky-true: a later flag-less re-push (older CLI default false) must
        // NOT clear the container flag.
        repo.merge_repos(
            "sc",
            Some("u@x"),
            &[core::RepoLink {
                repo: "monorepo".into(),
                remote_url: Some("git@github.com:org/monorepo.git".into()),
                branch: "main".into(),
                pr_url: None,
                local_path: Some("/w/monorepo".into()),
                is_container: false,
            }],
        )
        .await
        .unwrap();
        let links = repo.get_repos("sc").await.unwrap();
        let mono = links.iter().find(|l| l.repo == "monorepo").unwrap();
        assert!(
            mono.is_container,
            "container flag must stay set after a flag-less re-push"
        );
    }

    #[tokio::test]
    async fn summary_upsert_get_and_cascade() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, Some("o@x"), &sample_session("sess-sum"))
            .await
            .unwrap();

        let summary = SessionSummary {
            intent: "ship the summary feature".to_string(),
            invariants: vec![
                "must not break existing GET payload".to_string(),
                "dual-backend".to_string(),
            ],
            summary: "added a session summary table + endpoint".to_string(),
            model: "test-model".to_string(),
            generated_by: Some("o@x".to_string()),
            generated_at: Utc::now(),
        };
        repo.upsert_summary(SourceContext::Unmanaged, "sess-sum", &summary)
            .await
            .unwrap();

        // Read back via get_summary.
        let got = repo.get_summary("sess-sum").await.unwrap().unwrap();
        assert_eq!(got.intent, summary.intent);
        assert_eq!(got.invariants, summary.invariants);
        assert_eq!(got.summary, summary.summary);
        assert_eq!(got.model, summary.model);
        assert_eq!(got.generated_by, summary.generated_by);

        // get_session surfaces it in `.summary`.
        let stored = repo.get_session(None, "sess-sum").await.unwrap().unwrap();
        let session_summary = stored.summary.expect("summary present on session");
        assert_eq!(session_summary.invariants, summary.invariants);
        assert_eq!(session_summary.intent, summary.intent);

        // Upsert replaces.
        let replaced = SessionSummary {
            intent: "revised intent".to_string(),
            invariants: vec![],
            ..summary.clone()
        };
        repo.upsert_summary(SourceContext::Unmanaged, "sess-sum", &replaced)
            .await
            .unwrap();
        let got = repo.get_summary("sess-sum").await.unwrap().unwrap();
        assert_eq!(got.intent, "revised intent");
        assert!(got.invariants.is_empty());

        // Deleting the session cascades the summary away.
        repo.delete_session("sess-sum").await.unwrap();
        assert!(repo.get_summary("sess-sum").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn users_upsert_and_search() {
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.upsert_user("alice@example.com", Some("Alice Smith"))
            .await
            .unwrap();
        repo.upsert_user("bob@example.com", Some("Bob Jones"))
            .await
            .unwrap();

        // by email substring
        let r = repo.search_users(Some("alice"), 50).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].email, "alice@example.com");

        // by display_name substring (case-insensitive)
        let r = repo.search_users(Some("jones"), 50).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].email, "bob@example.com");

        // None q returns up to limit, ordered by email
        let r = repo.search_users(None, 1).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].email, "alice@example.com");

        // upsert updates display_name + last_seen_at
        let updated = repo
            .upsert_user("alice@example.com", Some("Alice S."))
            .await
            .unwrap();
        assert_eq!(updated.display_name.as_deref(), Some("Alice S."));
    }

    #[tokio::test]
    async fn password_set_get_and_absent() {
        let t = temp_repo().await;
        let repo = &t.repo;

        // No row yet.
        assert!(repo
            .get_user_with_password("nobody@example.com")
            .await
            .unwrap()
            .is_none());

        // A fresh user starts with no password.
        repo.upsert_user("carol@example.com", Some("Carol"))
            .await
            .unwrap();
        let (user, hash) = repo
            .get_user_with_password("carol@example.com")
            .await
            .unwrap()
            .expect("user exists");
        assert_eq!(user.email, "carol@example.com");
        assert!(hash.is_none(), "no password set yet");

        // Set a hash and read it back.
        repo.set_password("carol@example.com", "stored-hash")
            .await
            .unwrap();
        let (_, hash) = repo
            .get_user_with_password("carol@example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hash.as_deref(), Some("stored-hash"));

        // Setting a password for a non-existent user is NotFound.
        let err = repo
            .set_password("ghost@example.com", "x")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound));
    }

    #[tokio::test]
    async fn project_create_assign_and_counts() {
        let t = temp_repo().await;
        let repo = &t.repo;

        let proj = repo
            .create_project(Some("o@x"), "My Project", None)
            .await
            .unwrap();
        assert_eq!(proj.name, "My Project");
        assert_eq!(proj.session_count, 0);

        // Newly listed with count 0.
        let listed = repo.list_projects_detailed(None).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, proj.id);
        assert_eq!(listed[0].session_count, 0);

        // Assign two sessions => count 2.
        repo.upsert_session("u@x", None, None, &sample_session("s1"))
            .await
            .unwrap();
        repo.upsert_session("u@x", None, None, &sample_session("s2"))
            .await
            .unwrap();
        repo.set_session_project("s1", Some(&proj.id))
            .await
            .unwrap();
        repo.set_session_project("s2", Some(&proj.id))
            .await
            .unwrap();
        let listed = repo.list_projects_detailed(None).await.unwrap();
        assert_eq!(listed[0].session_count, 2);

        // The session reflects its project_id.
        let s1 = repo.get_session(None, "s1").await.unwrap().unwrap();
        assert_eq!(s1.project_id.as_deref(), Some(proj.id.as_str()));

        // Unassign one => count 1.
        repo.set_session_project("s1", None).await.unwrap();
        let listed = repo.list_projects_detailed(None).await.unwrap();
        assert_eq!(listed[0].session_count, 1);
        let s1 = repo.get_session(None, "s1").await.unwrap().unwrap();
        assert!(s1.project_id.is_none());
    }

    #[tokio::test]
    async fn get_project_returns_member_sessions() {
        let t = temp_repo().await;
        let repo = &t.repo;

        let proj = repo.create_project(None, "P", None).await.unwrap();
        repo.upsert_session("u@x", None, None, &sample_session("ga"))
            .await
            .unwrap();
        repo.upsert_session("u@x", None, None, &sample_session("gb"))
            .await
            .unwrap();
        repo.set_session_project("ga", Some(&proj.id))
            .await
            .unwrap();
        repo.set_session_project("gb", Some(&proj.id))
            .await
            .unwrap();

        let detail = repo.get_project(None, &proj.id).await.unwrap().unwrap();
        assert_eq!(detail.project.id, proj.id);
        assert_eq!(detail.project.session_count, 2);
        let ids: std::collections::HashSet<&str> =
            detail.sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains("ga"));
        assert!(ids.contains("gb"));

        // Unknown project => None.
        assert!(repo.get_project(None, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn project_description_round_trips() {
        let t = temp_repo().await;
        let repo = &t.repo;

        // Create with a description => round-trips in get_project + list.
        let proj = repo
            .create_project(Some("o@x"), "Described", Some("a blurb"))
            .await
            .unwrap();
        assert_eq!(proj.description.as_deref(), Some("a blurb"));

        let detail = repo.get_project(None, &proj.id).await.unwrap().unwrap();
        assert_eq!(detail.project.description.as_deref(), Some("a blurb"));

        let listed = repo.list_projects_detailed(None).await.unwrap();
        let row = listed.iter().find(|p| p.id == proj.id).unwrap();
        assert_eq!(row.description.as_deref(), Some("a blurb"));
    }

    #[tokio::test]
    async fn project_update_sets_and_clears_description() {
        let t = temp_repo().await;
        let repo = &t.repo;

        let proj = repo.create_project(None, "Orig", None).await.unwrap();
        assert!(proj.description.is_none());
        let detail = repo.get_project(None, &proj.id).await.unwrap().unwrap();
        assert!(detail.project.description.is_none());

        // Update sets a new description and renames.
        repo.update_project(&proj.id, "Renamed", Some("now described"))
            .await
            .unwrap();
        let detail = repo.get_project(None, &proj.id).await.unwrap().unwrap();
        assert_eq!(detail.project.name, "Renamed");
        assert_eq!(detail.project.description.as_deref(), Some("now described"));

        // Update can clear it back to None.
        repo.update_project(&proj.id, "Renamed", None)
            .await
            .unwrap();
        let detail = repo.get_project(None, &proj.id).await.unwrap().unwrap();
        assert!(detail.project.description.is_none());

        // Updating a missing project still 404s.
        let err = repo
            .update_project("ghost", "x", Some("y"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound));
    }

    #[tokio::test]
    async fn set_session_project_validations() {
        let t = temp_repo().await;
        let repo = &t.repo;
        let proj = repo.create_project(None, "P", None).await.unwrap();
        repo.upsert_session("u@x", None, None, &sample_session("v1"))
            .await
            .unwrap();

        // Missing session => NotFound.
        let err = repo
            .set_session_project("ghost", Some(&proj.id))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound));

        // Unknown project => BadRequest.
        let err = repo
            .set_session_project("v1", Some("missing"))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn project_assignment_never_makes_peers_and_does_not_gate_identity() {
        use crate::store::messaging::AgentMessagingRepository;
        let t = temp_repo().await;
        let repo = &t.repo;
        let project = repo.create_project(None, "P", None).await.unwrap();
        let other = repo.create_project(None, "Q", None).await.unwrap();
        for id in ["moving", "peer-p", "peer-q"] {
            repo.upsert_session("u@x", None, None, &sample_session(id))
                .await
                .unwrap();
        }
        repo.set_session_project("peer-p", Some(&project.id))
            .await
            .unwrap();
        repo.set_session_project("peer-q", Some(&other.id))
            .await
            .unwrap();
        // Sharing a project is not a messaging grant: nobody becomes a peer.
        for assignment in [
            None,
            Some(project.id.as_str()),
            Some(other.id.as_str()),
            None,
            Some(project.id.as_str()),
        ] {
            repo.set_session_project("moving", assignment)
                .await
                .unwrap();
            let context = repo
                .agent_messaging_context("moving")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(context.project_id.as_deref(), assignment);
            let peers = repo.agent_messaging_peers("moving").await.unwrap();
            assert!(peers.is_empty(), "{peers:?}");
        }
    }

    #[tokio::test]
    async fn delete_project_unassigns_sessions_and_cascades_chats() {
        let t = temp_repo().await;
        let repo = &t.repo;

        let proj = repo.create_project(None, "P", None).await.unwrap();
        repo.upsert_session("u@x", None, None, &sample_session("dp1"))
            .await
            .unwrap();
        repo.set_session_project("dp1", Some(&proj.id))
            .await
            .unwrap();

        // A project chat with a message.
        let chat = repo
            .create_chat(
                &ChatParent::Project(proj.id.clone()),
                SourceContext::Unmanaged,
                None,
                None,
            )
            .await
            .unwrap();
        repo.add_chat_message(&chat.id, SourceContext::Unmanaged, "user", "hi")
            .await
            .unwrap();

        repo.delete_project(&proj.id).await.unwrap();

        // Session survives, now unassigned.
        let s = repo.get_session(None, "dp1").await.unwrap().unwrap();
        assert!(s.project_id.is_none());

        // Project chat + its messages are gone.
        assert!(repo.get_chat(&chat.id).await.unwrap().is_none());
        assert!(repo
            .get_chat_with_messages(&chat.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn delete_session_cascades_session_chats() {
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.upsert_session("u@x", None, None, &sample_session("ds1"))
            .await
            .unwrap();
        let chat = repo
            .create_chat(
                &ChatParent::Session("ds1".to_string()),
                SourceContext::Unmanaged,
                None,
                None,
            )
            .await
            .unwrap();
        repo.add_chat_message(&chat.id, SourceContext::Unmanaged, "user", "q")
            .await
            .unwrap();

        repo.delete_session("ds1").await.unwrap();
        assert!(repo.get_chat(&chat.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn create_chat_validates_parent_and_check() {
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.upsert_session("u@x", None, None, &sample_session("cc1"))
            .await
            .unwrap();
        let proj = repo.create_project(None, "P", None).await.unwrap();

        // Valid session chat.
        let sc = repo
            .create_chat(
                &ChatParent::Session("cc1".to_string()),
                SourceContext::Unmanaged,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(sc.session_id.as_deref(), Some("cc1"));
        assert!(sc.project_id.is_none());

        // Valid project chat.
        let pc = repo
            .create_chat(
                &ChatParent::Project(proj.id.clone()),
                SourceContext::Unmanaged,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(pc.project_id.as_deref(), Some(proj.id.as_str()));
        assert!(pc.session_id.is_none());

        // Unknown parents are rejected (BadRequest), so the CHECK is never even
        // exercised with a bad parent id.
        let err = repo
            .create_chat(
                &ChatParent::Session("ghost".to_string()),
                SourceContext::Unmanaged,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        let err = repo
            .create_chat(
                &ChatParent::Project("ghost".to_string()),
                SourceContext::Unmanaged,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn chat_messages_oldest_first_and_missing_chat() {
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.upsert_session("u@x", None, None, &sample_session("cm1"))
            .await
            .unwrap();
        let chat = repo
            .create_chat(
                &ChatParent::Session("cm1".to_string()),
                SourceContext::Unmanaged,
                None,
                None,
            )
            .await
            .unwrap();

        // Distinct timestamps guarantee a stable oldest-first order.
        for i in 0..3 {
            repo.add_chat_message(&chat.id, SourceContext::Unmanaged, "user", &format!("m{i}"))
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let cwm = repo
            .get_chat_with_messages(&chat.id)
            .await
            .unwrap()
            .unwrap();
        let contents: Vec<&str> = cwm.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["m0", "m1", "m2"]);

        // Adding to a missing chat => NotFound.
        let err = repo
            .add_chat_message("ghost", SourceContext::Unmanaged, "user", "x")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound));
    }

    #[tokio::test]
    async fn list_chats_viewer_filtering() {
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.upsert_session("u@x", None, None, &sample_session("lc1"))
            .await
            .unwrap();
        let parent = ChatParent::Session("lc1".to_string());

        // Owned by alice (private).
        let _own = repo
            .create_chat(&parent, SourceContext::Unmanaged, Some("alice@x"), None)
            .await
            .unwrap();
        // Owned by bob, shared.
        let shared = repo
            .create_chat(&parent, SourceContext::Unmanaged, Some("bob@x"), None)
            .await
            .unwrap();
        repo.update_chat(&shared.id, true, None).await.unwrap();
        // Owned by bob, private (must be hidden from alice).
        let _hidden = repo
            .create_chat(&parent, SourceContext::Unmanaged, Some("bob@x"), None)
            .await
            .unwrap();

        // No viewer => all three.
        let all = repo.list_chats(&parent, None).await.unwrap();
        assert_eq!(all.len(), 3);

        // alice => her own + the shared one.
        let alice = repo.list_chats(&parent, Some("alice@x")).await.unwrap();
        assert_eq!(alice.len(), 2);
        assert!(alice
            .iter()
            .all(|c| c.owner_email.as_deref() == Some("alice@x") || c.shared));
    }

    #[tokio::test]
    async fn update_chat_and_delete_chat() {
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.upsert_session("u@x", None, None, &sample_session("uc1"))
            .await
            .unwrap();
        let chat = repo
            .create_chat(
                &ChatParent::Session("uc1".to_string()),
                SourceContext::Unmanaged,
                None,
                Some("t1"),
            )
            .await
            .unwrap();
        assert!(!chat.shared);

        // Toggle shared, leave title unchanged (None).
        repo.update_chat(&chat.id, true, None).await.unwrap();
        let got = repo.get_chat(&chat.id).await.unwrap().unwrap();
        assert!(got.shared);
        assert_eq!(got.title.as_deref(), Some("t1"));

        // Update title too.
        repo.update_chat(&chat.id, false, Some("t2")).await.unwrap();
        let got = repo.get_chat(&chat.id).await.unwrap().unwrap();
        assert!(!got.shared);
        assert_eq!(got.title.as_deref(), Some("t2"));

        // Unknown chat => NotFound.
        let err = repo.update_chat("ghost", true, None).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound));

        // Delete cascades messages.
        repo.add_chat_message(&chat.id, SourceContext::Unmanaged, "user", "m")
            .await
            .unwrap();
        repo.delete_chat(&chat.id).await.unwrap();
        assert!(repo.get_chat(&chat.id).await.unwrap().is_none());
        // Delete is a no-op when absent.
        repo.delete_chat(&chat.id).await.unwrap();
    }

    fn live_message(uuid: &str, text: &str) -> core::Message {
        core::Message {
            uuid: uuid.to_string(),
            parent_uuid: None,
            role: core::Role::User,
            kind: core::MessageKind::Text,
            model: None,
            is_sidechain: false,
            timestamp: None,
            blocks: vec![core::ContentBlock::Text {
                text: text.to_string(),
            }],
            text: text.to_string(),
            raw: serde_json::json!({ "content": text }),
            usage: None,
        }
    }

    fn new_live(id: &str) -> NewLiveSession {
        NewLiveSession {
            id: id.to_string(),
            session_id: id.to_string(),
            agent: core::AgentKind::Claude,
            title: Some(format!("live {id}")),
            project_path: Some("/tmp/proj".to_string()),
            cwd: Some("/tmp/proj".to_string()),
            git_branch: Some("main".to_string()),
            owner_email: Some("owner@x".to_string()),
            source_host: Some("host-1".to_string()),
            remote_url: Some("git@github.com:acme/widgets.git".to_string()),
            mode: "view".to_string(),
        }
    }

    #[tokio::test]
    async fn live_append_dedup_and_seq() {
        let t = temp_repo().await;
        let repo = &t.repo;

        let live = repo.create_live_session(&new_live("live-1")).await.unwrap();
        assert_eq!(live.id, "live-1");
        assert_eq!(live.status, "live");
        assert_eq!(live.mode, "view");

        // First append => seq 0, stored.
        let m0 = repo
            .append_live_message("live-1", &live_message("m0", "hi"))
            .await
            .unwrap()
            .expect("stored");
        assert_eq!(m0.seq, 0);

        // Second distinct message => seq 1.
        let m1 = repo
            .append_live_message("live-1", &live_message("m1", "yo"))
            .await
            .unwrap()
            .expect("stored");
        assert_eq!(m1.seq, 1);

        // Re-push of m0 => dup => None, not double-stored.
        let dup = repo
            .append_live_message("live-1", &live_message("m0", "hi"))
            .await
            .unwrap();
        assert!(dup.is_none());

        let msgs = repo.get_live_messages("live-1").await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].seq, 0);
        assert_eq!(msgs[0].message.uuid, "m0");
        assert_eq!(msgs[1].seq, 1);
        assert_eq!(msgs[1].message.text, "yo");

        // Incremental replay: `since` returns only messages with seq > since.
        let since_0 = repo.get_live_messages_since("live-1", 0).await.unwrap();
        assert_eq!(since_0.len(), 1);
        assert_eq!(since_0[0].seq, 1);
        assert_eq!(since_0[0].message.uuid, "m1");
        // A cursor at/after the latest seq yields nothing (client is caught up).
        assert!(repo
            .get_live_messages_since("live-1", 1)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn live_list_and_complete_graduates() {
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.create_live_session(&new_live("grad-1")).await.unwrap();
        repo.append_live_message("grad-1", &live_message("g0", "first"))
            .await
            .unwrap();
        repo.append_live_message("grad-1", &live_message("g1", "second"))
            .await
            .unwrap();

        // Listed as live.
        let live = repo.list_live_sessions(Some("live"), None).await.unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, "grad-1");

        // The eager `sessions` row exists from registration — a placeholder
        // (0 messages) until graduation, and listed as live.
        let eager = repo
            .get_session(None, "grad-1")
            .await
            .unwrap()
            .expect("eager session row exists from register");
        assert_eq!(eager.message_count, 0);
        let listed = repo.list_sessions(None, None, None).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "grad-1");
        assert!(listed[0].is_live, "live session should be flagged is_live");

        // Complete => graduates into sessions via upsert_session.
        let graduated = repo.complete_live_session("grad-1").await.unwrap();
        assert_eq!(graduated.as_deref(), Some("grad-1"));

        let stored = repo
            .get_session(None, "grad-1")
            .await
            .unwrap()
            .expect("graduated session row exists");
        assert_eq!(stored.message_count, 2);
        assert_eq!(stored.title, "live grad-1");
        assert_eq!(stored.messages[0].uuid, "g0");
        assert_eq!(stored.owner_email.as_deref(), Some("owner@x"));

        // No longer listed as live.
        let live = repo.list_live_sessions(Some("live"), None).await.unwrap();
        assert!(live.is_empty());
        // And the stored card is no longer flagged live (status → graduated).
        let listed = repo.list_sessions(None, None, None).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].is_live);

        // The live row now reports graduated status pointing at the session.
        let row = repo.get_live_session("grad-1").await.unwrap().unwrap();
        assert_eq!(row.status, "graduated");
        assert_eq!(row.graduated_session_id.as_deref(), Some("grad-1"));

        // Completing an absent live session => None.
        assert!(repo.complete_live_session("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn link_pr_to_live_session_persists_and_survives_graduation() {
        // Regression for the relaxed `session_prs` FK (migration 0016): a PR can
        // be linked to a LIVE session that has no `sessions` row yet, the link
        // persists, carries over verbatim on graduation (id == session_id), and
        // is cleaned up explicitly on delete (cascade no longer covers it).
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.create_live_session(&new_live("pr-live"))
            .await
            .unwrap();

        let pr = SessionPr {
            url: "https://github.com/Example-Org/example-platform/pull/193".to_string(),
            owner: "Example-Org".to_string(),
            repo: "example-platform".to_string(),
            number: 193,
        };
        // Linking succeeds even though no `sessions` row exists yet.
        repo.link_session_pr("pr-live", &pr).await.unwrap();

        let linked = repo.list_session_prs("pr-live").await.unwrap();
        assert_eq!(linked.len(), 1, "PR persists on the live session");
        assert_eq!(linked[0].number, 193);

        // Graduate; the link survives because the id is unchanged.
        repo.append_live_message("pr-live", &live_message("p0", "first"))
            .await
            .unwrap();
        let graduated = repo.complete_live_session("pr-live").await.unwrap();
        assert_eq!(graduated.as_deref(), Some("pr-live"));

        let after = repo.list_session_prs("pr-live").await.unwrap();
        assert_eq!(after.len(), 1, "PR link survives graduation");
        assert_eq!(after[0].url, pr.url);

        // Deleting the session removes the PR rows (explicit cleanup, not FK).
        repo.delete_session("pr-live").await.unwrap();
        let gone = repo.list_session_prs("pr-live").await.unwrap();
        assert!(gone.is_empty(), "PR rows cleaned up on delete");
    }

    #[tokio::test]
    async fn live_mark_abandoned() {
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.create_live_session(&new_live("stale-1"))
            .await
            .unwrap();
        // Nothing older than an hour ago => no rows marked.
        let n = repo
            .mark_abandoned_live_sessions(Utc::now() - chrono::Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(n, 0);

        // Everything older than "now + 1s" => the live row is marked abandoned.
        let n = repo
            .mark_abandoned_live_sessions(Utc::now() + chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(n, 1);
        let row = repo.get_live_session("stale-1").await.unwrap().unwrap();
        assert_eq!(row.status, "abandoned");
        // Abandoned rows drop out of the live list.
        assert!(repo
            .list_live_sessions(Some("live"), None)
            .await
            .unwrap()
            .is_empty());
    }

    /// A heartbeat (`touch_live_session`) resurrects a prematurely-abandoned row
    /// back to `live`, but never resurrects a terminal `graduated` row.
    #[tokio::test]
    async fn touch_live_session_resurrects_abandoned_not_graduated() {
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.create_live_session(&new_live("hb-1")).await.unwrap();
        // Force it abandoned (as the reaper would on a heartbeat gap).
        repo.mark_abandoned_live_sessions(Utc::now() + chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(
            repo.get_live_session("hb-1").await.unwrap().unwrap().status,
            "abandoned"
        );

        // A returning heartbeat brings it back to `live` and the live list.
        assert!(repo.touch_live_session("hb-1").await.unwrap());
        assert_eq!(
            repo.get_live_session("hb-1").await.unwrap().unwrap().status,
            "live"
        );
        assert_eq!(
            repo.list_live_sessions(Some("live"), None)
                .await
                .unwrap()
                .len(),
            1
        );

        // Graduate it, then a late heartbeat must NOT pull it back to `live` and
        // must NOT touch the terminal row at all — the `status IN (live,
        // abandoned)` predicate excludes it, so the update affects 0 rows.
        repo.complete_live_session("hb-1").await.unwrap();
        assert_eq!(
            repo.get_live_session("hb-1").await.unwrap().unwrap().status,
            "graduated"
        );
        assert!(
            !repo.touch_live_session("hb-1").await.unwrap(),
            "a heartbeat for a graduated session is a no-op"
        );
        assert_eq!(
            repo.get_live_session("hb-1").await.unwrap().unwrap().status,
            "graduated"
        );
    }

    #[tokio::test]
    async fn create_live_session_is_idempotent_resumes() {
        let t = temp_repo().await;
        let repo = &t.repo;

        // First registration inserts a fresh live row.
        let first = repo
            .create_live_session(&new_live("resume-1"))
            .await
            .unwrap();
        assert_eq!(first.id, "resume-1");
        assert_eq!(first.status, "live");
        assert_eq!(first.title.as_deref(), Some("live resume-1"));
        let original_started = first.started_at;

        // Drive the row out of 'live' so we can prove re-register resumes it.
        let n = repo
            .mark_abandoned_live_sessions(Utc::now() + chrono::Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            repo.get_live_session("resume-1")
                .await
                .unwrap()
                .unwrap()
                .status,
            "abandoned"
        );

        // Re-register the same id with refreshed metadata: no PK error, the
        // row resumes ('live', ended_at cleared), title is refreshed, and the
        // original started_at + id are preserved.
        let mut again = new_live("resume-1");
        again.title = Some("resumed title".to_string());
        let resumed = repo.create_live_session(&again).await.unwrap();

        assert_eq!(resumed.id, "resume-1");
        assert_eq!(resumed.status, "live");
        assert!(resumed.ended_at.is_none());
        assert_eq!(resumed.title.as_deref(), Some("resumed title"));
        assert_eq!(resumed.started_at, original_started);
    }

    #[tokio::test]
    async fn update_live_session_title_updates_existing_only() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.create_live_session(&new_live("title-1"))
            .await
            .unwrap();

        // Updates the existing row.
        let updated = repo
            .update_live_session_title("title-1", "Add the widget endpoint")
            .await
            .unwrap();
        assert!(updated);
        assert_eq!(
            repo.get_live_session("title-1")
                .await
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("Add the widget endpoint"),
        );

        // No row for an unknown id ⇒ false (→ 404 at the API).
        let updated = repo
            .update_live_session_title("nope", "whatever")
            .await
            .unwrap();
        assert!(!updated);

        // A late title push after graduation is a no-op: it must not mutate
        // the durable `sessions.title`. Graduate, then push — expect `false`
        // and the stored title left at its graduated value.
        repo.complete_live_session("title-1").await.unwrap();
        let before = repo
            .get_session(None, "title-1")
            .await
            .unwrap()
            .unwrap()
            .title;
        let updated = repo
            .update_live_session_title("title-1", "stale push after graduation")
            .await
            .unwrap();
        assert!(!updated, "graduated session must reject a title push");
        let after = repo
            .get_session(None, "title-1")
            .await
            .unwrap()
            .unwrap()
            .title;
        assert_eq!(before, after, "stored title must be untouched");
    }

    #[tokio::test]
    async fn live_mode_is_threaded() {
        let t = temp_repo().await;
        let repo = &t.repo;

        // Default helper uses mode = "view".
        let v = repo
            .create_live_session(&new_live("mode-view"))
            .await
            .unwrap();
        assert_eq!(v.mode, "view");

        // An interactive registration stores mode = "interactive".
        let mut interactive = new_live("mode-int");
        interactive.mode = "interactive".to_string();
        let i = repo.create_live_session(&interactive).await.unwrap();
        assert_eq!(i.mode, "interactive");
        assert_eq!(
            repo.get_live_session("mode-int")
                .await
                .unwrap()
                .unwrap()
                .mode,
            "interactive"
        );
    }

    #[tokio::test]
    async fn live_owner_and_touch() {
        let t = temp_repo().await;
        let repo = &t.repo;

        repo.create_live_session(&new_live("own-1")).await.unwrap();
        // Owner lookup mirrors get_session_owner's nesting.
        assert_eq!(
            repo.get_live_session_owner("own-1").await.unwrap(),
            Some(Some("owner@x".to_string()))
        );
        // Absent → outer None (→ 404).
        assert_eq!(repo.get_live_session_owner("ghost").await.unwrap(), None);

        // touch bumps last_event_at and reports found/absent.
        assert!(repo.touch_live_session("own-1").await.unwrap());
        assert!(!repo.touch_live_session("ghost").await.unwrap());
    }

    #[tokio::test]
    async fn live_shares_roundtrip_dedup_and_replace() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.create_live_session(&new_live("sh-1")).await.unwrap();

        // Set two shares (read, read+write). Email lowercased/trimmed.
        repo.set_live_shares(
            "sh-1",
            &[
                LiveShare {
                    email: "  Reader@X ".to_string(),
                    can_write: false,
                    can_read: true,
                    can_communicate: false,
                },
                LiveShare {
                    email: "writer@x".to_string(),
                    can_write: true,
                    can_read: true,
                    can_communicate: false,
                },
            ],
        )
        .await
        .unwrap();

        let shares = repo.get_live_shares("sh-1").await.unwrap();
        assert_eq!(shares.len(), 2);
        // Ordered by email.
        assert_eq!(shares[0].email, "reader@x");
        assert!(!shares[0].can_write);
        assert_eq!(shares[1].email, "writer@x");
        assert!(shares[1].can_write);

        // Dedup on email, last write wins.
        repo.set_live_shares(
            "sh-1",
            &[
                LiveShare {
                    email: "dup@x".to_string(),
                    can_write: false,
                    can_read: true,
                    can_communicate: false,
                },
                LiveShare {
                    email: "DUP@x".to_string(),
                    can_write: true,
                    can_read: true,
                    can_communicate: false,
                },
            ],
        )
        .await
        .unwrap();
        let shares = repo.get_live_shares("sh-1").await.unwrap();
        // Replace dropped the previous shares; only the deduped dup@x remains, rw.
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].email, "dup@x");
        assert!(shares[0].can_write);

        // Empty replace clears.
        repo.set_live_shares("sh-1", &[]).await.unwrap();
        assert!(repo.get_live_shares("sh-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn live_session_access_and_cascade() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.create_live_session(&new_live("acc-1")).await.unwrap();
        repo.set_live_shares(
            "acc-1",
            &[LiveShare {
                email: "s@x".to_string(),
                can_write: true,
                can_read: true,
                can_communicate: false,
            }],
        )
        .await
        .unwrap();

        let (owner, shares) = repo
            .get_live_session_access("acc-1")
            .await
            .unwrap()
            .expect("session exists");
        assert_eq!(owner.as_deref(), Some("owner@x"));
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].email, "s@x");

        // Absent session => None.
        assert!(repo
            .get_live_session_access("ghost")
            .await
            .unwrap()
            .is_none());

        // Person grants survive removal of live state and cascade with the session.
        sqlx::query("DELETE FROM live_sessions WHERE id = ?")
            .bind("acc-1")
            .execute(repo.pool())
            .await
            .unwrap();
        assert_eq!(repo.get_live_shares("acc-1").await.unwrap().len(), 1);
        repo.delete_session("acc-1").await.unwrap();
        assert!(repo.get_live_shares("acc-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_live_sessions_filters_by_viewer() {
        let t = temp_repo().await;
        let repo = &t.repo;

        // alice authors live-a; bob authors live-b shared (read) with alice;
        // carol authors live-c, no shares.
        let mut a = new_live("live-a");
        a.owner_email = Some("alice@x".to_string());
        repo.create_live_session(&a).await.unwrap();

        let mut b = new_live("live-b");
        b.owner_email = Some("bob@x".to_string());
        repo.create_live_session(&b).await.unwrap();
        repo.set_live_shares(
            "live-b",
            &[LiveShare {
                email: "alice@x".to_string(),
                can_write: false,
                can_read: true,
                can_communicate: false,
            }],
        )
        .await
        .unwrap();

        let mut c = new_live("live-c");
        c.owner_email = Some("carol@x".to_string());
        repo.create_live_session(&c).await.unwrap();

        // alice sees her own + the one shared with her = 2.
        let alice = repo
            .list_live_sessions(Some("live"), Some("alice@x"))
            .await
            .unwrap();
        let alice_ids: std::collections::HashSet<&str> =
            alice.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(alice.len(), 2, "got {alice_ids:?}");
        assert!(alice_ids.contains("live-a"));
        assert!(alice_ids.contains("live-b"));

        // carol sees only her own = 1.
        let carol = repo
            .list_live_sessions(Some("live"), Some("carol@x"))
            .await
            .unwrap();
        assert_eq!(carol.len(), 1);
        assert_eq!(carol[0].id, "live-c");

        // stranger sees none.
        let stranger = repo
            .list_live_sessions(Some("live"), Some("nobody@x"))
            .await
            .unwrap();
        assert!(stranger.is_empty());

        // No viewer => all 3 (auth-off default).
        let all = repo.list_live_sessions(Some("live"), None).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    /// Graduation preserves revocable person grants independently of live state.
    #[tokio::test]
    async fn soft_close_graduates_and_keeps_sharee_access() {
        let t = temp_repo().await;
        let repo = &t.repo;

        // bob authors a live session, shared (read) with alice.
        let mut b = new_live("live-soft");
        b.owner_email = Some("bob@x".to_string());
        repo.create_live_session(&b).await.unwrap();
        repo.set_live_shares(
            "live-soft",
            &[LiveShare {
                email: "alice@x".to_string(),
                can_write: false,
                can_read: true,
                can_communicate: false,
            }],
        )
        .await
        .unwrap();
        repo.append_live_message("live-soft", &live_message("m0", "hi"))
            .await
            .unwrap()
            .expect("stored");

        // Before: alice (a sharee, NOT the author) sees it live.
        let before = repo
            .list_live_sessions(Some("live"), Some("alice@x"))
            .await
            .unwrap();
        assert!(before.iter().any(|s| s.id == "live-soft"));

        let session_id = repo
            .complete_live_session("live-soft")
            .await
            .unwrap()
            .expect("graduated");
        // After: gone from the live list (status is no longer `live`)...
        let after = repo
            .list_live_sessions(Some("live"), Some("alice@x"))
            .await
            .unwrap();
        assert!(!after.iter().any(|s| s.id == "live-soft"));

        // ...but alice can still find it in the sessions view and open it.
        let sessions = repo
            .list_sessions(Some("alice@x"), None, None)
            .await
            .unwrap();
        assert!(
            sessions.iter().any(|s| s.id == session_id),
            "sharee must still find the soft-closed session in the sessions view"
        );
        assert!(repo
            .get_session(Some("alice@x"), &session_id)
            .await
            .unwrap()
            .is_some());
        repo.set_live_shares("live-soft", &[]).await.unwrap();
        assert!(repo
            .get_session(Some("alice@x"), &session_id)
            .await
            .unwrap()
            .is_none());
    }

    // --- Session PR link tests ---

    #[tokio::test]
    async fn session_prs_link_list_unlink() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, Some("o@x"), &sample_session("sess-pr"))
            .await
            .unwrap();

        // Initially empty.
        let prs = repo.list_session_prs("sess-pr").await.unwrap();
        assert!(prs.is_empty());

        let pr1 = SessionPr {
            url: "https://github.com/org/repo/pull/1".to_string(),
            owner: "org".to_string(),
            repo: "repo".to_string(),
            number: 1,
        };
        let pr2 = SessionPr {
            url: "https://github.com/org/repo/pull/2".to_string(),
            owner: "org".to_string(),
            repo: "repo".to_string(),
            number: 2,
        };

        repo.link_session_pr("sess-pr", &pr1).await.unwrap();
        repo.link_session_pr("sess-pr", &pr2).await.unwrap();

        let prs = repo.list_session_prs("sess-pr").await.unwrap();
        assert_eq!(prs.len(), 2);
        // Ordered by created_at ASC (pr1 first).
        assert_eq!(prs[0].number, 1);
        assert_eq!(prs[1].number, 2);
        assert_eq!(prs[0].owner, "org");
        assert_eq!(prs[0].repo, "repo");
        assert_eq!(prs[0].url, "https://github.com/org/repo/pull/1");

        // Duplicate link is idempotent (no error, no duplicate row).
        repo.link_session_pr("sess-pr", &pr1).await.unwrap();
        let prs = repo.list_session_prs("sess-pr").await.unwrap();
        assert_eq!(prs.len(), 2, "duplicate link must be a no-op");

        // Unlink pr1.
        let removed = repo
            .unlink_session_pr("sess-pr", "org", "repo", 1)
            .await
            .unwrap();
        assert!(removed, "unlink should return true when a row was deleted");

        let prs = repo.list_session_prs("sess-pr").await.unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 2);

        // Unlinking a non-existent PR returns false (not an error).
        let removed = repo
            .unlink_session_pr("sess-pr", "org", "repo", 999)
            .await
            .unwrap();
        assert!(!removed, "unlink of absent PR should return false");
    }

    #[tokio::test]
    async fn session_pr_status_cache_roundtrip() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, Some("o@x"), &sample_session("sess-pr-status"))
            .await
            .unwrap();

        let pr = SessionPr {
            url: "https://github.com/org/repo/pull/7".to_string(),
            owner: "org".to_string(),
            repo: "repo".to_string(),
            number: 7,
        };
        repo.link_session_pr("sess-pr-status", &pr).await.unwrap();

        // Freshly linked => no cached status.
        let prs = repo.list_session_prs("sess-pr-status").await.unwrap();
        assert_eq!(prs.len(), 1);
        assert!(prs[0].status_state.is_none());
        assert!(prs[0].status_fetched_at.is_none());

        // Push a status snapshot for the linked PR.
        let status = crate::github::PrStatus {
            review_decision: Some("APPROVED".into()),
            state: "open".to_string(),
            merged: false,
            draft: true,
            title: "Add feature".to_string(),
        };
        let now = Utc::now();
        let updated = repo
            .set_session_pr_status(
                SourceContext::Unmanaged,
                "sess-pr-status",
                "org",
                "repo",
                7,
                &status,
                now,
            )
            .await
            .unwrap();
        assert!(updated, "status push to a linked PR must update a row");

        let prs = repo.list_session_prs("sess-pr-status").await.unwrap();
        assert_eq!(prs[0].status_state.as_deref(), Some("open"));
        assert_eq!(prs[0].status_merged, Some(false));
        assert_eq!(prs[0].status_draft, Some(true));
        assert_eq!(prs[0].status_title.as_deref(), Some("Add feature"));
        assert_eq!(prs[0].status_review_decision.as_deref(), Some("APPROVED"));
        assert!(prs[0].status_fetched_at.is_some());

        // Older clients omit the decision; updated clients clear it with an empty string.
        for (decision, expected) in [
            (None, Some("APPROVED")),
            (Some("CHANGES_REQUESTED"), Some("CHANGES_REQUESTED")),
            (Some(""), Some("")),
        ] {
            let status = crate::github::PrStatus {
                review_decision: decision.map(str::to_owned),
                ..status.clone()
            };
            repo.set_session_pr_status(
                SourceContext::Unmanaged,
                "sess-pr-status",
                "org",
                "repo",
                7,
                &status,
                now,
            )
            .await
            .unwrap();
            let prs = repo.list_session_prs("sess-pr-status").await.unwrap();
            assert_eq!(prs[0].status_review_decision.as_deref(), expected);
        }

        // Pushing status for an UNLINKED PR returns false (so the API 404s).
        let updated = repo
            .set_session_pr_status(
                SourceContext::Unmanaged,
                "sess-pr-status",
                "org",
                "repo",
                999,
                &status,
                now,
            )
            .await
            .unwrap();
        assert!(!updated, "status push to an unlinked PR must return false");
    }

    #[tokio::test]
    async fn session_prs_cascade_on_delete() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, Some("o@x"), &sample_session("sess-pr-cascade"))
            .await
            .unwrap();

        repo.link_session_pr(
            "sess-pr-cascade",
            &SessionPr {
                url: "https://github.com/a/b/pull/5".to_string(),
                owner: "a".to_string(),
                repo: "b".to_string(),
                number: 5,
            },
        )
        .await
        .unwrap();

        // Deleting the session must cascade-delete the PR link.
        repo.delete_session("sess-pr-cascade").await.unwrap();
        // The table row is gone; list on a non-existent session returns empty
        // (the FK cascade removed the row).
        let prs = repo.list_session_prs("sess-pr-cascade").await.unwrap();
        assert!(prs.is_empty(), "PR link must cascade-delete with session");
    }

    #[tokio::test]
    async fn auto_link_marks_detected_and_never_clobbers_manual() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, Some("o@x"), &sample_session("sess-auto"))
            .await
            .unwrap();

        // Manually link PR #5 — stays detected = false.
        repo.link_session_pr(
            "sess-auto",
            &SessionPr {
                url: "https://github.com/org/repo/pull/5".to_string(),
                owner: "org".to_string(),
                repo: "repo".to_string(),
                number: 5,
            },
        )
        .await
        .unwrap();

        // Auto-link a batch: a NEW PR #6 (detected) + the already-manual #5.
        let batch = vec![
            SessionPr {
                url: "https://github.com/org/repo/pull/6".to_string(),
                owner: "org".to_string(),
                repo: "repo".to_string(),
                number: 6,
            },
            SessionPr {
                url: "https://github.com/org/repo/pull/5".to_string(),
                owner: "org".to_string(),
                repo: "repo".to_string(),
                number: 5,
            },
        ];
        let inserted = repo
            .auto_link_session_prs("sess-auto", &batch)
            .await
            .unwrap();
        assert_eq!(
            inserted, 1,
            "only the new #6 counts; the already-manual #5 conflict-no-ops"
        );

        let prs = repo.list_session_prs("sess-auto").await.unwrap();
        assert_eq!(prs.len(), 2);
        let pr5 = prs.iter().find(|p| p.number == 5).unwrap();
        let pr6 = prs.iter().find(|p| p.number == 6).unwrap();
        assert!(
            !pr5.detected,
            "manual link #5 must NOT be downgraded to detected"
        );
        assert!(pr6.detected, "auto-linked #6 must be detected");

        // Idempotent: re-running auto-link inserts nothing (so the live path
        // publishes no spurious "PRs changed" nudge) and keeps flags.
        let reinserted = repo
            .auto_link_session_prs("sess-auto", &batch)
            .await
            .unwrap();
        assert_eq!(reinserted, 0, "a re-seen batch must report zero new links");
        let prs = repo.list_session_prs("sess-auto").await.unwrap();
        assert_eq!(prs.len(), 2, "auto-link must be idempotent");
        assert!(!prs.iter().find(|p| p.number == 5).unwrap().detected);
    }

    #[tokio::test]
    async fn auto_link_to_live_session_without_sessions_row_links() {
        let t = temp_repo().await;
        let repo = &t.repo;
        // No `sessions` row for this id (mirrors a live, ungraduated session).
        // Since migration 0016 dropped the session_prs -> sessions(id) FK, the
        // link is now recorded under the live id (== the eventual sessions.id),
        // so it is already valid after graduation — no deferred backfill needed.
        let batch = vec![SessionPr {
            url: "https://github.com/org/repo/pull/9".to_string(),
            owner: "org".to_string(),
            repo: "repo".to_string(),
            number: 9,
        }];
        repo.auto_link_session_prs("live-session", &batch)
            .await
            .expect("auto-link must not fail for a session with no `sessions` row");
        let prs = repo.list_session_prs("live-session").await.unwrap();
        assert_eq!(prs.len(), 1, "PR is linked under the live session id");
        assert_eq!(prs[0].number, 9);
        assert!(prs[0].detected, "auto-linked refs are marked detected");
    }

    #[tokio::test]
    async fn open_pr_watch_targets_filter_status_and_owner() {
        let t = temp_repo().await;
        let repo = &t.repo;
        // Watch targets are scoped to CURRENTLY LIVE sessions, so create the
        // PR-bearing sessions as live ones (sa = alice, sb = bob).
        let live = |id: &str, owner: &str| NewLiveSession {
            id: id.to_string(),
            session_id: id.to_string(),
            agent: core::AgentKind::Claude,
            title: None,
            project_path: None,
            cwd: None,
            git_branch: None,
            owner_email: Some(owner.to_string()),
            source_host: None,
            remote_url: None,
            mode: "view".to_string(),
        };
        repo.create_live_session(&live("sa", "alice@x"))
            .await
            .unwrap();
        repo.create_live_session(&live("sb", "bob@x"))
            .await
            .unwrap();

        let link = |sess: &'static str, n: i64| {
            let pr = SessionPr {
                url: format!("https://github.com/org/repo/pull/{n}"),
                owner: "org".to_string(),
                repo: "repo".to_string(),
                number: n,
            };
            async move { repo.link_session_pr(sess, &pr).await.unwrap() }
        };
        // alice: #1 (no status → null), #2 (open), #3 (merged), #5 (closed)
        link("sa", 1).await;
        link("sa", 2).await;
        link("sa", 3).await;
        link("sa", 5).await;
        // bob: #4 (open)
        link("sb", 4).await;
        // A GRADUATED (stored, not live) session owned by alice with an open PR.
        // It must NOT appear as a proactive watch target — the whole point of
        // the live-only scoping: a finished session only refreshes on Sync-now.
        repo.upsert_session("u@x", None, Some("alice@x"), &sample_session("sg"))
            .await
            .unwrap();
        link("sg", 8).await;

        let now = Utc::now();
        let st = |state: &str, merged: bool| crate::github::PrStatus {
            review_decision: Some("APPROVED".into()),
            state: state.to_string(),
            merged,
            draft: false,
            title: "t".to_string(),
        };
        repo.set_session_pr_status(
            SourceContext::Unmanaged,
            "sa",
            "org",
            "repo",
            2,
            &st("open", false),
            now,
        )
        .await
        .unwrap();
        repo.set_session_pr_status(
            SourceContext::Unmanaged,
            "sa",
            "org",
            "repo",
            3,
            &st("merged", true),
            now,
        )
        .await
        .unwrap();
        repo.set_session_pr_status(
            SourceContext::Unmanaged,
            "sa",
            "org",
            "repo",
            5,
            &st("closed", false),
            now,
        )
        .await
        .unwrap();
        repo.set_session_pr_status(
            SourceContext::Unmanaged,
            "sb",
            "org",
            "repo",
            4,
            &st("open", false),
            now,
        )
        .await
        .unwrap();

        // No owner filter: open + null-status across both sessions, merged/closed
        // excluded, ordered by session_id then number.
        let all = repo.list_open_pr_watch_targets(None).await.unwrap();
        let got: Vec<(String, i64)> = all
            .iter()
            .map(|t| (t.session_id.clone(), t.number))
            .collect();
        assert_eq!(
            got,
            vec![
                ("sa".to_string(), 1),
                ("sa".to_string(), 2),
                ("sb".to_string(), 4),
            ],
            "open + null-status only; merged/closed excluded; graduated session excluded"
        );
        assert!(
            !all.iter().any(|t| t.session_id == "sg"),
            "a graduated (non-live) session is never a proactive watch target"
        );
        assert_eq!(all[0].owner, "org");
        assert_eq!(all[0].repo, "repo");
        assert_eq!(all[0].url, "https://github.com/org/repo/pull/1");

        // Owner filter: only alice's open/null PRs.
        let alice = repo
            .list_open_pr_watch_targets(Some("alice@x"))
            .await
            .unwrap();
        let got: Vec<i64> = alice.iter().map(|t| t.number).collect();
        assert_eq!(got, vec![1, 2], "owner filter scopes to alice's sessions");
    }

    #[tokio::test]
    async fn pr_scanned_at_roundtrip() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, Some("o@x"), &sample_session("sess-scan"))
            .await
            .unwrap();

        // Never scanned ⇒ None.
        assert!(repo.pr_scanned_at("sess-scan").await.unwrap().is_none());
        // Absent session ⇒ None.
        assert!(repo.pr_scanned_at("nope").await.unwrap().is_none());

        let now = Utc::now();
        repo.set_pr_scanned_at("sess-scan", now).await.unwrap();
        let got = repo.pr_scanned_at("sess-scan").await.unwrap();
        assert!(got.is_some(), "scan stamp must round-trip");
        assert!((got.unwrap() - now).num_seconds().abs() < 2);
    }

    // --- Session PR comment cache tests ---

    fn pr_comment(id: &str, created_at: &str, body: &str) -> crate::github::PrComment {
        crate::github::PrComment {
            comment_id: id.to_string(),
            kind: "issue".to_string(),
            author: "octocat".to_string(),
            body: body.to_string(),
            html_url: format!("https://github.com/org/repo/pull/1#issuecomment-{id}"),
            created_at: created_at.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn pr_comments_upsert_dedupes_and_lists_sorted() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, Some("o@x"), &sample_session("sess-prc"))
            .await
            .unwrap();

        // Initially empty.
        assert!(repo.list_pr_comments("sess-prc").await.unwrap().is_empty());

        // Push two comments out of chronological order.
        let batch = vec![
            pr_comment("200", "2024-01-02T00:00:00Z", "second"),
            pr_comment("100", "2024-01-01T00:00:00Z", "first"),
        ];
        let added = repo
            .upsert_pr_comments(
                SourceContext::Unmanaged,
                "sess-prc",
                "org",
                "repo",
                1,
                &batch,
            )
            .await
            .unwrap();
        assert!(added);

        let listed = repo.list_pr_comments("sess-prc").await.unwrap();
        assert_eq!(listed.len(), 2);
        // Sorted by created_at ascending.
        assert_eq!(listed[0].comment_id, "100");
        assert_eq!(listed[0].body, "first");
        assert_eq!(listed[1].comment_id, "200");
        assert_eq!(listed[0].owner, "org");
        assert_eq!(listed[0].repo, "repo");
        assert_eq!(listed[0].number, 1);
        assert_eq!(listed[0].kind, "issue");
        assert_eq!(listed[0].author, "octocat");
        assert_eq!(listed[0].created_at, "2024-01-01T00:00:00Z");
        let cursor = listed[0].revision;

        let unchanged = repo
            .upsert_pr_comments(
                SourceContext::Unmanaged,
                "sess-prc",
                "org",
                "repo",
                1,
                &batch,
            )
            .await
            .unwrap();
        assert!(!unchanged, "an unchanged GitHub snapshot is not a delta");
        assert_eq!(repo.list_pr_comments("sess-prc").await.unwrap(), listed);

        // Re-push the same comment_id with a refreshed body → dedupes (no new
        // row) and updates the body in place.
        let changed = repo
            .upsert_pr_comments(
                SourceContext::Unmanaged,
                "sess-prc",
                "org",
                "repo",
                1,
                &[pr_comment("100", "2024-01-01T00:00:00Z", "first-edited")],
            )
            .await
            .unwrap();
        assert!(changed);
        let delta = repo
            .list_pr_comments_since("sess-prc", cursor)
            .await
            .unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].comment_id, "100");

        let listed = repo.list_pr_comments("sess-prc").await.unwrap();
        assert_eq!(listed.len(), 2, "re-push of same comment_id must dedupe");
        assert_eq!(listed[0].comment_id, "100");
        assert_eq!(listed[0].body, "first-edited", "body must be refreshed");

        let long = "x".repeat(500);
        let more: Vec<_> = (300..306)
            .map(|id| {
                pr_comment(
                    &id.to_string(),
                    &format!("2024-01-03T00:00:{:02}Z", id - 300),
                    &long,
                )
            })
            .collect();
        repo.upsert_pr_comments(
            SourceContext::Unmanaged,
            "sess-prc",
            "org",
            "repo",
            1,
            &more,
        )
        .await
        .unwrap();
        let previews = repo.list_pr_comment_previews("sess-prc").await.unwrap();
        assert_eq!(previews.len(), 4, "the rail must not fetch every comment");
        assert!(previews
            .iter()
            .all(|c| c.body.len() == 240 && c.diff_hunk.is_none()));
    }

    #[tokio::test]
    async fn pr_comment_previews_keep_actionable_comments_after_newer_rollups() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, Some("o@x"), &sample_session("sess-preview"))
            .await
            .unwrap();

        let mut inline = pr_comment("inline", "2024-01-01T00:00:00Z", "fix this");
        inline.kind = "review_comment".to_string();
        inline.author = "coderabbitai".to_string();
        inline.resolved = Some(false);
        let human = pr_comment("human", "2024-01-02T00:00:00Z", "please review");
        let rollups: Vec<_> = (3..7)
            .map(|i| {
                let mut c = pr_comment(
                    &i.to_string(),
                    &format!("2024-01-0{i}T00:00:00Z"),
                    "summary",
                );
                c.kind = if i % 2 == 0 { "review" } else { "issue" }.to_string();
                c.author = if i % 2 == 0 {
                    " CodeRabbitAI[bot] "
                } else {
                    "coderabbitai"
                }
                .to_string();
                c
            })
            .collect();
        let comments = [vec![inline, human], rollups].concat();
        repo.upsert_pr_comments(
            SourceContext::Unmanaged,
            "sess-preview",
            "org",
            "repo",
            1,
            &comments,
        )
        .await
        .unwrap();

        let previews = repo.list_pr_comment_previews("sess-preview").await.unwrap();
        let ids: Vec<_> = previews.iter().map(|c| c.comment_id.as_str()).collect();
        assert_eq!(ids, ["inline", "human"]);
    }

    #[tokio::test]
    async fn review_comment_context_round_trips() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session("u@x", None, Some("o@x"), &sample_session("sess-rcc"))
            .await
            .unwrap();

        // An inline review comment carrying file/line/diff context + resolution,
        // alongside a plain issue comment with none of it.
        let review = crate::github::PrComment {
            comment_id: "review_comment:42".to_string(),
            kind: "review_comment".to_string(),
            author: "coderabbitai".to_string(),
            body: "off by one".to_string(),
            html_url: "https://github.com/org/repo/pull/1#discussion_r42".to_string(),
            created_at: "2024-01-03T00:00:00Z".to_string(),
            path: Some("src/main.rs".to_string()),
            line: Some(41),
            diff_hunk: Some("@@ -38,4 +38,4 @@\n-old\n+new".to_string()),
            resolved: Some(false),
        };
        repo.upsert_pr_comments(
            SourceContext::Unmanaged,
            "sess-rcc",
            "org",
            "repo",
            1,
            &[
                review,
                pr_comment("100", "2024-01-01T00:00:00Z", "plain issue"),
            ],
        )
        .await
        .unwrap();

        let listed = repo.list_pr_comments("sess-rcc").await.unwrap();
        assert_eq!(listed.len(), 2);

        // The inline review comment carries its context back verbatim.
        let rc = listed
            .iter()
            .find(|c| c.comment_id == "review_comment:42")
            .expect("review comment present");
        assert_eq!(rc.path.as_deref(), Some("src/main.rs"));
        assert_eq!(rc.line, Some(41));
        assert!(rc.diff_hunk.as_deref().unwrap().contains("@@"));
        assert_eq!(rc.resolved, Some(false));

        // The issue comment has NULL context (not populated for that kind).
        let issue = listed
            .iter()
            .find(|c| c.comment_id == "100")
            .expect("issue comment present");
        assert_eq!(issue.path, None);
        assert_eq!(issue.line, None);
        assert_eq!(issue.diff_hunk, None);
        assert_eq!(issue.resolved, None);

        // Re-push the review comment now RESOLVED → the flag flips in place.
        let resolved_now = crate::github::PrComment {
            resolved: Some(true),
            ..listed
                .iter()
                .find(|c| c.comment_id == "review_comment:42")
                .map(|c| crate::github::PrComment {
                    comment_id: c.comment_id.clone(),
                    kind: c.kind.clone(),
                    author: c.author.clone(),
                    body: c.body.clone(),
                    html_url: c.html_url.clone(),
                    created_at: c.created_at.clone(),
                    path: c.path.clone(),
                    line: c.line,
                    diff_hunk: c.diff_hunk.clone(),
                    resolved: c.resolved,
                })
                .unwrap()
        };
        repo.upsert_pr_comments(
            SourceContext::Unmanaged,
            "sess-rcc",
            "org",
            "repo",
            1,
            &[resolved_now],
        )
        .await
        .unwrap();
        let listed = repo.list_pr_comments("sess-rcc").await.unwrap();
        let rc = listed
            .iter()
            .find(|c| c.comment_id == "review_comment:42")
            .unwrap();
        assert_eq!(
            rc.resolved,
            Some(true),
            "re-sync must flip resolved in place"
        );
    }

    #[tokio::test]
    async fn pr_comments_cascade_on_delete() {
        let t = temp_repo().await;
        let repo = &t.repo;
        repo.upsert_session(
            "u@x",
            None,
            Some("o@x"),
            &sample_session("sess-prc-cascade"),
        )
        .await
        .unwrap();
        repo.upsert_pr_comments(
            SourceContext::Unmanaged,
            "sess-prc-cascade",
            "org",
            "repo",
            1,
            &[pr_comment("1", "2024-01-01T00:00:00Z", "hi")],
        )
        .await
        .unwrap();

        repo.delete_session("sess-prc-cascade").await.unwrap();
        let listed = repo.list_pr_comments("sess-prc-cascade").await.unwrap();
        assert!(
            listed.is_empty(),
            "comments must cascade-delete with session"
        );
    }
}
