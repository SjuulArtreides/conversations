//! Wire types for the private browser <-> host channel (a WebRTC data channel,
//! DTLS-encrypted end to end; the central server only relays its signaling).
//!
//! Handshake (over the opaque relay, `crate::host_relay`):
//! 1. `peer_challenge {nonce}` -> host stores a fresh one-use challenge durably
//!    and answers a `peer_challenge` statement `{v, typ, host, nonce, challenge, exp}`.
//! 2. `peer_offer {p, s, k, sdp}`: `p` is a signed operation envelope with
//!    `op: "peer.offer"`, `sess: "host"` and args `{challenge, sdp_sha256,
//!    fingerprint}`. The host verifies it like any operation (audience, key or
//!    delegation, lifetime), consumes the challenge, checks that the SDP hashes to
//!    `sdp_sha256` and carries exactly `fingerprint`, and pins that fingerprint for
//!    DTLS. It answers `peer_answer {sdp, statement}` where the statement is
//!    `{v, typ, host, kid, dlg, challenge, offer, answer, fingerprint}`: `offer` is
//!    the SHA-256 of the signed offer bytes, `answer` the SHA-256 of the answer SDP
//!    and `fingerprint` the host's DTLS fingerprint in it.
//!
//! A channel grants nothing by itself. Every message on it is a signed operation
//! that is verified and authorized against the host's CURRENT local state, and
//! it must be signed by the same key (and delegation) that signed the offer.
//! Hashes are base64url SHA-256; fingerprints are `sha-256 AB:CD:…` (uppercase).

use serde::{Deserialize, Serialize};

use crate::host_relay::Signed;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrCacheArgs {
    pub session: String,
    #[serde(default)]
    pub offset: u64,
    pub sha256: Option<String>,
}

/// Rename local history without changing its session identity or messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameArgs {
    pub session: String,
    pub title: String,
}

