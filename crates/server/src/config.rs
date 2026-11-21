//! Runtime configuration, loaded from environment variables.

use std::path::Path;

/// Server configuration.
///
/// All fields are populated from the environment in [`Config::from_env`], with
/// sensible defaults so the server runs out of the box for local / kind use.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to bind the HTTP listener to. Env: `BIND_ADDR`.
    pub bind_addr: String,
    /// sqlx connection URL. Env: `DATABASE_URL`.
    pub database_url: String,
    /// Whether auth is enabled. Env: `AUTH_ENABLED`, default `true`.
    ///
    /// When on (default), the auth slice gates the API data endpoints and wires
    /// whichever login method(s) are configured (Google OIDC and/or
    /// email+password). When explicitly turned off, the server exposes zero auth
    /// surface and every session is anonymously readable and writable — only
    /// for local development and tests on a trusted machine.
    pub auth_enabled: bool,

    /// Whether email+password login is enabled (only relevant when
    /// `auth_enabled`). Env: `PASSWORD_AUTH_ENABLED`, default `true`. This lets
    /// you turn on real identities — and therefore sharing — with no Google
    /// configuration at all. SMTP verification for new accounts is optional.
    pub password_auth_enabled: bool,

    pub signup_email: SignupEmailConfig,

    /// Normalized bootstrap administrator email addresses. Env:
    /// `INITIAL_ADMIN_EMAILS`, as a comma-separated list. These administrators
    /// cannot be revoked through the app and may grant admin access to existing
    /// users.
    pub initial_admin_emails: Vec<String>,

    // --- Google OIDC settings (only required when `auth_enabled`) ---
    /// Google OAuth client id. Env: `GOOGLE_CLIENT_ID`.
    pub google_client_id: Option<String>,
    /// Google OAuth client secret. Env: `GOOGLE_CLIENT_SECRET`.
    pub google_client_secret: Option<String>,
    /// OIDC redirect/callback URL registered with Google, e.g.
    /// `http://localhost:8080/auth/callback`. Env: `OIDC_REDIRECT_URL`.
    pub oidc_redirect_url: Option<String>,
    /// Optional Google hosted-domain (`hd`) restriction — only emails in this
    /// domain may log in. Env: `ALLOWED_HOSTED_DOMAIN`.
    pub allowed_hosted_domain: Option<String>,

    /// Directory of built SPA static assets to serve (with SPA fallback to
    /// `index.html`). Env: `STATIC_DIR`. When unset or missing, the server is
    /// API-only (handy for tests / API-only deployments).
    pub static_dir: Option<String>,

    /// Whether the session cookie carries the `Secure` attribute. Env:
    /// `COOKIE_SECURE`. When unset it defaults to `true` only when auth is on
    /// AND the redirect URL is https (i.e. the deploy is TLS-fronted), so local
    /// http development still works without configuration.
    pub cookie_secure: bool,

    /// Maximum accepted request body size, in bytes, for session uploads and
    /// live-event reconciliation batches. Env: `MAX_UPLOAD_BYTES`. Defaults to
    /// 32 MiB to match the ingress `proxy-body-size`. Caps memory use from a
    /// single large transcript request.
    pub max_upload_bytes: usize,

    /// OpenRouter API key for the LLM features (chat + summary). Env:
    /// `OPENROUTER_API_KEY`. When absent, the LLM features are disabled and
    /// their endpoints return 503 — the rest of the app is unaffected.
    pub openrouter_api_key: Option<String>,
    /// Model slug passed to OpenRouter. Env: `OPENROUTER_MODEL`.
    pub openrouter_model: String,
    /// OpenRouter API base URL. Env: `OPENROUTER_BASE_URL`.
    pub openrouter_base_url: String,

    /// Seconds to keep serving in-flight requests after SIGTERM while the
    /// readiness probe reports unready, before tearing connections down. Env:
    /// `SHUTDOWN_DRAIN_SECS`, default 5. This window lets the load balancer /
    /// kube-proxy observe the failing `/readyz` and stop routing new traffic
    /// before we close open connections — it replaces a `preStop: sleep` and
    /// works on any base image (including distroless). Must stay comfortably
    /// below the Deployment's `terminationGracePeriodSeconds`.
    pub shutdown_drain_secs: u64,

    /// How often the abandoned-live-session reaper runs, in seconds. Env:
    /// `LIVE_REAP_INTERVAL_SECS`, default 60. `0` disables the reaper.
    pub live_reap_interval_secs: u64,

    /// A `live` session whose `last_event_at` is older than this many seconds is
    /// considered abandoned (its CLI stopped heartbeating) and is reaped. Env:
    /// `LIVE_ABANDON_AFTER_SECS`, default 300. MUST stay comfortably above the
    /// CLI's 30s heartbeat interval or an idle-but-live interactive session is
    /// reaped mid-turn.
    pub live_abandon_after_secs: u64,
}

