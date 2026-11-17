//! Durable inbox operations shared by both SQL backends.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use chrono::Utc;
use conversations_core::messaging::{
    AgentHandoff, AgentMessage, AgentMessageStatus, AgentPeer, ConversationMessageTitle,
    ConversationSummary, HandoffRequest, SendAgentMessage,
};
use sqlx::Row;
use uuid::Uuid;

use super::{parse_agent, AgentSessionIdentity, PostgresRepo, SqliteRepo};
use crate::error::{AppError, Result};

#[async_trait]
pub trait AgentMessagingRepository: Send + Sync {
    async fn agent_session_identity(
        &self,
        session_id: &str,
    ) -> Result<Option<AgentSessionIdentity>>;
    async fn create_agent_handoff(
        &self,
        source: &str,
        request: &HandoffRequest,
        agent: &str,
    ) -> Result<AgentHandoff>;
    async fn get_agent_handoff(&self, source: &str, child: &str) -> Result<Option<AgentHandoff>>;
    /// The handoff that created `child`, if any; the child is unique per handoff.
    async fn agent_handoff_for_child(&self, child: &str) -> Result<Option<AgentHandoff>>;
    async fn claim_agent_handoff(&self, source: &str, child: &str) -> Result<bool>;
    async fn finish_agent_handoff(
        &self,
        source: &str,
        child: &str,
        error: Option<&str>,
    ) -> Result<()>;

    async fn agent_messaging_context(&self, session_id: &str) -> Result<Option<AgentPeer>>;
    async fn agent_messaging_peers(&self, session_id: &str) -> Result<Vec<AgentPeer>>;
    async fn send_agent_message(
        &self,
        sender: &str,
        request: &SendAgentMessage,
    ) -> Result<AgentMessage>;
    async fn receive_agent_messages(
        &self,
        recipient: &str,
        acknowledge: &[String],
    ) -> Result<Vec<AgentMessage>>;
    async fn get_agent_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<AgentMessage>>;
    /// Per-peer message metadata for `session_id` under current authorization,
    /// newest conversation first. Reads only; never records receipt.
    async fn agent_conversation_summaries(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, ConversationSummary)>>;
    /// The subset of `ids` whose transcript `viewer` may read, by the same rule
    /// as [`super::Repository::get_session`].
    async fn readable_session_ids(&self, viewer: &str, ids: &[String]) -> Result<HashSet<String>>;
}

