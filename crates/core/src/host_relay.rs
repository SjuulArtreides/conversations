//! Wire types for the opaque browser -> server -> host relay.
//!
//! The server only carries these messages. It holds no key, approves no
//! credential and cannot make the host act: every request is checked by the
//! host against locally stored keys. The set of request types is closed and
//! deliberately lacks anything that starts a registration or approves a
//! credential; those stay on the host's local socket.
//!
//! Requests do carry signed operations and WebAuthn responses, and a signed
//! input operation can contain typed prompt text. The `signed_op` path is
//! plaintext, so it is a non-release intermediate: the server can read what
//! passes through it (it still cannot forge it). Replies are limited to ceremony
//! data and operation status. `peer_challenge`/`peer_offer` carry only the
//! signaling for the private channel (`crate::private_channel`); operations and
//! content on that channel never cross this relay.

use serde::{Deserialize, Serialize};

/// Largest request body the relay accepts (a WebAuthn response is a few KB).
pub const MAX_REQUEST_BYTES: usize = 32 * 1024;
/// Largest reply the relay forwards.
pub const MAX_REPLY_BYTES: usize = 32 * 1024;
/// Longest human-readable text in a reply.
pub const MAX_TEXT_CHARS: usize = 160;
/// How long the server waits for a host to answer one request.
pub const REQUEST_TIMEOUT_SECS: u64 = 25;

/// What a browser may ask a host. Closed set, strict fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelayRequest {
    /// Finish a registration that was started on the host itself.
    EnrollFinish {
        challenge: String,
        response: serde_json::Value,
        /// Fresh 128-bit hex chosen by the browser; the host signs it back in
        /// the completion statement so an answer to any other request is recognised.
        nonce: String,
        /// Optional proof that the person completing this ceremony speaks for
        /// another host: a statement signed by THAT host's key, binding this
        /// ceremony. The enrolling host verifies it locally and records it with
        /// the pending registration (the pairwise trust anchor).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vouch: Option<VouchProof>,
    },
    /// Ask for a 24-hour delegation of a browser-held key.
    DelegateStart {
        browser_pub: String,
        dlg: String,
        caps: Vec<String>,
        /// Fresh 128-bit hex chosen by the browser; the host signs it back so a
        /// reply to any other request is recognised.
        nonce: String,
    },
    DelegateFinish {
        challenge: String,
        response: serde_json::Value,
    },
    /// A signed operation, exactly as the browser produced it.
    SignedOp { p: String, s: String, k: String },
    /// Ask for a fresh one-use challenge for a private channel offer. `nonce`
    /// (128-bit hex, chosen by the browser) is signed back by the host.
    PeerChallenge { nonce: String },
    /// A private channel offer: a signed `peer.offer` envelope (`p`, `s`, `k`)
    /// binding the challenge, the SHA-256 of `sdp` and its DTLS fingerprint.
    PeerOffer {
        p: String,
        s: String,
        k: String,
        sdp: String,
    },
}

/// Largest SDP either side of a private channel handshake may send.
pub const MAX_SDP_BYTES: usize = 12 * 1024;

/// Outcome of a signed operation, as far as the host will say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    Executed,
    /// The identical operation was handled before; nothing was repeated.
    Replayed,
    Refused,
    Failed,
    /// An earlier attempt was interrupted; the host will not repeat it.
    Uncertain,
}

impl OpStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Executed => "executed",
            Self::Replayed => "replayed",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }
}

/// A host statement: the exact payload bytes plus the host's Ed25519 signature
/// over `HOST_DOMAIN || payload`. Both are base64url. The relay cannot alter
/// either without the browser rejecting it against the pinned host key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signed {
    pub p: String,
    pub s: String,
}

/// Domain separation for the proof a host gives to attach to the relay.
pub const ATTACH_DOMAIN: &[u8] = b"conversations-attach/v1\n";

/// The exact bytes a host signs to attach as `host_id` with a server `nonce`.
pub fn attach_message(host_id: &str, nonce: &str) -> Vec<u8> {
    let mut message = ATTACH_DOMAIN.to_vec();
    message.extend_from_slice(host_id.as_bytes());
    message.push(b'\n');
    message.extend_from_slice(nonce.as_bytes());
    message
}

/// Domain separation for host statements (distinct from operation signatures).
pub const HOST_DOMAIN: &[u8] = b"conversations-host/v1\n";

