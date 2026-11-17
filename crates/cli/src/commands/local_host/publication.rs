//! Explicit publication of locally canonical history.
//!
//! The local `history.db` is the source of truth. Publishing a session is a
//! local decision recorded here (audience, owner, destination server and a
//! strictly increasing share revision); the publisher then sends that history
//! to the central server as host-signed operations
//! (`conversations_core::publication`). Ordinary launching and tailing never
//! publish anything: a session without a row here is private.
//!
//! Every request is frozen in the outbox (its exact bytes) before it is sent
//! and resent byte-for-byte until the server acknowledges it, so a lost
//! response or a restart cannot skip, duplicate or reorder content. Progress
//! (`publication_sent`: which version of which message the server holds) only
//! advances on an acknowledgement for exactly the frozen request.
//!
//! Unsharing durably moves to a newer revision BEFORE anything is sent; the
//! old outbox entry is discarded and the server's tombstone at that revision
//! refuses any older request still in flight. Removing a publication never
//! touches local history or the session's agent.
//!
//! One pump at a time per runtime directory, across processes (the host's loop
//! and `upload`): requests are sent and settled strictly one after another.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
pub use conversations_core::publication::Audience;
use conversations_core::publication::{
    signed_message, PublicationAck, PublicationAction, PublicationOp, PublicationRequest,
    PublicationState, MAX_AUDIENCE_EMAILS, MAX_CHUNK_BYTES, MAX_CHUNK_MESSAGES, MAX_OP_BYTES,
    PUBLICATION_VERSION,
};
use conversations_core::{AgentKind, Message, Role};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

use super::authz::envelope::{key_id, valid_session_id};
use super::authz::store::{now_secs, open_private, Store};
use super::history::History;

const SCHEMA: &str = r#"
-- One row per session the local user ever shared. `rev` only grows; `phase`
-- is what the publisher is doing for that revision.
CREATE TABLE IF NOT EXISTS publications(
  session_id TEXT PRIMARY KEY,
  phase TEXT NOT NULL CHECK(phase IN ('publishing','removing','private','error')),
  rev INTEGER NOT NULL CHECK(rev >= 1),
  audience TEXT,
  owner TEXT, server TEXT NOT NULL,
  announced INTEGER NOT NULL DEFAULT 0,
  next_seq INTEGER NOT NULL DEFAULT 0 CHECK(next_seq >= 0),
  error TEXT, last_attempt TEXT,
  updated_at INTEGER NOT NULL);
-- The version (history sha256) of each message the server acknowledged for the
-- current revision.
CREATE TABLE IF NOT EXISTS publication_sent(
  session_id TEXT NOT NULL, uuid TEXT NOT NULL, sha256 BLOB NOT NULL,
  PRIMARY KEY(session_id, uuid));
-- At most one frozen request per session: exact bytes, resent until settled.
CREATE TABLE IF NOT EXISTS publication_outbox(
  session_id TEXT PRIMARY KEY, rev INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('share','append','unshare')), seq INTEGER,
  request BLOB NOT NULL, entries TEXT NOT NULL, created_at INTEGER NOT NULL);
"#;

/// What a session's publication is doing, as reported to the local user and
/// to a signed status request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Private,
    Syncing,
    Shared,
    DeletionPending,
    Error,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Private => "private",
            State::Syncing => "syncing",
            State::Shared => "shared",
            State::DeletionPending => "deletion_pending",
            State::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub session: String,
    pub state: State,
    pub rev: u64,
    pub audience: Option<Audience>,
    pub owner: Option<String>,
    pub server: Option<String>,
    /// Messages the server holds at their current local version.
    pub published: u64,
    /// Local messages (new or changed) not yet acknowledged.
    pub pending: u64,
    /// Why publishing stopped (state `error`).
    pub error: Option<String>,
    /// The last transient failure (offline, login missing); retried.
    pub last_attempt: Option<String>,
}

