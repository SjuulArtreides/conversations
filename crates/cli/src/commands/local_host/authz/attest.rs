//! Host-signed statements for the browser relay.
//!
//! The relay is untrusted, so anything it returns must be authenticated by the
//! host key the browser pinned when the user paired. Statements are the exact
//! payload bytes plus an Ed25519 signature over `HOST_DOMAIN || payload`; the
//! browser verifies the signature and then compares every field with what it
//! asked for. Payload types are strict structs with fixed field sets.

use std::path::Path;

use anyhow::{ensure, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use conversations_core::host_relay::{Signed, HOST_DOMAIN};
use ring::digest;
use serde::Serialize;

use super::store::Store;

/// Descriptor prefix: `chd1.` + base64url(host identity public key).
pub const DESCRIPTOR_PREFIX: &str = "chd1.";

/// Longest text the host puts in a signed outcome.
const MAX_DETAIL: usize = 160;

/// Sign `payload` (already serialized JSON bytes) with the host identity key.
pub fn sign_payload(store: &mut Store, payload: &[u8]) -> Result<Signed> {
    let mut message = HOST_DOMAIN.to_vec();
    message.extend_from_slice(payload);
    let signature = store.host_sign(&message)?;
    Ok(Signed {
        p: B64.encode(payload),
        s: B64.encode(signature),
    })
}

pub fn sign_value<T: Serialize>(store: &mut Store, value: &T) -> Result<Signed> {
    sign_payload(store, &serde_json::to_vec(value)?)
}

/// Proof of key possession for attaching to the relay: `(public key, signature)`,
/// both base64url, over `attach_message(host id, nonce)`. Only the public key and
/// a signature leave the machine.
pub fn attach_proof(dir: &Path, nonce: &str) -> Result<(String, String)> {
    super::prepare_dir(dir)?;
    let mut store = Store::open(dir)?;
    let host = store.host_id()?;
    let public = store.host_public_key()?;
    let message = conversations_core::host_relay::attach_message(&host, nonce);
    let signature = store.host_sign(&message)?;
    Ok((B64.encode(public), B64.encode(signature)))
}

/// What the user pastes (or follows) to pair: the host's public key. The id is
/// derived from it, so the browser can check they agree and pin both.
pub fn descriptor(dir: &Path) -> Result<String> {
    super::prepare_dir(dir)?;
    let public = Store::open(dir)?.host_public_key()?;
    Ok(format!("{DESCRIPTOR_PREFIX}{}", B64.encode(public)))
}

/// Domain-separated, length-delimited digest of what an authenticator returned
/// at registration: raw credential id, attestation object (which carries the
/// public key) and client data JSON. The browser computes the same value from its
/// own authenticator output before anything touches the network.
pub fn registration_digest(
    raw_id: &[u8],
    attestation_object: &[u8],
    client_data_json: &[u8],
) -> String {
    let mut message = b"conversations-reg/v1\n".to_vec();
    for part in [raw_id, attestation_object, client_data_json] {
        message.extend_from_slice(&(part.len() as u32).to_be_bytes());
        message.extend_from_slice(part);
    }
    B64.encode(digest::digest(&digest::SHA256, &message))
}

#[derive(Serialize)]
struct EnrollDone<'a> {
    v: u8,
    typ: &'static str,
    host: &'a str,
    nonce: &'a str,
    ceremony: &'a str,
    cred_id: &'a str,
    response_sha256: &'a str,
}

/// Completion of a registration: bound to this ceremony, the browser's nonce, the
/// id of the passkey the host stored and the digest of the response it verified.
pub fn enroll_done(
    dir: &Path,
    ceremony: &str,
    nonce: &str,
    cred_id: &str,
    response_sha256: &str,
) -> Result<Signed> {
    let mut store = Store::open(dir)?;
    let host = store.host_id()?;
    sign_value(
        &mut store,
        &EnrollDone {
            v: 1,
            typ: "enroll_done",
            host: &host,
            nonce,
            ceremony,
            cred_id,
            response_sha256,
        },
    )
}

#[derive(Serialize)]
struct OpResult<'a> {
    v: u8,
    typ: &'static str,
    host: &'a str,
    kid: &'a str,
    payload_sha256: String,
    status: &'a str,
    detail: Option<String>,
}

/// Outcome of one signed operation, bound to the SHA-256 of the exact operation
/// bytes so it cannot be replayed as the answer to another operation.
pub fn op_result(
    dir: &Path,
    kid: &str,
    payload: &[u8],
    status: &str,
    detail: Option<&str>,
) -> Result<Signed> {
    let mut store = Store::open(dir)?;
    let host = store.host_id()?;
    ensure!(
        detail.is_none_or(|d| d.chars().count() <= MAX_DETAIL),
        "detail too long"
    );
    sign_value(
        &mut store,
        &OpResult {
            v: 1,
            typ: "op_result",
            host: &host,
            kid,
            payload_sha256: B64.encode(digest::digest(&digest::SHA256, payload)),
            status,
            detail: detail.map(str::to_owned),
        },
    )
}