// All columns in this additive schema have identical TEXT/INTEGER representations.
// Keep permission and delivery SQL identical, replacing only bind placeholders.
fn sql(query: &str, postgres: bool) -> String {
    if !postgres {
        return query.to_owned();
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

// A share grants incoming contact to the shared session. The reverse direction
// requires a persisted conversation edge, so sharing never publishes all of a
// person's unrelated sessions to the grantor.
const AUTHORIZED: &str = "NOT EXISTS (SELECT 1 FROM agent_handoffs h WHERE h.status = 'failed' AND (h.session_id = a.id OR h.session_id = b.id)) AND (
 (a.owner_email IS NOT NULL AND a.owner_email <> '' AND a.owner_email = b.owner_email)
 OR EXISTS (SELECT 1 FROM live_session_shares g WHERE g.live_id = b.id
    AND g.email = a.owner_email AND g.can_communicate = TRUE)
 OR (EXISTS (SELECT 1 FROM live_session_shares g WHERE g.live_id = a.id
    AND g.email = b.owner_email AND g.can_communicate = TRUE)
    AND EXISTS (SELECT 1 FROM agent_messages edge WHERE edge.sender_session_id = b.id
      AND edge.recipient_session_id = a.id AND edge.sender_owner_email = b.owner_email))
)";
const ACTIVE: &str = "SELECT s.id AS session_id, s.project_id, COALESCE(s.title_override, NULLIF(l.title, ''), s.title) AS title, s.agent,
 l.status AS live_status, l.mode AS live_mode, s.owner_email, COALESCE(l.last_event_at, s.uploaded_at) AS last_seen_at FROM sessions s
 LEFT JOIN live_sessions l ON l.id = s.id WHERE NOT EXISTS (SELECT 1 FROM agent_handoffs h WHERE h.session_id = s.id AND h.status = 'failed')";
const MESSAGE_ACCESS: &str = "SELECT msg.* FROM agent_messages msg
 JOIN sessions a ON a.id = msg.sender_session_id
 JOIN sessions b ON b.id = msg.recipient_session_id WHERE
 COALESCE(msg.sender_owner_email, '') = COALESCE(a.owner_email, '')";

macro_rules! messaging_repository {
    ($repo:ty, $postgres:expr) => {
        #[async_trait]
        impl AgentMessagingRepository for $repo {
            async fn agent_session_identity(&self, session_id: &str) -> Result<Option<AgentSessionIdentity>> {
                let row = sqlx::query(&sql("SELECT id, agent, agent_session_id FROM sessions WHERE id = ?", $postgres))
                    .bind(session_id).fetch_optional(self.pool()).await?;
                Ok(row.map(|row| AgentSessionIdentity {
                    session_id: row.get("id"),
                    agent: parse_agent(row.get::<Option<String>, _>("agent").as_deref()),
                    agent_session_id: row.get("agent_session_id"),
                }))
            }

            async fn create_agent_handoff(&self, source: &str, request: &HandoffRequest, agent: &str) -> Result<AgentHandoff> {
                let mut tx = self.pool().begin().await?;
                super::publication::reject_managed!(tx, $postgres, source);
                let now = Utc::now().to_rfc3339();
                // Lock the source even before it has a mailbox, so concurrent handoffs
                // cannot create separate projects for the same source.
                let locked = sqlx::query(&sql("UPDATE sessions SET project_id = project_id WHERE id = ?", $postgres))
                    .bind(source).execute(&mut *tx).await?;
                if locked.rows_affected() == 0 { return Err(AppError::NotFound); }
                let session = sqlx::query(&sql("SELECT project_id, owner_email, title FROM sessions WHERE id = ?", $postgres))
                    .bind(source).fetch_one(&mut *tx).await?;
                if session.get::<Option<String>, _>("project_id").is_none() {
                    let project = Uuid::new_v4().to_string();
                    let owner: Option<String> = session.get("owner_email");
                    let title: String = session.get("title");
                    let name = format!("Handoff: {}", title.chars().take(100).collect::<String>());
                    let timestamp = if $postgres { "CAST(? AS TIMESTAMPTZ)" } else { "?" };
                    sqlx::query(&sql(&format!("INSERT INTO projects (id, name, owner_email, created_at, updated_at) VALUES (?, ?, ?, {timestamp}, {timestamp})"), $postgres))
                        .bind(&project).bind(name).bind(&owner).bind(&now).bind(&now).execute(&mut *tx).await?;
                    sqlx::query(&sql("UPDATE sessions SET project_id = ? WHERE id = ?", $postgres))
                        .bind(&project).bind(source).execute(&mut *tx).await?;

                }
                let context = sqlx::query(&sql(&format!("{ACTIVE} AND s.id = ?"), $postgres))
                    .bind(source).fetch_optional(&mut *tx).await?.ok_or_else(|| AppError::Forbidden("session does not exist".into()))?;
                let project: String = context.get("project_id");
                let owner: Option<String> = context.get("owner_email");
                let previous = sqlx::query(&sql("SELECT * FROM agent_handoffs WHERE source_session_id = ? AND idempotency_key = ?", $postgres))
                    .bind(source).bind(&request.idempotency_key).fetch_optional(&mut *tx).await?;
                if let Some(row) = previous {
                    let handoff = handoff_from_row!(&row);
                    if handoff.project_id != project || handoff.agent != agent || handoff.brief != request.brief
                        || handoff.model != request.model || handoff.variant != request.variant {
                        return Err(AppError::Conflict("idempotency key already used for a different handoff".into()));
                    }
                    tx.commit().await?;
                    return Ok(handoff);
                }
                let child = Uuid::new_v4().to_string();
                let now = Utc::now().to_rfc3339();
                let title = format!("Handoff: {}", request.brief.lines().next().unwrap_or("").chars().take(100).collect::<String>());
                let uploaded_at = if $postgres { "CAST(? AS TIMESTAMPTZ)" } else { "?" };
                sqlx::query(&sql(&format!("INSERT INTO sessions (id, agent, title, message_count, skipped_lines, uploaded_by,
                    uploaded_at, owner_email, visibility, project_id) VALUES (?, ?, ?, 0, 0, ?, {uploaded_at}, ?, 'restricted', ?)"), $postgres))
                    .bind(&child).bind(agent).bind(title).bind(owner.as_deref().unwrap_or_default())
                    .bind(&now).bind(&owner).bind(&project).execute(&mut *tx).await?;
                sqlx::query(&sql("INSERT INTO agent_handoffs (session_id, source_session_id, project_id, agent, brief,
                    idempotency_key, status, created_at, model, variant) VALUES (?, ?, ?, ?, ?, ?, 'requested', ?, ?, ?)", $postgres))
                    .bind(&child).bind(source).bind(&project).bind(agent).bind(&request.brief)
                    .bind(&request.idempotency_key).bind(&now).bind(&request.model).bind(&request.variant)
                    .execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(AgentHandoff { session_id: child, source_session_id: source.into(), project_id: project,
                    agent: agent.into(), brief: request.brief.clone(), idempotency_key: request.idempotency_key.clone(),
                    status: "requested".into(), error: None, created_at: now,
                    model: request.model.clone(), variant: request.variant.clone() })
            }

            async fn get_agent_handoff(&self, source: &str, child: &str) -> Result<Option<AgentHandoff>> {
                let row = sqlx::query(&sql("SELECT * FROM agent_handoffs WHERE source_session_id = ? AND session_id = ?", $postgres))
                    .bind(source).bind(child).fetch_optional(self.pool()).await?;
                Ok(row.as_ref().map(|r| handoff_from_row!(r)))
            }

            async fn agent_handoff_for_child(&self, child: &str) -> Result<Option<AgentHandoff>> {
                let row = sqlx::query(&sql("SELECT * FROM agent_handoffs WHERE session_id = ?", $postgres))
                    .bind(child).fetch_optional(self.pool()).await?;
                Ok(row.as_ref().map(|r| handoff_from_row!(r)))
            }

            async fn claim_agent_handoff(&self, source: &str, child: &str) -> Result<bool> {
                let changed = sqlx::query(&sql("UPDATE agent_handoffs SET status = 'starting' WHERE source_session_id = ?
                    AND session_id = ? AND status = 'requested' AND EXISTS (
                    SELECT 1 FROM sessions s WHERE s.id = agent_handoffs.source_session_id
                    AND s.project_id = agent_handoffs.project_id)", $postgres))
                    .bind(source).bind(child).execute(self.pool()).await?;
                Ok(changed.rows_affected() == 1)
            }

            async fn finish_agent_handoff(&self, source: &str, child: &str, error: Option<&str>) -> Result<()> {
                let mut tx = self.pool().begin().await?;
                let status = if error.is_some() { "failed" } else { "started" };
                let updated = sqlx::query(&sql("UPDATE agent_handoffs SET status = ?, error = ? WHERE source_session_id = ? AND session_id = ? AND status = 'starting'", $postgres))
                    .bind(status).bind(error).bind(source).bind(child).execute(&mut *tx).await?;
                if updated.rows_affected() == 0 {
                    let row = sqlx::query(&sql("SELECT * FROM agent_handoffs WHERE source_session_id = ? AND session_id = ?", $postgres))
                        .bind(source).bind(child).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
                    let previous = handoff_from_row!(&row);
                    if previous.status != status || previous.error.as_deref() != error {
                        return Err(AppError::Conflict("handoff must be claimed before reporting startup, and its result cannot change".into()));
                    }
                }
                tx.commit().await?;
                Ok(())
            }

            async fn agent_messaging_context(&self, session_id: &str) -> Result<Option<AgentPeer>> {
                let row = sqlx::query(&sql(
                    &format!("{ACTIVE} AND s.id = ?"),
                    $postgres,
                ))
                .bind(session_id)
                .fetch_optional(self.pool())
                .await?;
                Ok(row.map(|r| peer_from_row!(&r, $postgres)))
            }

            async fn agent_messaging_peers(&self, session_id: &str) -> Result<Vec<AgentPeer>> {
                let query = format!("{ACTIVE} AND s.id <> ? AND EXISTS (SELECT 1 FROM sessions a JOIN sessions b ON b.id = s.id WHERE a.id = ? AND {AUTHORIZED}) ORDER BY last_seen_at DESC, s.id ASC LIMIT 100");
                let rows = sqlx::query(&sql(&query, $postgres)).bind(session_id).bind(session_id).fetch_all(self.pool()).await?;
                Ok(rows.iter().map(|r| peer_from_row!(r, $postgres)).collect())
            }

            async fn send_agent_message(
                &self,
                sender: &str,
                request: &SendAgentMessage,
            ) -> Result<AgentMessage> {
                let mut tx = self.pool().begin().await?;
                super::publication::reject_managed!(tx, $postgres, sender, request.recipient_session_id.as_str());
                let query = format!("SELECT CASE WHEN a.project_id = b.project_id THEN a.project_id ELSE NULL END AS project_id, a.owner_email FROM sessions a JOIN sessions b ON b.id = ? WHERE a.id = ? AND {AUTHORIZED}");
                let allowed = sqlx::query(&sql(&query, $postgres))
                    .bind(&request.recipient_session_id).bind(sender).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
                let project: Option<String> = allowed.get("project_id");
                let owner: Option<String> = allowed.get("owner_email");
                if let Some(parent) = &request.reply_to {
                    let parent_query = format!("{MESSAGE_ACCESS} AND {AUTHORIZED} AND msg.id = ? AND msg.sender_session_id = ? AND msg.recipient_session_id = ?");
                    let parent_exists = sqlx::query(&sql(&parent_query, $postgres))
                        .bind(parent).bind(&request.recipient_session_id).bind(sender)
                        .fetch_optional(&mut *tx).await?;
                    if parent_exists.is_none() {
                        return Err(AppError::NotFound);
                    }
                }
                let id = Uuid::new_v4().to_string();
                sqlx::query(&sql("INSERT INTO agent_messages (id, project_id, sender_session_id, sender_owner_email,
                            recipient_session_id, text, reply_to, idempotency_key, created_at)
                            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(sender_session_id, idempotency_key) DO NOTHING", $postgres))
                            .bind(&id).bind(&project).bind(sender).bind(owner).bind(&request.recipient_session_id)
                            .bind(&request.text).bind(&request.reply_to).bind(&request.idempotency_key).bind(Utc::now().to_rfc3339())
                            .execute(&mut *tx).await?;
                let row = sqlx::query(&sql(
                    "SELECT * FROM agent_messages WHERE sender_session_id = ? AND idempotency_key = ?",
                    $postgres,
                ))
                .bind(sender)
                .bind(&request.idempotency_key)
                .fetch_one(&mut *tx)
                .await?;
                let message = message_from_row!(&row);
                if message.recipient_session_id != request.recipient_session_id
                    || message.text != request.text
                    || message.reply_to != request.reply_to
                {
                    return Err(AppError::Conflict(
                        "idempotency key already used for a different message".into(),
                    ));
                }
                if let Some(parent) = &request.reply_to {
                    sqlx::query(&sql("UPDATE agent_messages SET acknowledged_at = COALESCE(acknowledged_at, ?) WHERE id = ?", $postgres))
                                .bind(Utc::now().to_rfc3339()).bind(parent).execute(&mut *tx).await?;
                }
                tx.commit().await?;
                Ok(message)
            }

            async fn receive_agent_messages(
                &self,
                recipient: &str,
                acknowledge: &[String],
            ) -> Result<Vec<AgentMessage>> {
                let mut tx = self.pool().begin().await?;
                let now = Utc::now().to_rfc3339();
                for id in acknowledge {
                    let result = sqlx::query(&sql(
                        &format!("UPDATE agent_messages SET acknowledged_at = COALESCE(acknowledged_at, ?) WHERE id = ? AND recipient_session_id = ? AND id IN (SELECT permitted.id FROM ({MESSAGE_ACCESS} AND {AUTHORIZED}) permitted)"),
                        $postgres,
                    ))
                    .bind(&now)
                    .bind(id)
                    .bind(recipient)
                    .execute(&mut *tx)
                    .await?;
                    if result.rows_affected() == 0 {
                        return Err(AppError::NotFound);
                    }
                }
                let rows = sqlx::query(&sql(
                    &format!("{MESSAGE_ACCESS} AND {AUTHORIZED} AND msg.recipient_session_id = ? AND msg.acknowledged_at IS NULL ORDER BY msg.created_at, msg.id LIMIT 50"),
                    $postgres,
                ))
                .bind(recipient)
                .fetch_all(&mut *tx)
                .await?;
                let mut messages: Vec<AgentMessage> = rows.iter().map(|r| message_from_row!(r)).collect();
                for message in &mut messages {
                    sqlx::query(&sql(
                        "UPDATE agent_messages SET received_at = COALESCE(received_at, ?) WHERE id = ?",
                        $postgres,
                    ))
                    .bind(&now)
                    .bind(&message.id)
                    .execute(&mut *tx)
                    .await?;
                    if message.received_at.is_none() {
                        message.received_at = Some(now.clone());
                    }
                    message.status = AgentMessageStatus::Received;
                }
                tx.commit().await?;
                Ok(messages)
            }

            async fn get_agent_message(
                &self,
                session_id: &str,
                message_id: &str,
            ) -> Result<Option<AgentMessage>> {
                let query = format!("{MESSAGE_ACCESS} AND {AUTHORIZED} AND (msg.sender_session_id = ? OR msg.recipient_session_id = ?) AND msg.id = ?");
                let row = sqlx::query(&sql(&query, $postgres)).bind(session_id).bind(session_id).bind(message_id).fetch_optional(self.pool()).await?;
                Ok(row.as_ref().map(|r| message_from_row!(r)))
            }

            async fn agent_conversation_summaries(&self, session_id: &str) -> Result<Vec<(String, ConversationSummary)>> {
                let query = format!("SELECT m.id, m.text, m.sender_session_id, m.recipient_session_id, m.created_at, m.received_at, m.acknowledged_at
                    FROM ({MESSAGE_ACCESS} AND {AUTHORIZED} AND (msg.sender_session_id = ? OR msg.recipient_session_id = ?)) m
                    ORDER BY m.created_at, m.id");
                let rows = sqlx::query(&sql(&query, $postgres)).bind(session_id).bind(session_id).fetch_all(self.pool()).await?;
                let mut summaries: HashMap<String, ConversationSummary> = HashMap::new();
                for row in &rows {
                    let sender: String = row.get("sender_session_id");
                    let sent = sender == session_id;
                    let peer = if sent { row.get("recipient_session_id") } else { sender };
                    let created_at: String = row.get("created_at");
                    let received_at: Option<String> = row.get("received_at");
                    let acknowledged_at: Option<String> = row.get("acknowledged_at");
                    let summary = summaries.entry(peer).or_insert_with(|| ConversationSummary {
                        sent_count: 0,
                        received_count: 0,
                        last_message_at: String::new(),
                        last_direction: String::new(),
                        last_sent_status: None,
                        unacknowledged_incoming: 0,
                        messages: Vec::new(),
                    });
                    let text: String = row.get("text");
                    let first_line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
                    let mut title: String = first_line.chars().take(160).collect();
                    if first_line.chars().count() > 160 {
                        title.push('…');
                    }
                    summary.messages.push(ConversationMessageTitle {
                        id: row.get("id"),
                        title,
                        direction: if sent { "sent" } else { "received" }.into(),
                        created_at: created_at.clone(),
                    });
                    summary.last_message_at = created_at;
                    if sent {
                        summary.sent_count += 1;
                        summary.last_direction = "sent".into();
                        summary.last_sent_status = Some(if acknowledged_at.is_some() {
                            AgentMessageStatus::Acknowledged
                        } else if received_at.is_some() {
                            AgentMessageStatus::Received
                        } else {
                            AgentMessageStatus::Queued
                        });
                    } else {
                        summary.received_count += 1;
                        summary.last_direction = "received".into();
                        if acknowledged_at.is_none() {
                            summary.unacknowledged_incoming += 1;
                        }
                    }
                }
                let mut summaries: Vec<_> = summaries.into_iter().collect();
                summaries.sort_by(|(a_id, a), (b_id, b)| b.last_message_at.cmp(&a.last_message_at).then_with(|| a_id.cmp(b_id)));
                Ok(summaries)
            }

            async fn readable_session_ids(&self, viewer: &str, ids: &[String]) -> Result<HashSet<String>> {
                if ids.is_empty() {
                    return Ok(HashSet::new());
                }
                let placeholders = vec!["?"; ids.len()].join(", ");
                let query = format!("SELECT s.id FROM sessions s WHERE s.id IN ({placeholders}) AND (s.visibility = 'organization'
                    OR s.owner_email = ?
                    OR EXISTS (SELECT 1 FROM session_shares x WHERE x.session_id = s.id AND x.email = ?)
                    OR EXISTS (SELECT 1 FROM live_session_shares l WHERE l.live_id = s.id AND l.email = ? AND l.communication_only = FALSE))");
                let query = sql(&query, $postgres);
                let mut statement = sqlx::query(&query);
                for id in ids {
                    statement = statement.bind(id);
                }
                let rows = statement.bind(viewer).bind(viewer).bind(viewer).fetch_all(self.pool()).await?;
                Ok(rows.iter().map(|r| r.get("id")).collect())
            }
        }
    };
}

macro_rules! peer_from_row {
    ($r:expr, $postgres:expr) => {
        AgentPeer {
            session_id: $r.get("session_id"),
            project_id: $r.get("project_id"),
            title: $r.get("title"),
            agent: $r.get("agent"),
            owner_email: $r.get("owner_email"),
            // Keep Postgres timestamp ordering native in SQL, then serialize
            // the API timestamp as RFC 3339 rather than its SQL text format.
            last_seen_at: if $postgres {
                $r.get::<chrono::DateTime<Utc>, _>("last_seen_at")
                    .to_rfc3339()
            } else {
                $r.get("last_seen_at")
            },
            live_status: $r.get("live_status"),
            live_mode: $r.get("live_mode"),
        }
    };
}

macro_rules! message_from_row {
    ($r:expr) => {{
        let received_at: Option<String> = $r.get("received_at");
        let acknowledged_at: Option<String> = $r.get("acknowledged_at");
        let status = if acknowledged_at.is_some() {
            AgentMessageStatus::Acknowledged
        } else if received_at.is_some() {
            AgentMessageStatus::Received
        } else {
            AgentMessageStatus::Queued
        };
        AgentMessage {
            id: $r.get("id"),
            project_id: $r.get("project_id"),
            sender_session_id: $r.get("sender_session_id"),
            sender_owner_email: $r.get("sender_owner_email"),
            recipient_session_id: $r.get("recipient_session_id"),
            text: $r.get("text"),
            reply_to: $r.get("reply_to"),
            idempotency_key: $r.get("idempotency_key"),
            created_at: $r.get("created_at"),
            received_at,
            acknowledged_at,
            status,
        }
    }};
}

macro_rules! handoff_from_row {
    ($r:expr) => {
        AgentHandoff {
            session_id: $r.get("session_id"),
            source_session_id: $r.get("source_session_id"),
            project_id: $r.get("project_id"),
            agent: $r.get("agent"),
            brief: $r.get("brief"),
            idempotency_key: $r.get("idempotency_key"),
            status: $r.get("status"),
            error: $r.get("error"),
            created_at: $r.get("created_at"),
            model: $r.get("model"),
            variant: $r.get("variant"),
        }
    };
}

messaging_repository!(SqliteRepo, false);
messaging_repository!(PostgresRepo, true);