/// Where and as whom the publisher sends. Only rows bound to exactly this
/// server are published.
pub struct Destination<'a> {
    pub server: &'a str,
    pub client: &'a reqwest::Client,
    pub cookie: Option<&'a str>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pumped {
    /// Requests the server acknowledged.
    pub sent: usize,
    /// Requests whose outcome is unknown (kept and resent later).
    pub transport_errors: usize,
    /// Another pump (the host's loop or an `upload`) holds this runtime
    /// directory's publication; nothing was sent.
    pub busy: bool,
}

fn open(dir: &Path) -> Result<Connection> {
    // Creates (or checks) the history schema first: publication reads it.
    drop(History::open(dir)?);
    let conn = open_private(dir, "history.db")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(SCHEMA)
        .context("creating publication schema")?;
    Ok(conn)
}

/// Validate and canonicalize an audience: lowercase, sorted, unique emails.
/// `write_pending` names audience members intended to type into the session
/// through this host once their key is trusted; each must also be a reader.
pub fn normalize(audience: Audience) -> Result<Audience> {
    Ok(match audience {
        Audience::Organization {} => Audience::Organization {},
        Audience::Restricted {
            emails,
            write_pending,
        } => {
            let mut emails: Vec<String> = emails.iter().map(|e| e.trim().to_lowercase()).collect();
            emails.sort();
            emails.dedup();
            ensure!(
                emails.len() <= MAX_AUDIENCE_EMAILS,
                "at most {MAX_AUDIENCE_EMAILS} people can be named"
            );
            for email in &emails {
                ensure!(valid_email(email), "not an email address: {email}");
            }
            let mut write_pending: Vec<String> = write_pending
                .iter()
                .map(|e| e.trim().to_lowercase())
                .collect();
            write_pending.sort();
            write_pending.dedup();
            ensure!(
                write_pending.len() <= MAX_AUDIENCE_EMAILS,
                "at most {MAX_AUDIENCE_EMAILS} people can be named"
            );
            for email in &write_pending {
                ensure!(
                    emails.iter().any(|r| r == email),
                    "{email} is named to write but not to read"
                );
            }
            Audience::Restricted {
                emails,
                write_pending,
            }
        }
    })
}

pub fn valid_email(email: &str) -> bool {
    let mut parts = email.splitn(2, '@');
    let (local, domain) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    !local.is_empty()
        && domain.contains('.')
        && email.len() <= 254
        && !email
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == ',')
}

/// `publications` columns: phase, rev, audience, owner, server.
type SharedRow = (String, i64, Option<String>, Option<String>, String);
/// `publications` columns: phase, rev, audience, owner, announced, next_seq.
type PendingRow = (String, i64, Option<String>, Option<String>, bool, i64);

