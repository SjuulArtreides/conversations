//! OpenRouter client and transcript rendering for the chat + summary features.
//!
//! The client is a thin wrapper over the OpenRouter `chat/completions` endpoint
//! (OpenAI-compatible). It is constructed once from [`Config`] and stored on
//! [`crate::state::AppState`]; it exists only when `OPENROUTER_API_KEY` is set,
//! so handlers treat `None` as "LLM disabled" and return 503.

use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use conversations_core as core;

use crate::config::Config;
use crate::error::{AppError, Result};

/// One message in an OpenRouter chat request (`role` ∈ system|user|assistant).
#[derive(Debug, Clone, Serialize)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

impl ChatTurn {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

/// OpenRouter chat client. Cloneable (cheap — wraps a `reqwest::Client`).
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl LlmClient {
    /// Build a client from config, or `None` when no API key is configured.
    pub fn from_config(cfg: &Config) -> Option<Self> {
        let api_key = cfg.openrouter_api_key.clone()?;
        Some(Self {
            http: reqwest::Client::new(),
            api_key,
            model: cfg.openrouter_model.clone(),
            base_url: cfg.openrouter_base_url.trim_end_matches('/').to_string(),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Send `turns` to the model and return the assistant's text reply.
    ///
    /// `model_override`, when `Some` and non-empty, replaces the configured
    /// default model for this request only.
    pub async fn complete(&self, turns: &[ChatTurn]) -> Result<String> {
        self.complete_with(None, turns).await
    }

    /// Like [`complete`] but accepts an optional per-request model override.
    pub async fn complete_with(
        &self,
        model_override: Option<&str>,
        turns: &[ChatTurn],
    ) -> Result<String> {
        let model = model_override
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.model);
        let url = format!("{}/chat/completions", self.base_url);
        let body = serde_json::json!({ "model": model, "messages": turns });

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            // OpenRouter attribution headers (optional but recommended).
            .header(
                "HTTP-Referer",
                "https://github.com/SjuulArtreides/conversations",
            )
            .header("X-Title", "Arrakis Conversations")
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("OpenRouter request failed: {e}")))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AppError::Internal(anyhow::anyhow!(
                "OpenRouter returned HTTP {status}: {text}"
            )));
        }
        let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
            AppError::Internal(anyhow::anyhow!(
                "OpenRouter response parse error: {e}; body: {text}"
            ))
        })?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| AppError::Internal(anyhow::anyhow!("OpenRouter returned no choices")))
    }

    /// Open a streaming chat completion against OpenRouter and yield assistant
    /// text deltas. The returned stream emits `Ok(token_text)` for each non-empty
    /// content delta, in order, and ends (with no item) when the upstream sends
    /// `data: [DONE]` or closes cleanly. A transport or HTTP error before the
    /// stream is established surfaces as the outer `Err`; an error mid-stream is
    /// yielded as an `Err` item (the caller must treat that as a failed reply and
    /// persist nothing).
    pub async fn complete_stream(
        &self,
        turns: &[ChatTurn],
    ) -> Result<impl Stream<Item = Result<String>>> {
        self.complete_stream_with(None, turns).await
    }

    /// Like [`complete_stream`] but accepts an optional per-request model override.
    pub async fn complete_stream_with(
        &self,
        model_override: Option<&str>,
        turns: &[ChatTurn],
    ) -> Result<impl Stream<Item = Result<String>>> {
        let model = model_override
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.model);
        let url = format!("{}/chat/completions", self.base_url);
        let body = serde_json::json!({ "model": model, "messages": turns, "stream": true });

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header(
                "HTTP-Referer",
                "https://github.com/SjuulArtreides/conversations",
            )
            .header("X-Title", "Arrakis Conversations")
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("OpenRouter request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(anyhow::anyhow!(
                "OpenRouter returned HTTP {status}: {text}"
            )));
        }

        // Convert the byte stream into a stream of text deltas. We buffer bytes
        // across chunks (SSE frames can split across network reads), parse every
        // complete `\n\n`-delimited frame, and emit each content delta. `[DONE]`
        // (or a clean close) ends the stream. State carried across `unfold`
        // iterations: the byte stream, a UTF-8 buffer, a queue of decoded deltas
        // not yet yielded, and a `done` flag.
        struct StreamState<S> {
            bytes: S,
            buf: String,
            pending: std::collections::VecDeque<String>,
            done: bool,
        }

        let state = StreamState {
            bytes: Box::pin(resp.bytes_stream()),
            buf: String::new(),
            pending: std::collections::VecDeque::new(),
            done: false,
        };

        let stream = futures::stream::unfold(state, |mut state| async move {
            loop {
                // Drain anything we already decoded before reading more bytes.
                if let Some(delta) = state.pending.pop_front() {
                    return Some((Ok(delta), state));
                }
                if state.done {
                    return None;
                }
                match state.bytes.next().await {
                    Some(Ok(chunk)) => {
                        state.buf.push_str(&String::from_utf8_lossy(&chunk));
                        let outcome = drain_sse_frames(&mut state.buf);
                        state.pending.extend(outcome.deltas);
                        if outcome.done {
                            state.done = true;
                        }
                        // Loop: emit a decoded delta, or read the next chunk.
                    }
                    Some(Err(e)) => {
                        // Surface the transport error as a stream item, then end.
                        state.done = true;
                        let err =
                            AppError::Internal(anyhow::anyhow!("OpenRouter stream error: {e}"));
                        return Some((Err(err), state));
                    }
                    // Upstream closed cleanly with no [DONE]; treat as complete.
                    None => return None,
                }
            }
        });
        Ok(stream)
    }
}

