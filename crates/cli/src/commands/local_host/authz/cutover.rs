//! Revocation cutover at the final local effect.
//!
//! Admission decides whether a signed operation may run; it does not keep that
//! decision alive. Every queued effect therefore carries the exact authority it
//! was admitted under ([`Authority`]) and is re-checked against the CURRENT
//! rows immediately before the effect happens:
//!
//! - journaled operations re-check inside the transaction that moves them to
//!   `dispatching` ([`super::store::Store::begin`]);
//! - terminal writes (keystrokes, a reply body and its delayed Enter, a peer
//!   message, a staged paste) re-check in the wrapper under the terminal writer
//!   lock, immediately before each write ([`Gate`]);
//! - native agent deliveries re-check the inbox row before each hand-off.
//!
//! Each queued effect names the exact grant ACTIVATION (epoch) it was admitted
//! under and its signed expiry: a grant removed and granted again is a new
//! activation, and an effect past its expiry never happens.
//!
//! Linearization: a revocation is one IMMEDIATE commit. Each effect's check is
//! one read transaction (a single snapshot, entirely before or after that
//! commit) taken immediately before its one write; terminal writes take it
//! under the terminal writer lock, so check and write are not interleaved with
//! other writes. An effect whose snapshot preceded the commit is dispatched: it
//! may complete after the commit and is reported, never undone. Per terminal
//! that is at most the ONE write in progress under the lock (a reply's Enter and
//! a peer message's submit are separate writes, each re-checked). Everything
//! queued behind it re-checks after the commit and is refused. The host's
//! interaction cancel waits a bounded time for the writer lock: once it has it,
//! no write admitted under the old authority is in progress; if a write is
//! blocked in the kernel, it reports the terminal busy instead of waiting.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use super::envelope::{Cap, Verified};
use super::store::{current_grant, delegated_parent};

/// The authority one queued effect was admitted under: exactly the signing key,
/// its delegation (if any), the session, the capability, the grant ACTIVATION
/// (epoch) that gave it, and the signed expiry. A grant removed and granted
/// again is a new activation, so queued work never revives on a re-grant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Authority {
    pub kid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dlg: Option<String>,
    pub session: String,
    pub cap: String,
    /// Absent = unbound, and refused at every effect.
    #[serde(default)]
    pub epoch: Option<i64>,
    /// Unix seconds after which the effect may no longer happen. `None` only
    /// for a standing record (a tracked pull request), bound by its epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,
}

impl Authority {
    /// The admitted operation's own authority, expiring with its envelope.
    pub fn of(op: &Verified) -> Self {
        Authority {
            kid: op.kid.clone(),
            dlg: op.dlg.clone(),
            session: op.op.session_id().to_owned(),
            cap: op.op.capability().as_str().into(),
            epoch: Some(op.grant_epoch),
            exp: Some(op.exp),
        }
    }

    /// A queued agent message: its sender's write authority over the recipient,
    /// at grant activation `epoch`, deliverable until `exp`.
    pub fn sender(kid: &str, session: &str, epoch: Option<i64>, exp: Option<i64>) -> Self {
        Authority {
            kid: kid.into(),
            dlg: None,
            session: session.into(),
            cap: Cap::SessionWrite.as_str().into(),
            epoch,
            exp,
        }
    }

    /// The same authority without its expiry: what a revocation looks for.
    /// (An expired input is not a revoked one; it must not trigger a cancel.)
    pub fn standing(self) -> Self {
        Authority { exp: None, ..self }
    }
}

/// Why `authority` no longer holds right now, or `None` when it still does.
/// The same rules as admission: delegation live and in scope, parent credential
/// live, inside its ceiling, and the SAME grant activation still current; and
/// the signed expiry not passed. Callers run it inside one read transaction, so
/// it sees one snapshot: entirely before or entirely after a revocation commit.
pub fn current_denial(
    conn: &Connection,
    authority: &Authority,
    now: i64,
) -> Result<Option<&'static str>> {
    let Some(epoch) = authority.epoch else {
        return Ok(Some("queued authority names no grant activation"));
    };
    Ok(check(conn, authority, Some(epoch), now)?.err())
}

/// The grant activation `authority` rests on now (any epoch), if any.
pub(super) fn current_epoch(
    conn: &Connection,
    authority: &Authority,
    now: i64,
) -> Result<Option<i64>> {
    Ok(check(conn, authority, None, now)?.ok())
}

fn check(
    conn: &Connection,
    authority: &Authority,
    epoch: Option<i64>,
    now: i64,
) -> Result<std::result::Result<i64, &'static str>> {
    if authority.exp.is_some_and(|exp| now > exp) {
        return Ok(Err("authorization expired before the effect"));
    }
    let Some(cap) = Cap::parse(&authority.cap) else {
        return Ok(Err("unknown capability"));
    };
    let parent = match &authority.dlg {
        None => authority.kid.clone(),
        Some(dlg) => {
            match delegated_parent(conn, &authority.kid, cap, &authority.session, dlg, now)? {
                Ok(parent) => parent,
                Err(reason) => return Ok(Err(reason)),
            }
        }
    };
    current_grant(
        conn,
        &parent,
        cap,
        Some(&authority.session),
        authority.dlg.is_some(),
        epoch,
    )
}

/// The effect-time check a wrapper (or any process sharing the host directory)
/// runs before a final write. Reads `<dir>/authz.db` directly: a WAL reader never
/// waits on the revoking writer, and sees its commit as soon as it is made.
/// Fails closed: a missing database or a read error refuses the effect.
pub struct Gate {
    dir: PathBuf,
    conn: Mutex<Option<Connection>>,
}

impl Gate {
    pub fn new(dir: &Path) -> Self {
        Gate {
            dir: dir.to_path_buf(),
            conn: Mutex::new(None),
        }
    }

    /// `None` = the effect may happen now. Effects without signed provenance
    /// (the local owner's own terminal) carry no authority and are not gated.
    pub fn denial(&self, authority: Option<&Authority>) -> Option<String> {
        let authority = authority?;
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if conn.is_none() {
            match Connection::open_with_flags(
                self.dir.join("authz.db"),
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .and_then(|c| c.busy_timeout(Duration::from_secs(2)).map(|()| c))
            {
                Ok(opened) => *conn = Some(opened),
                Err(_) => return Some("local authority is unavailable".into()),
            }
        }
        // One read transaction: the check sees a single snapshot.
        let checked = conn
            .as_ref()
            .expect("opened above")
            .unchecked_transaction()
            .map_err(anyhow::Error::from)
            .and_then(|snapshot| current_denial(&snapshot, authority, super::store::now_secs()));
        match checked {
            Ok(None) => None,
            Ok(Some(reason)) => Some(reason.into()),
            Err(_) => {
                *conn = None;
                Some("local authority is unavailable".into())
            }
        }
    }
}
