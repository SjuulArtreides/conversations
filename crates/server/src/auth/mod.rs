//! Authentication, gated entirely behind the `AUTH_ENABLED` flag.
//!
//! This module is the auth slice. When the flag is OFF it contributes nothing
//! to the router — [`crate::build_router`] does not call any of this. When the
//! flag is ON, [`build_auth`] produces the session layer + the auth routes +
//! the gate middleware, all composed at the single seam in `build_router`.
//!
//! Two login methods run in parallel, each independently toggleable:
//!   * **Email + password** (`PASSWORD_AUTH_ENABLED`, default on) — register and
//!     log in with an email + password, with optional SMTP signup verification.
//!   * **Google SSO (OIDC)** (active only when all three Google vars are set).
//!
//! ## Login flow (flag on)
//! - `POST /auth/register` → `{ email, password, display_name? }`: create the
//!   account (409 if it already has a password), hash the password with bcrypt,
//!   and immediately log in (store the email in the session).
//! - `POST /auth/login` → `{ email, password }`: verify against the stored
//!   bcrypt hash; 401 on bad credentials (same error for unknown email and wrong
//!   password, to avoid account enumeration).
//! - `GET /api/admin/users`: list every account with its administrator status.
//! - `DELETE /api/admin/users/{email}`: an admin permanently removes a user
//!   account (auth rows + preferences cascade; share grants are scrubbed;
//!   authored session/project content is kept). Refuses self-delete, initial
//!   admins, and the last effective admin.
//! - `GET/POST/DELETE /api/admin/admins`: initial admins grant and revoke
//!   persisted administrator access for existing users.
//! - `POST /api/admin/password-reset-links` → `{ email }`: an effective admin
//!   creates a 30-minute, one-time reset link for an existing account.
//! - `POST /auth/password-reset` → `{ token, password }`: consume that link and
//!   replace the password without starting a login session.
//! - `GET /auth/google/login` → builds the Google authorization URL (PKCE + CSRF
//!   state + nonce, all stashed in the session), 302-redirects to Google. Only
//!   mounted when Google is configured.
//! - `GET /auth/callback` → validates `state`, exchanges the `code`, verifies
//!   the ID token (including the `nonce`), enforces `email_verified` and the
//!   optional `allowed_hosted_domain` (`hd` claim), upserts the user into the
//!   directory, stores the email in the session, redirects to `/`.
//! - `POST /auth/logout` → clears the session, redirects to `/`.
//! - `GET /api/me` → `{ email, display_name }` for the logged-in user, else 401.
//! - `GET /api/auth/config` → `{ google_enabled, password_enabled, user }` so the
//!   SPA can render the right login affordances. (404 when auth is off, so the
//!   SPA shows no login UI at all.)
//!
//! ## Session store
//! Uses a database-backed [`SessionBackend`] built on the repository's own
//! pool, so sessions persist across restarts/redeploys (the cookie no longer
//! points at a vanished in-memory record). The backend follows the data store
//! chosen at runtime from `DATABASE_URL` (SQLite by default, Postgres when
//! configured).
//!
//! ## Current user
//! On every request the gate middleware reads the logged-in email from the
//! session and, when present, inserts [`CurrentUserEmail`] into the request
//! extensions. The existing [`crate::identity::CurrentUser`] extractor reads
//! that extension first, so handlers + sharing enforcement see the viewer with
//! no changes.

pub mod oidc;
pub mod signup_email;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use openidconnect::core::CoreAuthenticationFlow;
use openidconnect::{
    AuthorizationCode, CsrfToken, Nonce, PkceCodeChallenge, PkceCodeVerifier, Scope, TokenResponse,
};

use self::oidc::{oidc_http_client, GoogleClient};
use serde::Deserialize;
use serde_json::json;
use tower_sessions::{Session, SessionManagerLayer};

use crate::config::Config;
use crate::error::AppError;
use crate::identity::{CurrentUser, CurrentUserEmail};
use crate::state::AppState;
use crate::store::session::SessionBackend;