/// Result of draining all complete SSE frames currently in the buffer.
#[derive(Debug, Default, PartialEq)]
struct SseDrain {
    /// Non-empty content deltas, in order.
    deltas: Vec<String>,
    /// `true` once a `data: [DONE]` terminator has been seen.
    done: bool,
}

/// Parse and remove every complete (`\n\n`-terminated) SSE frame from `buf`,
/// returning the content deltas found and whether a `[DONE]` terminator was
/// seen. Any trailing partial frame is left in `buf` for the next chunk.
///
/// Each frame may contain multiple `data:` lines; `data: [DONE]` terminates the
/// stream; other `data:` payloads are parsed as OpenRouter chat-completion
/// chunks and their `choices[0].delta.content` extracted (absent/empty content
/// is skipped). Non-`data:` lines (comments, `event:`, blank) are ignored.
fn drain_sse_frames(buf: &mut String) -> SseDrain {
    let mut out = SseDrain::default();

    // Find complete frames separated by a blank line. Keep the trailing remainder
    // (an incomplete frame) in the buffer.
    let mut remainder_start = 0;
    while let Some(rel) = buf[remainder_start..].find("\n\n") {
        let frame_end = remainder_start + rel;
        let frame = buf[remainder_start..frame_end].to_string();
        remainder_start = frame_end + 2; // skip the "\n\n"

        for line in frame.lines() {
            let line = line.trim_end_matches('\r');
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                out.done = true;
                continue;
            }
            if let Some(delta) = parse_delta_content(data) {
                if !delta.is_empty() {
                    out.deltas.push(delta);
                }
            }
        }
    }

    if remainder_start > 0 {
        buf.drain(..remainder_start);
    }
    out
}

/// Extract `choices[0].delta.content` from a single OpenRouter SSE `data` JSON
/// payload. Returns `None` when the payload is not parseable or the field is
/// absent (both are normal — keepalive/role-only frames carry no content).
fn parse_delta_content(data: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct StreamChunk {
        choices: Vec<StreamChoice>,
    }
    #[derive(Deserialize)]
    struct StreamChoice {
        #[serde(default)]
        delta: StreamDelta,
    }
    #[derive(Deserialize, Default)]
    struct StreamDelta {
        #[serde(default)]
        content: Option<String>,
    }

    let parsed: StreamChunk = serde_json::from_str(data).ok()?;
    parsed.choices.into_iter().next()?.delta.content
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}
#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

/// Per-tool-result truncation when rendering the transcript.
const TOOL_RESULT_MAX: usize = 600;

/// Render a session transcript to a compact plain-text form suitable for an LLM
/// context. Tool calls become `[tool: Name] description/command`; tool results
/// are truncated. The whole thing is clamped to `budget_chars` with a head+tail
/// window so very long sessions still fit a context window.
pub fn render_transcript(session: &core::Session, budget_chars: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Session: {}\n", session.title));
    if let Some(p) = &session.project_path {
        out.push_str(&format!("Project: {p}\n"));
    }
    if let Some(b) = &session.git_branch {
        out.push_str(&format!("Branch: {b}\n"));
    }
    out.push('\n');

    for m in &session.messages {
        let role = match m.role {
            core::Role::User => "User",
            core::Role::Assistant => "Assistant",
        };
        let mut parts: Vec<String> = Vec::new();
        for block in &m.blocks {
            match block {
                core::ContentBlock::Text { text } => {
                    if !text.trim().is_empty() {
                        parts.push(text.clone());
                    }
                }
                core::ContentBlock::Thinking { .. } => {
                    // Skip thinking — often signature-only and not useful here.
                }
                core::ContentBlock::ToolUse { name, input, .. } => {
                    let detail = tool_use_detail(input);
                    parts.push(format!("[tool: {name}] {detail}"));
                }
                core::ContentBlock::ToolResult { content, .. } => {
                    parts.push(format!(
                        "[tool result] {}",
                        truncate(content, TOOL_RESULT_MAX)
                    ));
                }
                core::ContentBlock::Other(_) => {}
            }
        }
        if parts.is_empty() {
            continue;
        }
        out.push_str(&format!("## {role}\n{}\n\n", parts.join("\n")));
    }

    clamp_window(&out, budget_chars)
}

