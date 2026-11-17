//! The host's local authority database (`authz.db` in the private runtime
//! directory): host identity, locally enrolled credentials, grants, and the
//! durable replay/idempotency journal. Credentials and grants are written only
//! by local callers (`host credential …`); no code path accepts them from a
//! remote frame.

use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use ring::signature::{Ed25519KeyPair, KeyPair};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::envelope::{key_id, Alg, Cap, Operation, Verified};

pub type PrPollCandidate = (String, String, i64, Option<String>);

// Both signed admission and the authenticated local owner use these exact
// transaction rules. The caller owns authorization and signed replay journaling.
fn change_session_grants(
    tx: &rusqlite::Transaction<'_>,
    session: &str,
    recipient: &str,
    caps: &[Cap],
    remove: bool,
    now: i64,
) -> Result<Option<&'static str>> {
    if remove {
        // Exactly the named recipient's rows for THIS session: a host-wide
        // grant, another session and the credential itself are untouched.
        let known: i64 = tx.query_row(
            "SELECT COUNT(*) FROM credentials WHERE cred_id = ?1",
            params![recipient],
            |r| r.get(0),
        )?;
        if known == 0 {
            return Ok(Some("unknown recipient credential"));
        }
        // A live host-wide grant of a named capability would still authorize
        // the session after its row is removed. Never report that as removed:
        // refuse the whole removal, changing nothing (agents never use
        // host-wide rows, so theirs do not count).
        for cap in caps {
            let host_wide: i64 = tx.query_row(
                "SELECT COUNT(*) FROM grants g JOIN credentials c ON c.cred_id = g.cred_id
                     WHERE g.cred_id = ?1 AND g.session_id = ?2 AND g.cap = ?3
                       AND g.revoked_at IS NULL AND c.kind <> 'agent'",
                params![recipient, ANY_SESSION, cap.as_str()],
                |r| r.get(0),
            )?;
            if host_wide > 0 {
                return Ok(Some(
                        "the recipient holds a host-wide grant of this capability, which still authorizes the session; nothing was removed",
                    ));
            }
        }
        for cap in caps {
            tx.execute(
                "UPDATE grants SET revoked_at = ?4
                     WHERE cred_id = ?1 AND session_id = ?2 AND cap = ?3 AND revoked_at IS NULL",
                params![recipient, session, cap.as_str(), now],
            )?;
        }
        if caps.contains(&Cap::SessionWrite) {
            tx.execute(
                "UPDATE agent_inbox SET state = 'revoked', state_at = ?3
                     WHERE kid = ?1 AND session_id = ?2 AND state IN ('queued','received')",
                params![recipient, session, now],
            )?;
        }
    } else {
        // Approval only permits capabilities already in this recipient's ceiling.
        let row: Option<(String, Option<i64>)> = tx
            .query_row(
                "SELECT ceiling, revoked_at FROM credentials WHERE cred_id = ?1",
                params![recipient],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((ceiling, revoked)) = row else {
            return Ok(Some("unknown recipient credential"));
        };
        if revoked.is_some() {
            return Ok(Some("recipient credential revoked"));
        }
        if caps
            .iter()
            .any(|cap| !ceiling.split(',').any(|c| c == cap.as_str()))
        {
            return Ok(Some("capability outside the recipient's approved ceiling"));
        }
        for cap in caps {
            tx.execute(
                    "INSERT INTO grants(cred_id, session_id, cap, granted_at) VALUES(?1, ?2, ?3, ?4)
                     ON CONFLICT(cred_id, session_id, cap) DO UPDATE SET revoked_at = NULL, granted_at = ?4",
                    params![recipient, session, cap.as_str(), now],
                )?;
        }
    }
    Ok(None)
}

pub(super) const SCHEMA_VERSION: i64 = 15;

/// Minimum interval between automatic refreshes of one recorded PR target.
/// A manual signed `pr.sync`, a newly recorded target, or an observed change
/// in meaningful PR metadata resets the per-target delay to this floor.
pub const PR_POLL_MIN_DELAY_SECS: i64 = 60;

/// Upper bound the per-target delay doubles toward on unchanged results
/// and repeated transient failures. Fifteen minutes is enough to keep an
/// idle host near-silent without starving a legitimately quiet PR.
pub const PR_POLL_MAX_DELAY_SECS: i64 = 15 * 60;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS host_identity(
  id INTEGER PRIMARY KEY CHECK(id = 1), pkcs8 BLOB NOT NULL, public_key BLOB NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS credentials(
  cred_id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN ('owner','agent')),
  alg TEXT NOT NULL CHECK(alg IN ('es256','ed25519')), public_key BLOB NOT NULL,
  label TEXT NOT NULL, ceiling TEXT NOT NULL, approved_at INTEGER NOT NULL, revoked_at INTEGER,
  vouch_host_id TEXT, vouch_host_key TEXT);
CREATE TABLE IF NOT EXISTS grants(
  cred_id TEXT NOT NULL REFERENCES credentials(cred_id), session_id TEXT NOT NULL,
  cap TEXT NOT NULL, granted_at INTEGER NOT NULL, revoked_at INTEGER,
  PRIMARY KEY(cred_id, session_id, cap));
CREATE TABLE IF NOT EXISTS op_journal(
  kid TEXT NOT NULL, jti TEXT NOT NULL, payload_sha256 BLOB NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('admitted','dispatching','done','uncertain','failed')),
  result TEXT, session_id TEXT NOT NULL, admitted_at INTEGER NOT NULL,
  PRIMARY KEY(kid, jti));
-- Stream-class frames (keystrokes, resize, repaint) are not journaled as
-- operations, but each frame's jti is consumed durably BEFORE its effect, so a
-- frame is delivered at most once across host restarts. Rows are only needed
-- while the frame could still verify (exp + skew) and are pruned on use.
CREATE TABLE IF NOT EXISTS stream_frames(
  kid TEXT NOT NULL, jti TEXT NOT NULL, exp INTEGER NOT NULL,
  PRIMARY KEY(kid, jti)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS stream_frames_by_exp ON stream_frames(exp);
-- Passkey ceremonies. A challenge row holds the library's opaque ceremony state
-- and the IMMUTABLE request it was issued for; finishing takes only the row id.
CREATE TABLE IF NOT EXISTS challenges(
  id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN ('enroll','delegate')),
  state TEXT NOT NULL, params TEXT NOT NULL,
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, consumed_at INTEGER);
-- Verified registrations awaiting LOCAL approval of the actual credential.
CREATE TABLE IF NOT EXISTS pending_credentials(
  cred_id TEXT PRIMARY KEY, kind TEXT NOT NULL, label TEXT NOT NULL, grants TEXT NOT NULL,
  rp_id TEXT NOT NULL, passkey TEXT NOT NULL, created_at INTEGER NOT NULL,
  vouch_host_id TEXT, vouch_host_key TEXT);
-- Temporary browser signing keys. Authority = caps ∩ the parent's CURRENT grants.
CREATE TABLE IF NOT EXISTS delegations(
  dlg_id TEXT PRIMARY KEY, parent_cred TEXT NOT NULL REFERENCES credentials(cred_id),
  browser_kid TEXT NOT NULL, browser_pub BLOB NOT NULL, alg TEXT NOT NULL CHECK(alg = 'es256'),
  caps TEXT NOT NULL, issued_at INTEGER NOT NULL, not_after INTEGER NOT NULL, revoked_at INTEGER);
