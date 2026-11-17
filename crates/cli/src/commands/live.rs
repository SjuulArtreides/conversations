//! `arrakis-conversations live` — the local multiplexer.
//!
//! The byte-level line framing its sessions use to follow their transcripts
//! lives in [`Tailer`], a pure, unit-testable type that turns raw appended
//! bytes into complete `core::Message`s while correctly buffering partial
//! lines (including multi-byte UTF-8 sequences split across a poll tick) and
//! advancing its byte offset only up to the last newline.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
use conversations_core::{parse_line, Message};

use crate::cli::{GlobalOpts, LiveArgs};

pub(crate) mod availability;
pub(crate) mod host;
pub(crate) mod interactive;
mod local;
mod mux;
pub(crate) mod terminal_files;

pub fn run(global: &GlobalOpts, args: &LiveArgs) -> Result<()> {
    // A launcher only ever wraps the agent it was built for, so reject the
    // mismatch here rather than spawning `adk code` and then following a store
    // it will never write to.
    if !args.launcher.supports(args.agent) {
        anyhow::bail!(
            "`--launcher {}` launches OpenCode only; got `--agent {}`",
            args.launcher,
            args.agent
        );
    }
    // `live` is the local multiplexer: unless `--host` is set it seeds the
    // user's own local agent as the first child; `--host` instead resumes the
    // checkpointed sessions. Neither takes commands from the web. The branching
    // lives in `host::run`.
    host::run(global, args)
}

/// Decode a buffer of complete JSONL bytes into chat messages, tolerantly
/// skipping blank / malformed / non-message lines.
fn decode_messages(bytes: &[u8]) -> Vec<Message> {
    let mut out = Vec::new();
    for line in bytes.split(|&b| b == b'\n') {
        push_line(line, &mut out);
    }
    out
}

/// Extract a Codex session's NATIVE id from a buffer of complete JSONL bytes:
/// the `session_meta` record's `payload.id`. This is the id `codex resume <id>`
/// expects — distinct from the conversations-minted live id we register, since
/// Codex cannot be told an id at launch. Returns the first one found (a rollout
/// has exactly one `session_meta`), or `None` for a Claude transcript / a buffer
/// without the header yet. Tolerant: blank / non-UTF-8 / malformed lines skip.
fn decode_codex_session_id(bytes: &[u8]) -> Option<String> {
    for line in bytes.split(|&b| b == b'\n') {
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            continue;
        }
        if let Some(id) = value
            .get("payload")
            .and_then(|p| p.get("id"))
            .and_then(|i| i.as_str())
        {
            return Some(id.to_string());
        }
    }
    None
}

/// Parse one raw line (without its trailing `\n`) and append a message if it is
/// a chat turn. Blank / non-UTF-8 / malformed / non-message lines are skipped.
fn push_line(line: &[u8], out: &mut Vec<Message>) {
    if line.is_empty() {
        return;
    }
    let Ok(text) = std::str::from_utf8(line) else {
        return;
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return;
    };
    if let Some(msg) = parse_line(&value) {
        out.push(msg);
    }
}

/// Pure, byte-level tail framer. Holds the current read `offset` into the file
/// and a raw-byte `partial` buffer for an incomplete trailing line.
///
/// Splitting on raw bytes (and only UTF-8-decoding complete lines) means a
/// multi-byte UTF-8 character whose bytes land in two different poll ticks is
/// reassembled correctly rather than decoded eagerly and corrupted.
#[derive(Debug, Default)]
pub struct Tailer {
    /// Byte offset into the file at which the next read begins. Always points at
    /// the start of an as-yet-unconsumed (or partially-buffered) line.
    offset: u64,
    /// Raw bytes after the last consumed newline, awaiting completion.
    partial: Vec<u8>,
}

impl Tailer {
    /// Create a tailer primed to begin reading at `offset` (the number of bytes
    /// already consumed).
    pub fn new(offset: u64) -> Self {
        Self {
            offset,
            partial: Vec::new(),
        }
    }

