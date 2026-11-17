//! Current-user extraction for sharing enforcement.
//!
//! [`CurrentUser`] resolves the viewer email for a request. The resolution
//! order is designed so the auth slice (Google OIDC) can plug in without
//! touching handlers, while the auth-off default stays fully testable:
//!
//! 1. If a [`CurrentUserEmail`] request extension is present (the auth slice
//!    inserts it after validating a login session) → `Some(that email)`.
//! 2. Else, when auth is disabled, read the optional `X-Demo-User` header so
//!    enforcement can be exercised in dev / tests without a real login →
//!    `Some(header)` when present.
//! 3. Otherwise → `None` (no viewer; handlers treat this as "no filtering").

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::state::AppState;

/// The authenticated user's email, inserted as a request extension by the auth
/// slice once a login session is validated. Defined here so the auth slice and
/// this extractor agree on the type.
#[derive(Debug, Clone)]
pub struct CurrentUserEmail(pub String);

/// The resolved viewer for a request, if any. `None` means "no filtering".
#[derive(Debug, Clone)]
pub struct CurrentUser(pub Option<String>);

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // (1) Auth slice extension wins.
        if let Some(CurrentUserEmail(email)) = parts.extensions.get::<CurrentUserEmail>() {
            return Ok(CurrentUser(Some(email.clone())));
        }

        // (2) Auth off: optional dev/testing header.
        if !state.config.auth_enabled {
            if let Some(value) = parts.headers.get("x-demo-user") {
                if let Ok(s) = value.to_str() {
                    let s = s.trim();
                    if !s.is_empty() {
                        return Ok(CurrentUser(Some(s.to_string())));
                    }
                }
            }
        }

        // (3) No viewer.
        Ok(CurrentUser(None))
    }
}
