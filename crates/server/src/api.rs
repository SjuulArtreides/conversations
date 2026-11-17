//! Axum handlers and the API router.
//!
//! Handlers are intentionally thin: they extract typed inputs, delegate to the
//! [`Repository`] trait, and map the DTOs to JSON responses. All storage logic
//! lives behind the trait so the backend can be swapped without touching this
//! layer.

use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;

use conversations_core as core;

use crate::error::{AppError, Result};
use crate::identity::CurrentUser;
use crate::live::LiveEvent;
use crate::llm::ChatTurn;
use crate::state::AppState;
use crate::store::{
    ChatParent, CodexLaunchSettings, LiveSession, LiveShare, LiveShareRequest,
    OpenCodeLaunchSettings, RepoLinkInput, SessionPr, SessionSummary, SourceContext, StoredSession,
};

/// Request body for uploading (creating or replacing) a session.
#[derive(Debug, Deserialize)]
pub struct UploadRequest {
    pub uploaded_by: String,
    pub source_host: Option<String>,
    pub session: core::Session,
    /// Repos this conversation touched (with the uploader's local paths).
    #[serde(default)]
    pub repos: Vec<core::RepoLink>,
}

/// Response for a successful upload.
#[derive(Debug, Serialize)]
pub struct UploadResponse {
    pub id: String,
    pub created: bool,
    pub message_count: usize,
    pub skipped_lines: usize,
}

/// Query parameters for listing sessions.
#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub q: Option<String>,
    pub project: Option<String>,
}

/// Query parameters for the PTY stream.
#[derive(Debug, Deserialize, Default)]
pub struct PtyStreamParams {
    /// `history` asks for browser-shaped raw replay for a fresh xterm mount.
    /// Missing/anything else uses the self-clearing vt100 snapshot, which is
    /// safer when reconnecting the same mounted terminal because it does not
    /// duplicate scrollback.
    pub replay: Option<String>,
}

/// Query parameters for the user directory search.
#[derive(Debug, Deserialize)]
pub struct UserSearchParams {
    pub q: Option<String>,
    pub limit: Option<i64>,
}

/// Default and fallback page size for the user directory.
const DEFAULT_USER_LIMIT: i64 = 50;

/// Emit transport traffic well inside the load balancer's idle window. A live
/// session can be healthy while both its terminal and command channels are
/// silent, so the database heartbeat alone cannot keep these connections open.
pub(crate) const TRANSPORT_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
pub(crate) const SSE_KEEP_ALIVE_TEXT: &str = "keep-alive";

/// Axum encodes this as an SSE comment (`: keep-alive`), which EventSource
/// clients ignore while intermediaries still count it as connection activity.
pub(crate) fn sse_keep_alive() -> KeepAlive {
    sse_keep_alive_after(TRANSPORT_KEEP_ALIVE_INTERVAL)
}

fn sse_keep_alive_after(interval: Duration) -> KeepAlive {
    KeepAlive::new()
        .interval(interval)
        .text(SSE_KEEP_ALIVE_TEXT)
}

/// Request body for replacing a session's sharing settings.
#[derive(Debug, Deserialize)]
pub struct SharingRequest {
    #[serde(default)]
    pub reason: Option<String>,
    pub visibility: String,
    #[serde(default)]
    pub emails: Vec<String>,
}

/// Request body for renaming a session. A present, non-blank `title` sets the
/// DB-only override; `null`, absent, or a blank/whitespace-only string clears
/// it (reverting to the parsed transcript title). The on-disk JSONL is never
/// modified.
#[derive(Debug, Deserialize)]
pub struct TitleRequest {
    #[serde(default)]
    pub title: Option<String>,
}

/// Request body for replacing a session's repo links (web-UI edit).
#[derive(Debug, Deserialize)]
pub struct ReposRequest {
    #[serde(default)]
    pub repos: Vec<RepoLinkInput>,
}

/// Request/response body for the per-user settings.
#[derive(Debug, Deserialize, Serialize)]
pub struct UserSettingsBody {
    #[serde(default)]
    pub container_paths: Vec<String>,
    #[serde(default)]
    pub default_agent: core::AgentKind,
    #[serde(default)]
    pub default_launcher: core::AgentLauncher,
    /// Inline-code colour config for Slack-friendly terminal copy. Opaque to the
    /// server (the frontend owns the shape, e.g. `{"palette":[],"rgb":[…]}`);
    /// `null`/absent means "unset" and the client falls back to its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_code_colors: Option<serde_json::Value>,
    /// Optional Codex `--sandbox` override for sessions started by the live host.
    /// `None` means "use the host's local Codex config/default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_sandbox_mode: Option<String>,
    /// Optional Codex `--ask-for-approval` override for sessions started by the
    /// live host. `None` means "use the host's local Codex config/default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_approval_policy: Option<String>,
    /// Start OpenCode with `--auto`, automatically approving permissions that
    /// are not explicitly denied. Disabled by default.
    #[serde(default)]
    pub opencode_auto: bool,
    /// Start Claude with `--dangerously-skip-permissions`. Disabled by default.
    #[serde(default)]
    pub claude_skip_permissions: bool,
    /// Re-open an OpenCode session through `adk code` instead of a bare
    /// `opencode`, keeping the monorepo harness. A re-open has only the stored
    /// session, which records the
    /// agent and not the launcher — so this setting is how the user says which
    /// one they meant. Disabled by default.
    #[serde(default)]
    pub adk_reopen: bool,
    /// Let an `adk code` child draw its tmux status strip. Disabled by default:
    /// the strip puts OpenCode inside a tmux pane, and that pane is what the
    /// web terminal would mirror instead of the OpenCode TUI.
    #[serde(default)]
    pub adk_statusline: bool,
}

/// Public metadata for the running application build.
#[derive(Debug, Serialize)]
pub struct AppInfoResponse {
    pub version: &'static str,
}

async fn app_info() -> impl IntoResponse {
    Json(AppInfoResponse {
        version: env!("CARGO_PKG_VERSION"),
    })
}

const CODEX_SANDBOX_MODES: &[&str] = &["read-only", "workspace-write", "danger-full-access"];
const CODEX_APPROVAL_POLICIES: &[&str] = &["untrusted", "on-failure", "on-request", "never"];

/// Liveness probe. No DB access — a DB blip must not restart the pod.
async fn healthz() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Readiness probe. Touches the DB (`SELECT 1`) so a pod with an unreachable /
/// locked backend is taken out of the Service rotation instead of serving 500s.
///
/// Once the process has received a termination signal it reports unready
/// *before* the DB check — this is how a terminating pod is pulled from the
/// Service load-balancer rotation during the graceful-shutdown drain window,
/// regardless of whether the backend is still healthy.
async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if state.is_shutting_down() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "shutting_down" })),
        );
    }
    match state.repo.health().await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ready" }))),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "unready" })),
        ),
    }
}

/// Upsert a session. Returns 201 when newly created, 200 when replaced.
///
/// `owner_email` is derived server-side from the current user (the wire body is
/// unchanged); it is `None` when auth is off. `uploaded_by` is likewise recorded
/// as the authenticated viewer's email when auth is on, falling back to the wire
/// `uploaded_by` (the CLI's `user@host` string) when auth is off.
async fn upload_session(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Json(req): Json<UploadRequest>,
) -> Result<(StatusCode, Json<UploadResponse>)> {
    let skipped_lines = req.session.skipped_lines;
    let id = req.session.id.clone();

    // Record the authenticated viewer as the uploader when auth is on; otherwise
    // fall back to the wire `uploaded_by` (the CLI's `user@host` string). Reused
    // for both the session row and the per-user repo paths.
    let uploader = viewer.as_deref().unwrap_or(&req.uploaded_by);

    let outcome = state
        .repo
        .upsert_session(
            uploader,
            req.source_host.as_deref(),
            viewer.as_deref(),
            &req.session,
        )
        .await?;

    // Merge repo links. Per-user paths key on the authenticated viewer when auth
    // is on, otherwise on the uploaded_by string. Additive — never wipes repos
    // another uploader recorded.
    if !req.repos.is_empty() {
        state
            .repo
            .merge_repos(&id, Some(uploader), &req.repos)
            .await?;
    }

    // Auto-detect PR references in the uploaded messages and link them
    // (best-effort; never fails the upload). repo_ctx resolves from the
    // session's now-merged repo links (single-distinct-repo rule), EXCLUDING
    // any repo the uploader has marked as a monorepo container — so a single
    // remaining work sub-repo lets a bare "PR #N" resolve.
    let uploader_containers = state
        .repo
        .get_container_paths(uploader)
        .await
        .unwrap_or_default();
    let repo_ctx = state.repo.get_repos(&id).await.ok().and_then(|links| {
        repo_ctx_excluding_containers(&links, Some(uploader), &uploader_containers)
    });
    let repo_ctx_ref = repo_ctx.as_ref().map(|(o, r)| (o.as_str(), r.as_str()));
    detect_and_link_prs(
        &state,
        &id,
        repo_ctx_ref,
        req.session.messages.iter().flat_map(pr_message_texts),
    )
    .await;
    // Linear refs need no repo context (URLs are self-contained).
    detect_and_link_linear_docs(
        &state,
        &id,
        req.session.messages.iter().map(|m| m.text.as_str()),
    )
    .await;
    // We just scanned the full transcript, so mark it scanned. This stops the
    // one-time GET backfill from re-detecting (and thereby resurrecting a
    // manually unlinked) ref later — without it, a DELETE issued before the
    // first list would be undone by that first list's backfill scan.
    if let Err(e) = state
        .repo
        .set_linear_scanned_at(&id, chrono::Utc::now())
        .await
    {
        tracing::debug!(session_id = %id, error = %e, "set_linear_scanned_at failed (non-fatal)");
    }

    let status = if outcome.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };

    Ok((
        status,
        Json(UploadResponse {
            id,
            created: outcome.created,
            message_count: outcome.message_count,
            skipped_lines,
        }),
    ))
}

/// List session summaries, optionally filtered by free-text `q` and `project`,
/// and scoped to the current user when one is resolved.
async fn list_sessions(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse> {
    let summaries = state
        .repo
        .list_sessions(
            viewer.as_deref(),
            params.q.as_deref(),
            params.project.as_deref(),
        )
        .await?;
    let container_paths = match viewer.as_deref() {
        Some(email) => state
            .repo
            .get_container_paths(email)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let sessions: Vec<serde_json::Value> = summaries
        .into_iter()
        .map(|summary| summary_with_container_flags(summary, viewer.as_deref(), &container_paths))
        .collect();
    Ok(Json(json!({ "sessions": sessions })))
}

/// Fetch a single session with its ordered messages, scoped to the current user.
///
/// Each repo link in the response gains a computed (NOT stored) `is_container`
/// flag for the requesting viewer: `true` when one of the link's `paths` entries
/// for that viewer matches one of the viewer's configured container paths
/// (normalized). The stored data is unchanged — this is a per-viewer view field.
async fn get_session(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let session = state
        .repo
        .get_session(viewer.as_deref(), &id)
        .await?
        .ok_or(AppError::NotFound)?;

    // Resolve the viewer's container paths (empty for an anonymous viewer or
    // when none are set) to compute the per-link `is_container` flag.
    let container_paths = match viewer.as_deref() {
        Some(email) => state
            .repo
            .get_container_paths(email)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    // A handoff child has no transcript until its agent starts. Its startup
    // state explains an empty page; the brief stays with the source session.
    let handoff = state
        .repo
        .agent_handoff_for_child(&id)
        .await?
        .map(|handoff| {
            json!({
                "source_session_id": handoff.source_session_id,
                "status": handoff.status,
                "error": handoff.error,
                "created_at": handoff.created_at,
            })
        });
    // A managed session is changed only by its publishing host. With auth on
    // only the owner learns which host that is; with auth off there is no
    // owner and anyone may. The id grants nothing: the host verifies every
    // change against its own keys and grants.
    let publication = state.repo.publication_ledger(&id).await?.map(|ledger| {
        let mut marker = json!({"state": ledger.state, "rev": ledger.rev});
        let owner = viewer.is_some() && viewer.as_deref() == ledger.owner_email.as_deref();
        if owner || !state.config.auth_enabled {
            marker["host_id"] = json!(ledger.host_id);
        }
        marker
    });
    let mut value = session_with_container_flags(session, viewer.as_deref(), &container_paths);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("handoff".to_string(), json!(handoff));
        obj.insert("publication".to_string(), json!(publication));
    }
    Ok(Json(value))
}

/// Serialize a [`StoredSession`] to JSON, then annotate each repo link with a
/// computed `is_container` flag for the given viewer (does NOT mutate stored
/// data). A link is a container when one of its `paths` entries owned by
/// `viewer` matches one of the viewer's `container_paths` (normalized).
fn session_with_container_flags(
    session: StoredSession,
    viewer: Option<&str>,
    container_paths: &[String],
) -> serde_json::Value {
    let mut value = serde_json::to_value(&session).unwrap_or_else(|_| json!({}));
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "repos".to_string(),
            json!(repo_links_with_container_flags(
                &session.repos,
                viewer,
                container_paths
            )),
        );
        obj.insert(
            "project_is_container".to_string(),
            json!(project_is_container(
                session.project_path.as_deref(),
                container_paths
            )),
        );
    }
    value
}

fn summary_with_container_flags(
    summary: crate::store::StoredSummary,
    viewer: Option<&str>,
    container_paths: &[String],
) -> serde_json::Value {
    let mut value = serde_json::to_value(&summary).unwrap_or_else(|_| json!({}));
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "repos".to_string(),
            json!(repo_links_with_container_flags(
                &summary.repos,
                viewer,
                container_paths
            )),
        );
        obj.insert(
            "project_is_container".to_string(),
            json!(project_is_container(
                summary.project_path.as_deref(),
                container_paths
            )),
        );
    }
    value
}

