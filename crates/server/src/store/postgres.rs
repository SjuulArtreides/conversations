//! Postgres implementation of [`Repository`] backed by an sqlx `PgPool`.
//!
//! Behaviorally identical to [`super::sqlite::SqliteRepo`], differing only in
//! dialect: `$1,$2,...` placeholders (not `?`), `TIMESTAMPTZ`/`JSONB`/`BOOLEAN`
//! column types, and `ON CONFLICT ... DO UPDATE` upserts. Like the SQLite impl
//! it uses sqlx's runtime query API (`sqlx::query`) — NOT the compile-time
//! `query!` macros — so the crate builds without a live database / `DATABASE_URL`.
//!
//! The opt-in `postgres_search` integration test exercises this repository against
//! a disposable PostgreSQL instance. The schema lives in `migrations/postgres/`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use conversations_core as core;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};

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

/// Postgres-backed repository.
#[derive(Clone)]
pub struct PostgresRepo {
    pool: PgPool,
}

fn stored_pr_comment(r: &PgRow) -> StoredPrComment {
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
        resolved: r.get("resolved"),
        ingested_at: r.get("ingested_at"),
        revision: r.get("revision"),
    }
}

impl PostgresRepo {
    /// Connect to `database_url`, run migrations, and return the repo.
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await?;
        let repo = PostgresRepo { pool };
        repo.migrate().await?;
        Ok(repo)
    }

    /// Build a repo from an already-constructed pool (used in tests).
    pub fn from_pool(pool: PgPool) -> Self {
        PostgresRepo { pool }
    }

    /// Run the embedded Postgres migrations.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations/postgres")
            .run(&self.pool)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
        Ok(())
    }

    /// Borrow the underlying pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl Repository for PostgresRepo {
    async fn live_order(&self, viewer: &str) -> Result<super::LiveOrder> {
        let row = sqlx::query("SELECT revision, session_ids FROM live_tab_order WHERE viewer = $1")
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
        let result = sqlx::query("INSERT INTO live_tab_order (viewer, revision, session_ids) VALUES ($1, 1, $2) ON CONFLICT(viewer) DO UPDATE SET revision = live_tab_order.revision + 1, session_ids = excluded.session_ids WHERE live_tab_order.revision = $3")
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
        // A NUL anywhere in the transcript would fail every statement below —
        // and, on the graduation path, wedge the live session permanently. The
        // caller-supplied metadata rides the same INSERT, so it is scrubbed too.
        let session = scrub_session(session);
        let session = session.as_ref();
        let uploaded_by = scrub_str(uploaded_by);
        let uploaded_by = uploaded_by.as_ref();
        let source_host = scrub_opt(source_host);
        let source_host = source_host.as_deref();
        let owner_email = scrub_opt(owner_email);
        let owner_email = owner_email.as_deref();

        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, true, session.id.as_str());

        let existed: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = $1")
            .bind(&session.id)
            .fetch_optional(&mut *tx)
            .await?;
        let created = existed.is_none();

        let message_count = session.message_count();
        let uploaded_at = Utc::now();

        // On CREATE owner_email/visibility are set from the insert values; on
        // RE-UPLOAD (ON CONFLICT) those columns are NOT touched so sharing
        // settings are preserved.
        sqlx::query(
            r#"
            INSERT INTO sessions
                (id, agent, title, project_path, cwd, git_branch, claude_version,
                 started_at, ended_at, message_count, skipped_lines,
                 uploaded_by, uploaded_at, source_host, owner_email, visibility)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, 'restricted')
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
        .bind(session.started_at)
        .bind(session.ended_at)
        .bind(message_count as i64)
        .bind(session.skipped_lines as i64)
        .bind(uploaded_by)
        .bind(uploaded_at)
        .bind(source_host)
        .bind(owner_email)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM messages WHERE session_id = $1")
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
            let raw = serde_json::to_value(&msg.raw)?;
            let usage = msg.usage.as_ref().map(serde_json::to_value).transpose()?;

            sqlx::query(
                r#"
                INSERT INTO messages
                    (uuid, session_id, parent_uuid, role, kind, model,
                     is_sidechain, timestamp, text, raw, usage, ord)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                "#,
            )
            .bind(&msg.uuid)
            .bind(&session.id)
            .bind(&msg.parent_uuid)
            .bind(&role)
            .bind(&kind)
            .bind(&msg.model)
            .bind(msg.is_sidechain)
            .bind(msg.timestamp)
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
        let mut binds: Vec<String> = Vec::new();
        // Next positional placeholder index (`$1`, `$2`, ...).
        let mut next: usize = 1;

        if let Some(email) = viewer {
            conditions.push(format!(
                "(visibility = 'organization' OR owner_email = ${} \
                 OR EXISTS (SELECT 1 FROM session_shares s \
                    WHERE s.session_id = sessions.id AND s.email = ${}) OR EXISTS (SELECT 1 FROM live_session_shares ls WHERE ls.live_id = sessions.id AND ls.communication_only = FALSE AND ls.email = ${}))",
                next,
                next + 1,
                next + 1
            ));
            binds.push(email.to_string());
            binds.push(email.to_string());
            next += 2;
        }

        if let Some(needle) = q {
            let pattern = format!("%{}%", needle.to_lowercase());
            // PR refs also match against the stored repo `pr_url`; strip a
            // leading '#' so both "#123" and "123" find a `…/pull/123` link.
            let pr_pattern = format!("%{}%", needle.trim_start_matches('#').to_lowercase());
            let mut or_clauses = vec![
                format!("LOWER(COALESCE(title_override, title)) LIKE ${}", next),
                format!("LOWER(COALESCE(uploaded_by, '')) LIKE ${}", next + 1),
                format!("LOWER(COALESCE(project_path, '')) LIKE ${}", next + 2),
                // Cached normalization avoids decoding large bodies on every GIN recheck.
                // The exact LOWER value rejects separator-mapping collisions.
                format!(
                    "EXISTS (SELECT 1 FROM message_search_cache m \
                     WHERE m.session_id = sessions.id \
                     AND m.search_key LIKE TRANSLATE(${parameter}, ' :-/.', 'xqzvy') \
                     AND m.lower_text LIKE ${parameter})",
                    parameter = next + 3
                ),
                format!(
                    "EXISTS (SELECT 1 FROM session_repos r \
                     WHERE r.session_id = sessions.id AND LOWER(COALESCE(r.pr_url, '')) LIKE ${})",
                    next + 4
                ),
            ];
            for _ in 0..4 {
                binds.push(pattern.clone());
            }
            binds.push(pr_pattern);
            next += 5;

            // If the query is a PR reference (URL or owner/repo#N), also match
            // sessions linked to that PR via the dedicated `session_prs` table —
            // text/pr_url search alone never touches it. `number` is bound as
            // TEXT (cast for portability).
            if let Some((owner, repo, number)) = crate::github::parse_pr_query(needle) {
                or_clauses.push(format!(
                    "EXISTS (SELECT 1 FROM session_prs p \
                     WHERE p.session_id = sessions.id \
                       AND LOWER(p.owner) = ${} AND LOWER(p.repo) = ${} \
                       AND CAST(p.number AS TEXT) = ${})",
                    next,
                    next + 1,
                    next + 2
                ));
                binds.push(owner.to_lowercase());
                binds.push(repo.to_lowercase());
                binds.push(number.to_string());
                next += 3;
            }

            conditions.push(format!("({})", or_clauses.join(" OR ")));
        }
        if let Some(p) = project {
            conditions.push(format!("project_path = ${}", next));
            binds.push(p.to_string());
            next += 1;
        }

        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        // Rank the complete result set before callers display it. Effective
        // title equality is literal, so SQL wildcard characters carry no ranking authority.
        sql.push_str(" ORDER BY ");
        if let Some(needle) = q {
            sql.push_str(&format!("CASE WHEN LOWER(TRIM(COALESCE(title_override, title))) = ${next} THEN 0 ELSE 1 END, "));
            binds.push(needle.to_lowercase());
        }
        sql.push_str("is_live DESC, started_at DESC, uploaded_at DESC");

        let mut query = sqlx::query(&sql);
        for b in &binds {
            query = query.bind(b);
        }

        let rows = query.fetch_all(&self.pool).await?;
        let mut summaries: Vec<StoredSummary> = rows.iter().map(summary_from_row).collect();

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
            FROM sessions WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let share_rows = sqlx::query("SELECT email FROM session_shares WHERE session_id = $1")
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
        let shared_emails: Vec<String> = share_rows.iter().map(|r| r.get("email")).collect();

        let visibility: String = row.get("visibility");
        let owner_email: Option<String> = row.get("owner_email");
        // Whether THIS viewer was named by the publishing host as intended to
        // write into the session once their key is trusted there.
        let viewer_write_pending_key = match viewer {
            Some(email) => sqlx::query_scalar::<sqlx::Postgres, bool>(
                "SELECT write_pending_key FROM session_shares WHERE session_id = $1 AND email = $2",
            )
            .bind(id)
            .bind(email)
            .fetch_optional(&self.pool)
            .await?
            .unwrap_or(false),
            None => false,
        };
        if let Some(email) = viewer {
            let person_read = sqlx::query("SELECT 1 FROM live_session_shares WHERE live_id = $1 AND email = $2 AND communication_only = FALSE")
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
            FROM messages WHERE session_id = $1 ORDER BY ord ASC
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
            started_at: row.get("started_at"),
            ended_at: row.get("ended_at"),
            message_count: row.get::<i64, _>("message_count") as usize,
            skipped_lines: row.get::<i64, _>("skipped_lines") as usize,
            uploaded_by: row.get("uploaded_by"),
            uploaded_at: row.get("uploaded_at"),
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
        let row = sqlx::query("SELECT owner_email FROM sessions WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<Option<String>, _>("owner_email")))
    }

    async fn set_title_override(&self, id: &str, title: Option<&str>) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, true, id);
        let result = sqlx::query("UPDATE sessions SET title_override = $1 WHERE id = $2")
            .bind(title)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    async fn get_title_override(&self, id: &str) -> Result<Option<String>> {
        let row: Option<Option<String>> =
            sqlx::query_scalar("SELECT title_override FROM sessions WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.flatten())
    }

    async fn delete_session(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, true, id);
        // `session_prs` no longer carries an ON DELETE CASCADE FK (relaxed in
        // migration 0015 so PRs can be linked to a live session), so delete its
        // rows explicitly. Other child rows (messages, shares, repos, per-user
        // paths, pr_comments) still cascade via their FK.
        sqlx::query("DELETE FROM session_prs WHERE session_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // `session_linear_docs` likewise has no cascade FK (see migration 0020).
        sqlx::query("DELETE FROM session_linear_docs WHERE session_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE id = $1")
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
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO users (email, display_name, created_at, last_seen_at)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT(email) DO UPDATE SET
                display_name = COALESCE(excluded.display_name, users.display_name),
                last_seen_at = excluded.last_seen_at
            "#,
        )
        .bind(email)
        .bind(display_name)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query(
            "SELECT email, display_name, created_at, last_seen_at FROM users WHERE email = $1",
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
        sqlx::query("DELETE FROM session_shares WHERE email = $1")
            .bind(email)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM live_session_shares WHERE email = $1")
            .bind(email)
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query("DELETE FROM users WHERE email = $1")
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
                created_at: r.get("created_at"),
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
            .bind(&c.user_email).bind(&key.kid).bind(&key.public_key).bind(&key.label).bind(key.created_at).execute(&mut *tx).await?;
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
                    WHERE LOWER(email) LIKE $1
                       OR LOWER(COALESCE(display_name, '')) LIKE $1
                    ORDER BY email ASC
                    LIMIT $2
                    "#,
                )
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
                    LIMIT $1
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
             FROM users WHERE email = $1",
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
        let res = sqlx::query("UPDATE users SET password_hash = $1 WHERE email = $2")
            .bind(password_hash)
            .bind(email)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    async fn get_user_auth_version(&self, email: &str) -> Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT auth_version FROM users WHERE email = $1")
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
        let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM users WHERE email = $1")
            .bind(user_email)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::NotFound);
        }

        let now = Utc::now();
        sqlx::query(
            "UPDATE password_reset_tokens SET used_at = $1 \
             WHERE user_email = $2 AND used_at IS NULL",
        )
        .bind(now)
        .bind(user_email)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO password_reset_tokens \
             (token_id, user_email, secret_hash, created_by, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(token_id)
        .bind(user_email)
        .bind(secret_hash)
        .bind(created_by)
        .bind(now)
        .bind(expires_at)
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
             WHERE token_id = $1 AND used_at IS NULL AND expires_at > $2",
        )
        .bind(token_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| PasswordResetToken {
            token_id: r.get("token_id"),
            user_email: r.get("user_email"),
            secret_hash: r.get("secret_hash"),
            expires_at: r.get("expires_at"),
        }))
    }

    async fn consume_password_reset_token(
        &self,
        token_id: &str,
        password_hash: &str,
        used_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query(
            "UPDATE password_reset_tokens SET used_at = $1 \
             WHERE token_id = $2 AND used_at IS NULL AND expires_at > $1",
        )
        .bind(used_at)
        .bind(token_id)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(false);
        }

        let changed = sqlx::query(
            "UPDATE users SET password_hash = $1, auth_version = auth_version + 1 WHERE email = \
             (SELECT user_email FROM password_reset_tokens WHERE token_id = $2)",
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
        let row: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM user_admins WHERE user_email = $1")
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
        let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM users WHERE email = $1")
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::NotFound);
        }

        if is_admin {
            sqlx::query(
                "INSERT INTO user_admins (user_email, granted_by, granted_at) \
                 VALUES ($1, $2, $3) \
                 ON CONFLICT(user_email) DO UPDATE SET \
                    granted_by = excluded.granted_by, \
                    granted_at = excluded.granted_at",
            )
            .bind(email)
            .bind(granted_by)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query("DELETE FROM user_admins WHERE user_email = $1")
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
        super::publication::reject_managed!(tx, true, session_id);

        let existed: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
        if existed.is_none() {
            return Err(AppError::NotFound);
        }

        sqlx::query("UPDATE sessions SET visibility = $1 WHERE id = $2")
            .bind(visibility)
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM session_shares WHERE session_id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        if visibility == VISIBILITY_RESTRICTED {
            for email in super::dedup_nonempty(emails) {
                sqlx::query(
                    "INSERT INTO session_shares (session_id, email) VALUES ($1, $2) \
                     ON CONFLICT DO NOTHING",
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
        let row = sqlx::query("SELECT visibility FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let visibility: String = row.get("visibility");

        let share_rows = sqlx::query("SELECT email FROM session_shares WHERE session_id = $1")
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
        super::publication::reject_managed!(tx, true, session_id);
        for link in links {
            let repo = link.repo.trim();
            let branch = link.branch.trim();
            if repo.is_empty() || branch.is_empty() {
                continue;
            }
            sqlx::query(
                r#"
                INSERT INTO session_repos (session_id, repo, branch, remote_url, pr_url, is_container)
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT(session_id, repo, branch) DO UPDATE SET
                    remote_url   = COALESCE(excluded.remote_url, session_repos.remote_url),
                    pr_url       = COALESCE(excluded.pr_url, session_repos.pr_url),
                    -- Sticky-true: a later flag-less push (older CLI) must not
                    -- clear a container flag set by an earlier upload.
                    is_container = (session_repos.is_container OR excluded.is_container)
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

            if let (Some(user), Some(path)) = (uploader, link.local_path.as_deref()) {
                let path = path.trim();
                if !user.is_empty() && !path.is_empty() {
                    sqlx::query(
                        r#"
                        INSERT INTO session_repo_paths
                            (session_id, repo, branch, user_email, local_path)
                        VALUES ($1, $2, $3, $4, $5)
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
        super::publication::reject_managed!(tx, true, session_id);

        let existed: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
        if existed.is_none() {
            return Err(AppError::NotFound);
        }

        let keep = super::distinct_repo_links(links);

        let existing: Vec<(String, String)> =
            sqlx::query_as("SELECT repo, branch FROM session_repos WHERE session_id = $1")
                .bind(session_id)
                .fetch_all(&mut *tx)
                .await?;
        for (repo, branch) in &existing {
            let still = keep
                .iter()
                .any(|l| l.repo.trim() == repo && l.branch.trim() == branch);
            if !still {
                sqlx::query(
                    "DELETE FROM session_repos WHERE session_id = $1 AND repo = $2 AND branch = $3",
                )
                .bind(session_id)
                .bind(repo)
                .bind(branch)
                .execute(&mut *tx)
                .await?;
            }
        }

        for l in &keep {
            sqlx::query(
                r#"
                INSERT INTO session_repos (session_id, repo, branch, remote_url, pr_url)
                VALUES ($1, $2, $3, $4, $5)
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
        let placeholders = (1..=session_ids.len())
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ");

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
            FROM session_summaries WHERE session_id = $1
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
            generated_at: row.get("generated_at"),
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
        super::publication::fence_source!(tx, true, session_id, source);
        sqlx::query(
            r#"
            INSERT INTO session_summaries
                (session_id, intent, invariants, summary, model, generated_by, generated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
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
        .bind(summary.generated_at)
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
        sqlx::query(
            r#"
            INSERT INTO projects (id, name, description, owner_email, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(&id)
        .bind(name)
        .bind(description)
        .bind(owner_email)
        .bind(now)
        .bind(now)
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
        let count_filter = if viewer.is_some() {
            session_visibility_filter("s", 1)
        } else {
            String::new()
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
        if let Some(email) = viewer {
            query = query.bind(email);
        }
        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows.iter().map(project_from_row).collect())
    }

    async fn get_project(&self, viewer: Option<&str>, id: &str) -> Result<Option<ProjectDetail>> {
        // session_count: visibility-filtered for a known viewer (same predicate
        // as `list_sessions`, scoped to the `s` alias), all members otherwise.
        let count_filter = if viewer.is_some() {
            session_visibility_filter("s", 2)
        } else {
            String::new()
        };
        let count_sql = format!(
            r#"
            SELECT p.id, p.name, p.description, p.owner_email, p.created_at, p.updated_at,
                   (SELECT COUNT(*) FROM sessions s
                       WHERE s.project_id = p.id{count_filter}) AS session_count
            FROM projects p WHERE p.id = $1
            "#
        );
        let mut count_query = sqlx::query(&count_sql).bind(id);
        if let Some(email) = viewer {
            count_query = count_query.bind(email);
        }
        let row = count_query.fetch_optional(&self.pool).await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let project = project_from_row(&row);

        // Member summaries: same visibility filter as `list_sessions` (the
        // session list must not leak restricted sessions through the project
        // detail view either).
        let member_filter = if viewer.is_some() {
            session_visibility_filter("sessions", 2)
        } else {
            String::new()
        };
        let member_sql = format!(
            r#"
            SELECT id, COALESCE(title_override, title) AS title, project_path,
                   agent, git_branch, message_count,
                   started_at, ended_at, uploaded_by, uploaded_at, source_host,
                   owner_email, visibility
            FROM sessions WHERE project_id = $1{member_filter}
            ORDER BY started_at DESC, uploaded_at DESC
            "#
        );
        let mut member_query = sqlx::query(&member_sql).bind(id);
        if let Some(email) = viewer {
            member_query = member_query.bind(email);
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
        let row = sqlx::query("SELECT owner_email FROM projects WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<Option<String>, _>("owner_email")))
    }

    async fn update_project(&self, id: &str, name: &str, description: Option<&str>) -> Result<()> {
        // `description` is always set to the provided value (passing `None`
        // clears it) — the edit form always sends both fields.
        let res = sqlx::query(
            "UPDATE projects SET name = $1, description = $2, updated_at = $3 WHERE id = $4",
        )
        .bind(name)
        .bind(description)
        .bind(Utc::now())
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
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_session_project(&self, session_id: &str, project_id: Option<&str>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, true, session_id);

        let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::NotFound);
        }

        if let Some(pid) = project_id {
            let proj: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM projects WHERE id = $1")
                .bind(pid)
                .fetch_optional(&mut *tx)
                .await?;
            if proj.is_none() {
                return Err(AppError::BadRequest(format!("unknown project {pid:?}")));
            }
        }

        sqlx::query("UPDATE sessions SET project_id = $1 WHERE id = $2")
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
                super::publication::fence_source!(tx, true, id, source);
            }
            None => source.without_publication()?,
        }

        // Validate the parent exists (the CHECK only guards exactly-one-ness).
        let parent_exists: Option<(i32,)> = match parent {
            ChatParent::Session(id) => {
                sqlx::query_as("SELECT 1 FROM sessions WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?
            }
            ChatParent::Project(id) => {
                sqlx::query_as("SELECT 1 FROM projects WHERE id = $1")
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
        sqlx::query(
            r#"
            INSERT INTO chats
                (id, session_id, project_id, owner_email, title, shared,
                 created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, FALSE, $6, $7)
            "#,
        )
        .bind(&id)
        .bind(session_id)
        .bind(project_id)
        .bind(owner_email)
        .bind(title)
        .bind(now)
        .bind(now)
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
             created_at, updated_at FROM chats WHERE {column} = $1"
        );
        if viewer.is_some() {
            sql.push_str(" AND (owner_email = $2 OR shared = TRUE)");
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
             created_at, updated_at FROM chats WHERE id = $1",
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
             FROM chat_messages WHERE chat_id = $1 ORDER BY created_at ASC, id ASC",
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
            sqlx::query_as("SELECT session_id FROM chats WHERE id = $1")
                .bind(chat_id)
                .fetch_optional(&mut *tx)
                .await?;
        match parent {
            None => return Err(AppError::NotFound),
            Some((Some(session_id),)) => {
                super::publication::fence_source!(tx, true, session_id.as_str(), source);
            }
            Some((None,)) => source.without_publication()?,
        }
        // The chat may have gone with its session before the lock was taken.
        let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM chats WHERE id = $1")
            .bind(chat_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::NotFound);
        }

        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO chat_messages (id, chat_id, role, content, created_at)
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(&id)
        .bind(chat_id)
        .bind(role)
        .bind(content)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query("UPDATE chats SET updated_at = $1 WHERE id = $2")
            .bind(now)
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
        let now = Utc::now();
        let res = match title {
            Some(t) => {
                sqlx::query(
                    "UPDATE chats SET shared = $1, title = $2, updated_at = $3 WHERE id = $4",
                )
                .bind(shared)
                .bind(t)
                .bind(now)
                .bind(id)
                .execute(&self.pool)
                .await?
            }
            None => {
                sqlx::query("UPDATE chats SET shared = $1, updated_at = $2 WHERE id = $3")
                    .bind(shared)
                    .bind(now)
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
        sqlx::query("DELETE FROM chats WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn create_live_session(&self, ls: &NewLiveSession) -> Result<LiveSession> {
        let now = Utc::now();
        // Agent-derived metadata reaches text columns here too — a NUL in the
        // title (it is a summary of the transcript) would fail registration.
        let title = scrub_opt(ls.title.as_deref());
        let project_path = scrub_opt(ls.project_path.as_deref());
        let cwd = scrub_opt(ls.cwd.as_deref());
        let git_branch = scrub_opt(ls.git_branch.as_deref());
        let remote_url = scrub_opt(ls.remote_url.as_deref());
        let source_host = scrub_opt(ls.source_host.as_deref());

        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, true, ls.id.as_str(), ls.session_id.as_str());
        // Idempotent on `id` (== Claude session_id): re-registering the same
        // session resumes it instead of violating the PK. The original
        // `started_at` and `id` are preserved; `status` flips back to 'live',
        // `ended_at` clears, and the metadata is refreshed from the new values.
        sqlx::query(
            r#"
            INSERT INTO live_sessions
                (id, session_id, agent, title, project_path, cwd, git_branch,
                 owner_email, source_host, remote_url, status, mode, started_at, last_event_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'live', $11, $12, $13)
            ON CONFLICT (id) DO UPDATE SET
                status        = 'live',
                agent         = EXCLUDED.agent,
                mode          = EXCLUDED.mode,
                last_event_at = EXCLUDED.last_event_at,
                ended_at      = NULL,
                title         = EXCLUDED.title,
                project_path  = EXCLUDED.project_path,
                cwd           = EXCLUDED.cwd,
                git_branch    = EXCLUDED.git_branch,
                source_host   = EXCLUDED.source_host,
                remote_url    = EXCLUDED.remote_url,
                owner_email   = EXCLUDED.owner_email
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
        .bind(now)
        .bind(now)
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
            VALUES ($1, $2, $3, $4, $5, $6, $7, 0, 0, $8, $9, $10, $11, 'restricted')
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
        .bind(now)
        .bind(ls.owner_email.clone().unwrap_or_default())
        .bind(now)
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

        let dup: Option<(i32,)> =
            sqlx::query_as("SELECT 1 FROM live_messages WHERE live_id = $1 AND message_uuid = $2")
                .bind(live_id)
                .bind(&msg.uuid)
                .fetch_optional(&mut *tx)
                .await?;
        if dup.is_some() {
            return Ok(None);
        }

        let next_seq: i64 = sqlx::query_as::<_, (Option<i64>,)>(
            "SELECT MAX(seq) FROM live_messages WHERE live_id = $1",
        )
        .bind(live_id)
        .fetch_one(&mut *tx)
        .await?
        .0
        .map_or(0, |m| m + 1);

        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let payload = serde_json::to_string(msg)?;

        let inserted = sqlx::query(
            r#"
            INSERT INTO live_messages (id, live_id, seq, message_uuid, payload, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (live_id, message_uuid) DO NOTHING
            "#,
        )
        .bind(&id)
        .bind(live_id)
        .bind(next_seq)
        .bind(&msg.uuid)
        .bind(&payload)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        // Lost the de-dup race: a concurrent append already stored this
        // message_uuid, so the pre-check SELECT above (which can't see the
        // other transaction's uncommitted row under READ COMMITTED) missed it.
        // Treat as a no-op re-push rather than letting the UNIQUE violation 500.
        if inserted == 0 {
            return Ok(None);
        }

        sqlx::query("UPDATE live_sessions SET last_event_at = $1 WHERE id = $2")
            .bind(now)
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
             FROM live_messages WHERE live_id = $1 ORDER BY seq ASC",
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
             FROM live_messages WHERE live_id = $1 AND seq > $2 ORDER BY seq ASC",
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
        // Sequential `$n` placeholders; bind in the same order as appended.
        let mut conditions: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        let mut n = 0;
        if let Some(s) = status {
            n += 1;
            conditions.push(format!("status = ${n}"));
            binds.push(s.to_string());
        }
        // Visibility: author OR sharee. Live sessions are always private.
        if let Some(email) = viewer {
            let p1 = n + 1;
            let p2 = n + 2;
            conditions.push(format!(
                "(owner_email = ${p1} OR EXISTS (SELECT 1 FROM live_session_shares ls \
                 WHERE ls.live_id = live_sessions.id AND ls.communication_only = FALSE AND ls.email = ${p2}))"
            ));
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
            }
        }

        let now = Utc::now();
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM live_session_shares WHERE live_id = $1")
            .bind(live_id)
            .execute(&mut *tx)
            .await?;

        for email in &order {
            let (can_read, can_write, can_communicate) = by_email[email];
            sqlx::query(
                "INSERT INTO live_session_shares \
                 (live_id, email, can_write, communication_only, can_communicate, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (live_id, email) DO UPDATE SET \
                 can_write = EXCLUDED.can_write, \
                 communication_only = EXCLUDED.communication_only, \
                 can_communicate = EXCLUDED.can_communicate",
            )
            .bind(live_id)
            .bind(email)
            .bind(can_write)
            .bind(!can_read)
            .bind(can_communicate)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn get_live_shares(&self, live_id: &str) -> Result<Vec<LiveShare>> {
        let rows = sqlx::query(
            "SELECT email, can_write, communication_only, can_communicate FROM live_session_shares \
             WHERE live_id = $1 ORDER BY email ASC",
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
        let row = sqlx::query("SELECT owner_email FROM sessions WHERE id = $1")
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
             graduated_session_id FROM live_sessions WHERE id = $1",
        )
        .bind(live_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(live_session_from_row))
    }

    async fn get_live_session_owner(&self, live_id: &str) -> Result<Option<Option<String>>> {
        let row = sqlx::query("SELECT owner_email FROM sessions WHERE id = $1")
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
             SET last_event_at = $1, \
                 status = CASE WHEN status = $2 THEN $3 ELSE status END \
             WHERE id = $4 AND status IN ($2, $3)",
        )
        .bind(Utc::now())
        .bind(LIVE_STATUS_ABANDONED)
        .bind(LIVE_STATUS_LIVE)
        .bind(live_id)
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
        let res = sqlx::query("UPDATE live_sessions SET title = $1 WHERE id = $2 AND status = $3")
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
        sqlx::query("UPDATE sessions SET title = $1 WHERE id = $2")
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
            "UPDATE live_sessions SET agent_session_id = $1 WHERE id = $2 AND status = $3",
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
        sqlx::query("UPDATE sessions SET agent_session_id = $1 WHERE id = $2")
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
             SET status = $1, graduated_session_id = $2, ended_at = $3 WHERE id = $4",
        )
        .bind(LIVE_STATUS_GRADUATED)
        .bind(&session.id)
        .bind(Utc::now())
        .bind(live_id)
        .execute(&self.pool)
        .await?;

        Ok(Some(session.id))
    }

    async fn mark_abandoned_live_sessions(&self, older_than: DateTime<Utc>) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE live_sessions SET status = 'abandoned' \
             WHERE status = $1 AND last_event_at < $2",
        )
        .bind(LIVE_STATUS_LIVE)
        .bind(older_than)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    async fn session_store(&self) -> Result<SessionBackend> {
        let store = tower_sessions_sqlx_store::PostgresStore::new(self.pool.clone());
        store
            .migrate()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
        Ok(SessionBackend::Postgres(store))
    }

    async fn link_session_pr(&self, session_id: &str, pr: &SessionPr) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        super::publication::reject_managed!(tx, true, session_id);
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO session_prs (session_id, url, owner, repo, number, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
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
             WHERE session_id = $1 ORDER BY created_at ASC",
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
                status_merged: r.get("status_merged"),
                status_draft: r.get("status_draft"),
                status_title: r.get("status_title"),
                status_review_decision: r.get("status_review_decision"),
                status_fetched_at: r.get("status_fetched_at"),
                detected: r.get("detected"),
            })
            .collect())
    }

    async fn list_open_pr_watch_targets(&self, owner: Option<&str>) -> Result<Vec<PrWatchTarget>> {
        // Scoped to CURRENTLY LIVE sessions (`live_sessions.status = 'live'`):
        // the proactive timer only polls sessions in progress, so a graduated /
        // abandoned session — including any stale or phantom link it picked up —
        // is never re-fetched forever. Open = not merged AND (state OPEN or
        // never synced/unknown). `$2 IS NULL` short-circuits the owner filter
        // when auth is off (owner = None), else it scopes to that user's own
        // live sessions.
        let rows = sqlx::query(
            "SELECT sp.session_id, sp.owner, sp.repo, sp.number, sp.url \
             FROM session_prs sp \
             JOIN live_sessions ls ON ls.id = sp.session_id AND ls.status = $1 \
             WHERE COALESCE(sp.status_merged, false) = false \
               AND (sp.status_state IS NULL OR UPPER(sp.status_state) = 'OPEN') \
               AND ($2::text IS NULL OR ls.owner_email = $2) \
             ORDER BY sp.session_id ASC, sp.number ASC",
        )
        .bind(LIVE_STATUS_LIVE)
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
        super::publication::reject_managed!(tx, true, session_id);
        let now = Utc::now();
        let mut inserted = 0u64;
        for pr in prs {
            // detected = true; never clobbers/downgrades an existing row.
            let res = sqlx::query(
                r#"
                INSERT INTO session_prs (session_id, url, owner, repo, number, created_at, detected)
                VALUES ($1, $2, $3, $4, $5, $6, true)
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
            .await;
            // Best-effort: the insert now succeeds even for a session id with no
            // `sessions` row (e.g. a live, ungraduated session) since the FK was
            // dropped in migration 0015. Any residual error (e.g. a transient DB
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
        let row: Option<(Option<DateTime<Utc>>,)> =
            sqlx::query_as("SELECT pr_scanned_at FROM sessions WHERE id = $1")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(ts,)| ts))
    }

    async fn set_pr_scanned_at(&self, session_id: &str, ts: DateTime<Utc>) -> Result<()> {
        sqlx::query("UPDATE sessions SET pr_scanned_at = $1 WHERE id = $2")
            .bind(ts)
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
             WHERE session_id = $1 AND owner = $2 AND repo = $3 AND number = $4",
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
        super::publication::reject_managed!(tx, true, session_id);
        let now = Utc::now();
        let mut inserted = 0u64;
        for doc in docs {
            let res = sqlx::query(
                r#"
                INSERT INTO session_linear_docs
                    (session_id, url, workspace, kind, identifier, created_at)
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT(session_id, url) DO NOTHING
                "#,
            )
            .bind(session_id)
            .bind(&doc.url)
            .bind(&doc.workspace)
            .bind(&doc.kind)
            .bind(&doc.identifier)
            .bind(now)
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
             WHERE session_id = $1 ORDER BY created_at ASC, url ASC",
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
        let res = sqlx::query("DELETE FROM session_linear_docs WHERE session_id = $1 AND url = $2")
            .bind(session_id)
            .bind(url)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn linear_scanned_at(&self, session_id: &str) -> Result<Option<DateTime<Utc>>> {
        let row: Option<(Option<DateTime<Utc>>,)> =
            sqlx::query_as("SELECT linear_scanned_at FROM sessions WHERE id = $1")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(ts,)| ts))
    }

    async fn set_linear_scanned_at(&self, session_id: &str, ts: DateTime<Utc>) -> Result<()> {
        sqlx::query("UPDATE sessions SET linear_scanned_at = $1 WHERE id = $2")
            .bind(ts)
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
        super::publication::fence_source!(tx, true, session_id, source);
        let res = sqlx::query(
            "UPDATE session_prs SET \
                status_state = $1, status_merged = $2, status_draft = $3, \
                status_title = $4, status_fetched_at = $5, status_review_decision = COALESCE($10, status_review_decision) \
             WHERE session_id = $6 AND owner = $7 AND repo = $8 AND number = $9",
        )
        .bind(&status.state)
        .bind(status.merged)
        .bind(status.draft)
        .bind(&status.title)
        .bind(fetched_at)
        .bind(session_id)
        .bind(owner)
        .bind(repo)
        .bind(number)
        .bind(&status.review_decision)
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
        super::publication::fence_source!(tx, true, session_id, source);
        if matches!(source, SourceContext::Published(_)) {
            let linked: Option<(i32,)> = sqlx::query_as(
                "SELECT 1 FROM session_prs \
                 WHERE session_id = $1 AND owner = $2 AND repo = $3 AND number = $4",
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
            "INSERT INTO session_pr_comment_revisions (session_id, revision) VALUES ($1, 1) \
             ON CONFLICT(session_id) DO UPDATE SET revision = session_pr_comment_revisions.revision + 1 \
             RETURNING revision",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;
        let now = Utc::now();
        let mut changed = false;
        for c in comments {
            let updated = sqlx::query(
                r#"
                INSERT INTO session_pr_comments
                    (session_id, owner, repo, number, comment_id,
                     kind, author, body, html_url, created_at,
                     path, line, diff_hunk, resolved, ingested_at, revision)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
                ON CONFLICT(session_id, owner, repo, number, comment_id) DO UPDATE SET
                    kind = EXCLUDED.kind,
                    author = EXCLUDED.author,
                    body = EXCLUDED.body,
                    html_url = EXCLUDED.html_url,
                    created_at = EXCLUDED.created_at,
                    path = EXCLUDED.path,
                    line = EXCLUDED.line,
                    diff_hunk = EXCLUDED.diff_hunk,
                    resolved = EXCLUDED.resolved,
                    ingested_at = EXCLUDED.ingested_at,
                    revision = EXCLUDED.revision
                WHERE session_pr_comments.kind IS DISTINCT FROM EXCLUDED.kind
                   OR session_pr_comments.author IS DISTINCT FROM EXCLUDED.author
                   OR session_pr_comments.body IS DISTINCT FROM EXCLUDED.body
                   OR session_pr_comments.html_url IS DISTINCT FROM EXCLUDED.html_url
                   OR session_pr_comments.created_at IS DISTINCT FROM EXCLUDED.created_at
                   OR session_pr_comments.path IS DISTINCT FROM EXCLUDED.path
                   OR session_pr_comments.line IS DISTINCT FROM EXCLUDED.line
                   OR session_pr_comments.diff_hunk IS DISTINCT FROM EXCLUDED.diff_hunk
                   OR session_pr_comments.resolved IS DISTINCT FROM EXCLUDED.resolved
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
            .bind(now)
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
             WHERE session_id = $1 ORDER BY created_at ASC",
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
             WHERE session_id = $1 AND revision > $2 ORDER BY revision ASC",
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
                FROM session_pr_comments WHERE session_id = $1 \
                    AND (kind <> 'review_comment' OR resolved IS DISTINCT FROM true) \
                    AND NOT (kind IN ('review', 'issue') \
                        AND lower(trim(author)) IN ('coderabbitai', 'coderabbitai[bot]')) \
             ) SELECT c.owner, c.repo, c.number, c.comment_id, c.kind, c.author, \
                    left(c.body, 240) AS body, c.html_url, c.created_at, \
                     c.path, c.line, NULL::text AS diff_hunk, c.resolved, c.ingested_at, c.revision \
             FROM ranked r JOIN session_pr_comments c \
               ON c.session_id = $1 AND c.owner = r.owner AND c.repo = r.repo \
               AND c.number = r.number AND c.comment_id = r.comment_id \
             WHERE r.rank <= 4 ORDER BY c.created_at ASC, c.comment_id ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(stored_pr_comment).collect())
    }

    async fn get_container_paths(&self, email: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT path FROM user_container_paths WHERE user_email = $1 ORDER BY path ASC",
        )
        .bind(email)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("path")).collect())
    }

    async fn set_container_paths(&self, email: &str, paths: &[String]) -> Result<()> {
        let normalized = super::normalize_container_paths(paths);

        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM user_container_paths WHERE user_email = $1")
            .bind(email)
            .execute(&mut *tx)
            .await?;

        for path in &normalized {
            sqlx::query(
                "INSERT INTO user_container_paths (user_email, path) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
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
        let row = sqlx::query("SELECT default_agent FROM user_settings WHERE user_email = $1")
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
            "INSERT INTO user_settings (user_email, default_agent) VALUES ($1, $2) \
             ON CONFLICT (user_email) DO UPDATE SET default_agent = EXCLUDED.default_agent",
        )
        .bind(email)
        .bind(agent.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_inline_code_colors(&self, email: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT inline_code_colors FROM user_settings WHERE user_email = $1")
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
             FROM user_settings WHERE user_email = $1",
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
             FROM user_settings WHERE user_email = $1",
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
            "INSERT INTO user_settings (user_email, inline_code_colors) VALUES ($1, $2) \
             ON CONFLICT (user_email) DO UPDATE SET inline_code_colors = EXCLUDED.inline_code_colors",
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
             VALUES ($1, $2, $3) \
             ON CONFLICT (user_email) DO UPDATE SET \
                 codex_sandbox_mode = EXCLUDED.codex_sandbox_mode, \
                 codex_approval_policy = EXCLUDED.codex_approval_policy",
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
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (user_email) DO UPDATE SET \
                 opencode_auto = EXCLUDED.opencode_auto, \
                 adk_reopen = EXCLUDED.adk_reopen, \
                 adk_statusline = EXCLUDED.adk_statusline, \
                 default_launcher = EXCLUDED.default_launcher",
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

/// ` AND (...)` clause restricting `alias` (a `sessions` row) to what the
/// viewer bound at `$placeholder` may read — the `list_sessions` predicate.
fn session_visibility_filter(alias: &str, placeholder: usize) -> String {
    format!(
        " AND ({alias}.visibility = 'organization' OR {alias}.owner_email = ${placeholder} \
         OR EXISTS (SELECT 1 FROM session_shares ss \
            WHERE ss.session_id = {alias}.id AND ss.email = ${placeholder}) \
         OR EXISTS (SELECT 1 FROM live_session_shares lss \
            WHERE lss.live_id = {alias}.id AND lss.communication_only = FALSE \
            AND lss.email = ${placeholder}))"
    )
}

fn project_from_row(row: &PgRow) -> Project {
    Project {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        owner_email: row.get("owner_email"),
        session_count: row.get::<i64, _>("session_count") as usize,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn chat_from_row(row: &PgRow) -> Chat {
    Chat {
        id: row.get("id"),
        session_id: row.get("session_id"),
        project_id: row.get("project_id"),
        owner_email: row.get("owner_email"),
        title: row.get("title"),
        shared: row.get("shared"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn chat_message_from_row(row: &PgRow) -> ChatMessage {
    ChatMessage {
        id: row.get("id"),
        chat_id: row.get("chat_id"),
        role: row.get("role"),
        content: row.get("content"),
        created_at: row.get("created_at"),
    }
}

fn summary_from_row(row: &PgRow) -> StoredSummary {
    StoredSummary {
        id: row.get("id"),
        agent: super::parse_agent(row.try_get::<String, _>("agent").ok().as_deref()),
        title: row.get("title"),
        project_path: row.get("project_path"),
        git_branch: row.get("git_branch"),
        message_count: row.get::<i64, _>("message_count") as usize,
        started_at: row.get("started_at"),
        ended_at: row.get("ended_at"),
        uploaded_by: row.get("uploaded_by"),
        uploaded_at: row.get("uploaded_at"),
        source_host: row.get("source_host"),
        owner_email: row.get("owner_email"),
        visibility: row.get("visibility"),
        // `is_live` (a bool) is only present on the list query; tolerate its
        // absence on other SELECTs that feed this mapper.
        is_live: row.try_get::<bool, _>("is_live").unwrap_or(false),
        repos: Vec::new(),
    }
}

fn user_from_row(row: &PgRow) -> User {
    User {
        email: row.get("email"),
        display_name: row.get("display_name"),
        created_at: row.get("created_at"),
        last_seen_at: row.get("last_seen_at"),
    }
}

fn live_session_from_row(row: &PgRow) -> LiveSession {
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
        started_at: row.get("started_at"),
        last_event_at: row.get("last_event_at"),
        ended_at: row.get("ended_at"),
        graduated_session_id: row.get("graduated_session_id"),
    }
}

fn live_share_from_row(row: &PgRow) -> LiveShare {
    LiveShare {
        email: row.get("email"),
        can_write: row.get("can_write"),
        can_read: !row.get::<bool, _>("communication_only"),
        can_communicate: row.get("can_communicate"),
    }
}

fn live_message_from_row(row: &PgRow) -> Result<LiveStoredMessage> {
    let payload: String = row.get("payload");
    let message: core::Message = serde_json::from_str(&payload)?;
    Ok(LiveStoredMessage {
        seq: row.get("seq"),
        message_uuid: row.get("message_uuid"),
        message,
        created_at: row.get("created_at"),
    })
}

fn message_from_row(row: &PgRow) -> Result<core::Message> {
    let role_str: String = row.get("role");
    let kind_str: String = row.get("kind");
    let role: core::Role = serde_json::from_value(serde_json::Value::String(role_str))?;
    let kind: core::MessageKind = serde_json::from_value(serde_json::Value::String(kind_str))?;

    // raw/usage are stored as JSONB.
    let raw: serde_json::Value = row.get("raw");
    let usage = row
        .get::<Option<serde_json::Value>, _>("usage")
        .map(serde_json::from_value::<core::Usage>)
        .transpose()?;

    let blocks = core::blocks_from_raw_message(&raw);

    Ok(core::Message {
        uuid: row.get("uuid"),
        parent_uuid: row.get("parent_uuid"),
        role,
        kind,
        model: row.get("model"),
        is_sidechain: row.get("is_sidechain"),
        timestamp: row.get("timestamp"),
        blocks,
        text: row.get("text"),
        raw,
        usage,
    })
}
