//! Sender side of signed agent messaging, and the local helpers the MCP tools
//! use for the inbox and same-machine handoff.
//!
//! Identity: this host owns one Ed25519 AGENT key (separate from the host
//! identity key, kept in `authz.db`). The receiving host's owner approves its
//! public key ONCE with `host credential add --kind agent` and grants it
//! `session.write:<recipient session>` per session. Where a message goes comes
//! only from `agent_targets`, which the local user fills with `host credential
//! pair-peer` from the recipient's own descriptor; central discovery never writes
//! it. The recipient host's reply is verified against that pinned key.

use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD as B64},
    Engine,
};
use ring::{digest, signature};

use super::authz::{
    self,
    envelope::{agent_message_id, key_id, valid_session_id, DOMAIN_OP, MAX_REPLY_TEXT_BYTES},
    store::{SendRow, Store},
};

pub const HOST_DESCRIPTOR_PREFIX: &str = authz::attest::DESCRIPTOR_PREFIX;

/// The runtime directory this process talks to: the wrapper-provided one, else
/// the default.
pub fn local_dir() -> PathBuf {
    std::env::var_os("CONVERSATIONS_HOST_DIR")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".conversations-host-local")
        })
}

/// Resolve the existing host for this server before choosing the local default.
pub fn runtime_for(server: &str) -> PathBuf {
    crate::commands::host_owner::HostOwner::running_runtime(server).unwrap_or_else(local_dir)
}

fn open(dir: &Path) -> Result<Store> {
    authz::ensure_dir(dir)?;
    Store::open(dir)
}

/// `(standard-base64 public key, key id)` of this host's agent key, to approve on
/// the recipient with `host credential add --kind agent --public-key …`.
pub fn agent_public(dir: &Path) -> Result<(String, String)> {
    let (_, public) = open(dir)?.agent_key()?;
    Ok((STANDARD.encode(&public), key_id(&public)))
}

/// Pin a recipient: its host descriptor (`chd1.…`) and one of its sessions.
pub fn pair_peer(dir: &Path, descriptor: &str, session: &str, label: &str) -> Result<String> {
    ensure!(
        valid_session_id(session),
        "session must be a lowercase UUID"
    );
    ensure!(
        !label.is_empty() && label.len() <= 64 && !label.chars().any(char::is_control),
        "label must be 1-64 printable characters"
    );
    let key = descriptor
        .strip_prefix(HOST_DESCRIPTOR_PREFIX)
        .context("not a host descriptor (expected `chd1.…` from `host credential descriptor`)")?;
    let key = B64.decode(key).context("descriptor is not base64url")?;
    ensure!(key.len() == 32, "descriptor key must be 32 bytes");
    let host_id = key_id(&key);
    open(dir)?.target_add(session, &host_id, &key, label)?;
    Ok(host_id)
}

pub struct Prepared {
    pub message_id: String,
    pub host_id: String,
    pub host_key: Vec<u8>,
    pub kid: String,
    pub payload: Vec<u8>,
    pub sig: Vec<u8>,
}

/// Deterministic operation id (`jti`) for one attempt of a logical send. The
/// message id is [`agent_message_id`] of this and the signing key.
fn operation_id(source: &str, target: &str, idem: &str, attempt: i64) -> String {
    let mut input = b"conversations-agent-message/v2\n".to_vec();
    for part in [source, target, idem, &attempt.to_string()] {
        input.extend_from_slice(&(part.len() as u32).to_be_bytes());
        input.extend_from_slice(part.as_bytes());
    }
    digest::digest(&digest::SHA256, &input)
        .as_ref()
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Digest of what the caller asked for, independent of any signing time.
fn request_digest(source: &str, target: &str, text: &str, reply_to: Option<&str>) -> Vec<u8> {
    let mut input = b"conversations-agent-request/v1\n".to_vec();
    for part in [source, target, text, reply_to.unwrap_or("\u{0}none")] {
        input.extend_from_slice(&(part.len() as u32).to_be_bytes());
        input.extend_from_slice(part.as_bytes());
    }
    digest::digest(&digest::SHA256, &input).as_ref().to_vec()
}

fn sign_op(
    store: &mut Store,
    host_id: &str,
    target: &str,
    op: &str,
    args: serde_json::Value,
    jti: &str,
    now: i64,
) -> Result<(String, Vec<u8>, Vec<u8>)> {
    let (pkcs8, public) = store.agent_key()?;
    let kid = key_id(&public);
    let payload = serde_json::to_vec(&serde_json::json!({
        "v": 1, "typ": "op", "aud": host_id, "kid": kid, "dlg": null,
        "op": op, "sess": target, "jti": jti,
        "iat": now, "exp": now + 60, "args": args,
    }))?;
    let pair = signature::Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| anyhow::anyhow!("agent key is unreadable"))?;
    let mut signed = DOMAIN_OP.to_vec();
    signed.extend_from_slice(&payload);
    let sig = pair.sign(&signed).as_ref().to_vec();
    Ok((kid, payload, sig))
}

