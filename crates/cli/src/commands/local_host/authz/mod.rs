//! Local execution authority for the host. Remote operations arrive as signed
//! frames and are verified here against credentials and grants that were
//! approved on this machine; the central server, its database and the account
//! that authenticated the transport confer no authority.
//!
//! Authority is re-checked at the final local effect, not only at admission:
//! queued work carries the exact authority it was admitted under and stops
//! once a local revocation (`host credential revoke`, signed `grant.remove`)
//! commits. See [`cutover`]. Not
//! gated by signed authority here: same-machine handoff, MCP-to-PTY, legacy
//! `live --host` and central uploads.

pub mod attest;
pub mod cutover;
pub mod dispatch;
pub mod envelope;
pub mod human;
pub mod passkey;
pub mod sessions;
pub mod store;

pub use envelope::SignedFrame;

use std::path::Path;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};

use envelope::Alg;
use store::{Grant, Kind, Store};

/// `host credential add`: locally approve a key. Reaching this function is the
/// local-approval step (it needs write access to the host's private runtime
/// directory), including the owner-authenticated loopback management UI.
pub fn enroll_from_cli(
    dir: &Path,
    kind: &str,
    alg: &str,
    public_key_b64: &str,
    label: &str,
    grants: &[String],
) -> Result<(String, String)> {
    prepare_dir(dir)?;
    let kind = Kind::parse(kind).context("kind must be `owner` or `agent`")?;
    let alg = Alg::parse(alg).context("algorithm must be `ed25519` or `es256`")?;
    let public_key = STANDARD
        .decode(public_key_b64.trim())
        .context("public key must be standard base64")?;
    let grants = grants
        .iter()
        .map(|g| Grant::parse(g))
        .collect::<Result<Vec<_>>>()?;
    let mut store = Store::open(dir)?;
    let cred_id = store.enroll(kind, alg, &public_key, label, &grants)?;
    Ok((cred_id, store.host_id()?))
}

/// `host identity`: the audience id signers must put in `aud`.
pub fn host_id(dir: &Path) -> Result<String> {
    prepare_dir(dir)?;
    Store::open(dir)?.host_id()
}

pub(crate) fn ensure_dir(dir: &Path) -> Result<()> {
    prepare_dir(dir)
}

fn prepare_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    Ok(())
}

/// Look up the canonical local record for a session.
pub fn lookup(dir: &Path, session_id: &str) -> Result<Option<sessions::LocalSession>> {
    Store::open(dir)?.session(session_id)
}

/// The local session id history from `agent`'s native session `native` belongs
/// to, the same for every command and run:
/// - the canonical record bound to it;
/// - else the session its history was already stored under, when that is a
///   session id (never a native id stored as one);
/// - else the native id itself when it is a session id;
/// - else (OpenCode's `ses_…`) the id derived from it by [`derived_session_id`].
///
/// A runtime directory without an authority database is not given one.
pub fn canonical_for_native(
    dir: &Path,
    agent: conversations_core::AgentKind,
    native: &str,
) -> Result<String> {
    if dir.join("authz.db").exists() {
        if let Some(bound) = Store::open(dir)?.session_for_native(agent, native)? {
            return Ok(bound);
        }
    }
    if dir.join("history.db").exists() {
        if let Some(stored) = super::history::History::open(dir)?
            .session_for_native(agent, native)?
            .filter(|stored| envelope::valid_session_id(stored))
        {
            return Ok(stored);
        }
    }
    if envelope::valid_session_id(native) {
        return Ok(native.to_owned());
    }
    Ok(derived_session_id(agent, native))
}

