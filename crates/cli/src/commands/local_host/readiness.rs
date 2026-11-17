//! Native lifecycle evidence stays on the host. Terminal silence is not completion.
use std::collections::BTreeSet;
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use conversations_core::private_channel::AgentReadiness;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::protocol::Launch;

#[derive(Default, Serialize, Deserialize)]
struct ClaudeState {
    started: bool,
    stopped: bool,
    needs_input: bool,
    snapshot_known: bool,
    background: bool,
    children: BTreeSet<String>,
    blocked_children: BTreeSet<String>,
}

impl ClaudeState {
    fn observe(&mut self, event: &Value) {
        let hook = event["hook_event_name"].as_str().unwrap_or("");
        // Tool hooks also fire inside children. Their progress cannot restart
        // a parent turn that has already stopped.
        let child = event["agent_id"].as_str().filter(|id| id.len() <= 256);
        if !matches!(hook, "SubagentStart" | "SubagentStop") {
            if let Some(child) = child {
                match hook {
                    "PermissionRequest" => {
                        self.blocked_children.insert(child.to_owned());
                    }
                    "PreToolUse" if event["tool_name"] == "AskUserQuestion" => {
                        self.blocked_children.insert(child.to_owned());
                    }
                    "PostToolUse" | "PostToolUseFailure" | "UserPromptSubmit" => {
                        self.blocked_children.remove(child);
                    }
                    _ => {}
                }
                return;
            }
        }
        match hook {
            "SessionStart" | "SessionEnd" | "StopFailure" => *self = Self::default(),
            "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" => {
                self.started = true;
                self.stopped = false;
                self.needs_input = false;
            }
            "PreToolUse" => {
                self.started = true;
                self.stopped = false;
                self.needs_input = event["tool_name"] == "AskUserQuestion";
            }
            "PermissionRequest" => self.needs_input = true,
            "Notification" => {
                if matches!(
                    event["notification_type"].as_str(),
                    Some("permission_prompt" | "elicitation_dialog")
                ) {
                    self.needs_input = true;
                }
            }
            "SubagentStart" => {
                if let Some(id) = event["agent_id"].as_str().filter(|id| id.len() <= 256) {
                    self.children.insert(id.to_owned());
                }
            }
            "Stop" | "SubagentStop" => {
                if event["hook_event_name"] == "Stop" {
                    self.stopped = true;
                } else if let Some(id) = event["agent_id"].as_str() {
                    self.children.remove(id);
                    self.blocked_children.remove(id);
                }
                // Missing arrays mean an older agent or an unavailable registry.
                // Neither proves that background work has finished.
                let tasks = event["background_tasks"].as_array();
                let crons = event["session_crons"].as_array();
                self.snapshot_known = tasks.is_some() && crons.is_some();
                self.background = tasks.is_some_and(|tasks| !tasks.is_empty())
                    || crons.is_some_and(|crons| !crons.is_empty());
            }
            _ => {}
        }
    }

    fn readiness(&self) -> AgentReadiness {
        if self.needs_input || !self.blocked_children.is_empty() {
            AgentReadiness::NeedsInput
        } else if self.started && !self.stopped {
            AgentReadiness::Working
        } else if !self.stopped {
            AgentReadiness::Unknown
        } else if self.background || !self.children.is_empty() {
            AgentReadiness::Background
        } else if self.snapshot_known {
            AgentReadiness::Ready
        } else {
            AgentReadiness::Unknown
        }
    }
}

fn path(dir: &Path, id: &str, pid: u32) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id)?;
    Ok(dir.join("readiness").join(format!("{id}-{pid}.json")))
}

fn open_state(dir: &Path, id: &str, pid: u32) -> Result<std::fs::File> {
    let path = path(dir, id, pid)?;
    let parent = path.parent().unwrap();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(e)
            }
        })?;
    let metadata = std::fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "readiness directory must be owner-only"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
        "cannot lock lifecycle evidence"
    );
    Ok(file)
}