/// True when a session's launch directory (`project_path`) is itself one of the
/// viewer's configured container paths (normalized) — i.e. the session was
/// started at a monorepo root rather than inside a work sub-repo. The SPA uses
/// this to suppress the transcript-branch fallback badge: that branch is the
/// container's branch, not a work repo's, so surfacing it reads as if the
/// container were the repo being worked on. Unlike a repo link, `project_path`
/// carries no per-user attribution, so this matches the container paths directly
/// (empty `container_paths` ⇒ always `false`).
fn project_is_container(project_path: Option<&str>, container_paths: &[String]) -> bool {
    match project_path {
        Some(p) => crate::store::is_container_path(p, container_paths),
        None => false,
    }
}

fn repo_links_with_container_flags(
    links: &[crate::store::StoredRepoLink],
    viewer: Option<&str>,
    container_paths: &[String],
) -> Vec<serde_json::Value> {
    links
        .iter()
        .map(|link| repo_link_with_container_flag(link, viewer, container_paths))
        .collect()
}

fn repo_link_with_container_flag(
    link: &crate::store::StoredRepoLink,
    viewer: Option<&str>,
    container_paths: &[String],
) -> serde_json::Value {
    // A link is a container either structurally (CLI-detected monorepo root,
    // `link.is_container`) or because the viewer marked its local path as a
    // container. Either way the UI de-emphasizes it.
    let is_container = link.is_container || link_is_container_for(link, viewer, container_paths);
    let mut repo_json = serde_json::to_value(link).unwrap_or_else(|_| json!({}));
    if let Some(obj) = repo_json.as_object_mut() {
        obj.insert("is_container".to_string(), json!(is_container));
    }
    repo_json
}

/// True when a stored repo link is a container for `viewer`: one of the link's
/// `paths` entries owned by `viewer` matches one of `container_paths`. Returns
/// `false` for an anonymous viewer or a link with no matching per-user path.
fn link_is_container_for(
    link: &crate::store::StoredRepoLink,
    viewer: Option<&str>,
    container_paths: &[String],
) -> bool {
    let Some(viewer) = viewer else {
        return false;
    };
    if container_paths.is_empty() {
        return false;
    }
    link.paths
        .iter()
        .filter(|p| p.user_email == viewer)
        .any(|p| crate::store::is_container_path(&p.local_path, container_paths))
}

/// `GET /api/user/settings` — the current viewer's settings.
///
/// Returns the current viewer's settings; empty/default values when there is no
/// viewer or none are set. Never errors on the no-viewer path.
async fn get_user_settings(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
) -> Result<impl IntoResponse> {
    let (
        container_paths,
        default_agent,
        inline_code_colors,
        codex_launch,
        opencode_launch,
        claude_skip_permissions,
    ) = match viewer.as_deref() {
        Some(email) => (
            state.repo.get_container_paths(email).await?,
            state.repo.get_default_agent(email).await?,
            parse_inline_code_colors(state.repo.get_inline_code_colors(email).await?),
            normalize_codex_launch_settings(state.repo.get_codex_launch_settings(email).await?),
            state.repo.get_opencode_launch_settings(email).await?,
            state.repo.get_claude_skip_permissions(email).await?,
        ),
        None => (
            Vec::new(),
            core::AgentKind::Claude,
            None,
            CodexLaunchSettings::default(),
            OpenCodeLaunchSettings::default(),
            false,
        ),
    };
    Ok(Json(UserSettingsBody {
        container_paths,
        default_agent,
        default_launcher: opencode_launch.default_launcher,
        inline_code_colors,
        codex_sandbox_mode: codex_launch.sandbox_mode,
        codex_approval_policy: codex_launch.approval_policy,
        opencode_auto: opencode_launch.auto,
        claude_skip_permissions,
        adk_reopen: opencode_launch.adk_reopen,
        adk_statusline: opencode_launch.adk_statusline,
    }))
}

/// Parse the stored inline-code-colour JSON blob into a value, dropping a
/// malformed/`NULL` blob to `None` (the client then uses its default).
fn parse_inline_code_colors(stored: Option<String>) -> Option<serde_json::Value> {
    stored.and_then(|s| serde_json::from_str(&s).ok())
}

fn normalize_optional_setting(value: Option<String>, allowed: &[&str]) -> Result<Option<String>> {
    match value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        Some(v) if allowed.contains(&v.as_str()) => Ok(Some(v)),
        Some(v) => Err(AppError::BadRequest(format!(
            "unsupported Codex setting value `{v}`"
        ))),
        None => Ok(None),
    }
}

fn validate_codex_launch_settings(body: &UserSettingsBody) -> Result<CodexLaunchSettings> {
    Ok(CodexLaunchSettings {
        sandbox_mode: normalize_optional_setting(
            body.codex_sandbox_mode.clone(),
            CODEX_SANDBOX_MODES,
        )?,
        approval_policy: normalize_optional_setting(
            body.codex_approval_policy.clone(),
            CODEX_APPROVAL_POLICIES,
        )?,
    })
}

fn normalize_codex_launch_settings(settings: CodexLaunchSettings) -> CodexLaunchSettings {
    CodexLaunchSettings {
        sandbox_mode: normalize_optional_setting(settings.sandbox_mode, CODEX_SANDBOX_MODES)
            .unwrap_or(None),
        approval_policy: normalize_optional_setting(
            settings.approval_policy,
            CODEX_APPROVAL_POLICIES,
        )
        .unwrap_or(None),
    }
}

/// `PUT /api/user/settings` — replace the current viewer's settings.
///
/// Requires an authenticated viewer (401 otherwise). Paths are normalized
/// server-side (trim, drop a single trailing `/`, drop blanks, dedupe).
async fn set_user_settings(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Json(body): Json<UserSettingsBody>,
) -> Result<impl IntoResponse> {
    let Some(email) = viewer.as_deref() else {
        return Err(AppError::Unauthorized(
            "authentication required to update settings".to_string(),
        ));
    };
    if !body.default_launcher.supports(body.default_agent) {
        return Err(AppError::BadRequest(
            "default launcher cannot start the default agent".into(),
        ));
    }
    let codex_launch = validate_codex_launch_settings(&body)?;
    // Ensure a users row exists so the FK on user_container_paths is satisfied.
    state.repo.upsert_user(email, None).await?;
    state
        .repo
        .set_container_paths(email, &body.container_paths)
        .await?;
    state
        .repo
        .set_default_agent(email, body.default_agent)
        .await?;
    // Store the colour config as its compact JSON string (opaque to us), or
    // clear it when absent/null.
    let colors_json = body.inline_code_colors.as_ref().map(|v| v.to_string());
    state
        .repo
        .set_inline_code_colors(email, colors_json.as_deref())
        .await?;
    state
        .repo
        .set_codex_launch_settings(email, &codex_launch)
        .await?;
    state
        .repo
        .set_opencode_launch_settings(
            email,
            OpenCodeLaunchSettings {
                default_launcher: body.default_launcher,
                auto: body.opencode_auto,
                adk_reopen: body.adk_reopen,
                adk_statusline: body.adk_statusline,
            },
        )
        .await?;
    state
        .repo
        .set_claude_skip_permissions(email, body.claude_skip_permissions)
        .await?;
    let claude_skip_permissions = state.repo.get_claude_skip_permissions(email).await?;
    let container_paths = state.repo.get_container_paths(email).await?;
    let default_agent = state.repo.get_default_agent(email).await?;
    let inline_code_colors =
        parse_inline_code_colors(state.repo.get_inline_code_colors(email).await?);
    let codex_launch =
        normalize_codex_launch_settings(state.repo.get_codex_launch_settings(email).await?);
    let opencode_launch = state.repo.get_opencode_launch_settings(email).await?;
    Ok(Json(UserSettingsBody {
        container_paths,
        default_agent,
        default_launcher: opencode_launch.default_launcher,
        inline_code_colors,
        codex_sandbox_mode: codex_launch.sandbox_mode,
        codex_approval_policy: codex_launch.approval_policy,
        opencode_auto: opencode_launch.auto,
        claude_skip_permissions,
        adk_reopen: opencode_launch.adk_reopen,
        adk_statusline: opencode_launch.adk_statusline,
    }))
}

