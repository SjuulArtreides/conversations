//! The open-session checkpoint file (`open-sessions.json`) that `live --host`
//! wrote, keyed by server URL. The host reads it once to import those sessions
//! as its own local records (`authz::sessions`); nothing writes it any more.
//!
//! Stored at `$XDG_CONFIG_HOME/arrakis-conversations/open-sessions.json` (or the
//! platform equivalent via `dirs::config_dir`), next to `config.json` /
//! `credentials.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use conversations_core::{AgentKind, AgentLauncher};
use serde::{Deserialize, Serialize};

/// One persisted open session: the live/session id and the working directory its
/// `claude` ran in. Resume re-opens `session_id` via `claude --resume` in `cwd`
/// (identical to the web "Re-open as live" path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenSession {
    pub session_id: String,
    #[serde(default)]
    pub agent: AgentKind,
    pub cwd: String,
    /// The agent's NATIVE resume target, when it differs from `session_id`
    /// (Codex, whose `session_id` here is the conversations-minted live id that
    /// `codex resume` rejects). `None` for Claude / legacy sets ⇒ resume by
    /// `session_id`, unchanged. Defaults to `None` so older files still parse.
    #[serde(default)]
    pub resume_id: Option<String>,
    /// Server-resolved, non-secret agent launch arguments that were used for
    /// this child (currently Codex sandbox / approval settings). Persist these
    /// so a host restart re-opens the session with the same launch policy as a
    /// clean web start or re-open. Defaults to empty for older checkpoint files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_args: Vec<String>,
    /// How this child was invoked, so a host restart re-opens an `adk code`
    /// session through the same harness instead of dropping to a bare
    /// `opencode` (which would lose the monorepo's MCP credentials and agent
    /// profile). Defaults to the agent's own binary for older checkpoint files.
    #[serde(default, skip_serializing_if = "is_default_launcher")]
    pub launcher: AgentLauncher,
    /// Whether this child was allowed to draw the `adk code` status strip.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adk_statusline: bool,
}

fn is_default_launcher(launcher: &AgentLauncher) -> bool {
    *launcher == AgentLauncher::default()
}

/// The on-disk document: open-session sets keyed by normalized server URL.
#[derive(Debug, Default, Serialize, Deserialize)]
struct OpenSessionsFile {
    /// `{ "<server>": [ {session_id, cwd}, … ] }`. A `BTreeMap` keeps the file
    /// stable (deterministic key order) across rewrites.
    #[serde(default)]
    servers: BTreeMap<String, Vec<OpenSession>>,
}

fn path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("could not determine a config directory")?;
    Ok(dir.join("arrakis-conversations").join("open-sessions.json"))
}

/// Canonical key for a server: trailing slash trimmed (matches `ApiClient`,
/// `credentials`, and `config`).
fn normalize(server: &str) -> String {
    server.trim_end_matches('/').to_string()
}

fn load(p: &Path) -> Result<OpenSessionsFile> {
    match std::fs::read(p) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(OpenSessionsFile::default()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", p.display())),
    }
}

/// Strict read for one-time migrations: unreadable data must remain retryable.
pub fn try_load_for(server: &str) -> Result<Vec<OpenSession>> {
    Ok(load(&path()?)?
        .servers
        .remove(&normalize(server))
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_trailing_slash() {
        assert_eq!(normalize("https://x.dev/"), "https://x.dev");
        assert_eq!(normalize("https://x.dev"), "https://x.dev");
    }

    #[test]
    fn roundtrips_through_json() {
        let mut file = OpenSessionsFile::default();
        file.servers.insert(
            "https://api.example".into(),
            vec![
                OpenSession {
                    session_id: "a".into(),
                    agent: AgentKind::Claude,
                    cwd: "/one".into(),
                    resume_id: None,
                    agent_args: Vec::new(),
                    launcher: AgentLauncher::Opencode,
                    adk_statusline: false,
                },
                OpenSession {
                    session_id: "b".into(),
                    agent: AgentKind::Codex,
                    cwd: "/two".into(),
                    resume_id: Some("codex-real-id".into()),
                    agent_args: vec![
                        "--sandbox".into(),
                        "danger-full-access".into(),
                        "--ask-for-approval".into(),
                        "never".into(),
                    ],
                    launcher: AgentLauncher::Adk,
                    adk_statusline: true,
                },
            ],
        );
        let json = serde_json::to_vec(&file).unwrap();
        let back: OpenSessionsFile = serde_json::from_slice(&json).unwrap();
        let got = back.servers.get("https://api.example").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].session_id, "a");
        assert_eq!(got[0].resume_id, None);
        assert_eq!(got[1].agent, AgentKind::Codex);
        assert_eq!(got[1].cwd, "/two");
        assert_eq!(got[1].resume_id.as_deref(), Some("codex-real-id"));
        assert_eq!(
            got[1].agent_args,
            [
                "--sandbox",
                "danger-full-access",
                "--ask-for-approval",
                "never"
            ]
        );
        // The launcher and its status-strip opt-in survive a restart; an entry
        // written without them still reads back as the agent's own binary.
        assert_eq!(got[0].launcher, AgentLauncher::Opencode);
        assert!(!got[0].adk_statusline);
        assert_eq!(got[1].launcher, AgentLauncher::Adk);
        assert!(got[1].adk_statusline);
        // Default launcher / no status strip stay out of the file entirely.
        let text = String::from_utf8(json).unwrap();
        assert!(!text.contains(r#""launcher":"opencode""#));
        assert!(text.contains(r#""launcher":"adk""#));
    }

    #[test]
    fn legacy_checkpoint_defaults_agent_args_to_empty() {
        let session: OpenSession = serde_json::from_str(
            r#"{"session_id":"old","agent":"codex","cwd":"/repo","resume_id":"native"}"#,
        )
        .unwrap();
        assert!(session.agent_args.is_empty());
    }

    #[test]
    fn a_missing_file_loads_as_empty_and_an_unreadable_one_stays_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("open-sessions.json");
        assert!(load(&p).unwrap().servers.is_empty());
        std::fs::write(&p, b"{not json").unwrap();
        assert!(load(&p).is_err());
    }
}
