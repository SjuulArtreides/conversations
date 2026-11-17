//! Persistence for explicit, host-signed publication of a session.
//!
//! The `publications` row is the ledger: publishing host key, central owner,
//! highest share revision, state and chunk cursor. It has no foreign key to
//! `sessions`, so it survives unshare as the tombstone. The published content
//! lives in the ordinary `sessions`/`messages` rows (so existing read, search,
//! summary and PR features apply) and is deleted as one transaction on unshare
//! or on a share at a newer revision.
//!
//! A managed session id is reserved for this protocol. The legacy upload, live
//! registration, delete and sharing writers, and the metadata and link writers
//! (title, repos, project, PR/Linear links), call [`reject_managed!`] inside
//! their own transaction, after taking the same per-id lock as the publication
//! writer, so neither an old route nor a delayed request can adopt, overwrite or
//! resurrect a managed session. The derived writers (summary, session chat, PR
//! status/comment caches) instead carry the [`SourceContext`] their job read
//! and call [`fence_source!`]: a published result lands only while the ledger
//! still holds exactly the snapshot it was derived from.
//!
//! Claiming an id: the FIRST operation for a session id must be a share. The
//! first accepted share binds the id to that host and owner, and it is refused
//! when the id already has any central session, live, PR or Linear row:
//! existing data is never adopted or deleted by this protocol. An unshare with
//! no ledger row is acknowledged as deleted without creating a row, so a
//! stranger's unshare cannot plant a tombstone for an id it never published.

use async_trait::async_trait;
use chrono::Utc;
use conversations_core::publication::{
    Audience, PublicationAck, PublicationAction, PublicationChunk, PublicationOp,
    PublicationSnapshot, PublicationState,
};
use conversations_core::Session;
use sqlx::Row;

use super::nul::scrub_session;
use super::{PostgresRepo, SessionLinearDoc, SessionPr, SqliteRepo};
use crate::error::{AppError, Result};

/// Refusal for a legacy write to a session id owned by the publication protocol.
pub const MANAGED: &str =
    "this session is managed by host publication; the legacy route cannot change it";

/// Refusal for a derived result whose publication snapshot is no longer current.
pub const STALE_SOURCE: &str = "the published session changed after this result's context was read";

/// What a derived job read its source context from, captured before that read
/// and carried unchanged to the job's final write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceContext {
    /// A session with no publication ledger row (legacy upload or live).
    Unmanaged,
    /// An active publication at exactly this snapshot.
    Published(PublicationSnapshot),
}

impl SourceContext {
    /// Decide a final write against the ledger row read under the session lock:
    /// `(state, max_rev, next_seq)`, or `None` when there is none.
    pub(crate) fn admit(self, ledger: Option<(String, i64, i64)>) -> Result<()> {
        match (self, ledger) {
            (SourceContext::Unmanaged, None) => Ok(()),
            (SourceContext::Unmanaged, Some(_)) => Err(conflict(MANAGED)),
            (SourceContext::Published(snapshot), Some((state, rev, next_seq)))
                if state == "active"
                    && rev == snapshot.rev as i64
                    && next_seq == snapshot.next_seq as i64 =>
            {
                Ok(())
            }
            (SourceContext::Published(_), _) => Err(conflict(STALE_SOURCE)),
        }
    }

    /// For a write with no session parent (a project chat): only an
    /// unmanaged context applies.
    pub(crate) fn without_publication(self) -> Result<()> {
        match self {
            SourceContext::Unmanaged => Ok(()),
            SourceContext::Published(_) => Err(AppError::BadRequest(
                "a project has no publication revision".into(),
            )),
        }
    }
}

/// A publication operation whose signature, host identity, owner and bounds the
/// API layer has verified. Only `crate::publication` constructs one.
pub struct PublicationWrite {
    pub(crate) op: PublicationOp,
    pub(crate) host_public_key: String,
    /// base64url SHA-256 of the exact signed payload bytes.
    pub(crate) op_sha256: String,
    pub(crate) chunk: Option<VerifiedChunk>,
}

