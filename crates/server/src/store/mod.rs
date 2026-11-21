//! Storage abstraction.
//!
//! Handlers depend only on the [`Repository`] trait and the DB-agnostic DTOs in
//! this module, so a Postgres backend can be added later without touching the
//! API layer. The SQLite implementation lives in [`sqlite`].

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use conversations_core as core;
use conversations_core::AgentKind;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

pub mod messaging;
pub mod notifications;
mod nul;
pub mod postgres;
pub mod publication;
pub mod session;
pub mod sqlite;

pub use postgres::PostgresRepo;
pub use publication::SourceContext;
pub use session::SessionBackend;
pub use sqlite::SqliteRepo;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiveOrder {
    pub revision: i64,
    pub session_ids: Vec<String>,
}

/// Allowed session visibility values.
pub const VISIBILITY_ORGANIZATION: &str = "organization";
pub const VISIBILITY_RESTRICTED: &str = "restricted";

/// Parse an agent string from API/database storage. Unknown or missing values
/// fall back to Claude for compatibility with older rows and clients.
pub fn parse_agent(value: Option<&str>) -> AgentKind {
    match value {
        Some("codex") => AgentKind::Codex,
        Some("opencode") => AgentKind::OpenCode,
        Some("zsh") => AgentKind::Zsh,
        _ => AgentKind::Claude,
    }
}

/// Compute the (earliest, latest) timestamp across a message slice, for the
/// graduated session's `started_at`/`ended_at`.
pub(super) fn message_time_range(
    messages: &[core::Message],
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut latest: Option<DateTime<Utc>> = None;
    for m in messages {
        if let Some(ts) = m.timestamp {
            earliest = Some(earliest.map_or(ts, |e| e.min(ts)));
            latest = Some(latest.map_or(ts, |l| l.max(ts)));
        }
    }
    (earliest, latest)
}

/// Normalize a share-email list: trim, lowercase, drop empties, de-duplicate
/// (preserving order). Lowercasing keeps stored shares consistent with the
/// case-insensitive user-directory search.
pub(super) fn dedup_nonempty(emails: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for e in emails {
        let normalized = e.trim().to_lowercase();
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

/// Keep the first input for each nonempty, trimmed repo/branch pair.
fn distinct_repo_links(links: &[RepoLinkInput]) -> Vec<&RepoLinkInput> {
    let mut seen = std::collections::HashSet::new();
    links
        .iter()
        .filter(|link| {
            let repo = link.repo.trim();
            let branch = link.branch.trim();
            !repo.is_empty() && !branch.is_empty() && seen.insert((repo, branch))
        })
        .collect()
}

/// Validate a visibility string, returning a `BadRequest` error otherwise.
pub fn validate_visibility(visibility: &str) -> Result<()> {
    match visibility {
        VISIBILITY_ORGANIZATION | VISIBILITY_RESTRICTED => Ok(()),
        other => Err(AppError::BadRequest(format!(
            "invalid visibility {other:?}; expected 'organization' or 'restricted'"
        ))),
    }
}

/// Maximum number of users a single `search_users` call may return.
pub const USER_SEARCH_MAX_LIMIT: i64 = 50;

/// Construct a repository for `database_url`, selecting the backend from the URL
/// scheme: `postgres://` / `postgresql://` → [`PostgresRepo`], everything else
/// (`sqlite:` / `sqlite://`, the default) → [`SqliteRepo`]. Migrations are run
/// against the chosen backend before the repo is returned.
pub async fn connect(database_url: &str) -> Result<Arc<dyn Repository>> {
    if database_url.starts_with("postgres://") || database_url.starts_with("postgresql://") {
        let repo = PostgresRepo::connect(database_url).await?;
        Ok(Arc::new(repo))
    } else {
        let repo = SqliteRepo::connect(database_url).await?;
        Ok(Arc::new(repo))
    }
}

/// Outcome of an [`Repository::upsert_session`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpsertOutcome {
    /// `true` when the session id did not previously exist.
    pub created: bool,
    /// Number of messages stored for the session after the upsert.
    pub message_count: usize,
}

/// A session summary as stored, including upload metadata. Returned by listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSummary {
    pub id: String,
    #[serde(default)]
    pub agent: AgentKind,
    pub title: String,
    pub project_path: Option<String>,
    pub git_branch: Option<String>,
    pub message_count: usize,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub uploaded_by: String,
    pub uploaded_at: DateTime<Utc>,
    pub source_host: Option<String>,
    pub owner_email: Option<String>,
    pub visibility: String,
    /// True when a live session is currently streaming for this id (a row
    /// exists in `live_sessions`). The SPA renders these as LIVE cards.
    #[serde(default)]
    pub is_live: bool,
    /// Repos this conversation touched. Empty when none recorded.
    #[serde(default)]
    pub repos: Vec<StoredRepoLink>,
}

/// An LLM-generated 1:1 summary of a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub intent: String,
    pub invariants: Vec<String>,
    pub summary: String,
    pub model: String,
    pub generated_by: Option<String>,
    pub generated_at: DateTime<Utc>,
}

/// Owner-facing native binding metadata, independent of transcript size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionIdentity {
    pub session_id: String,
    pub agent: AgentKind,
    pub agent_session_id: Option<String>,
}

/// A full stored session: session metadata + upload metadata + ordered messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: String,
    #[serde(default)]
    pub agent: AgentKind,
    /// The agent's NATIVE session id, when it differs from `id` and is known.
    /// Populated for live sessions started through the host where the agent
    /// mints its own id (Codex): the CLI tailer reads it from the rollout and
    /// pushes it, so `reopen` can resume by it. `None` for Claude (its `id` is
    /// already the resume target), legacy rows, and uploaded sessions.
    #[serde(default)]
    pub agent_session_id: Option<String>,
    pub title: String,
    pub project_path: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub claude_version: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub message_count: usize,
    pub skipped_lines: usize,
    pub uploaded_by: String,
    pub uploaded_at: DateTime<Utc>,
    pub source_host: Option<String>,
    pub owner_email: Option<String>,
    pub visibility: String,
    /// The project this session is assigned to, when any. `None` until assigned.
    #[serde(default)]
    pub project_id: Option<String>,
    pub shared_emails: Vec<String>,
    /// True when the VIEWER is a shared reader whom the publishing host also
    /// named as intended to type into the session through it, once their key
    /// is trusted there. Reading is unchanged; typing stays blocked until the
    /// trust exists.
    #[serde(default)]
    pub viewer_write_pending_key: bool,
    /// Repos this conversation touched. Empty when none recorded.
    #[serde(default)]
    pub repos: Vec<StoredRepoLink>,
    /// The LLM-generated summary, when one has been produced. `None` until a
    /// summary is generated.
    #[serde(default)]
    pub summary: Option<SessionSummary>,
    pub messages: Vec<core::Message>,
}

