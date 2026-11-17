/// Use the parser's normalized, bounded prompt title after excluding harness context.
pub(super) fn readable_title(text: &str) -> Option<String> {
    let text = text.trim();
    if [
        "# AGENTS.md",
        "<environment_context>",
        "<INSTRUCTIONS>",
        "<system-reminder>",
        "# CLAUDE.md",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
    {
        return None;
    }
    conversations_core::codex_user_message_title(&serde_json::json!({
        "type":"event_msg", "payload":{"type":"user_message", "message":text}
    }))
}

use std::io::Read;
use std::path::Path;

use conversations_core::{codex_user_message_title, discover_sessions_for_agent, AgentKind};

/// Read only the bound native rollout, never the newest conversation in a directory.
pub(super) fn codex_subject(root: &Path, native_id: &str) -> Option<String> {
    for reference in discover_sessions_for_agent(root, AgentKind::Codex) {
        if !reference.session_id.ends_with(&format!("-{native_id}"))
            && reference.session_id != native_id
        {
            continue;
        }
        let mut bytes = Vec::new();
        std::fs::File::open(reference.path)
            .ok()?
            .take(1024 * 1024)
            .read_to_end(&mut bytes)
            .ok()?;
        let mut matched = false;
        let mut title = None;
        // Ignore the last, possibly incomplete record of an actively written file.
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            if !line.ends_with(b"\n") {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            if value["type"] == "session_meta" {
                matched = value["payload"]["id"].as_str() == Some(native_id);
            }
            if title.is_none() {
                title = codex_user_message_title(&value);
            }
        }
        if matched {
            return title;
        }
    }
    None
}

pub(super) fn may_detect(
    server_title: Option<&str>,
    local_title: Option<&str>,
    placeholder: &str,
) -> bool {
    server_title.is_none_or(|title| title == placeholder)
        && local_title.is_none_or(|title| title == placeholder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_rollout_subject_waits_for_prompt_and_ignores_other_sessions() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("rollout-native.jsonl");
        std::fs::write(
            &target,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"native\"}}\n",
        )
        .unwrap();
        std::fs::write(root.path().join("rollout-other.jsonl"), "{\"type\":\"session_meta\",\"payload\":{\"id\":\"other\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Wrong subject\"}}\n").unwrap();
        assert_eq!(codex_subject(root.path(), "native"), None);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(target)
            .unwrap();
        writeln!(file, "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"Investigate unexpected costs\"}}}}").unwrap();
        assert_eq!(
            codex_subject(root.path(), "native").as_deref(),
            Some("Investigate unexpected costs")
        );
        assert_eq!(codex_subject(root.path(), "missing"), None);
    }

    #[test]
    fn modern_prompt_requires_complete_record_and_matching_header() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("rollout-native.jsonl");
        let prompt = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"Prepare an analysis of unexpected costs"}]}}}"#;
        let header = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"native\"}}\n";
        std::fs::write(&target, format!("{header}{prompt}")).unwrap();
        assert_eq!(codex_subject(root.path(), "native"), None);
        std::fs::write(&target, format!("{header}{prompt}\n")).unwrap();
        assert_eq!(
            codex_subject(root.path(), "native").as_deref(),
            Some("Prepare an analysis of unexpected costs")
        );
        std::fs::write(
            &target,
            format!("{}{prompt}\n", header.replace("native", "other")),
        )
        .unwrap();
        assert_eq!(codex_subject(root.path(), "native"), None);
    }

    #[test]
    fn manual_web_or_workspace_subject_prevents_detection() {
        assert!(may_detect(
            Some("Local: codex"),
            Some("Local: codex"),
            "Local: codex"
        ));
        assert!(!may_detect(
            Some("My subject"),
            Some("Local: codex"),
            "Local: codex"
        ));
        assert!(!may_detect(
            Some("Local: codex"),
            Some("My workspace"),
            "Local: codex"
        ));
    }
}
