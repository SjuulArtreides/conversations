//! Persisted set of open `live --host` sessions, for resume across restarts.
//!
//! `live --host` runs N concurrent `claude` children; on any exit (`Ctrl-C`,
//! `Ctrl-a q/d`, control-stream close, last child closing) the host reaps every
//! child and the set of what was open is lost. To let a later `--host` offer to
//! resume, the host checkpoints the live registry — `{session_id, cwd}` per
//! child, in deterministic order — to `open-sessions.json`, keyed by server URL.
//!
//! Stored at `$XDG_CONFIG_HOME/arrakis-conversations/open-sessions.json` (or the
//! platform equivalent via `dirs::config_dir`), next to `config.json` /
//! `credentials.json`. Like `config.json` it holds a preference, not a secret, so
//! it is written with default permissions.
//!
//! Persistence is best-effort: a write failure is logged by the caller and is
//! non-fatal; a missing / malformed file ⇒ an empty set (never blocks startup).
//! Keying by server URL keeps a host pointed at staging from resuming prod's
//! sessions (and vice-versa).

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

fn save(p: &Path, file: &OpenSessionsFile) -> Result<()> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(file)?;
    // Startup adapters read concurrently while the daemon checkpoints identities.
    // Never expose a truncated document as an empty saved-session set.
    let temporary = p.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut out = std::fs::File::create(&temporary)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        std::io::Write::write_all(&mut out, &json)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        // Flush the contents before the rename so the name never points at a
        // torn file. The rename itself is not synced; a crash may keep the
        // previous checkpoint, which `load_for_update` also tolerates.
        out.sync_all()
            .with_context(|| format!("failed to flush {}", temporary.display()))?;
        std::fs::rename(&temporary, p).with_context(|| format!("failed to replace {}", p.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// A torn document is unreadable by every CLI version. Move it aside for
/// diagnosis so new sessions can be checkpointed again. Valid JSON with an
/// unknown shape may belong to a newer CLI and is never replaced.
fn load_for_update(p: &Path) -> Result<OpenSessionsFile> {
    let bytes = match std::fs::read(p) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", p.display())),
    };
    match serde_json::from_slice(&bytes) {
        Ok(file) => Ok(file),
        Err(e) if matches!(e.classify(), serde_json::error::Category::Data) => {
            Err(e).with_context(|| format!("failed to parse {}", p.display()))
        }
        Err(e) => {
            let aside = p.with_extension(format!(
                "json.corrupt-{}",
                chrono::Utc::now().format("%Y%m%dT%H%M%S%.f")
            ));
            std::fs::rename(p, &aside)
                .with_context(|| format!("failed to move aside unreadable {}", p.display()))?;
            eprintln!(
                "Saved Conversations sessions were unreadable ({e}); moved to {} and starting a new checkpoint file",
                aside.display()
            );
            Ok(Default::default())
        }
    }
}

/// The saved open-session set for `server` (empty when none / unreadable from the
/// caller's perspective — callers treat any load error as "no saved set").
pub fn load_for(server: &str) -> Vec<OpenSession> {
    path().map(|p| load_for_in(&p, server)).unwrap_or_default()
}

/// Strict read for one-time migrations: unreadable data must remain retryable.
pub fn try_load_for(server: &str) -> Result<Vec<OpenSession>> {
    Ok(load(&path()?)?
        .servers
        .remove(&normalize(server))
        .unwrap_or_default())
}

fn load_for_in(p: &Path, server: &str) -> Vec<OpenSession> {
    let key = normalize(server);
    load(p)
        .ok()
        .and_then(|f| f.servers.get(&key).cloned())
        .unwrap_or_default()
}

/// Persist `sessions` as the open set for `server`. An empty slice removes the
/// server's entry entirely (so "I closed everything" leaves no resume prompt
/// next time, and the file shrinks rather than accumulating empty arrays).
pub fn save_for(server: &str, sessions: &[OpenSession]) -> Result<()> {
    update_for(server, |saved| *saved = sessions.to_vec())
}

/// Serialize the entire read/modify/write, across tasks and CLI processes.
/// Atomic rename protects readers; it cannot prevent lost updates on its own.
pub fn update_for(server: &str, update: impl FnOnce(&mut Vec<OpenSession>)) -> Result<()> {
    update_in(&path()?, server, update)
}

fn update_in(p: &Path, server: &str, update: impl FnOnce(&mut Vec<OpenSession>)) -> Result<()> {
    static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _writer = WRITER
        .lock()
        .map_err(|_| anyhow::anyhow!("checkpoint writer lock poisoned"))?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(p.with_extension("lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // The file remains open until the updated document is atomically saved.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).context("locking session checkpoints");
        }
    }
    let key = normalize(server);
    let mut file = load_for_update(p)?;
    let sessions = file.servers.entry(key.clone()).or_default();
    update(sessions);
    if sessions.is_empty() {
        file.servers.remove(&key);
    }
    let result = save(p, &file);
    drop(lock);
    result
}

/// Clear any saved open set for `server` (resume "No", or after a resume
/// consumes the set). Equivalent to `save_for(server, &[])`.
pub fn clear_for(server: &str) -> Result<()> {
    save_for(server, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_trailing_slash() {
        assert_eq!(normalize("https://x.dev/"), "https://x.dev");
        assert_eq!(normalize("https://x.dev"), "https://x.dev");
    }

    fn session(id: &str) -> OpenSession {
        OpenSession {
            session_id: id.into(),
            agent: AgentKind::Claude,
            cwd: "/repo".into(),
            resume_id: None,
            agent_args: Vec::new(),
            launcher: AgentLauncher::Opencode,
            adk_statusline: false,
        }
    }

    #[test]
    fn torn_checkpoint_file_does_not_block_new_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("open-sessions.json");
        let server = "https://torn.example.test";

        // Valid JSON this build cannot interpret may belong to a newer CLI:
        // refuse to overwrite it rather than discard that version's sessions.
        let newer = r#"{"servers":{"https://torn.example.test":[{"session_id":"n","agent":"future-agent","cwd":"/"}]}}"#;
        std::fs::write(&file, newer).unwrap();
        assert!(update_in(&file, server, |sessions| sessions.push(session("a"))).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), newer);

        // A torn write is unreadable by every version. Keep it for diagnosis,
        // but let the host checkpoint and restore new sessions again.
        let torn = r#"{"servers": {"https://torn.example.test": [{"session_id": ""#;
        std::fs::write(&file, torn).unwrap();
        update_in(&file, server, |sessions| sessions.push(session("b"))).unwrap();
        assert_eq!(load_for_in(&file, server), vec![session("b")]);
        let preserved: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| std::fs::read_to_string(entry.unwrap().path()).ok())
            .filter(|contents| contents == torn)
            .collect();
        assert_eq!(preserved.len(), 1, "torn checkpoint was not preserved");
    }

    #[test]
    fn saving_an_empty_set_removes_the_server_entry_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("open-sessions.json");
        update_in(&p, "https://api.example/", |s| *s = vec![session("s1")]).unwrap();
        update_in(&p, "https://other.example", |s| *s = vec![session("s2")]).unwrap();
        assert_eq!(load_for_in(&p, "https://api.example"), vec![session("s1")]);

        update_in(&p, "https://api.example", |s| s.clear()).unwrap();

        assert!(load_for_in(&p, "https://api.example").is_empty());
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            on_disk["servers"],
            serde_json::json!({ "https://other.example": [
                { "session_id": "s2", "agent": "claude", "cwd": "/repo", "resume_id": null }
            ] })
        );
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
    fn missing_file_unknown_server_and_garbage_all_load_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("open-sessions.json");
        assert!(load_for_in(&p, "https://nope").is_empty());

        update_in(&p, "https://api.example", |s| *s = vec![session("s1")]).unwrap();
        assert!(load_for_in(&p, "https://nope").is_empty());

        std::fs::write(&p, b"{not json").unwrap();
        assert!(load_for_in(&p, "https://api.example").is_empty());
    }
}