/// Share (or change the audience of) a known local session at a new revision.
/// Re-sharing exactly what is already being published changes nothing.
pub fn share(
    dir: &Path,
    session: &str,
    audience: Audience,
    owner: Option<&str>,
    server: &str,
) -> Result<Status> {
    ensure!(valid_session_id(session), "not a session id: {session}");
    ensure!(
        owner.is_none_or(valid_email),
        "the publishing account is not an email address"
    );
    ensure!(!server.is_empty(), "no server to publish to");
    let audience = normalize(audience)?;
    let audience_json = serde_json::to_string(&audience)?;
    let mut conn = open(dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let known: i64 = tx.query_row(
        "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
        params![session],
        |r| r.get(0),
    )?;
    ensure!(known > 0, "no local history for session {session}");
    let current: Option<SharedRow> = tx
        .query_row(
            "SELECT phase, rev, audience, owner, server FROM publications WHERE session_id = ?1",
            params![session],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    if let Some((phase, _, _, bound_owner, bound_server)) = &current {
        // A central copy may exist (or be mid-deletion) on the bound server as
        // the bound owner. Rebinding would abandon it, so it moves only once
        // its deletion is acknowledged.
        if phase != "private" && (bound_owner.as_deref() != owner || bound_server != server) {
            let bound = describe_binding(bound_owner.as_deref(), bound_server);
            if phase == "removing" {
                bail!(
                    "session {session} was unshared {bound} and its deletion there is still \
                     pending; wait until `history status {session}` shows private, then \
                     share it again"
                );
            }
            bail!(
                "session {session} is published {bound}, not {}; unshare it first \
                 (`history unshare {session}`) and share it again once \
                 `history status {session}` shows private",
                describe_binding(owner, server)
            );
        }
    }
    let unchanged = current.as_ref().is_some_and(|(phase, _, a, o, s)| {
        phase == "publishing"
            && a.as_deref() == Some(audience_json.as_str())
            && o.as_deref() == owner
            && s == server
    });
    if !unchanged {
        let rev = current.as_ref().map_or(0, |c| c.1) + 1;
        restart(
            &tx,
            session,
            rev,
            "publishing",
            Some(&audience_json),
            owner,
            server,
        )?;
    }
    let status = read_status(&tx, session)?;
    tx.commit()?;
    Ok(status)
}

fn describe_binding(owner: Option<&str>, server: &str) -> String {
    match owner {
        Some(owner) => format!("as {owner} on {server}"),
        None => format!("without an account on {server}"),
    }
}

/// Stop publishing: record a newer revision and queue its deletion. Nothing
/// local is removed. Unsharing a session that was never shared is a no-op.
pub fn unshare(dir: &Path, session: &str) -> Result<Status> {
    ensure!(valid_session_id(session), "not a session id: {session}");
    let mut conn = open(dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: Option<(String, i64, Option<String>, String)> = tx
        .query_row(
            "SELECT phase, rev, owner, server FROM publications WHERE session_id = ?1",
            params![session],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((phase, rev, owner, server)) = current {
        if phase != "private" && phase != "removing" {
            restart(
                &tx,
                session,
                rev + 1,
                "removing",
                None,
                owner.as_deref(),
                &server,
            )?;
        }
    }
    let status = read_status(&tx, session)?;
    tx.commit()?;
    Ok(status)
}

/// Move a session to a fresh revision: forget the old revision's progress and
/// any frozen request of it.
fn restart(
    tx: &rusqlite::Transaction<'_>,
    session: &str,
    rev: i64,
    phase: &str,
    audience: Option<&str>,
    owner: Option<&str>,
    server: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO publications(session_id, phase, rev, audience, owner, server, announced, next_seq, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7)
         ON CONFLICT(session_id) DO UPDATE SET phase = ?2, rev = ?3, audience = ?4, owner = ?5,
           server = ?6, announced = 0, next_seq = 0, error = NULL, last_attempt = NULL, updated_at = ?7",
        params![session, phase, rev, audience, owner, server, now_secs()],
    )?;
    tx.execute(
        "DELETE FROM publication_outbox WHERE session_id = ?1",
        params![session],
    )?;
    tx.execute(
        "DELETE FROM publication_sent WHERE session_id = ?1",
        params![session],
    )?;
    Ok(())
}

pub fn status(dir: &Path, session: &str) -> Result<Status> {
    ensure!(valid_session_id(session), "not a session id: {session}");
    let conn = open(dir)?;
    read_status(&conn, session)
}

fn unsent(conn: &Connection, session: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM messages m LEFT JOIN publication_sent p
           ON p.session_id = m.session_id AND p.uuid = m.uuid
         WHERE m.session_id = ?1 AND (p.sha256 IS NULL OR p.sha256 != m.sha256)",
        params![session],
        |r| r.get(0),
    )?)
}

fn read_status(conn: &Connection, session: &str) -> Result<Status> {
    type Row = (
        String,
        i64,
        Option<String>,
        Option<String>,
        String,
        bool,
        Option<String>,
        Option<String>,
    );
    let row: Option<Row> = conn
        .query_row(
            "SELECT phase, rev, audience, owner, server, announced, error, last_attempt
             FROM publications WHERE session_id = ?1",
            params![session],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()?;
    let Some((phase, rev, audience, owner, server, announced, error, last_attempt)) = row else {
        return Ok(Status {
            session: session.to_owned(),
            state: State::Private,
            rev: 0,
            audience: None,
            owner: None,
            server: None,
            published: 0,
            pending: 0,
            error: None,
            last_attempt: None,
        });
    };
    let published: i64 = conn.query_row(
        "SELECT COUNT(*) FROM publication_sent p JOIN messages m
           ON m.session_id = p.session_id AND m.uuid = p.uuid AND m.sha256 = p.sha256
         WHERE p.session_id = ?1",
        params![session],
        |r| r.get(0),
    )?;
    let pending = if phase == "publishing" || phase == "error" {
        unsent(conn, session)?
    } else {
        0
    };
    let queued: i64 = conn.query_row(
        "SELECT COUNT(*) FROM publication_outbox WHERE session_id = ?1",
        params![session],
        |r| r.get(0),
    )?;
    let state = match phase.as_str() {
        "removing" => State::DeletionPending,
        "private" => State::Private,
        "error" => State::Error,
        _ if !announced || queued > 0 || pending > 0 => State::Syncing,
        _ => State::Shared,
    };
    Ok(Status {
        session: session.to_owned(),
        state,
        rev: rev as u64,
        audience: audience.as_deref().map(serde_json::from_str).transpose()?,
        owner,
        server: Some(server),
        published: published as u64,
        pending: pending as u64,
        error,
        last_attempt,
    })
}

/// A frozen request: exactly what is sent, and what its acknowledgement settles.
#[derive(Debug, Clone)]
struct Frozen {
    session: String,
    rev: i64,
    kind: String,
    seq: Option<i64>,
    request: Vec<u8>,
    /// `(uuid, history sha256)` of each message in an append chunk.
    entries: Vec<(String, Vec<u8>)>,
}

/// `publication_outbox` columns: rev, kind, seq, request, entries.
type OutboxRow = (i64, String, Option<i64>, Vec<u8>, String);

/// The pending request for `session` on `server`: the frozen one if any, else
/// the next one built from local state and frozen now. `None`: nothing to send,
/// including when the session is no longer bound to `server`.
fn prepare(dir: &Path, session: &str, server: &str) -> Result<Option<Frozen>> {
    let mut store = Store::open(dir)?;
    let host_key = store.host_public_key()?;
    let mut conn = open(dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let bound: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM publications WHERE session_id = ?1 AND server = ?2)",
        params![session, server],
        |r| r.get(0),
    )?;
    if !bound {
        return Ok(None);
    }
    let frozen: Option<OutboxRow> = tx
        .query_row(
            "SELECT rev, kind, seq, request, entries FROM publication_outbox WHERE session_id = ?1",
            params![session],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    if let Some((rev, kind, seq, request, entries)) = frozen {
        tx.commit()?;
        return Ok(Some(Frozen {
            session: session.to_owned(),
            rev,
            kind,
            seq,
            request,
            entries: decode_entries(&entries)?,
        }));
    }
    let row: Option<PendingRow> = tx
        .query_row(
            "SELECT phase, rev, audience, owner, announced, next_seq FROM publications WHERE session_id = ?1",
            params![session],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?;
    let Some((phase, rev, audience, owner, announced, next_seq)) = row else {
        return Ok(None);
    };
    let (kind, seq, action, chunk, entries) = match phase.as_str() {
        "removing" => (
            "unshare",
            None,
            PublicationAction::Unshare {},
            None,
            Vec::new(),
        ),
        "publishing" if !announced => {
            let audience: Audience =
                serde_json::from_str(audience.as_deref().context("shared without audience")?)?;
            let (agent, title) = describe(&tx, session)?;
            (
                "share",
                None,
                PublicationAction::Share {
                    audience,
                    agent,
                    title,
                },
                None,
                Vec::new(),
            )
        }
        "publishing" => match next_chunk(&tx, session)? {
            Chunk::Empty => return Ok(None),
            Chunk::Blocked(reason) => {
                tx.execute(
                    "UPDATE publications SET phase = 'error', error = ?2, updated_at = ?3 WHERE session_id = ?1",
                    params![session, reason, now_secs()],
                )?;
                tx.commit()?;
                return Ok(None);
            }
            Chunk::Ready { bytes, entries } => {
                let digest = B64.encode(ring::digest::digest(&ring::digest::SHA256, &bytes));
                (
                    "append",
                    Some(next_seq),
                    PublicationAction::Append {
                        seq: next_seq as u64,
                        chunk_sha256: digest,
                    },
                    Some(bytes),
                    entries,
                )
            }
        },
        _ => return Ok(None),
    };
    let op = PublicationOp {
        v: PUBLICATION_VERSION,
        host: key_id(&host_key),
        session: session.to_owned(),
        owner,
        rev: rev as u64,
        action,
    };
    let op_bytes = serde_json::to_vec(&op)?;
    if op_bytes.len() > MAX_OP_BYTES {
        // Only an audience can make it this large; stop this session visibly
        // rather than every session behind it.
        tx.execute(
            "UPDATE publications SET phase = 'error', error = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![
                session,
                format!(
                    "the audience is too long to publish ({} of at most {MAX_OP_BYTES} bytes); share with fewer people or the organization",
                    op_bytes.len()
                ),
                now_secs()
            ],
        )?;
        tx.commit()?;
        return Ok(None);
    }
    let sig = store.host_sign(&signed_message(&op_bytes))?;
    let request = serde_json::to_vec(&PublicationRequest {
        host_key: B64.encode(&host_key),
        op: B64.encode(&op_bytes),
        sig: B64.encode(sig),
        chunk: chunk.as_deref().map(|c| B64.encode(c)),
    })?;
    tx.execute(
        "INSERT INTO publication_outbox(session_id, rev, kind, seq, request, entries, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            session,
            rev,
            kind,
            seq,
            request,
            encode_entries(&entries),
            now_secs()
        ],
    )?;
    tx.commit()?;
    Ok(Some(Frozen {
        session: session.to_owned(),
        rev,
        kind: kind.to_owned(),
        seq,
        request,
        entries,
    }))
}

fn encode_entries(entries: &[(String, Vec<u8>)]) -> String {
    let list: Vec<(&str, String)> = entries
        .iter()
        .map(|(uuid, sha)| (uuid.as_str(), B64.encode(sha)))
        .collect();
    serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())
}

