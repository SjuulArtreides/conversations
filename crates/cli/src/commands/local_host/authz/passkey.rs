//! Passkey enrollment and 24-hour browser delegation, verified by the maintained
//! `webauthn-rs` relying-party library (no WebAuthn/CBOR parsing lives here).
//!
//! Division of authority:
//! - Permanent owner credentials normally require local approval. Hosted browser
//!   key approval is a separate, locally enabled ceremony that requires fresh
//!   passkey verification for each credential and checks authority at insertion.
//! - A passkey never signs operations. It authorizes a delegation: a temporary
//!   browser P-256 key (<= 24h). Operations are then signed by that key and
//!   admitted against the parent's CURRENT grants (see `store::admit`).
//!
//! Ceremony binding: `webauthn-rs` generates the challenge itself and offers no
//! way to supply one, so the challenge cannot be a hash of the request. Instead
//! each ceremony is one `challenges` row holding the library's opaque state and
//! the immutable request (`params`); finishing takes only the row id and the
//! authenticator response, consumes the row atomically before verifying, and
//! reads every parameter from the stored row. Nothing the caller sends at finish
//! time can change what is issued.

use std::{collections::BTreeMap, path::Path};

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use ring::digest;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Url, Webauthn, WebauthnBuilder,
};

use super::attest;
use super::envelope::{key_id, Alg};
use super::store::{holds_authority, validate_enrollment, Grant, Kind, Store};
use crate::commands::local_host::settings::Settings;
use conversations_core::host_relay::Signed;

/// An enrollment or delegation ceremony must complete within these windows.
pub const ENROLL_TTL_SECS: i64 = 300;
pub const DELEGATE_TTL_SECS: i64 = 120;
/// Browser delegations last at most this long, fixed at issuance.
pub const DELEGATION_SECS: i64 = 24 * 60 * 60;
/// Unfinished ceremonies a caller can leave open per kind.
const MAX_PENDING: i64 = 8;
const MAX_CAPS: usize = 16;

/// Proof that a request arrived on the host's owner-only Unix socket. It can only
/// be created from the accepted socket in `daemon::handle_client`, so no field of
/// any remote message can claim local trust.
pub struct LocalSocket(());

