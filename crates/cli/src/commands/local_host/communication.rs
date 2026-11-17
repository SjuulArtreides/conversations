//! Read-only projection of actual exchanges; never consumes or acknowledges an inbox.
use super::{
    authz::{
        envelope::{key_id, valid_session_id},
        store::open_private,
    },
    history::History,
};
use anyhow::{ensure, Context, Result};
use conversations_core::communications::{
    CommunicationExchange, CommunicationPage, CommunicationPeer,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const PAGE_BYTES: usize = 32 * 1024;
struct ExchangeRow {
    rowid: i64,
    sent: bool,
    peer: String,
    host: Option<String>,
    id: String,
    status: String,
    text: String,
    at: Option<i64>,
}

/// Cursors walk existing sent rowids, then inbox rowids. Refreshing from the beginning
/// observes in-place delivery updates and sends inserted after a previous sent page.
pub(super) fn list(
    dir: &Path,
    session: &str,
    after: Option<&str>,
    limit: u32,
    mut can_read: impl FnMut(&str) -> Result<bool>,
    running: Option<&[String]>,
) -> Result<CommunicationPage> {
    ensure!(
        valid_session_id(session) && can_read(session)?,
        "session history is not readable"
    );
    ensure!(
        (1..=100).contains(&limit),
        "communication limit must be 1..100"
    );
    let (sent, cursor) = match after {
        None => (true, 0),
        Some(value) => {
            let (phase, row) = value
                .split_once(':')
                .context("invalid communication cursor")?;
            let row = row.parse::<i64>().context("invalid communication cursor")?;
            ensure!(
                matches!(phase, "sent" | "received") && row >= 0,
                "invalid communication cursor"
            );
            (phase == "sent", row)
        }
    };
    let mut conn = open_private(dir, "authz.db")?;
    conn.pragma_update(None, "query_only", true)?;
    let tx = conn.transaction()?;
    ensure!(
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id=?1)",
            [session],
            |r| r.get::<_, bool>(0)
        )?,
        "unknown session"
    );
    let public: Vec<u8> =
        tx.query_row("SELECT public_key FROM host_identity WHERE id=1", [], |r| {
            r.get(0)
        })?;
    let own_host = key_id(&public);
    let mut rows = Vec::new();
    if sent {
        let mut query=tx.prepare("SELECT rowid,target_session,host_id,message_id,state,
            CASE WHEN json_valid(CAST(payload AS TEXT)) THEN CASE WHEN json_type(CAST(payload AS TEXT),'$.args.text')='text' THEN json_extract(CAST(payload AS TEXT),'$.args.text') END END,
            CASE WHEN json_valid(CAST(payload AS TEXT)) THEN CASE WHEN json_type(CAST(payload AS TEXT),'$.iat')='integer' THEN json_extract(CAST(payload AS TEXT),'$.iat') END END
            FROM agent_sends WHERE source_session=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3")?;
        rows.extend(
            query
                .query_map(params![session, cursor, limit + 1], |r| {
                    Ok(ExchangeRow {
                        rowid: r.get(0)?,
                        sent: true,
                        peer: r.get(1)?,
                        host: r.get(2)?,
                        id: r.get(3)?,
                        status: r.get(4)?,
                        text: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                        at: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    let remaining = limit as usize + 1 - rows.len();
    if remaining > 0 {
        let mut query=tx.prepare("SELECT rowid,source_session,source_host,message_id,state,text,admitted_at FROM agent_inbox WHERE session_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3")?;
        rows.extend(
            query
                .query_map(
                    params![session, if sent { 0 } else { cursor }, remaining],
                    |r| {
                        Ok(ExchangeRow {
                            rowid: r.get(0)?,
                            sent: false,
                            peer: r.get(1)?,
                            host: r.get(2)?,
                            id: r.get(3)?,
                            status: r.get(4)?,
                            text: r.get(5)?,
                            at: r.get(6)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    let mut page = CommunicationPage {
        peers: vec![],
        exchanges: vec![],
        next: None,
    };
    for row in rows {
        if page.exchanges.len() == limit as usize {
            return Ok(page);
        }
        let peer = peer(&tx, dir, &own_host, &row, &mut can_read, running)?;
        let mut candidate = page.clone();
        if !candidate
            .peers
            .iter()
            .any(|p| p.session_id == peer.session_id && p.host_id == peer.host_id)
        {
            candidate.peers.push(peer.clone());
        }
        candidate.exchanges.push(CommunicationExchange {
            id: row.id,
            peer_session_id: row.peer,
            host_id: peer.host_id,
            direction: if row.sent { "sent" } else { "received" }.into(),
            title: excerpt(&row.text),
            status: row.status,
            created_at: timestamp(row.at),
        });
        candidate.next = Some(format!(
            "{}:{}",
            if row.sent { "sent" } else { "received" },
            row.rowid
        ));
        if serde_json::to_vec(&serde_json::json!({"kind":"communications","peers":candidate.peers,"exchanges":candidate.exchanges,"next":candidate.next}))?.len() > PAGE_BYTES {
            ensure!(!page.exchanges.is_empty(),"communication entry exceeds the page budget");
            return Ok(page);
        }
        page = candidate;
    }
    page.next = None;
    Ok(page)
}

fn timestamp(at: Option<i64>) -> Option<String> {
    at.and_then(|at| chrono::DateTime::from_timestamp(at, 0))
        .map(|at| at.to_rfc3339())
}
fn excerpt(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect()
}
fn peer(
    conn: &Connection,
    dir: &Path,
    own_host: &str,
    row: &ExchangeRow,
    can_read: &mut impl FnMut(&str) -> Result<bool>,
    running: Option<&[String]>,
) -> Result<CommunicationPeer> {
    let local: Option<(String, String)> = if row.host.as_deref() == Some(own_host) {
        conn.query_row(
            "SELECT agent,cwd FROM sessions WHERE session_id=?1",
            [&row.peer],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    } else {
        None
    };
    let is_local = local.is_some();
    let readable = is_local && can_read(&row.peer)?;
    let mut peer = CommunicationPeer {
        session_id: row.peer.clone(),
        host_id: if is_local { None } else { row.host.clone() },
        title: if is_local {
            "Private session"
        } else {
            "External session"
        }
        .into(),
        agent: None,
        last_seen_at: None,
        connected: None,
        state: "unknown".into(),
        reachable: None,
        readable,
    };
    if let Some((agent, cwd)) = local.filter(|_| readable) {
        let summary = History::open(dir)?.summary(&row.peer)?;
        peer.title = excerpt(&summary.title.unwrap_or_else(|| {
            format!(
                "{} · {}",
                agent,
                Path::new(&cwd)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            )
        }));
        peer.agent = Some(serde_json::from_value(serde_json::Value::String(agent))?);
        peer.last_seen_at = timestamp(summary.last_updated);
        peer.connected = running.map(|ids| ids.contains(&row.peer));
        peer.state = match peer.connected {
            Some(true) => "live",
            Some(false) => "ended",
            None => "unknown",
        }
        .into();
    } else if !is_local {
        if let Some(label) = conn
            .query_row(
                "SELECT label FROM agent_targets WHERE session_id=?1 AND host_id=?2",
                params![row.peer, row.host],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            peer.title = excerpt(&label);
        }
    }
    Ok(peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::local_host::{
        authz::{
            cutover::Authority,
            envelope::Alg,
            sessions::LocalSession,
            store::{now_secs, Grant, Kind, Store},
        },
        history::History,
    };
    use conversations_core::{AgentKind, AgentLauncher};
    use rusqlite::{params, Connection};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    const SOURCE: &str = "11111111-1111-4111-8111-111111111111";
    const LOCAL: &str = "22222222-2222-4222-8222-222222222222";
    const FOREIGN: &str = "33333333-3333-4333-8333-333333333333";
    const UNRELATED: &str = "44444444-4444-4444-8444-444444444444";
    fn snapshot(conn: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
        let mut out = Vec::new();
        for table in ["agent_inbox", "agent_sends"] {
            let mut query = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let count = query.column_count();
            out.extend(
                query
                    .query_map([], |row| {
                        (0..count)
                            .map(|i| row.get(i))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap(),
            );
        }
        out
    }
    fn send(
        conn: &Connection,
        source: &str,
        target: &str,
        host: &str,
        id: &str,
        state: &str,
        at: i64,
    ) {
        let payload=json!({"iat":at,"args":{"text":format!("Message {id}\nsecret second line must stay out")}}).to_string();
        conn.execute("INSERT INTO agent_sends(source_session,target_session,idem,request_sha256,attempt,message_id,payload,sig,kid,host_id,exp,state) VALUES(?1,?2,?3,x'00',1,?3,?4,x'00','test-key',?5,1,?6)",params![source,target,id,payload.as_bytes(),host,state]).unwrap();
    }
    #[test]
    fn pages_only_actual_exchanges_preserve_state_and_respect_current_peer_read_scope() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let host = store.host_id().unwrap();
        for id in [SOURCE, LOCAL, UNRELATED] {
            store
                .insert_session(
                    &LocalSession {
                        session_id: id.into(),
                        agent: AgentKind::Claude,
                        cwd: "/local/project".into(),
                        resume_id: Some(id.into()),
                        launcher: AgentLauncher::Opencode,
                        adk_statusline: false,
                    },
                    "adopted",
                )
                .unwrap();
        }
        History::open(dir.path())
            .unwrap()
            .rename(LOCAL, "Sensitive peer title")
            .unwrap();
        let kid = store
            .enroll(
                Kind::Owner,
                Alg::Ed25519,
                &[7; 32],
                "Reader",
                &[Grant::parse(&format!("history.read:{SOURCE}")).unwrap()],
            )
            .unwrap();
        let foreign_host = super::super::authz::envelope::key_id(&[9; 32]);
        store
            .target_add(FOREIGN, &foreign_host, &[9; 32], "Pinned remote")
            .unwrap();
        store
            .target_add(
                UNRELATED,
                &foreign_host,
                &[9; 32],
                "UNRELATED PIN MUST NOT LEAK",
            )
            .unwrap();
        let conn = Connection::open(dir.path().join("authz.db")).unwrap();
        for (n, state) in [
            "prepared",
            "queued",
            "received",
            "acknowledged",
            "uncertain",
            "refused",
            "revoked",
            "expired",
        ]
        .iter()
        .enumerate()
        {
            send(
                &conn,
                SOURCE,
                FOREIGN,
                &foreign_host,
                &format!("sent-{n}"),
                state,
                n as i64 + 1,
            );
        }
        send(&conn, SOURCE, LOCAL, &host, "local-send", "prepared", 9);
        send(
            &conn,
            UNRELATED,
            FOREIGN,
            &foreign_host,
            "unrelated-send",
            "queued",
            10,
        );
        conn.execute("INSERT INTO agent_inbox(message_id,session_id,source_session,kid,text,signed_exp,payload_sha256,admitted_at,state,state_at,jti,source_host) VALUES('received-1',?1,?2,'test-key','Incoming first line',1,x'00',11,'queued',11,'incoming-op',?3)",params![SOURCE,LOCAL,host]).unwrap();
        let before = snapshot(&conn);
        let mut read = |session: &str| {
            store
                .current_epoch(
                    &Authority {
                        kid: kid.clone(),
                        dlg: None,
                        session: session.into(),
                        cap: "history.read".into(),
                        epoch: None,
                        exp: None,
                    },
                    now_secs(),
                )
                .map(|epoch| epoch.is_some())
        };
        assert!(list(dir.path(), UNRELATED, None, 2, &mut read, None).is_err());
        let mut after = None;
        let mut events = Vec::new();
        let mut peers = Vec::new();
        loop {
            let page = list(
                dir.path(),
                SOURCE,
                after.as_deref(),
                2,
                &mut read,
                Some(&[LOCAL.into()]),
            )
            .unwrap();
            assert!(page.exchanges.len() <= 2);
            assert!(serde_json::to_vec(&page).unwrap().len() < 32 * 1024);
            events.extend(page.exchanges);
            peers.extend(page.peers);
            match page.next {
                Some(cursor) => {
                    assert_ne!(Some(&cursor), after.as_ref());
                    after = Some(cursor);
                }
                None => break,
            }
        }
        assert_eq!(events.len(), 10);
        for state in [
            "prepared",
            "queued",
            "received",
            "acknowledged",
            "uncertain",
            "refused",
            "revoked",
            "expired",
        ] {
            assert!(events.iter().any(|e| e.status == state));
        }
        assert!(peers
            .iter()
            .filter(|p| p.session_id == LOCAL)
            .all(|p| !p.readable && p.agent.is_none() && p.title != "Sensitive peer title"));
        assert!(peers.iter().any(|p| p.session_id == FOREIGN
            && p.title == "Pinned remote"
            && !p.readable
            && p.host_id.as_deref() == Some(foreign_host.as_str())));
        let encoded = serde_json::to_string(&(peers, events)).unwrap();
        assert!(!encoded.contains("UNRELATED"));
        assert!(!encoded.contains("unrelated-send"));
        assert!(!encoded.contains("secret second line"));
        assert!(!encoded.contains("payload"));
        assert!(!encoded.contains("sig"));
        assert_eq!(
            snapshot(&conn),
            before,
            "listing must not deliver, acknowledge, settle, or expire records"
        );
        store
            .owner_change_grants(
                LOCAL,
                &kid,
                &[super::super::authz::envelope::Cap::HistoryRead],
                false,
                now_secs(),
            )
            .unwrap();
        let page = list(
            dir.path(),
            SOURCE,
            Some("sent:8"),
            2,
            |session| {
                store
                    .current_epoch(
                        &Authority {
                            kid: kid.clone(),
                            dlg: None,
                            session: session.into(),
                            cap: "history.read".into(),
                            epoch: None,
                            exp: None,
                        },
                        now_secs(),
                    )
                    .map(|e| e.is_some())
            },
            Some(&[LOCAL.into()]),
        )
        .unwrap();
        assert!(page.peers.iter().any(|p| p.session_id == LOCAL
            && p.readable
            && p.title == "Sensitive peer title"
            && p.host_id.is_none()
            && p.state == "live"));
        assert_eq!(snapshot(&conn), before);
    }
    #[test]
    fn unicode_exchange_pages_are_complete_bounded_and_reject_bad_cursors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let host = store.host_id().unwrap();
        store
            .insert_session(
                &LocalSession {
                    session_id: SOURCE.into(),
                    agent: AgentKind::Claude,
                    cwd: "/local".into(),
                    resume_id: None,
                    launcher: AgentLauncher::Opencode,
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        let conn = Connection::open(dir.path().join("authz.db")).unwrap();
        for n in 0..120 {
            let peer = uuid::Uuid::new_v4().to_string();
            store
                .target_add(&peer, &host, &[9; 32], &"界".repeat(160))
                .unwrap();
            send(
                &conn,
                SOURCE,
                &peer,
                &host,
                &format!("event-{n}"),
                "prepared",
                n,
            );
            let payload = json!({"iat":n,"args":{"text":"界".repeat(160)}}).to_string();
            conn.execute(
                "UPDATE agent_sends SET payload=?1 WHERE message_id=?2",
                params![payload.as_bytes(), format!("event-{n}")],
            )
            .unwrap();
        }
        let before = snapshot(&conn);
        let mut after = None;
        let mut ids = std::collections::HashSet::new();
        let mut pages = 0;
        loop {
            let page = list(
                dir.path(),
                SOURCE,
                after.as_deref(),
                100,
                |_| Ok(true),
                None,
            )
            .unwrap();
            let wire = serde_json::json!({"kind":"communications","peers":page.peers,"exchanges":page.exchanges,"next":page.next});
            assert!(serde_json::to_vec(&wire).unwrap().len() <= PAGE_BYTES);
            for event in page.exchanges {
                assert!(ids.insert(event.id));
            }
            pages += 1;
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(ids.len(), 120);
        assert!(
            pages > 2,
            "serialized metadata budget must split pages before the count limit"
        );
        for cursor in ["other:0", "sent:-1", "received:not-a-row"] {
            assert!(list(dir.path(), SOURCE, Some(cursor), 32, |_| Ok(true), None).is_err());
        }
        assert_eq!(snapshot(&conn), before);
    }
}