/// A `peer.offer` for a private channel to `host_id`, signed with this host's
/// agent key: the same envelope and domain as every agent operation.
pub(super) fn sign_peer_offer(
    dir: &Path,
    host_id: &str,
    args: serde_json::Value,
) -> Result<(String, Vec<u8>, Vec<u8>)> {
    sign_op(
        &mut open(dir)?,
        host_id,
        authz::envelope::HOST_SCOPE,
        "peer.offer",
        args,
        &uuid::Uuid::new_v4().simple().to_string(),
        authz::store::now_secs(),
    )
}

/// States the recipient host vouches for once it has admitted a message.
const ADMITTED: [&str; 3] = ["queued", "received", "acknowledged"];

fn payload_field(payload: &[u8], pointer: &str) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()?
        .pointer(pointer)?
        .as_str()
        .map(str::to_owned)
}

/// Whether `reply` is a message `source` received from `target`, sent by the
/// host this machine pins for `target`. Only such a reply may acknowledge it.
fn answers(store: &Store, source: &str, target: &str, host_id: &str, reply: &str) -> Result<bool> {
    Ok(matches!(
        store.inbox_origin(source, reply)?,
        Some((from, Some(from_host))) if from == target && from_host == host_id
    ))
}

/// Record a state the recipient host vouched for in a signed answer and return
/// the send as stored. A reply whose admission is confirmed acknowledges, in the
/// same transaction, the message it answers in this host's own inbox: only
/// then, and only for the message's real sender.
fn record(store: &mut Store, row: &SendRow, state: &str, now: i64) -> Result<SendRow> {
    let reply = match payload_field(&row.payload, "/args/reply_to") {
        Some(reply) if answers(store, &row.source, &row.target, &row.host_id, &reply)? => {
            Some(reply)
        }
        _ => None,
    };
    store
        .send_settle(row, state, now, reply.as_deref())?
        .context("the send is no longer recorded")
}

/// How a stored send state reads to its sender.
fn shown(state: String) -> String {
    if state == "prepared" {
        "unconfirmed".into()
    } else {
        state
    }
}

/// A FRESH signed status request for the message `row` carries.
fn status_query(store: &mut Store, row: &SendRow, host_key: Vec<u8>, now: i64) -> Result<Prepared> {
    let jti = payload_field(&row.payload, "/jti").context("stored envelope has no operation id")?;
    let (kid, payload, sig) = sign_op(
        store,
        &row.host_id,
        &row.target,
        "agent.status",
        serde_json::json!({"session": row.target, "message": jti}),
        &uuid::Uuid::new_v4().simple().to_string(),
        now,
    )?;
    Ok(Prepared {
        message_id: row.message_id.clone(),
        host_id: row.host_id.clone(),
        host_key,
        kid,
        payload,
        sig,
    })
}

fn prepared_from(row: &SendRow, host_key: Vec<u8>) -> Prepared {
    Prepared {
        message_id: row.message_id.clone(),
        host_id: row.host_id.clone(),
        host_key,
        kid: row.kid.clone(),
        payload: row.payload.clone(),
        sig: row.sig.clone(),
    }
}

/// What a send should do next.
pub enum Begin {
    /// The recipient host already holds it: its signed state.
    Done(SendRow),
    /// Post exactly these stored bytes.
    Post(Prepared),
}

