//! Google OIDC client construction.
//!
//! The client is discovered from `https://accounts.google.com` (the
//! discovery document is fetched at startup — the only network call this slice
//! makes, and only when `AUTH_ENABLED` is on). Endpoints are never hardcoded.

use openidconnect::core::{CoreClient, CoreProviderMetadata};
use openidconnect::{
    AsyncHttpClient, ClientId, ClientSecret, EndpointMaybeSet, EndpointNotSet, EndpointSet,
    HttpClientError, HttpRequest, HttpResponse, IssuerUrl, RedirectUrl,
};
use std::{future::Future, pin::Pin};

use crate::config::Config;

/// Google's OIDC issuer. Discovery (`/.well-known/openid-configuration`) is
/// performed against this URL; the concrete auth/token/jwks endpoints come from
/// the discovery document, never hardcoded.
pub const GOOGLE_ISSUER: &str = "https://accounts.google.com";

/// The concrete [`CoreClient`] typestate we build via discovery. OIDC metadata
/// always carries an authorization endpoint (`EndpointSet`); the token and
/// user-info endpoints are optional in the spec so openidconnect tracks them as
/// `EndpointMaybeSet` (used via fallible methods). The device / introspection /
/// revocation endpoints are never set.
pub type GoogleClient = CoreClient<
    EndpointSet,      // auth
    EndpointNotSet,   // device auth
    EndpointNotSet,   // introspection
    EndpointNotSet,   // revocation
    EndpointMaybeSet, // token
    EndpointMaybeSet, // user info
>;

/// Build the reqwest client used for OIDC network calls (discovery + token
/// exchange). Redirects are disabled to avoid SSRF, per the openidconnect 4.x
/// guidance for server-side HTTP clients.
pub fn oidc_http_client() -> anyhow::Result<OidcHttpClient> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map(OidcHttpClient)
        .map_err(|e| anyhow::anyhow!("failed to build OIDC http client: {e}"))
}

// openidconnect 4's built-in adapter targets reqwest 0.12, not 0.13.
pub struct OidcHttpClient(reqwest::Client);

impl<'c> AsyncHttpClient<'c> for OidcHttpClient {
    type Error = HttpClientError<reqwest::Error>;
    type Future =
        Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + Sync + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let response = self
                .0
                .execute(request.try_into().map_err(Box::new)?)
                .await
                .map_err(Box::new)?;
            let mut builder = openidconnect::http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            builder
                .body(response.bytes().await.map_err(Box::new)?.to_vec())
                .map_err(HttpClientError::Http)
        })
    }
}

/// Build a discovered [`GoogleClient`] from the configured Google OIDC
/// settings. Performs one network request (the discovery fetch).
///
/// Fails with a descriptive error if discovery fails or the configured
/// redirect URL is invalid.
pub async fn build_google_client(config: &Config) -> anyhow::Result<GoogleClient> {
    let client_id = config
        .google_client_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("GOOGLE_CLIENT_ID is required when AUTH_ENABLED"))?;
    let client_secret = config
        .google_client_secret
        .clone()
        .ok_or_else(|| anyhow::anyhow!("GOOGLE_CLIENT_SECRET is required when AUTH_ENABLED"))?;
    let redirect_url = config
        .oidc_redirect_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("OIDC_REDIRECT_URL is required when AUTH_ENABLED"))?;

    let issuer_url = IssuerUrl::new(GOOGLE_ISSUER.to_string())
        .map_err(|e| anyhow::anyhow!("invalid issuer url: {e}"))?;

    let http_client = oidc_http_client()?;
    let provider_metadata = CoreProviderMetadata::discover_async(issuer_url, &http_client)
        .await
        .map_err(|e| anyhow::anyhow!("OIDC discovery against {GOOGLE_ISSUER} failed: {e}"))?;

    let redirect = RedirectUrl::new(redirect_url.clone())
        .map_err(|e| anyhow::anyhow!("invalid OIDC_REDIRECT_URL {redirect_url:?}: {e}"))?;

    let client = CoreClient::from_provider_metadata(
        provider_metadata,
        ClientId::new(client_id),
        Some(ClientSecret::new(client_secret)),
    )
    .set_redirect_uri(redirect);

    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, http::StatusCode, routing::post, Router};

    #[tokio::test]
    async fn oidc_transport_preserves_requests_and_does_not_follow_redirects() {
        let app = Router::new().route(
            "/token",
            post(|headers: axum::http::HeaderMap, body: Bytes| async move {
                assert_eq!(headers["content-type"], "application/x-www-form-urlencoded");
                assert_eq!(headers["authorization"], "Basic dGVzdDo=");
                assert_eq!(body.as_ref(), b"grant_type=authorization_code&code=test");
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [("location", "/must-not-follow"), ("x-provider", "test")],
                    "redirect body",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let request = openidconnect::http::Request::builder()
            .method("POST")
            .uri(format!("http://{address}/token"))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("authorization", "Basic dGVzdDo=")
            .body(b"grant_type=authorization_code&code=test".to_vec())
            .unwrap();
        let response = oidc_http_client().unwrap().call(request).await.unwrap();
        server.abort();

        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()["location"], "/must-not-follow");
        assert_eq!(response.headers()["x-provider"], "test");
        assert_eq!(response.body(), b"redirect body");
    }

    #[tokio::test]
    async fn oidc_transport_reports_connection_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let request = openidconnect::http::Request::builder()
            .uri(format!("http://{address}/token"))
            .body(Vec::new())
            .unwrap();

        assert!(oidc_http_client().unwrap().call(request).await.is_err());
    }
}