/// Host-owned project creation; never account-owned metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCreateArgs {
    pub name: String,
    pub description: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionProjectArgs {
    pub session: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionProjectSetArgs {
    pub session: String,
    #[serde(deserialize_with = "required_list_cursor")]
    pub project_id: Option<String>,
}

/// Metadata for an already-enrolled host credential; contains no key material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessCredential {
    pub kid: String,
    pub label: String,
    pub alg: String,
    pub kind: String,
    pub revoked: bool,
    pub grants: Vec<AccessGrant>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessGrant {
    pub cap: String,
    pub session: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunicationsListArgs {
    pub session: String,
    pub after: Option<String>,
    pub limit: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinearListArgs {
    pub session: String,
    #[serde(deserialize_with = "required_list_cursor")]
    pub after: Option<String>,
    pub limit: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinearUnlinkArgs {
    pub session: String,
    pub url: String,
}

/// Signed session-list arguments. Older callers omit running_only and list the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsListArgs {
    #[serde(deserialize_with = "required_list_cursor")]
    pub after: Option<String>,
    pub limit: u32,
    #[serde(default)]
    pub running_only: bool,
    #[serde(default)]
    pub readiness: bool,
}
fn required_list_cursor<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

/// The only data channel label a host accepts.
pub const CHANNEL_LABEL: &str = "conversations-private/v1";
/// Largest message the browser may send (one signed operation plus framing).
pub const MAX_REQUEST_MESSAGE: usize = 32 * 1024;
/// Largest message the host sends (one signed reply).
pub const MAX_REPLY_MESSAGE: usize = 64 * 1024;
/// Largest signed reply payload (before base64url).
pub const MAX_REPLY_PAYLOAD: usize = 44 * 1024;
/// Most history items one `history.get` page returns.
pub const MAX_HISTORY_PAGE: u32 = 32;
/// Most items one digest-only `history.get` page returns (no content, only
/// each message's size and hash, so a reader can find updated messages).
pub const MAX_DIGEST_PAGE: u32 = 256;
/// Most hits one `history.search` returns.
pub const MAX_SEARCH_HITS: u32 = 20;
/// Longest search query, in bytes.
pub const MAX_SEARCH_QUERY: usize = 256;
/// Largest canonical message carried inline in a history page. A larger one is
/// named by size and hash and read in [`MAX_FRAGMENT_BYTES`] pieces with
/// `history.fragment`; nothing is ever cut.
pub const MAX_INLINE_MESSAGE: usize = 16 * 1024;
/// Raw bytes of one `history.fragment` piece (base64url on the wire).
pub const MAX_FRAGMENT_BYTES: usize = 24 * 1024;
/// Most sessions one `sessions.list` page returns.
pub const MAX_SESSION_PAGE: u32 = 50;
/// Longest session title a listing carries, in bytes.
pub const MAX_LIST_TITLE: usize = 200;

/// Raw bytes one transfer `chunk` carries (base64url on the wire).
pub const MAX_CHUNK_BYTES: usize = 16 * 1024;
/// Transfers one channel may be receiving at once.
pub const MAX_INBOUND_TRANSFERS: usize = 2;
/// Terminal subscriptions one channel may hold at once.
pub const MAX_TERMINAL_SUBSCRIPTIONS: usize = 2;
/// Raw terminal bytes one `terminal` frame carries (base64url on the wire).
pub const MAX_TERMINAL_DATA: usize = 24 * 1024;

/// Browser -> host. Closed set, strict fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelRequest {
    /// One signed operation (`p`, `s`, `k` as on the relay). `id` only
    /// correlates the reply; it is not authority and not replay protection.
    Op {
        id: u32,
        p: String,
        s: String,
        k: String,
    },
    /// The next bytes of a transfer this channel's key had admitted (a signed
    /// `session.send_files` / `session.paste_image` answered `executed` on THIS
    /// channel). Bytes of file `file` from `offset`, in order; base64url. The
    /// bytes are not signed: the signed operation names their exact sizes and
    /// SHA-256s, and nothing reaches the terminal unless they match.
    Chunk {
        transfer: String,
        file: u32,
        offset: u64,
        data: String,
    },
    /// Stop a transfer; buffered bytes are dropped and nothing is applied.
    Cancel { transfer: String },
    /// Stop one terminal subscription.
    Unsubscribe { sub: u32 },
}

/// Host -> browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelReply {
    /// A host-signed `channel_reply` statement (see [`ReplyStatement`]).
    Reply { id: u32, statement: Signed },
    /// What became of a transfer's bytes. Carries no content.
    Transfer {
        transfer: String,
        status: TransferStatus,
    },
    /// One piece of a terminal subscription (see [`TerminalFrame`]).
    Terminal(TerminalFrame),
}

/// The outcome of a transfer's bytes on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    /// Complete, matching what was signed, still authorized, and the session's
    /// terminal reported it staged and pasted (paths only; it never presses Enter).
    Delivered,
    /// Not what was signed (order, size, hash), not admitted on this channel,
    /// expired, or no longer authorized. Nothing was applied.
    Refused,
    /// Cancelled by the browser. Nothing was applied.
    Cancelled,
    /// The host could not hand it to the terminal, or the terminal reported it
    /// could not stage or paste it.
    Failed,
    /// Handed to the terminal, which did not report an outcome (it disconnected,
    /// or answered too late). It may or may not have been pasted; the transfer
    /// is spent and is never retried.
    Uncertain,
}

/// Terminal output for one admitted `terminal.subscribe`, carried only on the
/// private channel. A subscription is a sequence of frames numbered from 0 with
/// no gaps; a `snapshot` (possibly in several pieces, the last with `fin`)
/// replaces the viewer's screen, an `update` continues it, and `end` closes the
/// subscription (session gone, authority withdrawn or unsubscribed). Frames are
/// not signed one by one: the subscription was, and the channel is bound to the
/// host's signed answer. Each frame passes the host's current-authority check
/// before it is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalFrame {
    pub sub: u32,
    pub seq: u64,
    pub kind: TerminalFrameKind,
    pub cols: u16,
    pub rows: u16,
    /// base64url (no padding) of raw terminal bytes.
    pub data: String,
    /// The last piece of this snapshot or update.
    pub fin: bool,
    /// Why the stream ended: only on an `End` frame. Content-free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<TerminalEnd>,
}

