//! Explicit fallback through guarded Conversations terminal input.
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use conversations_core::messaging::AgentMessage;
use reqwest::Method;
use serde_json::{json, Value};

use super::{claude_channel::Ledger, messaging::MessagingClient};

pub(crate) struct ReplyGuard(tokio::task::JoinHandle<()>);
impl Drop for ReplyGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn reply_text(message: &AgentMessage) -> Result<String> {
    let text = format!("{} | Receipt: after consuming this message, call messages_receive with acknowledge containing message ID {}. Acknowledge receipt, not task completion.", super::delivery::envelope(message).replace('\n', " "), message.id);
    let printable = text
        .chars()
        .filter(|&c| c >= ' ' && c != '\u{7f}' && !('\u{80}'..='\u{9f}').contains(&c))
        .count();
    // The existing CLI reply sanitizer caps input at 4096 printable characters.
    ensure!(printable <= 4096, "message envelope exceeds Conversations live-reply limit of 4096 printable characters; message remains pending");
    Ok(text
        .chars()
        .filter(|&c| c >= ' ' && c != '\u{7f}' && !('\u{80}'..='\u{9f}').contains(&c))
        .collect())
}

fn eligible_replies(
    messages: Vec<AgentMessage>,
    warned: &mut std::collections::HashSet<String>,
) -> Vec<(AgentMessage, String)> {
    messages
        .into_iter()
        .filter_map(|message| match reply_text(&message) {
            Ok(text) => Some((message, text)),
            Err(error) => {
                if warned.insert(message.id.clone()) {
                    eprintln!(
                        "Conversations fallback message {} remains pending: {error}",
                        message.id
                    );
                }
                None
            }
        })
        .collect()
}

