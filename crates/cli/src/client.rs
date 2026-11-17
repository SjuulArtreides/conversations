//! Thin HTTP client wrapper around the conversations server API.
//!
//! The request/response structs here are the **wire contract** with the server
//! — the server slice deserializes exactly these shapes. Keep them in sync.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{HeaderMap, COOKIE, SET_COOKIE};
use serde::{Deserialize, Serialize};

use crate::credentials::{self, SESSION_COOKIE_NAME};

/// Wall-clock budget for retrying a request whose transport fails transiently
/// (connection reset/refused, send/timeout errors) — e.g. while the server is
/// being rolled during a deploy. The live + PR data-path calls retry within
/// this budget so an in-progress session survives a redeploy instead of dying
/// on the first `Connection reset by peer`. HTTP error *statuses* are never
/// retried (they are returned to the caller); only transport errors are.
const RETRY_BUDGET: Duration = Duration::from_secs(60);
/// First backoff after a failed attempt; doubles each retry up to `RETRY_MAX_DELAY`.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(5);

/// Response from `GET {server}/healthz`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
}

/// Result of a successful `login`/`register`: the user plus the session cookie
/// pair to persist and replay on later requests.
#[derive(Debug, Clone)]
pub struct AuthResult {
    pub email: String,
    pub display_name: Option<String>,
    /// Full `conversations.sid=<value>` pair from the `Set-Cookie` response.
    pub cookie: String,
}

/// HTTP client bound to a single server base URL.
pub struct ApiClient {
    base: String,
    http: Client,
    /// Stored session cookie for `base` (from a prior `login`), replayed on
    /// requests so uploads work when the server has auth enabled.
    cookie: Option<String>,
}

impl ApiClient {
    /// Build a client for `base` (trailing slash trimmed), loading any saved
    /// session cookie for that server so authenticated requests work.
    pub fn new(base: &str) -> Result<Self> {
        Self::with_cookie(base, credentials::cookie_for(base.trim_end_matches('/')))
    }

    /// Use the host's selected login, including an explicit --cookie-file.
    pub fn with_cookie(base: &str, cookie: Option<String>) -> Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("conversations/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;
        let base = base.trim_end_matches('/').to_string();
        Ok(Self { base, http, cookie })
    }

    /// Attach the stored session cookie (when present) to a request.
    fn with_auth(&self, rb: RequestBuilder) -> RequestBuilder {
        match &self.cookie {
            Some(c) => rb.header(COOKIE, c),
            None => rb,
        }
    }

    /// Send `rb`, retrying on **transient transport errors** (connection
    /// reset/refused, send/timeout failures) until [`RETRY_BUDGET`] is
    /// exhausted, with exponential-capped backoff. This is what lets the live +
    /// PR data-path calls ride out a server redeploy instead of dying on the
    /// first `Connection reset by peer`.
    ///
    /// HTTP error *statuses* are NOT retried — they come back as `Ok(resp)`
    /// with a non-success status for the caller to handle. Only transport-level
    /// failures (no usable response at all) are retried.
    ///
    /// All our request bodies are in-memory (JSON / empty), so `try_clone`
    /// always succeeds; the `expect` documents that invariant.
    fn send_retrying(&self, rb: RequestBuilder) -> reqwest::Result<Response> {
        let start = Instant::now();
        let mut delay = RETRY_BASE_DELAY;
        loop {
            let attempt = rb
                .try_clone()
                .expect("retryable request must have a cloneable (in-memory) body");
            match attempt.send() {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if !is_transient(&e) || start.elapsed() >= RETRY_BUDGET {
                        return Err(e);
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(RETRY_MAX_DELAY);
                }
            }
        }
    }

