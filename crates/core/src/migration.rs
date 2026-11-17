//! A one-time native session snapshot. No source paths or launch settings cross this boundary.
use serde::{Deserialize, Serialize};

pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub const MIGRATION_CHUNK_BYTES: usize = 6 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationManifest {
    pub agent: crate::AgentKind,
    pub native_id: String,
    pub title: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationImport {
    pub transfer: String,
    pub destination: String,
    pub cwd: String,
    pub manifest: MigrationManifest,
    pub offset: u64,
    pub b64: String,
}
