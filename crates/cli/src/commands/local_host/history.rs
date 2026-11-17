//! Canonical local transcript history. Messages arrive from the host's own
//! native-transcript producer (the same tailers that feed the central server) and
//! are stored in a private SQLite file as the shared core `Message` (blocks, raw
//! JSON, usage intact) with an FTS5 index over the rendered text.
//!
//! This database is separate from `authz.db`: history can be large and is
//! written constantly, authority records are small and latency-sensitive. Reads
//! are always scoped by an explicit session id (or an explicit "everything",
//! which only the local CLI, running as the host's own user, can ask for); a
//! future signed remote reader must pass the per-session grant set instead.

use std::path::Path;

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use conversations_core::private_channel::{HistoryItem, MAX_FRAGMENT_BYTES, MAX_INLINE_MESSAGE};
use conversations_core::{AgentKind, Message};
use ring::digest;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::authz::store::{now_secs, open_private};

const SCHEMA_VERSION: i64 = 6;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS imported_sessions(
  session_id TEXT PRIMARY KEY, title TEXT, remote_cwd TEXT);
-- Local-only explicit web rename: the user typed this title in the
-- `/private/<id>` UI. Separate from `imported_sessions.title`, which is
-- download provenance (a title shipped with an uploaded history), so the
-- summary can tell "I explicitly named this" apart from "this is what the
-- history came in with". Never leaves this host.
CREATE TABLE IF NOT EXISTS local_titles(
  session_id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS messages(
  session_id TEXT NOT NULL, uuid TEXT NOT NULL,
  seq INTEGER NOT NULL,            -- per-session order of first appearance
  message_json TEXT NOT NULL,      -- full core::Message (blocks, raw, usage)
  sha256 BLOB NOT NULL,
  agent TEXT NOT NULL, native_id TEXT NOT NULL, source TEXT NOT NULL,
  first_ingested INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  PRIMARY KEY(session_id, uuid));
CREATE TABLE IF NOT EXISTS linear_refs(
  session_id TEXT NOT NULL, url TEXT NOT NULL, workspace TEXT NOT NULL,
  kind TEXT NOT NULL, identifier TEXT NOT NULL, unlinked INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(session_id,url));
CREATE TABLE IF NOT EXISTS linear_backfilled(session_id TEXT PRIMARY KEY);
-- Old daemon tailers may keep schema3 connections alive during an upgrade.
-- Their ordinary transcript writes invalidate derived metadata without needing new code.
CREATE TRIGGER IF NOT EXISTS linear_insert_invalidates AFTER INSERT ON messages BEGIN
  DELETE FROM linear_backfilled WHERE session_id=NEW.session_id;
END;
CREATE TRIGGER IF NOT EXISTS linear_update_invalidates AFTER UPDATE OF message_json ON messages BEGIN
  DELETE FROM linear_backfilled WHERE session_id=NEW.session_id;
END;
CREATE TABLE IF NOT EXISTS terminal_prs(
  session_id TEXT NOT NULL, url TEXT NOT NULL,
  PRIMARY KEY(session_id, url));
-- Local-only effective title for a session, written by the host when a cmux
-- workspace rename names the session. Never leaves the machine over any
-- wire — the private channel's SessionsList reads it via `summary()`.
CREATE TABLE IF NOT EXISTS host_titles(
  session_id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  updated_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS messages_by_session ON messages(session_id, seq);
-- Listing counts/activity must not visit multi-megabyte transcript payload pages.
CREATE INDEX IF NOT EXISTS messages_activity ON messages(session_id, updated_at);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
  text, session_id UNINDEXED, uuid UNINDEXED, tokenize = 'unicode61');
"#;

/// Where a batch came from. Recorded per message so history never implies an
/// unrelated native transcript belongs to the session.
#[derive(Debug, Clone)]
pub struct Source {
    pub agent: AgentKind,
    /// The native agent session id the producer was bound to.
    pub native_id: String,
    /// Transcript file or database the messages were read from.
    pub origin: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Ingested {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Hit {
    pub session_id: String,
    pub uuid: String,
    pub seq: u64,
    pub snippet: String,
}

/// One piece of a stored message (see [`History::fragment`]).
#[derive(Debug, PartialEq, Eq)]
pub struct Fragment {
    pub data: Vec<u8>,
    pub size: u64,
    pub sha256: String,
    pub next: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Summary {
    pub messages: u64,
    pub title: Option<String>,
    /// Unix seconds.
    pub last_updated: Option<i64>,
}

pub struct History {
    conn: Connection,
}

fn remember_linear(conn: &Connection, session: &str, text: &str) -> Result<()> {
    for doc in conversations_core::linear::detect_linear_refs(text) {
        conn.execute("INSERT OR IGNORE INTO linear_refs(session_id,url,workspace,kind,identifier) VALUES(?1,?2,?3,?4,?5)",params![session,doc.url,doc.workspace,doc.kind,doc.identifier])?;
    }
    Ok(())
}

/// Record any GitHub PR URLs the agent emitted (user text, assistant text, tool
/// results) under the session, capped at [`MAX_TERMINAL_PR_URLS`]. The matcher
/// is shared with the shell screen scanner, so markdown emphasis and serialized
/// tool output both resolve to the canonical URL. Only full URLs are stored —
/// bare `PR #N` without a verified repo context is deliberately NOT inferred.
fn remember_terminal_prs(conn: &Connection, session: &str, text: &str) -> Result<()> {
    for url in conversations_core::terminal_prs::discover_in_text(text) {
        conn.execute(
            "INSERT OR IGNORE INTO terminal_prs(session_id,url)
             SELECT ?1,?2 WHERE (SELECT count(*) FROM terminal_prs WHERE session_id=?1) < ?3",
            params![
                session,
                url,
                conversations_core::terminal_prs::MAX_TERMINAL_PR_URLS as i64
            ],
        )?;
    }
    Ok(())
}

impl History {
    /// Retain discoveries across repaints and host restarts, with a hard per-session cap.
    pub fn remember_prs(&self, session: &str, urls: &[String]) -> Result<()> {
        for url in urls {
            self.conn.execute(
                "INSERT OR IGNORE INTO terminal_prs(session_id,url)
                SELECT ?1,?2 WHERE (SELECT count(*) FROM terminal_prs WHERE session_id=?1) < ?3",
                params![
                    session,
                    url,
                    conversations_core::terminal_prs::MAX_TERMINAL_PR_URLS as i64
                ],
            )?;
        }
        Ok(())
    }

    /// A listing carries many sessions: bound each link projection, not just its count.
    pub fn listed_prs(&self, session: &str) -> Result<Vec<String>> {
        let mut bytes = 0;
        Ok(self
            .prs(session)?
            .into_iter()
            .filter(|url| {
                let size =
                    serde_json::to_vec(url).map_or(usize::MAX, |json| json.len().saturating_add(1));
                if size <= 512usize.saturating_sub(bytes) {
                    bytes += size;
                    true
                } else {
                    false
                }
            })
            .collect())
    }

    pub fn prs(&self, session: &str) -> Result<Vec<String>> {
        let mut statement = self
            .conn
            .prepare("SELECT url FROM terminal_prs WHERE session_id=?1 ORDER BY rowid")?;
        let urls = statement
            .query_map(params![session], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(urls)
    }

    /// Existing histories are scanned once, using FTS candidate text rather than raw payloads.
    fn backfill_linear(&mut self, session: &str) -> Result<()> {
        let filled = |conn: &Connection| -> Result<bool> {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM linear_backfilled WHERE session_id=?1",
                    [session],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        };
        if filled(&self.conn)? {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !filled(&tx)? {
            let mut statement = tx.prepare(
                "SELECT text FROM messages_fts WHERE messages_fts MATCH ?1 AND session_id=?2",
            )?;
            let texts = statement.query_map(params!["\"linear.app\"", session], |row| {
                row.get::<_, String>(0)
            })?;
            for text in texts {
                remember_linear(&tx, session, &text?)?;
            }
            tx.execute(
                "INSERT INTO linear_backfilled(session_id) VALUES(?1)",
                [session],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A bounded metadata page. Warm reads touch only the per-session metadata index.
    pub fn linear_page(
        &mut self,
        session: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<(Vec<conversations_core::SessionLinearDoc>, Option<String>)> {
        ensure!(
            (1..=100).contains(&limit),
            "Linear page limit must be 1-100"
        );
        self.backfill_linear(session)?;
        let mut statement = self.conn.prepare("SELECT url,workspace,kind,identifier FROM linear_refs WHERE session_id=?1 AND unlinked=0 AND url>?2 ORDER BY url LIMIT ?3")?;
        let rows = statement.query_map(
            params![session, after.unwrap_or(""), i64::from(limit) + 1],
            |row| {
                Ok(conversations_core::SessionLinearDoc {
                    url: row.get(0)?,
                    workspace: row.get(1)?,
                    kind: row.get(2)?,
                    identifier: row.get(3)?,
                })
            },
        )?;
        let mut docs: Vec<conversations_core::SessionLinearDoc> = Vec::new();
        let mut bytes = 0;
        // Reserve the actual reply shape and serialized URL cursor as well as docs.
        let overhead =
            serde_json::to_vec(&serde_json::json!({"kind":"linear","docs":[],"next":""}))?.len()
                - 2;
        for row in rows {
            let doc = row?;
            let size = serde_json::to_vec(&doc)?.len();
            let cursor_bytes = serde_json::to_vec(&doc.url)?.len();
            let separator = usize::from(!docs.is_empty());
            ensure!(
                overhead + size + cursor_bytes <= 32 * 1024,
                "Linear reference exceeds the metadata reply limit"
            );
            if docs.len() == limit as usize
                || overhead + bytes + separator + size + cursor_bytes > 32 * 1024
            {
                let next = docs.last().map(|doc| doc.url.clone());
                return Ok((docs, next));
            }
            bytes += separator + size;
            docs.push(doc);
        }
        Ok((docs, None))
    }

    /// Explicit unlink persists across later mentions and imports; it never edits messages.
    pub fn unlink_linear(&mut self, session: &str, url: &str) -> Result<bool> {
        let canonical =
            conversations_core::linear::parse_linear_url(url).context("Invalid Linear URL")?;
        self.backfill_linear(session)?;
        Ok(self.conn.execute(
            "UPDATE linear_refs SET unlinked=1 WHERE session_id=?1 AND url=?2",
            params![session, canonical.url],
        )? > 0)
    }

    pub fn open(dir: &Path) -> Result<Self> {
        let mut conn = open_private(dir, "history.db")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= SCHEMA_VERSION,
            "history database was written by a newer host (schema {version})"
        );
        // Reading initialized history must not compete with a transcript import
        // for WAL's writer slot. Even writing the unchanged user_version does.
        if version == SCHEMA_VERSION {
            return Ok(Self { conn });
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Another opener may have completed migration while we waited.
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= SCHEMA_VERSION,
            "history database was written by a newer host (schema {version})"
        );
        if version < SCHEMA_VERSION {
            tx.execute_batch(SCHEMA)
                .context("creating history schema (is FTS5 available?)")?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        tx.commit()?;
        Ok(Self { conn })
    }

    /// Persist `messages` for `session_id` in one transaction. New messages are
    /// appended in order; a message already stored with identical content is a
    /// no-op; the same uuid with changed content (a streamed turn that finished)
    /// replaces its row and index entry in place, keeping its position. Rows are
    /// never deleted because a native transcript was truncated or rotated.
    pub fn ingest(
        &mut self,
        session_id: &str,
        source: &Source,
        messages: &[Message],
    ) -> Result<Ingested> {
        self.ingest_mode(session_id, source, messages, false)
    }

    /// Remote snapshots only fill gaps; they never replace a locally stored turn.
    pub fn import_messages(
        &mut self,
        session_id: &str,
        source: &Source,
        messages: &[Message],
    ) -> Result<Ingested> {
        self.ingest_mode(session_id, source, messages, true)
    }

    fn ingest_mode(
        &mut self,
        session_id: &str,
        source: &Source,
        messages: &[Message],
        insert_only: bool,
    ) -> Result<Ingested> {
        ensure!(!session_id.is_empty(), "history needs a session id");
        let now = now_secs();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let linear_was_complete = tx
            .query_row(
                "SELECT 1 FROM linear_backfilled WHERE session_id=?1",
                [session_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let mut stats = Ingested::default();
        let mut next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM messages WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        for message in messages {
            let json = serde_json::to_string(message).context("serializing message")?;
            let sha = digest::digest(&digest::SHA256, json.as_bytes());
            let existing: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT sha256 FROM messages WHERE session_id = ?1 AND uuid = ?2",
                    params![session_id, message.uuid],
                    |r| r.get(0),
                )
                .optional()?;
            match existing {
                Some(old) if insert_only || old == sha.as_ref() => stats.unchanged += 1,
                Some(_) => {
                    tx.execute(
                        "UPDATE messages SET message_json = ?3, sha256 = ?4, agent = ?5, native_id = ?6, source = ?7, updated_at = ?8
                         WHERE session_id = ?1 AND uuid = ?2",
                        params![
                            session_id,
                            message.uuid,
                            json,
                            sha.as_ref(),
                            source.agent.as_str(),
                            source.native_id,
                            source.origin,
                            now
                        ],
                    )?;
                    tx.execute(
                        "DELETE FROM messages_fts WHERE session_id = ?1 AND uuid = ?2",
                        params![session_id, message.uuid],
                    )?;
                    tx.execute(
                        "INSERT INTO messages_fts(text, session_id, uuid) VALUES(?1, ?2, ?3)",
                        params![message.text, session_id, message.uuid],
                    )?;
                    remember_linear(&tx, session_id, &message.text)?;
                    remember_terminal_prs(&tx, session_id, &message.text)?;
                    stats.updated += 1;
                }
                None => {
                    next_seq += 1;
                    // Importing old history is not new session activity. Keep
                    // first_ingested as the local audit time, and preserve the
                    // native message time for the listing's activity summary.
                    let updated_at = if insert_only {
                        message.timestamp.map_or(now, |time| time.timestamp())
                    } else {
                        now
                    };
                    tx.execute(
                        "INSERT INTO messages(session_id, uuid, seq, message_json, sha256, agent, native_id, source, first_ingested, updated_at)
                         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        params![
                            session_id,
                            message.uuid,
                            next_seq,
                            json,
                            sha.as_ref(),
                            source.agent.as_str(),
                            source.native_id,
                            source.origin,
                            now,
                            updated_at
                        ],
                    )?;
                    tx.execute(
                        "INSERT INTO messages_fts(text, session_id, uuid) VALUES(?1, ?2, ?3)",
                        params![message.text, session_id, message.uuid],
                    )?;
                    remember_linear(&tx, session_id, &message.text)?;
                    remember_terminal_prs(&tx, session_id, &message.text)?;
                    stats.inserted += 1;
                }
            }
        }
        // New writers update refs in this same transaction, so a previously complete
        // cache stays complete. Old writers leave the trigger's invalidation intact.
        if linear_was_complete {
            tx.execute(
                "INSERT OR IGNORE INTO linear_backfilled(session_id) VALUES(?1)",
                [session_id],
            )?;
        }
        tx.commit()?;
        Ok(stats)
    }

    /// The session `agent`'s native session `native` was first stored under.
    pub fn session_for_native(&self, agent: AgentKind, native: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT session_id FROM messages WHERE agent = ?1 AND native_id = ?2
                  ORDER BY first_ingested, rowid LIMIT 1",
                params![agent.as_str(), native],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The stored history of ONE session, in first-appearance order.
    pub fn messages(&self, session_id: &str, limit: usize) -> Result<Vec<Message>> {
        let mut statement = self.conn.prepare(
            "SELECT message_json FROM messages WHERE session_id = ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![session_id, i64::try_from(limit).unwrap_or(i64::MAX)],
            |r| r.get::<_, String>(0),
        )?;
        rows.map(|json| {
            let json = json?;
            serde_json::from_str(&json).context("stored message is unreadable")
        })
        .collect()
    }

    /// One bounded page of ONE session's history after sequence `after`, in
    /// order: at most `limit` items, each the complete stored message (inline
    /// when it is at most `MAX_INLINE_MESSAGE` bytes, otherwise named by size and
    /// hash for [`History::fragment`]), and the page stops before its serialized
    /// items exceed `max_bytes` (it always holds at least one item when one
    /// exists). `digest` leaves every message out: only sizes and hashes, which
    /// is how a reader finds messages rewritten in place. `next` is the cursor to
    /// continue from.
    pub fn page(
        &self,
        session_id: &str,
        after: u64,
        limit: u32,
        max_bytes: usize,
        digest: bool,
    ) -> Result<(Vec<HistoryItem>, Option<u64>)> {
        let mut statement = self.conn.prepare(
            "SELECT seq, uuid, message_json, sha256 FROM messages WHERE session_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let after = i64::try_from(after).context("history cursor out of range")?;
        let rows = statement.query_map(params![session_id, after, i64::from(limit) + 1], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut items: Vec<HistoryItem> = Vec::new();
        let mut used = 0usize;
        let mut more = false;
        for row in rows {
            let (seq, uuid, json, sha) = row?;
            if items.len() == limit as usize {
                more = true;
                break;
            }
            let message = (!digest && json.len() <= MAX_INLINE_MESSAGE)
                .then(|| serde_json::from_str::<serde_json::Value>(&json))
                .transpose()
                .context("stored message is unreadable")?;
            let item = HistoryItem {
                seq: seq as u64,
                uuid,
                size: json.len() as u64,
                sha256: B64.encode(&sha),
                message,
            };
            let size = serde_json::to_vec(&item)?.len();
            if !items.is_empty() && used + size > max_bytes {
                more = true;
                break;
            }
            used += size;
            items.push(item);
        }
        let next = more.then(|| items.last().map(|i| i.seq)).flatten();
        Ok((items, next))
    }

    /// Bytes `offset..offset + MAX_FRAGMENT_BYTES` of one stored message's
    /// canonical JSON, with its total size and hash, or `None` when the session
    /// has no message `seq` or `offset` is past its end.
    pub fn fragment(&self, session_id: &str, seq: u64, offset: u64) -> Result<Option<Fragment>> {
        let seq = i64::try_from(seq).context("history sequence out of range")?;
        let row: Option<(String, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT message_json, sha256 FROM messages WHERE session_id = ?1 AND seq = ?2",
                params![session_id, seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((json, sha)) = row else {
            return Ok(None);
        };
        let bytes = json.as_bytes();
        let Ok(start) = usize::try_from(offset) else {
            return Ok(None);
        };
        if start >= bytes.len() {
            return Ok(None);
        }
        let end = (start + MAX_FRAGMENT_BYTES).min(bytes.len());
        Ok(Some(Fragment {
            data: bytes[start..end].to_vec(),
            size: bytes.len() as u64,
            sha256: B64.encode(&sha),
            next: (end < bytes.len()).then_some(end as u64),
        }))
    }

    /// Per-session summary for a private listing: message count, readable title
    /// and last activity (original message time for imported history).
    ///
    /// Title precedence is **last-local-edit wins** between the two local
    /// sources, falling back to download provenance and then the transcript:
    ///
    /// 1. `local_titles.title` and `host_titles.title` — whichever has the
    ///    newer `updated_at` (a web rename and a cmux sidebar rename each
    ///    stamp `now_secs()` at the moment the user made the gesture; the
    ///    newer gesture is the one the user most recently expressed). This
    ///    stops `imported_sessions.title` from permanently shadowing a new
    ///    cmux rename on a session that was downloaded with an older title.
    /// 2. `imported_sessions.title` — download provenance, used only when
    ///    the user has not expressed any local title intent yet. A clear of
    ///    either local title falls through here so the imported name
    ///    resurfaces instead of vanishing.
    /// 3. The first user message — the ultimate transcript fallback.
    pub fn summary(&self, session_id: &str) -> Result<Summary> {
        let (messages, last): (i64, Option<i64>) = self.conn.query_row(
            "SELECT COUNT(*), MAX(updated_at) FROM messages WHERE session_id = ?1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let web = self.local_title_with_updated_at(session_id)?;
        let host = self.host_title_with_updated_at(session_id)?;
        let mut title = match (web, host) {
            (Some((w_title, w_at)), Some((h_title, h_at))) => {
                // Newer local gesture wins; ties favour the explicit web
                // rename because the user typed it into the title field.
                if h_at > w_at {
                    Some(h_title)
                } else {
                    Some(w_title)
                }
            }
            (Some((w_title, _)), None) => Some(w_title),
            (None, Some((h_title, _))) => Some(h_title),
            (None, None) => None,
        };
        if title.is_none() {
            title = self
                .conn
                .query_row(
                    "SELECT title FROM imported_sessions WHERE session_id=?1",
                    [session_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
        }
        if title.is_none() {
            let mut statement = self
                .conn
                .prepare("SELECT message_json FROM messages WHERE session_id = ?1 ORDER BY seq")?;
            for json in statement.query_map(params![session_id], |r| r.get::<_, String>(0))? {
                let Ok(message) = serde_json::from_str::<Message>(&json?) else {
                    continue;
                };
                if message.role == conversations_core::Role::User {
                    title = super::subject::readable_title(&message.text);
                    if title.is_some() {
                        break;
                    }
                }
            }
        }
        Ok(Summary {
            messages: messages as u64,
            title,
            last_updated: last,
        })
    }

    pub(super) fn local_title_with_updated_at(
        &self,
        session_id: &str,
    ) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT title, updated_at FROM local_titles WHERE session_id=?1",
                [session_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?)
    }

    pub(super) fn host_title_with_updated_at(
        &self,
        session_id: &str,
    ) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT title, updated_at FROM host_titles WHERE session_id=?1",
                [session_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?)
    }

    /// The host-sourced effective title for `session_id`, if one was recorded
    /// by [`History::set_host_title`]. Private to this machine — the central
    /// server never sees it. Only the local-host daemon tests inspect this
    /// directly; production callers go through [`History::summary`].
    #[cfg(test)]
    pub fn host_title(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT title FROM host_titles WHERE session_id=?1",
                [session_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Record the host-sourced effective title for `session_id`. The cmux
    /// rename sync calls this when it observes a workspace `custom_title` for
    /// a registered conversation; the private channel's `SessionsList` then
    /// surfaces it as the session's `title` unless an explicit web rename
    /// with a newer `updated_at` outranks it. An empty / whitespace-only
    /// title clears the record so `imported_sessions.title` (if present) or
    /// the first user message takes over. Never leaves this host.
    pub fn set_host_title(&self, session_id: &str, title: &str) -> Result<()> {
        ensure!(
            super::authz::envelope::valid_session_id(session_id),
            "invalid session id"
        );
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return self.clear_host_title(session_id);
        }
        self.conn.execute(
            "INSERT INTO host_titles(session_id,title,updated_at) VALUES(?1,?2,
                MAX(?3, COALESCE((SELECT updated_at + 1 FROM local_titles WHERE session_id=?1), 0),
                        COALESCE((SELECT updated_at + 1 FROM host_titles WHERE session_id=?1), 0)))
             ON CONFLICT(session_id) DO UPDATE SET title=excluded.title, updated_at=excluded.updated_at",
            params![session_id, trimmed, now_secs()],
        )?;
        Ok(())
    }

    /// Drop the host-sourced effective title for `session_id`. Called when a
    /// cmux workspace loses its `custom_title` (back to a generated name).
    pub fn clear_host_title(&self, session_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM host_titles WHERE session_id=?1", [session_id])?;
        Ok(())
    }

    /// Store an explicit web-sourced title for `session_id` without touching
    /// transcript or import provenance. An empty / whitespace-only title
    /// clears the record so `imported_sessions.title` (if present) or the
    /// first user message resurfaces — and, if the user then types a new
    /// rename, its fresh `updated_at` will outrank any cmux sidebar title
    /// that was written in between. The caller must first authorize an
    /// existing canonical local session.
    pub fn rename(&self, session: &str, title: &str) -> Result<String> {
        ensure!(
            super::authz::envelope::valid_session_id(session),
            "invalid session id"
        );
        let title = validate_title(title)?;
        match title {
            Some(title) => self.conn.execute(
                "INSERT INTO local_titles(session_id,title,updated_at) VALUES(?1,?2,
                    MAX(?3, COALESCE((SELECT updated_at + 1 FROM local_titles WHERE session_id=?1), 0),
                            COALESCE((SELECT updated_at + 1 FROM host_titles WHERE session_id=?1), 0)))
                 ON CONFLICT(session_id) DO UPDATE SET title=excluded.title, updated_at=excluded.updated_at",
                params![session, title, now_secs()],
            )?,
            None => self.conn.execute(
                "DELETE FROM local_titles WHERE session_id=?1",
                [session],
            )?,
        };
        Ok(self
            .summary(session)?
            .title
            .unwrap_or_else(|| session.to_owned()))
    }

    pub fn import_metadata(&self, session: &str, title: &str, cwd: Option<&str>) -> Result<()> {
        self.conn.execute("INSERT INTO imported_sessions(session_id,title,remote_cwd) VALUES(?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET title=excluded.title, remote_cwd=excluded.remote_cwd", params![session, super::subject::readable_title(title), cwd])?;
        Ok(())
    }

    /// Full-text search. The user's words become quoted FTS5 phrases (all must
    /// match), so FTS operator syntax in the query is never interpreted and cannot
    /// raise a syntax error. `session` restricts the search to one session.
    pub fn search(&self, query: &str, session: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
        let Some(expression) = fts_expression(query) else {
            anyhow::bail!("search query has no searchable words");
        };
        let mut statement = self.conn.prepare(
            "SELECT f.session_id, f.uuid, m.seq, snippet(messages_fts, 0, '[', ']', '…', 12)
             FROM messages_fts f JOIN messages m ON m.session_id = f.session_id AND m.uuid = f.uuid
             WHERE messages_fts MATCH ?1 AND (?2 IS NULL OR f.session_id = ?2)
             ORDER BY rank LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                expression,
                session,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |r| {
                Ok(Hit {
                    session_id: r.get(0)?,
                    uuid: r.get(1)?,
                    seq: r.get::<_, i64>(2)? as u64,
                    snippet: r.get(3)?,
                })
            },
        )?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }
}

/// Shared validation for owner-local and signed title changes. Blank clears the override.
pub fn validate_title(title: &str) -> Result<Option<&str>> {
    let title = title.trim();
    if title.is_empty() {
        return Ok(None);
    }
    ensure!(
        !title.chars().any(char::is_control),
        "title must not contain control characters"
    );
    ensure!(
        title.chars().count() <= 200,
        "title must be at most 200 characters"
    );
    Ok(Some(title))
}

/// Turn free text into an FTS5 expression of AND-ed quoted phrases. Returns
/// `None` when there is nothing to search for.
fn fts_expression(query: &str) -> Option<String> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| format!("\"{word}\""))
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// Human-readable rendering of one stored message for the CLI.
pub fn render(message: &Message) -> String {
    use conversations_core::ContentBlock;
    let mut out = format!(
        "[{:?}] {}\n",
        message.role,
        message
            .timestamp
            .map(|t| t.to_rfc3339())
            .unwrap_or_default()
    );
    for block in &message.blocks {
        match block {
            ContentBlock::Text { text } => out.push_str(&format!("{text}\n")),
            ContentBlock::Thinking { thinking } => {
                out.push_str(&format!("(thinking) {thinking}\n"))
            }
            ContentBlock::ToolUse { name, input, .. } => {
                out.push_str(&format!("(tool call) {name} {input}\n"))
            }
            ContentBlock::ToolResult { content, .. } => {
                out.push_str(&format!("(tool result) {content}\n"))
            }
            ContentBlock::Other(value) => out.push_str(&format!("(other) {value}\n")),
        }
    }
    out
}

/// Text for `host history get`.
pub fn render_get(dir: &Path, session: &str, limit: usize, json: bool) -> Result<String> {
    let history = History::open(dir)?;
    let messages = history.messages(session, limit)?;
    anyhow::ensure!(
        !messages.is_empty(),
        "no local history for session {session}"
    );
    let mut out = String::new();
    for message in &messages {
        if json {
            out.push_str(&serde_json::to_string(message)?);
            out.push('\n');
        } else {
            out.push_str(&render(message));
            out.push('\n');
        }
    }
    Ok(out)
}

/// Text for `host history search`.
pub fn render_search(
    dir: &Path,
    query: &str,
    session: Option<&str>,
    limit: usize,
) -> Result<String> {
    let hits = History::open(dir)?.search(query, session, limit)?;
    if hits.is_empty() {
        return Ok("No matches.\n".into());
    }
    Ok(hits
        .iter()
        .map(|hit| format!("{}  {}  {}\n", hit.session_id, hit.uuid, hit.snippet))
        .collect())
}

/// `host history get`.
pub fn run_get(dir: &Path, session: &str, limit: usize, json: bool) -> Result<()> {
    print!("{}", render_get(dir, session, limit, json)?);
    Ok(())
}

/// `host history search`.
pub fn run_search(dir: &Path, query: &str, session: Option<&str>, limit: usize) -> Result<()> {
    print!("{}", render_search(dir, query, session, limit)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn rename_persists_for_native_and_imported_history_without_changing_content_or_cwd() {
        let dir = private_dir();
        for imported in [false, true] {
            let session = uuid::Uuid::new_v4().to_string();
            let mut history = History::open(dir.path()).unwrap();
            history.ingest(&session, &source(), &fixture()).unwrap();
            if imported {
                history
                    .import_metadata(&session, "Imported name", Some("/original/cwd"))
                    .unwrap();
            }
            let activity = history.summary(&session).unwrap().last_updated;
            assert_eq!(
                history.rename(&session, "  My local title  ").unwrap(),
                "My local title"
            );
            drop(history);
            let history = History::open(dir.path()).unwrap();
            assert_eq!(
                history.summary(&session).unwrap().title.as_deref(),
                Some("My local title")
            );
            assert_eq!(history.messages(&session, 10).unwrap(), fixture());
            assert_eq!(history.summary(&session).unwrap().last_updated, activity);
            let cwd: Option<String> = history
                .conn
                .query_row(
                    "SELECT remote_cwd FROM imported_sessions WHERE session_id=?1",
                    [&session],
                    |row| row.get(0),
                )
                .optional()
                .unwrap()
                .flatten();
            assert_eq!(cwd.as_deref(), imported.then_some("/original/cwd"));
            for invalid in ["bad\nname".to_owned(), "x".repeat(201)] {
                assert!(history.rename(&session, &invalid).is_err());
                assert_eq!(
                    history.summary(&session).unwrap().title.as_deref(),
                    Some("My local title")
                );
            }
            // Clearing the web rename falls back to the next available
            // title: the imported download title if the session was
            // imported with one, otherwise the transcript lead. Either way
            // the import provenance row (including `remote_cwd`) stays put.
            let transcript = fixture()[0].text.clone();
            let expected_after_clear = if imported {
                "Imported name".to_owned()
            } else {
                transcript
            };
            assert_eq!(
                history.rename(&session, "   ").unwrap(),
                expected_after_clear
            );
            let cwd: Option<String> = history
                .conn
                .query_row(
                    "SELECT remote_cwd FROM imported_sessions WHERE session_id=?1",
                    [&session],
                    |row| row.get(0),
                )
                .optional()
                .unwrap()
                .flatten();
            assert_eq!(cwd.as_deref(), imported.then_some("/original/cwd"));
            assert_eq!(history.messages(&session, 10).unwrap(), fixture());
        }
    }

    #[test]
    fn linear_metadata_is_recorded_atomically_with_new_history() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        history
            .ingest(
                "s",
                &source(),
                &claude_messages(&[user("one", "See https://linear.app/ws/issue/ENG-1/title")]),
            )
            .unwrap();
        let url: String = history
            .conn
            .query_row(
                "SELECT url FROM linear_refs WHERE session_id='s'",
                [],
                |row| row.get(0),
            )
            .expect("recorded Linear reference must be available without a full transcript scan");
        assert_eq!(url, "https://linear.app/ws/issue/ENG-1");
    }

    #[test]
    fn linear_metadata_migrates_backfills_once_and_keeps_unlink_tombstones() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        let messages =
            claude_messages(&[user("one", "See https://linear.app/ws/issue/ENG-1/title")]);
        history.ingest("s", &source(), &messages).unwrap();
        history
            .ingest(
                "other",
                &source(),
                &claude_messages(&[user("two", "https://linear.app/ws/issue/ENG-2")]),
            )
            .unwrap();
        history.conn.execute_batch("DROP TABLE IF EXISTS linear_refs; DROP TABLE IF EXISTS linear_backfilled; PRAGMA user_version=3;").unwrap();
        drop(history);
        let mut history = History::open(dir.path()).unwrap();
        let (docs, next) = history.linear_page("s", None, 32).unwrap();
        assert_eq!(
            docs.iter()
                .map(|doc| doc.identifier.as_str())
                .collect::<Vec<_>>(),
            ["ENG-1"]
        );
        assert_eq!(next, None);
        assert_eq!(
            history
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM linear_backfilled WHERE session_id='other'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        history.unlink_linear("s", &docs[0].url).unwrap();
        // A rewrite in the same second must populate metadata without a timestamp cache race.
        history
            .ingest(
                "s",
                &source(),
                &claude_messages(&[user(
                    "one",
                    "https://linear.app/ws/issue/ENG-1 https://linear.app/ws/project/new-project",
                )]),
            )
            .unwrap();
        let (docs, _) = history.linear_page("s", None, 32).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].identifier, "new-project");
        assert_eq!(history.messages("other", 10).unwrap().len(), 1);
        assert_eq!(
            history
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            6
        );
        let plan:String=history.conn.query_row("EXPLAIN QUERY PLAN SELECT COUNT(*),MAX(updated_at) FROM messages WHERE session_id='s'",[],|row|row.get(3)).unwrap();
        assert!(plan.contains("COVERING INDEX"));
    }

    #[test]
    fn old_writer_connection_invalidates_linear_cache_and_new_ingest_keeps_it_complete() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        history
            .ingest(
                "s",
                &source(),
                &claude_messages(&[user("one", "https://linear.app/ws/issue/ENG-1")]),
            )
            .unwrap();
        history.conn.execute_batch("DROP TRIGGER linear_insert_invalidates; DROP TRIGGER linear_update_invalidates; DROP TABLE linear_refs; DROP TABLE linear_backfilled; PRAGMA user_version=3;").unwrap();
        drop(history);
        // This connection represents an already-open schema3 tailer; it does not
        // reopen through the new History implementation after the migration.
        let mut old = Connection::open(dir.path().join("history.db")).unwrap();
        assert_eq!(
            old.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        let mut history = History::open(dir.path()).unwrap();
        let original = history.linear_page("s", None, 32).unwrap().0;
        history.unlink_linear("s", &original[0].url).unwrap();
        let changed = claude_messages(&[user(
            "one",
            "https://linear.app/ws/issue/ENG-1 https://linear.app/ws/issue/ENG-2",
        )])
        .remove(0);
        let json = serde_json::to_string(&changed).unwrap();
        let tx = old.transaction().unwrap();
        tx.execute(
            "UPDATE messages SET message_json=?1,sha256=?2 WHERE session_id='s' AND uuid='one'",
            params![
                json,
                digest::digest(&digest::SHA256, json.as_bytes()).as_ref()
            ],
        )
        .unwrap();
        tx.execute(
            "DELETE FROM messages_fts WHERE session_id='s' AND uuid='one'",
            [],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO messages_fts(text,session_id,uuid) VALUES(?1,'s','one')",
            [&changed.text],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(
            history
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM linear_backfilled WHERE session_id='s'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        let refs = history.linear_page("s", None, 32).unwrap().0;
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].identifier, "ENG-2");
        history
            .ingest(
                "s",
                &source(),
                &claude_messages(&[user("two", "https://linear.app/ws/issue/ENG-3")]),
            )
            .unwrap();
        assert_eq!(
            history
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM linear_backfilled WHERE session_id='s'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(history.linear_page("s", None, 32).unwrap().0.len(), 2);
    }

    #[test]
    fn linear_pages_budget_the_serialized_cursor_without_truncating_long_references() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        let urls = [
            format!("https://linear.app/ws/issue/{}", "A".repeat(4500)),
            format!("https://linear.app/ws/issue/{}", "B".repeat(8000)),
            format!("https://linear.app/ws/issue/{}", "C".repeat(8000)),
        ];
        history
            .ingest(
                "s",
                &source(),
                &claude_messages(&[user("one", &urls.join(" "))]),
            )
            .unwrap();
        let mut after = None;
        let mut found = Vec::new();
        loop {
            let (docs, next) = history.linear_page("s", after.as_deref(), 100).unwrap();
            let encoded =
                serde_json::to_vec(&conversations_core::private_channel::ReplyBody::Linear {
                    docs: docs.clone(),
                    next: next.clone(),
                })
                .unwrap();
            assert!(
                encoded.len() <= 32 * 1024,
                "cursor must count towards the reply budget: {}",
                encoded.len()
            );
            found.extend(docs.into_iter().map(|doc| doc.url));
            match next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
        assert_eq!(found, urls);
    }

    #[test]
    fn linear_backfill_uses_fts_and_warm_pages_never_read_transcript_tables() {
        unsafe extern "C" fn forbid_payload(
            context: *mut std::ffi::c_void,
            action: i32,
            table: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
        ) -> i32 {
            if action == rusqlite::ffi::SQLITE_READ && !table.is_null() {
                let table = unsafe { std::ffi::CStr::from_ptr(table) }.to_bytes();
                let fts = unsafe { *(context as *const bool) };
                if table == b"messages" || (fts && table.starts_with(b"messages_fts")) {
                    return rusqlite::ffi::SQLITE_DENY;
                }
            }
            rusqlite::ffi::SQLITE_OK
        }
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        let text = (0..20)
            .map(|n| format!("https://linear.app/ws/issue/{}-{n}", "A".repeat(1000)))
            .collect::<Vec<_>>()
            .join(" ");
        history
            .ingest("s", &source(), &claude_messages(&[user("one", &text)]))
            .unwrap();
        history.conn.execute("DELETE FROM linear_refs", []).unwrap();
        let mut deny_fts = false;
        unsafe {
            rusqlite::ffi::sqlite3_set_authorizer(
                history.conn.handle(),
                Some(forbid_payload),
                &mut deny_fts as *mut bool as *mut std::ffi::c_void,
            );
        }
        let (first, mut after) = history.linear_page("s", None, 100).unwrap();
        assert!(first.len() < 20);
        assert!(serde_json::to_vec(&first).unwrap().len() <= 32 * 1024);
        deny_fts = true;
        let mut count = first.len();
        while let Some(cursor) = after {
            let (page, next) = history.linear_page("s", Some(&cursor), 100).unwrap();
            assert!(!page.is_empty());
            count += page.len();
            after = next;
        }
        assert_eq!(count, 20);
        assert!(deny_fts);
        unsafe {
            rusqlite::ffi::sqlite3_set_authorizer(
                history.conn.handle(),
                None,
                std::ptr::null_mut(),
            );
        }
    }

    #[test]
    #[ignore = "explicit private-clone benchmark, never a runtime database"]
    fn linear_private_clone_benchmark() {
        let dir =
            std::env::var("CONVERSATIONS_LINEAR_BENCH_DIR").expect("clone directory required");
        assert!(
            dir.contains("linear-schema4-benchmark"),
            "only the designated clone may be migrated"
        );
        let before = std::time::Instant::now();
        let mut history = History::open(Path::new(&dir)).unwrap();
        let migration_ms = before.elapsed().as_secs_f64() * 1000.0;
        let (session,candidates):(String,i64)=history.conn.query_row(r#"SELECT session_id,count(*) FROM messages_fts WHERE messages_fts MATCH '"linear.app"' GROUP BY session_id ORDER BY count(*) DESC LIMIT 1"#,[],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
        let before = std::time::Instant::now();
        let (docs, _) = history.linear_page(&session, None, 100).unwrap();
        let first_ms = before.elapsed().as_secs_f64() * 1000.0;
        let before = std::time::Instant::now();
        let (again, _) = history.linear_page(&session, None, 100).unwrap();
        let cached_ms = before.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(docs, again);
        let plan:String=history.conn.query_row("EXPLAIN QUERY PLAN SELECT COUNT(*),MAX(updated_at) FROM messages WHERE session_id=?1",[&session],|row|row.get(3)).unwrap();
        assert!(plan.contains("COVERING INDEX"));
        let integrity: String = history
            .conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        let result = serde_json::json!({"migration_ms":migration_ms,"first_page_ms":first_ms,"cached_page_ms":cached_ms,"fts_candidate_messages":candidates,"page_refs":docs.len(),"covering_index":true,"quick_check":integrity});
        std::fs::write(
            Path::new(&dir).join("result.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
        println!("{result}");
    }

    #[test]
    fn v2_history_upgrade_lists_from_a_covering_index_and_preserves_messages() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        history.ingest("s", &source(), &fixture()).unwrap();
        history
            .conn
            .execute_batch("DROP INDEX IF EXISTS messages_activity; PRAGMA user_version=2;")
            .unwrap();
        drop(history);
        let mut history = History::open(dir.path()).unwrap();
        let plans: Vec<String> = history.conn.prepare("EXPLAIN QUERY PLAN SELECT COUNT(*), MAX(updated_at) FROM messages WHERE session_id = ?1").unwrap()
            .query_map(["s"], |row| row.get(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert!(
            plans.iter().any(|plan| plan.contains("COVERING INDEX")),
            "listing must not visit transcript payload pages: {plans:?}"
        );
        assert_eq!(history.messages("s", 10).unwrap(), fixture());
        assert_eq!(history.summary("s").unwrap().messages, 3);
        history
            .ingest(
                "s",
                &source(),
                &claude_messages(&[user("new", "New local turn")]),
            )
            .unwrap();
        assert_eq!(history.summary("s").unwrap().messages, 4);
    }

    #[test]
    fn imported_listing_uses_metadata_without_accessing_message_bodies() {
        unsafe extern "C" fn metadata_only(
            _: *mut std::ffi::c_void,
            action: std::ffi::c_int,
            _: *const std::ffi::c_char,
            column: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
            _: *const std::ffi::c_char,
        ) -> std::ffi::c_int {
            // SAFETY: SQLite supplies a valid NUL-terminated column name for READ.
            if action == rusqlite::ffi::SQLITE_READ
                && !column.is_null()
                && unsafe { std::ffi::CStr::from_ptr(column) }.to_bytes() == b"message_json"
            {
                rusqlite::ffi::SQLITE_DENY
            } else {
                rusqlite::ffi::SQLITE_OK
            }
        }
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        history.ingest("s", &source(), &fixture()).unwrap();
        history
            .import_metadata("s", "Saved conversation name", None)
            .unwrap();
        // Enforce a metadata-only reader at SQLite's actual column-access boundary.
        // A listing must still work when transcript bodies are unavailable to it.
        let result = unsafe {
            rusqlite::ffi::sqlite3_set_authorizer(
                history.conn.handle(),
                Some(metadata_only),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, rusqlite::ffi::SQLITE_OK);
        let summary = history.summary("s").unwrap();
        assert_eq!(summary.title.as_deref(), Some("Saved conversation name"));
        assert_eq!(summary.messages, 3);
        assert!(summary.last_updated.is_some());
    }

    #[test]
    fn imported_activity_keeps_original_time_until_a_local_turn_arrives() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        let mut imported = source();
        imported.origin = "explicit-remote-import".into();
        let messages = fixture();
        let original_time = messages
            .iter()
            .filter_map(|m| m.timestamp)
            .max()
            .unwrap()
            .timestamp();
        history.import_messages("s", &imported, &messages).unwrap();
        assert_eq!(
            history.summary("s").unwrap().last_updated,
            Some(original_time)
        );
        let before = now_secs();
        history
            .ingest(
                "s",
                &source(),
                &claude_messages(&[user("new-turn", "Continue locally")]),
            )
            .unwrap();
        let activity = history.summary("s").unwrap().last_updated.unwrap();
        assert!(activity >= before && activity <= now_secs());
        let mut undated = messages[0].clone();
        undated.timestamp = None;
        history
            .import_messages("undated", &imported, &[undated])
            .unwrap();
        let fallback = history.summary("undated").unwrap().last_updated.unwrap();
        assert!(fallback >= before && fallback <= now_secs());
    }

    #[test]
    fn initialized_history_reads_do_not_need_the_writer_lock() {
        let dir = private_dir();
        let mut writer = History::open(dir.path()).unwrap();
        writer.ingest("s1", &source(), &fixture()).unwrap();
        writer.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        let reader = History::open(dir.path())
            .expect("opening initialized history must stay read-only while import writes");
        assert_eq!(reader.messages("s1", 10).unwrap(), fixture());
        assert_eq!(reader.summary("s1").unwrap().messages, 3);
        assert_eq!(
            reader.search("credentials", Some("s1"), 10).unwrap().len(),
            1
        );
        writer.conn.execute_batch("ROLLBACK").unwrap();
    }

    /// The summary title tracks the user's **most recent local gesture**
    /// (cmux sidebar rename or explicit web rename), never a passive data
    /// source like the first user message or a downloaded import title.
    /// This test drives the full sequence of transitions the user can make
    /// on a real session — including the "I downloaded this session months
    /// ago under a stale title, then just renamed its workspace" case that
    /// motivates this PR.
    #[test]
    fn summary_follows_the_last_local_rename_and_leaves_imported_metadata_intact() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        let session = uuid::Uuid::new_v4().hyphenated().to_string();
        history
            .ingest(
                &session,
                &source(),
                &claude_messages(&[user("u1", "Original transcript lead")]),
            )
            .unwrap();
        // The session was once imported with a title ("Simplify" is what a
        // downloaded `/private` session on this laptop carries today). That
        // is download provenance, not an explicit rename.
        history
            .import_metadata(&session, "Simplify", Some("/original/cwd"))
            .unwrap();
        assert_eq!(
            history.summary(&session).unwrap().title.as_deref(),
            Some("Simplify"),
            "with no local gesture yet, the download title is the fallback",
        );
        // The user renames their cmux workspace to "Conversations". The
        // daemon's title loop records that as the host-sourced title. The
        // download title must step aside — otherwise `/private/<id>` keeps
        // showing "Simplify" forever.
        history
            .set_host_title(&session, "Conversations")
            .expect("record cmux rename");
        assert_eq!(
            history.summary(&session).unwrap().title.as_deref(),
            Some("Conversations"),
            "a newer cmux rename outranks the older imported provenance",
        );
        // The user then types a title in the web UI — a newer gesture than
        // the cmux rename. Age the cmux row by ten seconds so the comparison
        // is strict (same-second ties favour the explicit web rename
        // anyway, but a strict test is cheap).
        push_updated_at_back(&history, "host_titles", 10);
        history
            .rename(&session, "I renamed this in the UI")
            .expect("record web rename");
        assert_eq!(
            history.summary(&session).unwrap().title.as_deref(),
            Some("I renamed this in the UI"),
            "an explicit newer web rename must still work on top of a cmux title",
        );
        // The user renames the cmux workspace again — a newer gesture than
        // the web rename. Age the web row so the cmux write is strictly
        // newer.
        push_updated_at_back(&history, "local_titles", 10);
        history
            .set_host_title(&session, "Conversations again")
            .expect("record newer cmux rename");
        assert_eq!(
            history.summary(&session).unwrap().title.as_deref(),
            Some("Conversations again"),
            "a newer cmux rename outranks an older web rename",
        );
        // The user clears their web rename. The latest-local winner is now
        // the cmux title (which is still newer than the imported fallback).
        history.rename(&session, "   ").expect("clear web rename");
        assert_eq!(
            history.summary(&session).unwrap().title.as_deref(),
            Some("Conversations again"),
            "clearing the web rename falls back to the still-newer cmux title",
        );
        // The user clears the cmux sidebar title too. The imported download
        // provenance now resurfaces — we preserved it all along.
        history
            .set_host_title(&session, "   ")
            .expect("clear cmux title");
        assert_eq!(
            history.summary(&session).unwrap().title.as_deref(),
            Some("Simplify"),
            "with no local gesture left, the imported download title resurfaces",
        );
        // import_metadata still has the original download cwd — a local
        // rename gesture never touched the import provenance row.
        let cwd: Option<String> = history
            .conn
            .query_row(
                "SELECT remote_cwd FROM imported_sessions WHERE session_id=?1",
                [&session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cwd.as_deref(), Some("/original/cwd"));
        // Finally, drop the imported row entirely; only then does the
        // transcript fallback take over.
        history
            .conn
            .execute(
                "DELETE FROM imported_sessions WHERE session_id=?1",
                [&session],
            )
            .unwrap();
        assert_eq!(
            history.summary(&session).unwrap().title.as_deref(),
            Some("Original transcript lead"),
        );
    }

    #[test]
    fn successive_local_title_gestures_are_ordered_even_within_one_second() {
        let dir = private_dir();
        let history = History::open(dir.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        history.rename(&id, "First web title").unwrap();
        history.set_host_title(&id, "Newer sidebar title").unwrap();
        assert_eq!(
            history.summary(&id).unwrap().title.as_deref(),
            Some("Newer sidebar title")
        );
        history.rename(&id, "Newest web title").unwrap();
        assert_eq!(
            history.summary(&id).unwrap().title.as_deref(),
            Some("Newest web title")
        );
    }

    /// Push every row's `updated_at` in `table` back by `seconds` so the
    /// next gesture — which writes `now_secs()` — lands at a strictly
    /// newer timestamp. The production path cannot land two local gestures
    /// within the same clock second in a way the user would not notice, so
    /// using a wall-clock subtraction here keeps the test close to how
    /// gestures actually race in production.
    fn push_updated_at_back(history: &History, table: &str, seconds: i64) {
        history
            .conn
            .execute(
                &format!("UPDATE {table} SET updated_at = updated_at - ?1"),
                [seconds],
            )
            .unwrap();
    }

    #[test]
    fn summary_skips_harness_instructions() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        history.ingest("s", &source(), &claude_messages(&[
            user("a", "# AGENTS.md instructions for /workspace\n<INSTRUCTIONS>ignore</INSTRUCTIONS>"),
            user("b", "Fix   the database\nconnection"),
        ])).unwrap();
        assert_eq!(
            history.summary("s").unwrap().title.as_deref(),
            Some("Fix the database connection")
        );
    }

    #[test]
    fn agent_transcript_pr_urls_are_recorded_for_the_pr_panel() {
        // Mirrors the Tenant Route Operator screenshot: a private Claude
        // session whose transcript contains both platform and deploy PR URLs
        // (inline, markdown-wrapped, and inside serialized tool output) must
        // register every canonical URL under the session, so the Pull Requests
        // card can render them as auto-detected without a signed sync op.
        let dir = private_dir();
        let session = "tenant-route-operator";
        let mut history = History::open(dir.path()).unwrap();
        let messages = claude_messages(&[
            user(
                "u1",
                "Check platform#1095 — https://github.com/acme/platform/pull/1095\n\
                 also [platform#1078](https://github.com/acme/platform/pull/1078).",
            ),
            assistant(
                "a1",
                serde_json::json!([
                    {"type":"text","text":"Found pending platform PRs: **https://github.com/acme/platform/pull/1099** and https://github.com/acme/platform/pull/870 and https://github.com/acme/platform/pull/1075."},
                    {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"gh pr list"}}
                ]),
            ),
            serde_json::json!({"type":"user","uuid":"u2","sessionId":"s",
                "timestamp":"2026-11-02T10:00:02.000Z",
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1",
                    "content":"{\"url\":\"https://github.com/acme/deploy/pull/7162\",\"next\":\"https://github.com/acme/deploy/pull/7078\"}\n{\\\"deploy\\\":\\\"https://github.com/acme/deploy/pull/7071\\\"}"}]}}),
        ]);
        history.ingest(session, &source(), &messages).unwrap();
        let recorded = history.prs(session).unwrap();
        for expected in [
            "https://github.com/acme/platform/pull/1095",
            "https://github.com/acme/platform/pull/1078",
            "https://github.com/acme/platform/pull/1099",
            "https://github.com/acme/platform/pull/870",
            "https://github.com/acme/platform/pull/1075",
            "https://github.com/acme/deploy/pull/7162",
            "https://github.com/acme/deploy/pull/7078",
            "https://github.com/acme/deploy/pull/7071",
        ] {
            assert!(
                recorded.iter().any(|u| u == expected),
                "missing {expected} in {recorded:?}"
            );
        }
        // Only this session's URLs appear — a sibling session on the same host
        // must not inherit the detections.
        assert!(history.prs("other-session").unwrap().is_empty());
    }

    #[test]
    fn terminal_refs_are_durable_bounded_and_session_scoped() {
        let dir = private_dir();
        let history = History::open(dir.path()).unwrap();
        let urls: Vec<_> = (1..=120)
            .map(|n| format!("https://github.com/acme/repo/pull/{n}"))
            .collect();
        history.remember_prs("shell", &urls).unwrap();
        history.remember_prs("shell", &urls).unwrap();
        drop(history);
        let history = History::open(dir.path()).unwrap();
        assert_eq!(history.prs("shell").unwrap().len(), 100);
        assert!(history.prs("other").unwrap().is_empty());
        let projection = history.listed_prs("shell").unwrap();
        assert!(!projection.is_empty());
        assert!(serde_json::to_vec(&projection).unwrap().len() <= 514);
        history
            .remember_prs(
                "oversized",
                &[format!(
                    "https://github.com/{}/repo/pull/1",
                    "x".repeat(2000)
                )],
            )
            .unwrap();
        assert!(history.listed_prs("oversized").unwrap().is_empty());
    }

    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn source() -> Source {
        Source {
            agent: AgentKind::Claude,
            native_id: "native-1".into(),
            origin: "/projects/x/native-1.jsonl".into(),
        }
    }

    /// Messages as the production Claude parser produces them from real JSONL.
    fn claude_messages(lines: &[serde_json::Value]) -> Vec<Message> {
        lines
            .iter()
            .filter_map(conversations_core::parse_line)
            .collect()
    }

    fn user(uuid: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"type":"user","uuid":uuid,"sessionId":"s","timestamp":"2026-11-02T10:00:00.000Z",
            "message":{"role":"user","content":text}})
    }

    fn assistant(uuid: &str, blocks: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type":"assistant","uuid":uuid,"parentUuid":"u1","sessionId":"s",
            "timestamp":"2026-11-02T10:00:01.000Z",
            "message":{"role":"assistant","model":"claude-x","content":blocks,
                       "usage":{"input_tokens":10,"output_tokens":5}}})
    }

    fn fixture() -> Vec<Message> {
        claude_messages(&[
            user("u1", "How do I rotate the postgres credentials?"),
            assistant(
                "a1",
                serde_json::json!([
                    {"type":"thinking","thinking":"consider vault rotation"},
                    {"type":"text","text":"Rotate them with the vault database engine."},
                    {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"vault write -f database/rotate-root/pg"}}
                ]),
            ),
            serde_json::json!({"type":"user","uuid":"u2","sessionId":"s",
                "timestamp":"2026-11-02T10:00:02.000Z",
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"Success! rotated"}]}}),
        ])
    }

    /// Read one item completely, through fragments when it is not inline, and
    /// check the bytes against its hash.
    fn whole(history: &History, session: &str, item: &HistoryItem) -> Message {
        let bytes = match &item.message {
            Some(message) => serde_json::to_vec(message).unwrap(),
            None => {
                let (mut offset, mut bytes) = (Some(0u64), Vec::new());
                while let Some(at) = offset {
                    let piece = history.fragment(session, item.seq, at).unwrap().unwrap();
                    assert!(piece.data.len() <= MAX_FRAGMENT_BYTES);
                    assert_eq!((piece.size, &piece.sha256), (item.size, &item.sha256));
                    bytes.extend_from_slice(&piece.data);
                    offset = piece.next;
                }
                assert_eq!(
                    B64.encode(digest::digest(&digest::SHA256, &bytes)),
                    item.sha256
                );
                bytes
            }
        };
        assert_eq!(bytes.len() as u64, item.size);
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Pages are bounded by encoded size, following `next` visits every item
    /// exactly once in order, and every message comes back COMPLETE: large ones
    /// through hash-checked fragments, never cut.
    #[test]
    fn history_pages_are_bounded_and_every_message_is_read_back_losslessly() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        let control_rich: String = "\u{1}\"\\é漢🙂\n".repeat(3000);
        let mut lines: Vec<serde_json::Value> = (0..40)
            .map(|i| {
                let text = match i % 3 {
                    0 => control_rich.clone(),
                    1 => "漢".repeat(5000),
                    _ => format!("short {i}"),
                };
                user(&format!("u{i}"), &text)
            })
            .collect();
        lines.push(assistant(
            "a-big",
            serde_json::json!([
                {"type":"text","text":"x".repeat(70_000)},
                {"type":"tool_use","id":"t9","name":"Bash","input":{"command":"cat big.log"}}
            ]),
        ));
        let stored = claude_messages(&lines);
        history.ingest("s1", &source(), &stored).unwrap();
        let budget = 40 * 1024;
        let (mut after, mut seen) = (0u64, Vec::new());
        loop {
            let (items, next) = history.page("s1", after, 32, budget, false).unwrap();
            assert!(!items.is_empty());
            assert!(serde_json::to_vec(&items).unwrap().len() <= budget + 64);
            for item in &items {
                assert_eq!(
                    item.message.is_some(),
                    item.size as usize <= MAX_INLINE_MESSAGE
                );
                let message = whole(&history, "s1", item);
                assert_eq!(
                    serde_json::to_value(&message).unwrap(),
                    serde_json::to_value(&stored[seen.len()]).unwrap(),
                    "message {} is lossless",
                    item.seq
                );
                seen.push(item.seq);
            }
            match next {
                Some(cursor) => {
                    assert_eq!(cursor, *seen.last().unwrap());
                    after = cursor;
                }
                None => break,
            }
        }
        assert_eq!(seen, (1..=41).collect::<Vec<u64>>());
        assert!(history.fragment("s1", 41, 1 << 30).unwrap().is_none());
        assert!(history.fragment("other", 41, 0).unwrap().is_none());
        assert!(history
            .page("s1", 41, 32, budget, false)
            .unwrap()
            .0
            .is_empty());
        assert!(history.page("s1", u64::MAX, 32, budget, false).is_err());
        assert!(history
            .page("other", 0, 32, budget, false)
            .unwrap()
            .0
            .is_empty());
    }

    /// A message rewritten in place (a tool result arriving, a streamed turn
    /// finishing) keeps its seq, so `seq > last` alone would miss it. The digest
    /// page exposes the new hash at the same position, and re-reading that one
    /// position returns the new structured content.
    #[test]
    fn a_message_rewritten_in_place_is_found_by_its_digest_at_the_same_position() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        let first = claude_messages(&[
            user("u1", "run it"),
            assistant("a1", serde_json::json!([{"type":"text","text":"running"}])),
        ]);
        history.ingest("s1", &source(), &first).unwrap();
        let (before, _) = history.page("s1", 0, 256, 40 * 1024, true).unwrap();
        assert!(
            before.iter().all(|item| item.message.is_none()),
            "digest pages carry no content"
        );
        let updated = claude_messages(&[assistant(
            "a1",
            serde_json::json!([
                {"type":"text","text":"running"},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"make check"}}
            ]),
        )]);
        history.ingest("s1", &source(), &updated).unwrap();
        let (after, _) = history.page("s1", 0, 256, 40 * 1024, true).unwrap();
        assert_eq!(
            before.iter().map(|i| (i.seq, &i.uuid)).collect::<Vec<_>>(),
            after.iter().map(|i| (i.seq, &i.uuid)).collect::<Vec<_>>(),
            "order and positions are kept"
        );
        assert_eq!(before[0].sha256, after[0].sha256);
        assert_ne!(before[1].sha256, after[1].sha256, "the rewrite is visible");
        let (again, _) = history
            .page("s1", after[1].seq - 1, 1, 40 * 1024, false)
            .unwrap();
        let message = whole(&history, "s1", &again[0]);
        assert!(serde_json::to_string(&message)
            .unwrap()
            .contains("make check"));
        assert_eq!(again[0].sha256, after[1].sha256);
    }

    #[test]
    fn fts5_is_available_and_history_keeps_structured_blocks() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        let stats = history.ingest("s1", &source(), &fixture()).unwrap();
        assert_eq!(
            stats,
            Ingested {
                inserted: 3,
                updated: 0,
                unchanged: 0
            }
        );
        let stored = history.messages("s1", 10).unwrap();
        assert_eq!(stored, fixture(), "round-trips the full core::Message");
        let assistant = &stored[1];
        assert!(assistant.usage.is_some() && assistant.model.as_deref() == Some("claude-x"));
        assert!(
            matches!(assistant.blocks[2], conversations_core::ContentBlock::ToolUse { ref name, .. } if name == "Bash")
        );
    }

    #[test]
    fn search_finds_relevant_messages_and_nothing_across_sessions() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        history.ingest("s1", &source(), &fixture()).unwrap();
        history
            .ingest(
                "s2",
                &source(),
                &claude_messages(&[user("o1", "unrelated question about kubernetes")]),
            )
            .unwrap();
        let hits = history.search("rotate vault", Some("s1"), 10).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.uuid.as_str()).collect::<Vec<_>>(),
            ["a1"]
        );
        assert!(
            hits[0].snippet.contains('['),
            "highlighted: {}",
            hits[0].snippet
        );
        assert!(history.search("rotate", Some("s2"), 10).unwrap().is_empty());
        assert!(history
            .search("kubernetes", Some("s1"), 10)
            .unwrap()
            .is_empty());
        let everywhere = history.search("kubernetes", None, 10).unwrap();
        assert_eq!(everywhere[0].session_id, "s2");
        // tool results are searchable too
        assert_eq!(
            history.search("rotated", Some("s1"), 10).unwrap()[0].uuid,
            "u2"
        );
    }

    #[test]
    fn hostile_search_syntax_is_inert_and_empty_queries_are_refused() {
        let dir = private_dir();
        let mut history = History::open(dir.path()).unwrap();
        history.ingest("s1", &source(), &fixture()).unwrap();
        for query in [
            r#"rotate" OR "x"#,
            "rotate AND",
            "((( rotate",
            "NEAR(rotate vault)",
            "text:rotate*",
            "'; DROP TABLE messages; --",
        ] {
            let hits = history.search(query, Some("s1"), 10);
            assert!(hits.is_ok(), "{query}: {hits:?}");
        }
        // the operator words are plain words, not operators
        assert!(history
            .search("rotate OR kubernetes", Some("s1"), 10)
            .unwrap()
            .is_empty());
        assert!(history.search("   ", None, 10).is_err());
        assert!(history.search("!!!", None, 10).is_err());
        assert_eq!(history.messages("s1", 10).unwrap().len(), 3, "table intact");
        // an unknown session discloses nothing
        assert!(history.messages("nope", 10).unwrap().is_empty());
        assert!(history
            .search("rotate", Some("nope"), 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn replay_is_idempotent_and_a_changed_turn_updates_in_place_across_restarts() {
        let dir = private_dir();
        {
            let mut history = History::open(dir.path()).unwrap();
            history.ingest("s1", &source(), &fixture()).unwrap();
        }
        // restart + backfill of the same transcript
        let mut history = History::open(dir.path()).unwrap();
        let again = history.ingest("s1", &source(), &fixture()).unwrap();
        assert_eq!(
            again,
            Ingested {
                inserted: 0,
                updated: 0,
                unchanged: 3
            }
        );
        assert_eq!(
            history.search("rotate", Some("s1"), 10).unwrap().len(),
            2,
            "no duplicate index rows"
        );
        // a streamed assistant turn finishes with different text: same uuid, same position
        let mut finished = fixture();
        finished[1] = claude_messages(&[assistant(
            "a1",
            serde_json::json!([{"type":"text","text":"Final answer: use pgbouncer"}]),
        )])
        .remove(0);
        let stats = history.ingest("s1", &source(), &finished).unwrap();
        assert_eq!(
            stats,
            Ingested {
                inserted: 0,
                updated: 1,
                unchanged: 2
            }
        );
        assert!(history.search("pgbouncer", Some("s1"), 10).unwrap().len() == 1);
        assert!(
            history
                .search("vault database engine", Some("s1"), 10)
                .unwrap()
                .is_empty(),
            "stale text left the index"
        );
        let order: Vec<_> = history
            .messages("s1", 10)
            .unwrap()
            .into_iter()
            .map(|m| m.uuid)
            .collect();
        assert_eq!(order, ["u1", "a1", "u2"]);
        // a native file that shrank sends fewer messages: nothing is deleted
        history.ingest("s1", &source(), &finished[..1]).unwrap();
        assert_eq!(history.messages("s1", 10).unwrap().len(), 3);
    }

    #[test]
    fn database_is_private_and_unusable_directories_are_refused() {
        let dir = private_dir();
        History::open(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join("history.db"))
            .unwrap()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let open = tempfile::tempdir().unwrap();
        std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(History::open(open.path()).is_err());
    }

    #[test]
    fn cli_renderers_print_structured_history_and_search_results() {
        let dir = private_dir();
        History::open(dir.path())
            .unwrap()
            .ingest("s1", &source(), &fixture())
            .unwrap();
        let text = render_get(dir.path(), "s1", 10, false).unwrap();
        assert!(text.contains("(thinking) consider vault rotation"));
        assert!(text.contains("(tool call) Bash"));
        assert!(text.contains("(tool result) Success! rotated"));
        let json = render_get(dir.path(), "s1", 10, true).unwrap();
        let first: Message = serde_json::from_str(json.lines().next().unwrap()).unwrap();
        assert_eq!(first.uuid, "u1");
        assert!(render_get(dir.path(), "missing", 10, false).is_err());
        let found = render_search(dir.path(), "credentials", Some("s1"), 5).unwrap();
        assert!(found.starts_with("s1  u1  "), "{found}");
        assert_eq!(
            render_search(dir.path(), "kubernetes", None, 5).unwrap(),
            "No matches.\n"
        );
    }
}