fn decode_entries(text: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let list: Vec<(String, String)> = serde_json::from_str(text)?;
    list.into_iter()
        .map(|(uuid, sha)| Ok((uuid, B64.decode(sha)?)))
        .collect()
}

/// The agent and a title for the share: the first user message's first line.
fn describe(conn: &Connection, session: &str) -> Result<(AgentKind, String)> {
    let agent: String = conn.query_row(
        "SELECT agent FROM messages WHERE session_id = ?1 ORDER BY seq LIMIT 1",
        params![session],
        |r| r.get(0),
    )?;
    let agent = agent.parse().unwrap_or(AgentKind::Claude);
    let mut statement = conn
        .prepare("SELECT message_json FROM messages WHERE session_id = ?1 ORDER BY seq LIMIT 50")?;
    let mut rows = statement.query(params![session])?;
    while let Some(row) = rows.next()? {
        let json: String = row.get(0)?;
        let Ok(message) = serde_json::from_str::<Message>(&json) else {
            continue;
        };
        if message.role != Role::User {
            continue;
        }
        let line = message.text.lines().map(str::trim).find(|l| !l.is_empty());
        if let Some(line) = line {
            let title: String = line.chars().filter(|c| !c.is_control()).take(120).collect();
            if !title.trim().is_empty() {
                return Ok((agent, title));
            }
        }
    }
    Ok((agent, "Untitled session".into()))
}