fn transcript_path(native: &str) -> Result<Option<PathBuf>> {
    uuid::Uuid::parse_str(native).context("Claude fallback requires an exact native UUID")?;
    let Some(root) = conversations_core::claude_projects_dir() else {
        return Ok(None);
    };
    for project in std::fs::read_dir(root)? {
        let project = project?;
        if project.file_type()?.is_dir() {
            let path = project.path().join(format!("{native}.jsonl"));
            if path.is_file() {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

/// Claude persists completed-turn markers after assistant output. Never type
/// while a tool or approval may be outstanding. This does not expose the TUI's
/// unsubmitted composer; the wrapper separately requires observed clean input.
fn transcript_idle(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let offset = length.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let data = String::from_utf8_lossy(&bytes);
    let mut ended = false;
    let mut idle = false;
    for line in data.lines().skip(usize::from(offset > 0)) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value["type"].as_str() {
            Some("user" | "queue-operation") => {
                ended = false;
                idle = false;
            }
            Some("assistant") => {
                ended = value["message"]["stop_reason"] == "end_turn";
                idle = false;
            }
            Some("system")
                if matches!(
                    value["subtype"].as_str(),
                    Some("stop_hook_summary" | "turn_duration")
                ) =>
            {
                idle = ended;
            }
            _ => {}
        }
    }
    Ok(idle)
}

pub(crate) fn claude_turn(native: &str) -> Result<Option<(u64, std::time::SystemTime)>> {
    let Some(path) = transcript_path(native)? else {
        return Ok(None);
    };
    let before = std::fs::metadata(&path)?;
    if !transcript_idle(&path)? {
        return Ok(None);
    }
    let after = std::fs::metadata(path)?;
    if before.len() != after.len() || before.modified()? != after.modified()? {
        return Ok(None);
    }
    Ok(Some((after.len(), after.modified()?)))
}

#[cfg(unix)]
async fn guarded_input(
    runtime: &Path,
    source: &str,
    native: &str,
    message_id: &str,
) -> Result<bool> {
    super::local_host::peer_input(runtime, source, native, message_id).await
}
#[cfg(not(unix))]
async fn guarded_input(
    _runtime: &Path,
    _source: &str,
    _native: &str,
    _message_id: &str,
) -> Result<bool> {
    bail!("guarded Conversations fallback requires a Unix local host")
}

pub(crate) fn start(client: MessagingClient) -> Result<ReplyGuard> {
    ensure!(
        cfg!(unix),
        "guarded Conversations fallback requires a Unix local host"
    );
    let runtime = std::env::var_os("CONVERSATIONS_HOST_DIR").map(PathBuf::from).context("guarded fallback requires a Conversations host wrapper; restart this terminal inside the updated host")?;
    let ledger = Ledger::open(&client)?;
    eprintln!("Conversations reply fallback explicitly enabled. Delivery waits for a completed Claude turn; guarded local input also defers unknown or unsent human drafts. Transport writes remain unconfirmed until messages_receive acknowledges receipt.");
    Ok(ReplyGuard(tokio::spawn(async move {
        let mut warned = std::collections::HashSet::new();
        let mut deferred = std::collections::HashSet::new();
        let mut oversized = std::collections::HashSet::new();
        let mut logged_error = None;
        loop {
            let result: Result<()> = async {
                let metadata = client.session_metadata().await?;
                ensure!(metadata["agent"] == "claude", "Conversations reply fallback currently requires a Claude session with completed-turn admission checks");
                let native = client.native_session_id().await?;
                let Some(path) = transcript_path(&native)? else { bail!("waiting for this Claude session's exact transcript"); };
                let body = client.request(Method::POST, &["inbox"], Some(json!({"wait_seconds": 25}))).await?;
                let messages: Vec<AgentMessage> = serde_json::from_value(body["messages"].clone())?;
                ensure!(messages.iter().all(|message| message.recipient_session_id == client.session_id()), "inbox recipient differs from bound session");
                for (message, _text) in eligible_replies(messages, &mut oversized) {
                    if !transcript_idle(&path)? { break; }
                    if !ledger.reserve(&message.id)? {
                        if warned.insert(message.id.clone()) {
                            eprintln!("Conversations fallback message {} has an unconfirmed previous input attempt; it remains pending in messages_receive and will not be replayed blindly.", message.id);
                        }
                        continue;
                    }
                    match guarded_input(&runtime, client.session_id(), &native, &message.id).await {
                        Ok(false) => {
                            ledger.release(&message.id)?;
                            if deferred.insert(message.id.clone()) { eprintln!("Conversations fallback message {} paused until its updated host/wrapper has a known empty composer and completed Claude turn. It remains pending and will retry without input changes.", message.id); }
                        }
                        Ok(true) => { deferred.remove(&message.id); warned.insert(message.id); }
                        Err(error) => { return Err(error.context("guarded-input delivery is uncertain; intent retained without acknowledgment or replay")); }
                    }
                    // One admission per completed turn. A successful write may not yet
                    // have reached Claude; never concatenate a second pending prompt.
                    break;
                }
                Ok(())
            }.await;
            if let Err(error) = result {
                let error = error.to_string();
                if logged_error.as_ref() != Some(&error) {
                    eprintln!("Conversations fallback waiting: {error}");
                    logged_error = Some(error);
                }
            } else {
                logged_error = None;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn idle_after(records: &[Value]) -> bool {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.jsonl");
        std::fs::write(
            &path,
            records
                .iter()
                .map(|value| format!("{value}\n"))
                .collect::<String>(),
        )
        .unwrap();
        transcript_idle(&path).unwrap()
    }
    #[test]
    fn deferred_through_busy_tool_and_approval_then_admitted_only_after_turn_completion() {
        let user = json!({"type":"user","message":{"content":"work"}});
        let tool = json!({"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use"}]}});
        let end = json!({"type":"assistant","message":{"stop_reason":"end_turn"}});
        let stop = json!({"type":"system","subtype":"turn_duration"});
        assert!(!idle_after(std::slice::from_ref(&user)));
        assert!(!idle_after(&[user.clone(), tool]));
        assert!(!idle_after(&[user.clone(), end.clone()]));
        assert!(idle_after(&[user.clone(), end.clone(), stop.clone()]));
        assert!(!idle_after(&[
            user.clone(),
            end.clone(),
            stop.clone(),
            user
        ]));
        assert!(!idle_after(&[
            end,
            stop,
            json!({"type":"queue-operation","operation":"enqueue"})
        ]));
    }
    #[test]
    fn oversized_head_does_not_block_a_later_deliverable_message() {
        let small: AgentMessage = serde_json::from_value(json!({"id":"small","project_id":"project","sender_session_id":"peer","sender_owner_email":null,"recipient_session_id":"source","text":"hello","reply_to":null,"idempotency_key":"small","created_at":"now","received_at":null,"acknowledged_at":null,"status":"queued"})).unwrap();
        let mut oversized = small.clone();
        oversized.id = "oversized".into();
        oversized.text = "x".repeat(4096);
        let mut warnings = std::collections::HashSet::new();
        let batch = eligible_replies(vec![oversized.clone(), small.clone()], &mut warnings);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].0.id, "small");
        assert!(batch[0].1.contains("Receipt:"));
        assert_eq!(
            warnings,
            std::collections::HashSet::from(["oversized".to_string()])
        );
        assert_eq!(
            eligible_replies(vec![oversized, small], &mut warnings).len(),
            1
        );
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn refuses_oversized_envelope_without_truncating_message_identity() {
        let mut message: AgentMessage = serde_json::from_value(json!({"id":"message-id","project_id":"project","sender_session_id":"peer","sender_owner_email":null,"recipient_session_id":"source","text":"hello","reply_to":null,"idempotency_key":"test","created_at":"now","received_at":null,"acknowledged_at":null,"status":"queued"})).unwrap();
        assert!(reply_text(&message).unwrap().contains("message-id"));
        message.text = "x".repeat(4096);
        assert!(reply_text(&message)
            .unwrap_err()
            .to_string()
            .contains("remains pending"));
    }
}