/// Delete a session (and all dependent rows). 404 when absent. **Owner-only**
/// when auth is on (via [`authorize_owner`]): only the owner may delete, and an
/// owner-less/legacy session is not deletable by an arbitrary authenticated
/// user. Auth off (`viewer = None`) stays permissive. Returns 204 on success.
async fn delete_session(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let owner = state.repo.get_session_owner(&id).await?;
    authorize_owner(viewer.as_deref(), owner, "session")?;
    state.repo.delete_session(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Replace a session's sharing settings. **Owner-only** (auth on): only the
/// session's owner may change who it is shared with or flip its visibility —
/// otherwise any authenticated user could make any private session
/// `organization`-visible. 404 when the session is absent, 403 for a non-owner,
/// 400 on invalid visibility. Auth off (`viewer = None`) stays permissive.
/// Returns the updated `{ visibility, emails }`.
async fn set_sharing(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<SharingRequest>,
) -> Result<impl IntoResponse> {
    // 404 before touching sharing if the session doesn't exist; owner-gate it.
    let owner = state.repo.get_session_owner(&id).await?;
    authorize_owner(viewer.as_deref(), owner, "session")?;
    state
        .repo
        .update_sharing_context(
            &id,
            &crate::store::notifications::SharingUpdate {
                sender: viewer.clone(),
                reason: req.reason,
                visibility: Some(req.visibility),
                emails: req.emails,
                shares: vec![],
            },
        )
        .await?;
    let (visibility, emails) = state
        .repo
        .get_sharing(&id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(json!({ "visibility": visibility, "emails": emails })))
}

/// Replace a session's repo links. **Owner-only** (auth on): repo links carry
/// per-user local paths and drive PR/branch context, so only the owner may
/// rewrite them. 404 when the session is absent, 403 for a non-owner. Auth off
/// (`viewer = None`) stays permissive. Returns the updated `{ repos }` (each with
/// per-user paths).
async fn set_repos(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<ReposRequest>,
) -> Result<impl IntoResponse> {
    let owner = state.repo.get_session_owner(&id).await?;
    authorize_owner(viewer.as_deref(), owner, "session")?;
    state.repo.set_repos(&id, &req.repos).await?;
    let repos = state.repo.get_repos(&id).await?;
    Ok(Json(json!({ "repos": repos })))
}

/// `PUT /api/sessions/{id}/title` — rename a session via a DB-only override.
/// Owner-only when auth is on; 404 when the session is absent. A blank/absent
/// title clears the override (reverts to the parsed transcript title). The
/// on-disk JSONL transcript is never touched. Returns the effective
/// `{ title }`.
async fn set_session_title(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<TitleRequest>,
) -> Result<impl IntoResponse> {
    let owner = state.repo.get_session_owner(&id).await?;
    authorize_owner(viewer.as_deref(), owner, "session")?;

    // A blank/whitespace-only title clears the override rather than storing it.
    let trimmed = req
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    state.repo.set_title_override(&id, trimmed).await?;

    // Reflect the effective title (override when set, else parsed) back.
    let title = state
        .repo
        .get_session(viewer.as_deref(), &id)
        .await?
        .map(|s| s.title)
        .ok_or(AppError::NotFound)?;
    Ok(Json(json!({ "title": title })))
}

/// List distinct project paths with their session counts.
async fn list_projects(State(state): State<AppState>) -> Result<impl IntoResponse> {
    let projects = state.repo.list_projects().await?;
    Ok(Json(json!({ "projects": projects })))
}

// ---------------------------------------------------------------------------
// Projects + chats (explicit groupings and saved threads)
// ---------------------------------------------------------------------------

/// Owner-only authorization for the session mutators (`delete_session`,
/// `set_sharing`, `set_repos`, `set_session_title`).
///
/// `owner` is the lookup result for the entity's owner (`get_*_owner`):
/// `None` ⇒ the entity does not exist (→ 404); `Some(stored_owner)` ⇒ it
/// exists, with `stored_owner` possibly `None` (owner-less — registered with
/// auth off / legacy).
///
/// Authorization is **fail-closed when auth is on**: a known `viewer` is allowed
/// only when it equals a known `stored_owner`. An owner-less row is NOT mutable
/// by an arbitrary authenticated user (it would otherwise let anyone re-share or
/// re-point any legacy session). A `None` viewer (auth off ⇒ no ownership
/// surface) stays fully permissive. `entity` names the resource for the 403.
fn authorize_owner(
    viewer: Option<&str>,
    owner: Option<Option<String>>,
    entity: &str,
) -> Result<()> {
    let owner = owner.ok_or(AppError::NotFound)?;
    match (viewer, owner.as_deref()) {
        // Auth off: no ownership surface — permissive.
        (None, _) => Ok(()),
        // Auth on: only the matching owner may mutate.
        (Some(viewer), Some(owner)) if viewer == owner => Ok(()),
        (Some(_), _) => Err(AppError::Forbidden(format!(
            "only the owner can modify this {entity}"
        ))),
    }
}

/// Live-session read authorization. Ok when auth is off (`viewer = None`), or
/// the viewer is the author, or the viewer has a read share (`can_read`). Otherwise `NotFound` — a non-viewer must not learn the session even
/// exists. The caller resolves the `Option<(owner, shares)>` from
/// `get_live_session_access` first; a `None` outer ⇒ 404 before this is called.
fn authorize_live_view(
    viewer: Option<&str>,
    owner: Option<String>,
    shares: &[LiveShare],
) -> Result<()> {
    let Some(viewer) = viewer else {
        return Ok(());
    };
    if owner.as_deref() == Some(viewer) {
        return Ok(());
    }
    if shares.iter().any(|s| s.email == viewer && s.can_read) {
        return Ok(());
    }
    Err(AppError::NotFound)
}

/// Live-session author-only authorization (the CLI-side routes + share
/// management). Ok when auth is off or the viewer is the author; else
/// `Forbidden`. Shares — at any level — never confer author rights.
pub(crate) fn authorize_live_author(viewer: Option<&str>, owner: Option<String>) -> Result<()> {
    let Some(viewer) = viewer else {
        return Ok(());
    };
    if owner.as_deref() == Some(viewer) {
        return Ok(());
    }
    Err(AppError::Forbidden(
        "only the author can manage this live session".to_string(),
    ))
}

async fn require_live_author(state: &AppState, id: &str, viewer: Option<&str>) -> Result<()> {
    let (owner, _) = state
        .repo
        .get_live_session_access(id)
        .await?
        .ok_or(AppError::NotFound)?;
    authorize_live_author(viewer, owner)
}

async fn require_live_view(state: &AppState, id: &str, viewer: Option<&str>) -> Result<()> {
    let (owner, shares) = state
        .repo
        .get_live_session_access(id)
        .await?
        .ok_or(AppError::NotFound)?;
    authorize_live_view(viewer, owner, &shares)
}

/// Request body for creating/renaming a project.
#[derive(Debug, Deserialize)]
pub struct ProjectRequest {
    pub name: String,
    /// Optional free-text description. Absent or explicit `null` ⇒ `None`.
    #[serde(default)]
    pub description: Option<String>,
}

/// Request body for assigning (or, with `null`, clearing) a session's project.
#[derive(Debug, Deserialize)]
pub struct SetProjectRequest {
    #[serde(default)]
    pub project_id: Option<String>,
}

/// Request body for creating a chat thread.
#[derive(Debug, Deserialize)]
pub struct CreateChatRequest {
    #[serde(default)]
    pub title: Option<String>,
}

/// Request body for updating a chat thread (toggle shared, optional rename).
#[derive(Debug, Deserialize)]
pub struct UpdateChatRequest {
    pub shared: bool,
    #[serde(default)]
    pub title: Option<String>,
}

/// Request body for posting a message to a chat thread.
#[derive(Debug, Deserialize)]
pub struct ChatMessageRequest {
    pub content: String,
    /// Optional per-request model override. When `Some` and non-empty, this
    /// model is used instead of the configured default for this one turn.
    /// An empty string is treated as absent (uses the default).
    #[serde(default)]
    pub model: Option<String>,
}

/// Query parameters for `DELETE /api/sessions/{id}/prs`.
#[derive(Debug, Deserialize)]
pub struct PrUnlinkParams {
    pub owner: String,
    pub repo: String,
    pub number: i64,
}

/// Request body for `POST /api/sessions/{id}/prs`.
#[derive(Debug, Deserialize)]
pub struct LinkPrRequest {
    pub url: String,
}

/// Query parameters for `DELETE /api/sessions/{id}/linear-docs` — the canonical
/// Linear URL identifying the ref to unlink.
#[derive(Debug, Deserialize)]
pub struct LinearDocUnlinkParams {
    pub url: String,
}

/// Request body for `POST /api/sessions/{id}/prs/status` — the per-user CLI
/// pushing the PR status it fetched via `gh`. `fetched_at` is stamped
/// server-side; the body only carries the identifiers + the status snapshot.
#[derive(Debug, Deserialize)]
pub struct PushPrStatusRequest {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub status: crate::github::PrStatus,
    /// The publication snapshot (`publication` of `GET /api/sessions/{id}/prs`)
    /// the producer read the PR link at. Required for a published session,
    /// absent for an unmanaged one.
    #[serde(default)]
    pub publication: Option<core::publication::PublicationSnapshot>,
}

/// Request body for `POST /api/sessions/{id}/prs/comments` — the per-user CLI
/// pushing the PR comments it fetched via `gh`. `ingested_at` is stamped
/// server-side; the body carries the PR identifiers + the comment snapshot.
#[derive(Debug, Deserialize)]
pub struct PushPrCommentsRequest {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub comments: Vec<crate::github::PrComment>,
    /// As [`PushPrStatusRequest::publication`].
    #[serde(default)]
    pub publication: Option<core::publication::PublicationSnapshot>,
}

/// The source context a producer claims for a pushed cache.
fn pushed_source(publication: Option<core::publication::PublicationSnapshot>) -> SourceContext {
    publication.map_or(SourceContext::Unmanaged, SourceContext::Published)
}

/// Request body for `POST /api/sessions/{id}/prs/reply` — relay a reply-to-PR
/// command to the attached CLI (which runs `gh pr comment`). The server holds
/// no GitHub creds and never posts the comment itself.
#[derive(Debug, Deserialize)]
pub struct PrReplyRequest {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub body: String,
    /// When set, the GitHub `databaseId` of the review comment to reply to in
    /// its thread. Absent ⇒ a top-level PR comment (unchanged behavior).
    #[serde(default)]
    pub in_reply_to: Option<i64>,
}

/// `GET /api/projects` — list explicit projects (each with its session count).
/// The count is visibility-filtered for an authenticated viewer, matching
/// `GET /api/sessions`.
async fn list_projects_detailed(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
) -> Result<impl IntoResponse> {
    let projects = state.repo.list_projects_detailed(viewer.as_deref()).await?;
    Ok(Json(json!({ "projects": projects })))
}

/// `POST /api/projects` — create a project owned by the current user (when auth
/// is on). Returns 201 with the created project.
async fn create_project(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Json(req): Json<ProjectRequest>,
) -> Result<(StatusCode, Json<crate::store::Project>)> {
    let project = state
        .repo
        .create_project(viewer.as_deref(), &req.name, req.description.as_deref())
        .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

/// `GET /api/projects/{id}` — a project with its member session summaries.
/// 404 when absent. The project row is visible to any viewer (no per-project
/// visibility in v1), but the member summaries (and `session_count`) are
/// visibility-filtered for an authenticated viewer, matching
/// `GET /api/sessions`.
async fn get_project(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    match state.repo.get_project(viewer.as_deref(), &id).await? {
        Some(detail) => Ok(Json(detail)),
        None => Err(AppError::NotFound),
    }
}

/// `PUT /api/projects/{id}` — rename a project. Owner-only when auth is on;
/// 404 when absent.
async fn update_project(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<ProjectRequest>,
) -> Result<impl IntoResponse> {
    let owner = state.repo.get_project_owner(&id).await?;
    authorize_owner(viewer.as_deref(), owner, "project")?;
    state
        .repo
        .update_project(&id, &req.name, req.description.as_deref())
        .await?;
    match state.repo.get_project(viewer.as_deref(), &id).await? {
        Some(detail) => Ok(Json(detail.project)),
        None => Err(AppError::NotFound),
    }
}

/// `DELETE /api/projects/{id}` — delete a project (unassigns its sessions,
/// cascades its project-chats). Owner-only when auth is on; 404 when absent.
/// Returns 204.
async fn delete_project(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let owner = state.repo.get_project_owner(&id).await?;
    authorize_owner(viewer.as_deref(), owner, "project")?;
    state.repo.delete_project(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/sessions/{id}/project` — assign (or, with `null`, clear) a
/// session's project. The viewer must be able to view the session (404 else);
/// the store surfaces 404 for an absent session and 400 for an unknown project.
async fn set_session_project(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<SetProjectRequest>,
) -> Result<impl IntoResponse> {
    authorize_session_view(&state, viewer.as_deref(), &id).await?;
    state
        .repo
        .set_session_project(&id, req.project_id.as_deref())
        .await?;
    Ok(Json(json!({ "project_id": req.project_id })))
}

/// `GET /api/sessions/{id}/chats` — session chat threads visible to the viewer.
/// The viewer must be able to view the session (404 else).
async fn list_session_chats(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    authorize_session_view(&state, viewer.as_deref(), &id).await?;
    let chats = state
        .repo
        .list_chats(&ChatParent::Session(id), viewer.as_deref())
        .await?;
    Ok(Json(json!({ "chats": chats })))
}

/// `POST /api/sessions/{id}/chats` — create a session chat thread. The viewer
/// must be able to view the session (404 else). Returns 201.
async fn create_session_chat(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<CreateChatRequest>,
) -> Result<(StatusCode, Json<crate::store::Chat>)> {
    authorize_session_view(&state, viewer.as_deref(), &id).await?;
    let source = state.repo.source_context(&id).await?;
    let chat = state
        .repo
        .create_chat(
            &ChatParent::Session(id),
            source,
            viewer.as_deref(),
            req.title.as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(chat)))
}

/// `GET /api/projects/{id}/chats` — project chat threads visible to the viewer.
/// The project must exist (404 else); projects are viewable by any viewer.
async fn list_project_chats(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    if state.repo.get_project_owner(&id).await?.is_none() {
        return Err(AppError::NotFound);
    }
    let chats = state
        .repo
        .list_chats(&ChatParent::Project(id), viewer.as_deref())
        .await?;
    Ok(Json(json!({ "chats": chats })))
}

/// `POST /api/projects/{id}/chats` — create a project chat thread. The project
/// must exist (404 else). Returns 201.
async fn create_project_chat(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<CreateChatRequest>,
) -> Result<(StatusCode, Json<crate::store::Chat>)> {
    if state.repo.get_project_owner(&id).await?.is_none() {
        return Err(AppError::NotFound);
    }
    let chat = state
        .repo
        .create_chat(
            &ChatParent::Project(id),
            SourceContext::Unmanaged,
            viewer.as_deref(),
            req.title.as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(chat)))
}

/// Can the `viewer` read this `chat`? Owner ⇒ yes. Otherwise the chat must be
/// `shared` AND the viewer must be able to view its parent (a session via
/// `get_session`; a project is viewable by any viewer in v1). Auth-off (viewer
/// `None`) ⇒ yes. Returns 404 when not allowed, to avoid leaking existence.
async fn authorize_chat_read(
    state: &AppState,
    viewer: Option<&str>,
    chat: &crate::store::Chat,
) -> Result<()> {
    // Auth off: everyone may read.
    let Some(viewer) = viewer else {
        return Ok(());
    };
    // Owner may always read.
    if chat.owner_email.as_deref() == Some(viewer) {
        return Ok(());
    }
    // Otherwise it must be shared AND the viewer must see the parent.
    if chat.shared {
        let can_view_parent = match (&chat.session_id, &chat.project_id) {
            (Some(session_id), _) => state
                .repo
                .get_session(Some(viewer), session_id)
                .await?
                .is_some(),
            // Projects are viewable by any viewer in v1.
            (None, Some(_)) => true,
            // The CHECK guarantees exactly one parent; treat a malformed row as
            // not viewable.
            (None, None) => false,
        };
        if can_view_parent {
            return Ok(());
        }
    }
    // Not owner, not (shared + viewable) ⇒ hide it.
    Err(AppError::NotFound)
}

/// `GET /api/chats/{id}` — a chat thread with its messages. Readable by the
/// owner, or by anyone who can view the parent when the thread is shared
/// (404 otherwise, to avoid leaking existence).
async fn get_chat(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let with_messages = state
        .repo
        .get_chat_with_messages(&id)
        .await?
        .ok_or(AppError::NotFound)?;
    authorize_chat_read(&state, viewer.as_deref(), &with_messages.chat).await?;
    Ok(Json(with_messages))
}

/// `PUT /api/chats/{id}` — toggle `shared` and optionally rename. Owner-only
/// when auth is on; 404 when absent.
async fn update_chat(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<UpdateChatRequest>,
) -> Result<impl IntoResponse> {
    let owner = state.repo.get_chat(&id).await?.map(|chat| chat.owner_email);
    authorize_owner(viewer.as_deref(), owner, "chat")?;
    state
        .repo
        .update_chat(&id, req.shared, req.title.as_deref())
        .await?;
    match state.repo.get_chat(&id).await? {
        Some(chat) => Ok(Json(chat)),
        None => Err(AppError::NotFound),
    }
}

/// `DELETE /api/chats/{id}` — delete a chat thread (messages cascade).
/// Owner-only when auth is on; 404 when absent. Returns 204.
async fn delete_chat(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let owner = state.repo.get_chat(&id).await?.map(|chat| chat.owner_email);
    authorize_owner(viewer.as_deref(), owner, "chat")?;
    state.repo.delete_chat(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/llm/status` — whether LLM features are enabled and which model is
/// configured. Ungated (like `/api/me`) so the SPA can show/hide LLM UI without
/// being authenticated. `enabled` is false (and `model` null) when no key is set.
async fn llm_status(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "enabled": state.config.llm_enabled(),
        "model": state.llm.as_ref().map(|c| c.model()),
    }))
}

/// `GET /api/llm/models` — proxy the OpenRouter model catalog and return a
/// subset useful for model selection. 503 when LLM features are not configured.
///
/// Response shape: `{ "models": [ { "id", "name", "context_length" } ] }`.
/// Fields missing from OpenRouter's response default to sensible values (empty
/// string / 0) so the list never breaks on schema drift.
async fn llm_models(State(state): State<AppState>) -> Result<impl IntoResponse> {
    let llm = state
        .llm
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("LLM features are not configured".into()))?;

    let url = format!(
        "{}/models",
        state.config.openrouter_base_url.trim_end_matches('/')
    );
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(
            state
                .config
                .openrouter_api_key
                .as_deref()
                .unwrap_or_default(),
        )
        .send()
        .await
        .map_err(|e| {
            AppError::Internal(anyhow::anyhow!("OpenRouter /models request failed: {e}"))
        })?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(anyhow::anyhow!(
            "OpenRouter /models returned HTTP {status}: {text}"
        )));
    }

    // Parse the OpenRouter model list. The shape is `{ "data": [ { "id", "name",
    // "context_length", ... } ] }`. Unknown fields are ignored.
    #[derive(serde::Deserialize)]
    struct OrModelsResponse {
        #[serde(default)]
        data: Vec<OrModel>,
    }
    #[derive(serde::Deserialize)]
    struct OrModel {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        context_length: Option<u64>,
    }

    let parsed: OrModelsResponse = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("OpenRouter /models parse error: {e}")))?;

    let models: Vec<serde_json::Value> = parsed
        .data
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "name": m.name,
                "context_length": m.context_length.unwrap_or(0),
            })
        })
        .collect();

    // Suppress the unused-variable warning from the borrow above; the llm ref
    // is only needed to gate the 503 check.
    let _ = llm;

    Ok(Json(json!({ "models": models })))
}

// ---------------------------------------------------------------------------
// Session PR links
// ---------------------------------------------------------------------------