/// Session key holding the logged-in user's email once authenticated.
const SESSION_EMAIL_KEY: &str = "email";
/// Session key holding the account authentication version current at login.
const SESSION_AUTH_VERSION_KEY: &str = "auth.version";
/// Session key for the in-flight CSRF state token.
const SESSION_CSRF_KEY: &str = "oidc.csrf";
/// Session key for the in-flight PKCE verifier.
const SESSION_PKCE_KEY: &str = "oidc.pkce";
/// Session key for the in-flight nonce.
const SESSION_NONCE_KEY: &str = "oidc.nonce";
/// How long an administrator-issued password-reset link remains valid.
const RESET_TOKEN_TTL_MINUTES: i64 = 30;

/// The OIDC client, shared into the auth handlers via an extension layer.
#[derive(Clone)]
pub struct OidcClient(pub Arc<GoogleClient>);

/// Everything the composition root needs to wire auth into the router: the
/// session layer (with CSRF cookie protection), the optional OIDC client
/// extension, the auth routes, and the gate middleware are all applied by the
/// caller. `client` is `Some` only when Google is configured.
pub struct AuthLayers {
    pub session_layer: SessionManagerLayer<SessionBackend>,
    pub client: Option<OidcClient>,
    pub google_enabled: bool,
    pub password_enabled: bool,
    pub signup_mailer: Option<signup_email::SignupMailer>,
}

/// Build the auth layers from config. Performs OIDC discovery (one network
/// call). Returns an error if discovery fails or required settings are missing
/// — callers should propagate this to fail fast at startup.
///
/// The session cookie carries only an opaque, random session id; the session
/// data lives server-side in the database-backed [`SessionBackend`] passed in
/// by the caller. We therefore do not sign the cookie — there is no
/// client-trusted payload to tamper with. (If a client-side cookie store is
/// ever adopted, switch to a signed/private store keyed by a new
/// `SESSION_SECRET` config value — there is deliberately none today.)
pub async fn build_auth(
    config: &Config,
    session_store: SessionBackend,
) -> anyhow::Result<AuthLayers> {
    // Google OIDC is optional: only do discovery (a network call) when it is
    // fully configured. Password-only auth therefore needs no network access.
    let google_enabled = config.google_configured();
    let client = if google_enabled {
        Some(OidcClient(Arc::new(
            oidc::build_google_client(config).await?,
        )))
    } else {
        None
    };

    let session_layer = SessionManagerLayer::new(session_store)
        .with_name("conversations.sid")
        // `Secure` is configurable (COOKIE_SECURE); defaults to true when auth
        // is on behind https, false for local http dev. See `Config`.
        .with_secure(config.cookie_secure)
        .with_http_only(true)
        .with_same_site(tower_sessions::cookie::SameSite::Lax);

    Ok(AuthLayers {
        session_layer,
        client,
        google_enabled,
        password_enabled: config.password_auth_enabled,
        signup_mailer: signup_email::SignupMailer::build(config)?,
    })
}

/// Decide whether a request path requires an authenticated user when the flag
/// is ON.
///
/// **Default-deny over `/api/`.** Every API data endpoint requires auth UNLESS
/// it is on the explicit unauthenticated allowlist below. New API routes are
/// therefore gated by construction — adding one cannot accidentally leave it
/// open (the previous hand-maintained allowlist of *gated* prefixes had already
/// drifted, leaving `/api/project-paths`, `/api/llm/models`, `/api/prs/*`, and
/// `/api/user/settings` ungated).
///
/// ALLOWED without auth (returns `false`): the health probes, every auth route
/// `/auth/*`, the SPA shell / static asset GETs (anything not under `/api/`),
/// and the unauthenticated probes the SPA uses to discover its own state
/// (`/api/app`, `/api/me`, `/api/auth/config`, `/api/llm/status`).
///
/// REQUIRE auth (returns `true`): everything else under `/api/`.
pub fn requires_auth(path: &str) -> bool {
    // Unauthenticated allowlist — the SPA's own bootstrap probes + health.
    if path == "/healthz"
        || path == "/readyz"
        || path == "/api/app"
        || path == "/api/me"
        || path == "/api/auth/config"
        || path == "/api/llm/status"
    {
        return false;
    }
    if path.starts_with("/auth/") {
        return false;
    }
    // Default-deny: any other API endpoint is gated. Non-API paths (SPA shell +
    // static assets) are served openly; the SPA routes the user to login itself.
    path.starts_with("/api/")
}