/// A vouch statement signed by ONE host's identity key, presented to a
/// DIFFERENT host (the enrolling host) together with the signer's public key.
/// It binds the signer to exactly one enrollment ceremony on that host AND
/// to the passkey id that ceremony is expected to produce (the id is derived
/// from the authenticator's own output before anything is submitted), so a
/// replayed vouch cannot be paired with a different registration. The
/// binding's authenticity is the humans' to compare (the pairwise code):
/// the statement itself carries no account
/// identity and grants nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyVouch {
    pub v: u8,
    /// Fixed: `key_vouch`.
    pub typ: String,
    /// The id of the host this vouch is FOR (the enrolling host).
    pub for_host: String,
    /// The enrollment ceremony id on that host.
    pub challenge: String,
    /// The passkey id this vouch speaks for: `base64url(SHA-256(raw
    /// credential id))`, derived by the browser from its authenticator's own
    /// output before the finish request is sent.
    pub cred_id: String,
}

/// A host-signed [`KeyVouch`] plus the signer host's base64url public key.
/// The statement and key are exact bytes: the enrolling host verifies the
/// signature against the key, the key's derived id, its own id and the
/// ceremony being finished — nothing from the server is believed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VouchProof {
    pub statement: Signed,
    pub host_key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelayReply {
    /// `statement` is the host-signed offer binding the browser's exact request
    /// to this ceremony; the browser must verify it BEFORE the passkey prompt.
    Started {
        challenge: String,
        options: serde_json::Value,
        statement: Signed,
    },
    /// `statement` is the host-signed `enroll_done`: it binds the ceremony, the
    /// browser's nonce, the passkey id the host stored and a digest of the exact
    /// registration response it verified. The browser compares all of it with the
    /// registration its own authenticator produced; nothing else is believed.
    Registered {
        statement: Signed,
    },
    Delegated {
        statement: Signed,
    },
    /// The outcome of one signed operation, signed by the host and bound to the
    /// hash of the exact operation bytes.
    Op {
        statement: Signed,
    },
    /// The answer to a signed `settings.get`: this host's launch flags, signed by
    /// the host and bound to the hash of the exact request bytes. Only safe flag
    /// fields are carried; paths, origins and credentials never are.
    Settings {
        statement: Signed,
    },
    /// `peer_challenge` statement: the browser's nonce, a fresh challenge and
    /// its expiry, signed by the host.
    PeerChallenge {
        statement: Signed,
    },
    /// The host's SDP answer and a `peer_answer` statement binding the signed
    /// offer, the challenge, the SHA-256 of this exact `sdp` and the host's DTLS
    /// fingerprint. The browser applies `sdp` only after verifying all of it.
    PeerAnswer {
        sdp: String,
        statement: Signed,
    },
    Error {
        message: String,
    },
}

/// One request as delivered to the host on its stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayEnvelope {
    pub id: String,
    pub request: RelayRequest,
}

/// The host's answer, posted back with the request's correlation id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAnswer {
    pub id: String,
    pub reply: RelayReply,
}

/// Host ids are `base64url(SHA-256(host public key))`.
pub fn valid_host_id(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Keep text short and printable before it crosses the server.
pub fn clean_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT_CHARS)
        .collect()
}

impl RelayReply {
    /// Bound and clean a reply received from a host.
    pub fn sanitized(self) -> Option<Self> {
        let cleaned = match self {
            RelayReply::Error { message } => RelayReply::Error {
                message: clean_text(&message),
            },
            other => other,
        };
        (serde_json::to_vec(&cleaned).ok()?.len() <= MAX_REPLY_BYTES).then_some(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_cannot_name_local_only_operations() {
        for body in [
            r#"{"type":"approve","cred_id":"x"}"#,
            r#"{"type":"enroll_start","label":"l","kind":"owner","grants":["session.start"]}"#,
            r#"{"type":"signed_op","p":"a","s":"b","k":"c","is_local":true}"#,
        ] {
            assert!(
                serde_json::from_str::<RelayRequest>(body).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn replies_are_bounded_and_cleaned() {
        let reply = RelayReply::Error {
            message: format!("a\u{0}b{}", "x".repeat(500)),
        }
        .sanitized()
        .unwrap();
        let RelayReply::Error { message } = reply else {
            panic!()
        };
        assert_eq!(message.chars().count(), MAX_TEXT_CHARS);
        assert!(!message.contains('\u{0}'));
        let huge = RelayReply::Started {
            challenge: "c".into(),
            options: serde_json::json!("x".repeat(MAX_REPLY_BYTES)),
            statement: Signed {
                p: "p".into(),
                s: "s".into(),
            },
        };
        assert!(huge.sanitized().is_none());
    }
}
