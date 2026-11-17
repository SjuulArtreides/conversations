//! Durable human sharing context and attention requests.
use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

use super::{LiveShare, PostgresRepo, SqliteRepo};
use crate::error::{AppError, Result};

#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub id: String,
    pub session_id: String,
    pub session_title: String,
    pub kind: String,
    pub sender_email: String,
    pub recipient_email: String,
    pub message: String,
    pub created_at: String,
    pub read_at: Option<String>,
    pub resolved_at: Option<String>,
}

pub struct SharingUpdate {
    pub sender: Option<String>,
    pub reason: Option<String>,
    pub visibility: Option<String>,
    pub emails: Vec<String>,
    pub shares: Vec<LiveShare>,
}

#[derive(Serialize)]
pub struct AttentionRecipient {
    pub email: String,
    pub display_name: Option<String>,
}

#[async_trait]
pub trait NotificationRepository: Send + Sync {
    async fn update_sharing_context(&self, id: &str, update: &SharingUpdate) -> Result<()>;
    async fn notifications(&self, viewer: &str) -> Result<Vec<Notification>>;
    async fn attention_recipients(&self, id: &str, viewer: &str)
        -> Result<Vec<AttentionRecipient>>;
    async fn request_attention(
        &self,
        id: &str,
        sender: &str,
        recipient: &str,
        message: &str,
    ) -> Result<()>;
    async fn acknowledge_notification(&self, id: &str, viewer: &str, resolve: bool) -> Result<()>;
}

const READ_ACCESS: &str = "(s.owner_email = viewer.email OR s.visibility = 'organization'
 OR EXISTS (SELECT 1 FROM session_shares g WHERE g.session_id = s.id AND g.email = viewer.email)
 OR EXISTS (SELECT 1 FROM live_session_shares g WHERE g.live_id = s.id AND g.email = viewer.email AND g.communication_only = FALSE))";