/// Distinguish API requests (want a 401 JSON) from browser navigations (want a
/// 302 to the login page). Anything under `/api/` is treated as an API call.
fn is_api(path: &str) -> bool {
    path.starts_with("/api/")
}

/// The auth gate + current-user injection middleware (flag-on only).
///
/// For every request it reads the logged-in email from the session; when
/// present it inserts [`CurrentUserEmail`] so downstream handlers / the
/// `CurrentUser` extractor / sharing enforcement see the viewer. When the path
/// requires auth and there is no session email, it short-circuits: 401 for API
/// requests, 302 → `/auth/login` for navigations.
pub async fn auth_gate(
    State(state): State<AppState>,
    session: Session,
    mut req: axum::extract::Request,
    next: Next,
) -> Response {
    let mut email: Option<String> = session.get(SESSION_EMAIL_KEY).await.ok().flatten();

    if let Some(current_email) = email.as_deref() {
        let session_version = match session.get::<i64>(SESSION_AUTH_VERSION_KEY).await {
            Ok(version) => version.unwrap_or(0),
            Err(_) => return auth_internal_error(),
        };
        let current_version = match state.repo.get_user_auth_version(current_email).await {
            Ok(Some(version)) => version,
            Ok(None) => -1,
            Err(_) => return auth_internal_error(),
        };
        if session_version != current_version {
            if session.flush().await.is_err() {
                return auth_internal_error();
            }
            email = None;
        }
    }

    if let Some(email) = email.clone() {
        req.extensions_mut().insert(CurrentUserEmail(email));
    }

    let path = req.uri().path();
    if email.is_none() && requires_auth(path) {
        return if is_api(path) {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "authentication required" })),
            )
                .into_response()
        } else {
            Redirect::to("/auth/login").into_response()
        };
    }

    next.run(req).await
}

fn auth_internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "could not validate session" })),
    )
        .into_response()
}

