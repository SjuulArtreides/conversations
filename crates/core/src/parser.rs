//! Parser for Claude Code's on-disk JSONL session format.
//!
//! Robustness is the headline invariant: a whole session must never fail to
//! parse because of (a) an unknown record `type` — captured/ignored — or (b) a
//! single malformed JSON line — skipped and counted in
//! [`Session::skipped_lines`]. No panics on real-world input.

use std::fs;
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use thiserror::Error;

use crate::model::{
    AgentKind, ContentBlock, Message, MessageKind, Role, Session, SessionSummary, Usage,
};

/// Maximum length (in chars) of a title derived from the first user prompt.
const TITLE_MAX_CHARS: usize = 120;

/// Errors that are *hard* failures. Bad individual lines and unknown record
/// types are NOT errors — they are tolerated by the parser.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("io error reading session: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not infer session id from path: {0}")]
    MissingSessionId(String),
    #[error("invalid session data: {0}")]
    InvalidFormat(String),
}

/// A single decoded line of the session file. Only `user` / `assistant` carry
/// chat content; everything else is metadata we either capture or ignore.
enum Record {
    /// A chat turn (user or assistant). Boxed to keep the enum small.
    Message(Box<Message>),
    /// An `ai-title` record carrying the human-readable title.
    AiTitle(String),
    /// Any other known-or-unknown metadata record — ignored for display.
    Ignored,
}

/// Parse a whole JSONL session from an in-memory string.
///
/// `id_hint` is used as the session id when records do not carry a `sessionId`
/// (e.g. an all-metadata or empty file). Per-line JSON errors are tolerated and
/// counted in [`Session::skipped_lines`]; this only returns `Err` for the
/// (currently unreachable from a string) hard-error cases.
pub fn parse_session_str(id_hint: Option<&str>, contents: &str) -> Result<Session, ParseError> {
    if detect_agent(contents) == AgentKind::Codex {
        return parse_codex_session_str(id_hint, contents);
    }

    let mut messages: Vec<Message> = Vec::new();
    let mut ai_title: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut claude_version: Option<String> = None;
    let mut skipped_lines = 0usize;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                // Malformed line: tolerate and count it.
                skipped_lines += 1;
                continue;
            }
        };

        // Pull session-level metadata from any record that carries it.
        if session_id.is_none() {
            if let Some(s) = value.get("sessionId").and_then(Value::as_str) {
                session_id = Some(s.to_string());
            }
        }
        if cwd.is_none() {
            if let Some(s) = value.get("cwd").and_then(Value::as_str) {
                cwd = Some(s.to_string());
            }
        }
        if git_branch.is_none() {
            if let Some(s) = value.get("gitBranch").and_then(Value::as_str) {
                git_branch = Some(s.to_string());
            }
        }
        if claude_version.is_none() {
            if let Some(s) = value.get("version").and_then(Value::as_str) {
                claude_version = Some(s.to_string());
            }
        }

        match parse_record(&value) {
            Record::Message(m) => messages.push(*m),
            Record::AiTitle(t) => ai_title = Some(t),
            Record::Ignored => {}
        }
    }

    let id = session_id
        .or_else(|| id_hint.map(str::to_string))
        .unwrap_or_default();

    let (started_at, ended_at) = time_range(&messages);

    let title = resolve_title(&ai_title, &messages, &id);

    Ok(Session {
        id,
        agent: AgentKind::Claude,
        title,
        project_path: cwd.clone(),
        cwd,
        git_branch,
        claude_version,
        started_at,
        ended_at,
        messages,
        skipped_lines,
    })
}

/// Parse a session file from disk, inferring the session id from the file stem.
pub fn parse_session_file(path: &Path) -> Result<Session, ParseError> {
    let contents = fs::read_to_string(path)?;
    let id_hint = path.file_stem().and_then(|s| s.to_str());
    parse_session_str(id_hint, &contents)
}

/// Read a file and produce just a [`SessionSummary`] without building full
/// [`Message`] objects (no block parsing, no raw JSON retention). Returns
/// `None` if the file cannot be read.
pub fn quick_summary(path: &Path) -> Option<SessionSummary> {
    let contents = fs::read_to_string(path).ok()?;
    let id_hint = path.file_stem().and_then(|s| s.to_str());

    if detect_agent(&contents) == AgentKind::Codex {
        let session = parse_codex_session_str(id_hint, &contents).ok()?;
        let message_count = session.message_count();
        return Some(SessionSummary {
            id: session.id,
            agent: AgentKind::Codex,
            title: session.title,
            project_path: session.project_path,
            git_branch: session.git_branch,
            message_count,
            started_at: session.started_at,
            ended_at: session.ended_at,
        });
    }

    let mut ai_title: Option<String> = None;
    let mut first_user_text: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut message_count = 0usize;
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut latest: Option<DateTime<Utc>> = None;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        if session_id.is_none() {
            if let Some(s) = value.get("sessionId").and_then(Value::as_str) {
                session_id = Some(s.to_string());
            }
        }
        if git_branch.is_none() {
            if let Some(s) = value.get("gitBranch").and_then(Value::as_str) {
                git_branch = Some(s.to_string());
            }
        }

        match value.get("type").and_then(Value::as_str) {
            Some("ai-title") => {
                if let Some(t) = value.get("aiTitle").and_then(Value::as_str) {
                    ai_title = Some(t.to_string());
                }
            }
            Some(role @ ("user" | "assistant")) => {
                message_count += 1;
                if role == "user" && first_user_text.is_none() {
                    if let Some(msg) = value.get("message") {
                        let text = render_message_text_from_json(msg);
                        if !text.is_empty() {
                            first_user_text = Some(text);
                        }
                    }
                }
                if let Some(ts) = parse_timestamp(&value) {
                    earliest = Some(earliest.map_or(ts, |e| e.min(ts)));
                    latest = Some(latest.map_or(ts, |l| l.max(ts)));
                }
            }
            _ => {}
        }
    }

    let id = session_id
        .or_else(|| id_hint.map(str::to_string))
        .unwrap_or_default();

    let title = if let Some(t) = ai_title {
        t
    } else if let Some(t) = first_user_text {
        truncate_title(&t)
    } else {
        id.clone()
    };

    Some(SessionSummary {
        id,
        agent: AgentKind::Claude,
        title,
        project_path: None,
        git_branch,
        message_count,
        started_at: earliest,
        ended_at: latest,
    })
}

