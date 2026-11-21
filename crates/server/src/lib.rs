//! `conversations-server`: Axum JSON API + SQLite/Postgres storage + the built
//! React SPA (served as static assets) for sharing Claude Code sessions.
//!
//! [`build_router`] is the single composition seam: it wires the API routes,
//! the SPA static-serving fallback, and (when `AUTH_ENABLED`) the auth routes +
//! session layer + gate middleware. Keeping all composition here means each
//! concern is layered in exactly one place.

mod agent_messaging;
pub mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod github;
pub mod host_relay;
pub mod identity;
pub mod linear;
pub mod live;
pub mod llm;
pub mod publication;
pub mod reaper;
pub mod state;
pub mod static_files;
pub mod store;
pub mod user_keys;

pub use config::Config;
pub use state::AppState;
pub use store::{Repository, SqliteRepo};

use axum::Router;
use tower_http::trace::TraceLayer;

/// Baseline security headers on every response (both routers):
///
/// - `X-Content-Type-Options: nosniff` — the SPA/API never relies on MIME
///   sniffing; forbidding it stops a mis-served `text/plain` asset from being
///   interpreted as script.
/// - `Referrer-Policy: strict-origin-when-cross-origin` — default-safe
///   referrer behavior without leaking full URLs to third parties.
/// - `Strict-Transport-Security` — only set when auth is on (deployments run
///   behind TLS). Browsers ignore the header on plain-HTTP responses anyway,
///   but local/kind HTTP development must not accumulate HSTS state.
/// - `Content-Security-Policy` — only on SPA responses (the API returns
///   JSON/SSE, where a policy adds nothing). The directive set is the
///   minimum the real Vite build needs, verified against `dist/`:
///   one external module script + one external stylesheet, no inline
///   scripts, no web fonts, no workers, no `<img>` tags (only the `data:`
///   favicon in `index.html`), and all API/WebSocket traffic is same-origin
///   (`ws:`/`wss:` to `window.location.host`). `style-src` carries
///   `'unsafe-inline'` because xterm.js injects a `<style>` element at
///   runtime and a handful of React `style={{…}}` attributes exist; styles
///   are non-executable so this is the pragmatic trade-off. Everything else
///   falls back to `'self'`, `object-src 'none'` blocks plugin content, and
///   `frame-ancestors 'none'` is the clickjacking defense.
async fn security_headers(
    hsts: bool,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::HeaderValue;
    let mut res = next.run(req).await;
    // CSP only on HTML documents: the API returns JSON/SSE (where a policy
    // adds nothing), and the SPA fallback always serves `text/html`.
    let is_html = res
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("text/html"));
    let headers = res.headers_mut();
    if is_html {
        headers.insert(
            "content-security-policy",
            HeaderValue::from_static(
                "default-src 'self'; \
                 script-src 'self'; \
                 style-src 'self' 'unsafe-inline'; \
                 img-src 'self' data:; \
                 font-src 'self'; \
                 connect-src 'self'; \
                 object-src 'none'; \
                 base-uri 'self'; \
                 form-action 'self'; \
                 frame-ancestors 'none'",
            ),
        );
    }
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "referrer-policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    if hsts {
        headers.insert(
            "strict-transport-security",
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    res
}

/// Build the full application router from an already-constructed [`AppState`].
///
/// This **auth-off** variant (the local/kind default and what the integration
/// tests use) wires the API routes + the SPA static fallback. There is zero
/// auth surface. When auth is on, [`build_router_with_auth`] performs OIDC
/// discovery and layers the auth pieces on top of the same base.
pub fn build_router(state: AppState) -> Router {
    let static_dir = state.config.static_dir.clone();
    let mut router = Router::new().merge(api::routes(state.config.max_upload_bytes));
    router = with_static(router, static_dir.as_deref());
    router
        .with_state(state)
        .layer(axum::middleware::from_fn(move |req, next| {
            security_headers(false, req, next)
        }))
        .layer(TraceLayer::new_for_http())
}

/// Attach the SPA static-serving fallback when `STATIC_DIR` is set and exists.
/// API routes are registered on the router and take precedence; anything
/// unmatched (assets, client routes) is served from the SPA build.
fn with_static(router: Router<AppState>, static_dir: Option<&str>) -> Router<AppState> {
    match static_dir {
        Some(dir) if std::path::Path::new(dir).is_dir() => {
            router.fallback_service(static_files::spa_service(dir))
        }
        Some(dir) => {
            tracing::warn!(dir, "STATIC_DIR set but not a directory; serving API only");
            router
        }
        None => router,
    }
}

/// Build the full router with auth layered on (email+password and/or Google
/// OIDC, per config).
///
/// Performs OIDC discovery against Google ONLY when Google is fully configured
/// (the only network call, made only here) — password-only auth needs no
/// network access. The auth routes are merged with the optional OIDC client
/// extension; the gate middleware (current-user injection + 401/redirect for
/// protected paths) and the session layer are layered over everything. The SPA
/// static fallback is included as in the auth-off path; the gate allows
/// static/asset GETs through so the login page can load.
pub async fn build_router_with_auth(state: AppState) -> anyhow::Result<Router> {
    use axum::middleware::from_fn_with_state;

    // Build the persistent session store on the repo's own pool (so sessions
    // live in the same DB as conversation data and survive restarts).
    let session_store = state.repo.session_store().await?;

    let auth::AuthLayers {
        session_layer,
        client,
        google_enabled,
        password_enabled,
        signup_mailer,
    } = auth::build_auth(&state.config, session_store).await?;

    let static_dir = state.config.static_dir.clone();
    let mut router = Router::new()
        .merge(api::routes(state.config.max_upload_bytes))
        .merge(auth::routes(
            google_enabled,
            password_enabled,
            signup_mailer.is_some(),
        ));
    router = with_static(router, static_dir.as_deref());

    let auth_state = state.clone();
    let mut router = router
        .with_state(state)
        // Gate middleware: injects CurrentUserEmail + enforces auth on protected
        // paths. Layered before the session layer so the session is available.
        .layer(from_fn_with_state(auth_state, auth::auth_gate));
    // OIDC client available to the auth handlers — only when Google is wired.
    if let Some(client) = client {
        router = router.layer(axum::Extension(client));
    }
    if let Some(mailer) = signup_mailer {
        router = router.layer(axum::Extension(mailer));
    }
    let router = router
        // Session layer outermost (runs first) so Session is in extensions.
        .layer(session_layer)
        .layer(axum::middleware::from_fn(move |req, next| {
            security_headers(true, req, next)
        }))
        .layer(TraceLayer::new_for_http());

    Ok(router)
}
