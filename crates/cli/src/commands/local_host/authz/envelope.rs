//! Signed operation envelope.
//!
//! The signature covers the exact payload bytes received ("sign what you send"),
//! so there is no canonicalization step. Verification order is signature first,
//! then strict typed parsing: every operation has its own `deny_unknown_fields`
//! argument type, so unknown or duplicated fields at ANY depth are rejected, and
//! an unknown operation name is rejected before its arguments are interpreted.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use conversations_core::{
    private_channel::{
        LinearListArgs, LinearUnlinkArgs, ProjectCreateArgs, SessionProjectArgs,
        SessionProjectSetArgs, SessionRenameArgs, SessionsListArgs,
    },
    AgentKind, AgentLauncher,
};
use ring::{digest, signature};
use serde::Deserialize;

pub const DOMAIN_OP: &[u8] = b"conversations-op/v1\n";
/// Largest accepted decoded payload, and the largest accepted encoded field.
/// Sized so a reply of `MAX_REPLY_TEXT_BYTES` (plus its envelope) fits, and the
/// base64 form still fits in one relay request (32 KiB).
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
/// Largest message text one signed reply may carry.
pub const MAX_REPLY_TEXT_BYTES: usize = 4096;
/// Largest raw input (decoded) one signed input frame may carry.
pub const MAX_INPUT_BYTES: usize = 4096;
/// Most capabilities one `grant.add` names.
pub const MAX_GRANT_CAPS: usize = 4;
/// Input-class (stream) frames must be fresh: they are not journaled, so their
/// short life bounds replay by a relay.
pub const STREAM_LIFETIME_SECS: i64 = 30;
const MAX_B64_FIELD: usize = MAX_PAYLOAD_BYTES.div_ceil(3) * 4;
pub const MAX_LIFETIME_SECS: i64 = 120;
pub const SKEW_SECS: i64 = 60;
const SIGNATURE_BYTES: usize = 64;

/// One signed control frame as received. Nothing here is trusted until
/// [`verify`] succeeds; `kid` only selects which locally stored key to try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedFrame {
    pub payload: Vec<u8>,
    pub sig: Vec<u8>,
    pub kid: String,
}

impl SignedFrame {
    /// Decode the wire form. Oversized or malformed fields yield `None`
    /// before any allocation proportional to attacker input.
    pub fn from_wire(payload: &str, sig: &str, kid: &str) -> Option<Self> {
        if payload.len() > MAX_B64_FIELD || sig.len() > MAX_B64_FIELD || !valid_key_id(kid) {
            return None;
        }
        let payload = B64.decode(payload).ok()?;
        let sig = B64.decode(sig).ok()?;
        (payload.len() <= MAX_PAYLOAD_BYTES && sig.len() == SIGNATURE_BYTES).then(|| Self {
            payload,
            sig,
            kid: kid.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alg {
    /// WebCrypto ECDSA P-256 / SHA-256 emits raw `r || s`.
    Es256,
    Ed25519,
}

impl Alg {
    pub fn as_str(self) -> &'static str {
        match self {
            Alg::Es256 => "es256",
            Alg::Ed25519 => "ed25519",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "es256" => Some(Alg::Es256),
            "ed25519" => Some(Alg::Ed25519),
            _ => None,
        }
    }

    /// Public key encodings accepted at enrollment: raw 32 bytes or an
    /// uncompressed SEC1 P-256 point.
    pub fn valid_public_key(self, key: &[u8]) -> bool {
        match self {
            Alg::Ed25519 => key.len() == 32,
            Alg::Es256 => key.len() == 65 && key[0] == 0x04,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    Malformed(String),
    BadSignature,
    UnknownOperation,
    WrongAudience,
    KeyMismatch,
    BadIdentifier(&'static str),
    SessionMismatch,
    InvalidLifetime,
    Expired,
    NotYetValid,
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reject::Malformed(detail) => write!(f, "malformed operation: {detail}"),
            Reject::BadSignature => f.write_str("signature does not verify"),
            Reject::UnknownOperation => f.write_str("unknown operation"),
            Reject::WrongAudience => f.write_str("operation is addressed to another host"),
            Reject::KeyMismatch => f.write_str("operation names a different key"),
            Reject::BadIdentifier(what) => write!(f, "invalid {what}"),
            Reject::SessionMismatch => f.write_str("session identifiers disagree"),
            Reject::InvalidLifetime => f.write_str("invalid operation lifetime"),
            Reject::Expired => f.write_str("operation expired"),
            Reject::NotYetValid => f.write_str("operation is not yet valid"),
        }
    }
}

impl std::error::Error for Reject {}

/// Operations this host understands. Unknown names fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    MigrationExport {
        session: String,
        transfer: String,
        offset: u64,
    },
    MigrationImport {
        input: conversations_core::migration::MigrationImport,
    },
    CommunicationsList {
        session: String,
        after: Option<String>,
        limit: u32,
    },
    LinearList {
        session: String,
        after: Option<String>,
        limit: u32,
    },
    LinearUnlink {
        session: String,
        url: String,
    },
    AccessList,
    AttentionRecipients {
        session: String,
    },
    NotificationsList {
        after: Option<i64>,
        limit: u32,
    },
    NotificationMark {
        session: String,
        id: String,
        resolve: bool,
    },
    AttentionRequest {
        session: String,
        input: conversations_core::attention::AttentionRequest,
    },
    ProjectsList,
    ProjectCreate {
        name: String,
        description: Option<String>,
    },
    SessionProjectGet {
        session: String,
    },
    SessionProjectSet {
        session: String,
        project_id: Option<String>,
    },
    SessionStart {
        session: String,
        agent: AgentKind,
        launcher: Option<AgentLauncher>,
    },
    /// Resume an existing local session. The payload names ONLY the session:
    /// cwd, native resume id, launcher and flags are resolved from local state.
    SessionReopen {
        session: String,
    },
    SessionRename {
        session: String,
        title: String,
    },
    /// Give an existing recipient credential authority over ONE existing local
    /// session. The recipient and capabilities are named exactly; whether they
    /// are allowed is decided against local state, never by the frame.
    GrantAdd {
        session: String,
        recipient: String,
        caps: Vec<Cap>,
    },
    /// Take back exactly these capabilities on ONE session from one recipient
    /// credential: only rows for that session, never a host-wide grant, the
    /// recipient's other sessions or its credential.
    GrantRemove {
        session: String,
        recipient: String,
        caps: Vec<Cap>,
    },
    /// Raw keystrokes. Stream class: each frame's `jti` is durably consumed
    /// (so no frame is ever delivered twice, in this process or after a host
    /// restart); `seq` orders frames of ONE input channel `ch`.
    SessionInput {
        session: String,
        ch: String,
        seq: u64,
        data: Vec<u8>,
    },
    /// A message: sanitized text followed by a submitting Enter. Journaled.
    SessionReply {
        session: String,
        text: String,
    },
    /// A message from one agent session to an EXISTING session: `session` is the
    /// recipient, `source` the sender's claimed session (a label under the signing
    /// key, never authority). It is recorded in the host's local inbox; it never
    /// types into a terminal by itself and cannot start or reopen anything.
    /// `source_host` is the id of the sender's own host, which a reply to this
    /// message must be addressed to before it can acknowledge it.
    AgentMessage {
        session: String,
        source: String,
        source_host: String,
        text: String,
        reply_to: Option<String>,
    },
    /// Latest-wins size request, ordered by `seq` within its own channel `ch`
    /// (never shared with input, so a resize cannot make input look stale).
    SessionResize {
        session: String,
        ch: String,
        seq: u64,
        cols: u16,
        rows: u16,
    },
    /// Idempotent keyframe request. Replay-protected by `jti`; no ordering.
    SessionRepaint {
        session: String,
    },
    /// End the session's terminal. Journaled.
    SessionEnd {
        session: String,
    },
    /// Authorize ONE clipboard image: exactly this type, size and SHA-256. The
    /// bytes travel separately; the host applies them only if they match.
    SessionPasteImage {
        session: String,
        transfer: String,
        mime: String,
        size: u64,
        sha256: String,
    },
    /// Authorize ONE file drop: exactly these names, sizes and SHA-256s, in order.
    SessionSendFiles {
        session: String,
        transfer: String,
        files: Vec<FileSpec>,
    },
    /// Ask whether the message THIS key sent to `session` as operation `message`
    /// (its `jti`) is in the recipient host's inbox, and in which state.
    /// Read-only; answered in the host-signed result.
    AgentStatus {
        session: String,
        message: String,
    },
    /// Re-read ONE named pull request of this session with the local `gh` and
    /// publish the result. The target is exactly what was signed; the host never
    /// takes a target from the server.
    PrSync {
        session: String,
        owner: String,
        repo: String,
        number: i64,
    },
    /// Post ONE comment (or a threaded reply) on a named pull request as this
    /// machine's GitHub user. Repo, number, thread and body are exactly what was
    /// signed.
    PrComment {
        session: String,
        owner: String,
        repo: String,
        number: i64,
        body: String,
        in_reply_to: Option<i64>,
    },
    /// Publish ONE known local session's canonical history to this host's
    /// configured account and server, readable by exactly `audience`. A newer
    /// share revision; never an execution grant.
    PublicationShare {
        session: String,
        audience: conversations_core::publication::Audience,
    },
    /// Stop publishing ONE session and delete its central projection.
    PublicationUnshare {
        session: String,
    },
    /// Read ONE session's local publication state (answered in the signed result).
    PublicationStatus {
        session: String,
    },
    /// Read one page of a session's local history, after sequence `after`.
    /// `digest`: sizes and hashes only, to find messages rewritten in place.
    /// Answered only on the private channel, never over the relay.
    HistoryGet {
        session: String,
        after: u64,
        limit: u32,
        digest: bool,
    },
    /// Read one piece of one stored message too large to be inline in a page.
    /// Answered only on the private channel.
    PrCacheList {
        session: String,
        offset: u64,
        sha256: Option<String>,
    },
    PrUnlink {
        session: String,
        owner: String,
        repo: String,
        number: i64,
    },
    HistoryFragment {
        session: String,
        seq: u64,
        offset: u64,
    },
    /// Follow ONE session's terminal (its current screen, then its output) on
    /// the private channel. Answered only there; every frame is released under
    /// the authority this subscription was admitted under.
    TerminalSubscribe {
        session: String,
    },
    /// Host-scoped listing of the local sessions whose history the signer may
    /// read NOW: every session is filtered by its own `history.read` authority,
    /// so this reveals nothing a per-session read would not. Answered only on
    /// the private channel.
    SessionsList {
        after: Option<String>,
        limit: u32,
        running_only: bool,
        readiness: bool,
    },
    /// Full-text search within one session's local history. Answered only on the
    /// private channel; the query never crosses the relay.
    HistorySearch {
        session: String,
        query: String,
        limit: u32,
    },
    /// Host-scoped (no session): read this host's launch settings.
    SettingsGet,
    /// Host-scoped: replace this host's launch settings. Only the launch block is
    /// representable; origins, relying party, credentials and grants are not.
    SettingsUpdate {
        launch: LaunchUpdate,
    },
    /// Host-scoped, owner-only: begin a passkey enrollment ceremony on this
    /// host, exactly as the local `host passkey enroll-start` does, and get the
    /// invite link to hand to the person being invited. The registration still
    /// waits for `PasskeyApprove`; nothing here trusts anything yet.
    PasskeyEnrollStart {
        label: String,
        kind: String,
        grants: Vec<String>,
    },
    /// Host-scoped, owner-only: list the verified registrations waiting for
    /// approval — the key offers this host is holding.
    PasskeyPending,
    /// Host-scoped, owner-only: approve one pending registration as a
    /// permanent credential (UI-based local approval:
    /// the host records the trust; the browser is a signed remote control).
    PasskeyApprove {
        cred_id: String,
    },
    /// Host-scoped, owner-only: sign one enrollment-ceremony vouch with THIS
    /// host's identity key, for a ceremony on ANOTHER host. The statement
    /// binds that host, that ceremony AND the passkey id the enrollment is
    /// expected to produce (`base64url(SHA-256(raw credential id))`, derived
    /// by the browser from its authenticator before anything is submitted);
    /// it names no session and grants nothing (it is only
    /// the pairwise trust anchor).
    PasskeyVouch {
        for_host: String,
        challenge: String,
        cred_id: String,
    },
}

/// The complete, typed launch configuration a signed settings update carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchUpdate {
    pub claude_skip_permissions: bool,
    pub opencode_auto: bool,
    pub codex_sandbox_mode: Option<String>,
    pub codex_approval_policy: Option<String>,
    pub default_launcher: AgentLauncher,
    pub adk_statusline: bool,
}

/// `sess` of an operation that is about the host, not a session.
pub const HOST_SCOPE: &str = "host";
/// Largest comment body one signed PR operation may carry.
pub const MAX_PR_BODY_BYTES: usize = 8000;

/// One authorized file: the name is a display hint (the host derives its own
/// path), the size and SHA-256 are what the received bytes must equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

impl Operation {
    /// The target and required capability are properties of the decoded operation,
    /// never independent authority fields that a caller can accidentally mismatch.
    pub fn session_id(&self) -> &str {
        match self {
            Self::MigrationExport { session, .. }
            | Self::SessionStart { session, .. }
            | Self::SessionReopen { session }
            | Self::SessionRename { session, .. }
            | Self::CommunicationsList { session, .. }
            | Self::AttentionRecipients { session }
            | Self::NotificationMark { session, .. }
            | Self::AttentionRequest { session, .. }
            | Self::LinearList { session, .. }
            | Self::LinearUnlink { session, .. }
            | Self::SessionProjectGet { session }
            | Self::SessionProjectSet { session, .. }
            | Self::GrantAdd { session, .. }
            | Self::GrantRemove { session, .. }
            | Self::SessionInput { session, .. }
            | Self::SessionReply { session, .. }
            | Self::AgentMessage { session, .. }
            | Self::SessionResize { session, .. }
            | Self::SessionRepaint { session }
            | Self::SessionEnd { session }
            | Self::SessionPasteImage { session, .. }
            | Self::SessionSendFiles { session, .. }
            | Self::AgentStatus { session, .. }
            | Self::PrSync { session, .. }
            | Self::PrUnlink { session, .. }
            | Self::PrCacheList { session, .. }
            | Self::PrComment { session, .. }
            | Self::PublicationShare { session, .. }
            | Self::PublicationUnshare { session }
            | Self::PublicationStatus { session }
            | Self::HistoryGet { session, .. }
            | Self::HistoryFragment { session, .. }
            | Self::HistorySearch { session, .. }
            | Self::TerminalSubscribe { session } => session,
            Self::MigrationImport { .. }
            | Self::NotificationsList { .. }
            | Self::AccessList
            | Self::ProjectsList
            | Self::ProjectCreate { .. }
            | Self::SettingsGet
            | Self::SettingsUpdate { .. }
            | Self::SessionsList { .. }
            | Self::PasskeyEnrollStart { .. }
            | Self::PasskeyPending
            | Self::PasskeyApprove { .. }
            | Self::PasskeyVouch { .. } => HOST_SCOPE,
        }
    }

