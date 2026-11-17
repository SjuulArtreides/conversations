//! Normalized data model for agent sessions.
//!
//! Everything here is `Serialize`/`Deserialize` so the server and CLI can move
//! these structures over the wire as JSON.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;

/// Agent/runtime that produced a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    #[default]
    Claude,
    Codex,
    #[serde(rename = "opencode")]
    OpenCode,
    Zsh,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::OpenCode => "opencode",
            AgentKind::Zsh => "zsh",
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" => Ok(AgentKind::Claude),
            "codex" => Ok(AgentKind::Codex),
            "opencode" => Ok(AgentKind::OpenCode),
            "zsh" => Ok(AgentKind::Zsh),
            other => Err(format!(
                "unsupported agent {other:?}; expected claude, codex, opencode, or zsh"
            )),
        }
    }
}

/// How an agent binary is invoked for a live session.
///
/// An agent is WHAT runs (and therefore how its transcript is parsed and
/// tailed); a launcher is HOW that agent is started. They are separate because
/// the Arrakis dev harness (`adk code`) starts the very same OpenCode — same
/// binary, same `opencode.db`, same session rows — but wraps it with the
/// monorepo's MCP credentials, agent profile, and host server. A session it
/// produces is an OpenCode session in every respect, so [`AgentKind`] stays
/// `opencode` and only the spawn differs.
///
/// Only OpenCode has a non-[`AgentLauncher::Opencode`] launcher; see
/// [`AgentLauncher::supports`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentLauncher {
    /// Run the agent's own binary directly (`claude`, `codex`, `opencode`, …).
    #[default]
    #[serde(rename = "opencode")]
    Opencode,
    /// Run OpenCode through the Arrakis dev harness: `adk code`.
    Adk,
}

impl AgentLauncher {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentLauncher::Opencode => "opencode",
            AgentLauncher::Adk => "adk",
        }
    }

    /// Whether this launcher can start `agent`. `adk code` only ever launches
    /// OpenCode, so every other pairing is a caller error (the server rejects
    /// it, and the CLI refuses to spawn it) rather than something to coerce.
    pub fn supports(self, agent: AgentKind) -> bool {
        match self {
            AgentLauncher::Opencode => true,
            AgentLauncher::Adk => agent == AgentKind::OpenCode,
        }
    }
}

impl fmt::Display for AgentLauncher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentLauncher {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "opencode" | "direct" => Ok(AgentLauncher::Opencode),
            "adk" | "adk-code" | "adk code" => Ok(AgentLauncher::Adk),
            other => Err(format!(
                "unsupported launcher {other:?}; expected opencode or adk"
            )),
        }
    }
}

/// The role of a chat turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// Coarse classification of a message, derived from the content blocks present.
///
/// - `Text` — only text blocks.
/// - `Thinking` — only thinking blocks.
/// - `ToolUse` — only tool-use blocks.
/// - `ToolResult` — only tool-result blocks.
/// - `Mixed` — a combination of the above (e.g. thinking + text + tool_use),
///   or no recognizable blocks at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Text,
    Thinking,
    ToolUse,
    ToolResult,
    Mixed,
}

/// A single content block within a message.
///
/// Serde is hand-written (see below) rather than derived: known variants use the
/// internally-tagged `{ "type": <tag>, .. }` shape, but [`ContentBlock::Other`]
/// preserves an unrecognized block **verbatim** — including its own `type`
/// field. A derived `#[serde(tag = "type")]` would emit a *second* `type` for
/// the `Other` variant, producing JSON with a duplicate `type` key that fails to
/// deserialize on the receiving end (the upload "duplicate field `type`" 422).
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: Option<String>,
        content: String,
    },
    /// Any block whose `type` we do not recognize. Preserved verbatim.
    Other(Value),
}