/// A repo link as stored/returned: the canonical repo+branch (+remote/PR) plus
/// every user's local checkout path for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRepoLink {
    pub repo: String,
    pub remote_url: Option<String>,
    pub branch: String,
    pub pr_url: Option<String>,
    pub paths: Vec<RepoPath>,
    /// True when this repo is a monorepo container (a git repo nesting other git
    /// repos). Set by the CLI (`RepoLink::is_container`) and excluded from
    /// single-repo PR-context resolution. Defaults to `false`.
    #[serde(default)]
    pub is_container: bool,
}

/// One user's local checkout path for a repo+branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoPath {
    pub user_email: String,
    pub local_path: String,
}

/// Canonical repo-link fields editable from the web UI (no per-user path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoLinkInput {
    pub repo: String,
    #[serde(default)]
    pub remote_url: Option<String>,
    pub branch: String,
    #[serde(default)]
    pub pr_url: Option<String>,
}

/// A directory user. `email` is the primary key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub email: String,
    pub display_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// An unused, unexpired password-reset token loaded for verification.
///
/// Only the bcrypt hash of the secret is persisted. The raw secret exists only
/// in the link returned to the initiating administrator.
#[derive(Debug, Clone, PartialEq)]
pub struct PasswordResetToken {
    pub token_id: String,
    pub user_email: String,
    pub secret_hash: String,
    pub expires_at: DateTime<Utc>,
}

/// Per-user launch overrides for Codex sessions spawned by the live CLI host.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodexLaunchSettings {
    pub sandbox_mode: Option<String>,
    pub approval_policy: Option<String>,
}

/// Per-user launch settings for OpenCode sessions spawned by the live CLI host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenCodeLaunchSettings {
    pub default_launcher: core::AgentLauncher,
    pub auto: bool,
    /// Re-open an OpenCode session through `adk code` rather than a bare
    /// `opencode`. New sessions use the default or selected launcher, but a
    /// re-open has only the stored session to go on — and the
    /// stored row records the agent, not the harness it was started with. This
    /// is how a user says "my OpenCode lives in the Arrakis harness", so a
    /// re-opened session keeps its MCP credentials and agent profile.
    pub adk_reopen: bool,
    /// Let an `adk code` child draw its tmux status strip. Off by default: the
    /// strip runs OpenCode inside a tmux pane, and in a live session that pane
    /// is what the web mirror shows instead of the OpenCode TUI.
    pub adk_statusline: bool,
}

/// Distinct project path with the number of sessions under it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectCount {
    pub project_path: String,
    pub session_count: usize,
}

pub use conversations_core::Project;

/// A project together with its member session summaries (newest first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectDetail {
    pub project: Project,
    pub sessions: Vec<StoredSummary>,
}

/// The parent a chat thread attaches to: exactly one of a session or a project.
#[derive(Debug, Clone)]
pub enum ChatParent {
    Session(String),
    Project(String),
}

/// A saved chat thread, scoped to either a session or a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub owner_email: Option<String>,
    pub title: Option<String>,
    pub shared: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A single message in a chat thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub chat_id: String,
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

/// A chat thread together with its messages (oldest-first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatWithMessages {
    pub chat: Chat,
    pub messages: Vec<ChatMessage>,
}

/// Default status of a freshly-registered live session.
pub const LIVE_STATUS_LIVE: &str = "live";
/// A live session that has been ended and graduated into a stored session.
pub const LIVE_STATUS_GRADUATED: &str = "graduated";
/// A live session whose CLI tailer stopped pushing events (stale).
pub const LIVE_STATUS_ABANDONED: &str = "abandoned";

/// A live (in-progress) session being streamed to the web UI. Mirrors a row in
/// `live_sessions`. By default `id == session_id == eventual sessions.id`, so
/// graduation is an upsert on that id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveSession {
    pub id: String,
    pub session_id: String,
    #[serde(default)]
    pub agent: AgentKind,
    pub title: Option<String>,
    pub project_path: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub owner_email: Option<String>,
    pub source_host: Option<String>,
    /// The launch repo's `origin` remote URL, as reported by the CLI at
    /// register. Parsed (`github::parse_repo_remote`) to resolve owner/repo for
    /// PR-shorthand auto-linking while the session is live; `None` when the CLI
    /// could not determine a remote (no git, detached, older CLI).
    pub remote_url: Option<String>,
    pub status: String,
    pub mode: String,
    pub started_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub graduated_session_id: Option<String>,
}

/// Fields supplied when registering a new live session. `id` defaults to
/// `session_id` when the caller does not override it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewLiveSession {
    pub id: String,
    pub session_id: String,
    pub agent: AgentKind,
    pub title: Option<String>,
    pub project_path: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub owner_email: Option<String>,
    pub source_host: Option<String>,
    /// The launch repo's `origin` remote URL (see [`LiveSession::remote_url`]).
    pub remote_url: Option<String>,
    /// Interaction mode for the live session: `"view"` (default, read-only
    /// streaming) or `"interactive"` (reply-back enabled). Threaded into the
    /// `live_sessions.mode` column instead of a hardcoded literal.
    pub mode: String,
}

/// A person grant on a stored or live session. Transcript access and
/// communication are independent:
/// - `can_read` — read the transcript and watch the live stream.
/// - `can_write` — also interactive: sanitized replies (`/replies`), raw
///   keyboard/clipboard input (`/input`, `/clipboard`), and terminal control
///   requests (`/repaint`, `/resize`). Requires `can_read`.
/// - `can_communicate` — the person's own sessions may discover and message
///   this session. Grants no transcript or terminal access.
///
/// Write is the single interactive grant — there is no separate keyboard-control
/// escalation (it was folded into write in migration 0024/0023). The author is
/// never represented here — they always have full access implicitly. Build one
/// from an API payload with [`LiveShareRequest::into_share`].
#[derive(Debug, Clone, PartialEq)]
pub struct LiveShare {
    pub email: String,
    pub can_read: bool,
    pub can_write: bool,
    pub can_communicate: bool,
}

impl LiveShare {
    /// Rejects write without read and a share that grants nothing.
    pub fn validate(&self) -> Result<()> {
        if self.can_write && !self.can_read {
            return Err(AppError::BadRequest(
                "write access requires read access".into(),
            ));
        }
        if !self.can_read && !self.can_communicate {
            return Err(AppError::BadRequest(format!(
                "the share for {} grants no access",
                self.email
            )));
        }
        Ok(())
    }
}

/// Serialized with `communication_only` (`!can_read`) so an older SPA bundle
/// still renders a share without transcript access correctly.
impl Serialize for LiveShare {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut share = serializer.serialize_struct("LiveShare", 5)?;
        share.serialize_field("email", &self.email)?;
        share.serialize_field("can_read", &self.can_read)?;
        share.serialize_field("can_write", &self.can_write)?;
        share.serialize_field("can_communicate", &self.can_communicate)?;
        share.serialize_field("communication_only", &!self.can_read)?;
        share.end()
    }
}