    /// `POST /auth/login` (or `/auth/register` when `register`) — authenticate
    /// with email + password and return the user plus the session cookie. The
    /// caller persists the cookie via [`crate::credentials`].
    pub fn authenticate(&self, register: bool, email: &str, password: &str) -> Result<AuthResult> {
        let path = if register {
            "/auth/register"
        } else {
            "/auth/login"
        };
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "email": email, "password": password }))
            .send()
            .with_context(|| format!("POST {url} failed"))?;

        let status = resp.status();
        // Pull the cookie out before the body consumes the response.
        let cookie = extract_session_cookie(resp.headers());

        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                anyhow::bail!("login failed: invalid email or password");
            }
            if status == reqwest::StatusCode::NOT_FOUND {
                anyhow::bail!(
                    "{} has no {path} endpoint — the server does not have password auth enabled",
                    self.base
                );
            }
            anyhow::bail!("POST {url} returned HTTP {status}: {body}");
        }

        let cookie = cookie.ok_or_else(|| {
            anyhow::anyhow!("login succeeded but the server returned no session cookie")
        })?;

        #[derive(Deserialize)]
        struct AuthBody {
            email: String,
            display_name: Option<String>,
        }
        let body: AuthBody = resp
            .json()
            .with_context(|| format!("POST {url} returned an unexpected body"))?;
        Ok(AuthResult {
            email: body.email,
            display_name: body.display_name,
            cookie,
        })
    }

    /// `GET /healthz` — returns the parsed health body on success.
    pub fn healthz(&self) -> Result<HealthResponse> {
        let url = format!("{}/healthz", self.base);
        let resp = self
            .with_auth(self.http.get(&url))
            .send()
            .with_context(|| format!("GET {url} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("GET {url} returned HTTP {status}");
        }
        resp.json::<HealthResponse>()
            .with_context(|| format!("GET {url} returned an unexpected body"))
    }

    /// The server this client talks to (no trailing slash).
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET /api/sessions/{id}/prs` — the `publication` snapshot (`None` for
    /// an unmanaged session, or a server that predates the field) and the PR
    /// links the server lists. A PR cache producer reads it before running `gh`
    /// and pushes under exactly that snapshot. The links may only NARROW what a
    /// producer publishes: they never choose a `gh` target and never prove that
    /// a pull request is published.
    pub fn central_prs(&self, session_id: &str) -> Result<CentralPrs> {
        #[derive(Deserialize)]
        struct Link {
            owner: String,
            repo: String,
            number: i64,
        }
        #[derive(Deserialize)]
        struct Prs {
            #[serde(default)]
            prs: Vec<Link>,
            #[serde(default)]
            publication: Option<conversations_core::publication::PublicationSnapshot>,
        }
        let url = format!("{}/api/sessions/{session_id}/prs", self.base);
        let resp = self
            .send_retrying(self.with_auth(self.http.get(&url)))
            .with_context(|| format!("GET {url} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            anyhow::bail!("GET {url} returned HTTP {status}: {text}");
        }
        let prs: Prs = resp
            .json()
            .with_context(|| format!("GET {url} returned an unexpected body"))?;
        Ok(CentralPrs {
            publication: prs.publication,
            links: prs
                .prs
                .into_iter()
                .map(|l| (l.owner, l.repo, l.number))
                .collect(),
        })
    }

    /// `POST /api/sessions/{id}/prs/status` — push a freshly-fetched PR status
    /// snapshot up to the server (which caches and serves it). The server is
    /// the GitHub-credential-free side of the Wave 2.1 design.
    pub fn push_pr_status(
        &self,
        session_id: &str,
        source: PrCacheSource,
        owner: &str,
        repo: &str,
        number: i64,
        status: &crate::commands::gh::PrStatus,
    ) -> Result<()> {
        let url = format!("{}/api/sessions/{session_id}/prs/status", self.base);
        let mut body = serde_json::json!({
            "owner": owner,
            "repo": repo,
            "number": number,
            "status": {
                "state": status.state,
                "merged": status.merged,
                "draft": status.draft,
                "title": status.title,
                "review_decision": status.review_decision,
            },
        });
        source.claim(&mut body);
        let resp = self
            .send_retrying(self.with_auth(self.http.post(&url).json(&body)))
            .with_context(|| format!("POST {url} failed"))?;
        let status_code = resp.status();
        if !status_code.is_success() {
            if status_code == reqwest::StatusCode::UNAUTHORIZED {
                anyhow::bail!(
                    "push PR status rejected (401): this server requires authentication — run \
                     `conversations login` first"
                );
            }
            let text = resp.text().unwrap_or_default();
            anyhow::bail!("POST {url} returned HTTP {status_code}: {text}");
        }
        Ok(())
    }

    /// `POST /api/sessions/{id}/prs/comments` — push a freshly-fetched batch of
    /// PR comments up to the server's cache (which serves them to the web
    /// panel). Server returns 204; we only surface a transport / status error.
    ///
    /// Wire body: `{ owner, repo, number, comments: [PrComment] }`, where each
    /// `PrComment` serializes to
    /// `{ comment_id, kind, author, body, html_url, created_at }`.
    pub fn push_pr_comments(
        &self,
        session_id: &str,
        source: PrCacheSource,
        owner: &str,
        repo: &str,
        number: i64,
        comments: &[crate::commands::gh::PrComment],
    ) -> Result<()> {
        let url = format!("{}/api/sessions/{session_id}/prs/comments", self.base);
        let mut body = serde_json::json!({
            "owner": owner,
            "repo": repo,
            "number": number,
            "comments": comments,
        });
        source.claim(&mut body);
        let resp = self
            .send_retrying(self.with_auth(self.http.post(&url).json(&body)))
            .with_context(|| format!("POST {url} failed"))?;
        let status_code = resp.status();
        if !status_code.is_success() {
            if status_code == reqwest::StatusCode::UNAUTHORIZED {
                anyhow::bail!(
                    "push PR comments rejected (401): this server requires authentication — run \
                     `conversations login` first"
                );
            }
            let text = resp.text().unwrap_or_default();
            anyhow::bail!("POST {url} returned HTTP {status_code}: {text}");
        }
        Ok(())
    }
}

/// What `GET /api/sessions/{id}/prs` says about a session's PR caches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CentralPrs {
    pub publication: Option<conversations_core::publication::PublicationSnapshot>,
    /// `(owner, repo, number)` of each PR the server lists for the session.
    pub links: Vec<(String, String, i64)>,
}

impl CentralPrs {
    pub fn lists(&self, owner: &str, repo: &str, number: i64) -> bool {
        self.links.iter().any(|(o, r, n)| {
            *n == number && o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repo)
        })
    }
}

/// The source a PR cache push claims. `Published` carries the exact snapshot
/// read before `gh` ran; the server accepts it only while the session is still
/// at that snapshot. `Unmanaged` sends no snapshot, which a published session
/// refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrCacheSource {
    Unmanaged,
    Published(conversations_core::publication::PublicationSnapshot),
}

impl PrCacheSource {
    fn claim(self, body: &mut serde_json::Value) {
        if let Self::Published(snapshot) = self {
            body["publication"] = serde_json::json!(snapshot);
        }
    }
}

/// Whether a `reqwest` error is a transient transport failure worth retrying
/// (the server is briefly unreachable — typically a deploy rolling the pod).
///
/// We retry connect/timeout/request-send failures, and — because reqwest's
/// categorization of a mid-send `Connection reset by peer` is version-dependent
/// — also walk the source chain for a connection-level `io::Error`. We never
/// treat a decoded body or an HTTP status as transient.
fn is_transient(e: &reqwest::Error) -> bool {
    if e.is_connect() || e.is_timeout() || e.is_request() {
        return true;
    }
    let mut src: Option<&dyn std::error::Error> = std::error::Error::source(e);
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            if matches!(
                io.kind(),
                ConnectionReset
                    | ConnectionRefused
                    | ConnectionAborted
                    | BrokenPipe
                    | UnexpectedEof
                    | TimedOut
            ) {
                return true;
            }
        }
        src = s.source();
    }
    false
}

/// Find the `conversations.sid=<value>` pair among the response's `Set-Cookie`
/// headers, dropping the attributes (`; HttpOnly; Path=/; …`).
fn extract_session_cookie(headers: &HeaderMap) -> Option<String> {
    let prefix = format!("{SESSION_COOKIE_NAME}=");
    for val in headers.get_all(SET_COOKIE) {
        if let Ok(s) = val.to_str() {
            let pair = s.split(';').next().unwrap_or(s).trim();
            if pair.starts_with(&prefix) {
                return Some(pair.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_host_cookie_is_used_for_authenticated_pr_pushes() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let head = String::from_utf8(request).unwrap().to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length: "))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            stream.read_exact(&mut body).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{\"prs\":[]}").unwrap();
            head
        });
        let client =
            ApiClient::with_cookie(&base, Some("conversations.sid=explicit-host".into())).unwrap();
        client
            .push_pr_comments("bound-session", PrCacheSource::Unmanaged, "o", "r", 1, &[])
            .unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("post /api/sessions/bound-session/prs/comments "));
        assert!(request.contains("\r\ncookie: conversations.sid=explicit-host\r\n"));
    }
}
