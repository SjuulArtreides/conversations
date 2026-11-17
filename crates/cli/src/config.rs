//! Persisted CLI configuration — the user's preferred default server, so they
//! don't have to pass `--server` (or set `$CONVERSATIONS_SERVER`) on every
//! invocation.
//!
//! Stored at `$XDG_CONFIG_HOME/arrakis-conversations/config.json` (or the
//! platform equivalent via `dirs::config_dir`), next to `credentials.json`.
//! Unlike `credentials.json` this holds no secret — just a preference — so it is
//! written with default permissions. `login` offers to populate `default_server`
//! after a successful authentication; `main` reads it when resolving the
//! effective server (flag/env > saved default > built-in fallback).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    /// Server base URL used when none is given on the command line or via the
    /// `CONVERSATIONS_SERVER` env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_server: Option<String>,
}

fn path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("could not determine a config directory")?;
    Ok(dir.join("arrakis-conversations").join("config.json"))
}

/// Canonical key for a server: trailing slash trimmed (matches `ApiClient` and
/// `credentials`).
fn normalize(server: &str) -> String {
    server.trim_end_matches('/').to_string()
}

fn load() -> Result<Config> {
    let p = path()?;
    match std::fs::read(&p) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", p.display())),
    }
}

fn save(config: &Config) -> Result<()> {
    let p = path()?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(config)?;
    std::fs::write(&p, &json).with_context(|| format!("failed to write {}", p.display()))
}

/// The saved default server, if one has been set (trailing slash trimmed).
pub fn default_server() -> Option<String> {
    load().ok()?.default_server.map(|s| normalize(&s))
}

/// Persist `server` as the default for future invocations.
pub fn set_default_server(server: &str) -> Result<()> {
    let mut config = load()?;
    config.default_server = Some(normalize(server));
    save(&config)
}