/// SMTP signup settings. Debug output must never include the SMTP URL credentials.
#[derive(Clone, Default)]
pub struct SignupEmailConfig {
    pub smtp_url: Option<String>,
    pub from: Option<String>,
    pub public_url: Option<String>,
}

impl std::fmt::Debug for SignupEmailConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignupEmailConfig")
            .field("smtp_configured", &self.smtp_url.is_some())
            .field("from", &self.from)
            .field("public_url", &self.public_url)
            .finish()
    }
}

/// Default transcript request body limit: 32 MiB (matches ingress
/// `proxy-body-size`).
const DEFAULT_MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;

/// Default OpenRouter model when `OPENROUTER_MODEL` is unset.
const DEFAULT_OPENROUTER_MODEL: &str = "anthropic/claude-sonnet-4";
/// Default OpenRouter API base URL.
const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Default graceful-shutdown drain window, in seconds.
const DEFAULT_SHUTDOWN_DRAIN_SECS: u64 = 5;

/// Default live-session reaper interval, in seconds.
const DEFAULT_LIVE_REAP_INTERVAL_SECS: u64 = 60;
/// Default abandon threshold, in seconds (10× the CLI's 30s heartbeat).
const DEFAULT_LIVE_ABANDON_AFTER_SECS: u64 = 300;
/// Hard floor for the abandon threshold: it must stay strictly above the CLI's
/// 30s heartbeat, or a live-but-idle interactive turn gets reaped mid-turn. A
/// configured value below this falls back to the default.
const MIN_LIVE_ABANDON_AFTER_SECS: u64 = 31;

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:8080".to_string(),
            database_url: "sqlite://data/conversations.db?mode=rwc".to_string(),
            auth_enabled: true,
            password_auth_enabled: true,
            initial_admin_emails: Vec::new(),
            google_client_id: None,
            google_client_secret: None,
            oidc_redirect_url: None,
            allowed_hosted_domain: None,
            signup_email: SignupEmailConfig::default(),
            static_dir: None,
            cookie_secure: false,
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
            openrouter_api_key: None,
            openrouter_model: DEFAULT_OPENROUTER_MODEL.to_string(),
            openrouter_base_url: DEFAULT_OPENROUTER_BASE_URL.to_string(),
            shutdown_drain_secs: DEFAULT_SHUTDOWN_DRAIN_SECS,
            live_reap_interval_secs: DEFAULT_LIVE_REAP_INTERVAL_SECS,
            live_abandon_after_secs: DEFAULT_LIVE_ABANDON_AFTER_SECS,
        }
    }
}