/// Parse a single already-decoded JSON line into a [`Message`], when it is a
/// `user` / `assistant` chat turn. Any other record type (metadata, titles,
/// summaries, unknown future types) yields `None`.
///
/// This is the incremental counterpart to [`parse_session_str`]: the live-session
/// CLI tailer decodes one appended JSONL line at a time and calls this to obtain
/// the same [`Message`] the full parser would have produced for that line, reusing
/// all block handling and unknown-type tolerance.
pub fn parse_line(value: &Value) -> Option<Message> {
    if is_codex_record(value) {
        return parse_codex_message(value, None);
    }

    match parse_record(value) {
        Record::Message(m) => Some(*m),
        _ => None,
    }
}

fn detect_agent(contents: &str) -> AgentKind {
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if is_codex_record(&value) {
            return AgentKind::Codex;
        }
        if value.get("type").and_then(Value::as_str).is_some() {
            return AgentKind::Claude;
        }
    }
    AgentKind::Claude
}

fn is_codex_record(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("session_meta" | "turn_context" | "response_item" | "event_msg")
    ) && value.get("payload").is_some()
}

fn parse_codex_session_str(id_hint: Option<&str>, contents: &str) -> Result<Session, ParseError> {
    let mut messages: Vec<Message> = Vec::new();
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut cli_version: Option<String> = None;
    let mut first_ts: Option<DateTime<Utc>> = None;
    let mut current_model: Option<String> = None;
    let mut user_message_title: Option<String> = None;
    let mut skipped_lines = 0usize;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                skipped_lines += 1;
                continue;
            }
        };

        if first_ts.is_none() {
            first_ts = parse_timestamp(&value)
                .or_else(|| value.get("payload").and_then(parse_codex_payload_timestamp));
        }

        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if let Some(payload) = value.get("payload") {
                    if session_id.is_none() {
                        session_id = payload
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    if cwd.is_none() {
                        cwd = payload
                            .get("cwd")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    if cli_version.is_none() {
                        cli_version = payload
                            .get("cli_version")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    if git_branch.is_none() {
                        git_branch = payload
                            .get("git")
                            .and_then(|g| g.get("branch"))
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                }
            }
            Some("turn_context") => {
                if let Some(model) = value
                    .get("payload")
                    .and_then(|p| p.get("model"))
                    .and_then(Value::as_str)
                {
                    current_model = Some(model.to_string());
                }
                if cwd.is_none() {
                    cwd = value
                        .get("payload")
                        .and_then(|p| p.get("cwd"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            _ => {}
        }

        if user_message_title.is_none() {
            user_message_title = codex_user_message_title(&value);
        }

        if let Some(message) = parse_codex_message(&value, current_model.as_deref()) {
            messages.push(message);
        }
    }

    let id = session_id
        .or_else(|| id_hint.map(str::to_string))
        .unwrap_or_default();
    let (mut started_at, ended_at) = time_range(&messages);
    if started_at.is_none() {
        started_at = first_ts;
    }
    // Modern Codex rollouts persist injected developer/AGENTS.md context as
    // user-role response items before the human prompt. `event_msg.user_message`
    // is the canonical copy of what the human actually typed, so prefer it for
    // the title while retaining the response-item fallback for older rollouts.
    let title = user_message_title.unwrap_or_else(|| resolve_title(&None, &messages, &id));

    Ok(Session {
        id,
        agent: AgentKind::Codex,
        title,
        project_path: cwd.clone(),
        cwd,
        git_branch,
        claude_version: cli_version,
        started_at,
        ended_at,
        messages,
        skipped_lines,
    })
}

fn parse_codex_payload_timestamp(payload: &Value) -> Option<DateTime<Utc>> {
    let s = payload.get("timestamp").and_then(Value::as_str)?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Return a display title from a Codex human-message event.
///
/// Codex versions through 0.146 use `event_msg.user_message`. Codex 0.147 uses
/// `event_msg.item_completed` with a `UserMessage` item. Both events contain
/// only human input, unlike user-role `response_item` records that can contain
/// injected AGENTS.md context.
pub fn codex_user_message_title(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    let message = match payload.get("type").and_then(Value::as_str) {
        Some("user_message") => payload.get("message").and_then(Value::as_str)?.to_string(),
        Some("item_completed") => {
            let item = payload.get("item")?;
            if item.get("type").and_then(Value::as_str) != Some("UserMessage") {
                return None;
            }
            item.get("content")?
                .as_array()?
                .iter()
                .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                    Some("text" | "input_text") => block.get("text").and_then(Value::as_str),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => return None,
    };
    if message.trim().is_empty() {
        return None;
    }
    Some(truncate_title(&message))
}

fn parse_codex_message(value: &Value, current_model: Option<&str>) -> Option<Message> {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = value.get("payload")?;
    let payload_type = payload.get("type").and_then(Value::as_str)?;
    let timestamp = parse_timestamp(value);
    let uuid = stable_codex_uuid(payload_type, value);

    match payload_type {
        "message" => {
            let role_str = payload.get("role").and_then(Value::as_str)?;
            let role = match role_str {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => return None,
            };
            let blocks = blocks_from_raw_message(payload);
            if blocks.is_empty() {
                return None;
            }
            let text = render_blocks_text(&blocks);
            Some(Message {
                uuid,
                parent_uuid: None,
                role,
                kind: Message::derive_kind(&blocks),
                model: (role == Role::Assistant)
                    .then(|| current_model.map(str::to_string))
                    .flatten(),
                is_sidechain: false,
                timestamp,
                blocks,
                text,
                raw: payload.clone(),
                usage: None,
            })
        }
        "function_call" | "custom_tool_call" => {
            let blocks = blocks_from_raw_message(payload);
            let text = render_blocks_text(&blocks);
            Some(Message {
                uuid,
                parent_uuid: None,
                role: Role::Assistant,
                kind: MessageKind::ToolUse,
                model: current_model.map(str::to_string),
                is_sidechain: false,
                timestamp,
                blocks,
                text,
                raw: payload.clone(),
                usage: None,
            })
        }
        "function_call_output" | "custom_tool_call_output" => {
            let blocks = blocks_from_raw_message(payload);
            let text = render_blocks_text(&blocks);
            Some(Message {
                uuid,
                parent_uuid: None,
                role: Role::User,
                kind: MessageKind::ToolResult,
                model: None,
                is_sidechain: false,
                timestamp,
                blocks,
                text,
                raw: payload.clone(),
                usage: None,
            })
        }
        "reasoning" => {
            let text = codex_reasoning_summary(payload.get("summary"));
            if text.trim().is_empty() {
                return None;
            }
            let blocks = vec![ContentBlock::Thinking { thinking: text }];
            let text = render_blocks_text(&blocks);
            Some(Message {
                uuid,
                parent_uuid: None,
                role: Role::Assistant,
                kind: MessageKind::Thinking,
                model: current_model.map(str::to_string),
                is_sidechain: false,
                timestamp,
                blocks,
                text,
                raw: payload.clone(),
                usage: None,
            })
        }
        _ => None,
    }
}

/// Reconstruct normalized blocks from a stored raw message payload.
///
/// Completed-session storage keeps normalized columns plus the original raw
/// payload, rather than serializing `blocks` separately. Keep this decoder in
/// core so initial Codex parsing and every storage backend use identical rules.
pub fn blocks_from_raw_message(raw: &Value) -> Vec<ContentBlock> {
    match raw.get("type").and_then(Value::as_str) {
        Some(payload_type @ ("function_call" | "custom_tool_call")) => {
            let name = raw
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let call_id = raw
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input = if payload_type == "custom_tool_call" {
                // Custom tools (notably apply_patch) carry an opaque string.
                raw.get("input").cloned().unwrap_or(Value::Null)
            } else {
                codex_arguments(raw.get("arguments"))
            };
            vec![ContentBlock::ToolUse {
                id: call_id,
                name,
                input,
            }]
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            vec![ContentBlock::ToolResult {
                tool_use_id: raw
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                content: codex_output_to_string(raw.get("output")),
            }]
        }
        _ => parse_content(raw.get("content")),
    }
}

fn codex_arguments(arguments: Option<&Value>) -> Value {
    match arguments {
        Some(Value::String(s)) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
        }
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

fn codex_output_to_string(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

fn codex_reasoning_summary(summary: Option<&Value>) -> String {
    match summary {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("summary_text"))
                    .and_then(Value::as_str)
            })
            .map(str::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn stable_codex_uuid(prefix: &str, value: &Value) -> String {
    let serialized = serde_json::to_string(value).unwrap_or_default();
    let mut hash = 0xcbf29ce484222325u64;
    for byte in serialized.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("codex-{prefix}-{hash:016x}")
}

/// Parse OpenCode's public `opencode export <session-id>` JSON shape.
///
/// OpenCode owns a SQLite database rather than one transcript file per
/// session. The CLI reads that database through OpenCode's read-only `db`
/// command, reconstructs this same export shape, and hands it to this pure
/// parser. Keeping the normalization here means uploads and fixtures exercise
/// one contract without adding SQLite or process dependencies to core.
pub fn parse_opencode_export_str(contents: &str) -> Result<Session, ParseError> {
    let value: Value = serde_json::from_str(contents)
        .map_err(|error| ParseError::InvalidFormat(error.to_string()))?;
    parse_opencode_export(&value)
}

/// Normalize an already-decoded OpenCode export object.
pub fn parse_opencode_export(value: &Value) -> Result<Session, ParseError> {
    let info = value
        .get("info")
        .and_then(Value::as_object)
        .ok_or_else(|| ParseError::InvalidFormat("missing object field `info`".to_string()))?;
    let id = info
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ParseError::InvalidFormat("missing session id".to_string()))?
        .to_string();

    let cwd = info
        .get("directory")
        .and_then(Value::as_str)
        .map(str::to_string);
    let started_at = info
        .get("time")
        .and_then(|time| time.get("created"))
        .and_then(opencode_timestamp);
    let updated_at = info
        .get("time")
        .and_then(|time| time.get("updated"))
        .and_then(opencode_timestamp);

    let mut messages = Vec::new();
    let mut skipped_lines = 0;
    if let Some(items) = value.get("messages").and_then(Value::as_array) {
        for item in items {
            match parse_opencode_message(item) {
                Some(message) => messages.push(message),
                None => skipped_lines += 1,
            }
        }
    }

    let title = info
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| resolve_title(&None, &messages, &id));
    let (message_start, message_end) = time_range(&messages);

    Ok(Session {
        id,
        agent: AgentKind::OpenCode,
        title,
        project_path: cwd.clone(),
        cwd,
        git_branch: None,
        // This field is the producing CLI version despite its legacy name.
        claude_version: info
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        started_at: started_at.or(message_start),
        ended_at: updated_at.or(message_end),
        messages,
        skipped_lines,
    })
}

fn parse_opencode_message(value: &Value) -> Option<Message> {
    let info = value.get("info")?;
    let role = match info.get("role").and_then(Value::as_str)? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let uuid = info.get("id")?.as_str()?.to_string();
    let parent_uuid = info
        .get("parentID")
        .and_then(Value::as_str)
        .map(str::to_string);
    let timestamp = info
        .get("time")
        .and_then(|time| time.get("created"))
        .and_then(opencode_timestamp);
    let model = info
        .get("modelID")
        .or_else(|| info.get("model").and_then(|model| model.get("modelID")))
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut blocks = Vec::new();
    for part in value
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text {
                            text: text.to_string(),
                        });
                    }
                }
            }
            Some("reasoning") => {
                if let Some(thinking) = part.get("text").and_then(Value::as_str) {
                    if !thinking.is_empty() {
                        blocks.push(ContentBlock::Thinking {
                            thinking: thinking.to_string(),
                        });
                    }
                }
            }
            Some("tool") => parse_opencode_tool_part(part, &mut blocks),
            // These are execution bookkeeping, not conversational content.
            Some("step-start" | "step-finish" | "snapshot") => {}
            // Preserve future/user-visible part types without making them a
            // hard parser failure, matching the JSONL parser invariant.
            Some(_) => blocks.push(ContentBlock::Other(part.clone())),
            None => {}
        }
    }
    if blocks.is_empty() {
        return None;
    }

    let usage = (role == Role::Assistant)
        .then(|| info.get("tokens").and_then(parse_opencode_usage))
        .flatten();
    let text = render_blocks_text(&blocks);
    Some(Message {
        uuid,
        parent_uuid,
        role,
        kind: Message::derive_kind(&blocks),
        model,
        is_sidechain: false,
        timestamp,
        blocks,
        text,
        raw: value.clone(),
        usage,
    })
}