fn sign_attempt(
    store: &mut Store,
    row: &mut SendRow,
    host_key: &[u8],
    text: &str,
    reply_to: Option<&str>,
    now: i64,
) -> Result<Prepared> {
    let jti = operation_id(&row.source, &row.target, &row.idem, row.attempt);
    let source_host = store.host_id()?;
    let (kid, payload, sig) = sign_op(
        store,
        &row.host_id,
        &row.target,
        "agent.message",
        serde_json::json!({
            "session": row.target, "source": row.source, "source_host": source_host,
            "text": text, "reply_to": reply_to,
        }),
        &jti,
        now,
    )?;
    row.message_id = agent_message_id(&kid, &jti);
    (
        row.kid,
        row.payload,
        row.sig,
        row.exp,
        row.state,
        row.detail,
    ) = (kid, payload, sig, now + 60, "prepared".into(), None);
    store.send_put(row)?;
    Ok(prepared_from(row, host_key.to_vec()))
}

/// Decide what a send does. The FIRST signed envelope is persisted BEFORE any
/// network use, so a retry (later, after a restart, after a lost answer) reuses
/// the same bytes; a different body, reply, source or target under the same
/// idempotency key is refused.
pub fn begin_send(
    dir: &Path,
    source: &str,
    target: &str,
    text: &str,
    reply_to: Option<&str>,
    idem: &str,
    now: i64,
) -> Result<Begin> {
    ensure!(valid_session_id(source), "invalid source session");
    ensure!(valid_session_id(target), "invalid recipient session");
    ensure!(
        (1..=128).contains(&idem.len()),
        "idempotency_key must be 1-128 bytes"
    );
    ensure!(
        !text.trim().is_empty() && text.len() <= MAX_REPLY_TEXT_BYTES && !text.contains('\0'),
        "message text must be 1-{MAX_REPLY_TEXT_BYTES} bytes"
    );
    let mut store = open(dir)?;
    let (host_id, host_key, _) = store.target(target)?.with_context(|| {
        format!(
            "recipient {target} is not paired on this machine. Pair it with \
             `host credential pair-peer --host <descriptor> --session {target} --label <name>`; \
             discovery results are not trusted to choose a destination"
        )
    })?;
    let digest = request_digest(source, target, text, reply_to);
    if let Some(row) = store.send_get(source, target, idem)? {
        ensure!(
            row.request_sha256 == digest,
            "idempotency key already used for a different message"
        );
        ensure!(
            row.host_id == host_id,
            "the recipient pairing changed since this message was prepared; use a new idempotency key"
        );
        return Ok(match row.state.as_str() {
            "prepared" if now <= row.exp => Begin::Post(prepared_from(&row, host_key)),
            "prepared" => bail!(
                "delivery unknown for message {}; the signed request expired. Use messages_status with this message_id before deciding whether to send a new message",
                row.message_id
            ),
            _ => Begin::Done(row),
        });
    }
    if let Some(reply) = reply_to {
        ensure!(
            answers(&store, source, target, &host_id, reply)?,
            "reply_to must be a message this session received from {target}, sent by the host paired for it"
        );
    }
    let mut row = SendRow {
        source: source.into(),
        target: target.into(),
        idem: idem.into(),
        request_sha256: digest,
        attempt: 1,
        message_id: String::new(),
        payload: Vec::new(),
        sig: Vec::new(),
        kid: String::new(),
        host_id,
        exp: 0,
        state: "prepared".into(),
        detail: None,
        checked_at: None,
    };
    let prepared = sign_attempt(&mut store, &mut row, &host_key, text, reply_to, now)?;
    Ok(Begin::Post(prepared))
}

/// Record that the recipient host admitted attempt `message_id` (its signed
/// answer) and return the send's state as now stored.
pub fn mark_queued(
    dir: &Path,
    source: &str,
    target: &str,
    idem: &str,
    message_id: &str,
    now: i64,
) -> Result<String> {
    let mut store = open(dir)?;
    let mut row = store
        .send_get(source, target, idem)?
        .context("the send is no longer recorded")?;
    row.message_id = message_id.to_owned();
    Ok(shown(record(&mut store, &row, "queued", now)?.state))
}

/// What a status request for message `id` of `session` should consult.
pub enum Lookup {
    /// A message `session` received: this host's own inbox record.
    Received { state: String, source: String },
    /// A message `session` sent: ask its recipient host with `query`, which is
    /// `None` when that host is no longer the one paired for the recipient.
    Sent {
        row: Box<SendRow>,
        query: Option<Prepared>,
    },
}