/// A PR enriched with its CACHED GitHub status (pushed by the per-user CLI;
/// the server never fetches it).
///
/// Used by both `GET /api/sessions/{id}/prs` (list) and
/// `POST /api/sessions/{id}/prs` (single link).
#[derive(Debug, serde::Serialize)]
pub struct PrWithStatus {
    url: String,
    owner: String,
    repo: String,
    number: i64,
    /// `null` until the CLI has pushed a status snapshot for this PR.
    status: Option<crate::github::PrStatus>,
    /// RFC3339 timestamp of the last CLI push, or `null` when never pushed.
    status_fetched_at: Option<String>,
    /// `true` when this PR was auto-detected from conversation text; `false`
    /// for a manually-linked PR.
    detected: bool,
}

impl From<crate::store::SessionPrWithStatus> for PrWithStatus {
    fn from(pr: crate::store::SessionPrWithStatus) -> Self {
        // Reassemble the PrStatus DTO from the cached columns. We only surface
        // a status object once a snapshot has actually been pushed — keyed off
        // `status_fetched_at` being present (the columns are set together).
        let status = pr.status_fetched_at.map(|_| crate::github::PrStatus {
            state: pr.status_state.unwrap_or_default(),
            merged: pr.status_merged.unwrap_or(false),
            draft: pr.status_draft.unwrap_or(false),
            title: pr.status_title.unwrap_or_default(),
            review_decision: pr.status_review_decision,
        });
        PrWithStatus {
            url: pr.url,
            owner: pr.owner,
            repo: pr.repo,
            number: pr.number,
            status,
            status_fetched_at: pr.status_fetched_at.map(|t| t.to_rfc3339()),
            detected: pr.detected,
        }
    }
}

/// Resolve a session's PR-detection repo context from its repo links.
///
/// `PR #N` / `pull request #N` references only resolve when the session maps to
/// exactly ONE distinct `(owner, repo)`. Each repo link contributes its
/// `(owner, repo)` derived from `remote_url` (`parse_repo_remote`) and, as a
/// fallback, from any `pr_url` (`parse_pr_url`). Returns `Some((owner, repo))`
/// only when every contributing link agrees on a single pair; `None` when zero
/// or more than one distinct pair is found (ambiguous ⇒ never guess).
fn repo_ctx_from_links(links: &[crate::store::StoredRepoLink]) -> Option<(String, String)> {
    let mut found: Option<(String, String)> = None;
    for link in links {
        // A monorepo container (CLI-detected top-level repo) must never be the
        // PR-detection context — a bare `PR #N` in it refers to a sub-repo PR,
        // not a top-level one. Skip it (the remaining sub-repo link, if any,
        // still resolves a context).
        if link.is_container {
            continue;
        }
        let pair = link
            .remote_url
            .as_deref()
            .and_then(crate::github::parse_repo_remote)
            .or_else(|| {
                link.pr_url
                    .as_deref()
                    .and_then(crate::github::parse_pr_url)
                    .map(|(o, r, _)| (o, r))
            });
        let Some(pair) = pair else { continue };
        match &found {
            None => found = Some(pair),
            Some(existing) if *existing == pair => {}
            // A second distinct (owner, repo) ⇒ ambiguous; bail.
            Some(_) => return None,
        }
    }
    found
}

/// Like [`repo_ctx_from_links`] but first excludes any stored link that is a
/// container for `viewer` (one of the link's `paths` entries owned by `viewer`
/// matches a `container_paths` entry, normalized). Excluding the container repo
/// lets a session with a single remaining work sub-repo resolve a bare "PR #N".
/// With an empty `container_paths` this is identical to `repo_ctx_from_links`.
fn repo_ctx_excluding_containers(
    links: &[crate::store::StoredRepoLink],
    viewer: Option<&str>,
    container_paths: &[String],
) -> Option<(String, String)> {
    if container_paths.is_empty() {
        return repo_ctx_from_links(links);
    }
    let filtered: Vec<crate::store::StoredRepoLink> = links
        .iter()
        .filter(|link| !link_is_container_for(link, viewer, container_paths))
        .cloned()
        .collect();
    repo_ctx_from_links(&filtered)
}

/// Resolve a live session's single-repo context `(owner, repo)` for PR
/// auto-linking. A live (ungraduated) session has no `session_repos` rows (that
/// table's FK still needs a `sessions` row), so the launch repo's `origin`
/// remote is carried on the `live_sessions` row instead — parse that first.
/// Falls back to any repo links recorded for the id (covers an already-graduated
/// id). `None` when neither yields a remote (older CLI / non-git launch) ⇒ only
/// full PR URLs link.
async fn live_repo_ctx(state: &AppState, id: &str) -> Option<(String, String)> {
    let live = state.repo.get_live_session(id).await.ok().flatten();
    live_repo_ctx_from(state, id, live.as_ref()).await
}

/// [`live_repo_ctx`] with a pre-fetched live session (avoids re-querying when
/// the caller already has it). `live` may be `None` (e.g. an already-graduated
/// id) — then only the `get_repos` fallback applies.
async fn live_repo_ctx_from(
    state: &AppState,
    id: &str,
    live: Option<&crate::store::LiveSession>,
) -> Option<(String, String)> {
    if let Some(pair) = live
        .and_then(|ls| ls.remote_url.as_deref())
        .and_then(crate::github::parse_repo_remote)
    {
        return Some(pair);
    }
    state
        .repo
        .get_repos(id)
        .await
        .ok()
        .and_then(|links| repo_ctx_from_links(&links))
}

/// PR links come from conversation text, not search results or internal reasoning.
pub(crate) fn pr_message_texts(message: &core::Message) -> impl Iterator<Item = String> + '_ {
    message
        .blocks
        .split(|block| !matches!(block, core::ContentBlock::Text { .. }))
        .filter(|blocks| !blocks.is_empty())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| match block {
                    core::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .chain(
            (message.blocks.is_empty() && message.kind == core::MessageKind::Text)
                .then(|| message.text.clone()),
        )
}

/// Detect PR references across `messages` and auto-link the union into
/// `session_prs` (detected = true). Best-effort: any error is logged and
/// swallowed so the surrounding ingest/GET path never fails.
///
/// `repo_ctx` is the session's single-repo context (see `repo_ctx_from_links`);
/// when `None`, bare `PR #N` refs are skipped. Full PR URLs are additionally
/// gated to the session's own repo owners (derived here from the stored links),
/// so a transcript that merely quotes an unrelated PR URL — a doc example, or
/// this repo's `acme/widgets` test fixtures — is not linked as a real PR.
async fn detect_and_link_prs(
    state: &AppState,
    session_id: &str,
    repo_ctx: Option<(&str, &str)>,
    messages: impl IntoIterator<Item = impl AsRef<str>>,
) -> bool {
    // The set of GitHub owners this session legitimately touches. Empty ⇒ no
    // known repos ⇒ don't restrict full-URL linking (legacy behavior).
    let allowed_owners = state
        .repo
        .get_repos(session_id)
        .await
        .map(|links| owners_from_links(&links))
        .unwrap_or_default();

    let mut refs: Vec<crate::store::SessionPr> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, i64)> =
        std::collections::HashSet::new();
    for text in messages {
        for pr in crate::github::detect_pr_refs(text.as_ref(), repo_ctx, &allowed_owners) {
            if seen.insert((pr.owner.clone(), pr.repo.clone(), pr.number)) {
                refs.push(pr);
            }
        }
    }
    if refs.is_empty() {
        return true;
    }
    match state.repo.auto_link_session_prs(session_id, &refs).await {
        // A genuinely new link landed — nudge any live subscribers to refetch
        // the PR list so the widgets update without a manual "Sync now". A
        // re-seen ref inserts 0 rows and publishes nothing. On the upload path
        // there are no subscribers, so this is a cheap no-op (0 receivers).
        Ok(inserted) if inserted > 0 => {
            state.live.publish(
                session_id,
                LiveEvent::PrsLinked {
                    session_id: session_id.to_string(),
                },
            );
            true
        }
        Ok(_) => true,
        Err(e) => {
            tracing::debug!(session_id, error = %e, "auto-link PR detection failed (non-fatal)");
            false
        }
    }
}