/// The canonical local record for a native session the user explicitly selected
/// on this machine (`upload`), created when none exists:
/// metadata only (agent, working directory, native binding), no agent is
/// launched. The id is [`canonical_for_native`]'s, so repeated discovery lands
/// on the same record. A record with that id bound to another native session
/// or agent is refused, never retargeted.
pub fn adopt_native(
    dir: &Path,
    agent: conversations_core::AgentKind,
    native: &str,
    cwd: Option<&str>,
) -> Result<String> {
    prepare_dir(dir)?;
    let id = canonical_for_native(dir, agent, native)?;
    let mut store = Store::open(dir)?;
    if store.session_for_native(agent, native)?.as_deref() == Some(id.as_str()) {
        return Ok(id);
    }
    let cwd = cwd
        .filter(|cwd| !cwd.is_empty())
        .with_context(|| format!("session {native} names no working directory"))?;
    // `adopted`: the host took a local session as its own.
    let inserted = store.insert_session(
        &sessions::LocalSession {
            session_id: id.clone(),
            agent,
            cwd: cwd.to_owned(),
            resume_id: Some(native.to_owned()),
            launcher: conversations_core::AgentLauncher::default(),
            adk_statusline: false,
        },
        "adopted",
    )?;
    if !inserted {
        let record = store.session(&id)?.context("session record vanished")?;
        anyhow::ensure!(
            record.agent == agent,
            "session {id} is recorded for a different agent"
        );
        store.bind_resume_once(&id, native)?;
    }
    Ok(id)
}

/// A stable session id (UUIDv8, RFC 9562) for a native session whose own id is
/// not one: the first 16 bytes of a domain-separated, length-prefixed SHA-256
/// of the agent and native id.
pub fn derived_session_id(agent: conversations_core::AgentKind, native: &str) -> String {
    let mut input = b"conversations-native-session/v1\n".to_vec();
    for part in [agent.as_str(), native] {
        input.extend_from_slice(&(part.len() as u32).to_be_bytes());
        input.extend_from_slice(part.as_bytes());
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, &input);
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_ref()[..16]);
    uuid::Builder::from_custom_bytes(bytes)
        .into_uuid()
        .hyphenated()
        .to_string()
}

/// Record a session this host is starting (signed start). `Ok(false)` means a
/// record already exists and was left untouched.
pub fn record_started(dir: &Path, session: &sessions::LocalSession) -> Result<bool> {
    Store::open(dir)?.insert_session(session, "signed_start")
}

/// Drop the record created by a start that failed before any terminal existed.
pub fn forget_started(dir: &Path, session_id: &str) -> Result<()> {
    Store::open(dir)?.delete_session(session_id, "signed_start")
}