fn parse_opencode_tool_part(part: &Value, blocks: &mut Vec<ContentBlock>) {
    let call_id = part
        .get("callID")
        .or_else(|| part.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let name = part
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let state = part.get("state").unwrap_or(&Value::Null);
    blocks.push(ContentBlock::ToolUse {
        id: call_id.clone(),
        name,
        input: state.get("input").cloned().unwrap_or(Value::Null),
    });

    let result = state
        .get("output")
        .and_then(Value::as_str)
        .or_else(|| state.get("error").and_then(Value::as_str));
    if let Some(content) = result {
        blocks.push(ContentBlock::ToolResult {
            tool_use_id: (!call_id.is_empty()).then_some(call_id),
            content: content.to_string(),
        });
    }
}

fn parse_opencode_usage(tokens: &Value) -> Option<Usage> {
    let u = Usage {
        input_tokens: tokens.get("input").and_then(Value::as_u64),
        output_tokens: tokens.get("output").and_then(Value::as_u64),
        cache_read_input_tokens: tokens
            .get("cache")
            .and_then(|cache| cache.get("read"))
            .and_then(Value::as_u64),
        cache_creation_input_tokens: tokens
            .get("cache")
            .and_then(|cache| cache.get("write"))
            .and_then(Value::as_u64),
    };
    (!u.is_empty()).then_some(u)
}

fn opencode_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    let millis = value.as_i64()?;
    Utc.timestamp_millis_opt(millis).single()
}

