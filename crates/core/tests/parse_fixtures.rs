//! Integration tests against the on-disk fixtures.

use std::path::Path;

use conversations_core::{
    parse_block, parse_opencode_export_str, parse_session_file, parse_session_str, quick_summary,
    AgentKind, ContentBlock, MessageKind, Role,
};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn synthetic_session_parses_fully() {
    let path = fixture("synthetic_session.jsonl");
    let session = parse_session_file(&path).unwrap();

    // Title comes from ai-title.
    assert_eq!(session.title, "Fix the flaky deploy script");
    // Session id pulled from records.
    assert_eq!(session.id, "sess-001");
    assert_eq!(session.agent, AgentKind::Claude);
    // Only user + assistant turns counted.
    assert_eq!(session.message_count(), 5);
    // Unknown type + metadata records did not break parsing.
    assert_eq!(session.skipped_lines, 0);
    // Session-level metadata captured.
    assert_eq!(session.cwd.as_deref(), Some("/Users/dev/proj"));
    assert_eq!(session.git_branch.as_deref(), Some("main"));
    assert_eq!(session.claude_version.as_deref(), Some("2.1.165"));

    // Block variants land in the right enum arms.
    let a1 = session.messages.iter().find(|m| m.uuid == "a-1").unwrap();
    assert_eq!(a1.role, Role::Assistant);
    assert_eq!(a1.kind, MessageKind::Mixed);
    assert!(matches!(a1.blocks[0], ContentBlock::Thinking { .. }));
    assert!(matches!(a1.blocks[2], ContentBlock::ToolUse { .. }));

    let u2 = session.messages.iter().find(|m| m.uuid == "u-2").unwrap();
    assert_eq!(u2.kind, MessageKind::ToolResult);
    assert!(matches!(u2.blocks[0], ContentBlock::ToolResult { .. }));

    // Sidechain marker preserved.
    let side = session
        .messages
        .iter()
        .find(|m| m.uuid == "a-3-side")
        .unwrap();
    assert!(side.is_sidechain);
}

#[test]
fn codex_session_parses_fully() {
    let path = fixture("codex_session.jsonl");
    let session = parse_session_file(&path).unwrap();

    assert_eq!(session.id, "codex-sess-001");
    assert_eq!(session.agent, AgentKind::Codex);
    assert_eq!(session.cwd.as_deref(), Some("/Users/dev/proj"));
    assert_eq!(session.git_branch.as_deref(), Some("feat/codex"));
    assert_eq!(session.claude_version.as_deref(), Some("0.141.0"));
    assert_eq!(session.message_count(), 5);
    assert_eq!(session.title, "Add Codex support");

    let tool = session
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::ToolUse)
        .unwrap();
    match &tool.blocks[0] {
        ContentBlock::ToolUse { name, input, .. } => {
            assert_eq!(name, "exec_command");
            assert_eq!(input["cmd"], "rg codex");
        }
        other => panic!("expected codex tool use, got {other:?}"),
    }

    let result = session
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::ToolResult)
        .unwrap();
    assert!(result.text.contains("parser.rs"));
}

#[test]
fn opencode_export_parses_observable_conversation() {
    let contents = std::fs::read_to_string(fixture("opencode_session.json")).unwrap();
    let session = parse_opencode_export_str(&contents).unwrap();

    assert_eq!(session.id, "ses_opencode_001");
    assert_eq!(session.agent, AgentKind::OpenCode);
    assert_eq!(session.title, "Add OpenCode support");
    assert_eq!(session.cwd.as_deref(), Some("/Users/dev/proj"));
    assert_eq!(session.claude_version.as_deref(), Some("1.18.18"));
    assert_eq!(session.message_count(), 2);
    assert_eq!(session.skipped_lines, 1);

    let assistant = &session.messages[1];
    assert_eq!(assistant.parent_uuid.as_deref(), Some("msg_user_001"));
    assert_eq!(assistant.model.as_deref(), Some("gpt-5.6-terra"));
    assert_eq!(assistant.kind, MessageKind::Mixed);
    assert!(assistant
        .blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Thinking { .. })));
    assert!(assistant.blocks.iter().any(|block| matches!(
        block,
        ContentBlock::ToolUse { name, input, .. }
            if name == "bash" && input["command"] == "rg AgentKind"
    )));
    assert!(assistant.blocks.iter().any(|block| matches!(
        block,
        ContentBlock::ToolResult { content, .. }
            if content.contains("model.rs")
    )));
    assert!(assistant.blocks.iter().any(|block| matches!(
        block,
        ContentBlock::Other(value)
            if value.get("type").and_then(serde_json::Value::as_str)
                == Some("some-future-part")
    )));
    assert_eq!(assistant.usage.as_ref().unwrap().input_tokens, Some(1200));
    assert_eq!(
        assistant.usage.as_ref().unwrap().cache_read_input_tokens,
        Some(800)
    );
}

#[test]
fn synthetic_tolerates_injected_garbage() {
    let contents = std::fs::read_to_string(fixture("synthetic_session.jsonl")).unwrap();
    let mutated = format!("{contents}\nnot json\n");
    let session = parse_session_str(Some("sess-001"), &mutated).unwrap();
    assert_eq!(session.skipped_lines, 1);
    assert_eq!(session.message_count(), 5);
}

#[test]
fn metadata_session_parses_without_error() {
    let path = fixture("metadata_session.jsonl");
    let session = parse_session_file(&path).unwrap();
    // Synthetic metadata and slash-command records have no ai-title.
    assert!(!session.id.is_empty());
    // It contains user turns (slash-command output), so count is non-zero.
    assert!(session.message_count() > 0);
    // Whatever its content, malformed-line count must be sane (these are valid
    // JSON lines, so zero).
    assert_eq!(session.skipped_lines, 0);
}

#[test]
fn quick_summary_matches_full_parse_basics() {
    let path = fixture("synthetic_session.jsonl");
    let full = parse_session_file(&path).unwrap();
    let summary = quick_summary(&path).unwrap();

    assert_eq!(summary.id, full.id);
    assert_eq!(summary.agent, full.agent);
    assert_eq!(summary.title, full.title);
    assert_eq!(summary.message_count, full.message_count());
    assert_eq!(summary.git_branch, full.git_branch);
    assert_eq!(summary.started_at, full.started_at);
    assert_eq!(summary.ended_at, full.ended_at);
}

#[test]
fn codex_quick_summary_matches_full_parse_basics() {
    let path = fixture("codex_session.jsonl");
    let full = parse_session_file(&path).unwrap();
    let summary = quick_summary(&path).unwrap();

    assert_eq!(summary.id, full.id);
    assert_eq!(summary.agent, AgentKind::Codex);
    assert_eq!(summary.title, full.title);
    assert_eq!(summary.message_count, full.message_count());
    assert_eq!(summary.git_branch, full.git_branch);
}

#[test]
fn parse_block_accepts_codex_text_variants() {
    // Codex emits `input_text` / `output_text` blocks; both (and plain `text`)
    // decode to a Text block. This is the public entrypoint live ingestion uses.
    for ty in ["text", "input_text", "output_text"] {
        let block = serde_json::json!({ "type": ty, "text": "hello" });
        match parse_block(&block) {
            ContentBlock::Text { text } => assert_eq!(text, "hello", "variant {ty}"),
            other => panic!("expected Text for {ty}, got {other:?}"),
        }
    }
    // An unknown block type is preserved verbatim as Other.
    let img = serde_json::json!({ "type": "image", "url": "x" });
    assert!(matches!(parse_block(&img), ContentBlock::Other(_)));
}
