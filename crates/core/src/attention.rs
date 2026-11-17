//! Host-owned human attention, separate from agent message delivery.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Subject {
    Credential { kid: String },
    LocalOwner,
}
impl Subject {
    pub fn key(&self) -> &str {
        match self {
            Self::Credential { kid } => kid,
            Self::LocalOwner => "local_owner",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub subject: Subject,
    pub label: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionRequest {
    pub recipient: Subject,
    pub message: String,
    pub idempotency_key: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub session_id: String,
    pub session_title: String,
    pub kind: String,
    pub sender: Identity,
    pub recipient: Subject,
    pub message: String,
    pub created_at: String,
    pub read_at: Option<String>,
    pub resolved_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestArgs {
    pub session: String,
    pub recipient: Subject,
    pub message: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionArgs {
    pub session: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    pub after: Option<i64>,
    pub limit: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkArgs {
    pub session: String,
    pub id: String,
}