fn sql(query: &str, postgres: bool) -> String {
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

pub fn validate_message(message: Option<&str>, required: bool) -> Result<Option<String>> {
    let message = message.map(str::trim).filter(|s| !s.is_empty());
    if required && message.is_none() {
        return Err(AppError::BadRequest("Please enter a message".into()));
    }
    if message.is_some_and(|s| s.chars().count() > 2000 || s.contains('\0')) {
        return Err(AppError::BadRequest(
            "Message must be at most 2000 characters and contain no NUL characters".into(),
        ));
    }
    Ok(message.map(str::to_owned))
}

macro_rules! notification_repository {
    ($repo:ty, $postgres:expr) => {
        #[async_trait]
        impl NotificationRepository for $repo {
            async fn update_sharing_context(&self, id: &str, update: &SharingUpdate) -> Result<()> {
                if let Some(visibility) = &update.visibility {
                    super::validate_visibility(visibility)?;
                }
                let reason = validate_message(update.reason.as_deref(), false)?;
                for share in &update.shares {
                    share.validate()?;
                }
                let mut tx = self.pool().begin().await?;
                // A published session's audience comes only from its host.
                super::publication::reject_managed!(tx, $postgres, id);
                // Serialize changes on the session so retries cannot create duplicate inbox entries.
                if sqlx::query(&sql(
                    "UPDATE sessions SET visibility = visibility WHERE id = ?",
                    $postgres,
                ))
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    == 0
                {
                    return Err(AppError::NotFound);
                }
                let owner: Option<String> = sqlx::query(&sql(
                    "SELECT owner_email FROM sessions WHERE id = ?",
                    $postgres,
                ))
                .bind(id)
                .fetch_one(&mut *tx)
                .await?
                .get("owner_email");
                if update.sender.is_some() && update.sender != owner {
                    return Err(AppError::Forbidden(
                        "Only the owner can change sharing".into(),
                    ));
                }
                let previous = sqlx::query(&sql("SELECT email FROM session_shares WHERE session_id = ? UNION SELECT email FROM live_session_shares WHERE live_id = ? AND communication_only = FALSE", $postgres)).bind(id).bind(id).fetch_all(&mut *tx).await?;
                let previous: std::collections::HashSet<String> =
                    previous.iter().map(|r| r.get("email")).collect();
                let mut recipients = std::collections::BTreeSet::new();
                if let Some(visibility) = &update.visibility {
                    sqlx::query(&sql(
                        "UPDATE sessions SET visibility = ? WHERE id = ?",
                        $postgres,
                    ))
                    .bind(visibility)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query(&sql(
                        "DELETE FROM session_shares WHERE session_id = ?",
                        $postgres,
                    ))
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                    if visibility == "restricted" {
                        for email in &update.emails {
                            let email = email.trim().to_lowercase();
                            if email.is_empty() {
                                continue;
                            }
                            sqlx::query(&sql("INSERT INTO session_shares (session_id, email) VALUES (?, ?) ON CONFLICT DO NOTHING", $postgres)).bind(id).bind(&email).execute(&mut *tx).await?;
                            recipients.insert(email);
                        }
                    }
                } else {
                    sqlx::query(&sql(
                        "DELETE FROM live_session_shares WHERE live_id = ?",
                        $postgres,
                    ))
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                    let mut shares = std::collections::BTreeMap::new();
                    for share in &update.shares {
                        let email = share.email.trim().to_lowercase();
                        if !email.is_empty() {
                            shares.insert(email, share);
                        }
                    }
                    let timestamp = if $postgres {
                        "CAST(? AS TIMESTAMPTZ)"
                    } else {
                        "?"
                    };
                    for (email, share) in shares {
                        sqlx::query(&sql(&format!("INSERT INTO live_session_shares (live_id,email,can_write,communication_only,can_communicate,created_at) VALUES (?,?,?,?,?,{timestamp})"), $postgres))
                                    .bind(id).bind(&email).bind(share.can_write).bind(!share.can_read).bind(share.can_communicate).bind(Utc::now().to_rfc3339()).execute(&mut *tx).await?;
                        if share.can_read {
                            recipients.insert(email);
                        }
                    }
                }
                if let Some(sender) = &update.sender {
                    for recipient in recipients {
                        if recipient == *sender {
                            continue;
                        }
                        let key = format!("share:{id}:{recipient}");
                        let prior = sqlx::query(&sql(
                            "SELECT message FROM session_notifications WHERE id = ?",
                            $postgres,
                        ))
                        .bind(&key)
                        .fetch_optional(&mut *tx)
                        .await?;
                        let message = reason.as_deref().unwrap_or("");
                        let reason_changed = reason.is_some()
                            && prior
                                .as_ref()
                                .is_none_or(|r| r.get::<String, _>("message") != message);
                        if !previous.contains(&recipient) || reason_changed {
                            sqlx::query(&sql("INSERT INTO session_notifications (id,session_id,kind,sender_email,recipient_email,message,created_at) VALUES (?,?,'share',?,?,?,?) ON CONFLICT (id) DO UPDATE SET sender_email=excluded.sender_email,message=excluded.message,created_at=excluded.created_at,read_at=NULL,resolved_at=NULL", $postgres))
                                        .bind(key).bind(id).bind(sender).bind(recipient).bind(message).bind(Utc::now().to_rfc3339()).execute(&mut *tx).await?;
                        }
                    }
                }
                tx.commit().await?;
                Ok(())
            }

            async fn notifications(&self, viewer: &str) -> Result<Vec<Notification>> {
                // One share-context row per recipient/session remains available after reading.
                // All unresolved attention requests remain available, independent of inbox age.
                let rows = sqlx::query(&sql(&format!("SELECT n.*, COALESCE(s.title_override, s.title) AS session_title FROM session_notifications n JOIN sessions s ON s.id=n.session_id CROSS JOIN (SELECT ? AS email) viewer WHERE n.recipient_email=viewer.email AND {READ_ACCESS} AND (n.kind='share' OR n.resolved_at IS NULL OR n.id IN (SELECT id FROM session_notifications WHERE recipient_email=viewer.email ORDER BY created_at DESC, id DESC LIMIT 100)) ORDER BY n.created_at DESC, n.id DESC"), $postgres)).bind(viewer).fetch_all(self.pool()).await?;
                Ok(rows
                    .iter()
                    .map(|r| Notification {
                        id: r.get("id"),
                        session_id: r.get("session_id"),
                        session_title: r.get("session_title"),
                        kind: r.get("kind"),
                        sender_email: r.get("sender_email"),
                        recipient_email: r.get("recipient_email"),
                        message: r.get("message"),
                        created_at: r.get("created_at"),
                        read_at: r.get("read_at"),
                        resolved_at: r.get("resolved_at"),
                    })
                    .collect())
            }

            async fn attention_recipients(
                &self,
                id: &str,
                viewer: &str,
            ) -> Result<Vec<AttentionRecipient>> {
                let visible = sqlx::query(&sql(&format!("SELECT s.id FROM sessions s CROSS JOIN (SELECT ? AS email) viewer WHERE s.id=? AND {READ_ACCESS}"), $postgres)).bind(viewer).bind(id).fetch_optional(self.pool()).await?;
                if visible.is_none() {
                    return Err(AppError::NotFound);
                }
                let rows = sqlx::query(&sql("SELECT people.email,u.display_name FROM (SELECT owner_email AS email FROM sessions WHERE id=? UNION SELECT email FROM session_shares WHERE session_id=? UNION SELECT email FROM live_session_shares WHERE live_id=? AND communication_only=FALSE) people LEFT JOIN users u ON u.email=people.email WHERE people.email IS NOT NULL AND people.email<>? ORDER BY people.email", $postgres)).bind(id).bind(id).bind(id).bind(viewer).fetch_all(self.pool()).await?;
                Ok(rows
                    .iter()
                    .map(|r| AttentionRecipient {
                        email: r.get("email"),
                        display_name: r.get("display_name"),
                    })
                    .collect())
            }

            async fn request_attention(
                &self,
                id: &str,
                sender: &str,
                recipient: &str,
                message: &str,
            ) -> Result<()> {
                let message = validate_message(Some(message), true)?.unwrap();
                let mut tx = self.pool().begin().await?;
                // Same lock as sharing mutations closes the access-check/revocation race.
                sqlx::query(&sql(
                    "UPDATE sessions SET visibility=visibility WHERE id=?",
                    $postgres,
                ))
                .bind(id)
                .execute(&mut *tx)
                .await?;
                for email in [sender, recipient] {
                    let found = sqlx::query(&sql(&format!("SELECT s.id FROM sessions s CROSS JOIN (SELECT ? AS email) viewer WHERE s.id=? AND {READ_ACCESS}"),$postgres)).bind(email).bind(id).fetch_optional(&mut *tx).await?;
                    if found.is_none() {
                        return Err(AppError::NotFound);
                    }
                }
                if sender == recipient {
                    return Err(AppError::BadRequest("Choose a colleague".into()));
                }
                // Recipients must be explicitly associated with the session, not arbitrary org addresses.
                let eligible = sqlx::query(&sql("SELECT owner_email AS email FROM sessions WHERE id=? AND owner_email=? UNION SELECT email FROM session_shares WHERE session_id=? AND email=? UNION SELECT email FROM live_session_shares WHERE live_id=? AND email=? AND communication_only=FALSE",$postgres)).bind(id).bind(recipient).bind(id).bind(recipient).bind(id).bind(recipient).fetch_optional(&mut *tx).await?;
                if eligible.is_none() {
                    return Err(AppError::NotFound);
                }
                sqlx::query(&sql("INSERT INTO session_notifications (id,session_id,kind,sender_email,recipient_email,message,created_at) VALUES (?,?,'attention',?,?,?,?)",$postgres)).bind(Uuid::new_v4().to_string()).bind(id).bind(sender).bind(recipient).bind(message).bind(Utc::now().to_rfc3339()).execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(())
            }

            async fn acknowledge_notification(&self, id: &str, viewer: &str, resolve: bool) -> Result<()> {
                let now = Utc::now().to_rfc3339();
                let result = sqlx::query(&sql(&format!("UPDATE session_notifications SET read_at=COALESCE(read_at,?), resolved_at=CASE WHEN ? THEN COALESCE(resolved_at,?) ELSE resolved_at END WHERE id=? AND recipient_email=? AND EXISTS (SELECT 1 FROM sessions s CROSS JOIN (SELECT ? AS email) viewer WHERE s.id=session_notifications.session_id AND {READ_ACCESS})"), $postgres)).bind(&now).bind(resolve).bind(&now).bind(id).bind(viewer).bind(viewer).execute(self.pool()).await?;
                if result.rows_affected() == 0 {
                    return Err(AppError::NotFound);
                }
                Ok(())
            }
        }
    }
}
notification_repository!(SqliteRepo, false);
notification_repository!(PostgresRepo, true);
