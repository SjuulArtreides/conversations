//! `POST /api/publications`: host-signed publication of a shared session.
//!
//! The account session only identifies the caller; it is never enough on its
//! own. Each request carries a payload signed by the publishing host's Ed25519
//! key. The server verifies the signature over the exact bytes, derives the
//! host id from the presented key, and requires the signed `owner` to be the
//! authenticated account. The store then checks the ledger (same host, same
//! owner, revision and cursor order) in one transaction.
//!
//! This is the central half of Phase 4 only. It is not yet driven by the CLI,
//! and legacy upload routes still exist for unmanaged sessions.

use axum::body::Bytes;
use axum::extract::State;
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use conversations_core::publication::{
    signed_message, Audience, PublicationAck, PublicationAction, PublicationChunk, PublicationOp,
    PublicationRequest, PublicationState, MAX_AUDIENCE_EMAILS, MAX_CHUNK_BYTES, MAX_CHUNK_MESSAGES,
    MAX_OP_BYTES, MAX_TITLE_CHARS, PUBLICATION_VERSION,
};

use crate::error::{AppError, Result};
use crate::identity::CurrentUser;
use crate::state::AppState;
use crate::store::publication::{MessageRefs, PublicationWrite, VerifiedChunk};

/// Largest request body: a maximal chunk in base64 plus the operation.
pub const MAX_REQUEST_BYTES: usize = MAX_CHUNK_BYTES / 3 * 4 + 4 + MAX_OP_BYTES * 2 + 4096;

fn bad(message: &str) -> AppError {
    AppError::BadRequest(message.into())
}

fn sha256_b64(bytes: &[u8]) -> String {
    B64.encode(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

fn decode(value: &str, what: &str, max: usize) -> Result<Vec<u8>> {
    if value.len() > max / 3 * 4 + 4 {
        return Err(AppError::PayloadTooLarge(format!("{what} is too large")));
    }
    B64.decode(value)
        .map_err(|_| AppError::BadRequest(format!("{what} is not base64url")))
}

/// Verify a request into a write the store may apply.
fn verify(request: PublicationRequest, viewer: Option<&str>) -> Result<PublicationWrite> {
    let key = decode(&request.host_key, "host_key", 32)?;
    let op_bytes = decode(&request.op, "op", MAX_OP_BYTES)?;
    let sig = decode(&request.sig, "sig", 64)?;
    if key.len() != 32 || op_bytes.len() > MAX_OP_BYTES {
        return Err(bad("invalid host key or operation"));
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
        .verify(&signed_message(&op_bytes), &sig)
        .map_err(|_| AppError::Forbidden("publication signature is invalid".into()))?;
    let op: PublicationOp =
        serde_json::from_slice(&op_bytes).map_err(|_| bad("malformed publication operation"))?;
    if op.v != PUBLICATION_VERSION {
        return Err(bad("unsupported publication version"));
    }
    if op.host != sha256_b64(&key) {
        return Err(AppError::Forbidden(
            "the operation names a different host than its key".into(),
        ));
    }
    if op.owner.as_deref() != viewer {
        return Err(AppError::Forbidden(
            "the publication belongs to a different account".into(),
        ));
    }
    let canonical = uuid::Uuid::parse_str(&op.session)
        .map(|u| u.hyphenated().to_string() == op.session)
        .unwrap_or(false);
    if !canonical {
        return Err(bad("session must be a lowercase hyphenated UUID"));
    }
    if op.rev == 0 || op.rev > i64::MAX as u64 {
        return Err(bad("revision out of range"));
    }

    let chunk = match &op.action {
        PublicationAction::Share {
            audience, title, ..
        } => {
            if title.trim().is_empty()
                || title.chars().count() > MAX_TITLE_CHARS
                || title.contains('\0')
            {
                return Err(bad("invalid title"));
            }
            if let Audience::Restricted {
                emails,
                write_pending,
            } = audience
            {
                if emails.len() > MAX_AUDIENCE_EMAILS {
                    return Err(bad("too many recipients"));
                }
                if write_pending.len() > MAX_AUDIENCE_EMAILS {
                    return Err(bad("too many pending writers"));
                }
                for pending in write_pending {
                    if !emails.iter().any(|r| r.eq_ignore_ascii_case(pending)) {
                        return Err(bad("write intent outside the audience"));
                    }
                }
            }
            None
        }
        PublicationAction::Append { seq, chunk_sha256 } => {
            // The cursor after this chunk (seq + 1) must also fit the column.
            if *seq >= i64::MAX as u64 {
                return Err(bad("sequence out of range"));
            }
            let encoded = request
                .chunk
                .as_deref()
                .ok_or_else(|| bad("append requires a chunk"))?;
            let bytes = decode(encoded, "chunk", MAX_CHUNK_BYTES)?;
            if bytes.len() > MAX_CHUNK_BYTES {
                return Err(AppError::PayloadTooLarge("chunk is too large".into()));
            }
            let sha256 = sha256_b64(&bytes);
            if &sha256 != chunk_sha256 {
                return Err(bad("chunk does not match its signed digest"));
            }
            let chunk: PublicationChunk =
                serde_json::from_slice(&bytes).map_err(|_| bad("malformed chunk"))?;
            if chunk.messages.is_empty() || chunk.messages.len() > MAX_CHUNK_MESSAGES {
                return Err(bad("a chunk carries 1 to 500 messages"));
            }
            if chunk.messages.iter().any(|m| m.uuid.is_empty()) {
                return Err(bad("every message needs a uuid"));
            }
            Some(derive(chunk, sha256))
        }
        PublicationAction::Unshare {} => None,
    };
    if chunk.is_none() && request.chunk.is_some() {
        return Err(bad("only append carries a chunk"));
    }
    Ok(PublicationWrite {
        op_sha256: sha256_b64(&op_bytes),
        host_public_key: request.host_key,
        op,
        chunk,
    })
}

/// The PR and Linear references of each message in a chunk, computed before
/// the store transaction so they are written, replaced (when a message is
/// updated) and deleted with the content. Only full URLs are detected: bare
/// `PR #N` needs local repo context the central server does not have.
fn derive(chunk: PublicationChunk, sha256: String) -> VerifiedChunk {
    let refs = chunk
        .messages
        .iter()
        .map(|message| {
            let mut prs: Vec<crate::store::SessionPr> = Vec::new();
            for text in crate::api::pr_message_texts(message) {
                for pr in crate::github::detect_pr_refs(&text, None, &[]) {
                    if !prs.iter().any(|p| p.url == pr.url) {
                        prs.push(pr);
                    }
                }
            }
            MessageRefs {
                prs,
                linear_docs: crate::linear::detect_linear_refs(&message.text),
            }
        })
        .collect();
    VerifiedChunk {
        chunk,
        sha256,
        refs,
    }
}

pub async fn publish(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    body: Bytes,
) -> Result<Json<PublicationAck>> {
    let request: PublicationRequest =
        serde_json::from_slice(&body).map_err(|_| bad("malformed publication request"))?;
    let write = verify(request, viewer.as_deref())?;
    let ack = state.repo.apply_publication(&write).await?;
    if ack.state == PublicationState::Deleted {
        // Live data cannot exist for a managed id (the store refuses it), but
        // drop any in-memory hub for the id before acknowledging deletion.
        state.live.drop_hub(&ack.session);
    }
    Ok(Json(ack))
}
