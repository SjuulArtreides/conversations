//! Persisted CLI credentials — the session cookie obtained via `login`, keyed
//! by server base URL so one machine can authenticate against several servers.
//!
//! Stored at `$XDG_CONFIG_HOME/arrakis-conversations/credentials.json` (or the
//! platform equivalent via `dirs::config_dir`). The file holds the full
//! `conversations.sid=<value>` cookie pair, which `upload`/`ping` replay as a
//! `Cookie` header. It is a credential, so the file is written `0600` on unix.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Name of the server's session cookie (see `auth::build_auth`).
pub const SESSION_COOKIE_NAME: &str = "conversations.sid";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    /// server base URL → session cookie pair (`conversations.sid=<value>`).
    #[serde(default)]
    servers: BTreeMap<String, String>,
}

fn path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("could not determine a config directory")?;
    Ok(dir.join("arrakis-conversations").join("credentials.json"))
}

/// Canonical key for a server: trailing slash trimmed (matches `ApiClient`).
fn normalize(server: &str) -> String {
    server.trim_end_matches('/').to_string()
}

fn load() -> Result<Store> {
    let p = path()?;
    match std::fs::read(&p) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", p.display())),
    }
}

fn save(store: &Store) -> Result<()> {
    let p = path()?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(store)?;
    std::fs::write(&p, &json).with_context(|| format!("failed to write {}", p.display()))?;
    // The cookie is a bearer credential — keep the file owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// The stored session cookie pair for `server`, if a login was saved.
pub fn cookie_for(server: &str) -> Option<String> {
    load().ok()?.servers.get(&normalize(server)).cloned()
}

/// Persist `cookie` (a full `name=value` pair) for `server`, replacing any
/// previous value.
pub fn store_cookie(server: &str, cookie: &str) -> Result<()> {
    let mut store = load()?;
    store.servers.insert(normalize(server), cookie.to_string());
    save(&store)
}

/// Forget any stored cookie for `server`. Returns whether one was removed.
pub fn clear_cookie(server: &str) -> Result<bool> {
    let mut store = load()?;
    let removed = store.servers.remove(&normalize(server)).is_some();
    if removed {
        save(&store)?;
    }
    Ok(removed)
}