    pub fn global_owner_metadata(&self) -> bool {
        matches!(
            self,
            Self::ProjectsList | Self::ProjectCreate { .. } | Self::AccessList
        )
    }
    pub fn owner_metadata(&self) -> bool {
        self.global_owner_metadata()
            || matches!(
                self,
                Self::CommunicationsList { .. }
                    | Self::AttentionRecipients { .. }
                    | Self::AttentionRequest { .. }
                    | Self::LinearUnlink { .. }
                    | Self::SessionProjectGet { .. }
                    | Self::SessionProjectSet { .. }
            )
    }

    pub fn capability(&self) -> Cap {
        match self {
            Self::SessionStart { .. } | Self::MigrationImport { .. } => Cap::SessionStart,
            Self::SessionReopen { .. } => Cap::SessionReopen,
            Self::GrantAdd { .. }
            | Self::GrantRemove { .. }
            | Self::PublicationShare { .. }
            | Self::PublicationUnshare { .. }
            | Self::PublicationStatus { .. } => Cap::SessionManageSharing,
            Self::AttentionRecipients { .. }
            | Self::SessionInput { .. }
            | Self::SessionReply { .. }
            | Self::SessionRename { .. }
            | Self::AttentionRequest { .. }
            | Self::LinearUnlink { .. }
            | Self::ProjectsList
            | Self::ProjectCreate { .. }
            | Self::AccessList
            | Self::SessionProjectGet { .. }
            | Self::SessionProjectSet { .. }
            | Self::AgentMessage { .. }
            | Self::SessionResize { .. }
            | Self::SessionRepaint { .. }
            | Self::SessionEnd { .. }
            | Self::SessionPasteImage { .. }
            | Self::SessionSendFiles { .. }
            | Self::AgentStatus { .. } => Cap::SessionWrite,
            // PR refresh (re-reading a PR and recording a target) follows the
            // session.write boundary: anyone allowed to drive the session's
            // terminal may also refresh its linked pull requests. The separate
            // session.pr_sync capability is obsolete; see `Cap::PrSync`.
            Self::PrSync { .. } | Self::PrUnlink { .. } => Cap::SessionWrite,
            Self::PrComment { .. } => Cap::PrComment,
            Self::MigrationExport { .. }
            | Self::CommunicationsList { .. }
            | Self::NotificationsList { .. }
            | Self::NotificationMark { .. }
            | Self::HistoryGet { .. }
            | Self::PrCacheList { .. }
            | Self::LinearList { .. }
            | Self::HistoryFragment { .. }
            | Self::HistorySearch { .. }
            | Self::TerminalSubscribe { .. }
            | Self::SessionsList { .. } => Cap::HistoryRead,
            Self::SettingsGet => Cap::SettingsRead,
            Self::SettingsUpdate { .. } => Cap::SettingsWrite,
            Self::PasskeyEnrollStart { .. }
            | Self::PasskeyPending
            | Self::PasskeyApprove { .. } => Cap::PasskeyManage,
            Self::PasskeyVouch { .. } => Cap::SettingsRead,
        }
    }

    /// Stream-class operations (keystrokes, resize, repaint) are checked against
    /// current authority on every frame and are not journaled as operations;
    /// their `jti` is consumed durably instead (see `Store::authorize_stream`).
    pub fn is_stream(&self) -> bool {
        matches!(
            self,
            Operation::SessionInput { .. }
                | Operation::SessionResize { .. }
                | Operation::SessionRepaint { .. }
        )
    }

