use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// An explicit, user-named project grouping sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// Optional free-text description. `None` when unset.
    pub description: Option<String>,
    pub owner_email: Option<String>,
    pub session_count: usize,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
