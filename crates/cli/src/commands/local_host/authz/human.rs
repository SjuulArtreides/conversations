//! Durable human notifications. Never reads or mutates the agent delivery inbox.
use super::{
    envelope::{valid_session_id, Cap},
    store::{holds_authority, Store},
};
use anyhow::Result;
use conversations_core::attention::{AttentionRequest, Identity, Notification, Subject};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// Additive extension: old wrappers continue opening minimum schema version 12.
/// Existing installations take a read-only existence check, not a DDL write lock.
pub(super) fn ensure_schema(conn: &Connection) -> Result<()> {
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='human_notifications')", [], |r|r.get(0))?;
    if !exists {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS human_notifications (
          seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
          session_id TEXT NOT NULL, sender TEXT NOT NULL, recipient TEXT NOT NULL,
          message TEXT NOT NULL, request_key TEXT NOT NULL, request_sha256 BLOB NOT NULL,
          created_at INTEGER NOT NULL, read_at INTEGER, resolved_at INTEGER,
          UNIQUE(sender, request_key));
          CREATE INDEX IF NOT EXISTS human_notifications_recipient ON human_notifications(recipient,seq DESC);")?;
    }
    Ok(())
}

pub(super) fn readable(conn: &Connection, subject: &Subject, session: &str) -> Result<bool> {
    Ok(match subject {
        Subject::LocalOwner => true,
        Subject::Credential { kid } => {
            holds_authority(conn, kid, Cap::HistoryRead, Some(session), false)?.is_none()
        }
    })
}

/// Caller supplies the authenticated sender, never a request body identity.
/// Run inside the same transaction as signed operation journaling.
pub(super) fn request(
    conn: &Connection,
    sender: &Subject,
    session: &str,
    input: &AttentionRequest,
    now: i64,
) -> Result<Option<&'static str>> {
    if !valid_session_id(session) || !valid_session_id(&input.idempotency_key) {
        return Ok(Some("invalid session or idempotency key"));
    }
    let len = input.message.chars().count();
    if len == 0 || len > 2000 || input.message.contains('\0') {
        return Ok(Some("message must contain 1 to 2000 characters"));
    }
    let known: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id=?1)",
        [session],
        |r| r.get(0),
    )?;
    if !known {
        return Ok(Some("unknown local session"));
    }
    if sender == &input.recipient {
        return Ok(Some("cannot request your own attention"));
    }
    if !readable(conn, &input.recipient, session)? {
        return Ok(Some("recipient cannot currently read this session"));
    }
    let bytes = serde_json::to_vec(&(session, &input.recipient, &input.message))?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let prior: Option<Vec<u8>> = conn
        .query_row(
            "SELECT request_sha256 FROM human_notifications WHERE sender=?1 AND request_key=?2",
            params![sender.key(), input.idempotency_key],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(prior) = prior {
        return Ok((prior != digest.as_ref())
            .then_some("idempotency key was already used for another request"));
    }
    conn.execute("INSERT INTO human_notifications(id,session_id,sender,recipient,message,request_key,request_sha256,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![uuid::Uuid::new_v4().to_string(),session,sender.key(),input.recipient.key(),input.message,input.idempotency_key,digest.as_ref(),now])?;
    Ok(None)
}

impl Store {
    pub fn owner_attention(
        &mut self,
        session: &str,
        input: &AttentionRequest,
        now: i64,
    ) -> Result<Option<&'static str>> {
        ensure_schema(&self.conn)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = request(&tx, &Subject::LocalOwner, session, input, now)?;
        tx.commit()?;
        Ok(result)
    }
}

