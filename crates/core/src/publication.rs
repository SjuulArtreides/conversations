//! Wire types for explicit publication of a locally canonical session.
//!
//! A host publishes a shared projection of a session it owns. Every write is a
//! host-signed operation: the server verifies the exact payload bytes against
//! the presented Ed25519 key, derives the host id from that key and checks it
//! against both the payload and the publication ledger. The authenticated
//! account must equal the payload's `owner`, so neither an account cookie alone
//! nor a replay of a signed packet under another account can publish, replace or
//! delete a managed session.
//!
//! Ordering is `(rev, seq)`. A `share` at a strictly newer revision (re)starts
//! the projection; `append` adds chunk `seq` of that revision; `unshare` at a
//! strictly newer revision deletes the projection and leaves a tombstone that
//! rejects anything at or below it. A chunk is bound to the operation by its
//! SHA-256, so the same `(rev, seq)` with different bytes is a conflict.
//!
//! The first accepted operation claims the id for its host and owner. Only a
//! `share` may be first; an `unshare` for a never-shared id is acknowledged as
//! deleted but records nothing, so it cannot claim the id.
//!
//! Publication is not execution authority and grants nothing on the host. The
//! read audience is enforced by the ordinary central account ACL.

use serde::{Deserialize, Serialize};

use crate::{AgentKind, Message};

/// Domain separation for publication signatures (distinct from operation and
/// host-statement signatures).
pub const PUBLICATION_DOMAIN: &[u8] = b"conversations-publication/v1\n";
/// Protocol version carried in every operation.
pub const PUBLICATION_VERSION: u8 = 1;
/// Largest signed operation payload.
pub const MAX_OP_BYTES: usize = 16 * 1024;
/// Largest chunk (decoded). A transcript is sent as many bounded chunks.
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
/// Most messages in one chunk.
pub const MAX_CHUNK_MESSAGES: usize = 500;
/// Most explicit recipients in a restricted audience.
pub const MAX_AUDIENCE_EMAILS: usize = 200;
/// Longest published title, in characters.
pub const MAX_TITLE_CHARS: usize = 300;

/// One publication request as posted to the server. All fields are base64url
/// without padding. `chunk` is present exactly when the operation is `append`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationRequest {
    /// The host's Ed25519 public key (32 bytes).
    pub host_key: String,
    /// The exact signed payload bytes: a JSON [`PublicationOp`].
    pub op: String,
    /// Signature over `PUBLICATION_DOMAIN || op bytes`.
    pub sig: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<String>,
}

/// The signed payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationOp {
    pub v: u8,
    /// `base64url(SHA-256(host public key))`.
    pub host: String,
    /// Session UUID.
    pub session: String,
    /// The central account this publication belongs to (`None` only when the
    /// server runs without auth).
    pub owner: Option<String>,
    /// Local share revision; strictly increasing per session.
    pub rev: u64,
    pub action: PublicationAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PublicationAction {
    /// Start (or restart) the projection at `rev` with this audience. Content
    /// is then sent with `append` from sequence 0.
    Share {
        audience: Audience,
        agent: AgentKind,
        title: String,
    },
    /// Chunk `seq` of revision `rev`; the chunk bytes hash to `chunk_sha256`
    /// (base64url).
    Append { seq: u64, chunk_sha256: String },
    /// Delete the projection and keep a tombstone at `rev`. A struct variant
    /// so that unknown fields are rejected like every other action.
    Unshare {},
}

/// Who may read the projection centrally. Organization visibility includes
/// future members; restricted lists explicit accounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "visibility", rename_all = "snake_case", deny_unknown_fields)]
pub enum Audience {
    Organization {},
    Restricted {
        emails: Vec<String>,
        /// Emails intended to type into the session through the publishing
        /// host once their key is trusted there. They read the published
        /// copy like any other audience member until then: typing stays
        /// blocked. Must be a subset
        /// of `emails`; enforced where an audience is normalized.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        write_pending: Vec<String>,
    },
}

/// The decoded chunk bytes: a JSON object of ordered messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationChunk {
    pub messages: Vec<Message>,
}

/// The exact bytes a host signs for `op_bytes`.
pub fn signed_message(op_bytes: &[u8]) -> Vec<u8> {
    let mut message = PUBLICATION_DOMAIN.to_vec();
    message.extend_from_slice(op_bytes);
    message
}

/// State of a publication after an accepted operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    Active,
    Deleted,
}

/// The server's answer to an accepted operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationAck {
    pub session: String,
    pub rev: u64,
    pub state: PublicationState,
    /// Next expected chunk sequence for `rev`.
    pub next_seq: u64,
    /// `true` when the identical operation had already been applied.
    pub replayed: bool,
}

/// The publication state a derived job (summary, chat reply, PR cache) read
/// its source context at: the active share revision and its chunk cursor. The
/// job's result is accepted only while the ledger still holds exactly this
/// pair, so neither an unshare/re-share nor a newer chunk of the same revision
/// (which may update a message in place) can be overtaken by an older result.
///
/// A read-side snapshot only: it authorizes nothing and is never resolved to
/// the current state at write time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationSnapshot {
    pub rev: u64,
    pub next_seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_audience_write_pending_defaults_empty_and_roundtrips() {
        // An old host's share payload has no write_pending: it must still parse.
        let old: Audience =
            serde_json::from_str(r#"{"visibility":"restricted","emails":["a@x.com"]}"#).unwrap();
        assert_eq!(
            old,
            Audience::Restricted {
                emails: vec!["a@x.com".to_string()],
                write_pending: Vec::new(),
            }
        );
        let full: Audience = serde_json::from_str(
            r#"{"visibility":"restricted","emails":["a@x.com"],"write_pending":["a@x.com"]}"#,
        )
        .unwrap();
        assert_eq!(
            full,
            Audience::Restricted {
                emails: vec!["a@x.com".to_string()],
                write_pending: vec!["a@x.com".to_string()],
            }
        );
        // Empty write_pending serializes back to the legacy shape.
        assert!(!serde_json::to_string(&old)
            .unwrap()
            .contains("write_pending"));
    }

    #[test]
    fn operations_reject_unknown_and_duplicate_fields() {
        let ok =
            r#"{"v":1,"host":"h","session":"s","owner":null,"rev":1,"action":{"type":"unshare"}}"#;
        assert!(serde_json::from_str::<PublicationOp>(ok).is_ok());
        for body in [
            r#"{"v":1,"host":"h","session":"s","owner":null,"rev":1,"action":{"type":"unshare"},"x":1}"#,
            r#"{"v":1,"host":"h","host":"g","session":"s","owner":null,"rev":1,"action":{"type":"unshare"}}"#,
            r#"{"v":1,"host":"h","session":"s","owner":null,"rev":1,"action":{"type":"unshare","seq":1}}"#,
            r#"{"v":1,"host":"h","session":"s","owner":null,"rev":1,"action":{"type":"append","seq":0,"chunk_sha256":"a","chunk_sha256":"b"}}"#,
            r#"{"v":1,"host":"h","session":"s","owner":null,"rev":1,"action":{"type":"share","agent":"claude","title":"t","audience":{"visibility":"organization","emails":[]}}}"#,
        ] {
            assert!(
                serde_json::from_str::<PublicationOp>(body).is_err(),
                "{body}"
            );
        }
    }
}