/// A person share as sent by a client. Absent fields keep the meaning older
/// clients relied on: read-only by default, a read-write share includes
/// communication, and `communication_only` means communicate without
/// transcript access.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LiveShareRequest {
    pub email: String,
    #[serde(default)]
    pub can_read: Option<bool>,
    #[serde(default)]
    pub can_write: bool,
    #[serde(default)]
    pub can_communicate: Option<bool>,
    #[serde(default)]
    pub communication_only: bool,
}

impl LiveShareRequest {
    pub fn into_share(self) -> Result<LiveShare> {
        let share = if self.communication_only {
            if self.can_read == Some(true) || self.can_write || self.can_communicate == Some(false)
            {
                return Err(AppError::BadRequest(
                    "communication_only conflicts with the other share fields".into(),
                ));
            }
            LiveShare {
                email: self.email,
                can_read: false,
                can_write: false,
                can_communicate: true,
            }
        } else {
            LiveShare {
                email: self.email,
                can_read: self.can_read.unwrap_or(true),
                can_write: self.can_write,
                can_communicate: self.can_communicate.unwrap_or(self.can_write),
            }
        };
        share.validate()?;
        Ok(share)
    }
}

/// A durably-stored live message: the server-assigned monotonic `seq`, the
/// originating message's `message_uuid` (the de-dup key), and the deserialized
/// [`core::Message`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveStoredMessage {
    pub seq: i64,
    pub message_uuid: String,
    pub message: core::Message,
    pub created_at: DateTime<Utc>,
}

/// A GitHub pull request linked to a session.
///
/// Identified by `(owner, repo, number)` — together with `session_id` this is
/// the primary key. `url` is the canonical `https://github.com/{owner}/{repo}/pull/{number}`
/// URL, stored for display without re-constructing it from parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPr {
    pub url: String,
    pub owner: String,
    pub repo: String,
    pub number: i64,
}

pub use conversations_core::SessionLinearDoc;

/// A linked PR that a connected sync agent should proactively poll: a session's PR that is
/// still **open** (or never synced, so its state is unknown). Carries the
/// `session_id` so the agent can push the fetched snapshot back. Merged/closed
/// PRs are excluded by the query — no new review activity is expected on them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrWatchTarget {
    pub session_id: String,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub url: String,
}

/// A linked PR together with its CACHED GitHub status (pushed by the per-user
/// CLI; the server never fetches it). The status fields are `None` until the
/// CLI has pushed a snapshot for this PR; `status_fetched_at` records when that
/// last push happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPrWithStatus {
    pub url: String,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    /// `"open"` / `"closed"` — `None` when never fetched.
    pub status_state: Option<String>,
    /// `true` when merged — `None` when never fetched.
    pub status_merged: Option<bool>,
    /// `true` for a draft PR — `None` when never fetched.
    pub status_draft: Option<bool>,
    /// PR title — `None` when never fetched.
    pub status_title: Option<String>,
    pub status_review_decision: Option<String>,
    /// When the cached status was last pushed by the CLI (`None` when never).
    pub status_fetched_at: Option<DateTime<Utc>>,
    /// `true` when this link was auto-detected from conversation text;
    /// `false` for a manually-linked PR. Defaults to `false` for backward
    /// compatibility on older rows.
    #[serde(default)]
    pub detected: bool,
}

/// A CACHED GitHub PR comment as stored (pushed by the per-user CLI; the server
/// never fetches it). Identified per session by
/// `(owner, repo, number, comment_id)`. `kind` is `issue` | `review` |
/// `review_comment`; `created_at` is the verbatim RFC3339 string `gh` reports;
/// `ingested_at` is the server's own stamp at upsert time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredPrComment {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub comment_id: String,
    pub kind: String,
    pub author: String,
    pub body: String,
    pub html_url: String,
    /// Verbatim RFC3339 timestamp `gh` reported for the comment.
    pub created_at: String,
    /// File the inline review comment hangs on (`review_comment` only; `None`
    /// for issue / review kinds and for rows predating this column).
    #[serde(default)]
    pub path: Option<String>,
    /// Line in `path` the inline review comment hangs on. `review_comment` only.
    #[serde(default)]
    pub line: Option<i64>,
    /// The diff snippet GitHub shows above the inline review comment.
    #[serde(default)]
    pub diff_hunk: Option<String>,
    /// Whether the review thread is resolved. `review_comment` only.
    #[serde(default)]
    pub resolved: Option<bool>,
    /// Server stamp recorded when this comment's content last changed.
    pub ingested_at: DateTime<Utc>,
    /// Monotonic per-session sync revision. Zero for comments predating deltas.
    #[serde(default)]
    pub revision: i64,
}

