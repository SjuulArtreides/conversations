//! Pure extraction of filesystem paths a session touched.
//!
//! Used by the CLI to *suggest* which repositories + branches a conversation
//! worked on — deliberately looking beyond the session `cwd`, since a session
//! frequently edits files in repos other than its launch directory. Resolving
//! these paths to git repos (and branches / PRs) needs filesystem + git access
//! and so lives in the CLI; this function is the pure, testable first step.

use crate::model::{ContentBlock, Session};

/// Tool-input keys whose string values are filesystem paths worth considering.
/// `file_path` (Edit/Write/Read/MultiEdit), `notebook_path` (NotebookEdit),
/// `path` (Glob/Grep search roots), `workdir` (Codex/OpenCode tool cwd), and
/// OpenCode's camel-case `filePath` / `projectPath` inputs.
const PATH_KEYS: [&str; 6] = [
    "file_path",
    "notebook_path",
    "filePath",
    "projectPath",
    "path",
    "workdir",
];

/// Collect distinct filesystem paths referenced by a session, in stable order:
/// the session `cwd` first (when present), then every path-like value found in
/// `tool_use` block inputs, de-duplicated preserving first-seen order.
pub fn touched_paths(session: &Session) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    let mut push = |p: &str| {
        let p = p.trim();
        if p.is_empty() {
            return;
        }
        if seen.insert(p.to_string()) {
            out.push(p.to_string());
        }
    };

    if let Some(cwd) = session.cwd.as_deref() {
        push(cwd);
    }

    for msg in &session.messages {
        for block in &msg.blocks {
            let ContentBlock::ToolUse { input, .. } = block else {
                continue;
            };
            let Some(obj) = input.as_object() else {
                continue;
            };
            for key in PATH_KEYS {
                if let Some(val) = obj.get(key).and_then(|v| v.as_str()) {
                    push(val);
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, MessageKind, Role};
    use serde_json::json;

    fn tool_use(name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: "t".into(),
            name: name.into(),
            input,
        }
    }

    fn msg_with(blocks: Vec<ContentBlock>) -> Message {
        Message {
            uuid: "u".into(),
            parent_uuid: None,
            role: Role::Assistant,
            kind: MessageKind::ToolUse,
            model: None,
            is_sidechain: false,
            timestamp: None,
            blocks,
            text: String::new(),
            raw: json!({}),
            usage: None,
        }
    }

    fn session_with(cwd: Option<&str>, messages: Vec<Message>) -> Session {
        Session {
            id: "s".into(),
            agent: crate::model::AgentKind::Claude,
            title: "t".into(),
            project_path: None,
            cwd: cwd.map(str::to_string),
            git_branch: None,
            claude_version: None,
            started_at: None,
            ended_at: None,
            messages,
            skipped_lines: 0,
        }
    }

    #[test]
    fn collects_cwd_first_then_tool_paths_deduped() {
        let s = session_with(
            Some("/home/dev/monorepo"),
            vec![
                msg_with(vec![tool_use(
                    "Edit",
                    json!({"file_path": "/home/dev/repo-a/src/x.rs"}),
                )]),
                msg_with(vec![tool_use(
                    "Read",
                    json!({"file_path": "/home/dev/repo-b/y.ts"}),
                )]),
                // duplicate path collapses
                msg_with(vec![tool_use(
                    "Write",
                    json!({"file_path": "/home/dev/repo-a/src/x.rs"}),
                )]),
                // grep search root via `path`
                msg_with(vec![tool_use("Grep", json!({"path": "/home/dev/repo-c"}))]),
                // Codex tool-call cwd.
                msg_with(vec![tool_use(
                    "exec_command",
                    json!({"workdir": "/home/dev/repo-e"}),
                )]),
                // OpenCode uses camel-case path fields.
                msg_with(vec![tool_use(
                    "read",
                    json!({"filePath": "/home/dev/repo-f/src/lib.rs"}),
                )]),
                // notebook
                msg_with(vec![tool_use(
                    "NotebookEdit",
                    json!({"notebook_path": "/home/dev/repo-d/nb.ipynb"}),
                )]),
            ],
        );
        let paths = touched_paths(&s);
        assert_eq!(
            paths,
            vec![
                "/home/dev/monorepo".to_string(),
                "/home/dev/repo-a/src/x.rs".to_string(),
                "/home/dev/repo-b/y.ts".to_string(),
                "/home/dev/repo-c".to_string(),
                "/home/dev/repo-e".to_string(),
                "/home/dev/repo-f/src/lib.rs".to_string(),
                "/home/dev/repo-d/nb.ipynb".to_string(),
            ]
        );
    }

    #[test]
    fn ignores_non_path_tools_and_empty() {
        let s = session_with(
            None,
            vec![msg_with(vec![
                tool_use("Bash", json!({"command": "ls -la"})),
                tool_use("Edit", json!({"file_path": "   "})),
            ])],
        );
        assert!(touched_paths(&s).is_empty());
    }
}