impl Config {
    /// Build a [`Config`] from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let default = Config::default();
        let auth_enabled = std::env::var("AUTH_ENABLED")
            .ok()
            .map(|v| parse_bool(&v))
            .unwrap_or(default.auth_enabled);
        let password_auth_enabled = std::env::var("PASSWORD_AUTH_ENABLED")
            .ok()
            .map(|v| parse_bool(&v))
            .unwrap_or(default.password_auth_enabled);
        let oidc_redirect_url = env_opt("OIDC_REDIRECT_URL");
        let signup_email = SignupEmailConfig {
            smtp_url: env_opt("SIGNUP_SMTP_URL"),
            from: env_opt("SIGNUP_EMAIL_FROM"),
            public_url: env_opt("SIGNUP_PUBLIC_URL"),
        };
        // Local HTTP still works when neither hosted auth flow is configured.
        let cookie_secure = match std::env::var("COOKIE_SECURE") {
            Ok(v) => parse_bool(&v),
            Err(_) => {
                auth_enabled
                    && [
                        oidc_redirect_url.as_deref(),
                        signup_email.public_url.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|url| url.starts_with("https://"))
            }
        };
        let max_upload_bytes = std::env::var("MAX_UPLOAD_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(default.max_upload_bytes);
        Config {
            bind_addr: std::env::var("BIND_ADDR").unwrap_or(default.bind_addr),
            database_url: std::env::var("DATABASE_URL").unwrap_or(default.database_url),
            auth_enabled,
            password_auth_enabled,
            signup_email,
            initial_admin_emails: parse_email_list(
                &std::env::var("INITIAL_ADMIN_EMAILS").unwrap_or_default(),
            ),
            google_client_id: env_opt("GOOGLE_CLIENT_ID"),
            google_client_secret: env_opt("GOOGLE_CLIENT_SECRET"),
            oidc_redirect_url,
            allowed_hosted_domain: env_opt("ALLOWED_HOSTED_DOMAIN"),
            static_dir: env_opt("STATIC_DIR"),
            cookie_secure,
            max_upload_bytes,
            openrouter_api_key: env_opt("OPENROUTER_API_KEY"),
            openrouter_model: env_opt("OPENROUTER_MODEL")
                .unwrap_or_else(|| default.openrouter_model.clone()),
            openrouter_base_url: env_opt("OPENROUTER_BASE_URL")
                .unwrap_or_else(|| default.openrouter_base_url.clone()),
            shutdown_drain_secs: std::env::var("SHUTDOWN_DRAIN_SECS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(default.shutdown_drain_secs),
            live_reap_interval_secs: std::env::var("LIVE_REAP_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(default.live_reap_interval_secs),
            live_abandon_after_secs: std::env::var("LIVE_ABANDON_AFTER_SECS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|&n| n >= MIN_LIVE_ABANDON_AFTER_SECS)
                .unwrap_or(default.live_abandon_after_secs),
        }
    }

    /// Whether the LLM features (chat + summary) are enabled — i.e. an
    /// OpenRouter API key is configured.
    pub fn llm_enabled(&self) -> bool {
        self.openrouter_api_key.is_some()
    }

    /// Whether Google OIDC is fully configured (all three vars present). Google
    /// is all-or-nothing: a partial config is a hard error in [`validate_auth`].
    pub fn google_configured(&self) -> bool {
        self.google_client_id.is_some()
            && self.google_client_secret.is_some()
            && self.oidc_redirect_url.is_some()
    }

    /// Whether `email` is in the immutable bootstrap administrator allowlist.
    pub fn is_initial_admin(&self, email: &str) -> bool {
        let normalized = email.trim().to_lowercase();
        self.initial_admin_emails
            .iter()
            .any(|admin| admin == &normalized)
    }

    /// Validate the auth configuration when `auth_enabled` is on. Fails fast
    /// with a clear error so a misconfigured deploy never silently starts with
    /// broken auth. The rules:
    ///   * Google OIDC is optional, but all-or-nothing — set all three of
    ///     `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` / `OIDC_REDIRECT_URL`, or
    ///     none. A partial Google config is rejected.
    ///   * At least one login method must be usable: either Google is fully
    ///     configured, or `PASSWORD_AUTH_ENABLED` is on (the default).
    ///
    /// No session-signing secret is required: the session cookie carries only an
    /// opaque, random server-side id (the data lives in the DB-backed
    /// `SessionBackend`), so there is no client-trusted payload to sign.
    pub fn validate_auth(&self) -> Result<(), String> {
        if !self.auth_enabled {
            return Ok(());
        }
        let google_present = [
            self.google_client_id.is_some(),
            self.google_client_secret.is_some(),
            self.oidc_redirect_url.is_some(),
        ];
        let present_count = google_present.iter().filter(|&&p| p).count();
        if present_count > 0 && present_count < 3 {
            let mut missing = Vec::new();
            if self.google_client_id.is_none() {
                missing.push("GOOGLE_CLIENT_ID");
            }
            if self.google_client_secret.is_none() {
                missing.push("GOOGLE_CLIENT_SECRET");
            }
            if self.oidc_redirect_url.is_none() {
                missing.push("OIDC_REDIRECT_URL");
            }
            return Err(format!(
                "AUTH_ENABLED: Google OIDC is partially configured; set all of \
                 GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET, OIDC_REDIRECT_URL or none \
                 (missing: {})",
                missing.join(", ")
            ));
        }
        if !self.google_configured() && !self.password_auth_enabled {
            return Err("AUTH_ENABLED is on but no login method is configured: \
                 enable PASSWORD_AUTH_ENABLED (the default) or set the Google OIDC env vars"
                .to_string());
        }
        Ok(())
    }

    /// For a file-backed sqlite URL, make sure the parent directory exists so
    /// sqlx can create the database file.
    pub fn ensure_sqlite_parent_dir(&self) -> std::io::Result<()> {
        if let Some(path) = sqlite_file_path(&self.database_url) {
            if let Some(parent) = Path::new(&path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
        }
        Ok(())
    }
}

/// Read an environment variable, treating empty/whitespace-only as absent.
fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Parse a permissive boolean (`1`, `true`, `yes`, `on` — case-insensitive).
fn parse_bool(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Parse and normalize a comma-separated email allowlist.
fn parse_email_list(value: &str) -> Vec<String> {
    let mut emails: Vec<String> = value
        .split(',')
        .map(|email| email.trim().to_lowercase())
        .filter(|email| !email.is_empty())
        .collect();
    emails.sort();
    emails.dedup();
    emails
}

/// Extract the on-disk file path from a `sqlite://` URL, if it is file-backed
/// (i.e. not an in-memory `:memory:` database).
fn sqlite_file_path(url: &str) -> Option<String> {
    // Only sqlite URLs have an on-disk file. A non-sqlite URL (e.g. postgres)
    // has no parent dir to create — returning None keeps the caller from
    // trying to `create_dir_all` a path derived from a `postgres://` URL,
    // which would fail on a read-only root filesystem.
    let rest = url.strip_prefix("sqlite://")?;
    // Strip any query string (e.g. `?mode=rwc`).
    let path = rest.split('?').next().unwrap_or(rest);
    if path.is_empty() || path.contains(":memory:") || path == "memory" {
        return None;
    }
    Some(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_booleans() {
        assert!(parse_bool("true"));
        assert!(parse_bool("1"));
        assert!(parse_bool(" YES "));
        assert!(!parse_bool("false"));
        assert!(!parse_bool("0"));
        assert!(!parse_bool(""));
    }

    #[test]
    fn normalizes_admin_email_allowlist() {
        let cfg = Config {
            initial_admin_emails: parse_email_list(
                " Alice@Example.com, bob@example.com, alice@example.com, ",
            ),
            ..Config::default()
        };
        assert!(cfg.is_initial_admin("ALICE@example.com"));
        assert!(cfg.is_initial_admin("bob@example.com"));
        assert!(!cfg.is_initial_admin("eve@example.com"));
        assert_eq!(cfg.initial_admin_emails.len(), 2);
    }

    #[test]
    fn auth_is_enabled_by_default() {
        let cfg = Config::default();
        assert!(cfg.auth_enabled);
        assert!(cfg.password_auth_enabled);
        assert!(cfg.validate_auth().is_ok());
    }

    #[test]
    fn validate_auth_allows_password_only() {
        let cfg = Config {
            auth_enabled: true,
            password_auth_enabled: true,
            ..Config::default()
        };
        assert!(cfg.validate_auth().is_ok());
        assert!(!cfg.google_configured());
    }

    #[test]
    fn validate_auth_rejects_no_method() {
        let cfg = Config {
            auth_enabled: true,
            password_auth_enabled: false,
            ..Config::default()
        };
        assert!(cfg.validate_auth().is_err());
    }

    #[test]
    fn validate_auth_rejects_partial_google() {
        let cfg = Config {
            auth_enabled: true,
            password_auth_enabled: true,
            google_client_id: Some("id".into()),
            ..Config::default()
        };
        let err = cfg.validate_auth().unwrap_err();
        assert!(err.contains("partially configured"), "got: {err}");
    }

    #[test]
    fn validate_auth_accepts_full_google_without_password() {
        let cfg = Config {
            auth_enabled: true,
            password_auth_enabled: false,
            google_client_id: Some("id".into()),
            google_client_secret: Some("secret".into()),
            oidc_redirect_url: Some("https://example.com/auth/callback".into()),
            ..Config::default()
        };
        assert!(cfg.validate_auth().is_ok());
        assert!(cfg.google_configured());
    }

    #[test]
    fn extracts_sqlite_path() {
        assert_eq!(
            sqlite_file_path("sqlite://data/conversations.db?mode=rwc"),
            Some("data/conversations.db".to_string())
        );
        assert_eq!(sqlite_file_path("sqlite::memory:"), None);
        assert_eq!(sqlite_file_path("sqlite://:memory:"), None);
        // A non-sqlite URL has no on-disk path — must not be mistaken for one
        // (else ensure_sqlite_parent_dir would create_dir_all a bogus path and
        // fail on a read-only root filesystem).
        assert_eq!(
            sqlite_file_path("postgresql://user:pw@host:5432/db?sslmode=require"),
            None
        );
        assert_eq!(sqlite_file_path("postgres://user:pw@host/db"), None);
    }
}