    /// The ordering domain, channel and sequence of a stream frame, if it has
    /// one. Domains are independent: input is ordered only against the same
    /// input channel, resize only against resizes of the same channel.
    pub fn stream_order(&self) -> Option<(&'static str, &str, u64)> {
        match self {
            Operation::SessionInput { ch, seq, .. } => Some(("input", ch, *seq)),
            Operation::SessionResize { ch, seq, .. } => Some(("resize", ch, *seq)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    SessionStart,
    SessionReopen,
    SessionWrite,
    HistoryRead,
    /// Share one existing session: sign `grant.add` for it. Never grantable.
    SessionManageSharing,
    /// Obsolete, kept parseable for storage compatibility with credentials
    /// enrolled before PR refresh followed `session.write`: a ceiling, grant
    /// row, delegation `caps` string or `session_pr_targets.cap` may still
    /// contain `session.pr_sync` on a laptop in the wild. Nothing requires it
    /// any more — `pr.sync` and `pr.unlink` now check [`Cap::SessionWrite`]
    /// against the exact signed session — so a standalone `session.pr_sync`
    /// grant no longer authorizes PR refresh on its own.
    PrSync,
    /// Post a comment as this machine's GitHub user. Owner keys only, never grantable.
    PrComment,
    /// Host-scoped: read launch settings. Owner keys only, host-wide grant.
    SettingsRead,
    /// Host-scoped: change launch settings. Owner keys only, host-wide grant.
    SettingsWrite,
    /// Host-scoped: enroll, list and approve passkey registrations — the key
    /// trust ceremony itself. Owner keys only, never grantable: a recipient of
    /// any session capability must never be able to mint or approve additional
    /// credentials on this host.
    PasskeyManage,
}

impl Cap {
    pub fn as_str(self) -> &'static str {
        match self {
            Cap::SessionStart => "session.start",
            Cap::SessionReopen => "session.reopen",
            Cap::SessionWrite => "session.write",
            Cap::HistoryRead => "history.read",
            Cap::SessionManageSharing => "session.manage_sharing",
            Cap::PrSync => "session.pr_sync",
            Cap::PrComment => "session.pr_comment",
            Cap::SettingsRead => "host.settings_read",
            Cap::SettingsWrite => "host.settings_write",
            Cap::PasskeyManage => "passkey.manage",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Cap::SessionStart,
            Cap::SessionReopen,
            Cap::SessionWrite,
            Cap::HistoryRead,
            Cap::SessionManageSharing,
            Cap::PrSync,
            Cap::PrComment,
            Cap::SettingsRead,
            Cap::SettingsWrite,
            Cap::PasskeyManage,
        ]
        .into_iter()
        .find(|cap| cap.as_str() == value)
    }

    /// Capabilities that create a session. Agent keys can never hold them.
    pub fn creates_sessions(self) -> bool {
        matches!(self, Cap::SessionStart | Cap::SessionReopen)
    }

    /// Capabilities about the host itself rather than a session: granted
    /// host-wide only, and checked against host-wide rows.
    pub fn host_scoped(self) -> bool {
        matches!(
            self,
            Cap::SettingsRead | Cap::SettingsWrite | Cap::PasskeyManage
        )
    }

    /// Capabilities only an owner-kind credential can ever hold.
    pub fn owner_only(self) -> bool {
        self.creates_sessions()
            || matches!(
                self,
                Cap::SessionManageSharing
                    | Cap::PrSync
                    | Cap::PrComment
                    | Cap::SettingsRead
                    | Cap::SettingsWrite
                    | Cap::PasskeyManage
            )
    }

    /// Capabilities `grant.add` may hand to a recipient. Sharing management,
    /// session creation and delegation are never transferable.
    pub fn grantable(self) -> bool {
        matches!(self, Cap::SessionWrite | Cap::HistoryRead)
    }
}

/// A fully verified operation: bound to this host, this key, exact parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The key that signed (for a delegated operation: the browser key).
    pub kid: String,
    /// Delegation the signing key acted under, `None` for a directly enrolled key.
    pub dlg: Option<String>,
    pub jti: String,
    /// Signing time, as stated (and range-checked) in the envelope.
    pub iat: i64,
    /// Expiry, as stated (and range-checked) in the envelope.
    pub exp: i64,
    pub payload_sha256: Vec<u8>,
    pub op: Operation,
    /// The grant activation the host admitted this operation under. Set only
    /// by local admission (0 until then); never read from the envelope.
    pub grant_epoch: i64,
    /// The authority rows the operation was admitted under, recorded inside the
    /// admission transaction (`Store::admit`); `None` until admitted.
    pub admitted_under: Option<String>,
}

impl Verified {
    /// A host-scoped `history.read` operation (the session listing): authorized
    /// per listed session, not by a host-wide grant.
    pub fn is_listing(&self) -> bool {
        matches!(
            self.op,
            Operation::SessionsList { .. } | Operation::NotificationsList { .. }
        )
    }
}

#[derive(Deserialize)]
struct Peek {
    op: String,
}

/// Makes `Option` fields mandatory: serde treats a missing `Option` as `None`
/// unless a custom deserializer is attached.
fn required<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// The operation name a payload CLAIMS (untrusted until verified). Used only to
/// shape the answer to an operation the verifier has already accepted.
pub fn claimed_operation(payload: &[u8]) -> Option<String> {
    serde_json::from_slice::<Peek>(payload).ok().map(|p| p.op)
}

/// The delegation id a payload CLAIMS, read without trusting it. Used only to
/// choose which stored key to verify against; the verified envelope must then
/// carry exactly the same value.
pub fn claimed_delegation(payload: &[u8]) -> Option<Option<String>> {
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    match value.get("dlg")? {
        serde_json::Value::Null => Some(None),
        serde_json::Value::String(id) => Some(Some(id.clone())),
        _ => None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<A> {
    v: u32,
    typ: String,
    aud: String,
    kid: String,
    /// Delegation id when `kid` is a delegated browser key, else `null`. The key
    /// must be present either way: an envelope cannot silently omit it.
    #[serde(deserialize_with = "required")]
    dlg: Option<String>,
    #[allow(dead_code)]
    op: String,
    sess: String,
    jti: String,
    iat: i64,
    exp: i64,
    args: A,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    session: String,
    agent: AgentKind,
    #[serde(default)]
    launcher: Option<AgentLauncher>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReopenArgs {
    session: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationShareArgs {
    session: String,
    audience: conversations_core::publication::Audience,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantAddArgs {
    session: String,
    recipient: String,
    caps: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputArgs {
    session: String,
    ch: String,
    seq: u64,
    b64: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentMessageArgs {
    session: String,
    source: String,
    source_host: String,
    text: String,
    #[serde(default, deserialize_with = "required")]
    reply_to: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentStatusArgs {
    session: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyArgs {
    session: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResizeArgs {
    session: String,
    ch: String,
    seq: u64,
    cols: u16,
    rows: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasteImageArgs {
    session: String,
    transfer: String,
    mime: String,
    size: u64,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileArgs {
    name: String,
    size: u64,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendFilesArgs {
    session: String,
    transfer: String,
    files: Vec<FileArgs>,
}

/// Lowercase hex SHA-256.
fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Signed message text: not blank, at most `max` bytes, no NUL.
fn valid_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max && !text.contains('\0')
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrSyncArgs {
    session: String,
    owner: String,
    repo: String,
    number: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrCommentArgs {
    session: String,
    owner: String,
    repo: String,
    number: i64,
    body: String,
    /// Absent or `null`: a top-level comment.
    #[serde(default)]
    in_reply_to: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryGetArgs {
    session: String,
    after: u64,
    limit: u32,
    #[serde(default)]
    digest: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryFragmentArgs {
    session: String,
    seq: u64,
    offset: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalSubscribeArgs {
    session: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistorySearchArgs {
    session: String,
    query: String,
    limit: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerOfferArgs {
    challenge: String,
    sdp_sha256: String,
    fingerprint: String,
}

/// Operations whose answers carry local content. They are answered only on the
/// private channel (`daemon::private_channel`); every other path refuses them.
pub fn private_only(op: &str) -> bool {
    matches!(
        op,
        "migration.export"
            | "communications.list"
            | "attention.recipients"
            | "notifications.list"
            | "linear.list"
            | "access.list"
            | "projects.list"
            | "session.project.get"
            | "history.get"
            | "pr.cache.list"
            | "history.fragment"
            | "history.search"
            | "sessions.list"
            | "settings.get"
            | "terminal.subscribe"
    )
}

/// Private controls travel only on the encrypted channel, including lifecycle,
/// key access and publication management—not just controls containing text.
/// Pairing ceremonies remain bootstrap messages. Native agent messages and
/// their status questions travel host to host on the same channel.
pub fn channel_only(op: &str) -> bool {
    matches!(
        op,
        "migration.import"
            | "notifications.read"
            | "notifications.resolve"
            | "attention.request"
            | "linear.unlink"
            | "projects.create"
            | "session.project.set"
            | "session.reply"
            | "session.rename"
            | "session.input"
            | "settings.update"
            | "session.start"
            | "session.reopen"
            | "pr.unlink"
            | "pr.sync"
            | "pr.comment"
            | "session.paste_image"
            | "session.send_files"
            | "session.end"
            | "session.resize"
            | "session.repaint"
            | "grant.add"
            | "grant.remove"
            | "publication.share"
            | "publication.unshare"
            | "publication.status"
            | "agent.message"
            | "agent.status"
            | "passkey.enroll_start"
            | "passkey.pending"
            | "passkey.approve"
            | "passkey.vouch"
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchArgs {
    claude_skip_permissions: bool,
    opencode_auto: bool,
    #[serde(deserialize_with = "required")]
    codex_sandbox_mode: Option<String>,
    #[serde(deserialize_with = "required")]
    codex_approval_policy: Option<String>,
    default_launcher: AgentLauncher,
    adk_statusline: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsUpdateArgs {
    launch: LaunchArgs,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasskeyEnrollStartArgs {
    label: String,
    kind: String,
    grants: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasskeyApproveArgs {
    cred_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasskeyVouchArgs {
    for_host: String,
    challenge: String,
    cred_id: String,
}

/// GitHub account or organization login: 1-39 of `[A-Za-z0-9-]`, no leading or
/// trailing hyphen. Anything else could be read as a gh option or a path.
pub fn valid_gh_owner(value: &str) -> bool {
    (1..=39).contains(&value.len())
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Repository name: 1-100 of `[A-Za-z0-9._-]`, never `.`/`..`, no leading hyphen.
pub fn valid_gh_repo(value: &str) -> bool {
    (1..=100).contains(&value.len())
        && value != "."
        && value != ".."
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn valid_pr_target(owner: &str, repo: &str, number: i64) -> bool {
    valid_gh_owner(owner) && valid_gh_repo(repo) && (1..=i64::from(i32::MAX)).contains(&number)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepaintArgs {
    session: String,
}

/// A stream channel id: random, chosen by the signer, 16–64 hex characters.
fn valid_channel(ch: &str) -> bool {
    (16..=64).contains(&ch.len()) && ch.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A host or credential id as the wire carries it: base64url, 43 characters
/// (SHA-256 sized).
fn is_id43(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// A ceremony id the host itself chose: 32 lowercase hex characters.
fn is_hex32(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The id of the agent message signed by `kid` as operation `jti`. Each signer
/// has its own id space, so another key reusing a `jti` names another message.
/// The sender computes it from its own key; the recipient from the verified
/// envelope. Neither takes it from the relay.
pub fn agent_message_id(kid: &str, jti: &str) -> String {
    let mut input = b"conversations-agent-message-id/v1\n".to_vec();
    for part in [kid, jti] {
        input.extend_from_slice(&(part.len() as u32).to_be_bytes());
        input.extend_from_slice(part.as_bytes());
    }
    digest::digest(&digest::SHA256, &input).as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// base64url(SHA-256(public key bytes exactly as enrolled)).
pub fn key_id(public_key: &[u8]) -> String {
    B64.encode(digest::digest(&digest::SHA256, public_key))
}

pub fn valid_key_id(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_jti(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Canonical lowercase hyphenated UUID only: the session id becomes a path
/// component, a process argument and a database key.
pub fn valid_session_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.hyphenated().to_string() == value)
}

#[cfg(test)]
pub fn verify(
    frame: &SignedFrame,
    alg: Alg,
    public_key: &[u8],
    host_id: &str,
    now: i64,
) -> Result<Verified, Reject> {
    verify_as(frame, alg, public_key, host_id, now, None)
}

/// Size, key id and signature over `DOMAIN_OP || payload`.
fn check_signature(frame: &SignedFrame, alg: Alg, public_key: &[u8]) -> Result<(), Reject> {
    if frame.payload.len() > MAX_PAYLOAD_BYTES || frame.sig.len() != SIGNATURE_BYTES {
        return Err(Reject::Malformed("size".into()));
    }
    if frame.kid != key_id(public_key) {
        return Err(Reject::KeyMismatch);
    }
    let mut signed = Vec::with_capacity(DOMAIN_OP.len() + frame.payload.len());
    signed.extend_from_slice(DOMAIN_OP);
    signed.extend_from_slice(&frame.payload);
    let algorithm: &dyn signature::VerificationAlgorithm = match alg {
        Alg::Es256 => &signature::ECDSA_P256_SHA256_FIXED,
        Alg::Ed25519 => &signature::ED25519,
    };
    signature::UnparsedPublicKey::new(algorithm, public_key)
        .verify(&signed, &frame.sig)
        .map_err(|_| Reject::BadSignature)
}

/// A verified private channel offer (`peer.offer`). It authorizes nothing by
/// itself: it names the challenge it consumes and the exact SDP and DTLS
/// fingerprint the browser will use, so the host can pin them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerOffer {
    pub kid: String,
    pub dlg: Option<String>,
    pub payload_sha256: Vec<u8>,
    pub challenge: String,
    /// base64url SHA-256 of the offer SDP.
    pub sdp_sha256: String,
    /// `sha-256 AB:…`, the browser's DTLS certificate fingerprint.
    pub fingerprint: String,
}

/// Verify a `peer.offer` envelope like any operation (signature, audience, key or
/// delegation, lifetime, strict fields), scoped to the host. It is not an
/// [`Operation`]: [`verify_as`] refuses the name, so an offer can never be
/// admitted as one.
pub fn verify_peer_offer(
    frame: &SignedFrame,
    alg: Alg,
    public_key: &[u8],
    host_id: &str,
    now: i64,
    expected_dlg: Option<&str>,
) -> Result<PeerOffer, Reject> {
    check_signature(frame, alg, public_key)?;
    let peek: Peek = serde_json::from_slice(&frame.payload)
        .map_err(|error| Reject::Malformed(error.to_string()))?;
    if peek.op != "peer.offer" {
        return Err(Reject::UnknownOperation);
    }
    let envelope: Envelope<PeerOfferArgs> = Expect {
        frame,
        host_id,
        now,
        dlg: expected_dlg,
    }
    .envelope()?;
    check_host_scope(&envelope.sess)?;
    let a = &envelope.args;
    if !valid_jti(&a.challenge) {
        return Err(Reject::BadIdentifier("challenge"));
    }
    if a.sdp_sha256.len() != 43
        || !a
            .sdp_sha256
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Reject::Malformed("sdp digest".into()));
    }
    if !conversations_core::private_channel::valid_fingerprint(&a.fingerprint) {
        return Err(Reject::Malformed("fingerprint".into()));
    }
    Ok(PeerOffer {
        kid: envelope.kid,
        dlg: envelope.dlg,
        payload_sha256: digest::digest(&digest::SHA256, &frame.payload)
            .as_ref()
            .to_vec(),
        challenge: envelope.args.challenge,
        sdp_sha256: envelope.args.sdp_sha256,
        fingerprint: envelope.args.fingerprint,
    })
}

/// `expected_dlg` is the delegation the caller looked the key up under.
pub fn verify_as(
    frame: &SignedFrame,
    alg: Alg,
    public_key: &[u8],
    host_id: &str,
    now: i64,
    expected_dlg: Option<&str>,
) -> Result<Verified, Reject> {
    check_signature(frame, alg, public_key)?;
    let peek: Peek = serde_json::from_slice(&frame.payload)
        .map_err(|error| Reject::Malformed(error.to_string()))?;
    let expect = Expect {
        frame,
        host_id,
        now,
        dlg: expected_dlg,
    };
    match peek.op.as_str() {
        "migration.export" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                session: String,
                transfer: String,
                offset: u64,
            }
            let envelope: Envelope<Args> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !super::super::migration::valid_transfer(&envelope.args.transfer)
                || envelope.args.offset > conversations_core::migration::MAX_SNAPSHOT_BYTES as u64
            {
                return Err(Reject::Malformed("migration".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::MigrationExport {
                session: a.session,
                transfer: a.transfer,
                offset: a.offset,
            }))
        }
        "migration.import" => {
            let envelope: Envelope<conversations_core::migration::MigrationImport> =
                expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            let a = &envelope.args;
            if !super::super::migration::valid_transfer(&a.transfer)
                || !valid_session_id(&a.destination)
                || !super::super::migration::valid_manifest(&a.manifest)
                || a.cwd.len() > 4096
                || a.cwd.contains('\0')
                || a.b64.len()
                    > conversations_core::migration::MIGRATION_CHUNK_BYTES.div_ceil(3) * 4
            {
                return Err(Reject::Malformed("migration".into()));
            }
            Ok(verified(envelope, frame, |input| {
                Operation::MigrationImport { input }
            }))
        }
        "session.start" => {
            let envelope: Envelope<StartArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            // Remote start is for coding agents only; a bare shell is not offered.
            if envelope.args.agent == AgentKind::Zsh {
                return Err(Reject::Malformed("agent".into()));
            }
            if let Some(launcher) = envelope.args.launcher {
                if !launcher.supports(envelope.args.agent) {
                    return Err(Reject::Malformed("launcher".into()));
                }
            }
            Ok(verified(envelope, frame, |a| Operation::SessionStart {
                session: a.session,
                agent: a.agent,
                launcher: a.launcher,
            }))
        }
        "session.reopen" => {
            let envelope: Envelope<ReopenArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            Ok(verified(envelope, frame, |a| Operation::SessionReopen {
                session: a.session,
            }))
        }
        "grant.add" | "grant.remove" => {
            let remove = peek.op == "grant.remove";
            let envelope: Envelope<GrantAddArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !valid_key_id(&envelope.args.recipient) {
                return Err(Reject::BadIdentifier("recipient"));
            }
            if envelope.args.caps.is_empty() || envelope.args.caps.len() > MAX_GRANT_CAPS {
                return Err(Reject::Malformed("caps".into()));
            }
            let mut caps = Vec::new();
            for name in &envelope.args.caps {
                let cap = Cap::parse(name).ok_or_else(|| Reject::Malformed("caps".into()))?;
                // Sorted, duplicate-free: one exact meaning per byte string.
                if !cap.grantable()
                    || caps
                        .last()
                        .is_some_and(|last: &Cap| last.as_str() >= cap.as_str())
                {
                    return Err(Reject::Malformed("caps".into()));
                }
                caps.push(cap);
            }
            Ok(verified(envelope, frame, |a| {
                if remove {
                    Operation::GrantRemove {
                        session: a.session,
                        recipient: a.recipient,
                        caps,
                    }
                } else {
                    Operation::GrantAdd {
                        session: a.session,
                        recipient: a.recipient,
                        caps,
                    }
                }
            }))
        }
        "session.input" => {
            let envelope: Envelope<InputArgs> = expect.envelope()?;
            check_stream(&envelope)?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !valid_channel(&envelope.args.ch) {
                return Err(Reject::BadIdentifier("channel"));
            }
            if envelope.args.b64.len() > MAX_INPUT_BYTES.div_ceil(3) * 4 {
                return Err(Reject::Malformed("input".into()));
            }
            let data = base64::engine::general_purpose::STANDARD
                .decode(&envelope.args.b64)
                .map_err(|_| Reject::Malformed("input".into()))?;
            if data.is_empty() || data.len() > MAX_INPUT_BYTES {
                return Err(Reject::Malformed("input".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::SessionInput {
                session: a.session,
                ch: a.ch,
                seq: a.seq,
                data,
            }))
        }
        "communications.list" => {
            let envelope: Envelope<conversations_core::private_channel::CommunicationsListArgs> =
                expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !(1..=100).contains(&envelope.args.limit.unwrap_or(32))
                || envelope
                    .args
                    .after
                    .as_ref()
                    .is_some_and(|value| value.len() > 80)
            {
                return Err(Reject::Malformed("communications cursor or limit".into()));
            }
            Ok(verified(envelope, frame, |a| {
                Operation::CommunicationsList {
                    session: a.session,
                    after: a.after,
                    limit: a.limit.unwrap_or(32),
                }
            }))
        }
        "attention.recipients" => {
            let envelope: Envelope<conversations_core::attention::SessionArgs> =
                expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            Ok(verified(envelope, frame, |a| {
                Operation::AttentionRecipients { session: a.session }
            }))
        }
        "notifications.list" => {
            let envelope: Envelope<conversations_core::attention::ListArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            if !(1..=50).contains(&envelope.args.limit) {
                return Err(Reject::Malformed("limit".into()));
            }
            Ok(verified(envelope, frame, |a| {
                Operation::NotificationsList {
                    after: a.after,
                    limit: a.limit,
                }
            }))
        }
        "notifications.read" | "notifications.resolve" => {
            let envelope: Envelope<conversations_core::attention::MarkArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !valid_session_id(&envelope.args.id) {
                return Err(Reject::Malformed("notification id".into()));
            }
            let resolve = envelope.op == "notifications.resolve";
            Ok(verified(envelope, frame, |a| Operation::NotificationMark {
                session: a.session,
                id: a.id,
                resolve,
            }))
        }
        "attention.request" => {
            let envelope: Envelope<conversations_core::attention::RequestArgs> =
                expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !valid_session_id(&envelope.args.idempotency_key)
                || envelope.args.message.is_empty()
                || envelope.args.message.chars().count() > 2000
                || envelope.args.message.contains('\0')
            {
                return Err(Reject::Malformed("attention request".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::AttentionRequest {
                session: a.session,
                input: conversations_core::attention::AttentionRequest {
                    recipient: a.recipient,
                    message: a.message,
                    idempotency_key: a.idempotency_key,
                },
            }))
        }
        "linear.list" => {
            let envelope: Envelope<LinearListArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !(1..=100).contains(&envelope.args.limit) {
                return Err(Reject::Malformed("limit".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::LinearList {
                session: a.session,
                after: a.after,
                limit: a.limit,
            }))
        }
        "linear.unlink" => {
            let envelope: Envelope<LinearUnlinkArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            let url = conversations_core::linear::parse_linear_url(&envelope.args.url)
                .ok_or_else(|| Reject::Malformed("Linear URL".into()))?
                .url;
            Ok(verified(envelope, frame, |a| Operation::LinearUnlink {
                session: a.session,
                url,
            }))
        }
        "access.list" => {
            let envelope: Envelope<NoArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            Ok(verified(envelope, frame, |_| Operation::AccessList))
        }
        "projects.list" => {
            let envelope: Envelope<NoArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            Ok(verified(envelope, frame, |_| Operation::ProjectsList))
        }
        "projects.create" => {
            let envelope: Envelope<ProjectCreateArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            if super::super::history::validate_title(&envelope.args.name)
                .ok()
                .flatten()
                .is_none()
                || envelope
                    .args
                    .description
                    .as_ref()
                    .is_some_and(|s| s.len() > 4096)
            {
                return Err(Reject::Malformed("project".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::ProjectCreate {
                name: a.name,
                description: a.description,
            }))
        }
        "session.project.get" => {
            let envelope: Envelope<SessionProjectArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            Ok(verified(envelope, frame, |a| {
                Operation::SessionProjectGet { session: a.session }
            }))
        }
        "session.project.set" => {
            let envelope: Envelope<SessionProjectSetArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if envelope
                .args
                .project_id
                .as_ref()
                .is_some_and(|id| !valid_session_id(id))
            {
                return Err(Reject::Malformed("project id".into()));
            }
            Ok(verified(envelope, frame, |a| {
                Operation::SessionProjectSet {
                    session: a.session,
                    project_id: a.project_id,
                }
            }))
        }
        "session.rename" => {
            let envelope: Envelope<SessionRenameArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            super::super::history::validate_title(&envelope.args.title)
                .map_err(|_| Reject::Malformed("title".into()))?;
            Ok(verified(envelope, frame, |a| Operation::SessionRename {
                session: a.session,
                title: a.title,
            }))
        }
        "session.reply" => {
            let envelope: Envelope<ReplyArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !valid_text(&envelope.args.text, MAX_REPLY_TEXT_BYTES) {
                return Err(Reject::Malformed("text".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::SessionReply {
                session: a.session,
                text: a.text,
            }))
        }
        "agent.status" => {
            let envelope: Envelope<AgentStatusArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !valid_jti(&envelope.args.message) {
                return Err(Reject::BadIdentifier("message id"));
            }
            Ok(verified(envelope, frame, |a| Operation::AgentStatus {
                session: a.session,
                message: a.message,
            }))
        }
        "agent.message" => {
            let envelope: Envelope<AgentMessageArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            let a = &envelope.args;
            if !valid_session_id(&a.source)
                || !valid_key_id(&a.source_host)
                || a.reply_to.as_deref().is_some_and(|r| !valid_jti(r))
            {
                return Err(Reject::BadIdentifier("source or reply"));
            }
            if !valid_text(&a.text, MAX_REPLY_TEXT_BYTES) {
                return Err(Reject::Malformed("text".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::AgentMessage {
                session: a.session,
                source: a.source,
                source_host: a.source_host,
                text: a.text,
                reply_to: a.reply_to,
            }))
        }
        "session.resize" => {
            let envelope: Envelope<ResizeArgs> = expect.envelope()?;
            check_stream(&envelope)?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !valid_channel(&envelope.args.ch) {
                return Err(Reject::BadIdentifier("channel"));
            }
            let (cols, rows) = (envelope.args.cols, envelope.args.rows);
            if !(1..=1000).contains(&cols) || !(1..=1000).contains(&rows) {
                return Err(Reject::Malformed("size".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::SessionResize {
                session: a.session,
                ch: a.ch,
                seq: a.seq,
                cols: a.cols,
                rows: a.rows,
            }))
        }
        "session.repaint" => {
            let envelope: Envelope<RepaintArgs> = expect.envelope()?;
            check_stream(&envelope)?;
            check_session(&envelope.sess, &envelope.args.session)?;
            Ok(verified(envelope, frame, |a| Operation::SessionRepaint {
                session: a.session,
            }))
        }
        "session.paste_image" => {
            use conversations_core::terminal_files::{IMAGE_MIMES, MAX_IMAGE_BYTES};
            let envelope: Envelope<PasteImageArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            let a = &envelope.args;
            if !valid_channel(&a.transfer) {
                return Err(Reject::BadIdentifier("transfer"));
            }
            if !IMAGE_MIMES.contains(&a.mime.as_str())
                || a.size == 0
                || a.size > MAX_IMAGE_BYTES as u64
                || !valid_sha256(&a.sha256)
            {
                return Err(Reject::Malformed("image".into()));
            }
            Ok(verified(envelope, frame, |a| {
                Operation::SessionPasteImage {
                    session: a.session,
                    transfer: a.transfer,
                    mime: a.mime,
                    size: a.size,
                    sha256: a.sha256,
                }
            }))
        }
        "session.send_files" => {
            use conversations_core::terminal_files::{
                validate_file_name, MAX_FILES, MAX_TOTAL_BYTES,
            };
            let envelope: Envelope<SendFilesArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            let a = &envelope.args;
            if !valid_channel(&a.transfer) {
                return Err(Reject::BadIdentifier("transfer"));
            }
            if a.files.is_empty() || a.files.len() > MAX_FILES {
                return Err(Reject::Malformed("files".into()));
            }
            let mut total = 0u64;
            for file in &a.files {
                if validate_file_name(&file.name).is_err()
                    || file.name.contains(['\u{2028}', '\u{2029}'])
                    || !valid_sha256(&file.sha256)
                {
                    return Err(Reject::Malformed("file".into()));
                }
                total = total.saturating_add(file.size);
            }
            if total > MAX_TOTAL_BYTES as u64 {
                return Err(Reject::Malformed("files".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::SessionSendFiles {
                session: a.session,
                transfer: a.transfer,
                files: a
                    .files
                    .into_iter()
                    .map(|f| FileSpec {
                        name: f.name,
                        size: f.size,
                        sha256: f.sha256,
                    })
                    .collect(),
            }))
        }
        "pr.unlink" => {
            let envelope: Envelope<PrSyncArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            let a = &envelope.args;
            if !valid_pr_target(&a.owner, &a.repo, a.number) {
                return Err(Reject::Malformed("pull request".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::PrUnlink {
                session: a.session,
                owner: a.owner,
                repo: a.repo,
                number: a.number,
            }))
        }
        "pr.sync" => {
            let envelope: Envelope<PrSyncArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            let a = &envelope.args;
            if !valid_pr_target(&a.owner, &a.repo, a.number) {
                return Err(Reject::Malformed("pull request".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::PrSync {
                session: a.session,
                owner: a.owner,
                repo: a.repo,
                number: a.number,
            }))
        }
        "pr.comment" => {
            let envelope: Envelope<PrCommentArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            let a = &envelope.args;
            if !valid_pr_target(&a.owner, &a.repo, a.number)
                || a.in_reply_to.is_some_and(|id| id < 1)
            {
                return Err(Reject::Malformed("pull request".into()));
            }
            if !valid_text(&a.body, MAX_PR_BODY_BYTES) {
                return Err(Reject::Malformed("body".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::PrComment {
                session: a.session,
                owner: a.owner,
                repo: a.repo,
                number: a.number,
                body: a.body,
                in_reply_to: a.in_reply_to,
            }))
        }
        "publication.share" => {
            let envelope: Envelope<PublicationShareArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            let audience =
                crate::commands::local_host::publication::normalize(envelope.args.audience.clone())
                    .map_err(|_| Reject::Malformed("audience".into()))?;
            // One exact meaning per byte string: the signed list is canonical.
            if audience != envelope.args.audience {
                return Err(Reject::Malformed("audience".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::PublicationShare {
                session: a.session,
                audience,
            }))
        }
        "publication.unshare" | "publication.status" => {
            let envelope: Envelope<ReopenArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            let unshare = peek.op == "publication.unshare";
            Ok(verified(envelope, frame, |a| {
                if unshare {
                    Operation::PublicationUnshare { session: a.session }
                } else {
                    Operation::PublicationStatus { session: a.session }
                }
            }))
        }
        "history.get" => {
            use conversations_core::private_channel::MAX_HISTORY_PAGE;
            let envelope: Envelope<HistoryGetArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            let most = if envelope.args.digest {
                conversations_core::private_channel::MAX_DIGEST_PAGE
            } else {
                MAX_HISTORY_PAGE
            };
            if !(1..=most).contains(&envelope.args.limit) {
                return Err(Reject::Malformed("limit".into()));
            }
            // Stored sequence numbers are SQLite integers: a cursor past them is invalid.
            if i64::try_from(envelope.args.after).is_err() {
                return Err(Reject::Malformed("cursor".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::HistoryGet {
                session: a.session,
                after: a.after,
                limit: a.limit,
                digest: a.digest,
            }))
        }
        "pr.cache.list" => {
            let envelope: Envelope<conversations_core::private_channel::PrCacheArgs> =
                expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if i64::try_from(envelope.args.offset).is_err()
                || envelope
                    .args
                    .sha256
                    .as_ref()
                    .is_some_and(|s| !valid_key_id(s))
            {
                return Err(Reject::Malformed("cache cursor".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::PrCacheList {
                session: a.session,
                offset: a.offset,
                sha256: a.sha256,
            }))
        }
        "history.fragment" => {
            let envelope: Envelope<HistoryFragmentArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if i64::try_from(envelope.args.seq).is_err()
                || i64::try_from(envelope.args.offset).is_err()
            {
                return Err(Reject::Malformed("cursor".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::HistoryFragment {
                session: a.session,
                seq: a.seq,
                offset: a.offset,
            }))
        }
        "terminal.subscribe" => {
            let envelope: Envelope<TerminalSubscribeArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            Ok(verified(envelope, frame, |a| {
                Operation::TerminalSubscribe { session: a.session }
            }))
        }
        "sessions.list" => {
            use conversations_core::private_channel::MAX_SESSION_PAGE;
            let envelope: Envelope<SessionsListArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            if !(1..=MAX_SESSION_PAGE).contains(&envelope.args.limit) {
                return Err(Reject::Malformed("limit".into()));
            }
            if let Some(after) = &envelope.args.after {
                if !valid_list_cursor(after) {
                    return Err(Reject::Malformed("cursor".into()));
                }
            }
            Ok(verified(envelope, frame, |a| Operation::SessionsList {
                after: a.after,
                limit: a.limit,
                running_only: a.running_only,
                readiness: a.readiness,
            }))
        }
        "history.search" => {
            use conversations_core::private_channel::{MAX_SEARCH_HITS, MAX_SEARCH_QUERY};
            let envelope: Envelope<HistorySearchArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            if !(1..=MAX_SEARCH_HITS).contains(&envelope.args.limit) {
                return Err(Reject::Malformed("limit".into()));
            }
            if !valid_text(&envelope.args.query, MAX_SEARCH_QUERY) {
                return Err(Reject::Malformed("query".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::HistorySearch {
                session: a.session,
                query: a.query,
                limit: a.limit,
            }))
        }
        "settings.get" => {
            let envelope: Envelope<NoArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            Ok(verified(envelope, frame, |_| Operation::SettingsGet))
        }
        "passkey.enroll_start" => {
            let envelope: Envelope<PasskeyEnrollStartArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            // Shape only here: label/kind/grants semantics are the host's
            // enroll-start rules, enforced when the ceremony starts.
            Ok(verified(envelope, frame, |a| {
                Operation::PasskeyEnrollStart {
                    label: a.label,
                    kind: a.kind,
                    grants: a.grants,
                }
            }))
        }
        "passkey.pending" => {
            let envelope: Envelope<NoArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            Ok(verified(envelope, frame, |_| Operation::PasskeyPending))
        }
        "passkey.approve" => {
            let envelope: Envelope<PasskeyApproveArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            Ok(verified(envelope, frame, |a| Operation::PasskeyApprove {
                cred_id: a.cred_id,
            }))
        }
        "passkey.vouch" => {
            let envelope: Envelope<PasskeyVouchArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            let args = &envelope.args;
            // Bound the signing oracle: only a real host id, a real ceremony
            // id and a real passkey id shape are ever signed, so the vouch
            // can never become a signature over attacker-chosen text.
            if !(is_id43(&args.for_host) && is_id43(&args.cred_id) && is_hex32(&args.challenge)) {
                return Err(Reject::Malformed("vouch ids".into()));
            }
            Ok(verified(envelope, frame, |a| Operation::PasskeyVouch {
                for_host: a.for_host,
                challenge: a.challenge,
                cred_id: a.cred_id,
            }))
        }
        "settings.update" => {
            let envelope: Envelope<SettingsUpdateArgs> = expect.envelope()?;
            check_host_scope(&envelope.sess)?;
            let l = &envelope.args.launch;
            for (value, allowed) in [
                (
                    &l.codex_sandbox_mode,
                    crate::commands::local_host::settings::CODEX_SANDBOX_MODES,
                ),
                (
                    &l.codex_approval_policy,
                    crate::commands::local_host::settings::CODEX_APPROVAL_POLICIES,
                ),
            ] {
                if value.as_deref().is_some_and(|v| !allowed.contains(&v)) {
                    return Err(Reject::Malformed("codex setting".into()));
                }
            }
            Ok(verified(envelope, frame, |a| Operation::SettingsUpdate {
                launch: LaunchUpdate {
                    claude_skip_permissions: a.launch.claude_skip_permissions,
                    opencode_auto: a.launch.opencode_auto,
                    codex_sandbox_mode: a.launch.codex_sandbox_mode,
                    codex_approval_policy: a.launch.codex_approval_policy,
                    default_launcher: a.launch.default_launcher,
                    adk_statusline: a.launch.adk_statusline,
                },
            }))
        }
        "session.end" => {
            let envelope: Envelope<ReopenArgs> = expect.envelope()?;
            check_session(&envelope.sess, &envelope.args.session)?;
            Ok(verified(envelope, frame, |a| Operation::SessionEnd {
                session: a.session,
            }))
        }
        _ => Err(Reject::UnknownOperation),
    }
}

/// What every operation's envelope must match once its signature verified.
struct Expect<'a> {
    frame: &'a SignedFrame,
    host_id: &'a str,
    now: i64,
    dlg: Option<&'a str>,
}

impl Expect<'_> {
    /// Strictly parse the envelope with its operation's argument type, then
    /// apply the checks every operation shares.
    fn envelope<A: serde::de::DeserializeOwned>(&self) -> Result<Envelope<A>, Reject> {
        let envelope = serde_json::from_slice(&self.frame.payload)
            .map_err(|error| Reject::Malformed(error.to_string()))?;
        check_common(&envelope, self)?;
        Ok(envelope)
    }
}

fn check_session(sess: &str, arg: &str) -> Result<(), Reject> {
    if !valid_session_id(sess) || !valid_session_id(arg) {
        return Err(Reject::BadIdentifier("session id"));
    }
    if sess != arg {
        return Err(Reject::SessionMismatch);
    }
    Ok(())
}

/// A host-scoped operation names no session: `sess` is the literal host marker
/// and the arguments cannot carry a session at all.
fn check_host_scope(sess: &str) -> Result<(), Reject> {
    if sess != HOST_SCOPE {
        return Err(Reject::SessionMismatch);
    }
    Ok(())
}

/// Stream frames live only seconds: replay is bounded by lifetime, not a journal.
fn check_stream<A>(envelope: &Envelope<A>) -> Result<(), Reject> {
    if envelope.exp - envelope.iat > STREAM_LIFETIME_SECS {
        return Err(Reject::InvalidLifetime);
    }
    Ok(())
}

fn verified<A>(
    envelope: Envelope<A>,
    frame: &SignedFrame,
    op: impl FnOnce(A) -> Operation,
) -> Verified {
    Verified {
        kid: envelope.kid,
        dlg: envelope.dlg,
        jti: envelope.jti,
        iat: envelope.iat,
        exp: envelope.exp,
        payload_sha256: digest::digest(&digest::SHA256, &frame.payload)
            .as_ref()
            .to_vec(),
        op: op(envelope.args),
        grant_epoch: 0,
        admitted_under: None,
    }
}

/// `<created_at>:<session id>`, as a listing page's `next` names its last entry.
pub fn valid_list_cursor(value: &str) -> bool {
    value
        .split_once(':')
        .is_some_and(|(at, id)| at.parse::<i64>().is_ok() && valid_session_id(id))
}

fn check_common<A>(envelope: &Envelope<A>, expect: &Expect) -> Result<(), Reject> {
    let Expect {
        frame,
        host_id,
        now,
        dlg: expected_dlg,
    } = *expect;
    if envelope.dlg.as_deref() != expected_dlg {
        return Err(Reject::KeyMismatch);
    }
    if envelope.dlg.as_deref().is_some_and(|dlg| !valid_jti(dlg)) {
        return Err(Reject::BadIdentifier("delegation id"));
    }
    if envelope.v != 1 || envelope.typ != "op" {
        return Err(Reject::Malformed("version or type".into()));
    }
    if envelope.aud != host_id {
        return Err(Reject::WrongAudience);
    }
    if envelope.kid != frame.kid {
        return Err(Reject::KeyMismatch);
    }
    if !valid_jti(&envelope.jti) {
        return Err(Reject::BadIdentifier("operation id"));
    }
    let lifetime = envelope
        .exp
        .checked_sub(envelope.iat)
        .ok_or(Reject::InvalidLifetime)?;
    if !(0..=MAX_LIFETIME_SECS).contains(&lifetime) {
        return Err(Reject::InvalidLifetime);
    }
    if now > envelope.exp {
        return Err(Reject::Expired);
    }
    let horizon = now.checked_add(SKEW_SECS).ok_or(Reject::NotYetValid)?;
    if envelope.iat > horizon {
        return Err(Reject::NotYetValid);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_signing {
    //! Deterministic signing helper for tests: a real Ed25519 owner/agent key.
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    pub struct Signer {
        pair: Ed25519KeyPair,
        pub public: Vec<u8>,
        pub kid: String,
    }

    impl Signer {
        pub fn new() -> Self {
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
            let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
            let public = pair.public_key().as_ref().to_vec();
            let kid = key_id(&public);
            Self { pair, public, kid }
        }

        pub fn sign_bytes(&self, payload: &[u8]) -> SignedFrame {
            let mut signed = DOMAIN_OP.to_vec();
            signed.extend_from_slice(payload);
            SignedFrame {
                payload: payload.to_vec(),
                sig: self.pair.sign(&signed).as_ref().to_vec(),
                kid: self.kid.clone(),
            }
        }

        pub fn reopen_payload(
            &self,
            host_id: &str,
            session: &str,
            jti: &str,
            iat: i64,
            exp: i64,
        ) -> String {
            format!(
                r#"{{"v":1,"typ":"op","aud":"{host_id}","kid":"{}","dlg":null,"op":"session.reopen","sess":"{session}","jti":"{jti}","iat":{iat},"exp":{exp},"args":{{"session":"{session}"}}}}"#,
                self.kid
            )
        }

        /// Any operation: `args` is the raw JSON object text for `args`.
        #[allow(clippy::too_many_arguments)]
        pub fn op_payload(
            &self,
            host_id: &str,
            op: &str,
            session: &str,
            args: &str,
            jti: &str,
            iat: i64,
            exp: i64,
        ) -> String {
            format!(
                r#"{{"v":1,"typ":"op","aud":"{host_id}","kid":"{}","dlg":null,"op":"{op}","sess":"{session}","jti":"{jti}","iat":{iat},"exp":{exp},"args":{args}}}"#,
                self.kid
            )
        }

        pub fn start_payload(
            &self,
            host_id: &str,
            session: &str,
            agent: &str,
            jti: &str,
            iat: i64,
            exp: i64,
        ) -> String {
            format!(
                r#"{{"v":1,"typ":"op","aud":"{host_id}","kid":"{}","dlg":null,"op":"session.start","sess":"{session}","jti":"{jti}","iat":{iat},"exp":{exp},"args":{{"session":"{session}","agent":"{agent}"}}}}"#,
                self.kid
            )
        }
    }

    /// The SSE `data:` payload carrying a signed frame.
    #[allow(dead_code)]
    pub fn wire(frame: &SignedFrame) -> String {
        serde_json::json!({
            "type": "signed_op",
            "p": B64.encode(&frame.payload),
            "s": B64.encode(&frame.sig),
            "k": frame.kid,
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::test_signing::*;
    use super::*;

    const HOST: &str = "HOSTID";
    const SESSION: &str = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";
    const JTI: &str = "0123456789abcdef0123456789abcdef";
    const NOW: i64 = 1_760_000_010;

    fn check(signer: &Signer, payload: &str) -> Result<Verified, Reject> {
        verify(
            &signer.sign_bytes(payload.as_bytes()),
            Alg::Ed25519,
            &signer.public,
            HOST,
            NOW,
        )
    }

    fn good(signer: &Signer) -> String {
        signer.start_payload(HOST, SESSION, "claude", JTI, 1_760_000_000, 1_760_000_060)
    }

    #[test]
    fn valid_start_binds_session_agent_and_cap() {
        let signer = Signer::new();
        let verified = check(&signer, &good(&signer)).unwrap();
        assert_eq!(verified.op.capability(), Cap::SessionStart);
        assert_eq!(verified.op.session_id(), SESSION);
        assert_eq!(
            verified.op,
            Operation::SessionStart {
                session: SESSION.into(),
                agent: AgentKind::Claude,
                launcher: None
            }
        );
    }

    #[test]
    fn lifetime_arithmetic_never_panics_and_rejects_extremes() {
        let signer = Signer::new();
        for (iat, exp) in [
            (i64::MIN, i64::MAX),
            (i64::MAX, i64::MIN),
            (0, i64::MAX),
            (i64::MAX, i64::MAX),
            (1_760_000_060, 1_760_000_000), // exp < iat
            (1_760_000_000, 1_760_000_121), // lifetime too long
            (-5, 100),
        ] {
            let payload = signer.start_payload(HOST, SESSION, "claude", JTI, iat, exp);
            let result = check(&signer, &payload);
            assert!(
                matches!(
                    result,
                    Err(Reject::InvalidLifetime | Reject::Expired | Reject::NotYetValid)
                ),
                "{iat} {exp}: {result:?}"
            );
        }
        // overflowing `now + skew` must not wrap into acceptance
        let payload = signer.start_payload(HOST, SESSION, "claude", JTI, i64::MAX - 1, i64::MAX);
        assert_eq!(
            verify(
                &signer.sign_bytes(payload.as_bytes()),
                Alg::Ed25519,
                &signer.public,
                HOST,
                i64::MAX - 10
            ),
            Err(Reject::NotYetValid)
        );
    }

    #[test]
    fn wrong_signer_audience_and_tampering_are_rejected() {
        let signer = Signer::new();
        let other = Signer::new();
        let payload = good(&signer);
        // payload signed by another key but presented under the first key id
        let mut frame = other.sign_bytes(payload.as_bytes());
        frame.kid = signer.kid.clone();
        assert_eq!(
            verify(&frame, Alg::Ed25519, &signer.public, HOST, NOW),
            Err(Reject::BadSignature)
        );
        assert_eq!(
            verify(
                &signer.sign_bytes(payload.as_bytes()),
                Alg::Ed25519,
                &signer.public,
                "OTHER",
                NOW
            ),
            Err(Reject::WrongAudience)
        );
        let mut frame = signer.sign_bytes(payload.as_bytes());
        frame.payload[10] ^= 1;
        assert_eq!(
            verify(&frame, Alg::Ed25519, &signer.public, HOST, NOW),
            Err(Reject::BadSignature)
        );
    }

    #[test]
    fn strict_parsing_rejects_unknown_duplicate_and_mismatched_fields() {
        let signer = Signer::new();
        let base = good(&signer);
        let malformed = |p: String| matches!(check(&signer, &p), Err(Reject::Malformed(_)));
        // unknown top-level and nested fields
        assert!(malformed(base.replacen(
            "\"args\":{",
            "\"admin\":true,\"args\":{",
            1
        )));
        assert!(malformed(base.replace(
            "\"agent\":\"claude\"",
            "\"agent\":\"claude\",\"cwd\":\"/\""
        )));
        // duplicate top-level and nested fields (serde would silently keep the last in a Value)
        assert!(malformed(base.replacen("\"v\":1", "\"v\":1,\"v\":1", 1)));
        assert!(malformed(base.replace(
            "\"agent\":\"claude\"",
            "\"agent\":\"claude\",\"agent\":\"codex\""
        )));
        // unknown agent / launcher pairing
        assert!(malformed(base.replace("\"claude\"", "\"rm\"")));
        assert!(malformed(base.replace(
            "\"agent\":\"claude\"",
            "\"agent\":\"claude\",\"launcher\":\"adk\""
        )));
        // remote start never offers a bare shell
        assert!(malformed(base.replace("\"claude\"", "\"zsh\"")));
        // session id confusion
        let other = "5e1aeb93-5aba-419e-8856-a7c721406279";
        let mismatched = base.replacen(
            &format!("\"sess\":\"{SESSION}\""),
            &format!("\"sess\":\"{other}\""),
            1,
        );
        assert_eq!(check(&signer, &mismatched), Err(Reject::SessionMismatch));
        let upper = base.replace(SESSION, &SESSION.to_uppercase());
        assert_eq!(
            check(&signer, &upper),
            Err(Reject::BadIdentifier("session id"))
        );
        // unknown operation fails closed
        let unknown = base.replace("session.start", "session.erase");
        assert_eq!(check(&signer, &unknown), Err(Reject::UnknownOperation));
        // bad nonce id
        assert_eq!(
            check(&signer, &base.replace(JTI, "short")),
            Err(Reject::BadIdentifier("operation id"))
        );
    }

    #[test]
    fn reopen_names_only_the_session_and_rejects_smuggled_execution_fields() {
        let signer = Signer::new();
        let good = signer.reopen_payload(HOST, SESSION, JTI, 1_760_000_000, 1_760_000_060);
        let verified = check(&signer, &good).unwrap();
        assert_eq!(verified.op.capability(), Cap::SessionReopen);
        assert_eq!(
            verified.op,
            Operation::SessionReopen {
                session: SESSION.into()
            }
        );
        let session_arg = format!("\"args\":{{\"session\":\"{SESSION}\"");
        for extra in [
            r#""cwd":"/tmp""#,
            r#""resume_id":"other""#,
            r#""agent_args":["--dangerously-skip-permissions"]"#,
            r#""launcher":"adk""#,
            r#""session":"5e1aeb93-5aba-419e-8856-a7c721406279""#, // duplicate
        ] {
            let tampered = good.replacen(&session_arg, &format!("{session_arg},{extra}"), 1);
            assert!(
                matches!(check(&signer, &tampered), Err(Reject::Malformed(_))),
                "{extra}"
            );
        }
        let other = "5e1aeb93-5aba-419e-8856-a7c721406279";
        let mismatched = good.replacen(
            &format!("\"sess\":\"{SESSION}\""),
            &format!("\"sess\":\"{other}\""),
            1,
        );
        assert_eq!(check(&signer, &mismatched), Err(Reject::SessionMismatch));
        assert_eq!(
            check(&signer, &good.replace("session.reopen", "session.reopenx")),
            Err(Reject::UnknownOperation)
        );
    }

    #[test]
    fn every_operation_binds_its_capability_and_enforces_the_shared_checks() {
        let signer = Signer::new();
        let other = "5e1aeb93-5aba-419e-8856-a7c721406279";
        let ch = "0123456789abcdef";
        let sha = "ab".repeat(32);
        let kid = &signer.kid;
        let s = SESSION;
        // (op, sess, args without braces, capability, stream)
        let cases = [
            ("grant.add", s, format!(r#""session":"{s}","recipient":"{kid}","caps":["history.read","session.write"]"#), Cap::SessionManageSharing, false),
            ("grant.remove", s, format!(r#""session":"{s}","recipient":"{kid}","caps":["session.write"]"#), Cap::SessionManageSharing, false),
            ("session.input", s, format!(r#""session":"{s}","ch":"{ch}","seq":1,"b64":"aGk=""#), Cap::SessionWrite, true),
            ("session.reply", s, format!(r#""session":"{s}","text":"hi""#), Cap::SessionWrite, false),
            ("agent.status", s, format!(r#""session":"{s}","message":"{JTI}""#), Cap::SessionWrite, false),
            ("agent.message", s, format!(r#""session":"{s}","source":"{other}","source_host":"{kid}","text":"hi","reply_to":null"#), Cap::SessionWrite, false),
            ("session.resize", s, format!(r#""session":"{s}","ch":"{ch}","seq":1,"cols":80,"rows":24"#), Cap::SessionWrite, true),
            ("session.repaint", s, format!(r#""session":"{s}""#), Cap::SessionWrite, true),
            ("session.paste_image", s, format!(r#""session":"{s}","transfer":"{ch}","mime":"image/png","size":10,"sha256":"{sha}""#), Cap::SessionWrite, false),
            ("session.send_files", s, format!(r#""session":"{s}","transfer":"{ch}","files":[{{"name":"a.txt","size":1,"sha256":"{sha}"}}]"#), Cap::SessionWrite, false),
            ("session.end", s, format!(r#""session":"{s}""#), Cap::SessionWrite, false),
            ("pr.sync", s, format!(r#""session":"{s}","owner":"octo","repo":"repo","number":7"#), Cap::SessionWrite, false),
            ("pr.comment", s, format!(r#""session":"{s}","owner":"octo","repo":"repo","number":7,"body":"hi","in_reply_to":null"#), Cap::PrComment, false),
            ("settings.get", HOST_SCOPE, String::new(), Cap::SettingsRead, false),
            ("history.get", s, format!(r#""session":"{s}","after":0,"limit":256,"digest":true"#), Cap::HistoryRead, false),
            ("history.fragment", s, format!(r#""session":"{s}","seq":3,"offset":24576"#), Cap::HistoryRead, false),
            ("sessions.list", HOST_SCOPE, r#""after":null,"limit":50"#.into(), Cap::HistoryRead, false),
            ("settings.update", HOST_SCOPE, r#""launch":{"claude_skip_permissions":false,"opencode_auto":false,"codex_sandbox_mode":null,"codex_approval_policy":null,"default_launcher":"adk","adk_statusline":false}"#.into(), Cap::SettingsWrite, false),
        ];
        for (op, sess, args, cap, stream) in cases {
            let payload = |sess: &str, args: &str, iat: i64, exp: i64| {
                signer.op_payload(HOST, op, sess, &format!("{{{args}}}"), JTI, iat, exp)
            };
            let good = payload(sess, &args, NOW - 5, NOW + 25);
            let verified = check(&signer, &good).unwrap_or_else(|e| panic!("{op}: {e}"));
            assert_eq!(
                (verified.op.capability(), verified.op.session_id()),
                (cap, sess),
                "{op}"
            );

            let sep = if args.is_empty() { "" } else { "," };
            let extra = payload(sess, &format!(r#"{args}{sep}"x":1"#), NOW - 5, NOW + 25);
            assert!(
                matches!(check(&signer, &extra), Err(Reject::Malformed(_))),
                "{op}"
            );
            let wrong_host = good.replacen(&format!(r#""aud":"{HOST}""#), r#""aud":"OTHER""#, 1);
            assert_eq!(
                check(&signer, &wrong_host),
                Err(Reject::WrongAudience),
                "{op}"
            );
            let expired = payload(sess, &args, NOW - 40, NOW - 1);
            assert_eq!(check(&signer, &expired), Err(Reject::Expired), "{op}");
            let mut altered = signer.sign_bytes(good.as_bytes());
            altered.payload[good.len() - 3] ^= 1;
            assert_eq!(
                verify(&altered, Alg::Ed25519, &signer.public, HOST, NOW),
                Err(Reject::BadSignature),
                "{op}"
            );
            // Only stream frames are bounded below the general lifetime.
            let long = check(&signer, &payload(sess, &args, NOW - 5, NOW + 26));
            assert_eq!(long.is_err(), stream, "{op}: {long:?}");
            // Host scope and session scope never stand in for each other.
            let swapped = if sess == HOST_SCOPE { s } else { HOST_SCOPE };
            let sess_field = format!(r#""sess":"{sess}""#);
            let rescoped = good.replacen(&sess_field, &format!(r#""sess":"{swapped}""#), 1);
            assert!(
                matches!(
                    check(&signer, &rescoped),
                    Err(Reject::SessionMismatch | Reject::BadIdentifier("session id"))
                ),
                "{op}"
            );
            if sess != HOST_SCOPE {
                let mismatched = good.replacen(&sess_field, &format!(r#""sess":"{other}""#), 1);
                assert_eq!(
                    check(&signer, &mismatched),
                    Err(Reject::SessionMismatch),
                    "{op}"
                );
            }
        }
    }

    #[test]
    fn wire_decoding_is_bounded() {
        let signer = Signer::new();
        let frame = signer.sign_bytes(good(&signer).as_bytes());
        let ok = SignedFrame::from_wire(
            &B64.encode(&frame.payload),
            &B64.encode(&frame.sig),
            &frame.kid,
        );
        assert_eq!(ok, Some(frame.clone()));
        assert!(SignedFrame::from_wire(
            &"A".repeat(MAX_B64_FIELD + 4),
            &B64.encode(&frame.sig),
            &frame.kid
        )
        .is_none());
        assert!(SignedFrame::from_wire(
            &B64.encode(&frame.payload),
            &B64.encode([0u8; 63]),
            &frame.kid
        )
        .is_none());
        assert!(SignedFrame::from_wire(
            &B64.encode(&frame.payload),
            &B64.encode(&frame.sig),
            "short"
        )
        .is_none());
        assert!(SignedFrame::from_wire(
            &B64.encode(vec![b'x'; MAX_PAYLOAD_BYTES + 1]),
            &B64.encode(&frame.sig),
            &frame.kid
        )
        .is_none());
    }
}