impl ContentBlock {
    /// The kind contribution of this single block, used to derive
    /// [`MessageKind`] for a message.
    fn kind(&self) -> Option<MessageKind> {
        match self {
            ContentBlock::Text { .. } => Some(MessageKind::Text),
            ContentBlock::Thinking { .. } => Some(MessageKind::Thinking),
            ContentBlock::ToolUse { .. } => Some(MessageKind::ToolUse),
            ContentBlock::ToolResult { .. } => Some(MessageKind::ToolResult),
            ContentBlock::Other(_) => None,
        }
    }
}

impl Serialize for ContentBlock {
    /// Known variants emit the internally-tagged `{ "type": <tag>, .. }` shape;
    /// `Other` is written exactly as it was preserved. Routing each variant
    /// through a single JSON value guarantees exactly one `type` key.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = match self {
            ContentBlock::Text { text } => serde_json::json!({ "type": "text", "text": text }),
            ContentBlock::Thinking { thinking } => {
                serde_json::json!({ "type": "thinking", "thinking": thinking })
            }
            ContentBlock::ToolUse { id, name, input } => {
                serde_json::json!({ "type": "tool_use", "id": id, "name": name, "input": input })
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
            }),
            ContentBlock::Other(v) => v.clone(),
        };
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    /// Deserialize via the raw JSON value and the shared block parser, so the
    /// wire form is interpreted identically to a freshly-parsed transcript and
    /// any unknown `type` falls back to [`ContentBlock::Other`].
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Ok(crate::parser::parse_block(&value))
    }
}

/// Token usage for an assistant turn, when present. All fields optional because
/// different model/runtime versions report different subsets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

impl Usage {
    /// True when every field is `None` — used to drop empty usage objects.
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_read_input_tokens.is_none()
            && self.cache_creation_input_tokens.is_none()
    }
}

/// A normalized chat turn (a `user` or `assistant` record).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub role: Role,
    pub kind: MessageKind,
    pub model: Option<String>,
    pub is_sidechain: bool,
    pub timestamp: Option<DateTime<Utc>>,
    pub blocks: Vec<ContentBlock>,
    /// Rendered plain text for search / preview (text + thinking + tool result
    /// content concatenated). Cheap to index.
    pub text: String,
    /// The original `message` JSON object, kept for full fidelity.
    pub raw: Value,
    pub usage: Option<Usage>,
}

impl Message {
    /// Derive [`MessageKind`] from a slice of blocks.
    pub fn derive_kind(blocks: &[ContentBlock]) -> MessageKind {
        let mut seen: Option<MessageKind> = None;
        for block in blocks {
            let Some(k) = block.kind() else { continue };
            match seen {
                None => seen = Some(k),
                Some(prev) if prev == k => {}
                Some(_) => return MessageKind::Mixed,
            }
        }
        seen.unwrap_or(MessageKind::Mixed)
    }
}

/// A fully parsed session.
///
/// `skipped_lines` reports how many input lines were tolerated-but-dropped
/// (malformed JSON). It rides along with the session so callers (server, CLI)
/// can surface a partial-parse warning without a separate stats channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub agent: AgentKind,
    pub title: String,
    pub project_path: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub claude_version: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub messages: Vec<Message>,
    /// Count of malformed lines skipped during parsing.
    #[serde(default)]
    pub skipped_lines: usize,
}

impl Session {
    /// Number of chat turns (user + assistant). Metadata records and
    /// attachments are not messages and are not counted.
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
}

/// A repository a conversation touched, as supplied at **upload** time.
///
/// A conversation can span several repos, so a session carries a list of these.
/// `local_path` is the *uploading user's* local checkout path for this
/// repo+branch — the server records it keyed by the uploading user, since
/// different engineers check the same repo out at different locations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoLink {
    /// Short repo name / slug, e.g. `conversations`.
    pub repo: String,
    /// Git remote URL (origin), when known.
    #[serde(default)]
    pub remote_url: Option<String>,
    /// Branch worked on.
    pub branch: String,
    /// Pull-request URL, when known.
    #[serde(default)]
    pub pr_url: Option<String>,
    /// The uploading user's local checkout path for this repo+branch.
    #[serde(default)]
    pub local_path: Option<String>,
    /// True when this repo is a monorepo **container** — a git repo that itself
    /// contains nested git repos (e.g. the `example-workspace` root). Detected
    /// structurally by the CLI. A container is excluded from single-repo
    /// PR-context resolution so a bare `PR #N` never links to a non-existent
    /// top-level PR. Defaults to `false` (older CLIs / non-container repos).
    #[serde(default)]
    pub is_container: bool,
}

