//! Wire types for the project-scoped agent inbox.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPeer {
    pub session_id: String,
    pub project_id: Option<String>,
    pub title: String,
    pub agent: String,
    pub owner_email: Option<String>,
    /// Last live activity, or upload time. This is not an online-presence guarantee.
    pub last_seen_at: String,
    /// Server-side presence inputs, omitted from the directory wire response.
    #[serde(skip)]
    pub live_status: Option<String>,
    #[serde(skip)]
    pub live_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageStatus {
    Queued,
    Received,
    Acknowledged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: String,
    pub project_id: Option<String>,
    pub sender_session_id: String,
    pub sender_owner_email: Option<String>,
    pub recipient_session_id: String,
    pub text: String,
    pub reply_to: Option<String>,
    pub idempotency_key: String,
    pub created_at: String,
    pub received_at: Option<String>,
    pub acknowledged_at: Option<String>,
    pub status: AgentMessageStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendAgentMessage {
    pub recipient_session_id: String,
    pub text: String,
    pub idempotency_key: String,
    pub reply_to: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiveAgentMessages {
    #[serde(default)]
    pub acknowledge: Vec<String>,
    #[serde(default)]
    pub wait_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffRequest {
    pub brief: String,
    pub idempotency_key: String,
    pub agent: Option<crate::AgentKind>,
    /// Omitted when unset, so model-less requests still reach older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHandoff {
    pub session_id: String,
    pub source_session_id: String,
    pub project_id: String,
    pub agent: String,
    pub brief: String,
    pub idempotency_key: String,
    pub status: String,
    pub error: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// The model a handoff child must start on, in the child agent's own naming.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffModel {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// Validate a local handoff selection before reserving or launching a child.
pub fn validate_handoff_model(
    agent: crate::AgentKind,
    model: Option<&str>,
    variant: Option<&str>,
) -> Result<Option<HandoffModel>, String> {
    let Some(model) = model else {
        return if variant.is_some() {
            Err("variant needs a model".into())
        } else {
            Ok(None)
        };
    };
    if model.len() > 200
        || !model
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        || !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '@'))
    {
        return Err(
            "model must be 1-200 model-name characters, starting with a letter or digit".into(),
        );
    }
    if agent == crate::AgentKind::OpenCode
        && !model
            .split_once('/')
            .is_some_and(|(p, m)| !p.is_empty() && !m.is_empty())
    {
        return Err("an OpenCode model must be provider/model".into());
    }
    if let Some(v) = variant {
        if !(1..=32).contains(&v.len())
            || !v
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
        {
            return Err("variant must be 1-32 lowercase letters, digits, _ or -".into());
        }
        if agent == crate::AgentKind::Claude
            && !["low", "medium", "high", "xhigh", "max"].contains(&v)
        {
            return Err("unsupported Claude effort variant".into());
        }
    }
    Ok(Some(HandoffModel {
        model: model.into(),
        variant: variant.map(str::to_owned),
    }))
}

/// Claim body sent by hosts that understand [`HandoffModel`]. Older hosts send none.
pub const HANDOFF_MODEL_CAPABILITY: &str = "model";

/// Message metadata between one session and one peer under current
/// authorization. Includes first-line message titles; no state proves work completed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSummary {
    pub sent_count: u64,
    pub received_count: u64,
    pub last_message_at: String,
    /// `sent` or `received`, from the summarized session's point of view.
    pub last_direction: String,
    /// Status of the newest message the session sent; `None` when it sent none.
    pub last_sent_status: Option<AgentMessageStatus>,
    /// Incoming messages the session has not acknowledged.
    pub unacknowledged_incoming: u64,
    pub messages: Vec<ConversationMessageTitle>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMessageTitle {
    pub id: String,
    pub title: String,
    pub direction: String,
    pub created_at: String,
}

#[cfg(test)]
mod handoff_model_tests {
    use super::*;
    use crate::AgentKind;

    #[test]
    fn validates_model_selection_before_launch() {
        assert_eq!(
            validate_handoff_model(AgentKind::Claude, None, None).unwrap(),
            None
        );
        assert!(validate_handoff_model(AgentKind::Claude, None, Some("high")).is_err());
        for model in ["", "--model", "x\ny", "x y"] {
            assert!(validate_handoff_model(AgentKind::Codex, Some(model), None).is_err());
        }
        assert!(validate_handoff_model(AgentKind::OpenCode, Some("model"), None).is_err());
        assert!(validate_handoff_model(AgentKind::OpenCode, Some("provider/"), None).is_err());
        assert!(
            validate_handoff_model(AgentKind::OpenCode, Some("provider/model"), Some("high"))
                .is_ok()
        );
        assert!(
            validate_handoff_model(AgentKind::Claude, Some("sonnet"), Some("invented")).is_err()
        );
        assert!(validate_handoff_model(AgentKind::Claude, Some("sonnet"), Some("high")).is_ok());
    }
}