pub(crate) struct VerifiedChunk {
    pub(crate) chunk: PublicationChunk,
    pub(crate) sha256: String,
    /// Parallel to `chunk.messages`.
    pub(crate) refs: Vec<MessageRefs>,
}

/// The links one published message carries.
pub(crate) struct MessageRefs {
    pub(crate) prs: Vec<SessionPr>,
    pub(crate) linear_docs: Vec<SessionLinearDoc>,
}

#[async_trait]
pub trait PublicationRepository: Send + Sync {
    /// Apply one verified operation atomically. Nothing is written unless the
    /// whole operation is accepted.
    async fn apply_publication(&self, write: &PublicationWrite) -> Result<PublicationAck>;

    /// The source context of a session, read BEFORE its content so a change
    /// in between makes the snapshot stale rather than the content newer than
    /// it. `NotFound` once unshared: a tombstone is no source.
    async fn source_context(&self, session_id: &str) -> Result<SourceContext>;
    /// The ledger row of a managed session id, if it is one.
    async fn publication_ledger(&self, session_id: &str) -> Result<Option<PublicationLedger>>;
}

/// Who publishes a managed session, as recorded by its accepted operations.
#[derive(Debug, Clone)]
pub struct PublicationLedger {
    pub host_id: String,
    pub owner_email: Option<String>,
    pub rev: i64,
    pub state: String,
}

pub(crate) fn sql(query: &str, postgres: bool) -> String {
    if !postgres {
        return query.into();
    }
    let mut n = 0;
    query
        .chars()
        .map(|c| {
            if c == '?' {
                n += 1;
                format!("${n}")
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Serialize every writer of one session id. SQLite: the first write takes the
/// database write lock, so the following reads are current. Postgres: a
/// transaction-scoped advisory lock (row locks cannot cover an absent row).
pub(crate) fn lock_sql(postgres: bool) -> &'static str {
    if postgres {
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))"
    } else {
        "UPDATE publications SET state = state WHERE session_id = ?"
    }
}

/// Inside a legacy writer's transaction: lock each id (sorted and deduped, so
/// two writers locking the same pair in opposite order cannot deadlock), then return
/// `Conflict(MANAGED)` from the enclosing function when it is managed, in any
/// state. Legacy and derived writes carry no publication revision, so even an
/// active publication refuses them: a delayed write could otherwise land in a
/// newer share's projection.
macro_rules! reject_managed {
    ($tx:expr, $postgres:expr, $($id:expr),+ $(,)?) => {
        let mut ids: Vec<&str> = vec![$($id),+];
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            sqlx::query($crate::store::publication::lock_sql($postgres))
                .bind(id)
                .execute(&mut *$tx)
                .await?;
            let managed: Option<String> = sqlx::query_scalar(&$crate::store::publication::sql(
                "SELECT state FROM publications WHERE session_id = ?",
                $postgres,
            ))
            .bind(id)
            .fetch_optional(&mut *$tx)
            .await?;
            if managed.is_some() {
                return Err($crate::error::AppError::Conflict(
                    $crate::store::publication::MANAGED.into(),
                ));
            }
        }
    };
}
pub(crate) use reject_managed;

/// Inside a derived writer's transaction: take the same per-id lock as the
/// publication writer, then return the [`SourceContext::admit`] refusal from
/// the enclosing function unless the ledger still matches `$source` exactly.
macro_rules! fence_source {
    ($tx:expr, $postgres:expr, $id:expr, $source:expr) => {
        sqlx::query($crate::store::publication::lock_sql($postgres))
            .bind($id)
            .execute(&mut *$tx)
            .await?;
        let ledger: Option<(String, i64, i64)> = sqlx::query_as(&$crate::store::publication::sql(
            "SELECT state, max_rev, next_seq FROM publications WHERE session_id = ?",
            $postgres,
        ))
        .bind($id)
        .fetch_optional(&mut *$tx)
        .await?;
        $source.admit(ledger)?;
    };
}
pub(crate) use fence_source;

fn conflict(message: &str) -> AppError {
    AppError::Conflict(message.into())
}

