//! Read-only pages of actual private agent exchanges. No messaging authority is implied.
use crate::AgentKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunicationPage {
    pub peers: Vec<CommunicationPeer>,
    pub exchanges: Vec<CommunicationExchange>,
    pub next: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunicationPeer {
    pub session_id: String,
    pub host_id: Option<String>,
    pub title: String,
    pub agent: Option<AgentKind>,
    pub last_seen_at: Option<String>,
    pub connected: Option<bool>,
    pub state: String,
    pub reachable: Option<bool>,
    pub readable: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunicationExchange {
    pub id: String,
    pub peer_session_id: String,
    pub host_id: Option<String>,
    pub direction: String,
    pub title: String,
    /// The recorded state, without polling, delivering, or inferring expiry.
    pub status: String,
    pub created_at: Option<String>,
}