/// Lightweight summary of a session, produced without building full messages.
/// Used for fast CLI listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    #[serde(default)]
    pub agent: AgentKind,
    pub title: String,
    pub project_path: Option<String>,
    pub git_branch: Option<String>,
    pub message_count: usize,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

/// A reference to a session file on disk, returned by discovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFileRef {
    pub path: std::path::PathBuf,
    pub session_id: String,
    #[serde(default)]
    pub agent: AgentKind,
    /// The raw (dash-encoded) project directory name.
    pub project_dir: String,
    pub size_bytes: u64,
    pub modified: Option<DateTime<Utc>>,
    /// Metadata already available from a non-file session index (currently
    /// OpenCode's SQLite store). File-backed Claude/Codex discovery leaves this
    /// empty and callers fall back to [`crate::quick_summary`].
    #[serde(default)]
    pub summary: Option<SessionSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn opencode_agent_uses_the_public_wire_name() {
        let encoded = serde_json::to_string(&AgentKind::OpenCode).unwrap();
        assert_eq!(encoded, r#""opencode""#);
        assert_eq!(
            serde_json::from_str::<AgentKind>(&encoded).unwrap(),
            AgentKind::OpenCode
        );
    }

    /// The launcher rides the same JSON as the agent, and an absent field must
    /// keep meaning "the agent's own binary" so older servers/hosts and stored
    /// checkpoints keep launching OpenCode directly.
    #[test]
    fn launcher_defaults_to_the_agents_own_binary_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&AgentLauncher::Opencode).unwrap(),
            r#""opencode""#
        );
        assert_eq!(
            serde_json::to_string(&AgentLauncher::Adk).unwrap(),
            r#""adk""#
        );
        assert_eq!(AgentLauncher::default(), AgentLauncher::Opencode);
        assert_eq!(
            serde_json::from_str::<AgentLauncher>(r#""adk""#).unwrap(),
            AgentLauncher::Adk
        );
        assert_eq!(
            "adk-code".parse::<AgentLauncher>().unwrap(),
            AgentLauncher::Adk
        );
        assert!("claude".parse::<AgentLauncher>().is_err());
    }

    #[test]
    fn adk_launches_only_opencode() {
        assert!(AgentLauncher::Adk.supports(AgentKind::OpenCode));
        for agent in [AgentKind::Claude, AgentKind::Codex, AgentKind::Zsh] {
            assert!(!AgentLauncher::Adk.supports(agent));
            assert!(AgentLauncher::Opencode.supports(agent));
        }
    }

    /// Regression for the upload `422 Unprocessable Entity: ... duplicate field
    /// `type``: an unrecognized block is preserved verbatim (it keeps its own
    /// `type`), and must serialize to JSON with a *single* `type` key that the
    /// receiver can deserialize again. The old `#[serde(tag = "type")]` derive
    /// emitted both the `"other"` tag and the preserved `type`.
    #[test]
    fn other_block_roundtrips_without_duplicate_type() {
        let original = json!({
            "type": "image",
            "source": { "type": "base64", "data": "abc" }
        });
        let block = ContentBlock::Other(original.clone());

        let serialized = serde_json::to_string(&block).unwrap();

        // The receiving side (server) deserializes the body — this is the exact
        // operation that 422'd before the fix.
        let back: ContentBlock = serde_json::from_str(&serialized)
            .expect("Other block must deserialize (no duplicate `type`)");
        assert_eq!(back, ContentBlock::Other(original));

        // Top-level `type` survives as the block's real type, not "other".
        let as_value: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(as_value.get("type").and_then(Value::as_str), Some("image"));
    }

    /// Known variants keep the internally-tagged `{ "type": .., .. }` wire shape
    /// the frontend expects, and round-trip cleanly.
    #[test]
    fn known_variants_keep_internally_tagged_shape() {
        let text = ContentBlock::Text { text: "hi".into() };
        assert_eq!(
            serde_json::to_value(&text).unwrap(),
            json!({ "type": "text", "text": "hi" })
        );

        let tool = ContentBlock::ToolUse {
            id: "tu_1".into(),
            name: "Bash".into(),
            input: json!({ "command": "ls" }),
        };
        let serialized = serde_json::to_value(&tool).unwrap();
        assert_eq!(serialized["type"], "tool_use");
        assert_eq!(serialized["name"], "Bash");

        for block in [text, tool] {
            let s = serde_json::to_string(&block).unwrap();
            let back: ContentBlock = serde_json::from_str(&s).unwrap();
            assert_eq!(back, block);
        }
    }

    /// The server receives blocks as raw JSON. An unknown `type` (e.g. `image`)
    /// must deserialize into `Other`, NOT error with "unknown variant `image`"
    /// — which is exactly the 422 the upload hit once the duplicate-`type` was
    /// gone but the server still used the derived internally-tagged enum.
    #[test]
    fn unknown_type_deserializes_to_other_not_error() {
        let raw = r#"{"type":"image","source":{"type":"base64","data":"zzz"}}"#;
        let block: ContentBlock =
            serde_json::from_str(raw).expect("unknown block type must fall back to Other");
        match block {
            ContentBlock::Other(v) => {
                assert_eq!(v.get("type").and_then(Value::as_str), Some("image"));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    /// A preserved block that has no `type` field at all still round-trips as
    /// `Other` (the parser's catch-all).
    #[test]
    fn other_block_without_type_field_roundtrips() {
        let original = json!({ "foo": "bar", "nested": [1, 2, 3] });
        let block = ContentBlock::Other(original.clone());
        let s = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ContentBlock::Other(original));
    }

    /// `thinking` and `tool_result` (with an absent `tool_use_id`) round-trip.
    #[test]
    fn thinking_and_tool_result_roundtrip() {
        let blocks = [
            ContentBlock::Thinking {
                thinking: "let me reason".into(),
            },
            ContentBlock::ToolResult {
                tool_use_id: None,
                content: "exit 0".into(),
            },
            ContentBlock::ToolResult {
                tool_use_id: Some("tu_9".into()),
                content: "done".into(),
            },
        ];
        for block in blocks {
            let s = serde_json::to_string(&block).unwrap();
            let back: ContentBlock = serde_json::from_str(&s).unwrap();
            assert_eq!(back, block);
        }
    }

    fn msg(blocks: Vec<ContentBlock>) -> Message {
        Message {
            uuid: "m1".into(),
            parent_uuid: None,
            role: Role::Assistant,
            kind: Message::derive_kind(&blocks),
            model: Some("claude".into()),
            is_sidechain: false,
            timestamp: None,
            blocks,
            text: String::new(),
            raw: Value::Null,
            usage: None,
        }
    }

    /// End-to-end regression at the boundary the bug actually crossed: a whole
    /// `Session` containing an unrecognized (`image`) block must survive the
    /// serialize→deserialize round-trip the upload endpoint performs.
    #[test]
    fn session_with_image_block_roundtrips() {
        let image = ContentBlock::Other(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" }
        }));
        let session = Session {
            id: "sess-img".into(),
            agent: AgentKind::Claude,
            title: "has an image".into(),
            project_path: None,
            cwd: None,
            git_branch: None,
            claude_version: None,
            started_at: None,
            ended_at: None,
            messages: vec![
                msg(vec![ContentBlock::Text {
                    text: "look at this".into(),
                }]),
                msg(vec![
                    ContentBlock::Text {
                        text: "before".into(),
                    },
                    image,
                    ContentBlock::Text {
                        text: "after".into(),
                    },
                ]),
            ],
            skipped_lines: 0,
        };

        let json = serde_json::to_string(&session).expect("serialize");
        let back: Session = serde_json::from_str(&json).expect("session must round-trip");
        assert_eq!(back, session);
    }
}