/// Extract a human-meaningful one-liner from a tool_use input value.
fn tool_use_detail(input: &serde_json::Value) -> String {
    for key in [
        "description",
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
    ] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            return truncate(s, 300);
        }
    }
    truncate(&input.to_string(), 300)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… [+{} chars]", s.chars().count() - max)
}

/// Clamp `s` to `budget` chars by keeping a head (60%) and tail (40%) window.
fn clamp_window(s: &str, budget: usize) -> String {
    let total = s.chars().count();
    if total <= budget {
        return s.to_string();
    }
    let head_len = budget * 6 / 10;
    let tail_len = budget.saturating_sub(head_len);
    let head: String = s.chars().take(head_len).collect();
    let tail: String = s.chars().skip(total - tail_len).collect();
    format!(
        "{head}\n\n…[transcript truncated: {} chars omitted]…\n\n{tail}",
        total - budget
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_and_marks_long() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate(&"x".repeat(50), 10), "xxxxxxxxxx… [+40 chars]");
    }

    #[test]
    fn clamp_window_keeps_head_and_tail() {
        let s = "A".repeat(100) + &"B".repeat(100);
        // A 50-char budget keeps 30 chars of head and 20 of tail.
        assert_eq!(
            clamp_window(&s, 50),
            format!(
                "{}\n\n…[transcript truncated: 150 chars omitted]…\n\n{}",
                "A".repeat(30),
                "B".repeat(20)
            )
        );
        assert_eq!(clamp_window("short", 50), "short");
    }

    /// Build an OpenRouter SSE content frame for `text`.
    fn frame(text: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({ "choices": [{ "delta": { "content": text } }] })
        )
    }

    #[test]
    fn drain_extracts_content_and_skips_empty() {
        let mut buf = String::new();
        buf.push_str(&frame("Hello"));
        buf.push_str(&frame(""));
        buf.push_str(&frame(", world"));
        let out = drain_sse_frames(&mut buf);
        assert_eq!(out.deltas, vec!["Hello".to_string(), ", world".to_string()]);
        assert!(!out.done);
        // All complete frames consumed.
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_skips_role_only_and_keepalive_frames() {
        let mut buf = String::new();
        // Role-only delta (no content) — typical first chunk.
        buf.push_str("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n");
        // An OpenRouter comment / keepalive line is not a `data:` line.
        buf.push_str(": OPENROUTER PROCESSING\n\n");
        buf.push_str(&frame("hi"));
        let out = drain_sse_frames(&mut buf);
        assert_eq!(out.deltas, vec!["hi".to_string()]);
        assert!(!out.done);
    }

    #[test]
    fn drain_detects_done_terminator() {
        let mut buf = String::new();
        buf.push_str(&frame("end"));
        buf.push_str("data: [DONE]\n\n");
        let out = drain_sse_frames(&mut buf);
        assert_eq!(out.deltas, vec!["end".to_string()]);
        assert!(out.done);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_buffers_partial_frame_across_chunks() {
        // First chunk ends mid-frame (no terminating blank line yet).
        let full = frame("split across chunks");
        let (head, tail) = full.split_at(full.len() / 2);

        let mut buf = head.to_string();
        let out = drain_sse_frames(&mut buf);
        // Nothing decodable yet; the partial frame stays buffered.
        assert!(out.deltas.is_empty());
        assert!(!out.done);
        assert_eq!(buf, head);

        // Second chunk completes the frame.
        buf.push_str(tail);
        let out = drain_sse_frames(&mut buf);
        assert_eq!(out.deltas, vec!["split across chunks".to_string()]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_handles_multiple_frames_in_one_chunk() {
        let mut buf = String::new();
        buf.push_str(&frame("a"));
        buf.push_str(&frame("b"));
        buf.push_str("data: [DONE]\n\n");
        // A trailing partial frame (no blank line) is preserved.
        buf.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"c\"}}]}");
        let out = drain_sse_frames(&mut buf);
        assert_eq!(out.deltas, vec!["a".to_string(), "b".to_string()]);
        assert!(out.done);
        // The incomplete final frame remains buffered.
        assert!(buf.starts_with("data:"));
    }

    #[test]
    fn parse_delta_content_tolerates_garbage_and_missing() {
        assert_eq!(parse_delta_content("not json"), None);
        assert_eq!(parse_delta_content("{\"choices\":[]}"), None);
        assert_eq!(parse_delta_content("{\"choices\":[{\"delta\":{}}]}"), None);
        assert_eq!(
            parse_delta_content("{\"choices\":[{\"delta\":{\"content\":\"x\"}}]}"),
            Some("x".to_string())
        );
    }
}