fn subject(key: String) -> Subject {
    if key == "local_owner" {
        Subject::LocalOwner
    } else {
        Subject::Credential { kid: key }
    }
}
fn identity(conn: &Connection, subject: Subject) -> Result<Identity> {
    let label = match &subject {
        Subject::LocalOwner => "This host's owner".into(),
        Subject::Credential { kid } => conn
            .query_row(
                "SELECT label FROM credentials WHERE cred_id=?1",
                [kid],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "Unknown key".into()),
    };
    Ok(Identity { subject, label })
}
fn timestamp(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

impl Store {
    pub fn attention_recipients(&self, session: &str, sender: &Subject) -> Result<Vec<Identity>> {
        let mut recipients = Vec::new();
        let mut statement = self.conn.prepare(
            "SELECT cred_id FROM credentials WHERE revoked_at IS NULL ORDER BY label,cred_id",
        )?;
        let keys = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for candidate in std::iter::once(Subject::LocalOwner)
            .chain(keys.into_iter().map(|kid| Subject::Credential { kid }))
        {
            if &candidate != sender && readable(&self.conn, &candidate, session)? {
                recipients.push(identity(&self.conn, candidate)?);
            }
        }
        Ok(recipients)
    }
    pub fn human_notifications(
        &mut self,
        viewer: &Subject,
        after: Option<i64>,
        limit: u32,
        mut can_read: impl FnMut(&str) -> Result<bool>,
    ) -> Result<(Vec<Notification>, Option<i64>)> {
        ensure_schema(&self.conn)?;
        let mut stmt=self.conn.prepare("SELECT seq,id,session_id,sender,message,created_at,read_at,resolved_at FROM human_notifications WHERE recipient=?1 AND seq<?2 ORDER BY seq DESC")?;
        let mut rows = stmt.query(params![viewer.key(), after.unwrap_or(i64::MAX)])?;
        let mut notifications = Vec::new();
        let mut last = None;
        let mut budget = 0;
        while let Some(row) = rows.next()? {
            let session: String = row.get(2)?;
            if !readable(&self.conn, viewer, &session)? || !can_read(&session)? {
                continue;
            }
            let notification = Notification {
                id: row.get(1)?,
                session_id: session.clone(),
                session_title: session,
                kind: "attention".into(),
                sender: identity(&self.conn, subject(row.get(3)?))?,
                recipient: viewer.clone(),
                message: row.get(4)?,
                created_at: timestamp(row.get(5)?),
                read_at: row.get::<_, Option<i64>>(6)?.map(timestamp),
                resolved_at: row.get::<_, Option<i64>>(7)?.map(timestamp),
            };
            let size = serde_json::to_vec(&notification)?.len() + 1200;
            if notifications.len() >= limit.clamp(1, 50) as usize || budget + size > 28 * 1024 {
                return Ok((notifications, last));
            }
            budget += size;
            last = Some(row.get(0)?);
            notifications.push(notification);
        }
        Ok((notifications, None))
    }
    pub fn owner_notification_mark(
        &mut self,
        session: &str,
        id: &str,
        resolve: bool,
        now: i64,
    ) -> Result<Option<&'static str>> {
        ensure_schema(&self.conn)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = mark(&tx, &Subject::LocalOwner, session, id, resolve, now)?;
        tx.commit()?;
        Ok(result)
    }
}
pub(super) fn mark(
    conn: &Connection,
    viewer: &Subject,
    session: &str,
    id: &str,
    resolve: bool,
    now: i64,
) -> Result<Option<&'static str>> {
    if !valid_session_id(session) || !valid_session_id(id) {
        return Ok(Some("invalid notification"));
    }
    if !readable(conn, viewer, session)? {
        return Ok(Some("session is no longer readable"));
    }
    let changed=conn.execute("UPDATE human_notifications SET read_at=COALESCE(read_at,?4),resolved_at=CASE WHEN ?5 THEN COALESCE(resolved_at,?4) ELSE resolved_at END WHERE id=?1 AND session_id=?2 AND recipient=?3",params![id,session,viewer.key(),now,resolve])?;
    Ok((changed == 0).then_some("notification not found"))
}

