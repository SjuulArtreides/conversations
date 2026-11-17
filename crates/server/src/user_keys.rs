//! Account-key discovery only. Publication never grants authority on any host.
use crate::{
    error::{AppError, Result},
    identity::CurrentUser,
    AppState,
};
use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use chrono::{DateTime, Utc};
use ring::{digest, signature};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DOMAIN: &str = "conversations-browser-key/v1\n";
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserKey {
    pub kid: String,
    pub alg: String,
    pub public_key: String,
    pub label: String,
    pub created_at: DateTime<Utc>,
}
#[derive(Debug, Clone)]
pub struct Challenge {
    pub challenge: String,
    pub user_email: String,
    pub payload: String,
    pub expires_at: i64,
}
#[derive(Deserialize)]
struct Search {
    q: Option<String>,
    limit: Option<i64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Begin {
    public_key: String,
    label: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Statement {
    v: u8,
    purpose: String,
    user_email: String,
    public_key: String,
    kid: String,
    label: String,
    challenge: String,
    expires_at: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finish {
    challenge: String,
    signature: String,
}
fn owner(user: Option<String>) -> Result<String> {
    user.ok_or_else(|| AppError::Unauthorized("Sign in to view or publish browser keys".into()))
}
fn invalid() -> AppError {
    AppError::BadRequest("Invalid, expired or already used browser key proof".into())
}

async fn directory(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(search): Query<Search>,
) -> Result<Json<Value>> {
    owner(user)?;
    let users = state
        .repo
        .search_users(
            search.q.as_deref().map(str::trim).filter(|q| !q.is_empty()),
            search.limit.unwrap_or(20),
        )
        .await?;
    let mut entries = Vec::with_capacity(users.len());
    for user in users {
        let keys = state.repo.browser_keys(&user.email).await?;
        entries.push(json!({"email":user.email,"display_name":user.display_name,"keys":keys}));
    }
    Ok(Json(json!({"users":entries})))
}
async fn begin(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(input): Json<Begin>,
) -> Result<Json<Value>> {
    let email = owner(user)?;
    let public = STANDARD.decode(&input.public_key).map_err(|_| invalid())?;
    if public.len() != 65
        || public[0] != 4
        || input.label.trim().is_empty()
        || input.label.len() > 80
        || input.label.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let challenge = uuid::Uuid::new_v4().simple().to_string();
    let expires_at = Utc::now().timestamp() + 300;
    let payload = serde_json::to_string(&Statement {
        v: 1,
        purpose: "publish_browser_key".into(),
        user_email: email.clone(),
        public_key: STANDARD.encode(&public),
        kid: URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, &public).as_ref()),
        label: input.label.trim().into(),
        challenge: challenge.clone(),
        expires_at,
    })?;
    state
        .repo
        .save_browser_key_challenge(&Challenge {
            challenge: challenge.clone(),
            user_email: email,
            payload: payload.clone(),
            expires_at,
        })
        .await?;
    Ok(Json(json!({"challenge":challenge,"payload":payload})))
}
async fn finish(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(input): Json<Finish>,
) -> Result<Json<Value>> {
    let email = owner(user)?;
    let now = Utc::now().timestamp();
    let c = state
        .repo
        .browser_key_challenge(&input.challenge, &email, now)
        .await?
        .ok_or_else(invalid)?;
    let statement: Statement = serde_json::from_str(&c.payload)?;
    let public = STANDARD
        .decode(&statement.public_key)
        .map_err(|_| invalid())?;
    let sig = URL_SAFE_NO_PAD
        .decode(&input.signature)
        .map_err(|_| invalid())?;
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, &public)
        .verify(format!("{DOMAIN}{}", c.payload).as_bytes(), &sig)
        .map_err(|_| invalid())?;
    let key = BrowserKey {
        kid: statement.kid,
        alg: "es256".into(),
        public_key: statement.public_key,
        label: statement.label,
        created_at: Utc::now(),
    };
    if !state.repo.publish_browser_key(&c, &key, now).await? {
        return Err(invalid());
    }
    Ok(Json(json!({"key":key})))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/user-keys", get(directory).post(finish))
        .route("/api/user-keys/challenge", post(begin))
}