enum Chunk {
    Empty,
    Blocked(String),
    Ready {
        bytes: Vec<u8>,
        entries: Vec<(String, Vec<u8>)>,
    },
}

/// The next messages the server does not hold at their current version, in
/// local order (the order of first appearance), as one bounded chunk. New
/// messages always have a later position than every earlier one, so sending a
/// prefix of this list never leaves a hole and never moves a message: an
/// updated message is replaced in place by the server.
fn next_chunk(conn: &Connection, session: &str) -> Result<Chunk> {
    const OVERHEAD: usize = br#"{"messages":[]}"#.len();
    let mut statement = conn.prepare(
        "SELECT m.uuid, m.message_json, m.sha256 FROM messages m LEFT JOIN publication_sent p
           ON p.session_id = m.session_id AND p.uuid = m.uuid
         WHERE m.session_id = ?1 AND (p.sha256 IS NULL OR p.sha256 != m.sha256)
         ORDER BY m.seq LIMIT ?2",
    )?;
    let mut rows = statement.query(params![session, MAX_CHUNK_MESSAGES as i64])?;
    let mut bytes = br#"{"messages":["#.to_vec();
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        let (uuid, json, sha): (String, String, Vec<u8>) = (row.get(0)?, row.get(1)?, row.get(2)?);
        let readable = serde_json::from_str::<Message>(&json)
            .is_ok_and(|message| message.uuid == uuid && !uuid.is_empty());
        if !readable {
            if entries.is_empty() {
                return Ok(Chunk::Blocked(format!(
                    "message {uuid} cannot be read from local history; publishing stopped before it"
                )));
            }
            break;
        }
        let separator = usize::from(!entries.is_empty());
        if bytes.len() + separator + json.len() + 2 > MAX_CHUNK_BYTES {
            if entries.is_empty() {
                return Ok(Chunk::Blocked(format!(
                    "message {uuid} is larger than the {} byte publication limit; publishing stopped before it",
                    MAX_CHUNK_BYTES - OVERHEAD
                )));
            }
            break;
        }
        if separator == 1 {
            bytes.push(b',');
        }
        bytes.extend_from_slice(json.as_bytes());
        entries.push((uuid, sha));
    }
    if entries.is_empty() {
        return Ok(Chunk::Empty);
    }
    bytes.extend_from_slice(b"]}");
    Ok(Chunk::Ready { bytes, entries })
}