pub(super) fn claude_hook(dir: &Path, id: &str, pid: u32, input: impl Read) -> Result<()> {
    let mut file = open_state(dir, id, pid)?;
    let event = (|| -> Result<Value> {
        let mut bytes = Vec::new();
        input.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1024 * 1024, "lifecycle event too large");
        let event: Value = serde_json::from_slice(&bytes).context("invalid lifecycle event")?;
        ensure!(
            event["hook_event_name"]
                .as_str()
                .is_some_and(|hook| matches!(
                    hook,
                    "SessionStart"
                        | "SessionEnd"
                        | "StopFailure"
                        | "UserPromptSubmit"
                        | "PreToolUse"
                        | "PostToolUse"
                        | "PostToolUseFailure"
                        | "PermissionRequest"
                        | "Notification"
                        | "SubagentStart"
                        | "SubagentStop"
                        | "Stop"
                )),
            "invalid lifecycle event kind"
        );
        Ok(event)
    })();
    let event = match event {
        Ok(event) => event,
        Err(error) => {
            file.set_len(0)?;
            file.write_all(&serde_json::to_vec(&ClaudeState::default())?)?;
            return Err(error);
        }
    };
    let mut state = Vec::new();
    Read::by_ref(&mut file)
        .take(16 * 1024)
        .read_to_end(&mut state)?;
    let mut state: ClaudeState = serde_json::from_slice(&state).unwrap_or_default();
    state.observe(&event);
    file.rewind()?;
    file.set_len(0)?;
    file.write_all(&serde_json::to_vec(&state)?)?;
    Ok(())
}

pub(super) fn claude(dir: &Path, id: &str, pid: u32) -> Result<AgentReadiness> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path(dir, id, pid)?)?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0,
        "cannot lock lifecycle evidence"
    );
    let state: ClaudeState = serde_json::from_reader(file.take(16 * 1024))?;
    Ok(state.readiness())
}

fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Merge launch-only hooks with an explicit settings file or object. Native user
/// and project settings retain their normal precedence and hook composition.
pub(super) fn prepare_claude(dir: &Path, launch: &Launch, args: &mut Vec<String>) -> Result<()> {
    if Path::new(&launch.command[0])
        .file_name()
        .and_then(|n| n.to_str())
        != Some("claude")
        || args.iter().any(|arg| {
            matches!(arg.as_str(), "-p" | "--print" | "--bare" | "--safe-mode")
                || arg.starts_with("--print=")
        })
    {
        return Ok(());
    }
    let pid = std::process::id();
    let mut file = open_state(dir, &launch.id, pid)?;
    file.set_len(0)?;
    file.write_all(&serde_json::to_vec(&ClaudeState::default())?)?;
    drop(file);
    let position = args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .enumerate()
        .filter(|(_, arg)| arg.as_str() == "--settings" || arg.starts_with("--settings="))
        .map(|(index, _)| index)
        .last();
    let mut settings = if let Some(index) = position {
        let value = args[index]
            .strip_prefix("--settings=")
            .or_else(|| args.get(index + 1).map(String::as_str))
            .context("--settings needs a value")?;
        let bytes = if value.trim_start().starts_with('{') {
            value.as_bytes().to_vec()
        } else {
            let file = Path::new(value);
            std::fs::read(if file.is_absolute() {
                file.to_path_buf()
            } else {
                launch.cwd.join(file)
            })?
        };
        serde_json::from_slice::<Value>(&bytes).context("Claude settings must be JSON")?
    } else {
        json!({})
    };
    let command = format!(
        "{} host --runtime-dir {} agent-hook --launch-id {} --wrapper-pid {}",
        quote(&std::env::current_exe()?.to_string_lossy()),
        quote(&dir.to_string_lossy()),
        quote(&launch.id),
        pid
    );
    let hooks = settings
        .as_object_mut()
        .context("Claude settings must be an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Claude hooks must be an object")?;
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "PermissionRequest",
        "Notification",
        "Stop",
        "StopFailure",
        "SubagentStart",
        "SubagentStop",
        "SessionEnd",
    ] {
        hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("Claude hooks must be arrays")?
            .push(json!({"hooks":[{"type":"command","command":command,"timeout":5}]}));
    }
    let settings_path = path(dir, &launch.id, pid)?.with_extension("settings.json");
    let mut settings_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&settings_path)?;
    settings_file.write_all(settings.to_string().as_bytes())?;
    let settings_arg = settings_path.to_string_lossy().into_owned();
    if let Some(index) = position {
        if args[index] == "--settings" {
            args.splice(
                index..index + 2,
                ["--settings".into(), settings_arg.clone()],
            );
        } else {
            args[index] = format!("--settings={settings_arg}");
        }
    } else {
        let index = args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(args.len());
        args.splice(index..index, ["--settings".into(), settings_arg.clone()]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_parent_stays_working_and_children_cannot_complete_it() {
        let mut state = ClaudeState::default();
        assert_eq!(state.readiness(), AgentReadiness::Unknown);
        state.observe(&json!({"hook_event_name":"UserPromptSubmit"}));
        assert_eq!(state.readiness(), AgentReadiness::Working);
        state.observe(&json!({"hook_event_name":"SubagentStart","agent_id":"child"}));
        state.observe(&json!({"hook_event_name":"Stop","background_tasks":[{"type":"subagent","id":"child"}],"session_crons":[]}));
        assert_eq!(state.readiness(), AgentReadiness::Background);
        state.observe(&json!({"hook_event_name":"PostToolUse","agent_id":"child"}));
        assert_eq!(state.readiness(), AgentReadiness::Background);
        state.observe(&json!({"hook_event_name":"SubagentStop","agent_id":"child","background_tasks":[],"session_crons":[]}));
        assert_eq!(state.readiness(), AgentReadiness::Ready);
    }

    #[test]
    fn only_an_explicit_empty_registry_proves_completion() {
        for pending in [json!([{"type":"shell"}]), json!([{"type":"monitor"}])] {
            let mut state = ClaudeState::default();
            state.observe(
                &json!({"hook_event_name":"Stop","background_tasks":pending,"session_crons":[]}),
            );
            assert_eq!(state.readiness(), AgentReadiness::Background);
        }
        let mut state = ClaudeState::default();
        for event in [
            json!({"hook_event_name":"Stop"}),
            json!({"hook_event_name":"Stop","background_tasks":[]}),
            json!({"hook_event_name":"StopFailure"}),
        ] {
            state.observe(&event);
            assert_eq!(state.readiness(), AgentReadiness::Unknown);
        }
        state.observe(&json!({"hook_event_name":"PermissionRequest"}));
        state.observe(&json!({"hook_event_name":"Stop","background_tasks":[],"session_crons":[]}));
        assert_eq!(state.readiness(), AgentReadiness::NeedsInput);
        state.observe(&json!({"hook_event_name":"UserPromptSubmit"}));
        assert_eq!(state.readiness(), AgentReadiness::Working);
    }

    #[test]
    fn malformed_or_oversized_hooks_invalidate_a_previous_ready_state() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        for invalid in [
            b"not-json".to_vec(),
            b"{}".to_vec(),
            vec![b' '; 1024 * 1024 + 1],
        ] {
            let stop = json!({"hook_event_name":"Stop","background_tasks":[],"session_crons":[]});
            claude_hook(dir.path(), &id, 42, stop.to_string().as_bytes()).unwrap();
            assert_eq!(claude(dir.path(), &id, 42).unwrap(), AgentReadiness::Ready);
            assert!(claude_hook(dir.path(), &id, 42, invalid.as_slice()).is_err());
            assert_eq!(
                claude(dir.path(), &id, 42).unwrap(),
                AgentReadiness::Unknown
            );
        }
    }

    #[test]
    fn hook_callback_discards_payload_and_binds_to_wrapper_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let pid = 1234;
        let payload = json!({"hook_event_name":"UserPromptSubmit","prompt":"sensitive prompt must not be retained"});
        claude_hook(dir.path(), &id, pid, payload.to_string().as_bytes()).unwrap();
        assert_eq!(
            claude(dir.path(), &id, pid).unwrap(),
            AgentReadiness::Working
        );
        assert!(claude(dir.path(), &id, pid + 1).is_err());
        let stored = std::fs::read_to_string(path(dir.path(), &id, pid).unwrap()).unwrap();
        assert!(!stored.contains("sensitive"));
        let launch = Launch {
            id: id.clone(),
            agent: conversations_core::AgentKind::Claude,
            cwd: dir.path().into(),
            command: vec!["claude".into()],
        };
        let mut args = vec!["--settings".into(), json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"user-hook"}]}]},"model":"chosen","env":{"PRIVATE_KEY":"never-copy-to-argv"}}).to_string()];
        prepare_claude(dir.path(), &launch, &mut args).unwrap();
        let settings: Value = serde_json::from_slice(&std::fs::read(&args[1]).unwrap()).unwrap();
        assert_eq!(settings["model"], "chosen");
        assert_eq!(settings["env"]["PRIVATE_KEY"], "never-copy-to-argv");
        assert!(args.iter().all(|arg| !arg.contains("never-copy-to-argv")));
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert!(settings["hooks"]["Stop"][1]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("agent-hook --launch-id"));
        assert_eq!(
            claude(dir.path(), &id, std::process::id()).unwrap(),
            AgentReadiness::Unknown
        );
    }
}