/// Store an authenticated identity together with the account version current
/// at login. Password resets increment that version and invalidate the session.
async fn set_authenticated_session(
    state: &AppState,
    session: &Session,
    email: &str,
) -> Result<(), AppError> {
    let version = state
        .repo
        .get_user_auth_version(email)
        .await?
        .ok_or(AppError::NotFound)?;
    session
        .insert(SESSION_AUTH_VERSION_KEY, version)
        .await
        .map_err(session_err)?;
    session
        .insert(SESSION_EMAIL_KEY, email)
        .await
        .map_err(session_err)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /auth/google/login` — start the OIDC flow: stash PKCE/CSRF/nonce in the
/// session and redirect to Google's authorization endpoint.
async fn login(
    axum::Extension(OidcClient(client)): axum::Extension<OidcClient>,
    session: Session,
) -> Result<Redirect, AppError> {
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, csrf_token, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    session
        .insert(SESSION_CSRF_KEY, csrf_token.secret())
        .await
        .map_err(session_err)?;
    session
        .insert(SESSION_PKCE_KEY, pkce_verifier.secret())
        .await
        .map_err(session_err)?;
    session
        .insert(SESSION_NONCE_KEY, nonce.secret())
        .await
        .map_err(session_err)?;

    Ok(Redirect::to(auth_url.as_str()))
}

/// Query parameters Google sends back to the callback.
#[derive(Debug, Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// `GET /auth/callback` — validate state, exchange the code, verify the ID
/// token (incl. nonce), enforce `email_verified` + hosted-domain, upsert the
/// user, store the email in the session, redirect to `/`.
async fn callback(
    State(state): State<AppState>,
    axum::Extension(OidcClient(client)): axum::Extension<OidcClient>,
    session: Session,
    Query(params): Query<CallbackParams>,
) -> Result<Redirect, AppError> {
    if let Some(err) = params.error {
        return Err(AppError::BadRequest(format!(
            "oidc error from provider: {err}"
        )));
    }
    let code = params
        .code
        .ok_or_else(|| AppError::BadRequest("missing authorization code".into()))?;
    let returned_state = params
        .state
        .ok_or_else(|| AppError::BadRequest("missing state".into()))?;

    // CSRF: the returned state must match what we stashed.
    let expected_state: Option<String> =
        session.get(SESSION_CSRF_KEY).await.map_err(session_err)?;
    let expected_state =
        expected_state.ok_or_else(|| AppError::BadRequest("no login in progress".into()))?;
    if returned_state != expected_state {
        return Err(AppError::BadRequest("state mismatch".into()));
    }

    let pkce_secret: String = session
        .get(SESSION_PKCE_KEY)
        .await
        .map_err(session_err)?
        .ok_or_else(|| AppError::BadRequest("missing pkce verifier".into()))?;
    let nonce_secret: String = session
        .get(SESSION_NONCE_KEY)
        .await
        .map_err(session_err)?
        .ok_or_else(|| AppError::BadRequest("missing nonce".into()))?;

    // Exchange the authorization code for tokens.
    let http_client = oidc_http_client().map_err(AppError::Internal)?;
    let token_response = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|e| {
            AppError::Internal(anyhow::anyhow!("OIDC provider has no token endpoint: {e}"))
        })?
        .set_pkce_verifier(PkceCodeVerifier::new(pkce_secret))
        .request_async(&http_client)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("token exchange failed: {e}")))?;

    // Verify the ID token, including the nonce.
    let id_token = token_response
        .id_token()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("provider returned no id token")))?;
    let nonce = Nonce::new(nonce_secret);
    let claims = id_token
        .claims(&client.id_token_verifier(), &nonce)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("id token verification failed: {e}")))?;

    // Email + verification.
    let email = claims
        .email()
        .ok_or_else(|| AppError::BadRequest("id token has no email claim".into()))?
        .as_str()
        .to_string();
    if claims.email_verified() != Some(true) {
        return Err(AppError::BadRequest("email is not verified".into()));
    }

    // Optional hosted-domain restriction. Google emits an `hd` claim for Google
    // Workspace accounts; since that is a non-standard claim, we enforce against
    // the verified email's domain, which is equivalent for the gmail/Workspace
    // login case and avoids a custom claims type.
    if let Some(required) = &state.config.allowed_hosted_domain {
        let domain = email.rsplit('@').next();
        if domain != Some(required.as_str()) {
            return Err(AppError::BadRequest(format!(
                "hosted domain not allowed (require {required:?})"
            )));
        }
    }

    let display_name = claims
        .name()
        .and_then(|n| n.get(None))
        .map(|n| n.as_str().to_string());

    // Make the user discoverable.
    state
        .repo
        .upsert_user(&email, display_name.as_deref())
        .await?;

    // Clear the in-flight OIDC values, store the logged-in email.
    let _ = session.remove::<String>(SESSION_CSRF_KEY).await;
    let _ = session.remove::<String>(SESSION_PKCE_KEY).await;
    let _ = session.remove::<String>(SESSION_NONCE_KEY).await;
    set_authenticated_session(&state, &session, &email).await?;

    Ok(Redirect::to("/"))
}

/// `POST /auth/logout` — clear the session, redirect to `/`.
async fn logout(session: Session) -> Result<Redirect, AppError> {
    session.flush().await.map_err(session_err)?;
    Ok(Redirect::to("/"))
}

/// `GET /api/me` — the logged-in user, or 401 when unauthenticated.
async fn me(State(state): State<AppState>, session: Session) -> Result<Response, AppError> {
    let email: Option<String> = session.get(SESSION_EMAIL_KEY).await.ok().flatten();
    match email {
        Some(email) => {
            // Best-effort display name from the directory.
            let display_name = state
                .repo
                .search_users(Some(&email), 1)
                .await
                .ok()
                .and_then(|users| users.into_iter().find(|u| u.email == email))
                .and_then(|u| u.display_name);
            Ok(Json(json!({
                "email": email,
                "display_name": display_name,
                "is_admin": effective_is_admin(&state, &email).await?,
            }))
            .into_response())
        }
        None => Ok((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "not authenticated" })),
        )
            .into_response()),
    }
}

// ---------------------------------------------------------------------------
// Email + password (no email verification)
// ---------------------------------------------------------------------------

/// Minimum accepted password length. Deliberately modest — this is an internal
/// session-sharing tool, not a public service.
const MIN_PASSWORD_LEN: usize = 8;

/// Normalize an email for storage/lookup: trim + lowercase. Keeps password
/// accounts consistent with the (lowercased) share list and user directory.
fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Hash a password with bcrypt at the default cost.
fn hash_password(password: &str) -> Result<String, AppError> {
    bcrypt::hash(password, bcrypt::DEFAULT_COST)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("password hash failed: {e}")))
}

/// Verify a password against a stored bcrypt hash. A malformed hash (or any
/// bcrypt error) is treated as a non-match rather than a 500.
fn verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

/// Body for `POST /auth/register`.
#[derive(Debug, Deserialize)]
struct RegisterBody {
    email: String,
    password: String,
    display_name: Option<String>,
}

/// Body for `POST /auth/login`.
#[derive(Debug, Deserialize)]
struct LoginBody {
    email: String,
    password: String,
}

/// `POST /auth/register` — create an account and log in immediately. No email
/// verification. 409 if the email already has a password set.
async fn register(
    State(state): State<AppState>,
    session: Session,
    Json(body): Json<RegisterBody>,
) -> Result<Response, AppError> {
    let email = normalize_email(&body.email);
    if email.is_empty() || !email.contains('@') {
        return Err(AppError::BadRequest("a valid email is required".into()));
    }
    if body.password.len() < MIN_PASSWORD_LEN {
        return Err(AppError::BadRequest(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    let display_name = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Reject if an account with a password already exists for this email. A row
    // with no password (Google-only, or created via sharing) is allowed to set
    // one here — that's how someone "claims" an email they were shared with.
    if let Some((_, Some(_))) = state.repo.get_user_with_password(&email).await? {
        return Err(AppError::Conflict(
            "an account with this email already exists".into(),
        ));
    }
    // Email/password registration has no email verification. Never let a new
    // registrant claim any administrator identity. Initial admins must already
    // have an account before configuration; delegated admins with no password
    // must use their existing SSO identity or an admin-issued reset link.
    if state.config.is_initial_admin(&email) || state.repo.is_user_admin(&email).await? {
        return Err(AppError::Forbidden(
            "administrator accounts cannot self-register".into(),
        ));
    }

    let hash = hash_password(&body.password)?;
    let user = state.repo.upsert_user(&email, display_name).await?;
    state.repo.set_password(&email, &hash).await?;

    set_authenticated_session(&state, &session, &email).await?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "email": user.email,
            "display_name": user.display_name,
            "is_admin": effective_is_admin(&state, &user.email).await?,
        })),
    )
        .into_response())
}

/// `POST /auth/login` — verify credentials and start a session. Returns the same
/// 401 for unknown email and wrong password (no account enumeration).
async fn password_login(
    State(state): State<AppState>,
    session: Session,
    Json(body): Json<LoginBody>,
) -> Result<Response, AppError> {
    let email = normalize_email(&body.email);
    let invalid = || AppError::Unauthorized("invalid email or password".into());

    let (user, hash) = state
        .repo
        .get_user_with_password(&email)
        .await?
        .ok_or_else(invalid)?;
    let hash = hash.ok_or_else(invalid)?;
    if !verify_password(&body.password, &hash) {
        return Err(invalid());
    }

    // Refresh last_seen_at.
    let _ = state.repo.upsert_user(&email, None).await;

    set_authenticated_session(&state, &session, &email).await?;

    Ok(Json(json!({
        "email": user.email,
        "display_name": user.display_name,
        "is_admin": effective_is_admin(&state, &user.email).await?,
    }))
    .into_response())
}

/// Body for `POST /api/admin/password-reset-links`.
#[derive(Debug, Deserialize)]
struct CreatePasswordResetBody {
    email: String,
}

/// Body for `POST /auth/password-reset`.
#[derive(Debug, Deserialize)]
struct PasswordResetBody {
    token: String,
    password: String,
}

/// Body for `POST /api/admin/admins`.
#[derive(Debug, Deserialize)]
struct GrantAdminBody {
    email: String,
}

/// Whether an email has effective administrator access. Bootstrap admins come
/// from config; delegated admins are persisted in the database.
async fn effective_is_admin(state: &AppState, email: &str) -> Result<bool, AppError> {
    if state.config.is_initial_admin(email) {
        return Ok(true);
    }
    state.repo.is_user_admin(email).await
}

/// Resolve and authorize the current administrator.
async fn require_admin(state: &AppState, viewer: Option<String>) -> Result<String, AppError> {
    let email = viewer.ok_or_else(|| AppError::Unauthorized("authentication required".into()))?;
    if !effective_is_admin(state, &email).await? {
        return Err(AppError::Forbidden("administrator access required".into()));
    }
    Ok(email)
}

/// Create a short-lived, one-time reset link for an existing account.
async fn create_password_reset_link(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Json(body): Json<CreatePasswordResetBody>,
) -> Result<Response, AppError> {
    let admin = require_admin(&state, viewer).await?;

    let email = normalize_email(&body.email);
    if email.is_empty() || !email.contains('@') {
        return Err(AppError::BadRequest("a valid email is required".into()));
    }

    let token_id = uuid::Uuid::new_v4().to_string();
    let secret = uuid::Uuid::new_v4().to_string();
    let secret_hash = hash_password(&secret)?;
    let expires_at = chrono::Utc::now() + chrono::Duration::minutes(RESET_TOKEN_TTL_MINUTES);
    state
        .repo
        .create_password_reset_token(&email, &admin, &token_id, &secret_hash, expires_at)
        .await?;

    let token = format!("{token_id}.{secret}");
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "reset_path": format!("/reset-password?token={token}"),
            "expires_at": expires_at,
        })),
    )
        .into_response())
}

/// Consume a reset link and set a new password. Invalid, expired, and already
/// used links deliberately share one error response.
async fn reset_password(
    State(state): State<AppState>,
    Json(body): Json<PasswordResetBody>,
) -> Result<Response, AppError> {
    if body.password.len() < MIN_PASSWORD_LEN {
        return Err(AppError::BadRequest(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }

    let invalid = || AppError::BadRequest("reset link is invalid or expired".into());
    let (token_id, secret) = body.token.split_once('.').ok_or_else(invalid)?;
    if secret.contains('.')
        || uuid::Uuid::parse_str(token_id).is_err()
        || uuid::Uuid::parse_str(secret).is_err()
    {
        return Err(invalid());
    }

    let token = state
        .repo
        .get_password_reset_token(token_id, chrono::Utc::now())
        .await?
        .ok_or_else(invalid)?;
    if !verify_password(secret, &token.secret_hash) {
        return Err(invalid());
    }

    let password_hash = hash_password(&body.password)?;
    if !state
        .repo
        .consume_password_reset_token(token_id, &password_hash, chrono::Utc::now())
        .await?
    {
        return Err(invalid());
    }

    Ok(Json(json!({ "ok": true })).into_response())
}

/// List bootstrap and delegated administrators.
async fn list_admins(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
) -> Result<Response, AppError> {
    require_admin(&state, viewer).await?;

    let mut admins = BTreeMap::new();
    for email in &state.config.initial_admin_emails {
        admins.insert(email.clone(), true);
    }
    for email in state.repo.list_admin_emails().await? {
        admins.entry(email).or_insert(false);
    }
    let admins: Vec<_> = admins
        .into_iter()
        .map(|(email, is_initial)| {
            json!({
                "email": email,
                "is_initial": is_initial,
            })
        })
        .collect();
    Ok(Json(json!({ "admins": admins })).into_response())
}

/// List every directory user together with effective administrator status.
async fn list_admin_users(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
) -> Result<Response, AppError> {
    require_admin(&state, viewer).await?;

    let delegated: BTreeSet<_> = state.repo.list_admin_emails().await?.into_iter().collect();
    let users: Vec<_> = state
        .repo
        .list_users()
        .await?
        .into_iter()
        .map(|user| {
            let is_initial = state.config.is_initial_admin(&user.email);
            let is_admin = is_initial || delegated.contains(&user.email);
            json!({
                "email": user.email,
                "display_name": user.display_name,
                "is_admin": is_admin,
                "is_initial": is_initial,
            })
        })
        .collect();
    Ok(Json(json!({ "users": users })).into_response())
}

/// Grant administrator access to an existing user.
async fn grant_admin(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Json(body): Json<GrantAdminBody>,
) -> Result<Response, AppError> {
    let granted_by = require_admin(&state, viewer).await?;
    let email = normalize_email(&body.email);
    if email.is_empty() || !email.contains('@') {
        return Err(AppError::BadRequest("a valid email is required".into()));
    }
    if !state.config.is_initial_admin(&email) {
        state.repo.set_user_admin(&email, true, &granted_by).await?;
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "email": email,
            "is_initial": state.config.is_initial_admin(&email),
        })),
    )
        .into_response())
}

/// Revoke delegated administrator access. Bootstrap admins are immutable.
async fn revoke_admin(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(email): Path<String>,
) -> Result<Response, AppError> {
    let revoked_by = require_admin(&state, viewer).await?;
    let email = normalize_email(&email);
    if state.config.is_initial_admin(&email) {
        return Err(AppError::BadRequest(
            "initial administrators cannot be revoked".into(),
        ));
    }
    state
        .repo
        .set_user_admin(&email, false, &revoked_by)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Permanently delete a user account. Removes auth / preference rows and strips
/// the account from every share list it appears on; sessions and projects the
/// user authored stay with their `owner_email` string in place, so the audit
/// history remains readable after the account is gone.
///
/// Three guards exist so an admin cannot lock themselves (or everyone) out:
/// - self-deletion is refused;
/// - bootstrap administrators cannot be deleted through the app (they are
///   configured, not stored, and would simply reappear on their next login);
/// - the last effective administrator cannot be deleted.
async fn delete_admin_user(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(email): Path<String>,
) -> Result<Response, AppError> {
    let caller = require_admin(&state, viewer).await?;
    let email = normalize_email(&email);
    if email.is_empty() || !email.contains('@') {
        return Err(AppError::BadRequest("a valid email is required".into()));
    }
    if email == caller {
        return Err(AppError::BadRequest(
            "administrators cannot delete their own account".into(),
        ));
    }
    if state.config.is_initial_admin(&email) {
        return Err(AppError::BadRequest(
            "initial administrators cannot be deleted".into(),
        ));
    }
    if effective_is_admin(&state, &email).await? && count_effective_admins(&state).await? <= 1 {
        return Err(AppError::BadRequest(
            "cannot delete the last administrator".into(),
        ));
    }
    if !state.repo.delete_user(&email).await? {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Total effective administrators (configured + persisted), deduped by email.
async fn count_effective_admins(state: &AppState) -> Result<usize, AppError> {
    let mut admins: BTreeSet<String> = state.config.initial_admin_emails.iter().cloned().collect();
    for email in state.repo.list_admin_emails().await? {
        admins.insert(email);
    }
    Ok(admins.len())
}

/// `GET /api/auth/config` — what the SPA needs to render login UI: which methods
/// are enabled and the current user (if any). Always 200 when auth is on; the
/// route simply isn't mounted when auth is off (so the SPA gets a 404 and hides
/// all login UI).
async fn auth_config(
    State(state): State<AppState>,
    session: Session,
) -> Result<Response, AppError> {
    let email: Option<String> = session.get(SESSION_EMAIL_KEY).await.ok().flatten();
    let user = match email {
        Some(email) => {
            let display_name = state
                .repo
                .search_users(Some(&email), 1)
                .await
                .ok()
                .and_then(|users| users.into_iter().find(|u| u.email == email))
                .and_then(|u| u.display_name);
            Some(json!({
                "email": email,
                "display_name": display_name,
                "is_admin": effective_is_admin(&state, &email).await?,
            }))
        }
        None => None,
    };
    Ok(Json(json!({
        "google_enabled": state.config.google_configured(),
        "password_enabled": state.config.password_auth_enabled,
        "signup_verification_enabled": state.config.signup_email.smtp_url.is_some(),
        "user": user,
    }))
    .into_response())
}

/// Map a tower-sessions error into an internal [`AppError`].
fn session_err(e: tower_sessions::session::Error) -> AppError {
    AppError::Internal(anyhow::anyhow!("session error: {e}"))
}

/// The auth routes (flag-on only), parameterized over [`AppState`]. Google
/// routes are mounted only when `google_enabled`; password routes only when
/// `password_enabled`. The OIDC client extension is layered on in
/// `build_router` (only when a client exists).
pub fn routes(
    google_enabled: bool,
    password_enabled: bool,
    signup_verification: bool,
) -> Router<AppState> {
    let mut router = Router::new()
        .route("/auth/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/auth/config", get(auth_config))
        .route("/api/admin/users", get(list_admin_users))
        .route(
            "/api/admin/users/{email}",
            axum::routing::delete(delete_admin_user),
        )
        .route("/api/admin/admins", get(list_admins).post(grant_admin))
        .route(
            "/api/admin/admins/{email}",
            axum::routing::delete(revoke_admin),
        );

    if password_enabled {
        router = router
            .route("/auth/login", post(password_login))
            .route("/auth/password-reset", post(reset_password))
            .route(
                "/api/admin/password-reset-links",
                post(create_password_reset_link),
            );
    }

    if signup_verification {
        router = router
            .route("/auth/signup-email", post(signup_email::request_link))
            .route(
                "/auth/signup-email/complete",
                post(signup_email::complete_signup),
            );
    } else if password_enabled {
        router = router.route("/auth/register", post(register));
    }

    if google_enabled {
        router = router
            .route("/auth/google/login", get(login))
            .route("/auth/callback", get(callback));
    }

    router
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_auth_predicate() {
        // Public / allowed.
        assert!(!requires_auth("/healthz"));
        assert!(!requires_auth("/auth/login"));
        assert!(!requires_auth("/auth/callback"));
        assert!(!requires_auth("/auth/logout"));
        assert!(!requires_auth("/api/app"));
        assert!(!requires_auth("/api/me"));
        assert!(!requires_auth("/api/auth/config"));
        assert!(!requires_auth("/api/llm/status")); // ungated LLM probe
        assert!(!requires_auth("/")); // SPA shell
        assert!(!requires_auth("/sessions/abc")); // SPA route
        assert!(!requires_auth("/assets/app.js")); // static asset

        // Gated API data endpoints.
        assert!(requires_auth("/api/sessions"));
        assert!(requires_auth("/api/sessions/it-sess-1"));
        assert!(requires_auth("/api/sessions/it-sess-1/sharing"));
        assert!(requires_auth("/api/sessions/it-sess-1/project"));
        assert!(requires_auth("/api/sessions/it-sess-1/chats"));
        assert!(requires_auth("/api/projects"));
        assert!(requires_auth("/api/projects/p1"));
        assert!(requires_auth("/api/projects/p1/chats"));
        assert!(requires_auth("/api/chats/c1"));
        assert!(requires_auth("/api/chats/c1/messages"));
        assert!(requires_auth("/api/users"));
        assert!(requires_auth("/api/live"));
        assert!(requires_auth("/api/live/x/events"));
        assert!(requires_auth("/api/live/x/stream"));
        assert!(requires_auth("/api/live/x/complete"));
        assert!(requires_auth("/api/live/x/replies"));
        assert!(requires_auth("/api/live/x/replies/stream"));
        assert!(requires_auth("/api/live/x/pty/ws"));
        assert!(requires_auth("/api/live/x/host/ws"));
        assert!(requires_auth("/api/live/x/heartbeat"));
        assert!(requires_auth("/api/live/x/sharing"));
        // Default-deny: the routes the OLD allowlist had drifted away from are
        // now gated by construction.
        assert!(requires_auth("/api/project-paths"));
        assert!(requires_auth("/api/llm/models"));
        assert!(requires_auth("/api/prs/sync/stream"));
        assert!(requires_auth("/api/prs/watch/targets"));
        assert!(requires_auth("/api/user/settings"));
        // A hypothetical brand-new API route is gated without touching this list.
        assert!(requires_auth("/api/something-new"));
    }

    #[test]
    fn is_api_classifies_paths() {
        assert!(is_api("/api/sessions"));
        assert!(!is_api("/sessions/abc"));
        assert!(!is_api("/"));
    }
}