pub fn lookup(dir: &Path, session: &str, id: &str, now: i64) -> Result<Option<Lookup>> {
    let mut store = open(dir)?;
    if let Some(row) = store.send_by_id(session, id)? {
        let query = match store.target(&row.target)? {
            Some((host_id, host_key, _)) if host_id == row.host_id => {
                Some(status_query(&mut store, &row, host_key, now)?)
            }
            _ => None,
        };
        return Ok(Some(Lookup::Sent {
            row: Box::new(row),
            query,
        }));
    }
    Ok(store
        .inbox_received(session, id)?
        .map(|(state, source)| Lookup::Received { state, source }))
}

/// A sent message's state: fresh from the recipient host's signed answer, or
/// the last one it vouched for with the reason no fresh answer was had.
pub struct SentStatus {
    pub state: String,
    pub checked_at: Option<i64>,
    pub unavailable: Option<String>,
}

/// Apply the answer to a [`Lookup::Sent`] query. `answer` is what the private
/// channel accepted (pinned host key, exactly this query's bytes), or why there
/// is none. The result is the state as stored afterwards, so a
/// slower answer to an earlier query reports what a later one established.
/// `absent` is reported but never stored: it does not undo an admission the
/// host signed earlier.
pub fn after_query(
    dir: &Path,
    row: SendRow,
    answer: Result<(String, Option<String>)>,
    now: i64,
) -> Result<SentStatus> {
    let reason = match answer {
        Ok((status, Some(state))) if status == "executed" => match state.as_str() {
            "absent" => {
                return Ok(SentStatus {
                    state,
                    checked_at: Some(now),
                    unavailable: None,
                })
            }
            known if ADMITTED.contains(&known) || known == "revoked" || known == "expired" => {
                let stored = record(&mut open(dir)?, &row, known, now)?;
                return Ok(SentStatus {
                    state: shown(stored.state),
                    checked_at: stored.checked_at,
                    unavailable: None,
                });
            }
            other => format!("unrecognized recipient state `{other}`"),
        },
        Ok((status, detail)) => format!(
            "the recipient host answered `{status}`{}",
            detail.map(|d| format!(": {d}")).unwrap_or_default()
        ),
        Err(error) => format!("{error:#}"),
    };
    Ok(SentStatus {
        state: shown(row.state),
        checked_at: row.checked_at,
        unavailable: Some(reason),
    })
}

/// Whether `source` is a session registered with the local host right now: the
/// managed registration, not a label the caller supplies.
pub async fn source_registered(dir: &Path, source: &str) -> Result<bool> {
    Ok(registered_agent(dir, source).await?.is_some())
}

/// The agent kind of a session registered with the local host, from its live
/// registration (`None` when it is not running here).
pub async fn registered_agent(
    dir: &Path,
    session: &str,
) -> Result<Option<conversations_core::AgentKind>> {
    Ok(registered_session(dir, session)
        .await?
        .map(|s| s.registration.launch.agent))
}

/// Resolve registration and native identity together through the owner socket.
pub async fn registered_session(
    dir: &Path,
    session: &str,
) -> Result<Option<super::protocol::SessionStatus>> {
    use super::protocol::{ClientMessage, ServerMessage};
    match super::request(dir, ClientMessage::Status)
        .await
        .context("the local Conversations host is not reachable")?
    {
        ServerMessage::Status { sessions, .. } => Ok(sessions
            .into_iter()
            .find(|s| s.registration.launch.id == session)),
        ServerMessage::Error { message } => bail!("{message}"),
        _ => bail!("unexpected answer from the local host"),
    }
}

/// Ask the local host (over its owner-only socket) to start a handoff child.
pub async fn local_handoff(
    dir: &Path,
    source: &str,
    idem: &str,
    brief: &str,
    agent: Option<conversations_core::AgentKind>,
    model: Option<String>,
    variant: Option<String>,
) -> Result<serde_json::Value> {
    use super::protocol::{ClientMessage, ServerMessage};
    let reply = super::request(
        dir,
        ClientMessage::Handoff {
            source: source.into(),
            idempotency_key: idem.into(),
            brief: brief.into(),
            agent,
            model: model.clone(),
            variant: variant.clone(),
        },
    )
    .await?;
    match reply {
        ServerMessage::HandoffResult {
            session_id,
            state,
            error,
        } => Ok(serde_json::json!({
            "session_id": session_id,
            "source_session_id": source,
            "status": state,
            "error": error,
            "dispatched": true,
            "model": model,
            "variant": variant,
        })),
        ServerMessage::Error { message } => bail!("{message}"),
        _ => bail!("unexpected answer from the local host"),
    }
}

pub fn iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}