/// Detect Linear references in `messages` and auto-link them to the session.
///
/// The Linear analogue of [`detect_and_link_prs`], but simpler: Linear URLs are
/// self-contained, so there is no repo context or owner gating. A pure
/// text-parse plus an idempotent insert; best-effort throughout (a failure
/// never breaks the surrounding ingest). On a genuinely new link it publishes
/// `LiveEvent::LinearDocsLinked` so live browsers refetch without a reload.
async fn detect_and_link_linear_docs(
    state: &AppState,
    session_id: &str,
    messages: impl IntoIterator<Item = &str>,
) {
    let mut refs: Vec<crate::store::SessionLinearDoc> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for text in messages {
        for doc in crate::linear::detect_linear_refs(text) {
            if seen.insert(doc.url.clone()) {
                refs.push(doc);
            }
        }
    }
    if refs.is_empty() {
        return;
    }
    match state
        .repo
        .auto_link_session_linear_docs(session_id, &refs)
        .await
    {
        Ok(inserted) if inserted > 0 => {
            state.live.publish(
                session_id,
                LiveEvent::LinearDocsLinked {
                    session_id: session_id.to_string(),
                },
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::debug!(session_id, error = %e, "auto-link Linear detection failed (non-fatal)");
        }
    }
}

/// The distinct GitHub owners across a session's repo links (from each link's
/// `remote_url`, falling back to its `pr_url`). Used to gate full-URL PR
/// detection to the session's own org(s). Container links are included — their
/// owner is the same org as the work sub-repos.
fn owners_from_links(links: &[crate::store::StoredRepoLink]) -> Vec<String> {
    let mut owners: Vec<String> = Vec::new();
    for link in links {
        let owner = link
            .remote_url
            .as_deref()
            .and_then(crate::github::parse_repo_remote)
            .map(|(o, _)| o)
            .or_else(|| {
                link.pr_url
                    .as_deref()
                    .and_then(crate::github::parse_pr_url)
                    .map(|(o, _, _)| o)
            });
        if let Some(owner) = owner {
            if !owners.iter().any(|o| o.eq_ignore_ascii_case(&owner)) {
                owners.push(owner);
            }
        }
    }
    owners
}

/// Authorize that the current viewer can see the session (404 otherwise), and
/// return the viewer string for convenience. Mirrors the pattern used by
/// `list_session_chats` and `create_session_chat`.
async fn authorize_session_view(
    state: &AppState,
    viewer: Option<&str>,
    session_id: &str,
) -> Result<()> {
    if state.repo.get_session(viewer, session_id).await?.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Authorize *view* access to a session that may be stored **or** live.
///
/// Tries the stored `sessions` row first (visibility-checked via `get_session`);
/// when none exists, falls back to the live-session access list. A live session
/// has no `sessions` row until it graduates, but its id IS the eventual
/// `sessions.id`, so PR linking/listing must resolve against the live id too.
/// 404 when neither path resolves (or the live session isn't visible).
///
/// Used by the PR link/list/unlink handlers so a PR can be attached to a
/// session while it is still live (the `session_prs` FK was relaxed to allow
/// the row; see migration 0016/0015).
async fn authorize_session_or_live_view(
    state: &AppState,
    viewer: Option<&str>,
    session_id: &str,
) -> Result<()> {
    if state.repo.get_session(viewer, session_id).await?.is_some() {
        return Ok(());
    }
    match state.repo.get_live_session_access(session_id).await? {
        Some((owner, shares)) => authorize_live_view(viewer, owner, &shares),
        None => Err(AppError::NotFound),
    }
}

/// `GET /api/sessions/{id}/prs` — list PRs linked to a session.
///
/// Returns the CACHED GitHub status the per-user CLI last pushed (the server
/// holds no GitHub creds and never fetches). Returns 404 when the session is
/// absent or not visible to the viewer.
///
/// Response: `{ "prs": [ { url, owner, repo, number, status, status_fetched_at } ] }`
/// where `status` is `{ state, merged, draft, title }` or `null`, and
/// `status_fetched_at` is an RFC3339 string or `null`.
async fn list_session_prs(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    authorize_session_or_live_view(&state, viewer.as_deref(), &id).await?;

    // One-time backfill: when this session has never been PR-scanned, scan ALL
    // of its stored messages (uploaded / graduated + any live messages), link
    // the detected union, then stamp `pr_scanned_at` so it never runs again.
    // Best-effort — a failure here must not break the listing. Forward messages
    // on a live session are covered by ingest-time detection.
    if state
        .repo
        .pr_scanned_at(&id)
        .await
        .unwrap_or(None)
        .is_none()
    {
        backfill_session_prs(&state, &id, viewer.as_deref()).await;
        // Stamp regardless of detection outcome so backfill runs at most once.
        if let Err(e) = state.repo.set_pr_scanned_at(&id, chrono::Utc::now()).await {
            tracing::debug!(session_id = %id, error = %e, "set_pr_scanned_at failed (non-fatal)");
        }
    }

    // Read before the links: a cache pushed for them carries this snapshot.
    let publication = match state.repo.source_context(&id).await? {
        SourceContext::Unmanaged => None,
        SourceContext::Published(snapshot) => Some(snapshot),
    };
    let prs = state.repo.list_session_prs(&id).await?;
    let enriched: Vec<PrWithStatus> = prs.into_iter().map(PrWithStatus::from).collect();
    Ok(Json(json!({ "prs": enriched, "publication": publication })))
}

/// One-time PR-detection backfill over a session's stored messages. Gathers the
/// session's uploaded/graduated messages plus any retained live messages,
/// computes the repo context, and auto-links the detected union. Best-effort —
/// the caller swallows nothing here; this function logs and returns on error.
async fn backfill_session_prs(state: &AppState, session_id: &str, viewer: Option<&str>) {
    // Collect message text from the stored session (uploaded or graduated).
    let mut texts: Vec<String> = Vec::new();
    if let Ok(Some(session)) = state.repo.get_session(None, session_id).await {
        texts.extend(session.messages.iter().flat_map(pr_message_texts));
    }
    // Include any live messages still retained under the same id (covers the
    // case where graduation copied messages but also where it has not yet run).
    if let Ok(live_msgs) = state.repo.get_live_messages(session_id).await {
        texts.extend(live_msgs.iter().flat_map(|m| pr_message_texts(&m.message)));
    }
    if texts.is_empty() {
        return;
    }

    // Exclude the requesting viewer's container repos from the PR-detection
    // context so a single remaining work sub-repo lets a bare "PR #N" resolve.
    let container_paths = match viewer {
        Some(email) => state
            .repo
            .get_container_paths(email)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let repo_ctx = state
        .repo
        .get_repos(session_id)
        .await
        .ok()
        .and_then(|links| repo_ctx_excluding_containers(&links, viewer, &container_paths));
    let repo_ctx_ref = repo_ctx.as_ref().map(|(o, r)| (o.as_str(), r.as_str()));
    detect_and_link_prs(
        state,
        session_id,
        repo_ctx_ref,
        texts.iter().map(String::as_str),
    )
    .await;
}

/// `POST /api/sessions/{id}/prs` — link a GitHub PR to a session.
///
/// Body: `{ "url": "<github pr url>" }`. Parses `owner/repo/number` from the
/// URL; returns 400 on a non-PR or malformed URL. Idempotent: linking the same
/// PR twice is a no-op (returns the existing row). Returns 404 when the session
/// is absent or not visible to the viewer.
///
/// Linking only stores the PR — status is `null` until the per-user CLI pushes
/// a snapshot via `POST /api/sessions/{id}/prs/status`.
///
/// Response: `{ url, owner, repo, number, status, status_fetched_at }`.
async fn link_session_pr(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<LinkPrRequest>,
) -> Result<impl IntoResponse> {
    authorize_session_or_live_view(&state, viewer.as_deref(), &id).await?;

    let (owner, repo, number) = crate::github::parse_pr_url(&req.url).ok_or_else(|| {
        AppError::BadRequest(format!("{:?} is not a valid GitHub PR URL", req.url))
    })?;

    let pr = SessionPr {
        url: req.url.clone(),
        owner: owner.clone(),
        repo: repo.clone(),
        number,
    };
    state.repo.link_session_pr(&id, &pr).await?;

    Ok(Json(json!({
        "url": pr.url,
        "owner": pr.owner,
        "repo": pr.repo,
        "number": pr.number,
        "status": serde_json::Value::Null,
        "status_fetched_at": serde_json::Value::Null,
        "detected": false,
    })))
}

/// `DELETE /api/sessions/{id}/prs?owner=&repo=&number=` — unlink a PR from a
/// session. Returns 404 when the session is absent/invisible or when no matching
/// PR link exists. Returns 204 on success.
async fn unlink_session_pr(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Query(params): Query<PrUnlinkParams>,
) -> Result<StatusCode> {
    authorize_session_or_live_view(&state, viewer.as_deref(), &id).await?;

    let removed = state
        .repo
        .unlink_session_pr(&id, &params.owner, &params.repo, params.number)
        .await?;

    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

/// `GET /api/sessions/{id}/linear-docs` — list the Linear references (document /
/// issue / project) linked to a session.
///
/// The Linear analogue of [`list_session_prs`], minus all status: the server
/// holds no Linear creds, so each entry is just the canonical URL plus parsed
/// display fields. Authorizes a live id too (so the card works while a session
/// streams). Runs the one-time backfill — guarded by `linear_scanned_at` — on
/// first read, exactly like the PR list. 404 when the session is not visible.
///
/// Response: `{ "linear_docs": [ { url, workspace, kind, identifier } ] }`.
async fn list_session_linear_docs(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    authorize_session_or_live_view(&state, viewer.as_deref(), &id).await?;

    // One-time backfill: scan ALL of the session's stored messages for Linear
    // refs when it has never been Linear-scanned, then stamp so it never runs
    // again. Best-effort — a failure must not break the listing.
    if state
        .repo
        .linear_scanned_at(&id)
        .await
        .unwrap_or(None)
        .is_none()
    {
        backfill_session_linear_docs(&state, &id).await;
        if let Err(e) = state
            .repo
            .set_linear_scanned_at(&id, chrono::Utc::now())
            .await
        {
            tracing::debug!(session_id = %id, error = %e, "set_linear_scanned_at failed (non-fatal)");
        }
    }

    // Older scanners saved JSON escape suffixes in both identifiers and URLs.
    // Normalize on read so existing sessions are fixed without a DB migration.
    let mut seen = std::collections::HashSet::new();
    let docs: Vec<_> = state
        .repo
        .list_session_linear_docs(&id)
        .await?
        .into_iter()
        .map(|doc| crate::linear::parse_linear_url(&doc.url).unwrap_or(doc))
        .filter(|doc| seen.insert(doc.url.clone()))
        .collect();
    Ok(Json(json!({ "linear_docs": docs })))
}

/// One-time Linear-detection backfill over a session's stored messages (the
/// Linear analogue of [`backfill_session_prs`]). Gathers the session's
/// uploaded/graduated messages plus any retained live messages and auto-links
/// the detected union. No repo context is needed — Linear URLs are
/// self-contained. Best-effort; logs and returns on error.
async fn backfill_session_linear_docs(state: &AppState, session_id: &str) {
    let mut texts: Vec<String> = Vec::new();
    if let Ok(Some(session)) = state.repo.get_session(None, session_id).await {
        texts.extend(session.messages.iter().map(|m| m.text.clone()));
    }
    if let Ok(live_msgs) = state.repo.get_live_messages(session_id).await {
        texts.extend(live_msgs.into_iter().map(|m| m.message.text));
    }
    if texts.is_empty() {
        return;
    }
    detect_and_link_linear_docs(state, session_id, texts.iter().map(String::as_str)).await;
}

/// `DELETE /api/sessions/{id}/linear-docs?url=<canonical>` — unlink a Linear ref
/// from a session. 404 when the session is absent/invisible or no matching ref
/// is linked; 204 on success.
async fn unlink_session_linear_doc(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Query(params): Query<LinearDocUnlinkParams>,
) -> Result<StatusCode> {
    authorize_session_or_live_view(&state, viewer.as_deref(), &id).await?;

    // Listing presents one canonical link for potentially several legacy rows.
    // Remove every alias, otherwise the same link reappears on the next read.
    let canonical = crate::linear::parse_linear_url(&params.url)
        .map(|doc| doc.url)
        .unwrap_or_else(|| params.url.clone());
    let mut removed = false;
    for doc in state.repo.list_session_linear_docs(&id).await? {
        let url = crate::linear::parse_linear_url(&doc.url)
            .map(|normalized| normalized.url)
            .unwrap_or_else(|| doc.url.clone());
        if url == canonical {
            removed |= state.repo.unlink_session_linear_doc(&id, &doc.url).await?;
        }
    }

    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

/// `POST /api/sessions/{id}/prs/status` — the per-user CLI pushes the cached
/// GitHub status for an already-linked PR. The server holds no GitHub creds;
/// this is the only way status gets populated.
///
/// Body: `{ owner, repo, number, status: { state, merged, draft, title } }`.
/// `fetched_at` is stamped server-side (now). Authz mirrors the link/unlink
/// routes (session must be visible to the viewer; 404 otherwise). The PR must
/// already be linked — 404 when no matching PR row exists. Returns 204.
async fn push_session_pr_status(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<PushPrStatusRequest>,
) -> Result<StatusCode> {
    authorize_session_view(&state, viewer.as_deref(), &id).await?;

    let updated = state
        .repo
        .set_session_pr_status(
            pushed_source(req.publication),
            &id,
            &req.owner,
            &req.repo,
            req.number,
            &req.status,
            chrono::Utc::now(),
        )
        .await?;

    if updated {
        Ok(StatusCode::NO_CONTENT)
    } else {
        // The PR is not linked to this session — 404 so the CLI links first.
        Err(AppError::NotFound)
    }
}

/// Ingested URLs are rendered as links (`href`) in the SPA. Only absolute
/// http(s) URLs are accepted, so a pushed cache cannot plant a `javascript:`
/// or `data:` scheme that the browser would execute on click.
fn is_web_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// `POST /api/sessions/{id}/prs/comments` — the per-user CLI pushes the PR
/// comments it fetched via `gh`. The server holds no GitHub creds; this is the
/// only way comments get cached.
///
/// Body: `{ owner, repo, number, comments: [ { comment_id, kind, author, body,
/// html_url, created_at } ] }`. `ingested_at` is stamped server-side. Authz
/// mirrors the status push (`authorize_session_view`): the session must be
/// visible to the viewer; 404 otherwise. Every `html_url` must be an absolute
/// http(s) URL (the SPA renders it as a link) — 400 otherwise. Returns 204.
async fn push_session_pr_comments(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<PushPrCommentsRequest>,
) -> Result<StatusCode> {
    authorize_session_view(&state, viewer.as_deref(), &id).await?;

    for comment in &req.comments {
        if !is_web_url(&comment.html_url) {
            return Err(AppError::BadRequest(format!(
                "comment html_url must be an absolute http(s) URL, got {:?}",
                comment.html_url
            )));
        }
    }

    let changed = state
        .repo
        .upsert_pr_comments(
            pushed_source(req.publication),
            &id,
            &req.owner,
            &req.repo,
            req.number,
            &req.comments,
        )
        .await?;

    if changed {
        state.live.publish(&id, LiveEvent::PrCommentsChanged);
    }

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/sessions/{id}/prs/comments` — list the CACHED PR comments for a
/// session (pushed by the per-user CLI; the server never fetches). Same view
/// authz as the push route; 404 when the session is absent/invisible.
///
/// Response: `{ "comments": [ { owner, repo, number, comment_id, kind, author,
/// body, html_url, created_at, ingested_at } ] }`, ordered by `created_at`
/// ascending.
async fn list_session_pr_comments(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Query(params): Query<PrCommentListParams>,
) -> Result<impl IntoResponse> {
    authorize_session_view(&state, viewer.as_deref(), &id).await?;
    let comments = match params.since {
        Some(since) => state.repo.list_pr_comments_since(&id, since).await?,
        None => state.repo.list_pr_comments(&id).await?,
    };
    Ok(Json(json!({ "comments": comments })))
}

#[derive(Default, Deserialize)]
struct PrCommentListParams {
    since: Option<i64>,
}

async fn list_session_pr_comment_previews(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    authorize_session_view(&state, viewer.as_deref(), &id).await?;
    let comments = state.repo.list_pr_comment_previews(&id).await?;
    Ok(Json(json!({ "comments": comments })))
}

// ---------------------------------------------------------------------------
// PR sync from the UI (per-user sync channel — gh stays on the CLI)
// ---------------------------------------------------------------------------

/// Character budget for the transcript passed to the summary model. A rough fit
/// for the default model's context window; the transcript is the only large
/// input (the system prompt is tiny), so it gets effectively the whole budget.
const SUMMARY_BUDGET_CHARS: usize = 48_000;

/// System prompt for the session-summary model. Demands strict JSON only.
const SUMMARY_SYSTEM_PROMPT: &str = "You summarize a Claude Code session transcript. \
Respond with ONLY a JSON object, no prose, no code fences, matching exactly this shape: \
{\"intent\": string, \"invariants\": string[], \"summary\": string}. \
- intent: the user's goal for this session, in one sentence. \
- invariants: constraints or decisions that must hold (an array of short strings; may be empty). \
- summary: a concise human-readable recap of what happened. \
Respond with ONLY the JSON object, no prose, no code fences.";

/// `POST /api/sessions/{id}/summary` — generate (and persist) an LLM summary of
/// a session. 503 when LLM features are not configured, 404 when the session is
/// absent or not visible to the viewer, 502 when the model returns unusable
/// output.
async fn generate_summary(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let llm = state
        .llm
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("LLM features are not configured".into()))?;

    // The snapshot is read before the transcript and pins the final write.
    let source = state.repo.source_context(&id).await?;
    // Authorizes can-view (404 otherwise) and gives us the transcript.
    let stored = state
        .repo
        .get_session(viewer.as_deref(), &id)
        .await?
        .ok_or(AppError::NotFound)?;

    // Build a full core::Session from the stored fields for transcript rendering.
    let session = stored_to_core_session(stored);

    let transcript = crate::llm::render_transcript(&session, SUMMARY_BUDGET_CHARS);
    let turns = [
        ChatTurn::system(SUMMARY_SYSTEM_PROMPT),
        ChatTurn::user(transcript),
    ];
    let reply = llm.complete(&turns).await?;
    let (intent, invariants, summary_text) = parse_summary_json(&reply)?;

    let summary = SessionSummary {
        intent,
        invariants,
        summary: summary_text,
        model: llm.model().to_string(),
        generated_by: viewer.clone(),
        generated_at: chrono::Utc::now(),
    };
    state.repo.upsert_summary(source, &id, &summary).await?;
    Ok(Json(summary))
}

/// Parse the model's reply into `(intent, invariants, summary)`. Tolerates a
/// leading/trailing prose or ```json code fences by slicing from the first `{`
/// to the last `}` before deserializing. A non-JSON / malformed body maps to a
/// 502 (`BadGateway`).
fn parse_summary_json(raw: &str) -> Result<(String, Vec<String>, String)> {
    #[derive(Deserialize)]
    struct SummaryJson {
        intent: String,
        #[serde(default)]
        invariants: Vec<String>,
        summary: String,
    }

    let start = raw.find('{');
    let end = raw.rfind('}');
    let slice = match (start, end) {
        (Some(s), Some(e)) if e >= s => &raw[s..=e],
        _ => {
            return Err(AppError::BadGateway(
                "model did not return valid summary JSON: no JSON object found".into(),
            ))
        }
    };

    let parsed: SummaryJson = serde_json::from_str(slice).map_err(|e| {
        AppError::BadGateway(format!("model did not return valid summary JSON: {e}"))
    })?;
    Ok((parsed.intent, parsed.invariants, parsed.summary))
}

/// Build a transcript-renderable [`core::Session`] from a [`StoredSession`].
fn stored_to_core_session(stored: StoredSession) -> core::Session {
    core::Session {
        id: stored.id,
        agent: stored.agent,
        title: stored.title,
        project_path: stored.project_path,
        cwd: stored.cwd,
        git_branch: stored.git_branch,
        claude_version: stored.claude_version,
        started_at: stored.started_at,
        ended_at: stored.ended_at,
        messages: stored.messages,
        skipped_lines: stored.skipped_lines,
    }
}

/// Character budget for the transcript(s) passed to the chat model. Smaller than
/// the summary's budget to leave room for the thread history in the same context.
const CHAT_TRANSCRIPT_BUDGET: usize = 32_000;

/// System prompt for a session chat.
const CHAT_SESSION_SYSTEM_PROMPT: &str =
    "You answer questions about this recorded Claude Code session. \
You cannot run tools or read the live filesystem.";

/// System prompt for a project chat (spans multiple recorded sessions).
const CHAT_PROJECT_SYSTEM_PROMPT: &str =
    "You answer questions about these recorded Claude Code sessions. \
You cannot run tools or read the live filesystem.";

/// `POST /api/chats/{id}/messages` — append the user message, call the model
/// with the relevant transcript(s) + thread history, and stream the assistant
/// reply back over SSE (`text/event-stream`). Owner-only (auth-off permissive).
/// 404 when the chat is absent, 403 for a non-owner, 503 when LLM features are
/// not configured (existence + authz are checked before the LLM-config gate so
/// the global LLM state is not leaked to a non-owner).
///
/// Persistence invariant: the user message is persisted *before* the upstream
/// call. The assistant reply is accumulated and persisted on clean UPSTREAM
/// completion by a spawned task that owns the upstream stream — so a browser
/// disconnect does not abort persistence, and an upstream error persists
/// nothing (no partial row).
async fn post_chat_message(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<ChatMessageRequest>,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, std::convert::Infallible>>>> {
    // Load the chat and enforce owner-only write BEFORE the LLM-config check, so
    // a non-owner (or a request for an absent chat) gets a 404/403 rather than
    // having the global LLM state leaked to them.
    let chat = state.repo.get_chat(&id).await?.ok_or(AppError::NotFound)?;
    authorize_owner(viewer.as_deref(), Some(chat.owner_email.clone()), "chat")?;

    // The LLM must be configured to answer.
    let llm = state
        .llm
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailable("LLM features are not configured".into()))?
        .clone();

    // The snapshot is read before the transcript and pins both writes below.
    let source = match &chat.session_id {
        Some(session_id) => state.repo.source_context(session_id).await?,
        None => SourceContext::Unmanaged,
    };
    // Build the conversational turns BEFORE persisting/streaming.
    let (system_prompt, transcript) = build_chat_context(&state, viewer.as_deref(), &chat).await?;

    // Prior thread history (oldest-first), before we append this user message.
    let history = state
        .repo
        .get_chat_with_messages(&id)
        .await?
        .ok_or(AppError::NotFound)?
        .messages;

    // Persist the user message FIRST (so it survives even if the model fails).
    state
        .repo
        .add_chat_message(&id, source, "user", &req.content)
        .await?;

    // Assemble the turn list: system prompt, transcript context, prior history,
    // then the just-added user message as the final turn.
    let mut turns: Vec<ChatTurn> = Vec::with_capacity(history.len() + 3);
    turns.push(ChatTurn::system(system_prompt));
    turns.push(ChatTurn::user(transcript));
    for m in history {
        match m.role.as_str() {
            "assistant" => turns.push(ChatTurn::assistant(m.content)),
            _ => turns.push(ChatTurn::user(m.content)),
        }
    }
    turns.push(ChatTurn::user(req.content));

    // Resolve the per-request model override (empty string → use default).
    let model_override = req.model.as_deref().filter(|s| !s.is_empty());

    // Open the upstream stream up-front so a connection/HTTP error maps to a
    // clean error response rather than a half-built SSE stream.
    let upstream = llm.complete_stream_with(model_override, &turns).await?;

    // Channel from the owning task to the SSE response. The task owns the
    // upstream stream and the accumulator; it forwards tokens to the client and,
    // on clean upstream completion, persists the full reply REGARDLESS of whether
    // the client is still connected (bounded channel + `send` errors are ignored
    // once the client drops).
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(32);
    let repo = state.repo.clone();
    let chat_id = id.clone();

    tokio::spawn(async move {
        futures::pin_mut!(upstream);
        let mut accumulated = String::new();
        let mut failed: Option<String> = None;

        while let Some(item) = upstream.next().await {
            match item {
                Ok(token) => {
                    accumulated.push_str(&token);
                    let data = json!({ "type": "token", "text": token }).to_string();
                    // Ignore send errors: a dropped client must NOT abort
                    // accumulation/persistence.
                    let _ = tx.send(Event::default().data(data)).await;
                }
                Err(e) => {
                    failed = Some(e.to_string());
                    break;
                }
            }
        }

        if let Some(reason) = failed {
            // Upstream error: persist nothing (no partial row), emit an error frame.
            let data = json!({ "type": "error", "message": reason }).to_string();
            let _ = tx.send(Event::default().data(data)).await;
            return;
        }

        // Clean upstream completion: persist the full reply, then emit the
        // terminal `done` frame carrying the persisted row.
        match repo
            .add_chat_message(&chat_id, source, "assistant", &accumulated)
            .await
        {
            Ok(message) => {
                let data = json!({ "type": "done", "message": message }).to_string();
                let _ = tx.send(Event::default().data(data)).await;
            }
            Err(e) => {
                let data =
                    json!({ "type": "error", "message": format!("failed to persist reply: {e}") })
                        .to_string();
                let _ = tx.send(Event::default().data(data)).await;
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok);
    Ok(Sse::new(stream).keep_alive(sse_keep_alive()))
}

/// Resolve the `(system_prompt, transcript)` for a chat thread. For a session
/// chat the transcript is that one session; for a project chat it is the
/// combined, per-session-budgeted transcripts of every member session under
/// `## Session: <title>` headers. The viewer's can-view gate on the session is
/// honored (404 when not visible / absent).
async fn build_chat_context(
    state: &AppState,
    viewer: Option<&str>,
    chat: &crate::store::Chat,
) -> Result<(&'static str, String)> {
    match (&chat.session_id, &chat.project_id) {
        (Some(session_id), _) => {
            let stored = state
                .repo
                .get_session(viewer, session_id)
                .await?
                .ok_or(AppError::NotFound)?;
            let session = stored_to_core_session(stored);
            let transcript = crate::llm::render_transcript(&session, CHAT_TRANSCRIPT_BUDGET);
            Ok((CHAT_SESSION_SYSTEM_PROMPT, transcript))
        }
        (None, Some(project_id)) => {
            let detail = state
                .repo
                .get_project(viewer, project_id)
                .await?
                .ok_or(AppError::NotFound)?;
            let n = detail.sessions.len().max(1);
            let sub_budget = CHAT_TRANSCRIPT_BUDGET / n;
            let mut combined = String::new();
            for summary in &detail.sessions {
                // Projects are viewable by any viewer in v1; render each member.
                if let Some(stored) = state.repo.get_session(viewer, &summary.id).await? {
                    let session = stored_to_core_session(stored);
                    let rendered = crate::llm::render_transcript(&session, sub_budget);
                    combined.push_str(&format!("## Session: {}\n", summary.title));
                    combined.push_str(&rendered);
                    combined.push_str("\n\n");
                }
            }
            Ok((CHAT_PROJECT_SYSTEM_PROMPT, combined))
        }
        // The CHECK constraint guarantees exactly one parent; a malformed row.
        (None, None) => Err(AppError::Internal(anyhow::anyhow!(
            "chat {} has neither a session nor a project parent",
            chat.id
        ))),
    }
}

// ---------------------------------------------------------------------------
// Live sessions (Increment 1, view-only)
// ---------------------------------------------------------------------------

/// A live session enriched with per-viewer authorization flags, as returned by
/// `GET /api/live`. Flattens every [`LiveSession`] field and adds the two
/// computed booleans the SPA needs to decide what UI to render. They are
/// computed per-request from the authenticated viewer and never stored.
#[derive(Debug, Serialize)]
pub struct LiveSessionView {
    #[serde(flatten)]
    pub session: LiveSession,
    /// Explicit user title, even when it equals the current automatic title.
    pub title_override: Option<String>,
    /// The viewer is the session's author (full read+write + manage shares).
    pub viewer_is_author: bool,
    /// The viewer may interact: author OR a read+write sharee. Write is the
    /// single interactive grant — it covers both sanitized replies (`/replies`)
    /// and raw keyboard/clipboard input (`/input`, `/clipboard`) plus live
    /// clipboard input.
    pub viewer_can_write: bool,
    /// A live CLI is currently attached for the session's interactive surface.
    /// For interactive sessions this requires both reply-stream and raw-input
    /// subscriptions; otherwise the UI could claim keyboard capture while raw
    /// keystrokes are dropped.
    pub cli_attached: bool,
    /// Wall-clock time of the last visible terminal text change. `None` when
    /// no screen text has changed since the server started. Repeated frames
    /// and cursor-only updates do not count as activity.
    pub last_pty_at: Option<chrono::DateTime<chrono::Utc>>,
    /// PRs auto-linked (or manually linked) to this live session, so the SPA can
    /// match a pasted PR URL / `owner/repo#N` against live sessions in search.
    /// Same shape (incl. cached open/merged/closed `status`) as the per-session
    /// `GET /api/sessions/{id}/prs` list.
    pub linked_prs: Vec<PrWithStatus>,
}

/// Request body for `PUT /api/live/{id}/sharing` — replace the share list.
#[derive(Debug, Deserialize)]
pub struct LiveSharingRequest {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub shares: Vec<LiveShareRequest>,
}

/// Request body for `POST /api/live` — register a live session.
#[derive(Debug, Deserialize)]
pub struct RegisterLiveRequest {
    pub session_id: String,
    #[serde(default)]
    pub agent: core::AgentKind,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub source_host: Option<String>,
    /// The launch repo's `origin` remote URL, discovered by the CLI. Parsed
    /// server-side to resolve owner/repo for PR-shorthand auto-linking while the
    /// session is live. Absent for older CLIs / non-git launches.
    #[serde(default)]
    pub remote_url: Option<String>,
    /// Interaction mode: `"view"` (default) or `"interactive"`. Threaded into
    /// the `live_sessions.mode` column. Absent ⇒ `"view"`.
    #[serde(default)]
    pub mode: Option<String>,
}

/// Request body for `POST /api/live/{id}/events` — a batch of parsed messages.
#[derive(Debug, Deserialize)]
pub struct LiveEventsRequest {
    #[serde(default)]
    pub messages: Vec<core::Message>,
}

/// Request body for `POST /api/live/{id}/title`.
#[derive(Debug, Deserialize)]
pub struct LiveTitleRequest {
    pub title: String,
}

/// `POST /api/live/{id}/title` — update a live session's display title. The CLI
/// pushes a transcript-derived title here so the live view shows it instead of
/// the raw `live: <uuid>` placeholder. **Author-only**: 404 when absent, 403 for
/// a non-author. A blank title is ignored (keeps the existing one). Returns
/// `{ "updated": bool }`.
async fn post_live_title(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<LiveTitleRequest>,
) -> Result<impl IntoResponse> {
    require_live_author(&state, &id, viewer.as_deref()).await?;

    let title = req.title.trim();
    if title.is_empty() {
        return Ok(Json(json!({ "updated": false })));
    }
    let updated = state.repo.update_live_session_title(&id, title).await?;

    // A live title often carries the PR being reviewed (e.g. "Review pull
    // request #409"). Scan the new title for PR refs and auto-link them
    // (best-effort) — the title arrives on its own endpoint, so the events-path
    // scan may not see it. Uses the live repo context so the `PR #N` shorthand
    // resolves to a full owner/repo URL.
    if updated {
        let repo_ctx = live_repo_ctx(&state, &id).await;
        let repo_ctx_ref = repo_ctx.as_ref().map(|(o, r)| (o.as_str(), r.as_str()));
        detect_and_link_prs(&state, &id, repo_ctx_ref, std::iter::once(title)).await;
        // A title can also carry a Linear URL (e.g. the doc being implemented).
        detect_and_link_linear_docs(&state, &id, std::iter::once(title)).await;
    }

    Ok(Json(json!({ "updated": updated })))
}

/// Request body for `POST /api/live/{id}/agent-session-id`.
#[derive(Debug, Deserialize)]
pub struct AgentSessionIdRequest {
    pub agent_session_id: String,
}

/// Query parameters for the live message stream.
#[derive(Debug, Deserialize, Default)]
pub struct LiveStreamParams {
    /// Resume cursor: replay only messages with `seq > since`. A browser that
    /// already cached messages up to `since` (see the frontend
    /// `liveMessageCache`) passes it on reconnect / session-switch so the
    /// initial replay carries only the tail instead of the whole backlog.
    /// Absent ⇒ full replay from `seq 0` (a first visit / cold cache).
    pub since: Option<i64>,
    /// `false` replaces every `message` frame with a body-less
    /// `message_outline` frame. A collapsed transcript only needs the count
    /// and the latest visible role, so it does not download message bodies.
    /// Absent ⇒ `true` (full messages).
    #[serde(default = "default_true")]
    pub bodies: bool,
}

fn default_true() -> bool {
    true
}

/// One live-stream `message` frame, or its body-less outline.
fn live_message_frame(seq: i64, message: &conversations_core::Message, bodies: bool) -> String {
    if bodies {
        json!({ "type": "message", "seq": seq, "message": message })
    } else {
        json!({
            "type": "message_outline",
            "seq": seq,
            "role": message.role,
            "visible": transcript_visible(message),
        })
    }
    .to_string()
}

/// Whether the browser transcript renders a bubble for `message`: messages made
/// only of tool results or only of signature-only thinking are folded away.
/// Mirrors the frontend `filterRenderedMessages`.
fn transcript_visible(message: &conversations_core::Message) -> bool {
    use conversations_core::ContentBlock;
    let blocks = &message.blocks;
    let all =
        |predicate: fn(&ContentBlock) -> bool| !blocks.is_empty() && blocks.iter().all(predicate);
    !all(|b| matches!(b, ContentBlock::ToolResult { .. }))
        && !all(|b| matches!(b, ContentBlock::Thinking { thinking } if thinking.trim().is_empty()))
}

/// `GET /api/live/{id}/stream` — SSE: first replay stored messages as
/// `message` frames, then subscribe to the hub and forward live `message` /
/// `complete` events. 404 when the live session is absent.
///
/// `?since=<seq>` bounds the initial replay to `seq > since` (incremental
/// resume); omitting it replays the full backlog. The live-hub tail is
/// unaffected either way — it always carries every new event with its `seq`.
///
/// `?bodies=false` swaps each `message` frame for its outline (the other
/// frames are unchanged), under the same view authorization.
///
/// The SSE `data` payloads (EXACT, the contract Slice B's UI matches):
///   - `{"type":"message","seq":N,"message":<core::Message JSON>}`
///   - `{"type":"message_outline","seq":N,"role":"user"|"assistant","visible":bool}`
///   - `{"type":"complete","session_id":"<graduated id>"}`
async fn stream_live(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Query(params): Query<LiveStreamParams>,
    headers: axum::http::HeaderMap,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, std::convert::Infallible>>>> {
    // CSWSH gate (see `authorize_ws_origin`): EventSource attaches the
    // session cookie cross-site exactly like a WebSocket handshake.
    authorize_ws_origin(&headers, state.config.auth_enabled)?;
    require_live_view(&state, &id, viewer.as_deref()).await?;

    // Subscribe BEFORE reading the replay backlog so no event slips through the
    // gap between "replay finished" and "subscribed". A message that arrives
    // during replay may therefore appear both in the backlog and on the channel;
    // the UI de-dups on `seq`.
    let mut receiver = state.live.subscribe(&id);
    let replay = match params.since {
        Some(after) => state.repo.get_live_messages_since(&id, after).await?,
        None => state.repo.get_live_messages(&id).await?,
    };

    let bodies = params.bodies;
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);

    tokio::spawn(async move {
        // 1. Replay the durable backlog.
        for stored in replay {
            let data = live_message_frame(stored.seq, &stored.message, bodies);
            if tx.send(Event::default().data(data)).await.is_err() {
                return; // client gone
            }
        }

        // 2. Forward live events until the hub closes or the client drops.
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let data = match &event {
                        LiveEvent::Complete { session_id } => {
                            json!({ "type": "complete", "session_id": session_id }).to_string()
                        }
                        LiveEvent::PrsLinked { session_id } => {
                            json!({ "type": "prs_linked", "session_id": session_id }).to_string()
                        }
                        LiveEvent::PrCommentsChanged => {
                            json!({ "type": "pr_comments_changed" }).to_string()
                        }
                        LiveEvent::LinearDocsLinked { session_id } => {
                            json!({ "type": "linear_docs_linked", "session_id": session_id })
                                .to_string()
                        }
                    };
                    if tx.send(Event::default().data(data)).await.is_err() {
                        return;
                    }
                    // A completed session emits its terminal frame, then ends.
                    if matches!(event, LiveEvent::Complete { .. }) {
                        return;
                    }
                }
                // Lagged: durable table is the source of truth; keep going.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                // Sender dropped (hub removed on completion) — end the stream.
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok);
    Ok(Sse::new(stream).keep_alive(sse_keep_alive()))
}

/// Request body for `POST /api/live/{id}/replies`.
#[derive(Debug, Deserialize)]
pub struct ReplyRequest {
    pub text: String,
}

/// Cross-site WebSocket hijacking (CSWSH) defense, shared by the WS upgrades
/// and the browser-facing SSE streams (`EventSource` attaches the session
/// cookie cross-site exactly like a WS handshake). Browsers attach `Origin`
/// (and the session cookie) to every WS handshake; the SPA is served by this
/// very server, so a legitimate browser origin always matches the request
/// host. A mismatched `Origin` therefore means a hostile page on another site
/// is dialing an authenticated stream — refuse it. Non-browser
/// clients (the CLI, tests) send no `Origin` header at all and are unaffected,
/// and the auth-off local/dev posture skips the check entirely (there are no
/// credentials to hijack).
///
/// The comparison is on the authority (host[:port]) only, ignoring the scheme:
/// behind a TLS-terminating ingress the browser speaks `https`/`wss` while
/// the server sees plain `http`, and both must match the `Host` it forward.
pub(crate) fn authorize_ws_origin(
    headers: &axum::http::HeaderMap,
    auth_enabled: bool,
) -> Result<()> {
    if !auth_enabled {
        return Ok(());
    }
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(()); // non-browser client (no Origin header)
    };
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(()); // no Host to compare against — don't block
    };
    let origin_authority = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    if origin_authority.eq_ignore_ascii_case(host) {
        return Ok(());
    }
    Err(AppError::Forbidden(format!(
        "cross-origin websocket upgrades are not allowed (origin {origin:?} vs host {host:?})"
    )))
}

/// Only project currently visible IDs; saved positions survive temporary disconnection.
async fn visible_live_order(
    state: &AppState,
    viewer: Option<&str>,
) -> Result<crate::store::LiveOrder> {
    let mut order = state.repo.live_order(viewer.unwrap_or("")).await?;
    let mut sessions = state
        .repo
        .list_live_sessions(Some(crate::store::LIVE_STATUS_LIVE), viewer)
        .await?;
    sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(a.id.cmp(&b.id)));
    let visible: std::collections::HashSet<_> = sessions.iter().map(|s| s.id.as_str()).collect();
    order.session_ids.retain(|id| visible.contains(id.as_str()));
    let mut new_ids: Vec<_> = sessions
        .into_iter()
        .filter(|session| !order.session_ids.contains(&session.id))
        .map(|session| session.id)
        .collect();
    new_ids.extend(order.session_ids);
    order.session_ids = new_ids;
    Ok(order)
}

async fn get_live_order(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
) -> Result<impl IntoResponse> {
    Ok(Json(visible_live_order(&state, viewer.as_deref()).await?))
}

async fn put_live_order(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Json(request): Json<crate::store::LiveOrder>,
) -> Result<impl IntoResponse> {
    if request.revision < 0 || request.revision == i64::MAX || request.session_ids.len() > 1000 {
        return Err(AppError::BadRequest("invalid live order".into()));
    }
    let current = visible_live_order(&state, viewer.as_deref()).await?;
    if request.revision != current.revision {
        return Err(AppError::Conflict(
            "live order changed; refresh before reordering".into(),
        ));
    }
    let selected: std::collections::HashSet<_> = request.session_ids.iter().collect();
    if selected.len() != request.session_ids.len()
        || request
            .session_ids
            .iter()
            .any(|id| !current.session_ids.contains(id))
    {
        return Err(AppError::BadRequest(
            "order must contain unique visible live session IDs".into(),
        ));
    }
    let mut saved = state
        .repo
        .live_order(viewer.as_deref().unwrap_or(""))
        .await?;
    // Preserve missing sessions' positions for reconnect; prepend genuinely new IDs.
    let mut new_ids: Vec<_> = current
        .session_ids
        .into_iter()
        .filter(|id| !saved.session_ids.contains(id))
        .collect();
    new_ids.extend(saved.session_ids);
    saved.session_ids = new_ids;
    let mut replacement = request.session_ids.iter();
    for id in &mut saved.session_ids {
        if selected.contains(id) {
            *id = replacement.next().unwrap().clone();
        }
    }
    if !state
        .repo
        .set_live_order(
            viewer.as_deref().unwrap_or(""),
            request.revision,
            &saved.session_ids,
        )
        .await?
    {
        return Err(AppError::Conflict(
            "live order changed; refresh before reordering".into(),
        ));
    }
    Ok(Json(visible_live_order(&state, viewer.as_deref()).await?))
}

/// `GET /api/live` — list active (`status = 'live'`) live sessions visible to
/// the viewer (author or sharee), newest first. Each item carries the computed
/// `viewer_is_author` + `viewer_can_write` flags.
async fn list_live(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
) -> Result<impl IntoResponse> {
    let live_sessions = state
        .repo
        .list_live_sessions(Some(crate::store::LIVE_STATUS_LIVE), viewer.as_deref())
        .await?;

    let mut views: Vec<LiveSessionView> = Vec::with_capacity(live_sessions.len());
    for mut session in live_sessions {
        // A web rename writes `sessions.title_override`, not `live_sessions.title`
        // (the CLI-pushed title). Overlay the override so a renamed live session
        // shows its new name in the live header and the unified list's LIVE card.
        let title_override = state.repo.get_title_override(&session.id).await?;
        if let Some(title) = &title_override {
            session.title = Some(title.clone());
        }
        let (viewer_is_author, viewer_can_write) = match viewer.as_deref() {
            // Auth off: no identity to scope to — full access, mirroring the
            // authorize_live_* helpers' `None` viewer branch.
            None => (true, true),
            Some(v) => {
                let is_author = session.owner_email.as_deref() == Some(v);
                if is_author {
                    (true, true)
                } else {
                    // Not the author: look up this viewer's share level. The list
                    // query already guarantees the viewer is author-or-sharee, so
                    // a missing share row here means they only got in via author.
                    let can_write = state
                        .repo
                        .get_live_shares(&session.id)
                        .await?
                        .into_iter()
                        .find(|s| s.email == v)
                        .map(|s| s.can_write)
                        .unwrap_or(false);
                    (false, can_write)
                }
            }
        };
        // Central attachment no longer exists: a host's terminals live behind
        // its own signed, end-to-end channels, which the server cannot observe.
        // `cli_attached` is therefore always false here — honest absence, not a
        // claim that the session has no host. Terminal presence is a
        // private-channel fact (see the `/private` page).
        let cli_attached = false;
        // No terminal bytes are relayed centrally, so there is no central
        // "last terminal output at" to report.
        let last_pty_at = None;
        // Best-effort: a PR-list failure must not sink the whole live listing.
        let linked_prs: Vec<PrWithStatus> = state
            .repo
            .list_session_prs(&session.id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(PrWithStatus::from)
            .collect();
        views.push(LiveSessionView {
            session,
            title_override,
            viewer_is_author,
            viewer_can_write,
            cli_attached,
            last_pty_at,
            linked_prs,
        });
    }

    Ok(Json(json!({ "live_sessions": views })))
}

/// `POST /api/live/{id}/dismiss` — soft-close a live session you can see but do
/// not own. Unlike author-only [`complete_live`], any viewer who can see the
/// session (author OR sharee — [`authorize_live_view`]) may dismiss it. It
/// graduates the session the same way `complete` does (so it leaves the live
/// list and is findable in the ordinary sessions view), broadcasts the terminal
/// `complete` event, and drops the hub. This only changes central session state;
/// stopping a local process requires a separately authorized host operation.
///
/// Person access remains independently revocable after graduation.
/// Returns `{ session_id }`.
async fn dismiss_live(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    // Author-or-sharee may dismiss (404 for a stranger). NOT author-only —
    // the whole point is letting a viewer clear a not-mine session.
    require_live_view(&state, &id, viewer.as_deref()).await?;

    let session_id = state
        .repo
        .complete_live_session(&id)
        .await?
        .ok_or(AppError::NotFound)?;

    state.live.publish(
        &id,
        LiveEvent::Complete {
            session_id: session_id.clone(),
        },
    );
    state.live.drop_hub(&id);

    // A dismissal only changes central session state.
    // must not terminate the owner's running `claude` process — that is reserved
    // for the author's own `complete`.

    Ok(Json(json!({ "session_id": session_id })))
}

/// `PUT /api/live/{id}/sharing` — replace the share list. **Author-only**: 404
/// when the session is absent, 403 for a non-author. Body
/// `{ "shares": [{ "email": "...", "can_read": bool, "can_write": bool, "can_communicate": bool }] }`.
/// Transcript access and communication are independent; see [`LiveShareRequest`]
/// for the defaults older clients rely on and the legacy `communication_only`
/// field. Write is the single interactive grant (replies + keyboard) and
/// requires read; a legacy `can_control` field in the payload is ignored.
/// `400` for write without read or a share that grants nothing.
/// Returns the stored `{ "shares": [...] }` (deduped, ordered by email).
async fn set_live_sharing(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<LiveSharingRequest>,
) -> Result<impl IntoResponse> {
    require_live_author(&state, &id, viewer.as_deref()).await?;
    let shares = req
        .shares
        .into_iter()
        .map(LiveShareRequest::into_share)
        .collect::<Result<Vec<_>>>()?;

    state
        .repo
        .update_sharing_context(
            &id,
            &crate::store::notifications::SharingUpdate {
                sender: viewer,
                reason: req.reason,
                visibility: None,
                emails: vec![],
                shares,
            },
        )
        .await?;
    let shares = state.repo.get_live_shares(&id).await?;
    Ok(Json(json!({ "shares": shares })))
}

/// `GET /api/live/{id}/sharing` — the current share list. **Author-only**: 404
/// when the session is absent, 403 for a non-author. Returns
/// `{ "shares": [{ "email", "can_read", "can_write", "can_communicate", "communication_only" }] }`,
/// where `communication_only` is `!can_read` for older clients.
async fn get_live_sharing(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let (owner, shares) = state
        .repo
        .get_live_session_access(&id)
        .await?
        .ok_or(AppError::NotFound)?;
    authorize_live_author(viewer.as_deref(), owner)?;
    Ok(Json(json!({ "shares": shares })))
}

#[derive(Deserialize)]
struct AttentionRequest {
    recipient_email: String,
    message: String,
}

async fn list_notifications(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
) -> Result<impl IntoResponse> {
    let Some(viewer) = viewer else {
        return Ok(Json(vec![]));
    };
    Ok(Json(state.repo.notifications(&viewer).await?))
}

async fn attention_recipients(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let viewer =
        viewer.ok_or_else(|| AppError::Unauthorized("Sign in to request attention".into()))?;
    Ok(Json(state.repo.attention_recipients(&id, &viewer).await?))
}

async fn request_attention(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<AttentionRequest>,
) -> Result<impl IntoResponse> {
    let viewer =
        viewer.ok_or_else(|| AppError::Unauthorized("Sign in to request attention".into()))?;
    state
        .repo
        .request_attention(
            &id,
            &viewer,
            &req.recipient_email.trim().to_lowercase(),
            &req.message,
        )
        .await?;
    Ok(Json(json!({"ok": true})))
}

async fn read_notification(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let viewer =
        viewer.ok_or_else(|| AppError::Unauthorized("Sign in to read notifications".into()))?;
    state
        .repo
        .acknowledge_notification(&id, &viewer, false)
        .await?;
    Ok(Json(json!({"ok": true})))
}

async fn resolve_notification(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    let viewer =
        viewer.ok_or_else(|| AppError::Unauthorized("Sign in to resolve requests".into()))?;
    state
        .repo
        .acknowledge_notification(&id, &viewer, true)
        .await?;
    Ok(Json(json!({"ok": true})))
}

/// Search the discoverable user directory.
async fn search_users(
    State(state): State<AppState>,
    Query(params): Query<UserSearchParams>,
) -> Result<impl IntoResponse> {
    let q = params.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let limit = params.limit.unwrap_or(DEFAULT_USER_LIMIT);
    let users = state.repo.search_users(q, limit).await?;
    Ok(Json(json!({ "users": users })))
}

/// The API routes, parameterized over [`AppState`]. State is applied by the
/// composition root in `lib.rs`.
///
/// `max_upload_bytes` caps request bodies that carry session history: full
/// uploads and live-event reconciliation batches. Both deserialize transcript
/// data in memory, so the shared cap prevents one large/abusive payload from
/// exhausting memory while overriding axum's conservative 2 MiB default.
pub fn routes(max_upload_bytes: usize) -> Router<AppState> {
    Router::new()
        .merge(crate::host_relay::routes())
        .merge(crate::user_keys::routes())
        // Agent messaging authority is local to each host machine (signed,
        // peer-to-peer): the server carries only read-only peer discovery and
        // communication-share lookups. The central send/inbox/handoff routes
        // were removed; nothing the server stores may be injected into an
        // agent's queue.
        .route(
            "/api/agent-messaging/sessions/{id}/peers",
            get(crate::agent_messaging::peers),
        )
        .route(
            "/api/agent-messaging/sessions/{id}/communications",
            get(crate::agent_messaging::communications),
        )
        .route("/api/app", get(app_info))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route(
            "/api/sessions",
            post(upload_session)
                .get(list_sessions)
                .layer(DefaultBodyLimit::max(max_upload_bytes)),
        )
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/sessions/{id}/sharing", put(set_sharing))
        .route(
            "/api/publications",
            post(crate::publication::publish)
                .layer(DefaultBodyLimit::max(crate::publication::MAX_REQUEST_BYTES)),
        )
        .route("/api/notifications", get(list_notifications))
        .route("/api/notifications/{id}/read", post(read_notification))
        .route(
            "/api/notifications/{id}/resolve",
            post(resolve_notification),
        )
        .route(
            "/api/sessions/{id}/attention-recipients",
            get(attention_recipients),
        )
        .route("/api/sessions/{id}/attention", post(request_attention))
        .route("/api/sessions/{id}/title", put(set_session_title))
        .route("/api/sessions/{id}/repos", put(set_repos))
        .route("/api/sessions/{id}/summary", post(generate_summary))
        .route("/api/sessions/{id}/project", put(set_session_project))
        .route(
            "/api/sessions/{id}/chats",
            get(list_session_chats).post(create_session_chat),
        )
        // Distinct legacy project *paths* (from session metadata), with counts.
        .route("/api/project-paths", get(list_projects))
        // Explicit, user-named projects.
        .route(
            "/api/projects",
            get(list_projects_detailed).post(create_project),
        )
        .route(
            "/api/projects/{id}",
            get(get_project).put(update_project).delete(delete_project),
        )
        .route(
            "/api/projects/{id}/chats",
            get(list_project_chats).post(create_project_chat),
        )
        .route(
            "/api/chats/{id}",
            get(get_chat).put(update_chat).delete(delete_chat),
        )
        .route("/api/chats/{id}/messages", post(post_chat_message))
        // Live sessions.
        .route("/api/live", get(list_live))
        .route("/api/live/order", get(get_live_order).put(put_live_order))
        // Live-session metadata curation only. The central live mirror
        // (registration, event/heartbeat push, terminal byte channels, replies,
        // clipboard, files) and every central control channel were removed:
        // nothing the server stores or relays may reach a session's process,
        // terminal or arguments on a host machine.
        .route("/api/live/{id}/title", post(post_live_title))
        .route("/api/live/{id}/stream", get(stream_live))
        .route("/api/live/{id}/dismiss", post(dismiss_live))
        .route(
            "/api/live/{id}/sharing",
            put(set_live_sharing).get(get_live_sharing),
        )
        .route("/api/users", get(search_users))
        // Per-user settings (container/monorepo-top-level paths).
        .route(
            "/api/user/settings",
            get(get_user_settings).put(set_user_settings),
        )
        .route("/api/llm/status", get(llm_status))
        .route("/api/llm/models", get(llm_models))
        // Session PR links: GET+POST share the path; DELETE uses query params.
        .route(
            "/api/sessions/{id}/prs",
            get(list_session_prs)
                .post(link_session_pr)
                .delete(unlink_session_pr),
        )
        // Session Linear refs (auto-detected): list + unlink (DELETE by ?url=).
        .route(
            "/api/sessions/{id}/linear-docs",
            get(list_session_linear_docs).delete(unlink_session_linear_doc),
        )
        // The per-user CLI pushes cached PR status here (server holds no creds).
        .route(
            "/api/sessions/{id}/prs/status",
            post(push_session_pr_status),
        )
        // The per-user CLI pushes cached PR comments here; viewers list them.
        .route(
            "/api/sessions/{id}/prs/comments",
            get(list_session_pr_comments).post(push_session_pr_comments),
        )
        .route(
            "/api/sessions/{id}/prs/comments/preview",
            get(list_session_pr_comment_previews),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sse_keep_alive_is_an_ignorable_comment() {
        use std::convert::Infallible;

        use http_body_util::BodyExt;

        let stream = futures::stream::pending::<std::result::Result<Event, Infallible>>();
        let response = Sse::new(stream)
            .keep_alive(sse_keep_alive_after(Duration::from_millis(1)))
            .into_response();
        let mut body = response.into_body();

        let frame = tokio::time::timeout(Duration::from_secs(1), body.frame())
            .await
            .expect("SSE keep-alive should arrive before the timeout")
            .expect("SSE body should remain open")
            .expect("SSE body frame should be valid");
        let data = frame.into_data().expect("keep-alive frame should be data");

        assert_eq!(data.as_ref(), b": keep-alive\n\n");
        assert!(TRANSPORT_KEEP_ALIVE_INTERVAL < Duration::from_secs(350));
    }

    #[test]
    fn pr_detection_joins_adjacent_text_without_crossing_tool_output() {
        let raw = json!({"type": "assistant", "uuid": "split-pr", "message": {
            "role": "assistant", "content": [
                {"type": "text", "text": "Review PR"},
                {"type": "text", "text": " #7. Another PR"},
                {"type": "tool_result", "content": "https://github.com/acme/widgets/pull/99"},
                {"type": "text", "text": "#8"}
            ]
        }});
        let session = core::parse_session_str(Some("split-pr"), &raw.to_string()).unwrap();
        let refs: Vec<_> = pr_message_texts(&session.messages[0])
            .flat_map(|text| crate::github::detect_pr_refs(&text, Some(("acme", "widgets")), &[]))
            .collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].number, 7);
    }

    #[test]
    fn parse_summary_json_bare() {
        let raw = r#"{"intent":"do X","invariants":["a","b"],"summary":"did X"}"#;
        let (intent, invariants, summary) = parse_summary_json(raw).unwrap();
        assert_eq!(intent, "do X");
        assert_eq!(invariants, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(summary, "did X");
    }

    #[test]
    fn parse_summary_json_fenced_with_prose() {
        let raw = "Here is the summary:\n```json\n{\"intent\":\"goal\",\"summary\":\"recap\"}\n```\nThanks!";
        let (intent, invariants, summary) = parse_summary_json(raw).unwrap();
        assert_eq!(intent, "goal");
        // invariants defaults to empty when omitted.
        assert!(invariants.is_empty());
        assert_eq!(summary, "recap");
    }

    #[test]
    fn parse_summary_json_invalid_is_bad_gateway() {
        // No JSON object at all.
        assert!(matches!(
            parse_summary_json("sorry, I cannot help with that").unwrap_err(),
            AppError::BadGateway(_)
        ));
        // Looks like an object but missing required fields.
        assert!(matches!(
            parse_summary_json(r#"{"foo":"bar"}"#).unwrap_err(),
            AppError::BadGateway(_)
        ));
    }

    fn stored_repo_link(local_path: &str, is_container: bool) -> crate::store::StoredRepoLink {
        crate::store::StoredRepoLink {
            repo: "example".to_string(),
            remote_url: Some("git@github.com:Example-Org/example-workspace.git".to_string()),
            branch: "chore/top-level".to_string(),
            pr_url: None,
            paths: vec![crate::store::RepoPath {
                user_email: "alice@example.com".to_string(),
                local_path: local_path.to_string(),
            }],
            is_container,
        }
    }

    fn stored_summary(repo: crate::store::StoredRepoLink) -> crate::store::StoredSummary {
        crate::store::StoredSummary {
            id: "s1".to_string(),
            agent: core::AgentKind::Claude,
            title: "Session".to_string(),
            project_path: Some("/Users/alice/example".to_string()),
            git_branch: Some("chore/top-level".to_string()),
            message_count: 1,
            started_at: None,
            ended_at: None,
            uploaded_by: "alice@example.com".to_string(),
            uploaded_at: chrono::Utc::now(),
            source_host: None,
            owner_email: Some("alice@example.com".to_string()),
            visibility: "organization".to_string(),
            is_live: false,
            repos: vec![repo],
        }
    }

    #[test]
    fn summary_with_container_flags_marks_viewer_container_path() {
        let summary = stored_summary(stored_repo_link("/Users/alice/example", false));
        let value = summary_with_container_flags(
            summary,
            Some("alice@example.com"),
            &["/Users/alice/example/".to_string()],
        );

        assert_eq!(value["repos"][0]["is_container"], true);
    }

    #[test]
    fn summary_with_container_flags_preserves_structural_container_flag() {
        let summary = stored_summary(stored_repo_link("/Users/alice/example", true));
        let value = summary_with_container_flags(summary, None, &[]);

        assert_eq!(value["repos"][0]["is_container"], true);
    }

    #[test]
    fn summary_flags_project_is_container_when_launch_dir_is_a_container() {
        // A session launched *at* the container root (its `project_path` matches
        // a viewer container path) is flagged so the SPA can suppress the
        // transcript-branch fallback — even with zero discovered repos.
        let summary = stored_summary(stored_repo_link("/Users/alice/example", false));
        let value = summary_with_container_flags(
            summary,
            Some("alice@example.com"),
            &["/Users/alice/example/".to_string()],
        );

        assert_eq!(value["project_is_container"], true);
    }

    #[test]
    fn summary_project_is_container_false_without_matching_container_path() {
        // No configured container paths ⇒ the launch dir is a normal work repo.
        let summary = stored_summary(stored_repo_link("/Users/alice/example", false));
        let value = summary_with_container_flags(summary, Some("alice@example.com"), &[]);

        assert_eq!(value["project_is_container"], false);
    }

    #[test]
    fn project_is_container_is_exact_root_not_descendant() {
        let containers = ["/Users/alice/example".to_string()];
        // The container root itself matches.
        assert!(project_is_container(
            Some("/Users/alice/example"),
            &containers
        ));
        // A sub-repo *below* the container does not (it's a real work repo).
        assert!(!project_is_container(
            Some("/Users/alice/example/example-platform"),
            &containers
        ));
        // A missing project path never matches.
        assert!(!project_is_container(None, &containers));
    }

    fn shares() -> Vec<LiveShare> {
        vec![
            LiveShare {
                email: "reader@x".to_string(),
                can_read: true,
                can_write: false,
                can_communicate: false,
            },
            LiveShare {
                email: "writer@x".to_string(),
                can_read: true,
                can_write: true,
                can_communicate: false,
            },
        ]
    }

    #[test]
    fn live_view_truth_table() {
        let owner = || Some("author@x".to_string());
        // Auth off: always ok.
        assert!(authorize_live_view(None, owner(), &shares()).is_ok());
        // Author ok.
        assert!(authorize_live_view(Some("author@x"), owner(), &shares()).is_ok());
        // Read-sharee ok.
        assert!(authorize_live_view(Some("reader@x"), owner(), &shares()).is_ok());
        // RW-sharee ok.
        assert!(authorize_live_view(Some("writer@x"), owner(), &shares()).is_ok());
        // Stranger => 404 (hide existence).
        assert!(matches!(
            authorize_live_view(Some("nobody@x"), owner(), &shares()).unwrap_err(),
            AppError::NotFound
        ));
    }

    #[test]
    fn live_author_truth_table() {
        let owner = || Some("author@x".to_string());
        assert!(authorize_live_author(None, owner()).is_ok());
        assert!(authorize_live_author(Some("author@x"), owner()).is_ok());
        // Even an rw-sharee is not the author => 403.
        assert!(matches!(
            authorize_live_author(Some("writer@x"), owner()).unwrap_err(),
            AppError::Forbidden(_)
        ));
        assert!(matches!(
            authorize_live_author(Some("reader@x"), owner()).unwrap_err(),
            AppError::Forbidden(_)
        ));
    }
}