    /// Current read offset. Exposed for tests / introspection.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Poll `path` once: read appended bytes from `offset`, handle truncation,
    /// and return the messages from any newly-completed lines.
    ///
    /// On truncation/rotation (file length < `offset`) the tailer resets to the
    /// start and re-reads the whole file; re-storing messages is safe because
    /// history de-dups on each message `uuid`.
    fn poll(&mut self, path: &Path) -> Result<Vec<Message>> {
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let len = file
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?
            .len();

        // The file region `[offset, offset + partial.len())` was already read into
        // `partial` on a previous poll, so resume reading just past it. (Truncation
        // is detected against this same read position.)
        let read_from = self.offset + self.partial.len() as u64;

        if len < read_from {
            // File shrank or was replaced — start over from the top.
            self.offset = 0;
            self.partial.clear();
            file.seek(SeekFrom::Start(0))
                .with_context(|| format!("seeking {}", path.display()))?;
        } else {
            file.seek(SeekFrom::Start(read_from))
                .with_context(|| format!("seeking {}", path.display()))?;
        }

        let mut appended = Vec::new();
        file.read_to_end(&mut appended)
            .with_context(|| format!("reading {}", path.display()))?;

        Ok(self.ingest(&appended))
    }

    /// Pure core: feed a chunk of freshly-read bytes, emit messages for every
    /// line completed by a `\n`, buffer any trailing partial line, and advance
    /// `offset` by exactly the bytes consumed up to and including the last
    /// newline (never past a trailing partial line).
    ///
    /// This is the unit-testable heart of the tailer: it takes only `&[u8]` and
    /// touches no filesystem or network.
    pub fn ingest(&mut self, chunk: &[u8]) -> Vec<Message> {
        self.partial.extend_from_slice(chunk);

        // Find the last newline in the accumulated buffer; everything up to and
        // including it forms complete lines, the remainder is the new partial.
        let Some(last_nl) = self.partial.iter().rposition(|&b| b == b'\n') else {
            // No complete line yet; keep buffering. Offset does not advance.
            return Vec::new();
        };

        let complete = &self.partial[..=last_nl];
        let mut messages = Vec::new();
        // Split the complete region on newlines; the final element after the
        // last `\n` is empty and skipped by `push_line`.
        for line in complete.split(|&b| b == b'\n') {
            push_line(line, &mut messages);
        }

        // Every byte in `partial` was read from the file but not yet reflected in
        // `offset` (a carried-in partial line came from an earlier poll whose
        // offset stopped at the previous newline). So advancing `offset` by the
        // full number of bytes we now drain keeps it pointing at the start of the
        // remaining partial line.
        let consumed = last_nl + 1;
        self.offset += consumed as u64;

        // Retain the trailing partial line.
        self.partial.drain(..consumed);

        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn user_line(uuid: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": "sess-live",
            "timestamp": "2026-06-07T10:00:00.000Z",
            "message": { "role": "user", "content": text }
        })
        .to_string()
    }

    fn assistant_line(uuid: &str, text: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "uuid": uuid,
            "sessionId": "sess-live",
            "timestamp": "2026-06-07T10:00:01.000Z",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": text }]
            }
        })
        .to_string()
    }

    #[test]
    fn decode_codex_session_id_reads_session_meta() {
        // A Codex rollout leads with a `session_meta` carrying the real id.
        let rollout = concat!(
            r#"{"type":"session_meta","payload":{"id":"8302708a-ee28-4b7d-83e5-03ea997e55b4","cwd":"/w"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user"}}"#,
            "\n",
        );
        assert_eq!(
            decode_codex_session_id(rollout.as_bytes()).as_deref(),
            Some("8302708a-ee28-4b7d-83e5-03ea997e55b4")
        );
    }

    #[test]
    fn decode_codex_session_id_none_for_claude_or_empty() {
        // A Claude transcript has no `session_meta` → None.
        let claude = format!(
            "{}\n{}\n",
            user_line("u1", "hi"),
            assistant_line("a1", "yo")
        );
        assert_eq!(decode_codex_session_id(claude.as_bytes()), None);
        // No header yet (empty / partial) → None, never panics.
        assert_eq!(decode_codex_session_id(b""), None);
        assert_eq!(decode_codex_session_id(b"{not json"), None);
    }

    #[test]
    fn ingest_emits_complete_lines_and_buffers_partial() {
        let mut t = Tailer::new(0);

        // First chunk: one complete line + the start of a second (no trailing nl).
        let line1 = format!("{}\n", user_line("u1", "hello"));
        let partial_start = assistant_line("a1", "world"); // no newline yet
        let mut chunk1 = line1.clone().into_bytes();
        chunk1.extend_from_slice(partial_start.as_bytes());

        let msgs = t.ingest(&chunk1);
        assert_eq!(msgs.len(), 1, "only the newline-terminated line emits");
        assert_eq!(msgs[0].uuid, "u1");
        // Offset advanced only past the first complete line.
        assert_eq!(t.offset(), line1.len() as u64);

        // Second chunk completes the buffered line.
        let chunk2 = b"\n".to_vec();
        let msgs = t.ingest(&chunk2);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].uuid, "a1");
        assert_eq!(
            t.offset(),
            (line1.len() + partial_start.len() + 1) as u64,
            "offset includes the now-completed second line"
        );
    }

    #[test]
    fn ingest_reassembles_multibyte_utf8_split_across_chunks() {
        // "café 🚀" — the emoji is 4 bytes, the é is 2 bytes; we split mid-char.
        let mut t = Tailer::new(0);
        let full = format!("{}\n", user_line("u1", "café 🚀 done"));
        let bytes = full.into_bytes();

        // Pick a split point that lands on a UTF-8 continuation byte
        // (0b10xxxxxx), i.e. strictly inside a multi-byte character — the emoji
        // and the `é` both have them. Splitting here means the first chunk ends
        // mid-character and must NOT be decoded eagerly.
        let split = bytes
            .iter()
            .position(|&b| (b & 0b1100_0000) == 0b1000_0000)
            .expect("string contains a multi-byte char with a continuation byte");

        let first = &bytes[..split];
        let second = &bytes[split..];

        // First chunk ends mid-character: nothing decodable, nothing emitted.
        let msgs = t.ingest(first);
        assert!(msgs.is_empty());

        // Second chunk completes the char and the line.
        let msgs = t.ingest(second);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].uuid, "u1");
        assert!(
            msgs[0].text.contains("café 🚀"),
            "multi-byte content reassembled intact: {:?}",
            msgs[0].text
        );
        assert_eq!(t.offset(), bytes.len() as u64);
    }

    #[test]
    fn ingest_skips_blank_and_malformed_lines() {
        let mut t = Tailer::new(0);
        let mut buf = String::new();
        buf.push('\n'); // blank
        buf.push_str("{ not json }\n"); // malformed
        buf.push_str(&format!("{}\n", user_line("u1", "ok")));
        buf.push_str("{\"type\":\"summary\",\"summary\":\"x\"}\n"); // non-message

        let msgs = t.ingest(buf.as_bytes());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].uuid, "u1");
        assert_eq!(t.offset(), buf.len() as u64);
    }

    #[test]
    fn poll_resets_on_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");

        // Write two lines, prime a tailer past them.
        let l1 = format!("{}\n", user_line("u1", "one"));
        let l2 = format!("{}\n", assistant_line("a1", "two"));
        std::fs::write(&path, format!("{l1}{l2}")).unwrap();
        let mut t = Tailer::new((l1.len() + l2.len()) as u64);

        // No new bytes: nothing emitted.
        assert!(t.poll(&path).unwrap().is_empty());

        // Truncate/replace the file with a shorter single line.
        let l3 = format!("{}\n", user_line("u9", "fresh"));
        std::fs::write(&path, &l3).unwrap();

        // poll detects len < offset, resets to 0, and re-reads the whole file.
        let msgs = t.poll(&path).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].uuid, "u9");
        assert_eq!(t.offset(), l3.len() as u64);
    }

    #[test]
    fn poll_reads_appended_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let l1 = format!("{}\n", user_line("u1", "one"));
        std::fs::write(&path, &l1).unwrap();

        let mut t = Tailer::new(l1.len() as u64);
        assert!(t.poll(&path).unwrap().is_empty());

        // Append a new line.
        let l2 = format!("{}\n", assistant_line("a1", "two"));
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(l2.as_bytes()).unwrap();
        drop(f);

        let msgs = t.poll(&path).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].uuid, "a1");
        assert_eq!(t.offset(), (l1.len() + l2.len()) as u64);
    }

    /// A transcript backlog decodes into its chat messages, in order, without
    /// any HTTP call.
    #[test]
    fn decode_messages_reads_a_backlog_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let body = format!(
            "{}\n{}\n{}\n",
            user_line("u1", "hi"),
            assistant_line("a1", "hello there"),
            user_line("u2", "thanks"),
        );
        std::fs::write(&path, &body).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let messages = decode_messages(&bytes);
        assert_eq!(messages.len(), 3);
        let ids: Vec<_> = messages.iter().map(|m| m.uuid.as_str()).collect();
        assert_eq!(ids, vec!["u1", "a1", "u2"]);
    }
}