/// Why the host ended a terminal subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEnd {
    /// The signed subscribe it was released under expired, unrenewed.
    Expired,
    /// The authority it was admitted under is gone, or could not be checked.
    Withdrawn,
    /// The terminal itself went away.
    Gone,
    /// The page asked to stop.
    Unsubscribed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFrameKind {
    Snapshot,
    Update,
    End,
}

/// The payload of a `channel_reply` statement. It binds this host, this channel
/// (`peer` = the consumed challenge), the signing key and the exact request bytes,
/// so a reply cannot be moved to another request, key or channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyStatement {
    pub v: u32,
    pub typ: String,
    pub host: String,
    pub peer: String,
    pub kid: String,
    /// base64url SHA-256 of the operation payload bytes this answers.
    pub request: String,
    /// `executed`, `replayed`, `refused`, `failed` or `uncertain`.
    pub status: String,
    pub detail: Option<String>,
    pub body: Option<ReplyBody>,
}

/// Content of an admitted read. Only ever sent on the private channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplyBody {
    Migration {
        manifest: crate::migration::MigrationManifest,
        b64: String,
        next: Option<u64>,
    },
    Communications {
        peers: Vec<crate::communications::CommunicationPeer>,
        exchanges: Vec<crate::communications::CommunicationExchange>,
        next: Option<String>,
    },
    AttentionRecipients {
        recipients: Vec<crate::attention::Identity>,
    },
    Notifications {
        notifications: Vec<crate::attention::Notification>,
        next: Option<i64>,
    },
    Linear {
        docs: Vec<crate::SessionLinearDoc>,
        next: Option<String>,
    },
    Access {
        credentials: Vec<AccessCredential>,
    },
    PrCache {
        b64: String,
        sha256: String,
        next: Option<u64>,
    },
    Projects {
        projects: Vec<crate::Project>,
    },
    ProjectMembership {
        project_id: Option<String>,
    },
    /// A page of one session's local history, in first-appearance order. `next`
    /// is the cursor for the following page (`None` when this page reached the
    /// end). A message keeps its `seq` when the agent later rewrites it (a
    /// streamed turn finishing, a tool result arriving), so a reader that only
    /// asks for `seq > last` misses updates: it compares `sha256` from a digest
    /// page and re-reads the items that changed.
    History {
        items: Vec<HistoryItem>,
        next: Option<u64>,
    },
    /// One piece of one stored message, `offset..offset+len` of its canonical
    /// bytes. `sha256` names the whole message these bytes belong to, so pieces
    /// of a message that changed between reads cannot be joined.
    Fragment {
        seq: u64,
        offset: u64,
        /// base64url (no padding) of the raw bytes.
        data: String,
        size: u64,
        sha256: String,
        next: Option<u64>,
    },
    Search {
        hits: Vec<SearchHit>,
    },
    /// The local sessions the signer may read now, newest first.
    Sessions {
        sessions: Vec<SessionEntry>,
        next: Option<String>,
    },
    /// The host's launch settings.
    Settings {
        launch: serde_json::Value,
    },
    /// An admitted `terminal.subscribe`: its frames follow as
    /// [`ChannelReply::Terminal`] with this `sub`.
    Terminal {
        sub: u32,
    },
}

/// One stored message: the complete canonical `conversations_core::Message`
/// (blocks, tool calls and results, usage, raw) exactly as the local history
/// holds it. `size` and `sha256` describe its canonical JSON bytes; `message` is
/// absent when they exceed [`MAX_INLINE_MESSAGE`] and must be read with
/// `history.fragment` and checked against `sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryItem {
    pub seq: u64,
    pub uuid: String,
    pub size: u64,
    pub sha256: String,
    pub message: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchHit {
    pub seq: u64,
    pub uuid: String,
    pub snippet: String,
}