impl Store {
    pub fn human_subject(&self, op: &super::envelope::Verified, now: i64) -> Result<Subject> {
        let kid = match &op.dlg {
            None => op.kid.clone(),
            Some(dlg) => super::store::delegated_parent(
                &self.conn,
                &op.kid,
                op.op.capability(),
                op.op.session_id(),
                dlg,
                now,
            )?
            .map_err(anyhow::Error::msg)?,
        };
        Ok(Subject::Credential { kid })
    }
    pub fn can_read_session(
        &mut self,
        op: &super::envelope::Verified,
        session: &str,
        now: i64,
    ) -> Result<bool> {
        let mut probe = op.clone();
        probe.op = super::envelope::Operation::HistoryGet {
            session: session.into(),
            after: 0,
            limit: 1,
            digest: false,
        };
        let tx = self.conn.transaction()?;
        let readable = super::store::current(&tx, &probe, now)?.is_ok();
        tx.commit()?;
        Ok(readable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::local_host::authz::{
        envelope::Alg,
        sessions::LocalSession,
        store::{Grant, Kind},
    };
    #[test]
    fn notifications_page_complete_unicode_bodies_and_hide_revoked_content() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        store
            .insert_session(
                &LocalSession {
                    session_id: session.clone(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/fixture".into(),
                    resume_id: None,
                    launcher: Default::default(),
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        let kid = store
            .enroll(
                Kind::Owner,
                Alg::Ed25519,
                &[52; 32],
                "Reader",
                &[Grant::parse(&format!("history.read:{session}")).unwrap()],
            )
            .unwrap();
        let viewer = Subject::Credential { kid: kid.clone() };
        for _ in 0..12 {
            assert_eq!(
                store
                    .owner_attention(
                        &session,
                        &AttentionRequest {
                            recipient: viewer.clone(),
                            message: "🦀".repeat(2000),
                            idempotency_key: uuid::Uuid::new_v4().to_string()
                        },
                        100
                    )
                    .unwrap(),
                None
            );
        }
        let mut after = None;
        let mut ids = std::collections::HashSet::new();
        let mut pages = 0;
        loop {
            let (notifications, next) = store
                .human_notifications(&viewer, after, 50, |_| Ok(true))
                .unwrap();
            assert!(serde_json::to_vec(&serde_json::json!({"kind":"notifications","notifications":notifications,"next":next})).unwrap().len()<=32*1024);
            for item in notifications {
                assert_eq!(item.message, "🦀".repeat(2000));
                assert!(ids.insert(item.id));
            }
            pages += 1;
            after = next;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(ids.len(), 12);
        assert!(pages > 1);
        assert!(store
            .human_notifications(&viewer, None, 50, |_| Ok(false))
            .unwrap()
            .0
            .is_empty());
        store
            .conn
            .execute(
                "UPDATE credentials SET revoked_at=101 WHERE cred_id=?1",
                [kid],
            )
            .unwrap();
        assert!(store
            .human_notifications(&viewer, None, 50, |_| Ok(true))
            .unwrap()
            .0
            .is_empty());
    }
    #[test]
    fn attention_extension_preserves_old_schema_and_revocation_denies_new_requests() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut old = Store::open(dir.path()).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        old.insert_session(
            &LocalSession {
                session_id: session.clone(),
                agent: conversations_core::AgentKind::Claude,
                cwd: dir.path().to_string_lossy().into(),
                resume_id: None,
                launcher: Default::default(),
                adk_statusline: false,
            },
            "adopted",
        )
        .unwrap();
        let kid = old
            .enroll(
                Kind::Owner,
                Alg::Ed25519,
                &[51; 32],
                "Reader",
                &[Grant::parse(&format!("history.read:{session}")).unwrap()],
            )
            .unwrap();
        let before: i64 = old
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        let mut new = Store::open(dir.path()).unwrap();
        let input = AttentionRequest {
            recipient: Subject::Credential { kid: kid.clone() },
            message: "Check result".into(),
            idempotency_key: uuid::Uuid::new_v4().to_string(),
        };
        assert_eq!(new.owner_attention(&session, &input, 100).unwrap(), None);
        assert_eq!(
            old.conn
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            before
        );
        assert!(old.session(&session).unwrap().is_some());
        assert!(Store::open(dir.path())
            .unwrap()
            .session(&session)
            .unwrap()
            .is_some());
        old.conn
            .execute("UPDATE grants SET revoked_at=101 WHERE cred_id=?1", [kid])
            .unwrap();
        assert_eq!(
            new.owner_attention(&session, &input, 102).unwrap(),
            Some("recipient cannot currently read this session")
        );
        assert_eq!(
            new.conn
                .query_row("SELECT count(*) FROM human_notifications", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            new.conn
                .query_row("SELECT count(*) FROM agent_inbox", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
