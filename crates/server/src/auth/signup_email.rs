//! Optional email-first password registration.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::Json;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use openidconnect::url::Url;
use serde::Deserialize;
use serde_json::json;

use crate::{config::Config, error::AppError, state::AppState};

use super::{hash_password, normalize_email, MIN_PASSWORD_LEN};

const LINK_LIFETIME_MINUTES: i64 = 30;
const SEND_WINDOW: Duration = Duration::from_secs(3600);
const MAX_EMAILS_PER_WINDOW: usize = 60;

#[derive(Clone)]
pub struct SignupMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Address,
    public_url: Url,
    sent: Arc<Mutex<VecDeque<Instant>>>,
}

impl SignupMailer {
    pub(crate) fn build(config: &Config) -> anyhow::Result<Option<Self>> {
        let settings = &config.signup_email;
        let (smtp_url, from, public_url) =
            match (&settings.smtp_url, &settings.from, &settings.public_url) {
                (None, None, None) => return Ok(None),
                (Some(smtp_url), Some(from), Some(public_url)) => (smtp_url, from, public_url),
                _ => bail!("set SIGNUP_SMTP_URL, SIGNUP_EMAIL_FROM and SIGNUP_PUBLIC_URL together"),
            };
        if !config.password_auth_enabled {
            bail!("SMTP signup verification requires PASSWORD_AUTH_ENABLED");
        }

        let smtp = Url::parse(smtp_url).map_err(|_| anyhow::anyhow!("invalid SIGNUP_SMTP_URL"))?;
        if smtp.scheme() != "smtps"
            || smtp.host().is_none()
            || smtp.username().is_empty()
            || smtp.password().is_none_or(str::is_empty)
            || smtp.query().is_some()
            || smtp.fragment().is_some()
        {
            bail!("SIGNUP_SMTP_URL must be an authenticated smtps:// URL");
        }
        let public_url = Url::parse(public_url).context("invalid SIGNUP_PUBLIC_URL")?;
        if public_url.scheme() != "https"
            || public_url.host().is_none()
            || !public_url.username().is_empty()
            || public_url.password().is_some()
            || public_url.path() != "/"
            || public_url.query().is_some()
            || public_url.fragment().is_some()
        {
            bail!("SIGNUP_PUBLIC_URL must be an HTTPS origin");
        }
        Ok(Some(Self {
            transport: AsyncSmtpTransport::<Tokio1Executor>::from_url(smtp_url)
                .map_err(|_| anyhow::anyhow!("invalid SIGNUP_SMTP_URL"))?
                .timeout(Some(Duration::from_secs(15)))
                .build(),
            from: from.parse().context("invalid SIGNUP_EMAIL_FROM")?,
            public_url,
            sent: Arc::default(),
        }))
    }

    fn reserve_send(&self) -> bool {
        let now = Instant::now();
        let mut sent = self.sent.lock().expect("signup rate limiter");
        while sent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= SEND_WINDOW)
        {
            sent.pop_front();
        }
        if sent.len() >= MAX_EMAILS_PER_WINDOW {
            return false;
        }
        sent.push_back(now);
        true
    }

    async fn send(&self, to: Address, token: &str) -> anyhow::Result<()> {
        let mut link = self.public_url.join("/verify-email")?;
        link.set_fragment(Some(token));
        let message = Message::builder()
            .from(self.from.clone().into())
            .to(to.into())
            .subject("Verify your Conversations email address")
            .body(format!(
                "Open this link and choose a password for your Conversations account. \
                 The link expires in {LINK_LIFETIME_MINUTES} minutes.\n\n{link}\n\n\
                 If you did not request this email, you can ignore it."
            ))?;
        tokio::time::timeout(Duration::from_secs(15), self.transport.send(message)).await??;
        Ok(())
    }
}

#[derive(Deserialize)]
pub(super) struct RequestSignup {
    email: String,
}

#[derive(Deserialize)]
pub(super) struct CompleteSignup {
    token: String,
    password: String,
    display_name: Option<String>,
}

fn digest(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(ring::digest::digest(
        &ring::digest::SHA256,
        token.as_bytes(),
    ))
}

pub(super) async fn request_link(
    State(state): State<AppState>,
    Extension(mailer): Extension<SignupMailer>,
    Json(body): Json<RequestSignup>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let accepted = || (StatusCode::ACCEPTED, Json(json!({ "ok": true })));
    let email = normalize_email(&body.email);
    let address = email
        .parse::<Address>()
        .map_err(|_| AppError::BadRequest("a valid email is required".into()))?;

    if super::effective_is_admin(&state, &email).await?
        || state
            .repo
            .get_user_with_password(&email)
            .await?
            .is_some_and(|(_, hash)| hash.is_some())
    {
        return Ok(accepted());
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    let hash = digest(&token);
    let now = chrono::Utc::now();
    if !state
        .repo
        .reserve_signup_token(
            &email,
            &hash,
            now,
            now + chrono::Duration::minutes(LINK_LIFETIME_MINUTES),
        )
        .await?
    {
        return Ok(accepted());
    }
    if !mailer.reserve_send() {
        state.repo.remove_signup_token(&hash).await?;
        return Ok(accepted());
    }
    if mailer.send(address, &token).await.is_err() {
        state.repo.remove_signup_token(&hash).await?;
        // Relay errors can contain recipient details. Keep the response uniform.
        tracing::warn!("SMTP signup verification email could not be delivered");
    }
    Ok(accepted())
}

pub(super) async fn complete_signup(
    State(state): State<AppState>,
    Json(body): Json<CompleteSignup>,
) -> Result<Json<serde_json::Value>, AppError> {
    let invalid = || AppError::BadRequest("verification link is invalid or expired".into());
    if body.token.len() != 32 || uuid::Uuid::parse_str(&body.token).is_err() {
        return Err(invalid());
    }
    if !(MIN_PASSWORD_LEN..=72).contains(&body.password.len()) {
        return Err(AppError::BadRequest(format!(
            "password must be between {MIN_PASSWORD_LEN} and 72 bytes"
        )));
    }
    let hash = digest(&body.token);
    if !state
        .repo
        .has_valid_signup_token(&hash, chrono::Utc::now())
        .await?
    {
        return Err(invalid());
    }
    let password = body.password;
    let password_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(anyhow::Error::from)??;
    let display_name = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if !state
        .repo
        .complete_signup(
            &hash,
            &password_hash,
            display_name,
            &state.config.initial_admin_emails,
            chrono::Utc::now(),
        )
        .await?
    {
        return Err(invalid());
    }
    Ok(Json(json!({ "ok": true })))
}