/// Native agent evidence, independent of terminal output or connection state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentReadiness {
    #[default]
    Unknown,
    Working,
    Background,
    NeedsInput,
    Ready,
}

/// A local session in a private listing. This is private data (the folder is
/// never published) and crosses only the private channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEntry {
    pub id: String,
    pub agent: String,
    pub cwd: String,
    pub title: Option<String>,
    pub messages: u64,
    /// RFC 3339 time of the last stored message.
    pub last_activity: Option<String>,
    /// The host has a running terminal for it.
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<AgentReadiness>,
    /// Explicit false means this host cannot reopen the stored history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reopenable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_mode: Option<String>,
    #[serde(default)]
    pub prs: Vec<String>,
}

/// The one `a=fingerprint` of an SDP, normalized to `sha-256 AB:…`. Exactly one
/// distinct SHA-256 value must be present; anything else is refused.
pub fn sdp_fingerprint(sdp: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for line in sdp.split('\n') {
        let line = line.trim_end_matches('\r');
        let Some(rest) = line.strip_prefix("a=fingerprint:") else {
            continue;
        };
        let mut parts = rest.split(' ');
        let (Some(hash), Some(value), None) = (parts.next(), parts.next(), parts.next()) else {
            return None;
        };
        if !hash.eq_ignore_ascii_case("sha-256") {
            return None;
        }
        let value = value.to_ascii_uppercase();
        if !valid_fingerprint_value(&value) {
            return None;
        }
        let fingerprint = format!("sha-256 {value}");
        match &found {
            Some(existing) if *existing != fingerprint => return None,
            _ => found = Some(fingerprint),
        }
    }
    found
}

/// `sha-256 AB:CD:…` with exactly 32 uppercase hex bytes.
pub fn valid_fingerprint(value: &str) -> bool {
    value
        .strip_prefix("sha-256 ")
        .is_some_and(valid_fingerprint_value)
}