-- Canonical local session record: where it runs, which native agent session it
-- resumes, and how it was launched. Written only by this host (signed start,
-- wrapper registration, adoption of local checkpoints), never from a remote frame.
CREATE TABLE IF NOT EXISTS sessions(
  session_id TEXT PRIMARY KEY, agent TEXT NOT NULL, cwd TEXT NOT NULL, resume_id TEXT,
  launcher TEXT NOT NULL, adk_statusline INTEGER NOT NULL DEFAULT 0,
  origin TEXT NOT NULL CHECK(origin IN ('signed_start','adopted')),
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS remote_imports(
  session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
  resume_mode TEXT NOT NULL CHECK(resume_mode IN ('native','continuation','unsupported')));
-- Restore intent is separate from canonical metadata: closing a session keeps
-- its history and resume identity without making it reopen at startup.
CREATE TABLE IF NOT EXISTS local_restore(
  session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
  server TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS local_restore_imports(server TEXT PRIMARY KEY);
-- Pull requests THIS host was told, by a signed operation, to work on for a
-- session. Automatic polling reads only these rows: a server-supplied list never
-- adds one. Written by this host only.
CREATE TABLE IF NOT EXISTS session_pr_targets(
  session_id TEXT NOT NULL, owner TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT NULL,
  added_at INTEGER NOT NULL, PRIMARY KEY(session_id, owner, repo, number));
-- Agent messages this host admitted from a signed `agent.message`. The text,
-- sender key, signed expiry and request digest are what the signature covered;
-- nothing in a row comes from the server. Delivery re-checks the sender's CURRENT
-- grant for the recipient session, so a removed grant stops queued items.
-- `message_id` is derived from (kid, jti): each signer has its own id space.
-- `jti` and `source_host` (the sender's signed own host id) are added below.
CREATE TABLE IF NOT EXISTS agent_inbox(
  message_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, source_session TEXT NOT NULL,
  kid TEXT NOT NULL, text TEXT NOT NULL, reply_to TEXT, signed_exp INTEGER NOT NULL,
  payload_sha256 BLOB NOT NULL, admitted_at INTEGER NOT NULL,
  state TEXT NOT NULL DEFAULT 'queued', state_at INTEGER NOT NULL);
-- Sender side: this host's agent signing key (distinct from the host identity key).
CREATE TABLE IF NOT EXISTS agent_key(
  id INTEGER PRIMARY KEY CHECK(id = 1), pkcs8 BLOB NOT NULL, public_key BLOB NOT NULL, created_at INTEGER NOT NULL);
-- Sender side: recipients the local user paired by hand. host_key is the pinned
-- public key of the recipient host; central discovery never writes here.
CREATE TABLE IF NOT EXISTS agent_targets(
  session_id TEXT PRIMARY KEY, host_id TEXT NOT NULL, host_key BLOB NOT NULL, label TEXT NOT NULL, added_at INTEGER NOT NULL);
-- Sender side: one row per logical send (source, target, idempotency key). The
-- FIRST signed envelope is stored before it is sent and reused byte-for-byte on
-- retry; `request_sha256` binds the semantic request so a different body, reply,
-- source or target under the same key is refused.
CREATE TABLE IF NOT EXISTS agent_sends(
  source_session TEXT NOT NULL, target_session TEXT NOT NULL, idem TEXT NOT NULL,
  request_sha256 BLOB NOT NULL, attempt INTEGER NOT NULL, message_id TEXT NOT NULL,
  payload BLOB NOT NULL, sig BLOB NOT NULL, kid TEXT NOT NULL, host_id TEXT NOT NULL,
  exp INTEGER NOT NULL, state TEXT NOT NULL, detail TEXT,
  PRIMARY KEY(source_session, target_session, idem));
-- Native agent identity of a local session, bound by a local process that proved
-- it (wrapper environment / owned endpoint). Never written from server metadata.
-- A native id belongs to at most one session and a session keeps its first one.
CREATE TABLE IF NOT EXISTS native_bindings(
  session_id TEXT PRIMARY KEY, native TEXT NOT NULL UNIQUE, bound_at INTEGER NOT NULL);
-- Which signed authorities put content into a session's terminal, so a
-- revocation can interrupt the work they started. Written by this host BEFORE
-- the effect is handed to the terminal; no age or count limit; kept across host
-- restarts (a revocation made while no host ran is acted on when the wrapper
-- reconnects); cleared when a fresh agent process registers for the session.
CREATE TABLE IF NOT EXISTS session_interactions(
  session_id TEXT NOT NULL, kid TEXT NOT NULL, dlg TEXT, cap TEXT NOT NULL, epoch INTEGER,
  recorded_at INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS session_interactions_by_authority
  ON session_interactions(session_id, kid, IFNULL(dlg, ''), cap, IFNULL(epoch, -1));
-- Same-machine handoffs requested over the local socket (idempotent per source+key).
CREATE TABLE IF NOT EXISTS agent_handoffs(
  source_session TEXT NOT NULL, idem TEXT NOT NULL, child TEXT NOT NULL, agent TEXT NOT NULL,
  brief_sha256 BLOB NOT NULL, state TEXT NOT NULL, error TEXT, created_at INTEGER NOT NULL,
  PRIMARY KEY(source_session, idem));
-- One-use challenges for private channel offers. A challenge is consumed by the
-- first offer that names it, whatever the outcome, and every open challenge is
-- consumed when the host starts: none survives a restart.
CREATE TABLE IF NOT EXISTS peer_challenges(
  id TEXT PRIMARY KEY, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
  consumed_at INTEGER, kid TEXT, offer_sha256 BLOB);
"#;

/// How long a private channel challenge can be used.
pub const PEER_CHALLENGE_TTL_SECS: i64 = 60;
/// Most unused, unexpired private channel challenges at once.
const MAX_OPEN_PEER_CHALLENGES: i64 = 16;

/// Host-wide scope marker in `grants.session_id`.
pub const ANY_SESSION: &str = "*";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Owner,
    Agent,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Owner => "owner",
            Kind::Agent => "agent",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Kind::Owner),
            "agent" => Some(Kind::Agent),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub cap: Cap,
    /// `None` = host-wide.
    pub session: Option<String>,
    /// `ceiling:<cap>`: approve the capability CATEGORY for this credential
    /// without granting it on any session. Sessions are added later by an owner
    /// signing `grant.add`; the ceiling bounds what they can ever add.
    pub ceiling_only: bool,
}

impl Grant {
    /// `session.start` (host-wide), `session.write:<uuid>`, or `ceiling:<cap>`.
    pub fn parse(value: &str) -> Result<Self> {
        if let Some(name) = value.strip_prefix("ceiling:") {
            let cap = Cap::parse(name).with_context(|| format!("unknown capability `{name}`"))?;
            return Ok(Self {
                cap,
                session: None,
                ceiling_only: true,
            });
        }
        let (cap, session) = match value.split_once(':') {
            Some((cap, session)) => (cap, Some(session)),
            None => (value, None),
        };
        let cap = Cap::parse(cap).with_context(|| format!("unknown capability `{cap}`"))?;
        if let Some(session) = session {
            ensure!(
                super::envelope::valid_session_id(session),
                "grant session must be a lowercase UUID"
            );
            ensure!(
                !cap.creates_sessions() && !cap.host_scoped(),
                "{} cannot be scoped to one session",
                cap.as_str()
            );
        }
        Ok(Self {
            cap,
            session: session.map(str::to_owned),
            ceiling_only: false,
        })
    }
}

/// Where a journaled operation is in its lifecycle: `Admitted` (journaled, effect
/// not started) -> `dispatching` (effect may be in progress, entered only through
/// [`Store::begin`]) -> one settled state.
/// `Uncertain` is settled for replay (never re-run) but a connected wrapper can
/// still resolve an uncertain start to `Done` ([`Store::confirm_start`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalState {
    Done,
    Uncertain,
    Failed,
}

impl JournalState {
    pub fn as_str(self) -> &'static str {
        match self {
            JournalState::Done => "done",
            JournalState::Uncertain => "uncertain",
            JournalState::Failed => "failed",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// Admitted under grant activation `epoch`.
    New {
        epoch: i64,
    },
    /// The operation was applied inside the admission transaction (a local
    /// authority change); there is no separate effect to run.
    Applied(&'static str),
    Duplicate {
        state: String,
        result: Option<String>,
    },
    PayloadMismatch,
    Denied(&'static str),
}

/// What one [`Store::revoke_credential`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    /// The credential was already revoked before this call.
    pub already: bool,
    pub grants: usize,
    pub delegations: usize,
    pub messages: usize,
    /// Browser keys that signed under this credential's delegations: their
    /// queued effects are revoked with it.
    pub browser_keys: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InboxRow {
    pub message_id: String,
    pub session_id: String,
    pub source_session: String,
    pub kid: String,
    pub text: String,
    pub reply_to: Option<String>,
    pub admitted_at: i64,
    pub state: String,
}

impl InboxRow {
    /// The shape the harness delivery consumers expect.
    pub fn into_agent_message(self) -> conversations_core::messaging::AgentMessage {
        use conversations_core::messaging::{AgentMessage, AgentMessageStatus};
        AgentMessage {
            id: self.message_id.clone(),
            project_id: None,
            sender_session_id: self.source_session,
            sender_owner_email: None,
            recipient_session_id: self.session_id,
            text: self.text,
            reply_to: self.reply_to,
            idempotency_key: self.message_id,
            created_at: super::super::agent_link::iso(self.admitted_at),
            received_at: (self.state == "received")
                .then(|| super::super::agent_link::iso(self.admitted_at)),
            acknowledged_at: None,
            status: if self.state == "received" {
                AgentMessageStatus::Received
            } else {
                AgentMessageStatus::Queued
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct SendRow {
    pub source: String,
    pub target: String,
    pub idem: String,
    pub request_sha256: Vec<u8>,
    pub attempt: i64,
    pub message_id: String,
    pub payload: Vec<u8>,
    pub sig: Vec<u8>,
    pub kid: String,
    pub host_id: String,
    pub exp: i64,
    pub state: String,
    pub detail: Option<String>,
    /// When the recipient host last vouched for `state` (signed answer).
    pub checked_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct HandoffRow {
    pub child: String,
    pub agent: String,
    pub brief_sha256: Vec<u8>,
    pub state: String,
    pub error: Option<String>,
}

pub struct Store {
    pub(super) conn: Connection,
}

/// PKCS#8 v1 prefix of an Ed25519 seed (RFC 8410), for test identities.
#[cfg(test)]
fn hex_prefix() -> [u8; 16] {
    [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ]
}

pub fn now_secs() -> i64 {
    #[cfg(test)]
    if let Some(fixed) = test_clock::get() {
        return fixed;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Open (creating if needed) an owner-only SQLite file `<dir>/<file>`. The
/// parent must already be an owner-only directory; the database and its WAL/SHM
/// files are 0600. Shared by the authority and local-history databases.
pub(crate) fn open_private(dir: &Path, file: &str) -> Result<Connection> {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    let meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("host runtime directory {}", dir.display()))?;
    ensure!(
        meta.is_dir() && meta.uid() == euid && meta.mode() & 0o077 == 0,
        "host runtime directory must be an owner-only directory: {}",
        dir.display()
    );
    // Serialize the brief create/open boundary within this process: no other
    // thread may open SQLite on a newly created file before its creator closes
    // the raw descriptor. SQLite connections remain concurrent after open.
    static OPEN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let opening = OPEN
        .lock()
        .map_err(|_| anyhow::anyhow!("private database open lock poisoned"))?;
    let path = dir.join(file);
    // Never raw-open an EXISTING SQLite inode. POSIX close() releases every
    // fcntl lock this process holds on that inode, including locks belonging to
    // other live SQLite connections. That can permit another process to remove
    // WAL/SHM files still in use and corrupt the database.
    // Exclusive creation also sets final permissions before SQLite touches it;
    // SQLite gives the WAL/SHM files the main database's permissions.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(created) => drop(created),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let meta = std::fs::symlink_metadata(&path)?;
    ensure!(
        meta.is_file() && meta.uid() == euid && meta.mode() & 0o077 == 0,
        "authority database must be an owner-only file: {}",
        path.display()
    );
    let conn = Connection::open(&path)?;
    drop(opening);
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let journal: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    if !journal.eq_ignore_ascii_case("wal") {
        // SQLite's initial journal-mode change can return BUSY without using
        // its busy timeout. Serialize only this cold transition across CLI
        // processes, then recheck in case another opener already completed it.
        // A separate file avoids interfering with SQLite's own database locks.
        let initialization = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(format!("{file}.init.lock")))?;
        // SAFETY: initialization is an open descriptor, closed after this branch.
        if unsafe { libc::flock(initialization.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).context("initializing WAL journal");
        }
        let journal: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        if !journal.eq_ignore_ascii_case("wal") {
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
    }
    // Replay and revocation records must survive power loss.
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Rules every credential approval obeys, whatever its key type.
pub fn validate_enrollment(kind: Kind, label: &str, grants: &[Grant]) -> Result<()> {
    ensure!(
        !label.is_empty() && label.len() <= 64 && !label.chars().any(char::is_control),
        "label must be 1-64 printable characters"
    );
    ensure!(!grants.is_empty(), "enroll at least one grant");
    if kind == Kind::Agent {
        ensure!(
            grants.iter().all(|g| !g.cap.creates_sessions()),
            "agent keys cannot be granted session.start or session.reopen"
        );
        ensure!(
            grants.iter().all(|g| g.cap != Cap::SessionManageSharing),
            "agent keys cannot manage sharing"
        );
        ensure!(
            grants.iter().all(|g| !g.cap.owner_only()),
            "agent keys cannot hold pull-request or host-settings capabilities"
        );
        ensure!(
            grants.iter().all(|g| g.ceiling_only || g.session.is_some()),
            "agent keys need an explicit session for every grant (session.write:<uuid>)"
        );
    }
    Ok(())
}

/// The parent credential of a live delegation, or why it is not usable now.
/// Expiry, revocation and the delegation's own scope are checked here, inside
/// the admission transaction.
pub(super) fn delegated_parent(
    tx: &rusqlite::Connection,
    kid: &str,
    cap: Cap,
    session_id: &str,
    dlg: &str,
    now: i64,
) -> Result<std::result::Result<String, &'static str>> {
    let row: Option<(String, String, String, i64, Option<i64>)> = tx
        .query_row(
            "SELECT parent_cred, browser_kid, caps, not_after, revoked_at FROM delegations WHERE dlg_id = ?1",
            params![dlg],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((parent, browser_kid, caps, not_after, revoked)) = row else {
        return Ok(Err("unknown delegation"));
    };
    if browser_kid != kid {
        return Ok(Err("delegation belongs to another key"));
    }
    if revoked.is_some() {
        return Ok(Err("delegation revoked"));
    }
    if now > not_after {
        return Ok(Err("delegation expired"));
    }
    let scoped = caps
        .split(',')
        .filter_map(|grant| Grant::parse(grant).ok())
        .any(|grant| {
            grant.cap == cap
                && match &grant.session {
                    None => true,
                    // A listing is filtered per session afterwards.
                    Some(_)
                        if cap == Cap::HistoryRead && session_id == super::envelope::HOST_SCOPE =>
                    {
                        true
                    }
                    Some(session) => {
                        !cap.creates_sessions() && !cap.host_scoped() && *session == session_id
                    }
                }
        });
    Ok(if scoped {
        Ok(parent)
    } else {
        Err("operation outside the delegation's scope")
    })
}

fn unknown_session(tx: &rusqlite::Connection, session_id: &str) -> Result<Option<&'static str>> {
    let known: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sessions WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )?;
    Ok((known == 0).then_some("unknown local session"))
}

/// Whether credential `cred_id` currently holds the authority `op` needs: not
/// revoked, an allowed kind, inside its approved ceiling, and a present grant
/// for this capability (and session). `via_delegation` additionally requires
/// the credential to be a passkey, the only kind that can parent a delegation.
fn authority_denial(
    tx: &rusqlite::Connection,
    cred_id: &str,
    op: &Verified,
    via_delegation: bool,
) -> Result<Option<&'static str>> {
    if matches!(op.op, Operation::SessionRename { .. }) || op.op.owner_metadata() {
        match credential_allows(tx, cred_id, op.op.capability(), via_delegation)? {
            Ok(Kind::Owner) => {}
            Ok(_) => return Ok(Some("only an owner credential may change session metadata")),
            Err(reason) => return Ok(Some(reason)),
        }
    }
    if op.is_listing() {
        return Ok(credential_allows(tx, cred_id, op.op.capability(), via_delegation)?.err());
    }
    holds_authority(
        tx,
        cred_id,
        op.op.capability(),
        Some(op.op.session_id()),
        via_delegation,
    )
}

/// The CURRENT authority of `op`'s signer, with the same delegation, parent,
/// grant and session rules as admission: the refusal, or the exact rows the
/// authority rests on now (see [`authority_rows`]).
pub(super) fn current(
    tx: &rusqlite::Transaction<'_>,
    op: &Verified,
    now: i64,
) -> Result<std::result::Result<String, &'static str>> {
    let parent = match &op.dlg {
        None => op.kid.clone(),
        Some(dlg) => match delegated_parent(
            tx,
            &op.kid,
            op.op.capability(),
            op.op.session_id(),
            dlg,
            now,
        )? {
            Ok(parent) => parent,
            Err(reason) => return Ok(Err(reason)),
        },
    };
    if let Some(reason) = authority_denial(tx, &parent, op, op.dlg.is_some())? {
        return Ok(Err(reason));
    }
    if !op.op.capability().host_scoped()
        && !op.is_listing()
        && !op.op.global_owner_metadata()
        && !matches!(op.op, Operation::MigrationImport { .. })
    {
        if let Some(reason) = unknown_session(tx, op.op.session_id())? {
            return Ok(Err(reason));
        }
    }
    Ok(Ok(authority_rows(tx, &parent, op)?))
}

/// The authority rows `op` rests on, as one comparable value: the parent
/// credential, the delegation (if any) and every live grant row that could
/// authorize it, with their never-reused activation epochs. Any revocation,
/// removal/re-grant (including in the same second), ceiling or delegation
/// change invalidates a queued read. Listing snapshots conservatively bind all
/// eligible grants; each returned session is also checked independently.
fn authority_rows(tx: &rusqlite::Transaction<'_>, parent: &str, op: &Verified) -> Result<String> {
    type Row5<A, B, C> = Option<(A, B, C, i64, Option<i64>)>;
    let credential: Row5<String, String, String> = tx
        .query_row(
            "SELECT kind, ceiling, auth, approved_at, revoked_at FROM credentials WHERE cred_id = ?1",
            params![parent],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let delegation: Row5<String, String, i64> = match &op.dlg {
        None => None,
        Some(dlg) => tx
            .query_row(
                "SELECT browser_kid, caps, issued_at, not_after, revoked_at FROM delegations WHERE dlg_id = ?1",
                params![dlg],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?,
    };
    let mut statement = tx.prepare(
        "SELECT session_id, COALESCE(epoch, 0) FROM grants
         WHERE cred_id = ?1 AND cap = ?2 AND revoked_at IS NULL
           AND (?3 OR session_id = ?4 OR session_id = ?5)
         ORDER BY session_id",
    )?;
    let grants = statement
        .query_map(
            params![
                parent,
                op.op.capability().as_str(),
                op.is_listing() || matches!(op.op, Operation::CommunicationsList { .. }),
                ANY_SESSION,
                op.op.session_id()
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(serde_json::to_string(&(
        parent, credential, delegation, grants,
    ))?)
}

fn same_authority(tx: &rusqlite::Transaction<'_>, op: &Verified, now: i64) -> Result<bool> {
    let Some(admitted) = &op.admitted_under else {
        return Ok(false);
    };
    if now > op.exp {
        return Ok(false);
    }
    Ok(current(tx, op, now)?.is_ok_and(|rows| rows == *admitted))
}

/// One local session in a private listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSession {
    pub session_id: String,
    pub agent: String,
    pub cwd: String,
    pub created_at: i64,
}

/// `scope`: `Some(session)` = authority over that session (a host-wide grant
/// also counts); `None` = host-wide authority is required (used when issuing a
/// delegation that is itself host-wide). Session-creating capabilities are
/// always host-wide.
pub(super) fn holds_authority(
    tx: &rusqlite::Connection,
    cred_id: &str,
    cap: Cap,
    scope: Option<&str>,
    via_delegation: bool,
) -> Result<Option<&'static str>> {
    Ok(current_grant(tx, cred_id, cap, scope, via_delegation, None)?.err())
}

/// [`holds_authority`], returning the epoch of the grant activation that gives
/// it (the highest, if several rows do). `epoch: Some(e)` counts only the
/// activation `e`: a grant removed and granted again is a different one.
pub(super) fn current_grant(
    tx: &rusqlite::Connection,
    cred_id: &str,
    cap: Cap,
    scope: Option<&str>,
    via_delegation: bool,
    epoch: Option<i64>,
) -> Result<std::result::Result<i64, &'static str>> {
    let kind = match credential_allows(tx, cred_id, cap, via_delegation)? {
        Ok(kind) => kind,
        Err(reason) => return Ok(Err(reason)),
    };
    // An agent's authority is only ever explicit sessions, never a host-wide row.
    let host_wide = if kind == Kind::Agent {
        "\u{0}none"
    } else {
        ANY_SESSION
    };
    // Creating a session has no pre-existing session to scope to: host-wide only.
    let session = if cap.creates_sessions() || cap.host_scoped() || scope.is_none() {
        host_wide
    } else {
        scope.unwrap_or(host_wide)
    };
    let granted: Option<i64> = tx.query_row(
        "SELECT MAX(COALESCE(epoch, 0)) FROM grants WHERE cred_id = ?1 AND cap = ?2 AND revoked_at IS NULL
           AND (session_id = ?3 OR session_id = ?4) AND (?5 IS NULL OR COALESCE(epoch, 0) = ?5)",
        params![cred_id, cap.as_str(), host_wide, session, epoch],
        |r| r.get(0),
    )?;
    Ok(granted.ok_or("no current grant"))
}

/// Everything about a credential that does not depend on a session: it exists,
/// is not revoked, may parent a delegation if asked to, has the kind the
/// capability needs, and the capability is inside its approved ceiling.
/// Returns the credential's kind.
pub(super) fn credential_allows(
    tx: &rusqlite::Connection,
    cred_id: &str,
    cap: Cap,
    via_delegation: bool,
) -> Result<std::result::Result<Kind, &'static str>> {
    let credential: Option<(String, String, String, Option<i64>)> = tx
        .query_row(
            "SELECT kind, ceiling, auth, revoked_at FROM credentials WHERE cred_id = ?1",
            params![cred_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((kind, ceiling, auth, revoked)) = credential else {
        return Ok(Err("unknown credential"));
    };
    if revoked.is_some() {
        return Ok(Err("credential revoked"));
    }
    if via_delegation && auth != "passkey" {
        return Ok(Err("delegation parent is not a passkey"));
    }
    let Some(kind) = Kind::parse(&kind) else {
        return Ok(Err("unknown credential kind"));
    };
    // Session creation and sharing management are owner-only no matter what
    // labels or grants say.
    if cap.creates_sessions() && kind != Kind::Owner {
        return Ok(Err("this key kind cannot create sessions"));
    }
    if cap == Cap::SessionManageSharing && kind != Kind::Owner {
        return Ok(Err("this key kind cannot manage sharing"));
    }
    if cap.owner_only() && kind != Kind::Owner {
        return Ok(Err("this key kind cannot hold this capability"));
    }
    if !ceiling.split(',').any(|c| c == cap.as_str()) {
        return Ok(Err("capability outside credential ceiling"));
    }
    Ok(Ok(kind))
}

impl Store {
    /// Open (creating if needed) `<dir>/authz.db`. The parent must already be
    /// an owner-only directory; the database and its WAL/SHM files are 0600.
    /// Current databases need no schema or identity writes. Independent CLI
    /// processes can initialize an older database through the same cold path.
    pub fn open(dir: &Path) -> Result<Self> {
        let mut conn = open_private(dir, "authz.db")?;
        Self::initialize(&mut conn)?;
        Ok(Self { conn })
    }

    /// Migration and identity creation share one writer transaction. Re-read
    /// the version after taking the lock: another process may have initialized
    /// the database while this one waited. A crash rolls the whole upgrade back.
    fn initialize(conn: &mut Connection) -> Result<()> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= SCHEMA_VERSION,
            "authority database was written by a newer host (schema {version})"
        );
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= SCHEMA_VERSION,
            "authority database was written by a newer host (schema {version})"
        );
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        tx.execute_batch(SCHEMA)?;
        // v1 journals had no operation kind; their only operation was a start.
        let has_cap: bool = tx
            .prepare("SELECT 1 FROM pragma_table_info('op_journal') WHERE name = 'cap'")?
            .exists([])?;
        if !has_cap {
            tx.execute(
                "ALTER TABLE op_journal ADD COLUMN cap TEXT NOT NULL DEFAULT 'session.start'",
                [],
            )?;
        }
        for (table, column, ddl) in [
            ("sessions", "handoff_model", "TEXT"),
            ("credentials", "auth", "TEXT NOT NULL DEFAULT 'raw_key'"),
            ("credentials", "webauthn", "TEXT"),
            ("credentials", "rp_id", "TEXT"),
            ("op_journal", "dlg", "TEXT"),
            ("op_journal", "parent_cred", "TEXT"),
            ("agent_inbox", "jti", "TEXT"),
            ("agent_inbox", "source_host", "TEXT"),
            ("agent_sends", "checked_at", "INTEGER"),
            ("session_pr_targets", "kid", "TEXT"),
            ("session_pr_targets", "dlg", "TEXT"),
            ("session_pr_targets", "cap", "TEXT"),
            ("session_pr_targets", "epoch", "INTEGER"),
            // v14: per-target gradual backoff for the host poll loop.
            // next_poll_at = 0 polls immediately (the default for both
            // migrated rows and newly recorded targets). current_delay_secs
            // starts at the minimum and doubles on unchanged/transient
            // failures up to PR_POLL_MAX_DELAY_SECS. fingerprint is a
            // SHA-256 over the meaningful PR metadata (status + comment
            // ids and bodies), so comparing it between polls tells
            // "actually changed" from "we just happened to re-read".
            (
                "session_pr_targets",
                "next_poll_at",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "session_pr_targets",
                "current_delay_secs",
                "INTEGER NOT NULL DEFAULT 60",
            ),
            ("session_pr_targets", "fingerprint", "TEXT"),
            ("grants", "epoch", "INTEGER"),
            ("pending_credentials", "vouch_host_id", "TEXT"),
            ("pending_credentials", "vouch_host_key", "TEXT"),
            ("credentials", "vouch_host_id", "TEXT"),
            ("credentials", "vouch_host_key", "TEXT"),
        ] {
            let present: bool = tx
                .prepare(&format!(
                    "SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'"
                ))?
                .exists([])?;
            if !present {
                tx.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"),
                    [],
                )?;
            }
        }
        // Every activation of a grant (first insert, or re-grant after removal)
        // gets a new, never reused epoch. Work queued under an earlier
        // activation names that epoch and so never revives on a re-grant.
        // Rows from before epochs existed are epoch 0 until re-activated.
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS grant_epochs(epoch INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL);
             CREATE TRIGGER IF NOT EXISTS grant_activated AFTER INSERT ON grants
             BEGIN
               INSERT INTO grant_epochs(at) VALUES(NEW.granted_at);
               UPDATE grants SET epoch = (SELECT MAX(epoch) FROM grant_epochs) WHERE rowid = NEW.rowid;
             END;
             CREATE TRIGGER IF NOT EXISTS grant_reactivated AFTER UPDATE OF revoked_at ON grants
             WHEN OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS NULL
             BEGIN
               INSERT INTO grant_epochs(at) VALUES(NEW.granted_at);
               UPDATE grants SET epoch = (SELECT MAX(epoch) FROM grant_epochs) WHERE rowid = NEW.rowid;
             END;",
        )?;
        // v6 inbox rows were keyed by the bare operation id; it stays their
        // message id (what their agent was shown) and becomes their `jti`.
        tx.execute_batch(
            "UPDATE agent_inbox SET jti = message_id WHERE jti IS NULL;
             CREATE UNIQUE INDEX IF NOT EXISTS agent_inbox_by_operation ON agent_inbox(kid, jti);",
        )?;
        // v13: PR refresh follows session.write. Rewrite the capability recorded
        // against every remembered PR target so the background poll re-checks
        // under the recording credential's CURRENT session.write authority. A
        // legacy owner credential enrolled with the four-cap set (which already
        // includes session.write) keeps polling; a credential that held only
        // session.pr_sync no longer keeps a target alive on its own.
        let legacy_targets: Vec<(String, String, String, i64, String, Option<String>)> = {
            let mut stmt = tx.prepare(
                "SELECT session_id,owner,repo,number,kid,dlg FROM session_pr_targets
                 WHERE cap='session.pr_sync' AND kid IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for (session, owner, repo, number, kid, dlg) in legacy_targets {
            let authority = super::cutover::Authority {
                kid,
                dlg,
                session: session.clone(),
                cap: Cap::SessionWrite.as_str().into(),
                epoch: None,
                exp: None,
            };
            // Resolve both direct and delegated authority through normal admission.
            // Missing/revoked write access stays unbound and cannot poll later.
            let epoch = super::cutover::current_epoch(&tx, &authority, now_secs())?;
            tx.execute(
                "UPDATE session_pr_targets SET cap='session.write',epoch=?5
                 WHERE session_id=?1 AND owner=?2 AND repo=?3 AND number=?4",
                params![session, owner, repo, number, epoch],
            )?;
        }
        let has_identity: bool = tx
            .prepare("SELECT 1 FROM host_identity WHERE id = 1")?
            .exists([])?;
        if !has_identity {
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
                .map_err(|_| anyhow::anyhow!("generating host identity"))?;
            let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
                .map_err(|_| anyhow::anyhow!("host identity key"))?;
            tx.execute(
                "INSERT INTO host_identity(id, pkcs8, public_key, created_at) VALUES(1, ?1, ?2, ?3)",
                params![pkcs8.as_ref(), pair.public_key().as_ref(), now_secs()],
            )?;
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    /// This host's stable audience id: base64url(SHA-256(identity public key)).
    pub fn host_id(&mut self) -> Result<String> {
        Ok(key_id(&self.host_public_key()?))
    }

    /// The identity public key created during database initialization.
    pub fn host_public_key(&mut self) -> Result<Vec<u8>> {
        Ok(self.conn.query_row(
            "SELECT public_key FROM host_identity WHERE id = 1",
            [],
            |r| r.get(0),
        )?)
    }

    /// Sign `message` with the host identity key (Ed25519).
    pub fn host_sign(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        let pkcs8: Vec<u8> =
            self.conn
                .query_row("SELECT pkcs8 FROM host_identity WHERE id = 1", [], |r| {
                    r.get(0)
                })?;
        let pair = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&pkcs8)
            .map_err(|_| anyhow::anyhow!("host identity key is unreadable"))?;
        Ok(pair.sign(message).as_ref().to_vec())
    }

    /// Locally approve a credential and its grants. `kind=agent` can never hold a
    /// session-creating capability: the rule is stored data (ceiling) AND
    /// re-checked at admission, so neither a label nor a later grant changes it.
    pub fn enroll(
        &mut self,
        kind: Kind,
        alg: Alg,
        public_key: &[u8],
        label: &str,
        grants: &[Grant],
    ) -> Result<String> {
        ensure!(
            alg.valid_public_key(public_key),
            "public key is not a valid {} key",
            alg.as_str()
        );
        validate_enrollment(kind, label, grants)?;
        let cred_id = key_id(public_key);
        let mut ceiling: Vec<&str> = grants.iter().map(|g| g.cap.as_str()).collect();
        ceiling.sort_unstable();
        ceiling.dedup();
        let now = now_secs();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "INSERT OR IGNORE INTO credentials(cred_id, kind, alg, public_key, label, ceiling, approved_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![cred_id, kind.as_str(), alg.as_str(), public_key, label, ceiling.join(","), now],
        )? == 0
        {
            bail!("credential {cred_id} is already enrolled");
        }
        for grant in grants.iter().filter(|g| !g.ceiling_only) {
            tx.execute(
                "INSERT OR IGNORE INTO grants(cred_id, session_id, cap, granted_at) VALUES(?1, ?2, ?3, ?4)",
                params![
                    cred_id,
                    grant.session.as_deref().unwrap_or(ANY_SESSION),
                    grant.cap.as_str(),
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(cred_id)
    }

    /// Add owner-only capabilities (held host-wide) to an EXISTING owner
    /// credential without revoking or re-enrolling it. The caller is the local
    /// management web server, authorized by the owner's one-use loopback
    /// cookie. Agent credentials are refused by [`credential_allows`]'s own
    /// owner-only check elsewhere; here we gate once at the entry point too.
    ///
    /// Returns the caps that were newly added (not caps that were already
    /// held). Caps already held are no-ops. Ceiling is extended with the same
    /// caps so `credential_allows` admits them to a browser delegation.
    pub fn grant_owner_only_caps(&mut self, cred_id: &str, caps: &[Cap]) -> Result<Vec<Cap>> {
        ensure!(!caps.is_empty(), "name at least one capability to add");
        ensure!(
            caps.iter().all(|c| c.owner_only()),
            "this path only adds owner-only capabilities"
        );
        let now = now_secs();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, Option<i64>)> = tx
            .query_row(
                "SELECT kind, ceiling, revoked_at FROM credentials WHERE cred_id = ?1",
                params![cred_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((kind, ceiling, revoked)) = row else {
            bail!("no credential with that id");
        };
        ensure!(revoked.is_none(), "credential is revoked");
        ensure!(
            Kind::parse(&kind) == Some(Kind::Owner),
            "only an owner credential can hold these capabilities"
        );
        let mut current: std::collections::BTreeSet<String> =
            ceiling.split(',').map(str::to_owned).collect();
        let mut added = Vec::new();
        for cap in caps {
            let active: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM grants WHERE cred_id=?1 AND session_id=?2 AND cap=?3 AND revoked_at IS NULL)",
                params![cred_id, ANY_SESSION, cap.as_str()],
                |row| row.get(0),
            )?;
            if current.insert(cap.as_str().to_owned()) || !active {
                added.push(*cap);
            }
            tx.execute(
                "INSERT INTO grants(cred_id, session_id, cap, granted_at) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(cred_id, session_id, cap) DO UPDATE SET revoked_at=NULL, granted_at=excluded.granted_at
                 WHERE grants.revoked_at IS NOT NULL",
                params![cred_id, ANY_SESSION, cap.as_str(), now],
            )?;
        }
        if !added.is_empty() {
            let ceiling = current.into_iter().collect::<Vec<_>>().join(",");
            tx.execute(
                "UPDATE credentials SET ceiling = ?2 WHERE cred_id = ?1",
                params![cred_id, ceiling],
            )?;
        }
        tx.commit()?;
        Ok(added)
    }

    /// The key a frame must verify against: a directly enrolled raw key, or the
    /// browser key of a live delegation. Passkeys never sign operations; they
    /// only authorize delegations.
    pub fn signing_key(
        &self,
        kid: &str,
        dlg: Option<&str>,
        now: i64,
    ) -> Result<Option<(Alg, Vec<u8>)>> {
        let row: Option<(String, Vec<u8>)> = match dlg {
            None => self
                .conn
                .query_row(
                    "SELECT alg, public_key FROM credentials WHERE cred_id = ?1 AND auth = 'raw_key' AND revoked_at IS NULL",
                    params![kid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?,
            Some(dlg) => self
                .conn
                .query_row(
                    "SELECT alg, browser_pub FROM delegations WHERE dlg_id = ?1 AND browser_kid = ?2 AND revoked_at IS NULL AND not_after >= ?3",
                    params![dlg, kid, now],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?,
        };
        Ok(row.and_then(|(alg, key)| Alg::parse(&alg).map(|alg| (alg, key))))
    }

    /// A fresh one-use challenge for a private channel offer, or `None` when
    /// too many are open. Returns `(id, expires_at)`.
    pub fn issue_peer_challenge(&mut self, now: i64) -> Result<Option<(String, i64)>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM peer_challenges WHERE expires_at < ?1",
            params![now - 3600],
        )?;
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM peer_challenges WHERE consumed_at IS NULL AND expires_at >= ?1",
            params![now],
            |r| r.get(0),
        )?;
        if open >= MAX_OPEN_PEER_CHALLENGES {
            return Ok(None);
        }
        let mut bytes = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
            .map_err(|_| anyhow::anyhow!("no randomness"))?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let expires = now + PEER_CHALLENGE_TTL_SECS;
        tx.execute(
            "INSERT INTO peer_challenges(id, created_at, expires_at) VALUES(?1, ?2, ?3)",
            params![id, now, expires],
        )?;
        tx.commit()?;
        Ok(Some((id, expires)))
    }

    /// Consume the challenge a verified offer names, then decide whether its
    /// signer may open a channel at all. The challenge is spent even when the
    /// answer is no. Returns the refusal reason, if any.
    pub fn admit_peer_offer(
        &mut self,
        offer: &super::envelope::PeerOffer,
        now: i64,
    ) -> Result<Option<&'static str>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let consumed = tx.execute(
            "UPDATE peer_challenges SET consumed_at = ?2, kid = ?3, offer_sha256 = ?4
             WHERE id = ?1 AND consumed_at IS NULL AND expires_at >= ?2",
            params![offer.challenge, now, offer.kid, offer.payload_sha256],
        )?;
        let denial = if consumed != 1 {
            Some("unknown, used or expired challenge")
        } else {
            match &offer.dlg {
                // `signing_key` already required an unrevoked raw-key credential.
                None => None,
                Some(dlg) => {
                    let parent: Option<(Option<i64>, String)> = tx
                        .query_row(
                            "SELECT c.revoked_at, c.auth FROM delegations d JOIN credentials c ON c.cred_id = d.parent_cred
                             WHERE d.dlg_id = ?1 AND d.browser_kid = ?2 AND d.revoked_at IS NULL AND d.not_after >= ?3",
                            params![dlg, offer.kid, now],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    match parent {
                        Some((None, auth)) if auth == "passkey" => None,
                        Some(_) => Some("delegation parent is revoked or not a passkey"),
                        None => Some("unknown, revoked or expired delegation"),
                    }
                }
            }
        };
        tx.commit()?;
        Ok(denial)
    }

    /// The CURRENT authority of a verified operation's signer, with the same
    /// delegation, parent, grant and session rules as admission. Used when an
    /// answer carries content: a journal row from an earlier admission is never
    /// enough to deliver it. Returns the refusal reason, if any.
    pub fn current_authority(&mut self, op: &Verified, now: i64) -> Result<Option<&'static str>> {
        let tx = self.conn.transaction()?;
        let denial = current(&tx, op, now)?.err();
        tx.commit()?;
        Ok(denial)
    }

    /// Whether `op` still rests on EXACTLY the authority it was admitted under
    /// (`Verified::admitted_under`): the same parent, delegation and grant
    /// activations, and its signed expiry not passed.
    pub fn still_admitted(&mut self, op: &Verified, now: i64) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let same = same_authority(&tx, op, now)?;
        tx.commit()?;
        Ok(same)
    }

    /// The final authority check of a private reply and its hand-off, in ONE
    /// immediate transaction on `authz.db`: `deliver` runs only while `op`
    /// still rests on exactly the authority it was admitted under, and holding
    /// the database write lock across it orders it against every authority
    /// change committed by ANY process (the host, `host credential revoke`, a
    /// grant change from the CLI): a change committed first is seen here and the
    /// reply is withheld; a change attempted meanwhile waits until the reply is
    /// handed off. `deliver` must not block on anything else (no awaits); it is
    /// the hand-off to the encrypted connection, which cannot be recalled.
    /// Returns `None` when the reply was withheld.
    pub fn with_current_authority<R>(
        &mut self,
        op: &Verified,
        now: i64,
        deliver: impl FnOnce() -> R,
    ) -> Result<Option<R>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !same_authority(&tx, op, now)? {
            return Ok(None);
        }
        let delivered = deliver();
        tx.commit()?;
        Ok(Some(delivered))
    }

    /// The local sessions `op` (a listing) may read now, newest first, after the
    /// keyset cursor `after` (`<created_at>:<session id>`), at most `limit`, and
    /// the cursor of the last one when more follow. Each session is authorized
    /// on its own, exactly as a `history.get` for it would be.
    pub fn readable_sessions(
        &mut self,
        op: &Verified,
        after: Option<&str>,
        limit: u32,
        now: i64,
        running: Option<&[String]>,
    ) -> Result<(Vec<ListedSession>, Option<String>)> {
        anyhow::ensure!(op.is_listing(), "not a listing");
        let (at, id) = match after.and_then(|a| a.split_once(':')) {
            Some((at, id)) => (at.parse::<i64>().context("cursor")?, id.to_owned()),
            None => (i64::MAX, String::new()),
        };
        let tx = self.conn.transaction()?;
        let rows: Vec<ListedSession> = if let Some(ids) = running {
            super::sessions::running_sessions(&tx, ids, after)?
        } else {
            let mut statement = tx.prepare(
                "SELECT session_id, agent, cwd, created_at FROM sessions
                 WHERE ?1 = '' OR created_at < ?2 OR (created_at = ?2 AND session_id < ?1)
                 ORDER BY created_at DESC, session_id DESC",
            )?;
            let rows = statement
                .query_map(params![id, at], |r| {
                    Ok(ListedSession {
                        session_id: r.get(0)?,
                        agent: r.get(1)?,
                        cwd: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut readable = Vec::new();
        let mut next = None;
        for row in rows {
            let mut probe = op.clone();
            probe.op = Operation::HistoryGet {
                session: row.session_id.clone(),
                after: 0,
                limit: 1,
                digest: false,
            };
            if current(&tx, &probe, now)?.is_err() {
                continue;
            }
            if readable.len() == limit as usize {
                next = readable
                    .last()
                    .map(|last: &ListedSession| format!("{}:{}", last.created_at, last.session_id));
                break;
            }
            readable.push(row);
        }
        tx.commit()?;
        Ok((readable, next))
    }

    /// Spend every open private channel challenge (host start): an offer made
    /// for a previous host process is never accepted.
    pub fn expire_peer_challenges(&mut self, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE peer_challenges SET consumed_at = ?1 WHERE consumed_at IS NULL",
            params![now],
        )?;
        Ok(())
    }

    /// Replay check, authority check and journal insert in one immediate
    /// transaction. A duplicate `(kid, jti)` never re-authorizes or re-executes.
    pub fn admit(&mut self, op: &mut Verified, now: i64) -> Result<Admission> {
        if matches!(
            op.op,
            Operation::AttentionRequest { .. } | Operation::NotificationMark { .. }
        ) {
            super::human::ensure_schema(&self.conn)?;
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, Option<String>, Vec<u8>)> = tx
            .query_row(
                "SELECT state, result, payload_sha256 FROM op_journal WHERE kid = ?1 AND jti = ?2",
                params![op.kid, op.jti],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((state, result, sha)) = existing {
            tx.commit()?;
            return Ok(if sha == op.payload_sha256 {
                Admission::Duplicate { state, result }
            } else {
                Admission::PayloadMismatch
            });
        }
        // Whose authority applies: the signing key itself, or (for a browser key)
        // the parent credential, re-read from the CURRENT rows. Nothing about the
        // parent's grants is copied into the delegation.
        let parent = match &op.dlg {
            None => op.kid.clone(),
            Some(dlg) => match delegated_parent(
                &tx,
                &op.kid,
                op.op.capability(),
                op.op.session_id(),
                dlg,
                now,
            )? {
                Ok(parent) => parent,
                Err(reason) => return Ok(Admission::Denied(reason)),
            },
        };
        if let Some(reason) = authority_denial(&tx, &parent, op, op.dlg.is_some())? {
            return Ok(Admission::Denied(reason));
        }
        // A start names the session it creates. Another start for the same id
        // (under a fresh operation id) must not run unless the earlier one
        // never took effect.
        // Migration chunks use creation authority, but are not terminal starts.
        if matches!(op.op, Operation::SessionStart { .. }) {
            let earlier: i64 = tx.query_row(
                "SELECT COUNT(*) FROM op_journal WHERE session_id = ?1 AND cap = 'session.start' AND state != 'failed'",
                params![op.op.session_id()],
                |r| r.get(0),
            )?;
            if earlier > 0 {
                return Ok(Admission::Denied("session was already started"));
            }
        }
        // Everything but a start acts on a session this host already knows.
        if op.op.capability() != Cap::SessionStart
            && op.op.capability() != Cap::SessionReopen
            && !op.op.capability().host_scoped()
            && !op.is_listing()
            && !op.op.global_owner_metadata()
        {
            if let Some(reason) = unknown_session(&tx, op.op.session_id())? {
                return Ok(Admission::Denied(reason));
            }
        }
        if let Operation::NotificationMark {
            session,
            id,
            resolve,
        } = &op.op
        {
            let viewer = conversations_core::attention::Subject::Credential {
                kid: parent.clone(),
            };
            if let Some(reason) = super::human::mark(&tx, &viewer, session, id, *resolve, now)? {
                return Ok(Admission::Denied(reason));
            }
            tx.execute("INSERT INTO op_journal(kid,jti,payload_sha256,state,result,session_id,admitted_at,cap,dlg,parent_cred) VALUES(?1,?2,?3,'done','notification updated',?4,?5,?6,?7,?8)",params![op.kid,op.jti,op.payload_sha256,session,now,op.op.capability().as_str(),op.dlg,parent])?;
            tx.commit()?;
            return Ok(Admission::Applied("notification updated"));
        }
        if let Operation::AttentionRequest { session, input } = &op.op {
            let sender = conversations_core::attention::Subject::Credential {
                kid: parent.clone(),
            };
            if let Some(reason) = super::human::request(&tx, &sender, session, input, now)? {
                return Ok(Admission::Denied(reason));
            }
            tx.execute("INSERT INTO op_journal(kid,jti,payload_sha256,state,result,session_id,admitted_at,cap,dlg,parent_cred) VALUES(?1,?2,?3,'done','attention requested',?4,?5,?6,?7,?8)",params![op.kid,op.jti,op.payload_sha256,session,now,op.op.capability().as_str(),op.dlg,parent])?;
            tx.commit()?;
            return Ok(Admission::Applied("attention requested"));
        }
        if let super::envelope::Operation::GrantAdd {
            recipient, caps, ..
        } = &op.op
        {
            if let Some(reason) =
                change_session_grants(&tx, op.op.session_id(), recipient, caps, false, now)?
            {
                return Ok(Admission::Denied(reason));
            }
            tx.execute(
                "INSERT INTO op_journal(kid, jti, payload_sha256, state, result, session_id, admitted_at, cap, dlg, parent_cred)
                 VALUES(?1, ?2, ?3, 'done', 'granted', ?4, ?5, ?6, ?7, ?8)",
                params![
                    op.kid,
                    op.jti,
                    op.payload_sha256,
                    op.op.session_id(),
                    now,
                    op.op.capability().as_str(),
                    op.dlg,
                    parent
                ],
            )?;
            tx.commit()?;
            return Ok(Admission::Applied("granted"));
        }
        if let super::envelope::Operation::GrantRemove {
            recipient, caps, ..
        } = &op.op
        {
            if let Some(reason) =
                change_session_grants(&tx, op.op.session_id(), recipient, caps, true, now)?
            {
                return Ok(Admission::Denied(reason));
            }
            tx.execute(
                "INSERT INTO op_journal(kid, jti, payload_sha256, state, result, session_id, admitted_at, cap, dlg, parent_cred)
                 VALUES(?1, ?2, ?3, 'done', 'removed', ?4, ?5, ?6, ?7, ?8)",
                params![
                    op.kid,
                    op.jti,
                    op.payload_sha256,
                    op.op.session_id(),
                    now,
                    op.op.capability().as_str(),
                    op.dlg,
                    parent
                ],
            )?;
            tx.commit()?;
            return Ok(Admission::Applied("removed"));
        }
        if op.op.capability() == Cap::SessionReopen {
            let known: i64 = tx.query_row(
                "SELECT COUNT(*) FROM sessions WHERE session_id = ?1",
                params![op.op.session_id()],
                |r| r.get(0),
            )?;
            if known == 0 {
                return Ok(Admission::Denied("unknown local session"));
            }
            // Continuing an import creates a fresh native session. Until its
            // identity is bound, an uncertain previous reopen cannot be retried
            // under a new operation ID without potentially forking twice.
            let unsettled_continuation: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions s JOIN remote_imports r USING(session_id)
                 JOIN op_journal j USING(session_id)
                 WHERE s.session_id = ?1 AND s.resume_id IS NULL
                   AND r.resume_mode = 'continuation' AND j.cap = 'session.reopen'
                   AND j.state NOT IN ('done', 'failed'))",
                params![op.op.session_id()],
                |r| r.get(0),
            )?;
            if unsettled_continuation {
                return Ok(Admission::Denied(
                    "imported continuation outcome is unknown",
                ));
            }
            // A start whose outcome is unknown must not be papered over by a
            // reopen: only an explicit local decision resolves it.
            let unsettled: i64 = tx.query_row(
                "SELECT COUNT(*) FROM op_journal WHERE session_id = ?1 AND cap = 'session.start' AND state NOT IN ('done', 'failed')",
                params![op.op.session_id()],
                |r| r.get(0),
            )?;
            if unsettled > 0 {
                return Ok(Admission::Denied("session start outcome is unknown"));
            }
        }
        tx.execute(
            "INSERT INTO op_journal(kid, jti, payload_sha256, state, session_id, admitted_at, cap, dlg, parent_cred)
             VALUES(?1, ?2, ?3, 'admitted', ?4, ?5, ?6, ?7, ?8)",
            params![
                op.kid,
                op.jti,
                op.payload_sha256,
                op.op.session_id(),
                now,
                op.op.capability().as_str(),
                op.dlg,
                parent
            ],
        )?;
        let epoch = current_grant(
            &tx,
            &parent,
            op.op.capability(),
            Some(op.op.session_id()),
            op.dlg.is_some(),
            None,
        )?
        .unwrap_or(0);
        op.admitted_under = Some(authority_rows(&tx, &parent, op)?);
        tx.commit()?;
        Ok(Admission::New { epoch })
    }

    /// Admission for a stream-class frame: the same delegation, parent and
    /// grant rules as [`Store::admit`], evaluated against CURRENT rows on every
    /// frame, plus durable at-most-once consumption of the frame's `jti`.
    ///
    /// The `jti` is recorded in the same transaction as the decision and BEFORE
    /// the caller performs the effect. A frame from a known signer is consumed
    /// even when authority is denied (a denied frame must not become valid
    /// later). A crash between this commit and the effect LOSES that frame; it
    /// is never delivered twice. This is at-most-once, not exactly-once.
    /// Returns the epoch of the grant activation it was admitted under, or the
    /// refusal reason.
    pub fn authorize_stream(
        &mut self,
        op: &Verified,
        now: i64,
    ) -> Result<std::result::Result<i64, &'static str>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let parent = match &op.dlg {
            None => {
                let known: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM credentials WHERE cred_id = ?1",
                    params![op.kid],
                    |r| r.get(0),
                )?;
                if known == 0 {
                    return Ok(Err("unknown credential"));
                }
                op.kid.clone()
            }
            Some(dlg) => match delegated_parent(
                &tx,
                &op.kid,
                op.op.capability(),
                op.op.session_id(),
                dlg,
                now,
            )? {
                Ok(parent) => parent,
                Err(reason) => return Ok(Err(reason)),
            },
        };
        tx.execute(
            "DELETE FROM stream_frames WHERE exp < ?1",
            params![now - super::envelope::SKEW_SECS],
        )?;
        let fresh = tx.execute(
            "INSERT OR IGNORE INTO stream_frames(kid, jti, exp) VALUES(?1, ?2, ?3)",
            params![op.kid, op.jti, op.exp],
        )?;
        let decision = if fresh == 0 {
            Err("frame already used")
        } else {
            match current_grant(
                &tx,
                &parent,
                op.op.capability(),
                Some(op.op.session_id()),
                op.dlg.is_some(),
                None,
            )? {
                Err(reason) => Err(reason),
                Ok(epoch) => match unknown_session(&tx, op.op.session_id())? {
                    Some(reason) => Err(reason),
                    None => Ok(epoch),
                },
            }
        };
        tx.commit()?;
        Ok(decision)
    }

    /// Move an OPEN entry (`admitted` or `dispatching`) to `state`. A settled entry
    /// never changes here: in particular a start that its connected wrapper already
    /// confirmed (see [`Store::confirm_start`]) is not regressed to `uncertain` by
    /// the launch wait that gave up a moment earlier.
    pub fn mark(
        &mut self,
        kid: &str,
        jti: &str,
        state: JournalState,
        result: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE op_journal SET state = ?3, result = ?4
             WHERE kid = ?1 AND jti = ?2 AND state IN ('admitted', 'dispatching')",
            params![kid, jti, state.as_str(), result],
        )?;
        Ok(())
    }

    /// The effect of an admitted operation is about to start. In ONE immediate
    /// transaction (serialized against [`Store::revoke_credential`] and
    /// `grant.remove`), the authority it was admitted under is re-checked against
    /// the current rows: still held, the entry becomes `dispatching`; no longer
    /// held, it fails as `authority_revoked` and the reason is returned.
    pub fn begin(&mut self, op: &Verified, now: i64) -> Result<Option<&'static str>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let denial = if op.is_listing() {
            // A list has no host-wide read grant: its results are filtered per
            // session. Bind the admitted set of epoch-bearing grants instead,
            // rather than demanding a wildcard or widening scoped authority.
            (!same_authority(&tx, op, now)?)
                .then_some("listing authority changed before the effect")
        } else {
            super::cutover::current_denial(&tx, &super::cutover::Authority::of(op), now)?
        };
        let (state, result) = match denial {
            None => ("dispatching", None),
            Some(_) => ("failed", Some("authority_revoked")),
        };
        tx.execute(
            "UPDATE op_journal SET state = ?3, result = ?4
             WHERE kid = ?1 AND jti = ?2 AND state IN ('admitted', 'dispatching')",
            params![op.kid, op.jti, state, result],
        )?;
        tx.commit()?;
        Ok(denial)
    }

    /// Permanently revoke a locally approved credential, in one immediate
    /// transaction: the credential, every grant it holds, every browser
    /// delegation it parents, and its undelivered agent messages. Records are
    /// kept (marked, never deleted). Idempotent: revoking again changes nothing.
    /// Local-owner mutation: never manufactures a signed credential or journal entry.
    pub fn owner_change_grants(
        &mut self,
        session: &str,
        recipient: &str,
        caps: &[Cap],
        remove: bool,
        now: i64,
    ) -> Result<Option<&'static str>> {
        if !super::envelope::valid_session_id(session)
            || !super::envelope::valid_key_id(recipient)
            || caps.is_empty()
            || caps.len() > super::envelope::MAX_GRANT_CAPS
            || caps.iter().any(|cap| !cap.grantable())
            || caps
                .windows(2)
                .any(|pair| pair[0].as_str() >= pair[1].as_str())
        {
            return Ok(Some("invalid session, recipient or capabilities"));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(reason) = unknown_session(&tx, session)? {
            return Ok(Some(reason));
        }
        if let Some(reason) = change_session_grants(&tx, session, recipient, caps, remove, now)? {
            return Ok(Some(reason));
        }
        tx.commit()?;
        Ok(None)
    }

    pub fn revoke_credential(&mut self, cred_id: &str, now: i64) -> Result<Revocation> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revoked_at: Option<Option<i64>> = tx
            .query_row(
                "SELECT revoked_at FROM credentials WHERE cred_id = ?1",
                params![cred_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(earlier) = revoked_at else {
            bail!("unknown credential {cred_id}");
        };
        tx.execute(
            "UPDATE credentials SET revoked_at = ?2 WHERE cred_id = ?1 AND revoked_at IS NULL",
            params![cred_id, now],
        )?;
        let grants = tx.execute(
            "UPDATE grants SET revoked_at = ?2 WHERE cred_id = ?1 AND revoked_at IS NULL",
            params![cred_id, now],
        )?;
        let delegations = tx.execute(
            "UPDATE delegations SET revoked_at = ?2 WHERE parent_cred = ?1 AND revoked_at IS NULL",
            params![cred_id, now],
        )?;
        let messages = tx.execute(
            "UPDATE agent_inbox SET state = 'revoked', state_at = ?2
             WHERE kid = ?1 AND state IN ('queued','received')",
            params![cred_id, now],
        )?;
        let browser_keys = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT browser_kid FROM delegations WHERE parent_cred = ?1 ORDER BY browser_kid",
            )?;
            let rows = stmt.query_map(params![cred_id], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        tx.commit()?;
        Ok(Revocation {
            already: earlier.is_some(),
            grants,
            delegations,
            messages,
            browser_keys,
        })
    }

    /// Attribute content `authority` put into `session`'s terminal (idempotent).
    pub fn record_interaction(
        &self,
        session: &str,
        authority: &super::cutover::Authority,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO session_interactions(session_id, kid, dlg, cap, epoch, recorded_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![session, authority.kid, authority.dlg, authority.cap, authority.epoch, now],
        )?;
        Ok(())
    }

    /// Every authority attributed to `session`, as standing records (no expiry:
    /// an expired input is not a revoked one).
    pub fn interactions(&self, session: &str) -> Result<Vec<super::cutover::Authority>> {
        let mut statement = self.conn.prepare(
            "SELECT kid, dlg, cap, epoch FROM session_interactions WHERE session_id = ?1 ORDER BY rowid",
        )?;
        let rows = statement.query_map([session], |row| {
            Ok(super::cutover::Authority {
                kid: row.get(0)?,
                dlg: row.get(1)?,
                session: session.to_owned(),
                cap: row.get(2)?,
                epoch: row.get(3)?,
                exp: None,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Drop attributions whose interruption was handed to the terminal.
    pub fn forget_interactions(
        &mut self,
        session: &str,
        authorities: &[super::cutover::Authority],
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for authority in authorities {
            tx.execute(
                "DELETE FROM session_interactions WHERE session_id = ?1 AND kid = ?2
                   AND IFNULL(dlg, '') = IFNULL(?3, '') AND cap = ?4 AND IFNULL(epoch, -1) = IFNULL(?5, -1)",
                params![session, authority.kid, authority.dlg, authority.cap, authority.epoch],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A fresh agent process registered for `session`: nothing earlier runs in it.
    pub fn clear_interactions(&self, session: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM session_interactions WHERE session_id = ?1",
            [session],
        )?;
        Ok(())
    }

    /// Why `authority` does not hold right now (see [`super::cutover::current_denial`]).
    pub fn authority_denial(
        &self,
        authority: &super::cutover::Authority,
        now: i64,
    ) -> Result<Option<&'static str>> {
        let snapshot = self.conn.unchecked_transaction()?;
        super::cutover::current_denial(&snapshot, authority, now)
    }

    /// The epoch of the grant activation `authority` (whatever epoch it names)
    /// rests on right now, or `None` when it holds no current grant.
    pub fn current_epoch(
        &self,
        authority: &super::cutover::Authority,
        now: i64,
    ) -> Result<Option<i64>> {
        let snapshot = self.conn.unchecked_transaction()?;
        let any = super::cutover::Authority {
            epoch: None,
            ..authority.clone()
        };
        super::cutover::current_epoch(&snapshot, &any, now)
    }

    /// Startup recovery. `dispatching` may or may not have had its effect, so it
    /// becomes `uncertain` and is never retried automatically. `admitted` never
    /// started its effect: it fails, and the client signs a new operation.
    pub fn recover(&mut self) -> Result<usize> {
        let uncertain = self.conn.execute(
            "UPDATE op_journal SET state = 'uncertain' WHERE state = 'dispatching'",
            [],
        )?;
        let failed = self.conn.execute(
            "UPDATE op_journal SET state = 'failed', result = 'not_started_after_restart' WHERE state = 'admitted'",
            [],
        )?;
        Ok(uncertain + failed)
    }

    /// True when the journal holds an operation for this session that is not
    /// settled as `done`: its effect may or may not have happened, so state
    /// derived from it (a restore checkpoint) cannot be trusted to re-run it.
    pub fn session_unsettled(&self, session_id: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM op_journal WHERE session_id = ?1 AND cap = 'session.start' AND state NOT IN ('done', 'failed')",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// A wrapper for `session_id` is connected: that is the evidence a signed start
    /// was waiting for, so a start still `dispatching` (its launch wait has not
    /// recorded an outcome yet, or the host crashed first) or already `uncertain`
    /// (the wait gave up, or startup recovery) settles as `done`. One statement, so
    /// it cannot interleave with [`Store::mark`], which never moves a settled entry.
    ///
    /// Only the record the start itself wrote counts, and only when the wrapper
    /// matches it: a record a registration adopted for itself, another agent or
    /// another directory settles nothing.
    pub fn confirm_start(
        &mut self,
        session_id: &str,
        agent: conversations_core::AgentKind,
        cwd: &str,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE op_journal
             SET result = CASE state WHEN 'uncertain' THEN 'registered_after_deadline' ELSE 'started' END,
                 state = 'done'
             WHERE session_id = ?1 AND cap = 'session.start' AND state IN ('dispatching', 'uncertain')
               AND EXISTS (SELECT 1 FROM sessions s WHERE s.session_id = ?1
                           AND s.origin = 'signed_start' AND s.agent = ?2 AND s.cwd = ?3)",
            params![session_id, agent.as_str(), cwd],
        )? > 0)
    }

    /// Remember that a signed operation named this pull request for this session.
    /// Bounded per session and per host so a stream of signed operations cannot
    /// grow the polling set without limit.
    ///
    /// `authority` is the signed operation that named it: the poll re-reads the
    /// target only while that authority still holds (the latest signer wins).
    pub fn record_pr_target(
        &mut self,
        session: &str,
        owner: &str,
        repo: &str,
        number: i64,
        authority: Option<&super::cutover::Authority>,
        now: i64,
    ) -> Result<()> {
        self.record_pr_target_inner(session, owner, repo, number, authority, now, true)
    }

    /// Local transcript discovery can add a target but never replace its provenance or schedule.
    pub fn discover_pr_target(
        &mut self,
        session: &str,
        owner: &str,
        repo: &str,
        number: i64,
        now: i64,
    ) -> Result<()> {
        self.record_pr_target_inner(session, owner, repo, number, None, now, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_pr_target_inner(
        &mut self,
        session: &str,
        owner: &str,
        repo: &str,
        number: i64,
        authority: Option<&super::cutover::Authority>,
        now: i64,
        replace: bool,
    ) -> Result<()> {
        const PER_SESSION: i64 = 20;
        const PER_HOST: i64 = 200;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let known: i64 = tx.query_row(
            "SELECT COUNT(*) FROM session_pr_targets WHERE session_id = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
            params![session, owner, repo, number],
            |r| r.get(0),
        )?;
        if !replace {
            if known != 0 {
                return Ok(());
            }
            let local = tx
                .query_row(
                    "SELECT 1 FROM sessions WHERE session_id=?1",
                    [session],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            ensure!(local, "PR discovery requires a local session");
        }
        if known == 0 {
            let (in_session, in_host): (i64, i64) = tx.query_row(
                "SELECT (SELECT COUNT(*) FROM session_pr_targets WHERE session_id = ?1),
                        (SELECT COUNT(*) FROM session_pr_targets)",
                params![session],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            ensure!(
                in_session < PER_SESSION && in_host < PER_HOST,
                "too many tracked pull requests"
            );
        }
        // Any signed `pr.sync` or `pr.comment` is the user explicitly asking
        // for this target to be fresh: reset the per-target backoff to the
        // floor so the next host poll pass runs it immediately. A newly
        // inserted row inherits the column defaults (next_poll_at = 0,
        // current_delay_secs = 60). On conflict we also wipe `fingerprint`
        // so the fetch right after a manual refresh does not treat its
        // first result as "unchanged" and immediately double the delay.
        tx.execute(
            "INSERT INTO session_pr_targets(session_id, owner, repo, number, added_at, kid, dlg, cap, epoch)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(session_id, owner, repo, number)
             DO UPDATE SET kid = excluded.kid, dlg = excluded.dlg, cap = excluded.cap,
                           epoch = excluded.epoch, next_poll_at = 0,
                           current_delay_secs = ?10, fingerprint = NULL",
            params![
                session,
                owner,
                repo,
                number,
                now,
                authority.map(|a| &a.kid),
                authority.and_then(|a| a.dlg.as_ref()),
                authority.map(|a| &a.cap),
                authority.and_then(|a| a.epoch),
                PR_POLL_MIN_DELAY_SECS,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record the outcome of one background-poll pass against one target so
    /// the next pass picks the right interval. `changed = true` means the PR's
    /// meaningful metadata (status + comments) differs from what was last
    /// seen; in that case the delay resets to the floor. `changed = false`
    /// (including a transient failure reported by `transient = true`) doubles
    /// the delay up to the ceiling. The caller passes the newly-computed
    /// fingerprint; on a transient failure pass `None` to keep the previous
    /// fingerprint so the next successful read can still see "actually
    /// changed vs. the last good read".
    #[allow(clippy::too_many_arguments)]
    pub fn apply_pr_poll_outcome(
        &mut self,
        session: &str,
        owner: &str,
        repo: &str,
        number: i64,
        new_fingerprint: Option<&str>,
        changed: bool,
        now: i64,
    ) -> Result<()> {
        let current: Option<i64> = self
            .conn
            .query_row(
                "SELECT current_delay_secs FROM session_pr_targets
                 WHERE session_id = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
                params![session, owner, repo, number],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Ok(());
        };
        let next_delay = if changed {
            PR_POLL_MIN_DELAY_SECS
        } else {
            (current.max(PR_POLL_MIN_DELAY_SECS) * 2).min(PR_POLL_MAX_DELAY_SECS)
        };
        if let Some(fp) = new_fingerprint {
            self.conn.execute(
                "UPDATE session_pr_targets
                 SET next_poll_at = ?5, current_delay_secs = ?6, fingerprint = ?7
                 WHERE session_id = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
                params![
                    session,
                    owner,
                    repo,
                    number,
                    now + next_delay,
                    next_delay,
                    fp
                ],
            )?;
        } else {
            self.conn.execute(
                "UPDATE session_pr_targets
                 SET next_poll_at = ?5, current_delay_secs = ?6
                 WHERE session_id = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
                params![session, owner, repo, number, now + next_delay, next_delay],
            )?;
        }
        Ok(())
    }

    /// Pull requests recorded for `session` whose recording authority still
    /// holds now AND whose per-target schedule says they are due now. Returns
    /// each target with its last-seen fingerprint, so the caller can tell a
    /// changed result from a repeated one without reading the local cache.
    /// Targets to poll for `session`. With `explicit=true`, ignores the backoff
    /// schedule and returns every recorded target whose authority still holds —
    /// what a user-initiated `pr sync` wants (manual bypass).
    pub fn pr_targets_poll_candidates(
        &self,
        session: &str,
        now: i64,
        explicit: bool,
    ) -> Result<Vec<PrPollCandidate>> {
        type Row = (
            String,
            String,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
        );
        let snapshot = self.conn.unchecked_transaction()?;
        let rows: Vec<Row> = if explicit {
            let mut stmt = snapshot.prepare(
                "SELECT owner, repo, number, kid, dlg, cap, epoch, fingerprint
                 FROM session_pr_targets WHERE session_id = ?1
                 ORDER BY added_at, owner, repo, number",
            )?;
            let rows = stmt.query_map(params![session], |r| {
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
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        } else {
            let mut stmt = snapshot.prepare(
                "SELECT owner, repo, number, kid, dlg, cap, epoch, fingerprint
                 FROM session_pr_targets WHERE session_id = ?1 AND next_poll_at <= ?2
                 ORDER BY added_at, owner, repo, number",
            )?;
            let rows = stmt.query_map(params![session, now], |r| {
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
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        // Owner-recorded targets (kid IS NULL) are polled if the session is
        // locally managed. This covers PRs the owner synced directly via the
        // web UI without a signed operation. No authority recheck is needed:
        // the host owns these sessions and the user explicitly recorded them.
        let session_is_local: bool = snapshot
            .query_row(
                "SELECT 1 FROM sessions WHERE session_id = ?1",
                params![session],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let mut due = Vec::new();
        for (owner, repo, number, kid, dlg, cap, epoch, fingerprint) in rows {
            let (Some(kid), Some(cap)) = (kid, cap) else {
                // No recorded authority — include only when the session is local
                // (owner-initiated target) and the schedule is due. Legacy rows
                // on sessions the host does not own are still skipped.
                if session_is_local {
                    due.push((owner, repo, number, fingerprint));
                }
                continue;
            };
            let authority = super::cutover::Authority {
                kid,
                dlg,
                session: session.into(),
                cap,
                epoch,
                exp: None,
            };
            if super::cutover::current_denial(&snapshot, &authority, now)?.is_none() {
                due.push((owner, repo, number, fingerprint));
            }
        }
        Ok(due)
    }

    /// Pull requests recorded for `session` whose recording authority still
    /// holds now. A row without provenance (recorded before provenance existed)
    /// is never polled; a new signed sync or comment records it again.
    pub fn pr_targets(&self, session: &str, now: i64) -> Result<Vec<(String, String, i64)>> {
        type Row = (
            String,
            String,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
        );
        let snapshot = self.conn.unchecked_transaction()?;
        let rows: Vec<Row> = {
            let mut stmt = snapshot.prepare(
                "SELECT owner, repo, number, kid, dlg, cap, epoch FROM session_pr_targets WHERE session_id = ?1
                 ORDER BY added_at, owner, repo, number",
            )?;
            let rows = stmt.query_map(params![session], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut current = Vec::new();
        for (owner, repo, number, kid, dlg, cap, epoch) in rows {
            let (Some(kid), Some(cap)) = (kid, cap) else {
                continue;
            };
            // A standing target: bound to the grant activation that recorded
            // it, not to the recording operation's expiry.
            let authority = super::cutover::Authority {
                kid,
                dlg,
                session: session.into(),
                cap,
                epoch,
                exp: None,
            };
            if super::cutover::current_denial(&snapshot, &authority, now)?.is_none() {
                current.push((owner, repo, number));
            }
        }
        Ok(current)
    }

    /// Admit one signed agent message into the local inbox under its
    /// signer-scoped id. Re-admitting the same operation is a no-op; any other
    /// row already holding that id or that (kid, jti) is a conflict, never a
    /// silent success.
    pub fn inbox_admit(
        &mut self,
        op: &Verified,
        source: &str,
        source_host: &str,
        text: &str,
        reply_to: Option<&str>,
        now: i64,
    ) -> Result<String> {
        let id = super::envelope::agent_message_id(&op.kid, &op.jti);
        let inserted = self.conn.execute(
            "INSERT INTO agent_inbox(message_id, session_id, source_session, kid, text, reply_to, signed_exp, payload_sha256, admitted_at, state_at, jti, source_host)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11) ON CONFLICT DO NOTHING",
            params![id, op.op.session_id(), source, op.kid, text, reply_to, op.exp, op.payload_sha256, now, op.jti, source_host],
        )?;
        if inserted == 0 {
            let same: bool = self
                .conn
                .query_row(
                    "SELECT message_id = ?1 AND kid = ?2 AND jti = ?3 AND session_id = ?4 AND payload_sha256 = ?5
                     FROM agent_inbox WHERE message_id = ?1 OR (kid = ?2 AND jti = ?3)",
                    params![id, op.kid, op.jti, op.op.session_id(), op.payload_sha256],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(false);
            ensure!(same, "message id already holds another message");
        }
        Ok(id)
    }

    /// The sender's authority over `session` right now (credential live, inside
    /// its ceiling, explicit current grant). The only source of truth for whether
    /// a queued message may still be handed to the terminal or agent.
    fn sender_still_authorized(&mut self, kid: &str, session: &str) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let denied = holds_authority(&tx, kid, Cap::SessionWrite, Some(session), false)?;
        tx.commit()?;
        Ok(denied.is_none())
    }

    /// Messages for `session` that may still be delivered, oldest first. Any item
    /// whose sender no longer holds a current grant is marked `revoked` and
    /// withheld; items older than a day are marked `expired`.
    pub fn inbox_deliverable(
        &mut self,
        session: &str,
        now: i64,
        limit: usize,
    ) -> Result<Vec<InboxRow>> {
        const TTL_SECS: i64 = 24 * 3600;
        let rows: Vec<InboxRow> = {
            let mut stmt = self.conn.prepare(
                "SELECT message_id, session_id, source_session, kid, text, reply_to, admitted_at, state
                 FROM agent_inbox WHERE session_id = ?1 AND state IN ('queued','received')
                 ORDER BY admitted_at, message_id LIMIT ?2",
            )?;
            let iter = stmt.query_map(params![session, limit as i64], |r| {
                Ok(InboxRow {
                    message_id: r.get(0)?,
                    session_id: r.get(1)?,
                    source_session: r.get(2)?,
                    kid: r.get(3)?,
                    text: r.get(4)?,
                    reply_to: r.get(5)?,
                    admitted_at: r.get(6)?,
                    state: r.get(7)?,
                })
            })?;
            iter.collect::<std::result::Result<_, _>>()?
        };
        let mut out = Vec::new();
        for row in rows {
            let verdict = if now - row.admitted_at > TTL_SECS {
                Some("expired")
            } else if !self.sender_still_authorized(&row.kid, &row.session_id)? {
                Some("revoked")
            } else {
                None
            };
            match verdict {
                Some(state) => {
                    self.conn.execute(
                        "UPDATE agent_inbox SET state = ?2, state_at = ?3 WHERE message_id = ?1",
                        params![row.message_id, state, now],
                    )?;
                }
                None => out.push(row),
            }
        }
        Ok(out)
    }

    /// One deliverable message by id for `session` (same checks as the list).
    pub fn inbox_message(
        &mut self,
        session: &str,
        message_id: &str,
        now: i64,
    ) -> Result<Option<InboxRow>> {
        Ok(self
            .inbox_deliverable(session, now, 200)?
            .into_iter()
            .find(|row| row.message_id == message_id))
    }

    pub fn inbox_mark(
        &mut self,
        session: &str,
        ids: &[String],
        state: &str,
        now: i64,
    ) -> Result<usize> {
        let mut changed = 0;
        for id in ids {
            changed += self.conn.execute(
                "UPDATE agent_inbox SET state = ?3, state_at = ?4 WHERE message_id = ?1 AND session_id = ?2
                 AND state IN ('queued','received')",
                params![id, session, state, now],
            )?;
        }
        Ok(changed)
    }

    /// The signing key of a message `session` received.
    pub fn inbox_sender(&self, session: &str, message_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT kid FROM agent_inbox WHERE message_id = ?1 AND session_id = ?2",
                params![message_id, session],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// `(state, sender session)` of a message `session` received.
    pub fn inbox_received(
        &self,
        session: &str,
        message_id: &str,
    ) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT state, source_session FROM agent_inbox WHERE message_id = ?1 AND session_id = ?2",
                params![message_id, session],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Who sent a message `session` received: `(source session, signed source
    /// host)`. Rows from before hosts signed their own id have no host.
    pub fn inbox_origin(
        &self,
        session: &str,
        message_id: &str,
    ) -> Result<Option<(String, Option<String>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT source_session, source_host FROM agent_inbox WHERE message_id = ?1 AND session_id = ?2",
                params![message_id, session],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Native identity for a local session: an explicit local binding, else the
    /// native id the host recorded from the agent (Claude: the session id).
    pub fn native_for(&self, session: &str) -> Result<Option<String>> {
        if let Some(native) = self
            .conn
            .query_row(
                "SELECT native FROM native_bindings WHERE session_id = ?1",
                params![session],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(Some(native));
        }
        Ok(self.session(session)?.and_then(|s| s.resume_id))
    }

    /// Bind `native` to `session`, once. Rebinding to the same value is a no-op;
    /// a different value, or a native id already owned by another session, fails.
    pub fn bind_native(&mut self, session: &str, native: &str) -> Result<()> {
        if let Some(existing) = self.native_for(session)? {
            ensure!(
                existing == native,
                "this session already belongs to another native session"
            );
            return Ok(());
        }
        let owner: Option<String> = self
            .conn
            .query_row(
                "SELECT session_id FROM native_bindings WHERE native = ?1",
                params![native],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            owner.is_none(),
            "that native session already belongs to another Conversations session"
        );
        self.conn.execute(
            "INSERT INTO native_bindings(session_id, native, bound_at) VALUES(?1, ?2, ?3)",
            params![session, native, now_secs()],
        )?;
        Ok(())
    }

    /// This host's agent signing key (generated once, 0600 with the database).
    pub fn agent_key(&mut self) -> Result<(Vec<u8>, Vec<u8>)> {
        if let Some(row) = self
            .conn
            .query_row(
                "SELECT pkcs8, public_key FROM agent_key WHERE id = 1",
                [],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
        {
            return Ok(row);
        }
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| anyhow::anyhow!("key generation failed"))?;
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| anyhow::anyhow!("key generation failed"))?;
        let public = pair.public_key().as_ref().to_vec();
        self.conn.execute(
            "INSERT INTO agent_key(id, pkcs8, public_key, created_at) VALUES(1, ?1, ?2, ?3)",
            params![pkcs8.as_ref(), public, now_secs()],
        )?;
        Ok((pkcs8.as_ref().to_vec(), public))
    }

    pub fn target_add(
        &mut self,
        session: &str,
        host_id: &str,
        host_key: &[u8],
        label: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agent_targets(session_id, host_id, host_key, label, added_at) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id) DO UPDATE SET host_id = ?2, host_key = ?3, label = ?4, added_at = ?5",
            params![session, host_id, host_key, label, now_secs()],
        )?;
        Ok(())
    }

    pub fn target_remove(&mut self, session: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM agent_targets WHERE session_id = ?1",
            params![session],
        )? > 0)
    }

    pub fn target(&self, session: &str) -> Result<Option<(String, Vec<u8>, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT host_id, host_key, label FROM agent_targets WHERE session_id = ?1",
                params![session],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    pub fn targets(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT session_id, host_id, label FROM agent_targets ORDER BY added_at")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn send_get(&self, source: &str, target: &str, idem: &str) -> Result<Option<SendRow>> {
        self.send_where(
            "source_session = ?1 AND target_session = ?2 AND idem = ?3",
            params![source, target, idem],
        )
    }

    /// The send `source` made under message id `message_id`.
    pub fn send_by_id(&self, source: &str, message_id: &str) -> Result<Option<SendRow>> {
        self.send_where(
            "source_session = ?1 AND message_id = ?2",
            params![source, message_id],
        )
    }

    fn send_where(&self, filter: &str, args: impl rusqlite::Params) -> Result<Option<SendRow>> {
        select_send(&self.conn, filter, args)
    }

    pub fn send_put(&mut self, row: &SendRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agent_sends(source_session, target_session, idem, request_sha256, attempt, message_id, payload, sig, kid, host_id, exp, state, detail, checked_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(source_session, target_session, idem) DO UPDATE SET
               attempt=?5, message_id=?6, payload=?7, sig=?8, kid=?9, host_id=?10, exp=?11, state=?12, detail=?13, checked_at=?14",
            params![row.source, row.target, row.idem, row.request_sha256, row.attempt, row.message_id,
                    row.payload, row.sig, row.kid, row.host_id, row.exp, row.state, row.detail, row.checked_at],
        )?;
        Ok(())
    }

    /// Record a state the recipient host vouched for on attempt `row.message_id`
    /// and return the send as it is now stored. An answer for another attempt
    /// changes nothing, and a slower answer never moves the state back: a
    /// recipient only goes queued -> received -> a final state. While the stored
    /// attempt is admitted, `reply` (the message this send answers) is
    /// acknowledged in the SAME transaction, so neither is kept without the other.
    pub fn send_settle(
        &mut self,
        row: &SendRow,
        state: &str,
        now: i64,
        reply: Option<&str>,
    ) -> Result<Option<SendRow>> {
        const RANK: &str =
            "CASE {} WHEN 'prepared' THEN 0 WHEN 'queued' THEN 1 WHEN 'received' THEN 2 ELSE 3 END";
        let keys = "source_session = ?1 AND target_session = ?2 AND idem = ?3";
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            &format!(
                "UPDATE agent_sends SET state = ?5, checked_at = ?6
                 WHERE {keys} AND message_id = ?4 AND (state = ?5 OR {} < {})",
                RANK.replace("{}", "state"),
                RANK.replace("{}", "?5"),
            ),
            params![row.source, row.target, row.idem, row.message_id, state, now],
        )?;
        let stored = select_send(&tx, keys, params![row.source, row.target, row.idem])?;
        if let (Some(stored), Some(reply)) = (&stored, reply) {
            if stored.message_id == row.message_id
                && ["queued", "received", "acknowledged"].contains(&stored.state.as_str())
            {
                tx.execute(
                    "UPDATE agent_inbox SET state = 'acknowledged', state_at = ?3
                     WHERE message_id = ?1 AND session_id = ?2 AND state IN ('queued','received')",
                    params![reply, row.source, now],
                )?;
            }
        }
        tx.commit()?;
        Ok(stored)
    }

    /// State of the message `kid` sent to `session` as operation `jti`, as this
    /// host recorded it. Only that signer's own operations are visible to it.
    pub fn inbox_state(&self, kid: &str, session: &str, jti: &str) -> Result<String> {
        Ok(self
            .conn
            .query_row(
                "SELECT state FROM agent_inbox WHERE kid = ?1 AND jti = ?2 AND session_id = ?3",
                params![kid, jti, session],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| "absent".to_owned()))
    }

    /// Reserve (or find) a same-machine handoff. `Ok(Err(row))` = this source and
    /// key were used before: the recorded child and state.
    pub fn handoff_reserve(
        &mut self,
        source: &str,
        idem: &str,
        child: &str,
        agent: &str,
        brief_sha256: &[u8],
    ) -> Result<std::result::Result<(), HandoffRow>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior = tx.query_row(
            "SELECT child, agent, brief_sha256, state, error FROM agent_handoffs WHERE source_session = ?1 AND idem = ?2",
            params![source, idem],
            |r| Ok(HandoffRow { child: r.get(0)?, agent: r.get(1)?, brief_sha256: r.get(2)?, state: r.get(3)?, error: r.get(4)? }),
        ).optional()?;
        if let Some(prior) = prior {
            return Ok(Err(prior));
        }
        tx.execute(
            "INSERT INTO agent_handoffs(source_session, idem, child, agent, brief_sha256, state, created_at) VALUES(?1, ?2, ?3, ?4, ?5, 'starting', ?6)",
            params![source, idem, child, agent, brief_sha256, now_secs()],
        )?;
        tx.commit()?;
        Ok(Ok(()))
    }

    pub fn handoff_child_state(&self, source: &str, child: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT state FROM agent_handoffs WHERE source_session = ?1 AND child = ?2",
                params![source, child],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn handoff_finish(
        &mut self,
        source: &str,
        idem: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE agent_handoffs SET state = ?3, error = ?4 WHERE source_session = ?1 AND idem = ?2",
            params![source, idem, state, error],
        )?;
        Ok(())
    }

    /// Fixed host identity so signatures captured from a real browser can be
    /// replayed against a host whose audience id is known in advance.
    #[cfg(test)]
    pub(crate) fn install_test_identity(&mut self, seed: &[u8; 32]) -> Result<String> {
        let pair =
            Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| anyhow::anyhow!("seed"))?;
        let public = pair.public_key().as_ref().to_vec();
        // PKCS#8 v1 wrapper of the seed (RFC 8410), as ring accepts for signing.
        let mut pkcs8 = hex_prefix().to_vec();
        pkcs8.extend_from_slice(seed);
        self.conn.execute(
            "INSERT OR REPLACE INTO host_identity(id, pkcs8, public_key, created_at) VALUES(1, ?2, ?1, 1)",
            params![public, pkcs8],
        )?;
        Ok(key_id(&public))
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }
}

fn select_send(
    conn: &Connection,
    filter: &str,
    args: impl rusqlite::Params,
) -> Result<Option<SendRow>> {
    Ok(conn.query_row(
        &format!(
            "SELECT source_session, target_session, idem, request_sha256, attempt, message_id, payload, sig, kid, host_id, exp, state, detail, checked_at
             FROM agent_sends WHERE {filter}"
        ),
        args,
        |r| Ok(SendRow {
            source: r.get(0)?, target: r.get(1)?, idem: r.get(2)?,
            request_sha256: r.get(3)?, attempt: r.get(4)?, message_id: r.get(5)?, payload: r.get(6)?,
            sig: r.get(7)?, kid: r.get(8)?, host_id: r.get(9)?, exp: r.get(10)?, state: r.get(11)?,
            detail: r.get(12)?, checked_at: r.get(13)?,
        }),
    ).optional()?)
}

/// A settable clock for tests that run in their own process (`isolated`); it
/// lets fixtures signed for a fixed time and expiry rules be exercised exactly.
#[cfg(test)]
pub(crate) mod test_clock {
    use std::sync::atomic::{AtomicI64, Ordering};
    static NOW: AtomicI64 = AtomicI64::new(0);
    pub fn set(now: i64) {
        NOW.store(now, Ordering::SeqCst);
    }
    pub fn get() -> Option<i64> {
        match NOW.load(Ordering::SeqCst) {
            0 => None,
            now => Some(now),
        }
    }
}