/// The storage contract used by the API handlers.
#[async_trait]
pub trait Repository:
    Send
    + Sync
    + messaging::AgentMessagingRepository
    + notifications::NotificationRepository
    + publication::PublicationRepository
{
    async fn live_order(&self, viewer: &str) -> Result<LiveOrder>;
    async fn set_live_order(&self, viewer: &str, revision: i64, ids: &[String]) -> Result<bool>;

    /// Lightweight readiness check: runs `SELECT 1` to confirm the backend is
    /// reachable. Used by the `/readyz` probe; `Err` ⇒ the pod is not ready.
    async fn health(&self) -> Result<()>;

    /// Insert or fully replace a session (idempotent by `session.id`).
    ///
    /// On CREATE the session is recorded with the given `owner_email`,
    /// `visibility = 'restricted'`, and no shares. On RE-UPLOAD of an existing
    /// id only the content (messages) and upload metadata (`uploaded_by`,
    /// `uploaded_at`, `source_host`) are refreshed — the existing
    /// `owner_email`, `visibility`, and share list are PRESERVED so idempotent
    /// re-uploads never wipe sharing settings.
    async fn upsert_session(
        &self,
        uploaded_by: &str,
        source_host: Option<&str>,
        owner_email: Option<&str>,
        session: &core::Session,
    ) -> Result<UpsertOutcome>;

    /// List session summaries, newest first, filtered for `viewer`.
    ///
    /// `q` does a case-insensitive LIKE search across title, message text,
    /// uploader, and project path. `project` filters by exact `project_path`.
    ///
    /// Visibility enforcement: `viewer = None` returns everything (the auth-off
    /// default). `viewer = Some(email)` returns only sessions where
    /// `visibility = 'organization'`, OR `owner_email = email`, OR `email`
    /// appears in the session's share list.
    async fn list_sessions(
        &self,
        viewer: Option<&str>,
        q: Option<&str>,
        project: Option<&str>,
    ) -> Result<Vec<StoredSummary>>;

    /// Fetch a full session with ordered messages, or `None` if absent.
    ///
    /// Honors the same visibility rules as [`Repository::list_sessions`]: when
    /// `viewer = Some(email)` cannot see the session, `None` is returned (so the
    /// handler 404s).
    async fn get_session(&self, viewer: Option<&str>, id: &str) -> Result<Option<StoredSession>>;

    /// The session's `owner_email`, for authorization checks.
    ///
    /// Returns `Ok(None)` when no session with `id` exists; `Ok(Some(None))`
    /// when the session exists but has no owner (uploaded with auth off / by the
    /// CLI without an identity); `Ok(Some(Some(email)))` otherwise.
    async fn get_session_owner(&self, id: &str) -> Result<Option<Option<String>>>;

    /// Set (or, with `None`, clear) a session's DB-only title override. The
    /// on-disk transcript and the parsed `title` column are never touched, so a
    /// re-upload preserves the override; reads return
    /// `COALESCE(title_override, title)`. Returns `false` when no session with
    /// `id` exists, `true` when the override was written.
    async fn set_title_override(&self, id: &str, title: Option<&str>) -> Result<bool>;

    /// The session's user-set title override, if any. `Ok(None)` when the
    /// session is absent or has no override. Cheap (no messages loaded) — used
    /// to overlay a rename onto the live listing's CLI-pushed title.
    async fn get_title_override(&self, id: &str) -> Result<Option<String>>;

    /// Delete a session and all its dependent rows (messages, shares, repos,
    /// per-user paths cascade via FK). No-op if the session does not exist.
    async fn delete_session(&self, id: &str) -> Result<()>;

    /// Distinct project paths with their session counts.
    async fn list_projects(&self) -> Result<Vec<ProjectCount>>;

    /// Insert or update a directory user: sets `display_name` (when provided)
    /// and refreshes `last_seen_at`. Used by the auth slice on login.
    async fn upsert_user(&self, email: &str, display_name: Option<&str>) -> Result<User>;

    /// Fetch a directory user together with their stored password hash, or
    /// `None` if the user does not exist. The hash is `None` when the user has
    /// no local password (a Google-only account, or a row created by sharing
    /// before that person ever set a password). Used by email+password login.
    async fn get_user_with_password(&self, email: &str) -> Result<Option<(User, Option<String>)>>;

    /// Set (replace) a user's bcrypt password hash. Returns `NotFound` if no
    /// user row exists for `email` (callers create the row first via
    /// [`Repository::upsert_user`]).
    async fn set_password(&self, email: &str, password_hash: &str) -> Result<()>;

    /// Reserve a signup email, with a cooldown per recipient.
    async fn reserve_signup_token(
        &self,
        email: &str,
        token_hash: &str,
        now: chrono::DateTime<chrono::Utc>,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool>;

    async fn remove_signup_token(&self, token_hash: &str) -> Result<()>;

    async fn has_valid_signup_token(
        &self,
        token_hash: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool>;

    /// Consume a valid token and set an initial password in the same transaction.
    async fn complete_signup(
        &self,
        token_hash: &str,
        password_hash: &str,
        display_name: Option<&str>,
        reserved_admins: &[String],
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool>;

    /// Return the account's current authentication version, or `None` when the
    /// user does not exist. Sessions with an older version are invalid.
    async fn get_user_auth_version(&self, email: &str) -> Result<Option<i64>>;

    /// Create a reset token and invalidate every older unused token for the
    /// same account. Returns `NotFound` when the account does not exist.
    async fn create_password_reset_token(
        &self,
        user_email: &str,
        created_by: &str,
        token_id: &str,
        secret_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Load a reset token only while it is unused and has not expired.
    async fn get_password_reset_token(
        &self,
        token_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<PasswordResetToken>>;

    /// Atomically consume a valid token, replace its user's password hash, and
    /// increment the authentication version to invalidate existing sessions.
    /// Returns `false` if another request consumed or expired it first.
    async fn consume_password_reset_token(
        &self,
        token_id: &str,
        password_hash: &str,
        used_at: DateTime<Utc>,
    ) -> Result<bool>;

    /// Whether an existing user has been granted administrator access in the
    /// database. Bootstrap administrators are handled by runtime config.
    async fn is_user_admin(&self, email: &str) -> Result<bool>;

    /// List email addresses with persisted administrator grants.
    async fn list_admin_emails(&self) -> Result<Vec<String>>;

    /// Grant or revoke persisted administrator access for an existing user.
    /// Returns `NotFound` when the user does not exist.
    async fn set_user_admin(&self, email: &str, is_admin: bool, granted_by: &str) -> Result<()>;

    /// List every user in the directory, ordered by email. This is reserved for
    /// administrator-facing account management; interactive user pickers use
    /// the bounded [`Repository::search_users`] method instead.
    async fn list_users(&self) -> Result<Vec<User>>;

    /// Permanently delete a directory user and the auth / preference rows that
    /// hang off the account. In one transaction:
    /// - the user row is removed from `users` (cascading FKs drop
    ///   `user_admins`, `user_container_paths`, `user_settings`,
    ///   `browser_keys`, `browser_key_challenges`, and
    ///   `password_reset_tokens` for the account);
    /// - the email is scrubbed from every stored and live session share list
    ///   (`session_shares`, `live_session_shares`), so a revoked account loses
    ///   access that was granted TO it.
    ///
    /// Content the user authored is intentionally preserved as-is. The
    /// `owner_email` / `uploaded_by` columns on `sessions`, `projects`,
    /// `chats`, `live_sessions`, `agent_messages`, and `publications` have no
    /// FK to `users` — they are string tombstones that keep audit history
    /// readable after an account is removed. Returns `Ok(true)` when a user
    /// row was removed, `Ok(false)` when no such user existed.
    async fn delete_user(&self, email: &str) -> Result<bool>;

    /// Search the user directory. `q` does a case-insensitive substring match on
    /// email and display name; `None` returns the first `limit` users ordered by
    /// email. `limit` is clamped to `[1, USER_SEARCH_MAX_LIMIT]`.
    async fn search_users(&self, q: Option<&str>, limit: i64) -> Result<Vec<User>>;

    async fn browser_keys(&self, email: &str) -> Result<Vec<crate::user_keys::BrowserKey>>;
    async fn save_browser_key_challenge(
        &self,
        challenge: &crate::user_keys::Challenge,
    ) -> Result<()>;
    async fn browser_key_challenge(
        &self,
        id: &str,
        email: &str,
        now: i64,
    ) -> Result<Option<crate::user_keys::Challenge>>;
    /// Atomically consume a fresh challenge and publish its verified key.
    async fn publish_browser_key(
        &self,
        challenge: &crate::user_keys::Challenge,
        key: &crate::user_keys::BrowserKey,
        now: i64,
    ) -> Result<bool>;

    /// Replace a session's sharing settings. Validates `visibility`. For
    /// `restricted`, `emails` becomes the share list; for `organization` the
    /// share list is cleared.
    async fn set_sharing(
        &self,
        session_id: &str,
        visibility: &str,
        emails: &[String],
    ) -> Result<()>;

    /// Return `(visibility, shared_emails)` for a session, or `None` if absent.
    async fn get_sharing(&self, session_id: &str) -> Result<Option<(String, Vec<String>)>>;

    /// Merge repo links into a session from an upload (idempotent, additive).
    ///
    /// For each link: upsert `(session_id, repo, branch)`, filling
    /// `remote_url`/`pr_url` when provided but NEVER overwriting an existing
    /// non-null value with null; and, when `uploader` is `Some` and the link
    /// carries a `local_path`, record that per-user path. Links not present in
    /// `links` are left untouched — another engineer's upload only adds.
    async fn merge_repos(
        &self,
        session_id: &str,
        uploader: Option<&str>,
        links: &[core::RepoLink],
    ) -> Result<()>;

    /// Replace a session's canonical repo links (web-UI edit). Replaces the
    /// `(repo, remote_url, branch, pr_url)` set; per-user paths for surviving
    /// `(repo, branch)` pairs are preserved, paths for removed pairs dropped.
    /// Returns `NotFound` if the session is absent.
    async fn set_repos(&self, session_id: &str, links: &[RepoLinkInput]) -> Result<()>;

    /// All repo links for a session, each with every user's local paths.
    async fn get_repos(&self, session_id: &str) -> Result<Vec<StoredRepoLink>>;

    /// Batched [`Repository::get_repos`] for many sessions at once (avoids N+1
    /// when listing). Returns `(session_id, links)` pairs; sessions with no
    /// repos may be omitted.
    async fn get_repos_for_sessions(
        &self,
        session_ids: &[String],
    ) -> Result<Vec<(String, Vec<StoredRepoLink>)>>;

    /// Fetch the stored LLM summary for a session, or `None` if none exists.
    async fn get_summary(&self, session_id: &str) -> Result<Option<SessionSummary>>;

    /// Insert or replace (upsert on the `session_id` PK) a session's LLM
    /// summary derived from `source` (see [`SourceContext`]); `Conflict` when
    /// the session's publication no longer matches it.
    async fn upsert_summary(
        &self,
        source: SourceContext,
        session_id: &str,
        summary: &SessionSummary,
    ) -> Result<()>;

    /// Create a new explicit project with a generated id. `owner_email` records
    /// the creator (may be `None` when auth is off). `description` is optional
    /// (stored as `NULL` when `None`).
    async fn create_project(
        &self,
        owner_email: Option<&str>,
        name: &str,
        description: Option<&str>,
    ) -> Result<Project>;

    /// List explicit projects, newest first, each with its `session_count`.
    ///
    /// `session_count` honors the same visibility rules as
    /// [`Repository::list_sessions`]: `viewer = None` counts everything (the
    /// auth-off default); `viewer = Some(email)` counts only that viewer's
    /// visible member sessions, so restricted/private members do not leak
    /// through the aggregate.
    async fn list_projects_detailed(&self, viewer: Option<&str>) -> Result<Vec<Project>>;

    /// Fetch a project together with its member session summaries (newest
    /// first), or `None` if the project does not exist.
    ///
    /// The project row itself is visible to any viewer (there is no
    /// per-project visibility in v1), but the member session summaries — and
    /// the `session_count` — honor the same visibility rules as
    /// [`Repository::list_sessions`]: `viewer = None` returns everything (the
    /// auth-off default); `viewer = Some(email)` returns only that viewer's
    /// visible members, so restricted/private sessions do not leak through
    /// the project detail view.
    async fn get_project(&self, viewer: Option<&str>, id: &str) -> Result<Option<ProjectDetail>>;

    /// The project's `owner_email`, for authorization checks. Mirrors
    /// [`Repository::get_session_owner`]: `Ok(None)` when no project with `id`
    /// exists; `Ok(Some(None))` when it exists with no owner; `Ok(Some(Some(_)))`
    /// otherwise.
    async fn get_project_owner(&self, id: &str) -> Result<Option<Option<String>>>;

    /// Rename a project (and set its `description`), bumping `updated_at`.
    /// `NotFound` if absent. `description` is always set to the provided value
    /// (so passing `None` clears it) — the edit form always sends both fields.
    async fn update_project(&self, id: &str, name: &str, description: Option<&str>) -> Result<()>;

    /// Delete a project. No-op if absent. Member sessions survive with their
    /// `project_id` set to NULL (FK SET NULL); the project's project-chats and
    /// their messages cascade away.
    async fn delete_project(&self, id: &str) -> Result<()>;

    /// Assign (or, with `None`, clear) a session's project. `NotFound` if the
    /// session is absent; `BadRequest` if `Some(project_id)` does not exist.
    /// Assigning replaces any prior assignment (a session has at most one).
    async fn set_session_project(&self, session_id: &str, project_id: Option<&str>) -> Result<()>;

    /// Create a new chat thread under `parent` (a session or a project), with a
    /// generated id, the given `owner_email`, and optional `title`. Threads are
    /// private (`shared = false`) by default. `BadRequest` if the parent does
    /// not exist. A session chat is fenced on the session's `source`; a project
    /// chat has no publication source and takes only `Unmanaged`.
    async fn create_chat(
        &self,
        parent: &ChatParent,
        source: SourceContext,
        owner_email: Option<&str>,
        title: Option<&str>,
    ) -> Result<Chat>;

    /// List chat threads under `parent`, newest first, filtered for `viewer`.
    /// `viewer = None` returns every thread (auth-off default);
    /// `viewer = Some(email)` returns only threads the viewer owns OR that are
    /// shared.
    async fn list_chats(&self, parent: &ChatParent, viewer: Option<&str>) -> Result<Vec<Chat>>;

    /// Fetch a chat thread by id, or `None` if absent.
    async fn get_chat(&self, id: &str) -> Result<Option<Chat>>;

    /// Fetch a chat thread together with its messages (oldest-first), or `None`
    /// if absent.
    async fn get_chat_with_messages(&self, id: &str) -> Result<Option<ChatWithMessages>>;

    /// Append a message to a chat thread, bumping the thread's `updated_at`.
    /// `NotFound` if the chat is absent. Fenced like [`Repository::create_chat`]
    /// on the `source` the message was derived from.
    async fn add_chat_message(
        &self,
        chat_id: &str,
        source: SourceContext,
        role: &str,
        content: &str,
    ) -> Result<ChatMessage>;

    /// Update a chat thread's `shared` flag and (when `Some`) its `title`,
    /// bumping `updated_at`. A `None` title leaves the existing title unchanged.
    /// `NotFound` if absent.
    async fn update_chat(&self, id: &str, shared: bool, title: Option<&str>) -> Result<()>;

    /// Delete a chat thread (its messages cascade away). No-op if absent.
    async fn delete_chat(&self, id: &str) -> Result<()>;

    /// Register a new live session row (`status = 'live'`). The `mode` column is
    /// set from `ls.mode` (`"view"` or `"interactive"`).
    /// `started_at`/`last_event_at` are set to now. Returns the stored row.
    async fn create_live_session(&self, ls: &NewLiveSession) -> Result<LiveSession>;

    /// Append a streamed message to a live session, idempotently. De-dup is on
    /// `(live_id, message.uuid)`: a re-push of an already-stored message returns
    /// `Ok(None)` and stores nothing. A fresh message is assigned the next `seq`
    /// (max existing + 1), serialized to the `payload` column, bumps the live
    /// session's `last_event_at`, and is returned as `Ok(Some(_))`.
    async fn append_live_message(
        &self,
        live_id: &str,
        msg: &core::Message,
    ) -> Result<Option<LiveStoredMessage>>;

    /// All stored messages for a live session, ordered by `seq` ascending. The
    /// replay source for a browser subscribing mid-session.
    async fn get_live_messages(&self, live_id: &str) -> Result<Vec<LiveStoredMessage>>;

    /// Stored messages for a live session with `seq` strictly greater than
    /// `after_seq`, ordered by `seq` ascending — the incremental-replay
    /// counterpart of [`Repository::get_live_messages`]. A browser that already
    /// holds messages up to `after_seq` reconnects with `?since=after_seq` and
    /// receives only the tail instead of the whole backlog. `seq` is dense and
    /// monotonic per session (see `append_live_message`), so `after_seq` is an
    /// exact resume cursor with no risk of a skipped row.
    async fn get_live_messages_since(
        &self,
        live_id: &str,
        after_seq: i64,
    ) -> Result<Vec<LiveStoredMessage>>;

    /// List live sessions, newest first (by `started_at`). `status` filters by
    /// the `status` column; pass `None` for all statuses.
    ///
    /// Visibility enforcement mirrors [`Repository::list_sessions`]:
    /// `viewer = None` returns every matching row (the auth-off default);
    /// `viewer = Some(email)` returns only sessions where `owner_email = email`
    /// OR `email` has a non-communication-only person grant. Live
    /// sessions are always private — there is no `organization` visibility.
    async fn list_live_sessions(
        &self,
        status: Option<&str>,
        viewer: Option<&str>,
    ) -> Result<Vec<LiveSession>>;

    /// Replace person grants for a stored or live session (delete-all-then-insert
    /// in a transaction). `shares` is deduped on email (last write wins);
    /// emails are trimmed + lowercased to match the user-directory convention.
    /// The author-only route checks session existence first.
    async fn set_live_shares(&self, live_id: &str, shares: &[LiveShare]) -> Result<()>;

    /// All shares for a live session, ordered by email.
    async fn get_live_shares(&self, live_id: &str) -> Result<Vec<LiveShare>>;

    /// Combined authorization read for a live session: `(owner_email, shares)`,
    /// or `None` when the live session row is absent (⇒ 404). The owner may be
    /// `None` when the session was registered with auth off. Feeds
    /// `authorize_live_view` / `authorize_live_write` / `authorize_live_author`
    /// in the API layer.
    #[allow(clippy::type_complexity)]
    async fn get_live_session_access(
        &self,
        id: &str,
    ) -> Result<Option<(Option<String>, Vec<LiveShare>)>>;

    /// Fetch a single live session row, or `None` if absent.
    async fn get_live_session(&self, live_id: &str) -> Result<Option<LiveSession>>;

    /// The live session's `owner_email`, for authorization checks. Mirrors
    /// [`Repository::get_session_owner`]: `Ok(None)` when no live session with
    /// `id` exists (→ 404); `Ok(Some(None))` when it exists with no owner
    /// (registered with auth off); `Ok(Some(Some(email)))` otherwise. Used by the
    /// owner-only reply routes.
    async fn get_live_session_owner(&self, live_id: &str) -> Result<Option<Option<String>>>;

    /// Bump a live session's `last_event_at` to now (the CLI heartbeat). Also
    /// self-heals: a row the reaper prematurely flipped to `abandoned` (a
    /// heartbeat gap longer than the abandon threshold, e.g. a rolling deploy) is
    /// resurrected to `live` so its mirror reappears. A `graduated` row is
    /// terminal: the update is scoped to `live`/`abandoned` rows, so it is never
    /// touched (no `last_event_at` bump, no resurrection). Returns `Ok(true)`
    /// when a row was updated, `Ok(false)` when no `live`/`abandoned` row with
    /// `live_id` exists — i.e. it is absent (→ 404) or already `graduated`.
    async fn touch_live_session(&self, live_id: &str) -> Result<bool>;

    /// Update a live session's display `title` (the CLI pushes a
    /// transcript-derived title here so the live view shows it instead of the
    /// raw session id).
    /// Returns `Ok(true)` when a row was updated, `Ok(false)` when no live
    /// session with `live_id` exists (→ 404).
    async fn update_live_session_title(&self, live_id: &str, title: &str) -> Result<bool>;

    /// Record the agent's NATIVE session id for a live session (the resume
    /// target). Used by the CLI tailer for Codex, whose real session id is only
    /// known once it writes its rollout (it cannot be told an id at launch).
    /// Updates the `live_sessions` row AND its eager `sessions` row so a later
    /// `reopen` — which reads the (possibly graduated) `sessions` row — resumes
    /// by the right id. Only mutates a still-`live` row (a late push for a
    /// graduated/abandoned/absent session is a no-op, mirroring
    /// [`Repository::update_live_session_title`]). Returns `Ok(true)` when a row
    /// was updated, `Ok(false)` otherwise.
    async fn set_live_agent_session_id(
        &self,
        live_id: &str,
        agent_session_id: &str,
    ) -> Result<bool>;

    /// End + graduate a live session: assemble a [`core::Session`] from the
    /// stored `live_messages` (id = the live session's `session_id`, with its
    /// title/cwd/git_branch), upsert it via [`Repository::upsert_session`], set
    /// `graduated_session_id` + `status = 'graduated'`, and return the graduated
    /// session id. `Ok(None)` when the live session does not exist.
    async fn complete_live_session(&self, live_id: &str) -> Result<Option<String>>;

    /// Mark `live` sessions whose `last_event_at` is older than `older_than` as
    /// `abandoned`. Returns the number of rows updated.
    ///
    /// Driven by [`crate::reaper`] (spawned from `main`). Interactive sessions
    /// rely on the `POST /api/live/{id}/heartbeat` endpoint
    /// ([`Repository::touch_live_session`]) to keep `last_event_at` fresh during
    /// a turn with no streamed messages, so the abandon threshold MUST exceed the
    /// CLI's heartbeat interval (enforced as a floor in `config`), or an
    /// interactive session gets reaped mid-turn. A premature abandon is
    /// recoverable: the next heartbeat resurrects the row (see
    /// [`Repository::touch_live_session`]).
    async fn mark_abandoned_live_sessions(&self, older_than: DateTime<Utc>) -> Result<u64>;

    /// Build a database-backed [`SessionBackend`] for persistent login sessions.
    ///
    /// Reuses the repository's own pool (NOT a fresh pool from the config URL),
    /// so the sessions table lives in the same database as the conversation
    /// data and survives pod restarts. The store's schema is created/migrated
    /// against that pool before the backend is returned.
    async fn session_store(&self) -> Result<crate::store::session::SessionBackend>;

    // --- Session PR links ---

    /// Link (or re-link) a GitHub PR to a session. Upserts on the primary key
    /// `(session_id, owner, repo, number)` — a duplicate link is silently ignored
    /// (INSERT OR IGNORE / ON CONFLICT DO NOTHING) so callers can call
    /// idempotently.
    async fn link_session_pr(&self, session_id: &str, pr: &SessionPr) -> Result<()>;

    /// All PRs linked to a session, each with its CACHED status, ordered by
    /// `created_at` ascending.
    async fn list_session_prs(&self, session_id: &str) -> Result<Vec<SessionPrWithStatus>>;

    /// Open (or never-synced) linked PRs across the given owner's **currently
    /// live** sessions — the proactive targets for a connected sync agent. Scoped to live
    /// sessions (`live_sessions.status = 'live'`) by design: a graduated /
    /// abandoned session is no longer polled on a timer, so a stale or phantom
    /// link can't be re-fetched forever — its PRs only refresh on an explicit
    /// "Sync now" (the reactive `sync_session` path). A PR qualifies when it is
    /// not merged and its cached `status_state` is `OPEN` or NULL (unknown);
    /// merged/closed PRs are excluded. `owner = Some(email)` scopes to that
    /// user's own live sessions; `owner = None` (auth off) returns every live
    /// session's open PRs. Ordered by `session_id`, then PR number.
    async fn list_open_pr_watch_targets(&self, owner: Option<&str>) -> Result<Vec<PrWatchTarget>>;

    /// Auto-link a batch of PRs detected from conversation text. Each is
    /// inserted with `detected = true` via INSERT … ON CONFLICT(<pk>) DO
    /// NOTHING, so an already-linked PR (whether manual or previously detected)
    /// is left UNTOUCHED — a manual link (`detected = false`) is never clobbered
    /// or downgraded. Best-effort by contract: callers treat any error as
    /// non-fatal so detection failure never fails the surrounding ingest.
    ///
    /// Returns the number of rows ACTUALLY inserted (the conflict no-ops count
    /// 0), so a live-ingest caller can publish a "PRs changed" nudge only when a
    /// genuinely new link landed — re-seeing the same ref in later messages does
    /// not re-notify.
    ///
    /// NOTE: migration 0016/0015 dropped the `session_prs` -> `sessions(id)` FK,
    /// so a ref links successfully even for a session id that has no `sessions`
    /// row yet — e.g. a still-live, ungraduated session. The link is recorded
    /// under the live id (== the eventual `sessions.id`), so it is already valid
    /// after graduation; no deferred merge is needed.
    async fn auto_link_session_prs(&self, session_id: &str, prs: &[SessionPr]) -> Result<u64>;

    /// Read a session's one-time PR-scan stamp. `Ok(None)` when the session is
    /// absent OR has never been scanned (the column is NULL); `Ok(Some(ts))`
    /// once a backfill scan has stamped it. The GET handler scans when this is
    /// `None`, then calls [`Repository::set_pr_scanned_at`].
    async fn pr_scanned_at(&self, session_id: &str) -> Result<Option<DateTime<Utc>>>;

    /// Stamp a session's one-time PR-scan marker (`sessions.pr_scanned_at`). A
    /// no-op when the session is absent.
    async fn set_pr_scanned_at(&self, session_id: &str, ts: DateTime<Utc>) -> Result<()>;

    /// Unlink a PR from a session. Returns `true` when a row was deleted,
    /// `false` when no matching `(session_id, owner, repo, number)` existed.
    async fn unlink_session_pr(
        &self,
        session_id: &str,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> Result<bool>;

    /// Auto-link a batch of detected Linear refs (the Linear analogue of
    /// [`Repository::auto_link_session_prs`]). Inserts each via INSERT … ON
    /// CONFLICT(session_id, url) DO NOTHING and returns the number of genuinely
    /// new rows (a re-seen ref inserts 0). Best-effort: a per-row DB error is
    /// logged and skipped so detection never fails the surrounding ingest. Like
    /// `session_prs` there is no FK to `sessions`, so a ref linked under a live
    /// id is already valid after graduation.
    async fn auto_link_session_linear_docs(
        &self,
        session_id: &str,
        docs: &[SessionLinearDoc],
    ) -> Result<u64>;

    /// All Linear refs linked to a session, ordered by `created_at` ascending.
    async fn list_session_linear_docs(&self, session_id: &str) -> Result<Vec<SessionLinearDoc>>;

    /// Unlink a Linear ref from a session by its canonical `url`. Returns `true`
    /// when a row was deleted, `false` when no matching `(session_id, url)`
    /// existed.
    async fn unlink_session_linear_doc(&self, session_id: &str, url: &str) -> Result<bool>;

    /// Read a session's one-time Linear-scan stamp (`sessions.linear_scanned_at`).
    /// `Ok(None)` when the session is absent OR has never been scanned; the GET
    /// handler scans when this is `None`, then calls
    /// [`Repository::set_linear_scanned_at`]. Separate from `pr_scanned_at`.
    async fn linear_scanned_at(&self, session_id: &str) -> Result<Option<DateTime<Utc>>>;

    /// Stamp a session's one-time Linear-scan marker. A no-op when the session
    /// is absent.
    async fn set_linear_scanned_at(&self, session_id: &str, ts: DateTime<Utc>) -> Result<()>;

    /// Upsert the CACHED GitHub status for an already-linked PR, stamping
    /// `status_fetched_at = fetched_at`. Returns `true` when a matching
    /// `(session_id, owner, repo, number)` row was updated, `false` when no
    /// such PR is linked (so the caller can 404). The server never fetches
    /// status itself — this is the per-user CLI pushing the snapshot it got
    /// from `gh`. Fenced on the `source` the producer read the PR link from.
    #[allow(clippy::too_many_arguments)]
    async fn set_session_pr_status(
        &self,
        source: SourceContext,
        session_id: &str,
        owner: &str,
        repo: &str,
        number: i64,
        status: &crate::github::PrStatus,
        fetched_at: DateTime<Utc>,
    ) -> Result<bool>;

    // --- Session PR comments cache ---

    /// Upsert a batch of CACHED GitHub PR comments for a session, keyed on
    /// `(session_id, owner, repo, number, comment_id)`. On conflict the mutable
    /// fields are refreshed only when they differ. Returns whether any row
    /// changed; unchanged snapshots leave `ingested_at` untouched. Fenced on
    /// the `source` the producer read the PR link from; for a published session
    /// the PR must be linked by its published history (`NotFound` otherwise).
    async fn upsert_pr_comments(
        &self,
        source: SourceContext,
        session_id: &str,
        owner: &str,
        repo: &str,
        number: i64,
        comments: &[crate::github::PrComment],
    ) -> Result<bool>;

    /// All cached PR comments for a session, ordered by `created_at` ascending.
    async fn list_pr_comments(&self, session_id: &str) -> Result<Vec<StoredPrComment>>;

    /// Comments added or edited after a previously observed sync revision.
    async fn list_pr_comments_since(
        &self,
        session_id: &str,
        since: i64,
    ) -> Result<Vec<StoredPrComment>>;

    /// Bounded previews for the compact rail. Never send full bodies or diffs.
    async fn list_pr_comment_previews(&self, session_id: &str) -> Result<Vec<StoredPrComment>>;

    // --- Per-user container ("monorepo top-level") paths ---

    /// The user's configured container paths, sorted ascending. Empty when the
    /// user has none set (never errors for a user with no rows).
    async fn get_container_paths(&self, email: &str) -> Result<Vec<String>>;

    /// The user's default live agent. Missing settings default to Claude.
    async fn get_default_agent(&self, email: &str) -> Result<AgentKind>;

    /// The user's inline-code colour config for Slack-friendly terminal copy,
    /// as the raw JSON blob the client stored (`{"palette":[],"rgb":[]}`), or
    /// `None` when unset. Opaque to the server — the frontend owns the shape.
    async fn get_inline_code_colors(&self, email: &str) -> Result<Option<String>>;

    /// The user's Codex launch overrides. Missing settings mean the CLI host
    /// lets Codex use its normal local config/defaults.
    async fn get_codex_launch_settings(&self, email: &str) -> Result<CodexLaunchSettings>;

    /// Whether web-started Claude sessions bypass permission prompts. Default off.
    async fn get_claude_skip_permissions(&self, email: &str) -> Result<bool>;

    async fn set_claude_skip_permissions(&self, email: &str, enabled: bool) -> Result<()>;

    /// The user's OpenCode launch settings. Automatic permission approval is
    /// disabled when no settings row exists.
    async fn get_opencode_launch_settings(&self, email: &str) -> Result<OpenCodeLaunchSettings>;

    /// Replace the user's full set of container paths in a transaction
    /// (delete-all-then-insert for that email). Each path is normalized via
    /// [`normalize_container_path`] before storage; blanks are dropped and
    /// duplicates (after normalization) are deduped.
    async fn set_container_paths(&self, email: &str, paths: &[String]) -> Result<()>;

    /// Replace the user's default live agent.
    async fn set_default_agent(&self, email: &str, agent: AgentKind) -> Result<()>;

    /// Replace the user's inline-code colour config (the raw JSON blob), or
    /// clear it when `json` is `None`.
    async fn set_inline_code_colors(&self, email: &str, json: Option<&str>) -> Result<()>;

    /// Replace the user's Codex launch overrides.
    async fn set_codex_launch_settings(
        &self,
        email: &str,
        settings: &CodexLaunchSettings,
    ) -> Result<()>;

    /// Replace the user's OpenCode launch settings.
    async fn set_opencode_launch_settings(
        &self,
        email: &str,
        settings: OpenCodeLaunchSettings,
    ) -> Result<()>;
}

/// Normalize a container path for storage and matching: trim surrounding
/// whitespace, then strip a single trailing `/` — but never reduce the root
/// path `/` itself to an empty string. The SAME normalization is applied to
/// stored container paths and to a repo link's `local_path` when matching, so
/// equality is consistent regardless of a trailing slash on either side.
pub fn normalize_container_path(p: &str) -> String {
    let trimmed = p.trim();
    if trimmed == "/" {
        return trimmed.to_string();
    }
    trimmed.strip_suffix('/').unwrap_or(trimmed).to_string()
}

/// Normalize, drop blanks, and dedupe (preserving first-seen order) a set of
/// container paths. Shared by both store backends so normalization is identical.
pub fn normalize_container_paths(paths: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in paths {
        let norm = normalize_container_path(p);
        if norm.is_empty() {
            continue;
        }
        if seen.insert(norm.clone()) {
            out.push(norm);
        }
    }
    out
}

/// True when `local_path` (a repo link's stored per-user checkout path) exactly
/// matches one of `container_paths` after normalization on both sides. Exact
/// root match only — a sub-repo whose root is BELOW a container path is NOT a
/// match (we keep the work sub-repos and drop only the container itself).
pub fn is_container_path(local_path: &str, container_paths: &[String]) -> bool {
    let needle = normalize_container_path(local_path);
    if needle.is_empty() {
        return false;
    }
    container_paths
        .iter()
        .any(|c| normalize_container_path(c) == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_and_strips_trailing_slash() {
        assert_eq!(normalize_container_path("  /a/b  "), "/a/b");
        assert_eq!(normalize_container_path("/a/b/"), "/a/b");
        assert_eq!(normalize_container_path("/a/b"), "/a/b");
        // Root is preserved, not reduced to empty.
        assert_eq!(normalize_container_path("/"), "/");
        assert_eq!(normalize_container_path(" / "), "/");
        // A blank / whitespace-only path normalizes to empty.
        assert_eq!(normalize_container_path("   "), "");
    }

    #[test]
    fn normalize_set_drops_blanks_and_dedupes() {
        let input = vec![
            "/Users/x/example".to_string(),
            "/Users/x/example/".to_string(), // dup after trailing-slash strip
            "  ".to_string(),                // blank
            "/Users/x/other".to_string(),
        ];
        assert_eq!(
            normalize_container_paths(&input),
            vec!["/Users/x/example".to_string(), "/Users/x/other".to_string()]
        );
    }

    #[test]
    fn is_container_exact_match() {
        let containers = vec!["/Users/x/example".to_string()];
        assert!(is_container_path("/Users/x/example", &containers));
    }

    #[test]
    fn is_container_trailing_slash_either_side() {
        // Trailing slash on the stored local_path.
        assert!(is_container_path(
            "/Users/x/example/",
            &["/Users/x/example".to_string()]
        ));
        // Trailing slash on the configured container path.
        assert!(is_container_path(
            "/Users/x/example",
            &["/Users/x/example/".to_string()]
        ));
    }

    #[test]
    fn is_container_non_match() {
        let containers = vec!["/Users/x/example".to_string()];
        assert!(!is_container_path("/Users/x/elsewhere", &containers));
        // Empty local_path never matches.
        assert!(!is_container_path("", &containers));
        // No containers configured.
        assert!(!is_container_path("/Users/x/example", &[]));
    }

    #[test]
    fn is_container_descendant_is_not_a_match() {
        // A sub-repo whose root is BELOW the container path is NOT the container.
        let containers = vec!["/Users/x/example".to_string()];
        assert!(!is_container_path(
            "/Users/x/example/example-platform",
            &containers
        ));
        // And a prefix-but-not-segment path must not match either.
        assert!(!is_container_path("/Users/x/arrakis-other", &containers));
    }
}