/// Decode one already-parsed JSON line into a [`Record`].
fn parse_record(value: &Value) -> Record {
    let ty = value.get("type").and_then(Value::as_str);
    match ty {
        Some("ai-title") => match value.get("aiTitle").and_then(Value::as_str) {
            Some(t) => Record::AiTitle(t.to_string()),
            None => Record::Ignored,
        },
        Some(role @ ("user" | "assistant")) => match parse_message(role, value) {
            Some(m) => Record::Message(Box::new(m)),
            None => Record::Ignored,
        },
        // attachment, mode, permission-mode, last-prompt, file-history-snapshot,
        // and any unknown future type — all ignored, never an error.
        _ => Record::Ignored,
    }
}

/// Build a [`Message`] from a `user` / `assistant` record. Returns `None` if
/// the record lacks a `message` object (then it is treated as ignorable).
fn parse_message(role_str: &str, value: &Value) -> Option<Message> {
    let message = value.get("message")?;

    let role = match role_str {
        "assistant" => Role::Assistant,
        _ => Role::User,
    };

    let uuid = value
        .get("uuid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let parent_uuid = value
        .get("parentUuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let is_sidechain = value
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let timestamp = parse_timestamp(value);

    // Model lives on the message for assistant turns.
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);

    let usage = message.get("usage").and_then(parse_usage);

    let blocks = parse_content(message.get("content"));
    let text = render_blocks_text(&blocks);
    let kind = Message::derive_kind(&blocks);

    Some(Message {
        uuid,
        parent_uuid,
        role,
        kind,
        model,
        is_sidechain,
        timestamp,
        blocks,
        text,
        raw: message.clone(),
        usage,
    })
}

/// Parse a `message.content` value into content blocks. Content is either a
/// bare string (a user prompt) or an array of typed blocks.
fn parse_content(content: Option<&Value>) -> Vec<ContentBlock> {
    match content {
        Some(Value::String(s)) => vec![ContentBlock::Text { text: s.clone() }],
        Some(Value::Array(arr)) => arr.iter().map(parse_block).collect(),
        _ => Vec::new(),
    }
}

/// Parse one block object. Unknown block types become [`ContentBlock::Other`].
///
/// Shared with `ContentBlock`'s `Deserialize` impl so a block read off the wire
/// is interpreted exactly as one parsed from a raw transcript.
pub fn parse_block(block: &Value) -> ContentBlock {
    match block.get("type").and_then(Value::as_str) {
        Some("text" | "input_text" | "output_text") => ContentBlock::Text {
            text: block
                .get("text")
                .or_else(|| block.get("input_text"))
                .or_else(|| block.get("output_text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        Some("thinking") => ContentBlock::Thinking {
            thinking: block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        Some("tool_use") => ContentBlock::ToolUse {
            id: block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input: block.get("input").cloned().unwrap_or(Value::Null),
        },
        Some("tool_result") => ContentBlock::ToolResult {
            tool_use_id: block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            content: tool_result_content_to_string(block.get("content")),
        },
        _ => ContentBlock::Other(block.clone()),
    }
}

/// A tool_result `content` may be a string or an array of `{type:text,text}`
/// blocks. Flatten to a single string.
fn tool_result_content_to_string(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                } else if let Some(s) = item.as_str() {
                    parts.push(s.to_string());
                }
            }
            parts.join("\n")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Render the searchable/previewable plain text of a message from its blocks.
fn render_blocks_text(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => parts.push(text.clone()),
            ContentBlock::Thinking { thinking } => parts.push(thinking.clone()),
            ContentBlock::ToolResult { content, .. } => parts.push(content.clone()),
            ContentBlock::ToolUse { name, .. } => parts.push(format!("[tool_use: {name}]")),
            ContentBlock::Other(_) => {}
        }
    }
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lightweight text render from a raw `message` JSON object (used by
/// [`quick_summary`] to avoid full block parsing).
fn render_message_text_from_json(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for block in arr {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// Parse `usage` into a [`Usage`], returning `None` when no fields are present.
fn parse_usage(usage: &Value) -> Option<Usage> {
    let field = |k: &str| usage.get(k).and_then(Value::as_u64);
    let u = Usage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_read_input_tokens: field("cache_read_input_tokens"),
        cache_creation_input_tokens: field("cache_creation_input_tokens"),
    };
    if u.is_empty() {
        None
    } else {
        Some(u)
    }
}

/// Parse the RFC3339 `timestamp` field into a UTC datetime.
fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    let s = value.get("timestamp").and_then(Value::as_str)?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Compute the (earliest, latest) timestamp across messages.
fn time_range(messages: &[Message]) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut latest: Option<DateTime<Utc>> = None;
    for m in messages {
        if let Some(ts) = m.timestamp {
            earliest = Some(earliest.map_or(ts, |e| e.min(ts)));
            latest = Some(latest.map_or(ts, |l| l.max(ts)));
        }
    }
    (earliest, latest)
}

/// Resolve the session title: `ai-title` wins; else the first user message's
/// text (truncated, single-lined); else the session id.
fn resolve_title(ai_title: &Option<String>, messages: &[Message], id: &str) -> String {
    if let Some(t) = ai_title {
        if !t.trim().is_empty() {
            return t.clone();
        }
    }
    for m in messages {
        if m.role == Role::User && !m.text.trim().is_empty() {
            return truncate_title(&m.text);
        }
    }
    id.to_string()
}

/// Collapse whitespace to a single line and truncate to [`TITLE_MAX_CHARS`]
/// chars (adding an ellipsis when cut).
fn truncate_title(text: &str) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= TITLE_MAX_CHARS {
        return single_line;
    }
    let truncated: String = single_line.chars().take(TITLE_MAX_CHARS).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MessageKind;

    const SYNTHETIC: &str = include_str!("../tests/fixtures/synthetic_session.jsonl");

    #[test]
    fn title_prefers_ai_title() {
        let s = parse_session_str(Some("fallback"), SYNTHETIC).unwrap();
        assert_eq!(s.title, "Fix the flaky deploy script");
    }

    #[test]
    fn message_count_only_chat_turns() {
        let s = parse_session_str(None, SYNTHETIC).unwrap();
        // u-1, a-1, u-2, a-2, a-3-side = 5 chat turns; attachment, mode,
        // permission-mode, file-history-snapshot, unknown, last-prompt excluded.
        assert_eq!(s.message_count(), 5);
    }

    #[test]
    fn unknown_type_does_not_error() {
        // The fixture contains a "some-future-unknown-type" line.
        let s = parse_session_str(None, SYNTHETIC).unwrap();
        assert_eq!(s.skipped_lines, 0);
    }

    #[test]
    fn malformed_line_is_skipped_and_counted() {
        let mut content = SYNTHETIC.to_string();
        content.push_str("\nnot json at all\n");
        content.push_str("{ \"type\": \"user\", broken json\n");
        let s = parse_session_str(None, &content).unwrap();
        assert_eq!(s.skipped_lines, 2);
        // Valid messages still parsed.
        assert_eq!(s.message_count(), 5);
    }

    #[test]
    fn single_garbage_line_tolerated() {
        let content = format!("not json\n{SYNTHETIC}");
        let s = parse_session_str(None, &content).unwrap();
        assert_eq!(s.skipped_lines, 1);
        assert_eq!(s.message_count(), 5);
    }

    #[test]
    fn block_variants_parse() {
        let s = parse_session_str(None, SYNTHETIC).unwrap();
        let a1 = s.messages.iter().find(|m| m.uuid == "a-1").unwrap();
        // thinking + text + tool_use => Mixed
        assert_eq!(a1.kind, MessageKind::Mixed);
        assert_eq!(a1.role, Role::Assistant);
        assert_eq!(a1.model.as_deref(), Some("claude-opus-4-8"));
        assert!(matches!(a1.blocks[0], ContentBlock::Thinking { .. }));
        assert!(matches!(a1.blocks[1], ContentBlock::Text { .. }));
        match &a1.blocks[2] {
            ContentBlock::ToolUse { name, id, .. } => {
                assert_eq!(name, "Bash");
                assert_eq!(id, "tool-1");
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        // usage captured
        let usage = a1.usage.as_ref().unwrap();
        assert_eq!(usage.input_tokens, Some(1200));
        assert_eq!(usage.output_tokens, Some(340));
    }

    #[test]
    fn tool_result_block_parses() {
        let s = parse_session_str(None, SYNTHETIC).unwrap();
        let u2 = s.messages.iter().find(|m| m.uuid == "u-2").unwrap();
        assert_eq!(u2.kind, MessageKind::ToolResult);
        match &u2.blocks[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => {
                assert_eq!(tool_use_id.as_deref(), Some("tool-1"));
                assert!(content.contains("for i in 1 2 3"));
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn assistant_text_render_concatenates() {
        let s = parse_session_str(None, SYNTHETIC).unwrap();
        let a2 = s.messages.iter().find(|m| m.uuid == "a-2").unwrap();
        assert_eq!(a2.kind, MessageKind::Text);
        assert!(a2.text.contains("Found it"));
    }

    #[test]
    fn sidechain_flag_parsed() {
        let s = parse_session_str(None, SYNTHETIC).unwrap();
        let side = s.messages.iter().find(|m| m.uuid == "a-3-side").unwrap();
        assert!(side.is_sidechain);
        let main = s.messages.iter().find(|m| m.uuid == "a-1").unwrap();
        assert!(!main.is_sidechain);
    }

    #[test]
    fn time_range_computed() {
        let s = parse_session_str(None, SYNTHETIC).unwrap();
        assert!(s.started_at.is_some());
        assert!(s.ended_at.is_some());
        assert!(s.started_at <= s.ended_at);
    }

    #[test]
    fn title_truncation_single_line() {
        let long = "a ".repeat(200);
        let line = format!(
            "{{\"type\":\"user\",\"uuid\":\"x\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}",
            long.trim()
        );
        let s = parse_session_str(Some("id"), &line).unwrap();
        assert!(s.title.chars().count() <= TITLE_MAX_CHARS + 1);
        assert!(s.title.ends_with('…'));
        assert!(!s.title.contains('\n'));
    }

    #[test]
    fn parse_line_matches_full_parser_for_a_chat_line() {
        // A single assistant line parsed incrementally must equal the message the
        // full parser produces for the same line.
        let line = r#"{"type":"assistant","uuid":"a-x","parentUuid":"u-x","timestamp":"2026-06-05T10:00:02.000Z","sessionId":"s","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"hi back"}]}}"#;
        let value: Value = serde_json::from_str(line).unwrap();
        let via_line = parse_line(&value).expect("chat line yields a message");

        let session = parse_session_str(Some("s"), line).unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(via_line, session.messages[0]);
        assert_eq!(via_line.uuid, "a-x");
        assert_eq!(via_line.role, Role::Assistant);

        // A non-chat record (metadata / title) yields None.
        let title: Value =
            serde_json::from_str(r#"{"type":"ai-title","aiTitle":"x","sessionId":"s"}"#).unwrap();
        assert!(parse_line(&title).is_none());
    }

    #[test]
    fn empty_input_falls_back_to_id_hint() {
        let s = parse_session_str(Some("the-id"), "").unwrap();
        assert_eq!(s.id, "the-id");
        assert_eq!(s.title, "the-id");
        assert_eq!(s.message_count(), 0);
    }

    // --- Codex -------------------------------------------------------------

    const CODEX: &str = include_str!("../tests/fixtures/codex_session.jsonl");

    #[test]
    fn detect_agent_classifies_sources() {
        assert_eq!(detect_agent(SYNTHETIC), AgentKind::Claude);
        assert_eq!(detect_agent(CODEX), AgentKind::Codex);
        // Nothing to go on → default Claude.
        assert_eq!(detect_agent(""), AgentKind::Claude);
        // Only unparseable / typeless lines → default Claude.
        assert_eq!(
            detect_agent("not json\n{\"no\":\"type\"}\n"),
            AgentKind::Claude
        );
        // Leading blank + garbage lines before the first Codex record still
        // resolve to Codex.
        let padded = format!("\n   \nnot json\n{CODEX}");
        assert_eq!(detect_agent(&padded), AgentKind::Codex);
        // A Claude metadata line first (has a `type`, but not a Codex one) wins
        // for Claude — the first typed line decides.
        assert_eq!(
            detect_agent("{\"type\":\"summary\",\"summary\":\"x\"}\n"),
            AgentKind::Claude
        );
    }

    #[test]
    fn is_codex_record_requires_a_payload() {
        assert!(is_codex_record(
            &serde_json::json!({"type":"session_meta","payload":{"id":"x"}})
        ));
        // A Codex type with no payload is ambiguous — not treated as Codex.
        assert!(!is_codex_record(
            &serde_json::json!({"type":"session_meta"})
        ));
        // Claude record types are never Codex.
        assert!(!is_codex_record(
            &serde_json::json!({"type":"user","message":{}})
        ));
        // Typeless object is never Codex.
        assert!(!is_codex_record(&serde_json::json!({"payload":{}})));
    }

    #[test]
    fn stable_codex_uuid_is_deterministic_and_input_sensitive() {
        let a =
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user"}});
        let b = serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant"}});
        assert_eq!(
            stable_codex_uuid("message", &a),
            stable_codex_uuid("message", &a),
            "same record → same id (so live de-dup is stable)"
        );
        assert_ne!(
            stable_codex_uuid("message", &a),
            stable_codex_uuid("message", &b)
        );
        assert!(stable_codex_uuid("message", &a).starts_with("codex-message-"));
    }

    #[test]
    fn codex_reasoning_becomes_thinking_and_empty_is_dropped() {
        let content = concat!(
            r#"{"type":"session_meta","payload":{"id":"r1","cwd":"/w"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"step one"},{"type":"summary_text","text":"step two"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
            "\n",
        );
        let s = parse_session_str(None, content).unwrap();
        assert_eq!(s.agent, AgentKind::Codex);
        let thinking: Vec<_> = s
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Thinking)
            .collect();
        assert_eq!(thinking.len(), 1, "empty-summary reasoning is dropped");
        assert_eq!(thinking[0].role, Role::Assistant);
        match &thinking[0].blocks[0] {
            ContentBlock::Thinking { thinking } => {
                assert!(thinking.contains("step one"));
                assert!(thinking.contains("step two"));
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn codex_function_call_argument_forms() {
        let tool_input = |args: &str| {
            let v: Value = serde_json::from_str(&format!(
                r#"{{"type":"response_item","payload":{{"type":"function_call","name":"shell","call_id":"c1","arguments":{args}}}}}"#
            ))
            .unwrap();
            match parse_line(&v).unwrap().blocks.into_iter().next().unwrap() {
                ContentBlock::ToolUse { name, id, input } => {
                    assert_eq!(name, "shell");
                    assert_eq!(id, "c1");
                    input
                }
                other => panic!("expected ToolUse, got {other:?}"),
            }
        };
        // A JSON string is parsed into a structured object.
        assert_eq!(tool_input(r#""{\"cmd\":\"ls\"}""#)["cmd"], "ls");
        // An object is passed through verbatim.
        assert_eq!(tool_input(r#"{"cmd":"ls"}"#)["cmd"], "ls");
        // A non-JSON string is preserved as a string value.
        assert_eq!(
            tool_input(r#""raw text""#),
            Value::String("raw text".into())
        );
        // null → Null.
        assert_eq!(tool_input("null"), Value::Null);
    }

    #[test]
    fn codex_function_call_output_stringifies_non_string() {
        let v: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":{"ok":true}}}"#,
        )
        .unwrap();
        let m = parse_line(&v).unwrap();
        assert_eq!(m.kind, MessageKind::ToolResult);
        assert_eq!(m.role, Role::User);
        assert!(m.model.is_none());
        match &m.blocks[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => {
                assert_eq!(tool_use_id.as_deref(), Some("c1"));
                assert!(content.contains("\"ok\":true"), "got {content:?}");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn codex_custom_tool_records_preserve_input_output_and_stable_call_ids() {
        let content = concat!(
            r#"{"type":"session_meta","payload":{"id":"custom-tools","cwd":"/w"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","id":"item-1","status":"completed","call_id":"call-42","name":"apply_patch","input":"*** Begin Patch\nraw input\n*** End Patch","internal_chat_message_metadata_passthrough":{"source":"test"}}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","id":"item-2","call_id":"call-42","output":"Done!","internal_chat_message_metadata_passthrough":{"source":"test"}}}"#,
            "\n",
        );

        let first = parse_session_str(None, content).unwrap();
        let second = parse_session_str(None, content).unwrap();
        assert_eq!(first.messages.len(), 2);
        assert_eq!(
            first
                .messages
                .iter()
                .map(|message| &message.uuid)
                .collect::<Vec<_>>(),
            second
                .messages
                .iter()
                .map(|message| &message.uuid)
                .collect::<Vec<_>>(),
            "reconciliation must produce the same UUIDs for server de-duplication"
        );
        let call_record: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","id":"item-1","status":"completed","call_id":"call-42","name":"apply_patch","input":"*** Begin Patch\nraw input\n*** End Patch","internal_chat_message_metadata_passthrough":{"source":"test"}}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_line(&call_record).unwrap().uuid,
            first.messages[0].uuid,
            "tail parsing and complete reconciliation must share a de-dup key"
        );

        let call = &first.messages[0];
        assert_eq!(call.kind, MessageKind::ToolUse);
        assert_eq!(call.role, Role::Assistant);
        assert_eq!(
            call.raw["input"],
            "*** Begin Patch\nraw input\n*** End Patch"
        );
        assert_eq!(
            call.raw["internal_chat_message_metadata_passthrough"]["source"],
            "test"
        );
        match &call.blocks[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call-42");
                assert_eq!(name, "apply_patch");
                assert_eq!(input, "*** Begin Patch\nraw input\n*** End Patch");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }

        let output = &first.messages[1];
        assert_eq!(output.kind, MessageKind::ToolResult);
        assert_eq!(output.role, Role::User);
        match &output.blocks[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => {
                assert_eq!(tool_use_id.as_deref(), Some("call-42"));
                assert_eq!(content, "Done!");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn codex_model_carried_onto_assistant_turns_only() {
        // The fixture's turn_context sets model=gpt-5.5.
        let s = parse_session_str(None, CODEX).unwrap();
        assert!(
            s.messages.iter().any(|m| m.role == Role::Assistant),
            "fixture has assistant turns"
        );
        for m in &s.messages {
            match m.role {
                Role::Assistant => assert_eq!(
                    m.model.as_deref(),
                    Some("gpt-5.5"),
                    "assistant {:?} carries the turn model",
                    m.kind
                ),
                Role::User => {
                    assert!(m.model.is_none(), "user {:?} has no model", m.kind)
                }
            }
        }
    }

    #[test]
    fn parse_line_codex_dispatch() {
        let meta: Value =
            serde_json::from_str(r#"{"type":"session_meta","payload":{"id":"x"}}"#).unwrap();
        let turn: Value =
            serde_json::from_str(r#"{"type":"turn_context","payload":{"model":"m"}}"#).unwrap();
        let event: Value =
            serde_json::from_str(r#"{"type":"event_msg","payload":{"type":"info"}}"#).unwrap();
        let msg: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
        )
        .unwrap();
        // Only `response_item` payloads become messages; headers/events do not.
        assert!(parse_line(&meta).is_none());
        assert!(parse_line(&turn).is_none());
        assert!(parse_line(&event).is_none());
        let m = parse_line(&msg).expect("response_item message yields a Message");
        assert_eq!(m.role, Role::Assistant);
        assert!(m.text.contains("hi"));
    }

    #[test]
    fn codex_skips_garbage_and_falls_back_to_id_hint() {
        // No session_meta → id comes from the hint; the garbage line is counted.
        let content = concat!(
            "not json at all\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello codex"}]}}"#,
            "\n",
        );
        let s = parse_session_str(Some("hint-id"), content).unwrap();
        assert_eq!(s.agent, AgentKind::Codex);
        assert_eq!(s.id, "hint-id");
        assert_eq!(s.skipped_lines, 1);
        assert_eq!(s.message_count(), 1);
        assert_eq!(s.title, "hello codex");
    }

    #[test]
    fn codex_title_prefers_user_message_event_over_injected_context() {
        // Recent Codex rollouts persist injected AGENTS.md/developer context as
        // user-role response items before the human's prompt. The matching
        // `event_msg.user_message` contains only the prompt typed by the user
        // and must therefore drive the session title.
        let content = concat!(
            r#"{"type":"session_meta","payload":{"id":"codex-title","cwd":"/w"}}"#,
            "\n",
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\nRead CLAUDE.md first."}]}}"##,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Fix Codex session descriptions"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Fix Codex session descriptions"}]}}"#,
            "\n",
        );

        let session = parse_session_str(None, content).unwrap();
        assert_eq!(session.title, "Fix Codex session descriptions");
    }

    #[test]
    fn codex_title_supports_item_completed_user_message() {
        // Codex 0.147 emits human prompts as completed UserMessage items. The
        // earlier user-role response item still contains injected AGENTS.md
        // context and must not become the session title.
        let content = concat!(
            r#"{"type":"session_meta","payload":{"id":"codex-title","cwd":"/w"}}"#,
            "\n",
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\nRead CLAUDE.md first."}]}}"##,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"item-1","content":[{"type":"text","text":"Fix Codex","text_elements":[]},{"type":"local_image","path":"/tmp/input.png"},{"type":"input_text","text":"0.147 session descriptions"}]}}}"#,
            "\n",
        );

        let session = parse_session_str(None, content).unwrap();
        assert_eq!(session.title, "Fix Codex 0.147 session descriptions");
    }

    #[test]
    fn codex_unknown_content_block_becomes_other() {
        let v: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"image","url":"x"},{"type":"output_text","text":"see above"}]}}"#,
        )
        .unwrap();
        let m = parse_line(&v).unwrap();
        assert!(matches!(m.blocks[0], ContentBlock::Other(_)));
        assert!(matches!(m.blocks[1], ContentBlock::Text { .. }));
    }
}