fn valid_fingerprint_value(value: &str) -> bool {
    let bytes: Vec<&str> = value.split(':').collect();
    bytes.len() == 32
        && bytes.iter().all(|b| {
            b.len() == 2
                && b.bytes()
                    .all(|c| c.is_ascii_digit() || (b'A'..=b'F').contains(&c))
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn running_only_listing_is_opt_in_and_strictly_typed() {
        let all: super::SessionsListArgs =
            serde_json::from_str(r#"{"after":null,"limit":50}"#).unwrap();
        assert!(!all.running_only);
        let active: super::SessionsListArgs =
            serde_json::from_str(r#"{"after":null,"limit":50,"running_only":true}"#).unwrap();
        assert!(active.running_only);
        assert!(serde_json::from_str::<super::SessionsListArgs>(
            r#"{"after":null,"limit":50,"running_only":"true"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<super::SessionsListArgs>(r#"{"limit":50}"#).is_err());
    }

    use super::*;

    const FP: &str = "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99";

    #[test]
    fn exactly_one_sha256_fingerprint_is_accepted() {
        let one = format!("v=0\r\na=fingerprint:sha-256 {}\r\n", FP.to_lowercase());
        assert_eq!(sdp_fingerprint(&one).unwrap(), format!("sha-256 {FP}"));
        let twice = format!("{one}a=fingerprint:sha-256 {FP}\r\n");
        assert_eq!(sdp_fingerprint(&twice).unwrap(), format!("sha-256 {FP}"));
        let other = FP.replacen("AA", "AB", 1);
        assert!(sdp_fingerprint(&format!("{one}a=fingerprint:sha-256 {other}\r\n")).is_none());
        assert!(sdp_fingerprint("v=0\r\n").is_none());
        assert!(sdp_fingerprint(&format!("a=fingerprint:sha-1 {FP}\r\n")).is_none());
        assert!(sdp_fingerprint("a=fingerprint:sha-256 AA:BB\r\n").is_none());
        assert!(valid_fingerprint(&format!("sha-256 {FP}")));
        assert!(!valid_fingerprint(&format!(
            "sha-256 {}",
            FP.to_lowercase()
        )));
    }

    #[test]
    fn only_an_end_names_why_the_stream_ended() {
        let end = TerminalFrame {
            sub: 1,
            seq: 4,
            kind: TerminalFrameKind::End,
            cols: 0,
            rows: 0,
            data: String::new(),
            fin: true,
            end: Some(TerminalEnd::Expired),
        };
        let text = serde_json::to_string(&ChannelReply::Terminal(end.clone())).unwrap();
        assert!(text.contains(r#""end":"expired""#), "{text}");
        let update = TerminalFrame {
            kind: TerminalFrameKind::Update,
            end: None,
            ..end
        };
        let text = serde_json::to_string(&ChannelReply::Terminal(update)).unwrap();
        assert!(!text.contains(r#""end""#), "{text}");
        for cause in ["expired", "withdrawn", "gone", "unsubscribed"] {
            let parsed: TerminalEnd = serde_json::from_str(&format!("{cause:?}")).unwrap();
            assert_eq!(
                serde_json::to_string(&parsed).unwrap(),
                format!("{cause:?}")
            );
        }
    }

    #[test]
    fn channel_messages_are_strict() {
        for body in [
            r#"{"t":"op","id":1,"p":"a","s":"b","k":"c","x":1}"#,
            r#"{"t":"control","cmd":"end"}"#,
            r#"{"t":"op","id":-1,"p":"a","s":"b","k":"c"}"#,
        ] {
            assert!(
                serde_json::from_str::<ChannelRequest>(body).is_err(),
                "{body}"
            );
        }
        let ok: ChannelRequest =
            serde_json::from_str(r#"{"t":"op","id":7,"p":"a","s":"b","k":"c"}"#).unwrap();
        assert!(matches!(ok, ChannelRequest::Op { id: 7, .. }));
    }

    #[test]
    fn transfer_and_terminal_messages_are_strict() {
        for body in [
            r#"{"t":"chunk","transfer":"ab","file":0,"offset":0}"#,
            r#"{"t":"chunk","transfer":"ab","file":0,"offset":0,"data":"","sha256":"x"}"#,
            r#"{"t":"chunk","transfer":"ab","file":-1,"offset":0,"data":""}"#,
            r#"{"t":"cancel","transfer":"ab","why":"x"}"#,
            r#"{"t":"unsubscribe","sub":1,"session":"s"}"#,
        ] {
            assert!(
                serde_json::from_str::<ChannelRequest>(body).is_err(),
                "{body}"
            );
        }
        let chunk: ChannelRequest = serde_json::from_str(
            r#"{"t":"chunk","transfer":"ab","file":1,"offset":16384,"data":"AAEC"}"#,
        )
        .unwrap();
        assert!(matches!(
            chunk,
            ChannelRequest::Chunk {
                file: 1,
                offset: 16384,
                ..
            }
        ));
        let frame = ChannelReply::Terminal(TerminalFrame {
            sub: 3,
            seq: 0,
            kind: TerminalFrameKind::Snapshot,
            cols: 80,
            rows: 24,
            data: "aGk".into(),
            fin: true,
            end: None,
        });
        let text = serde_json::to_string(&frame).unwrap();
        assert_eq!(
            text,
            r#"{"t":"terminal","sub":3,"seq":0,"kind":"snapshot","cols":80,"rows":24,"data":"aGk","fin":true}"#
        );
        assert_eq!(serde_json::from_str::<ChannelReply>(&text).unwrap(), frame);
        let done = ChannelReply::Transfer {
            transfer: "ab".into(),
            status: TransferStatus::Delivered,
        };
        assert_eq!(
            serde_json::to_string(&done).unwrap(),
            r#"{"t":"transfer","transfer":"ab","status":"delivered"}"#
        );
    }
}
