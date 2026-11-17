//! Native Claude channel transport. A successful notification write is not a receipt.
use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use conversations_core::messaging::AgentMessage;
use reqwest::Method;
use rmcp::{
    model::{CustomNotification, ServerNotification},
    service::Peer,
    RoleServer,
};
use serde_json::{json, Value};

use super::messaging::MessagingClient;

pub(crate) struct ChannelGuard(tokio::task::JoinHandle<()>);
impl Drop for ChannelGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Persist intent before sending. Claude offers no notification receipts or dedup key,
/// so an interrupted send remains uncertain and must be recovered through the inbox.
pub(crate) struct Ledger(PathBuf);
impl Ledger {
    pub(crate) fn open(client: &MessagingClient) -> Result<Self> {
        // Dedicated override keeps process tests out of personal delivery state.
        let root = match std::env::var_os("CONVERSATIONS_CLAUDE_DELIVERY_STATE_DIR") {
            Some(path) => PathBuf::from(path),
            None => dirs::data_local_dir()
                .context("cannot locate personal data directory")?
                .join("arrakis-conversations")
                .join("claude-delivery"),
        };
        private_directory(&root)?;
        let encoded = URL_SAFE_NO_PAD.encode(client.session_url(client.session_id())?);
        let mut path = root;
        for chunk in encoded.as_bytes().chunks(120) {
            path.push(std::str::from_utf8(chunk).expect("base64 ASCII"));
            private_directory(&path)?;
        }
        Ok(Self(path))
    }
    pub(crate) fn reserve(&self, id: &str) -> Result<bool> {
        let path = self.0.join(URL_SAFE_NO_PAD.encode(id));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(mut file) => {
                file.write_all(b"delivery attempted; receipt unconfirmed\n")?;
                file.sync_all()?;
                std::fs::File::open(&self.0)?.sync_all()?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
    pub(crate) fn release(&self, id: &str) -> Result<()> {
        std::fs::remove_file(self.0.join(URL_SAFE_NO_PAD.encode(id)))?;
        std::fs::File::open(&self.0)?.sync_all()?;
        Ok(())
    }
}

fn private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn start(client: MessagingClient, peer: Peer<RoleServer>) -> Result<ChannelGuard> {
    let ledger = Ledger::open(&client)?;
    eprintln!("Conversations Claude channel enabled locally; native preview confirmation and organization policy must also allow it. Transport writes are unconfirmed until messages_receive acknowledges the message ID.");
    Ok(ChannelGuard(tokio::spawn(async move {
        let mut warned = HashSet::new();
        loop {
            let result = client
                .request(Method::POST, &["inbox"], Some(json!({"wait_seconds": 25})))
                .await;
            if let Ok(body) = result {
                if let Ok(messages) =
                    serde_json::from_value::<Vec<AgentMessage>>(body["messages"].clone())
                {
                    for message in messages {
                        if message.recipient_session_id != client.session_id() {
                            continue;
                        }
                        // The sender's authority, re-checked right before the hand-off.
                        if !client.still_deliverable(&message.id).unwrap_or(false) {
                            continue;
                        }
                        match ledger.reserve(&message.id) {
                            Ok(true) => {
                                warned.insert(message.id.clone());
                                let notification = CustomNotification::new(
                                    "notifications/claude/channel",
                                    Some(json!({
                                        "content": super::delivery::envelope(&message),
                                        "meta": {"message_id": message.id, "sender_session_id": message.sender_session_id,
                                            "sender_email": message.sender_owner_email.unwrap_or_default(), "project_id": message.project_id}
                                    })),
                                );
                                if peer
                                    .send_notification(ServerNotification::CustomNotification(
                                        notification,
                                    ))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Ok(false) => {
                                if warned.insert(message.id.clone()) {
                                    eprintln!("Conversations message {} remains pending after an unconfirmed Claude notification attempt. Use messages_receive to recover it; it has not been acknowledged or automatically re-emitted.", message.id);
                                }
                            }
                            Err(error) => {
                                eprintln!("Claude channel delivery paused: {error}");
                                return;
                            }
                        }
                    }
                }
            }
            // Some servers return immediately when all pending messages were already
            // emitted. Bound retries without repeating uncertain notifications.
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })))
}

/// Enable only an explicitly registered personal channel. The native interactive
/// confirmation and organization policy remain in control of actual admission.
pub(crate) fn prepare(command: &str, args: &mut Vec<String>) -> Result<()> {
    if Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        != Some("claude")
        || args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "-p" | "--print" | "--bare" | "--safe-mode" | "--strict-mcp-config"
            ) || arg.starts_with("--print=")
        })
    {
        return Ok(());
    }
    let Some(home) = dirs::home_dir() else {
        return Ok(());
    };
    let path = home.join(".claude.json");
    let config = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("Conversations optional Claude channel setup skipped: could not parse {}: {error}", path.display());
                return Ok(());
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            eprintln!(
                "Conversations optional Claude channel setup skipped: could not read {}: {error}",
                path.display()
            );
            return Ok(());
        }
    };
    let registration = &config["mcpServers"]["conversations"];
    // Registrations made before the executable was renamed still name a
    // working `arrakis-conversations` install.
    let registered = registration["command"].as_str().is_some_and(|command| {
        matches!(
            Path::new(command)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("conversations" | "arrakis-conversations")
        )
    }) && registration["args"].as_array().is_some_and(|args| {
        args.windows(2)
            .any(|pair| pair[0] == "mcp" && pair[1] == "serve")
            && args
                .windows(2)
                .any(|pair| pair[0] == "--delivery" && pair[1] == "claude")
    });
    if registered && !args.iter().any(|arg| arg == "server:conversations") {
        let position = args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(args.len());
        args.splice(
            position..position,
            [
                "--dangerously-load-development-channels".into(),
                "server:conversations".into(),
            ],
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_attempt_survives_restart_without_claiming_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = Ledger(directory.path().into());
        assert!(ledger.reserve("message-one").unwrap());
        assert!(!ledger.reserve("message-one").unwrap());
        drop(ledger);
        assert!(!Ledger(directory.path().into())
            .reserve("message-one")
            .unwrap());
        assert!(Ledger(directory.path().into())
            .reserve("message-two")
            .unwrap());
    }
}