impl LocalSocket {
    pub(in crate::commands::local_host) fn from_unix_socket(
        _peer: &tokio::net::UnixStream,
    ) -> Self {
        Self(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum PasskeyRequest {
    /// Local only. Begin registering a passkey for `label` with these grants.
    EnrollStart {
        label: String,
        kind: String,
        grants: Vec<String>,
    },
    /// Complete a started registration with the authenticator's response.
    /// `vouch` is the optional cross-host proof recorded with the pending
    /// registration (see [`conversations_core::host_relay::VouchProof`]).
    EnrollFinish {
        challenge: String,
        response: Value,
        vouch: Option<conversations_core::host_relay::VouchProof>,
    },
    /// Local only. List registrations waiting for approval.
    Pending,
    /// Local only. Approve the actual credential (by its id) as a permanent key.
    Approve { cred_id: String },
    /// Ask for a 24-hour browser delegation. `browser_pub` is the standard-base64
    /// uncompressed P-256 point; `dlg` is the 128-bit hex id the browser chose.
    DelegateStart {
        browser_pub: String,
        dlg: String,
        caps: Vec<String>,
        /// 128-bit hex chosen by the browser, signed back in the offer.
        nonce: String,
    },
    /// Complete a delegation with the passkey assertion.
    DelegateFinish { challenge: String, response: Value },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PasskeyResponse {
    /// `options` is the WebAuthn options object for `navigator.credentials`.
    Started {
        challenge: String,
        options: Value,
    },
    Pending {
        pending: Vec<PendingCredential>,
    },
    /// The verified registration now waits for local approval.
    Registered {
        cred_id: String,
        /// Digest of the exact response that was verified and stored (see
        /// `attest::registration_digest`); the relay path signs it for the browser.
        response_digest: String,
    },
    Approved {
        cred_id: String,
    },
    /// A delegation ceremony was opened. `statement` is the host-signed offer
    /// binding the requested key, scope, id, nonce and challenge together.
    Offered {
        challenge: String,
        options: Value,
        statement: Signed,
    },
    Delegated {
        dlg: String,
        not_after: i64,
        statement: Signed,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingCredential {
    pub cred_id: String,
    pub label: String,
    pub kind: String,
    pub grants: Vec<String>,
    /// The host that vouched for this enrollment, when one did: its id and
    /// base64url public key. The pairwise trust anchor (EX-04/EX-05).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vouch_host_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vouch_host_key: Option<String>,
    /// True when an ALREADY-approved credential on this host was vouched by
    /// the same host: the pair was verified before, so this offer inherits
    /// that trust (EX-05).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trusted_host: bool,
}

/// A link that carries a locally started registration to the hosted UI. All
/// ceremony data is in the fragment, which browsers never send to a server. The
/// link grants nothing: the registration still needs local approval here.
pub fn enrollment_link(
    server: &str,
    descriptor: &str,
    challenge: &str,
    options: &Value,
) -> Result<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let options = URL_SAFE_NO_PAD.encode(serde_json::to_vec(options)?);
    Ok(format!(
        "{}/host-link#desc={descriptor}&challenge={challenge}&options={options}",
        server.trim_end_matches('/')
    ))
}

/// Local relying-party policy. Fails closed when unset or malformed.
pub fn relying_party(settings: &Settings) -> Result<(Webauthn, String)> {
    let config = &settings.webauthn;
    let rp_id = config
        .rp_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .context("passkeys are not configured: set webauthn.rp_id in the local host settings")?;
    ensure!(
        !config.origins.is_empty() && config.origins.len() <= 8,
        "set 1-8 webauthn.origins in the local host settings"
    );
    let origins = config
        .origins
        .iter()
        .map(|origin| {
            let url = Url::parse(origin).with_context(|| format!("invalid origin `{origin}`"))?;
            let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
            ensure!(
                url.scheme() == "https" || (url.scheme() == "http" && local),
                "origin `{origin}` must be https (or http on localhost)"
            );
            ensure!(
                url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
                "origin `{origin}` must be a bare origin"
            );
            Ok(url)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut builder = WebauthnBuilder::new(rp_id, &origins[0])
        .map_err(|error| anyhow::anyhow!("invalid relying party: {error}"))?
        .rp_name("Conversations");
    for origin in &origins[1..] {
        builder = builder.append_allowed_origin(origin);
    }
    let webauthn = builder
        .build()
        .map_err(|error| anyhow::anyhow!("invalid relying party: {error}"))?;
    Ok((webauthn, rp_id.to_owned()))
}

/// `base64url(SHA-256(credential id))`: the host's id for a passkey.
fn passkey_id(passkey: &Passkey) -> String {
    let digest = digest::digest(&digest::SHA256, passkey.cred_id().as_ref());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn hex_id() -> Result<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("no randomness"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Serialize, Deserialize)]
struct EnrollParams {
    label: String,
    kind: String,
    grants: Vec<String>,
    rp_id: String,
}

#[derive(Serialize, Deserialize)]
struct DelegateParams {
    host_id: String,
    nonce: String,
    browser_pub: String,
    browser_kid: String,
    dlg: String,
    caps: Vec<String>,
    issued_at: i64,
    not_after: i64,
}

impl Store {
    fn put_challenge(
        &mut self,
        id: &str,
        kind: &str,
        state: &str,
        params_json: &str,
        now: i64,
        ttl: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Housekeeping: ceremonies that can no longer be finished.
        tx.execute(
            "DELETE FROM challenges WHERE expires_at < ?1 OR consumed_at IS NOT NULL AND consumed_at < ?1",
            params![now.saturating_sub(86_400)],
        )?;
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM challenges WHERE kind = ?1 AND consumed_at IS NULL AND expires_at >= ?2",
            params![kind, now],
            |r| r.get(0),
        )?;
        ensure!(open < MAX_PENDING, "too many unfinished {kind} ceremonies");
        tx.execute(
            "INSERT INTO challenges(id, kind, state, params, created_at, expires_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, kind, state, params_json, now, now.saturating_add(ttl)],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically mark a ceremony used and return what was stored for it. The
    /// row is consumed whether or not verification later succeeds, so a failed
    /// response cannot be retried against the same challenge.
    fn take_challenge(&mut self, id: &str, kind: &str, now: i64) -> Result<(String, String)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, i64, Option<i64>)> = tx
            .query_row(
                "SELECT state, params, expires_at, consumed_at FROM challenges WHERE id = ?1 AND kind = ?2",
                params![id, kind],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((state, stored, expires_at, consumed)) = row else {
            bail!("unknown ceremony");
        };
        ensure!(consumed.is_none(), "ceremony was already used");
        tx.execute(
            "UPDATE challenges SET consumed_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        tx.commit()?;
        ensure!(now <= expires_at, "ceremony expired");
        Ok((state, stored))
    }

    fn approved_passkeys(&self, rp_id: &str) -> Result<Vec<(String, Passkey)>> {
        let mut statement = self.conn.prepare(
            "SELECT cred_id, webauthn FROM credentials WHERE auth = 'passkey' AND revoked_at IS NULL AND rp_id = ?1",
        )?;
        let rows = statement.query_map(params![rp_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, json) = row?;
            Ok((id, serde_json::from_str(&json).context("stored passkey")?))
        })
        .collect()
    }
}

pub fn enroll_start(
    store: &mut Store,
    webauthn: &Webauthn,
    rp_id: &str,
    label: &str,
    kind: &str,
    grants: &[String],
    now: i64,
) -> Result<PasskeyResponse> {
    let kind_parsed = Kind::parse(kind).context("kind must be `owner` or `agent`")?;
    let parsed = grants
        .iter()
        .map(|grant| Grant::parse(grant))
        .collect::<Result<Vec<_>>>()?;
    validate_enrollment(kind_parsed, label, &parsed)?;
    let existing: Vec<_> = store
        .approved_passkeys(rp_id)?
        .into_iter()
        .map(|(_, passkey)| passkey.cred_id().clone())
        .collect();
    let (options, state) = webauthn
        .start_passkey_registration(
            uuid::Uuid::new_v4(),
            label,
            label,
            (!existing.is_empty()).then_some(existing),
        )
        .map_err(|error| anyhow::anyhow!("cannot start registration: {error}"))?;
    let request = EnrollParams {
        label: label.to_owned(),
        kind: kind.to_owned(),
        grants: grants.to_vec(),
        rp_id: rp_id.to_owned(),
    };
    let challenge = hex_id()?;
    store.put_challenge(
        &challenge,
        "enroll",
        &serde_json::to_string(&state)?,
        &serde_json::to_string(&request)?,
        now,
        ENROLL_TTL_SECS,
    )?;
    Ok(PasskeyResponse::Started {
        challenge,
        options: serde_json::to_value(options)?,
    })
}

pub fn enroll_finish(
    store: &mut Store,
    webauthn: &Webauthn,
    challenge: &str,
    response: &Value,
    vouch: Option<conversations_core::host_relay::VouchProof>,
    now: i64,
) -> Result<PasskeyResponse> {
    let host_id = store.host_id()?;
    // An optional cross-host vouch: the person completing this ceremony speaks
    // for another host. Its signature is verified BEFORE the ceremony is
    // consumed, so a bad proof burns nothing (the attacker cannot kill an
    // invite with garbage); the passkey id it names is checked AFTER the
    // registration is verified, so a replayed vouch cannot be paired with a
    // different key (EX-04, PRI-002).
    let vouch = vouch
        .map(|proof| verify_vouch_proof(proof, &host_id, challenge))
        .transpose()?;
    let (state, stored) = store.take_challenge(challenge, "enroll", now)?;
    let state: PasskeyRegistration = serde_json::from_str(&state).context("stored ceremony")?;
    let request: EnrollParams = serde_json::from_str(&stored).context("stored request")?;
    let response: RegisterPublicKeyCredential =
        serde_json::from_value(response.clone()).context("malformed registration response")?;
    let passkey = webauthn
        .finish_passkey_registration(&response, &state)
        .map_err(|error| anyhow::anyhow!("registration was refused: {error}"))?;
    let cred_id = passkey_id(&passkey);
    if let Some((_, _, vouch_cred_id)) = &vouch {
        ensure!(
            vouch_cred_id == &cred_id,
            "the vouch names a different key than this enrollment produced"
        );
    }
    let response_digest = super::attest::registration_digest(
        &response.raw_id,
        &response.response.attestation_object,
        &response.response.client_data_json,
    );
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let known: i64 = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM credentials WHERE cred_id = ?1) + (SELECT COUNT(*) FROM pending_credentials WHERE cred_id = ?1)",
        params![cred_id],
        |r| r.get(0),
    )?;
    ensure!(known == 0, "this passkey is already registered");
    let vouch = vouch.map(|(id, key, _)| (id, key));
    tx.execute(
        "INSERT INTO pending_credentials(cred_id, kind, label, grants, rp_id, passkey, created_at, vouch_host_id, vouch_host_key)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            cred_id,
            request.kind,
            request.label,
            request.grants.join(","),
            request.rp_id,
            serde_json::to_string(&passkey)?,
            now,
            vouch.as_ref().map(|(id, _)| id),
            vouch.as_ref().map(|(_, key)| key),
        ],
    )?;
    tx.commit()?;
    Ok(PasskeyResponse::Registered {
        cred_id,
        response_digest,
    })
}

/// Verify the host, ceremony and credential binding without consuming the
/// challenge. The caller checks the verified registration's actual credential
/// id before accepting the proof.
fn verify_vouch_proof(
    proof: conversations_core::host_relay::VouchProof,
    host_id: &str,
    challenge: &str,
) -> Result<(String, String, String)> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::signature::UnparsedPublicKey;
    let payload = URL_SAFE_NO_PAD
        .decode(&proof.statement.p)
        .context("vouch statement is not base64url")?;
    let vouch: conversations_core::host_relay::KeyVouch = serde_json::from_str(
        &String::from_utf8(payload.clone()).context("vouch statement is not UTF-8")?,
    )
    .context("vouch statement is malformed")?;
    ensure!(vouch.v == 1, "unsupported vouch version");
    ensure!(vouch.typ == "key_vouch", "not a key vouch statement");
    let public = URL_SAFE_NO_PAD
        .decode(&proof.host_key)
        .context("vouch host key is not base64url")?;
    ensure!(
        Alg::Ed25519.valid_public_key(&public),
        "vouch host key is not an Ed25519 point"
    );
    let signature = URL_SAFE_NO_PAD
        .decode(&proof.statement.s)
        .context("vouch signature is not base64url")?;
    let mut message = conversations_core::host_relay::HOST_DOMAIN.to_vec();
    message.extend_from_slice(&payload);
    UnparsedPublicKey::new(&ring::signature::ED25519, &public)
        .verify(&message, &signature)
        .map_err(|_| anyhow::anyhow!("vouch signature does not match the presented host key"))?;
    ensure!(
        vouch.for_host == host_id,
        "this vouch was made for another host"
    );
    ensure!(
        vouch.challenge == challenge,
        "this vouch was made for another ceremony"
    );
    let vouch_host_id = key_id(&public);
    Ok((vouch_host_id, proof.host_key, vouch.cred_id))
}

/// Sign only a well-formed, host/ceremony/credential-bound key vouch.
pub(in crate::commands::local_host) fn vouch(
    store: &mut Store,
    for_host: &str,
    challenge: &str,
    cred_id: &str,
) -> Result<Value> {
    ensure!(
        super::envelope::valid_key_id(for_host) && super::envelope::valid_key_id(cred_id),
        "Invalid host or credential id."
    );
    ensure!(
        challenge.len() == 32
            && challenge
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Invalid ceremony id."
    );
    let value = conversations_core::host_relay::KeyVouch {
        v: 1,
        typ: "key_vouch".into(),
        for_host: for_host.into(),
        challenge: challenge.into(),
        cred_id: cred_id.into(),
    };
    let statement = attest::sign_value(store, &value)?;
    let host_key =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(store.host_public_key()?);
    Ok(
        serde_json::json!({ "statement": { "p": statement.p, "s": statement.s }, "host_key": host_key }),
    )
}

pub fn pending(store: &Store) -> Result<PasskeyResponse> {
    let mut statement = store.conn.prepare(
        "SELECT p.cred_id, p.label, p.kind, p.grants, p.vouch_host_id, p.vouch_host_key,
                EXISTS(SELECT 1 FROM credentials c
                       WHERE c.vouch_host_id IS NOT NULL
                         AND c.vouch_host_id = p.vouch_host_id
                         AND c.revoked_at IS NULL) AS trusted_host
         FROM pending_credentials p ORDER BY p.created_at",
    )?;
    let rows = statement.query_map([], |r| {
        Ok(PendingCredential {
            cred_id: r.get(0)?,
            label: r.get(1)?,
            kind: r.get(2)?,
            grants: r
                .get::<_, String>(3)?
                .split(',')
                .map(str::to_owned)
                .collect(),
            vouch_host_id: r.get(4)?,
            vouch_host_key: r.get(5)?,
            trusted_host: r.get(6)?,
        })
    })?;
    Ok(PasskeyResponse::Pending {
        pending: rows.collect::<rusqlite::Result<_>>()?,
    })
}

pub fn approve(store: &mut Store, cred_id: &str, now: i64) -> Result<PasskeyResponse> {
    approve_pending(store, cred_id, now, false)
}

fn approve_pending(
    store: &mut Store,
    cred_id: &str,
    now: i64,
    channel: bool,
) -> Result<PasskeyResponse> {
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row: Option<(String, String, String, String, String)> = tx
        .query_row(
            "SELECT kind, label, grants, rp_id, passkey FROM pending_credentials WHERE cred_id = ?1",
            params![cred_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((kind, label, grants, rp_id, passkey)) = row else {
        bail!("no pending registration with that id");
    };
    let (vouch_host_id, vouch_host_key): (Option<String>, Option<String>) = tx
        .query_row(
            "SELECT vouch_host_id, vouch_host_key FROM pending_credentials WHERE cred_id = ?1",
            params![cred_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((None, None));
    let parsed = grants
        .split(',')
        .map(Grant::parse)
        .collect::<Result<Vec<_>>>()?;
    if channel {
        ensure!(
            Kind::parse(&kind) == Some(Kind::Agent),
            "a channel can only approve an agent key"
        );
        ensure!(
            parsed.iter().all(|grant| !grant.cap.owner_only()),
            "a channel cannot approve owner-only capabilities"
        );
    }
    validate_enrollment(Kind::parse(&kind).context("stored kind")?, &label, &parsed)?;
    let mut ceiling: Vec<&str> = parsed.iter().map(|g| g.cap.as_str()).collect();
    ceiling.sort_unstable();
    ceiling.dedup();
    tx.execute(
        "INSERT INTO credentials(cred_id, kind, alg, public_key, label, ceiling, approved_at, auth, webauthn, rp_id, vouch_host_id, vouch_host_key)
         VALUES(?1, ?2, 'es256', x'', ?3, ?4, ?5, 'passkey', ?6, ?7, ?8, ?9)",
        params![
            cred_id,
            kind,
            label,
            ceiling.join(","),
            now,
            passkey,
            rp_id,
            vouch_host_id,
            vouch_host_key
        ],
    )?;
    for grant in parsed.iter().filter(|g| !g.ceiling_only) {
        tx.execute(
            "INSERT OR IGNORE INTO grants(cred_id, session_id, cap, granted_at) VALUES(?1, ?2, ?3, ?4)",
            params![
                cred_id,
                grant.session.as_deref().unwrap_or(super::store::ANY_SESSION),
                grant.cap.as_str(),
                now
            ],
        )?;
    }
    tx.execute(
        "DELETE FROM pending_credentials WHERE cred_id = ?1",
        params![cred_id],
    )?;
    tx.commit()?;
    Ok(PasskeyResponse::Approved {
        cred_id: cred_id.to_owned(),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn delegate_start(
    store: &mut Store,
    webauthn: &Webauthn,
    rp_id: &str,
    browser_pub_b64: &str,
    dlg: &str,
    caps: &[String],
    nonce: &str,
    now: i64,
) -> Result<PasskeyResponse> {
    ensure!(is_hex32(nonce), "nonce must be 32 lowercase hex characters");
    // The offer signs the scope as a sorted, duplicate-free list.
    let mut caps = caps.to_vec();
    caps.sort();
    caps.dedup();
    let caps = caps.as_slice();
    let browser_pub = STANDARD
        .decode(browser_pub_b64)
        .context("browser key must be standard base64")?;
    ensure!(
        Alg::Es256.valid_public_key(&browser_pub),
        "browser key must be an uncompressed P-256 point"
    );
    ensure!(
        dlg.len() == 32
            && dlg
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "delegation id must be 32 lowercase hex characters"
    );
    ensure!(
        !caps.is_empty() && caps.len() <= MAX_CAPS,
        "request 1-{MAX_CAPS} capabilities"
    );
    for cap in caps {
        Grant::parse(cap)?;
    }
    let taken: i64 = store.conn.query_row(
        "SELECT COUNT(*) FROM delegations WHERE dlg_id = ?1",
        params![dlg],
        |r| r.get(0),
    )?;
    ensure!(taken == 0, "delegation id is already in use");
    let passkeys = store.approved_passkeys(rp_id)?;
    ensure!(
        !passkeys.is_empty(),
        "no locally approved passkey for this relying party"
    );
    let only: Vec<Passkey> = passkeys.into_iter().map(|(_, passkey)| passkey).collect();
    let (options, state) = webauthn
        .start_passkey_authentication(&only)
        .map_err(|error| anyhow::anyhow!("cannot start authentication: {error}"))?;
    let host_id = store.host_id()?;
    let not_after = now
        .checked_add(DELEGATION_SECS)
        .context("clock out of range")?;
    let options = serde_json::to_value(options)?;
    let webauthn_challenge = options["publicKey"]["challenge"]
        .as_str()
        .context("authenticator options carry no challenge")?
        .to_owned();
    let ceremony = hex_id()?;
    let request = DelegateParams {
        host_id: host_id.clone(),
        nonce: nonce.to_owned(),
        browser_kid: key_id(&browser_pub),
        browser_pub: browser_pub_b64.to_owned(),
        dlg: dlg.to_owned(),
        caps: caps.to_vec(),
        issued_at: now,
        not_after,
    };
    let statement = attest::sign_value(
        store,
        &DelegateOffer {
            v: 1,
            typ: "delegate_offer",
            host: &host_id,
            nonce,
            ceremony: &ceremony,
            browser_kid: &request.browser_kid,
            browser_pub: browser_pub_b64,
            dlg,
            caps,
            issued_at: now,
            not_after,
            webauthn_challenge: &webauthn_challenge,
            rp_id,
        },
    )?;
    store.put_challenge(
        &ceremony,
        "delegate",
        &serde_json::to_string(&state)?,
        &serde_json::to_string(&request)?,
        now,
        DELEGATE_TTL_SECS,
    )?;
    Ok(PasskeyResponse::Offered {
        challenge: ceremony,
        options,
        statement,
    })
}

fn is_hex32(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What the host commits to when it opens a delegation ceremony. The browser
/// verifies the signature with the pinned host key and compares every field
/// with the request it made before it lets a passkey sign the challenge.
#[derive(Serialize)]
struct DelegateOffer<'a> {
    v: u8,
    typ: &'static str,
    host: &'a str,
    nonce: &'a str,
    ceremony: &'a str,
    browser_kid: &'a str,
    browser_pub: &'a str,
    dlg: &'a str,
    caps: &'a [String],
    issued_at: i64,
    not_after: i64,
    webauthn_challenge: &'a str,
    rp_id: &'a str,
}

#[derive(Serialize)]
struct DelegateDone<'a> {
    v: u8,
    typ: &'static str,
    host: &'a str,
    nonce: &'a str,
    ceremony: &'a str,
    browser_kid: &'a str,
    dlg: &'a str,
    caps: &'a [String],
    not_after: i64,
}

pub fn delegate_finish(
    store: &mut Store,
    webauthn: &Webauthn,
    rp_id: &str,
    challenge: &str,
    response: &Value,
    now: i64,
) -> Result<PasskeyResponse> {
    let (state, stored) = store.take_challenge(challenge, "delegate", now)?;
    let state: PasskeyAuthentication = serde_json::from_str(&state).context("stored ceremony")?;
    let request: DelegateParams = serde_json::from_str(&stored).context("stored request")?;
    ensure!(
        request.host_id == store.host_id()?,
        "request was issued for another host"
    );
    let response: PublicKeyCredential =
        serde_json::from_value(response.clone()).context("malformed assertion")?;
    let result = webauthn
        .finish_passkey_authentication(&response, &state)
        .map_err(|error| anyhow::anyhow!("assertion was refused: {error}"))?;
    ensure!(result.user_verified(), "user verification is required");

    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Which approved passkey answered: look it up among the CURRENT approved set.
    let mut found = None;
    {
        let mut statement = tx.prepare(
            "SELECT cred_id, webauthn FROM credentials WHERE auth = 'passkey' AND revoked_at IS NULL AND rp_id = ?1",
        )?;
        let rows = statement.query_map(params![rp_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, json) = row?;
            let passkey: Passkey = serde_json::from_str(&json).context("stored passkey")?;
            if passkey.cred_id() == result.cred_id() {
                found = Some((id, passkey));
                break;
            }
        }
    }
    let (parent, mut passkey) = found.context("passkey is no longer approved")?;
    // Counters are recorded for diagnostics only; replay safety is the single-use
    // ceremony row, because synced passkeys legitimately report a zero counter.
    if passkey.update_credential(&result) == Some(true) {
        tx.execute(
            "UPDATE credentials SET webauthn = ?2 WHERE cred_id = ?1",
            params![parent, serde_json::to_string(&passkey)?],
        )?;
    }
    ensure!(now <= request.not_after, "request expired");
    // The delegation can never exceed what the parent holds RIGHT NOW. It is
    // the requested scope narrowed to that: session capabilities that are
    // scoped per session (write, sharing, history) are delegated as a CATEGORY
    // (the parent's ceiling) and every operation is still checked against the
    // parent's current grant for ITS session.
    let mut granted_caps: Vec<String> = Vec::new();
    let mut first_refusal = None;
    for cap in &request.caps {
        let grant = Grant::parse(cap)?;
        let denial =
            if grant.session.is_none() && !grant.cap.creates_sessions() && !grant.cap.host_scoped()
            {
                super::store::credential_allows(&tx, &parent, grant.cap, true)?.err()
            } else {
                holds_authority(&tx, &parent, grant.cap, grant.session.as_deref(), true)?
            };
        match denial {
            None => granted_caps.push(cap.clone()),
            Some(reason) => first_refusal = first_refusal.or(Some((cap.clone(), reason))),
        }
    }
    if granted_caps.is_empty() {
        let (cap, reason) = first_refusal.unwrap_or_default();
        bail!("requested scope `{cap}` exceeds the passkey's authority: {reason}");
    }
    let request = DelegateParams {
        caps: granted_caps,
        ..request
    };
    let browser_pub = STANDARD.decode(&request.browser_pub)?;
    tx.execute(
        "INSERT INTO delegations(dlg_id, parent_cred, browser_kid, browser_pub, alg, caps, issued_at, not_after)
         VALUES(?1, ?2, ?3, ?4, 'es256', ?5, ?6, ?7)",
        params![
            request.dlg,
            parent,
            request.browser_kid,
            browser_pub,
            request.caps.join(","),
            now,
            request.not_after
        ],
    )?;
    tx.commit()?;
    let statement = attest::sign_value(
        store,
        &DelegateDone {
            v: 1,
            typ: "delegate_done",
            host: &request.host_id,
            nonce: &request.nonce,
            ceremony: challenge,
            browser_kid: &request.browser_kid,
            dlg: &request.dlg,
            caps: &request.caps,
            not_after: request.not_after,
        },
    )?;
    Ok(PasskeyResponse::Delegated {
        dlg: request.dlg,
        not_after: request.not_after,
        statement,
    })
}

/// The single entry point the host daemon calls. `local` is `Some` only for a
/// request read from the owner-only Unix socket; local-only operations refuse
/// everything else. Blocking SQLite work: call from `spawn_blocking`.
pub fn handle(
    dir: &Path,
    settings: &Settings,
    request: PasskeyRequest,
    local: Option<&LocalSocket>,
    now: i64,
) -> Result<PasskeyResponse> {
    let (webauthn, rp_id) = relying_party(settings)?;
    let mut store = Store::open(dir)?;
    let require_local = || {
        local
            .map(|_| ())
            .context("this operation is only available on the host's local socket")
    };
    match request {
        PasskeyRequest::EnrollStart {
            label,
            kind,
            grants,
        } => {
            require_local()?;
            enroll_start(&mut store, &webauthn, &rp_id, &label, &kind, &grants, now)
        }
        PasskeyRequest::EnrollFinish {
            challenge,
            response,
            vouch,
        } => enroll_finish(&mut store, &webauthn, &challenge, &response, vouch, now),
        PasskeyRequest::Pending => {
            require_local()?;
            pending(&store)
        }
        PasskeyRequest::Approve { cred_id } => {
            require_local()?;
            approve(&mut store, &cred_id, now)
        }
        PasskeyRequest::DelegateStart {
            browser_pub,
            dlg,
            caps,
            nonce,
        } => delegate_start(
            &mut store,
            &webauthn,
            &rp_id,
            &browser_pub,
            &dlg,
            &caps,
            &nonce,
            now,
        ),
        PasskeyRequest::DelegateFinish {
            challenge,
            response,
        } => delegate_finish(&mut store, &webauthn, &rp_id, &challenge, &response, now),
    }
}

const BROWSER_TRUST: &str = "browser_trust";

#[derive(Serialize, Deserialize)]
struct BrowserTrustApproval {
    request: super::envelope::BrowserTrust,
    authority: Option<String>,
    signer: String,
    opt_in: i64,
    approver_epochs: BTreeMap<String, i64>,
    delegation: Option<String>,
}

fn browser_trust_grants(request: &super::envelope::BrowserTrust) -> Result<(Vec<u8>, Vec<Grant>)> {
    use super::envelope::Cap;
    let public = STANDARD.decode(&request.public_key)?;
    ensure!(
        request.compared && Alg::Es256.valid_public_key(&public) && key_id(&public) == request.kid,
        "compare the exact browser key fingerprint before approving"
    );
    ensure!(
        (1..=4).contains(&request.grants.len()),
        "choose 1–4 permissions"
    );
    let grants = request
        .grants
        .iter()
        .map(|name| {
            let cap = Cap::parse(name).context("unknown browser trust permission")?;
            ensure!(
                matches!(
                    cap,
                    Cap::HistoryRead | Cap::SessionStart | Cap::SessionReopen | Cap::SessionWrite
                ),
                "this permission requires local approval"
            );
            Grant::parse(name)
        })
        .collect::<Result<Vec<_>>>()?;
    validate_enrollment(Kind::Owner, &request.label, &grants)?;
    Ok((public, grants))
}

pub(in crate::commands::local_host) fn browser_trust(
    dir: &Path,
    settings: &Settings,
    op: &super::envelope::Verified,
) -> Result<Value> {
    use super::envelope::{Cap, Operation, HOST_SCOPE};
    use super::store::{current_grant, delegated_parent, same_authority};

    let (webauthn, rp_id) = relying_party(settings)?;
    let mut store = Store::open(dir)?;
    let now = super::store::now_secs();
    let parent = op
        .dlg
        .as_ref()
        .map(|dlg| {
            delegated_parent(
                &store.conn,
                &op.kid,
                Cap::PasskeyManage,
                HOST_SCOPE,
                dlg,
                now,
            )?
            .map_err(anyhow::Error::msg)
        })
        .transpose()?;
    let signer = parent.as_deref().unwrap_or(&op.kid);
    let opt_in = current_grant(
        &store.conn,
        signer,
        Cap::BrowserTrust,
        None,
        op.dlg.is_some(),
        None,
    )?
    .map_err(|_| anyhow::anyhow!("remote browser approvals are not enabled on this key"))?;
    match &op.op {
        Operation::BrowserTrustStart(request) => {
            browser_trust_grants(request)?;
            ensure!(store.still_admitted(op, now)?, "approval authority changed");
            let mut approvers = Vec::new();
            let mut approver_epochs = BTreeMap::new();
            for (kid, passkey) in store.approved_passkeys(&rp_id)? {
                if parent.as_ref().is_some_and(|parent| parent != &kid) {
                    continue;
                }
                if let Ok(epoch) =
                    current_grant(&store.conn, &kid, Cap::BrowserTrust, None, true, None)?
                {
                    approver_epochs.insert(kid, epoch);
                    approvers.push(passkey);
                }
            }
            ensure!(
                !approvers.is_empty(),
                "enable remote browser approvals on an approved passkey on this host"
            );
            let (options, state) = webauthn
                .start_passkey_authentication(&approvers)
                .map_err(|error| anyhow::anyhow!("cannot start approval verification: {error}"))?;
            let challenge = hex_id()?;
            let approval = BrowserTrustApproval {
                request: request.clone(),
                authority: op.admitted_under.clone(),
                signer: op.kid.clone(),
                opt_in,
                approver_epochs,
                delegation: op.dlg.clone(),
            };
            store.put_challenge(
                &challenge,
                BROWSER_TRUST,
                &serde_json::to_string(&state)?,
                &serde_json::to_string(&approval)?,
                now,
                DELEGATE_TTL_SECS,
            )?;
            Ok(serde_json::json!({ "challenge": challenge, "options": options }))
        }
        Operation::BrowserTrustFinish {
            challenge,
            response,
        } => {
            let (state, approval) = store.take_challenge(challenge, BROWSER_TRUST, now)?;
            let approval: BrowserTrustApproval = serde_json::from_str(&approval)?;
            ensure!(
                approval.signer == op.kid
                    && approval.delegation == op.dlg
                    && approval.authority == op.admitted_under
                    && approval.opt_in == opt_in,
                "approval belongs to a different browser or its authority changed"
            );
            let assertion: PublicKeyCredential = serde_json::from_value(response.clone())?;
            let result = webauthn
                .finish_passkey_authentication(&assertion, &serde_json::from_str(&state)?)
                .map_err(|error| anyhow::anyhow!("approval verification refused: {error}"))?;
            ensure!(result.user_verified(), "user verification is required");

            let approver = store
                .approved_passkeys(&rp_id)?
                .into_iter()
                .find(|(_, passkey)| passkey.cred_id() == result.cred_id())
                .map(|(kid, _)| kid)
                .context("approving passkey is no longer active")?;
            ensure!(
                parent.as_ref().is_none_or(|parent| parent == &approver),
                "approval must use the delegated passkey"
            );
            let (public, grants) = browser_trust_grants(&approval.request)?;
            let tx = store
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            ensure!(
                same_authority(&tx, op, super::store::now_secs())?,
                "approval authority changed"
            );
            ensure!(
                current_grant(
                    &tx,
                    signer,
                    Cap::BrowserTrust,
                    None,
                    op.dlg.is_some(),
                    Some(opt_in)
                )?
                .is_ok(),
                "remote browser approval permission changed"
            );
            let epoch = approval
                .approver_epochs
                .get(&approver)
                .copied()
                .context("passkey was not eligible when approval started")?;
            ensure!(
                current_grant(&tx, &approver, Cap::BrowserTrust, None, true, Some(epoch))?.is_ok(),
                "passkey approval permission changed"
            );
            for grant in &grants {
                ensure!(
                    holds_authority(&tx, &approver, grant.cap, None, true)?.is_none()
                        && holds_authority(&tx, signer, grant.cap, None, op.dlg.is_some())?
                            .is_none(),
                    "approver lacks host-wide permission for {}",
                    grant.cap.as_str()
                );
            }
            let kid = Store::insert_credential(
                &tx,
                Kind::Owner,
                Alg::Es256,
                &public,
                &approval.request.label,
                &grants,
            )?;
            tx.commit()?;
            Ok(serde_json::json!({ "kid": kid }))
        }
        _ => bail!("not a browser approval operation"),
    }
}

/// The owner side of the key-trust ceremony, over the signed private channel:
/// the owner's delegated browser as a remote control for what the local CLI
/// does. The authority proof is
/// the channel admission itself — `dispatch::decide` already checked the
/// operation against the owner-only, never-grantable `passkey.manage`
/// capability — so no transport claim is taken here. Only the daemon's
/// admitted channel path may call this.
///
/// Channel enrollments are always AGENT keys scoped to named sessions: the
/// delegation must not be able to mint a permanent OWNER credential (which
/// would outlive it and its revocation). The local CLI keeps its full
/// ceremony; the narrowing is the channel's, exactly as delegation caps are
/// narrowed to what the parent holds.
pub(in crate::commands::local_host) fn handle_channel(
    dir: &Path,
    settings: &Settings,
    request: PasskeyRequest,
    now: i64,
) -> Result<PasskeyResponse> {
    let (webauthn, rp_id) = relying_party(settings)?;
    let mut store = Store::open(dir)?;
    match request {
        PasskeyRequest::EnrollStart {
            label,
            kind,
            grants,
        } => {
            ensure!(
                Kind::parse(&kind) == Some(Kind::Agent),
                "a channel invitation can only enroll an agent key"
            );
            ensure!(
                grants.len() <= super::envelope::MAX_GRANT_CAPS,
                "at most {} grants per invitation",
                super::envelope::MAX_GRANT_CAPS
            );
            for grant in &grants {
                let parsed = Grant::parse(grant)?;
                ensure!(
                    !parsed.cap.owner_only(),
                    "a channel invitation cannot grant the owner-only `{}`",
                    parsed.cap.as_str()
                );
            }
            enroll_start(&mut store, &webauthn, &rp_id, &label, &kind, &grants, now)
        }
        PasskeyRequest::Pending => pending(&store),
        PasskeyRequest::Approve { cred_id } => approve_pending(&mut store, &cred_id, now, true),
        _ => bail!("this passkey operation is not available on the channel"),
    }
}