enum Outcome {
    Ack(PublicationAck),
    /// Definitively refused: retrying the same bytes cannot succeed.
    Refused(String),
    /// Unknown: the request may or may not have been applied. Kept and resent.
    Transport(String),
}

/// Apply the outcome of `frozen`, if it is still the session's frozen request
/// (an unshare or re-share in the meantime discarded it).
fn settle(dir: &Path, frozen: &Frozen, outcome: &Outcome) -> Result<()> {
    let mut conn = open(dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: Option<Vec<u8>> = tx
        .query_row(
            "SELECT request FROM publication_outbox WHERE session_id = ?1 AND rev = ?2",
            params![frozen.session, frozen.rev],
            |r| r.get(0),
        )
        .optional()?;
    if current.as_deref() != Some(frozen.request.as_slice()) {
        return Ok(());
    }
    let session = frozen.session.as_str();
    let now = now_secs();
    let refuse = |tx: &rusqlite::Transaction<'_>, reason: &str| -> Result<()> {
        tx.execute(
            "UPDATE publications SET phase = 'error', error = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session, reason, now],
        )?;
        tx.execute(
            "DELETE FROM publication_outbox WHERE session_id = ?1",
            params![session],
        )?;
        Ok(())
    };
    match outcome {
        Outcome::Transport(reason) => {
            tx.execute(
                "UPDATE publications SET last_attempt = ?2 WHERE session_id = ?1",
                params![session, reason],
            )?;
        }
        Outcome::Refused(reason) => refuse(&tx, reason)?,
        Outcome::Ack(ack) => {
            let expected = match frozen.kind.as_str() {
                "share" => ack.state == PublicationState::Active,
                "append" => {
                    ack.state == PublicationState::Active
                        && Some(ack.next_seq as i64) == frozen.seq.map(|s| s + 1)
                }
                _ => ack.state == PublicationState::Deleted,
            };
            if ack.session != frozen.session || ack.rev != frozen.rev as u64 || !expected {
                refuse(
                    &tx,
                    "the server acknowledged something other than what was sent",
                )?;
            } else {
                match frozen.kind.as_str() {
                    "share" => {
                        tx.execute(
                            "UPDATE publications SET announced = 1, next_seq = ?2, last_attempt = NULL, updated_at = ?3
                             WHERE session_id = ?1",
                            params![session, ack.next_seq as i64, now],
                        )?;
                    }
                    "append" => {
                        for (uuid, sha) in &frozen.entries {
                            tx.execute(
                                "INSERT INTO publication_sent(session_id, uuid, sha256) VALUES(?1, ?2, ?3)
                                 ON CONFLICT(session_id, uuid) DO UPDATE SET sha256 = ?3",
                                params![session, uuid, sha],
                            )?;
                        }
                        tx.execute(
                            "UPDATE publications SET next_seq = ?2, last_attempt = NULL, updated_at = ?3
                             WHERE session_id = ?1",
                            params![session, ack.next_seq as i64, now],
                        )?;
                    }
                    _ => {
                        tx.execute(
                            "UPDATE publications SET phase = 'private', last_attempt = NULL, updated_at = ?2
                             WHERE session_id = ?1",
                            params![session, now],
                        )?;
                        tx.execute(
                            "DELETE FROM publication_sent WHERE session_id = ?1",
                            params![session],
                        )?;
                    }
                }
                tx.execute(
                    "DELETE FROM publication_outbox WHERE session_id = ?1",
                    params![session],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Sessions with publication work for this destination, pending deletions
/// first.
fn active(dir: &Path, server: &str) -> Result<Vec<String>> {
    let conn = open(dir)?;
    let mut statement = conn.prepare(
        "SELECT session_id FROM publications WHERE phase IN ('publishing','removing') AND server = ?1
         ORDER BY phase = 'removing' DESC, updated_at",
    )?;
    let rows = statement.query_map(params![server], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await?
}

/// Requests one pump sends for a session before moving on, so a session whose
/// history keeps changing cannot hold back the others.
const REQUESTS_PER_SESSION: usize = 8;

/// Send what is due for `to`, one request at a time per session and at most
/// `REQUESTS_PER_SESSION` each; the rest waits for the next pump. A session
/// stops at its first unknown outcome; its frozen request is resent on the
/// next pump. While another pump holds the directory, it returns at once as
/// `busy`, without waiting.
pub async fn pump(dir: &Path, to: &Destination<'_>) -> Result<Pumped> {
    let Some(_serialized) = PumpLock::try_acquire(dir)? else {
        return Ok(Pumped {
            busy: true,
            ..Pumped::default()
        });
    };
    let mut pumped = Pumped::default();
    let sessions = {
        let (dir, server) = (dir.to_path_buf(), to.server.to_owned());
        blocking(move || active(&dir, &server)).await?
    };
    for session in sessions {
        for _ in 0..REQUESTS_PER_SESSION {
            let frozen = {
                let (dir, session, server) =
                    (dir.to_path_buf(), session.clone(), to.server.to_owned());
                blocking(move || prepare(&dir, &session, &server)).await?
            };
            let Some(frozen) = frozen else { break };
            let outcome = send(to, &frozen.request).await;
            let stop = !matches!(outcome, Outcome::Ack(_));
            match &outcome {
                Outcome::Ack(_) => pumped.sent += 1,
                Outcome::Transport(_) => pumped.transport_errors += 1,
                Outcome::Refused(_) => {}
            }
            {
                let dir = dir.to_path_buf();
                let frozen = frozen.clone();
                blocking(move || settle(&dir, &frozen, &outcome)).await?;
            }
            if stop {
                break;
            }
        }
    }
    Ok(pumped)
}

/// The runtime directory's publication lock, held for one whole pump. Closing
/// the file (dropping this) releases it, also when the process dies.
struct PumpLock(#[allow(dead_code)] std::fs::File);

impl PumpLock {
    /// The lock, or `None` while another pump holds it. Never waits.
    fn try_acquire(dir: &Path) -> Result<Option<Self>> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(dir.join("publication.lock"))
            .context("opening the publication lock")?;
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            bail!("could not lock publication: {error}");
        }
        Ok(Some(Self(file)))
    }
}

async fn send(to: &Destination<'_>, request: &[u8]) -> Outcome {
    let mut builder = to
        .client
        .post(format!(
            "{}/api/publications",
            to.server.trim_end_matches('/')
        ))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(Duration::from_secs(30))
        .body(request.to_vec());
    if let Some(cookie) = to.cookie {
        builder = builder.header(reqwest::header::COOKIE, cookie);
    }
    let response = match builder.send().await {
        Ok(response) => response,
        Err(error) => return Outcome::Transport(format!("server unreachable: {error}")),
    };
    let status = response.status();
    if status.is_success() {
        return match response.json::<PublicationAck>().await {
            Ok(ack) => Outcome::Ack(ack),
            Err(_) => Outcome::Transport("unreadable server acknowledgement".into()),
        };
    }
    let code = status.as_u16();
    if code == 401 || code == 408 || code == 429 || status.is_server_error() {
        return Outcome::Transport(format!("server answered {code}; will retry"));
    }
    let detail = response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .map(|text| conversations_core::host_relay::clean_text(&text))
        .unwrap_or_default();
    Outcome::Refused(format!("server refused publication ({code}): {detail}"))
}

/// Publish in the background for the life of the host.
pub async fn run_loop(
    dir: std::path::PathBuf,
    server: String,
    client: reqwest::Client,
    cookie: Option<String>,
) {
    let mut last: Option<String> = None;
    loop {
        let to = Destination {
            server: &server,
            client: &client,
            cookie: cookie.as_deref(),
        };
        let report = match pump(&dir, &to).await {
            Ok(pumped) if pumped.transport_errors > 0 => {
                Some("publication waiting for the server; will retry".to_owned())
            }
            Ok(_) => None,
            Err(error) => Some(format!("publication error: {error:#}")),
        };
        if report != last {
            if let Some(text) = &report {
                eprintln!("{text}");
            }
            last = report;
        }
        tokio::time::sleep(Duration::from_secs(if last.is_some() { 5 } else { 1 })).await;
    }
}

/// Text for the local CLI.
pub fn render(status: &Status) -> String {
    let mut out = format!("{}  {}", status.session, status.state.as_str());
    if status.rev > 0 {
        out.push_str(&format!("  rev {}", status.rev));
    }
    if let Some(audience) = &status.audience {
        match audience {
            Audience::Organization {} => out.push_str("  audience: organization"),
            Audience::Restricted { emails, .. } => {
                out.push_str(&format!("  audience: {}", emails.join(", ")))
            }
        }
    }
    match (&status.owner, status.rev) {
        (Some(owner), _) => out.push_str(&format!("\n  as {owner}")),
        (None, 0) => {}
        (None, _) => out.push_str("\n  without an account (for a server with authentication off)"),
    }
    if let Some(server) = &status.server {
        out.push_str(&format!(" on {server}"));
    }
    if status.rev > 0 {
        out.push_str(&format!(
            "\n  {} published, {} pending",
            status.published, status.pending
        ));
    }
    if let Some(error) = &status.error {
        out.push_str(&format!("\n  stopped: {error}"));
    }
    if let Some(attempt) = &status.last_attempt {
        out.push_str(&format!("\n  last attempt: {attempt}"));
    }
    out
}

/// The central account publications are signed for, from local settings.
/// `None` signs no owner, which only a server with authentication off accepts;
/// an authenticated server refuses it visibly rather than adopting a login.
pub fn configured_owner(account: Option<&str>) -> Result<Option<String>> {
    match account {
        Some(owner) if valid_email(owner) => Ok(Some(owner.to_owned())),
        Some(_) => bail!("the configured publication account is not an email address"),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_pending_writers_inside_the_audience() {
        // Canonicalization: lowercase, sorted, unique, and a pending writer
        // must also be a reader (EX-02: write is read + a trusted key).
        let ok = normalize(Audience::Restricted {
            emails: vec!["Bob@X.com".into(), "alice@x.com".into()],
            write_pending: vec!["BOB@x.com".into()],
        })
        .unwrap();
        assert_eq!(
            ok,
            Audience::Restricted {
                emails: vec!["alice@x.com".into(), "bob@x.com".into()],
                write_pending: vec!["bob@x.com".into()],
            }
        );
        let outside = normalize(Audience::Restricted {
            emails: vec!["bob@x.com".into()],
            write_pending: vec!["eve@x.com".into()],
        })
        .unwrap_err();
        assert!(outside
            .to_string()
            .contains("named to write but not to read"));
    }
}