/// Canonical record for a terminal that registered with this host. Every
/// managed launch (signed start, local `Start`, restore, cmux or normal backend)
/// reaches the host through this registration, so the record cannot depend on a
/// checkpoint or a known native id. An existing record is never overwritten: a
/// registration naming a different agent for the same session, or a native id
/// other than the established one, is refused before anything is written.
pub fn register_launch(
    dir: &Path,
    id: &str,
    agent: conversations_core::AgentKind,
    cwd: &str,
    command: &[String],
) -> Result<()> {
    anyhow::ensure!(envelope::valid_session_id(id), "invalid session id");
    // Claude names its own session after `--session-id`, and resumes the one
    // named by `--resume`: a launch argv naming the launch's own id is the
    // local proof of the native id.
    let native = (agent == conversations_core::AgentKind::Claude
        && command
            .windows(2)
            .any(|w| (w[0] == "--session-id" || w[0] == "--resume") && w[1] == id))
    .then(|| id.to_owned());
    prepare_dir(dir)?;
    let mut store = Store::open(dir)?;
    // The wrapper cannot say which launcher it was started with; a host-created
    // record (signed start, handoff) already carries it and is kept.
    let adopted = match store.session(id)? {
        Some(_) => false,
        // `adopted`: the host took a terminal that registered locally as its own.
        None => store.insert_session(
            &sessions::LocalSession {
                session_id: id.to_owned(),
                agent,
                cwd: cwd.to_owned(),
                resume_id: native.clone(),
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
            "adopted",
        )?,
    };
    if adopted {
        return Ok(());
    }
    // An existing record (possibly written since the lookup above) is validated
    // before anything in it changes.
    let record = store.session(id)?.context("session record vanished")?;
    anyhow::ensure!(
        record.agent == agent,
        "session {id} is recorded for a different agent"
    );
    if let Some(native) = &native {
        store.bind_resume_once(id, native)?;
    }
    Ok(())
}

/// A registered wrapper is now connected. A signed start of this session that is
/// still waiting for it, or that gave up waiting, is settled as done when the
/// wrapper matches the start's own record (see [`Store::confirm_start`]).
pub fn confirm_launch(
    dir: &Path,
    id: &str,
    agent: conversations_core::AgentKind,
    cwd: &str,
) -> Result<bool> {
    Store::open(dir)?.confirm_start(id, agent, cwd)
}

/// Bind the native agent session this host discovered through its own probes
/// (OpenCode bridge report, private Codex app-server). The first binding wins.
pub fn bind_native(dir: &Path, session_id: &str, native: &str) -> Result<bool> {
    Store::open(dir)?.bind_resume_once(session_id, native)
}

/// Remember a pull request a signed operation named for `session`.
pub fn record_pr_target(
    dir: &Path,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
    authority: &cutover::Authority,
) -> Result<()> {
    Store::open(dir)?.record_pr_target(
        session,
        owner,
        repo,
        number,
        Some(authority),
        store::now_secs(),
    )
}

/// Record an owner-initiated PR target with no signed authority. The
/// background poll includes these when the session is locally managed —
/// see `store::pr_targets_poll_candidates` — without manufacturing a fake
/// signed authority. Used by the web owner-local sync endpoint through
/// `refresh_local` so the target gets automatic background polling.
pub fn record_local_pr_target(
    dir: &Path,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<()> {
    Store::open(dir)?.discover_pr_target(session, owner, repo, number, store::now_secs())
}

/// Pull requests recorded locally for `session` whose recording authority still
/// holds (the only source of poll targets).
pub fn pr_targets(dir: &Path, session: &str) -> Result<Vec<(String, String, i64)>> {
    Store::open(dir)?.pr_targets(session, store::now_secs())
}

/// PR targets to poll for `session`. With `explicit=true` the backoff schedule
/// is bypassed and every recorded target whose authority still holds is
/// returned (a user-initiated `pr sync`). With `explicit=false` only targets
/// whose per-target `next_poll_at` is now or earlier are returned. Each
/// returned tuple carries the last-seen fingerprint so the caller can
/// detect changes without re-reading the local cache.
pub fn pr_poll_candidates(
    dir: &Path,
    session: &str,
    explicit: bool,
) -> Result<Vec<store::PrPollCandidate>> {
    Store::open(dir)?.pr_targets_poll_candidates(session, store::now_secs(), explicit)
}

/// Record the outcome of one background poll of one target so the next pass
/// takes the right interval. See [`Store::apply_pr_poll_outcome`].
pub fn apply_pr_poll_outcome(
    dir: &Path,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
    new_fingerprint: Option<&str>,
    changed: bool,
) -> Result<()> {
    Store::open(dir)?.apply_pr_poll_outcome(
        session,
        owner,
        repo,
        number,
        new_fingerprint,
        changed,
        store::now_secs(),
    )
}

/// Startup crash recovery for the operation journal.
pub fn recover(dir: &Path) -> Result<usize> {
    Store::open(dir)?.recover()
}

/// Whether a restore checkpoint for `session_id` may be acted on. An
/// interrupted signed start (`uncertain`, or any unsettled journal state) is
/// not: only the user can decide whether to start it again.
pub fn restorable(dir: &Path, session_id: &str) -> bool {
    // No authority database means no signed operation was ever admitted here.
    if !dir.join("authz.db").exists() {
        return true;
    }
    match Store::open(dir).and_then(|store| store.session_unsettled(session_id)) {
        Ok(unsettled) => !unsettled,
        // Fail closed: if the authority record cannot be read, do not relaunch.
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests;