fn ack(
    session: &str,
    rev: u64,
    state: PublicationState,
    next_seq: u64,
    replayed: bool,
) -> PublicationAck {
    PublicationAck {
        session: session.into(),
        rev,
        state,
        next_seq,
        replayed,
    }
}

/// Split an audience into its session-row parts: the visibility string, the
/// reader emails, and which readers are also intended to write into the
/// session through the publishing host once their key is trusted there.
fn audience_parts(audience: &Audience) -> (&'static str, Vec<String>, Vec<String>) {
    match audience {
        Audience::Organization {} => (super::VISIBILITY_ORGANIZATION, Vec::new(), Vec::new()),
        Audience::Restricted {
            emails,
            write_pending,
        } => {
            let mut emails: Vec<String> = emails
                .iter()
                .map(|e| e.trim().to_lowercase())
                .filter(|e| !e.is_empty())
                .collect();
            emails.sort();
            emails.dedup();
            let mut pending: Vec<String> = write_pending
                .iter()
                .map(|e| e.trim().to_lowercase())
                .filter(|e| emails.contains(e))
                .collect();
            pending.sort();
            pending.dedup();
            (super::VISIBILITY_RESTRICTED, emails, pending)
        }
    }
}

macro_rules! publication_repository {
    ($repo:ty, $postgres:expr) => {
        impl $repo {
            /// Delete the published projection and everything derived from it.
            /// Rows without a cascading foreign key are removed explicitly; the
            /// rest go with the `sessions` row. The ledger stays.
            async fn purge_projection(
                tx: &mut sqlx::Transaction<'_, <$repo as PublicationDb>::Db>,
                id: &str,
            ) -> Result<()> {
                for table in ["session_prs", "session_linear_docs"] {
                    sqlx::query(&sql(
                        &format!("DELETE FROM {table} WHERE session_id = ?"),
                        $postgres,
                    ))
                    .bind(id)
                    .execute(&mut **tx)
                    .await?;
                }
                sqlx::query(&sql("DELETE FROM sessions WHERE id = ?", $postgres))
                    .bind(id)
                    .execute(&mut **tx)
                    .await?;
                Ok(())
            }

            /// Create the empty projection row for a share at a new revision.
            async fn create_projection(
                tx: &mut sqlx::Transaction<'_, <$repo as PublicationDb>::Db>,
                op: &PublicationOp,
                audience: &Audience,
                agent: conversations_core::AgentKind,
                title: &str,
                now: &str,
            ) -> Result<()> {
                let (visibility, emails, write_pending) = audience_parts(audience);
                let ts = if $postgres { "CAST(? AS TIMESTAMPTZ)" } else { "?" };
                let uploaded_by = op.owner.clone().unwrap_or_else(|| op.host.clone());
                sqlx::query(&sql(
                    &format!(
                        "INSERT INTO sessions (id, agent, title, message_count, skipped_lines,
                         uploaded_by, uploaded_at, owner_email, visibility)
                         VALUES (?, ?, ?, 0, 0, ?, {ts}, ?, ?)"
                    ),
                    $postgres,
                ))
                .bind(&op.session)
                .bind(agent.as_str())
                .bind(title)
                .bind(uploaded_by)
                .bind(now)
                .bind(&op.owner)
                .bind(visibility)
                .execute(&mut **tx)
                .await?;
                for email in emails {
                    let pending = write_pending.iter().any(|p| p == &email);
                    sqlx::query(&sql(
                        "INSERT INTO session_shares (session_id, email, write_pending_key) VALUES (?, ?, ?)",
                        $postgres,
                    ))
                    .bind(&op.session)
                    .bind(email)
                    .bind(pending)
                    .execute(&mut **tx)
                    .await?;
                }
                Ok(())
            }

            /// Insert chunk `seq` after the existing messages.
            /// Record exactly the links one message carries now.
            async fn replace_message_refs(
                tx: &mut sqlx::Transaction<'_, <$repo as PublicationDb>::Db>,
                id: &str,
                uuid: &str,
                refs: &MessageRefs,
            ) -> Result<()> {
                sqlx::query(&sql(
                    "DELETE FROM publication_message_refs WHERE session_id = ? AND uuid = ?",
                    $postgres,
                ))
                .bind(id)
                .bind(uuid)
                .execute(&mut **tx)
                .await?;
                let urls = refs
                    .prs
                    .iter()
                    .map(|pr| ("pr", &pr.url))
                    .chain(refs.linear_docs.iter().map(|doc| ("linear", &doc.url)));
                for (kind, url) in urls {
                    sqlx::query(&sql(
                        "INSERT INTO publication_message_refs (session_id, uuid, kind, url)
                         VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
                        $postgres,
                    ))
                    .bind(id)
                    .bind(uuid)
                    .bind(kind)
                    .bind(url)
                    .execute(&mut **tx)
                    .await?;
                }
                Ok(())
            }

            async fn append_chunk(
                tx: &mut sqlx::Transaction<'_, <$repo as PublicationDb>::Db>,
                id: &str,
                seq: u64,
                chunk: &VerifiedChunk,
                now: &str,
            ) -> Result<()> {
                // The NUL scrubber works on a session; borrow its shape.
                let shaped = Session {
                    id: id.into(),
                    agent: Default::default(),
                    title: String::new(),
                    project_path: None,
                    cwd: None,
                    git_branch: None,
                    claude_version: None,
                    started_at: None,
                    ended_at: None,
                    messages: chunk.chunk.messages.clone(),
                    skipped_lines: 0,
                };
                let scrubbed = scrub_session(&shaped);
                let messages = &scrubbed.messages;
                let base: i64 = sqlx::query_scalar(&sql(
                    "SELECT COALESCE(MAX(ord) + 1, 0) FROM messages WHERE session_id = ?",
                    $postgres,
                ))
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
                let (json, ts) = if $postgres {
                    ("CAST(? AS JSONB)", "CAST(? AS TIMESTAMPTZ)")
                } else {
                    ("?", "?")
                };
                // A message already published in this session is updated in
                // place (same position); a new one is appended. Within one
                // chunk a uuid may appear once.
                let mut seen = std::collections::HashSet::new();
                let mut added: i64 = 0;
                for (msg, refs) in messages.iter().zip(&chunk.refs) {
                    if !seen.insert(msg.uuid.as_str()) {
                        return Err(conflict("the chunk repeats a message"));
                    }
                    let existing: Option<i64> = sqlx::query_scalar(&sql(
                        "SELECT ord FROM messages WHERE session_id = ? AND uuid = ?",
                        $postgres,
                    ))
                    .bind(id)
                    .bind(&msg.uuid)
                    .fetch_optional(&mut **tx)
                    .await?;
                    let ord = match existing {
                        Some(ord) => ord,
                        None => {
                            added += 1;
                            base + added - 1
                        }
                    };
                    let role = serde_json::to_value(msg.role)?
                        .as_str()
                        .unwrap_or("user")
                        .to_string();
                    let kind = serde_json::to_value(msg.kind)?
                        .as_str()
                        .unwrap_or("mixed")
                        .to_string();
                    let usage = msg.usage.as_ref().map(serde_json::to_string).transpose()?;
                    sqlx::query(&sql(
                        &format!(
                            "INSERT INTO messages (uuid, session_id, parent_uuid, role, kind, model,
                             is_sidechain, timestamp, text, raw, usage, ord)
                             VALUES (?, ?, ?, ?, ?, ?, ?, {ts}, ?, {json}, {json}, ?)
                             ON CONFLICT(session_id, uuid) DO UPDATE SET
                             parent_uuid = excluded.parent_uuid, role = excluded.role,
                             kind = excluded.kind, model = excluded.model,
                             is_sidechain = excluded.is_sidechain, timestamp = excluded.timestamp,
                             text = excluded.text, raw = excluded.raw, usage = excluded.usage"
                        ),
                        $postgres,
                    ))
                    .bind(&msg.uuid)
                    .bind(id)
                    .bind(&msg.parent_uuid)
                    .bind(role)
                    .bind(kind)
                    .bind(&msg.model)
                    .bind(msg.is_sidechain)
                    .bind(msg.timestamp.map(|t| t.to_rfc3339()))
                    .bind(&msg.text)
                    .bind(serde_json::to_string(&msg.raw)?)
                    .bind(usage)
                    .bind(ord)
                    .execute(&mut **tx)
                    .await?;
                    Self::replace_message_refs(tx, id, &msg.uuid, refs).await?;
                }
                let first = messages.iter().find_map(|m| m.timestamp).map(|t| t.to_rfc3339());
                let last = messages.iter().rev().find_map(|m| m.timestamp).map(|t| t.to_rfc3339());
                sqlx::query(&sql(
                    &format!(
                        "UPDATE sessions SET message_count = message_count + ?,
                         started_at = COALESCE(started_at, {ts}), ended_at = COALESCE({ts}, ended_at)
                         WHERE id = ?"
                    ),
                    $postgres,
                ))
                .bind(added)
                .bind(first)
                .bind(last)
                .bind(id)
                .execute(&mut **tx)
                .await?;
                sqlx::query(&sql(
                    "INSERT INTO publication_chunks (session_id, seq, sha256) VALUES (?, ?, ?)",
                    $postgres,
                ))
                .bind(id)
                .bind(seq as i64)
                .bind(&chunk.sha256)
                .execute(&mut **tx)
                .await?;
                let created = if $postgres { "CAST(? AS TIMESTAMPTZ)" } else { "?" };
                for pr in chunk.refs.iter().flat_map(|r| &r.prs) {
                    sqlx::query(&sql(
                        &format!(
                            "INSERT INTO session_prs (session_id, url, owner, repo, number, created_at, detected)
                             VALUES (?, ?, ?, ?, ?, {created}, ?)
                             ON CONFLICT(session_id, owner, repo, number) DO NOTHING"
                        ),
                        $postgres,
                    ))
                    .bind(id)
                    .bind(&pr.url)
                    .bind(&pr.owner)
                    .bind(&pr.repo)
                    .bind(pr.number)
                    .bind(now)
                    .bind(true)
                    .execute(&mut **tx)
                    .await?;
                }
                for doc in chunk.refs.iter().flat_map(|r| &r.linear_docs) {
                    sqlx::query(&sql(
                        &format!(
                            "INSERT INTO session_linear_docs (session_id, url, workspace, kind, identifier, created_at)
                             VALUES (?, ?, ?, ?, ?, {created})
                             ON CONFLICT(session_id, url) DO NOTHING"
                        ),
                        $postgres,
                    ))
                    .bind(id)
                    .bind(&doc.url)
                    .bind(&doc.workspace)
                    .bind(&doc.kind)
                    .bind(&doc.identifier)
                    .bind(now)
                    .execute(&mut **tx)
                    .await?;
                }
                // A link no current message carries (its message was updated)
                // is removed. Every link of a managed session comes from here.
                for (table, kind) in [("session_prs", "pr"), ("session_linear_docs", "linear")] {
                    sqlx::query(&sql(
                        &format!(
                            "DELETE FROM {table} WHERE session_id = ? AND url NOT IN
                             (SELECT url FROM publication_message_refs WHERE session_id = ? AND kind = ?)"
                        ),
                        $postgres,
                    ))
                    .bind(id)
                    .bind(id)
                    .bind(kind)
                    .execute(&mut **tx)
                    .await?;
                }
                Ok(())
            }
        }

        #[async_trait]
        impl PublicationRepository for $repo {
            async fn source_context(&self, session_id: &str) -> Result<SourceContext> {
                let ledger: Option<(String, i64, i64)> = sqlx::query_as(&sql(
                    "SELECT state, max_rev, next_seq FROM publications WHERE session_id = ?",
                    $postgres,
                ))
                .bind(session_id)
                .fetch_optional(self.pool())
                .await?;
                match ledger {
                    None => Ok(SourceContext::Unmanaged),
                    Some((state, rev, next_seq)) if state == "active" => {
                        Ok(SourceContext::Published(PublicationSnapshot {
                            rev: rev as u64,
                            next_seq: next_seq as u64,
                        }))
                    }
                    Some(_) => Err(AppError::NotFound),
                }
            }

            async fn publication_ledger(&self, session_id: &str) -> Result<Option<PublicationLedger>> {
                let row: Option<(String, Option<String>, i64, String)> = sqlx::query_as(&sql(
                    "SELECT host_id, owner_email, max_rev, state FROM publications WHERE session_id = ?",
                    $postgres,
                ))
                .bind(session_id)
                .fetch_optional(self.pool())
                .await?;
                Ok(row.map(|(host_id, owner_email, rev, state)| PublicationLedger {
                    host_id,
                    owner_email,
                    rev,
                    state,
                }))
            }

            async fn apply_publication(&self, write: &PublicationWrite) -> Result<PublicationAck> {
                let op = &write.op;
                let id = op.session.as_str();
                let now = Utc::now().to_rfc3339();
                let ts = if $postgres { "CAST(? AS TIMESTAMPTZ)" } else { "?" };
                let mut tx = self.pool().begin().await?;
                sqlx::query(lock_sql($postgres)).bind(id).execute(&mut *tx).await?;
                let ledger = sqlx::query(&sql(
                    "SELECT host_id, owner_email, max_rev, state, next_seq, share_sha256
                     FROM publications WHERE session_id = ?",
                    $postgres,
                ))
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;

                let Some(ledger) = ledger else {
                    // First operation for this id: only a share claims it.
                    // An append needs an established revision; an unshare has nothing
                    // to delete and must NOT create a tombstone: the id stays
                    // unclaimed, so a stranger's unshare cannot lock out the session's
                    // real publisher (and a publisher that unshared before its share
                    // was ever accepted is acknowledged, consistently, as deleted).
                    match &op.action {
                        PublicationAction::Append { .. } => {
                            return Err(conflict("this session has not been shared"));
                        }
                        PublicationAction::Unshare {} => {
                            return Ok(ack(id, op.rev, PublicationState::Deleted, 0, false));
                        }
                        PublicationAction::Share { audience, agent, title } => {
                            let taken: bool = sqlx::query_scalar(&sql(
                                "SELECT EXISTS (SELECT 1 FROM sessions WHERE id = ?)
                                 OR EXISTS (SELECT 1 FROM live_sessions WHERE id = ? OR session_id = ?)
                                 OR EXISTS (SELECT 1 FROM session_prs WHERE session_id = ?)
                                 OR EXISTS (SELECT 1 FROM session_linear_docs WHERE session_id = ?)",
                                $postgres,
                            ))
                            .bind(id)
                            .bind(id)
                            .bind(id)
                            .bind(id)
                            .bind(id)
                            .fetch_one(&mut *tx)
                            .await?;
                            if taken {
                                return Err(conflict(
                                    "this session id already has central data the publication does not own",
                                ));
                            }
                            sqlx::query(&sql(
                                &format!(
                                    "INSERT INTO publications (session_id, host_id, host_public_key, owner_email,
                                     max_rev, state, next_seq, share_sha256, created_at, updated_at)
                                     VALUES (?, ?, ?, ?, ?, ?, 0, ?, {ts}, {ts})"
                                ),
                                $postgres,
                            ))
                            .bind(id)
                            .bind(&op.host)
                            .bind(&write.host_public_key)
                            .bind(&op.owner)
                            .bind(op.rev as i64)
                            .bind(state_str(PublicationState::Active))
                            .bind(Some(&write.op_sha256))
                            .bind(&now)
                            .bind(&now)
                            .execute(&mut *tx)
                            .await?;
                            Self::create_projection(&mut tx, op, audience, *agent, title, &now)
                                .await?;
                            tx.commit().await?;
                            return Ok(ack(id, op.rev, PublicationState::Active, 0, false));
                        }
                    }
                };

                let host_id: String = ledger.get("host_id");
                let owner: Option<String> = ledger.get("owner_email");
                if host_id != op.host || owner != op.owner {
                    return Err(AppError::Forbidden(
                        "this session is published by another host or account".into(),
                    ));
                }
                let max_rev = ledger.get::<i64, _>("max_rev") as u64;
                let active = ledger.get::<String, _>("state") == "active";
                let next_seq = ledger.get::<i64, _>("next_seq") as u64;
                let share_sha: Option<String> = ledger.get("share_sha256");
                let stale = || conflict("a newer share revision has already been accepted");

                match &op.action {
                    PublicationAction::Share { audience, agent, title } => {
                        if op.rev == max_rev && active && share_sha.as_deref() == Some(&write.op_sha256) {
                            return Ok(ack(id, max_rev, PublicationState::Active, next_seq, true));
                        }
                        if op.rev <= max_rev {
                            return Err(stale());
                        }
                        Self::purge_projection(&mut tx, id).await?;
                        Self::create_projection(&mut tx, op, audience, *agent, title, &now).await?;
                        sqlx::query(&sql(
                            &format!(
                                "UPDATE publications SET max_rev = ?, state = 'active', next_seq = 0,
                                 share_sha256 = ?, updated_at = {ts} WHERE session_id = ?"
                            ),
                            $postgres,
                        ))
                        .bind(op.rev as i64)
                        .bind(&write.op_sha256)
                        .bind(&now)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                        tx.commit().await?;
                        Ok(ack(id, op.rev, PublicationState::Active, 0, false))
                    }
                    PublicationAction::Append { seq, .. } => {
                        if !active || op.rev < max_rev {
                            return Err(stale());
                        }
                        if op.rev > max_rev {
                            return Err(conflict("this revision has not been shared"));
                        }
                        let chunk = write
                            .chunk
                            .as_ref()
                            .ok_or_else(|| AppError::BadRequest("append requires a chunk".into()))?;
                        if *seq < next_seq {
                            let stored: Option<String> = sqlx::query_scalar(&sql(
                                "SELECT sha256 FROM publication_chunks WHERE session_id = ? AND seq = ?",
                                $postgres,
                            ))
                            .bind(id)
                            .bind(*seq as i64)
                            .fetch_optional(&mut *tx)
                            .await?;
                            return if stored.as_deref() == Some(chunk.sha256.as_str()) {
                                Ok(ack(id, max_rev, PublicationState::Active, next_seq, true))
                            } else {
                                Err(conflict("a different chunk was already accepted at this sequence"))
                            };
                        }
                        if *seq > next_seq {
                            return Err(conflict("chunk sequence skips the publication cursor"));
                        }
                        Self::append_chunk(&mut tx, id, *seq, chunk, &now).await?;
                        sqlx::query(&sql(
                            &format!(
                                "UPDATE publications SET next_seq = ?, updated_at = {ts} WHERE session_id = ?"
                            ),
                            $postgres,
                        ))
                        .bind((next_seq + 1) as i64)
                        .bind(&now)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                        tx.commit().await?;
                        Ok(ack(id, max_rev, PublicationState::Active, next_seq + 1, false))
                    }
                    PublicationAction::Unshare {} => {
                        if op.rev == max_rev && !active {
                            return Ok(ack(id, max_rev, PublicationState::Deleted, next_seq, true));
                        }
                        if op.rev <= max_rev {
                            return Err(stale());
                        }
                        Self::purge_projection(&mut tx, id).await?;
                        sqlx::query(&sql(
                            &format!(
                                "UPDATE publications SET max_rev = ?, state = 'deleted', share_sha256 = NULL,
                                 updated_at = {ts} WHERE session_id = ?"
                            ),
                            $postgres,
                        ))
                        .bind(op.rev as i64)
                        .bind(&now)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                        tx.commit().await?;
                        Ok(ack(id, op.rev, PublicationState::Deleted, next_seq, false))
                    }
                }
            }
        }
    };
}

fn state_str(state: PublicationState) -> &'static str {
    match state {
        PublicationState::Active => "active",
        PublicationState::Deleted => "deleted",
    }
}

/// The sqlx database of each repository, so the shared macro can name its
/// transaction type.
pub trait PublicationDb {
    type Db: sqlx::Database;
}
impl PublicationDb for SqliteRepo {
    type Db = sqlx::Sqlite;
}
impl PublicationDb for PostgresRepo {
    type Db = sqlx::Postgres;
}

publication_repository!(SqliteRepo, false);
publication_repository!(PostgresRepo, true);
