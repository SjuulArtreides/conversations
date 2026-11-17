//! Read-only adapter for OpenCode's SQLite-backed session store.
//!
//! OpenCode exposes the active database path through `opencode debug paths db`
//! (v2) or `opencode db path` (v1). The database is read directly because
//! `opencode db --format json` truncates output at 64 KiB, which corrupts
//! exports containing large tool or file parts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use conversations_core::{
    parse_opencode_export, AgentKind, Session, SessionFileRef, SessionSummary,
};
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde::Deserialize;
use serde_json::{json, Map, Value};

#[derive(Debug, Deserialize)]
struct IndexRow {
    id: String,
    title: String,
    directory: String,
    time_created: i64,
    time_updated: i64,
    message_count: i64,
}

#[derive(Debug, Deserialize)]
struct ExportRow {
    row_kind: String,
    row_id: String,
    parent_id: Option<String>,
    session_id: String,
    time_created: i64,
    time_updated: i64,
    data: String,
}

/// Commands that print the active database path, newest OpenCode first.
///
/// v2 moved `opencode db path` to `opencode debug paths db`. Neither version
/// rejects the other's arguments outright: both accept a trailing optional
/// positional, so the leading word of a wrong-version probe is absorbed as a
/// directory and the command answers with a usage dump instead of a path. Exit
/// status alone therefore cannot tell the versions apart, so a probe counts
/// only when it prints something path-shaped.
const DB_PATH_COMMANDS: [&[&str]; 2] = [&["debug", "paths", "db"], &["db", "path"]];

pub fn resolve_db_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return validate_db_path(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("OPENCODE_DB").filter(|value| !value.is_empty()) {
        return validate_db_path(PathBuf::from(path));
    }

    let mut rejected = Vec::new();
    for args in DB_PATH_COMMANDS {
        let output = Command::new("opencode")
            .args(args)
            .output()
            .with_context(|| {
                format!(
                    "running `opencode {}` (is OpenCode on PATH?)",
                    args.join(" ")
                )
            })?;
        match parse_db_path_output(&String::from_utf8_lossy(&output.stdout)) {
            // Existence is checked separately so a known-but-missing database
            // reports its path rather than looking like a failed lookup.
            Some(path) => return validate_db_path(path),
            None => rejected.push(format!("`opencode {}`", args.join(" "))),
        }
    }

    anyhow::bail!(
        "could not resolve the OpenCode database: {} did not print a path. \
         Set --opencode-db or OPENCODE_DB.",
        rejected.join(" and ")
    )
}

/// Accept stdout only when it is a single absolute path, which is what both
/// supported commands print on success and what neither prints on a usage dump.
fn parse_db_path_output(stdout: &str) -> Option<PathBuf> {
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let path = Path::new(lines.next()?.trim());
    (lines.next().is_none() && path.is_absolute()).then(|| path.to_path_buf())
}

fn validate_db_path(path: PathBuf) -> Result<PathBuf> {
    if !path.is_file() {
        anyhow::bail!("OpenCode database does not exist: {}", path.display());
    }
    Ok(path)
}

pub fn discover(db_path: &Path) -> Result<Vec<SessionFileRef>> {
    let mut rows: Vec<IndexRow> = Vec::new();
    for (sessions, messages, filter) in [
        (
            "session_v2",
            "session_message",
            " AND type IN ('user', 'assistant')",
        ),
        ("session", "message", ""),
    ] {
        if !table_exists(db_path, sessions)? {
            continue;
        }
        let sql = format!("SELECT id, COALESCE(title, '') AS title, directory, time_created, time_updated,
            (SELECT COUNT(*) FROM {messages} WHERE {messages}.session_id = {sessions}.id{filter}) AS message_count
            FROM {sessions}");
        rows.extend(query::<IndexRow>(db_path, &sql)?);
    }
    // V2 wins if a migration left the same native ID in both schemas.
    let mut seen = std::collections::HashSet::new();
    rows.retain(|row| seen.insert(row.id.clone()));
    rows.sort_by(|a, b| {
        b.time_updated
            .cmp(&a.time_updated)
            .then_with(|| a.id.cmp(&b.id))
    });
    let size_bytes = std::fs::metadata(db_path)
        .map(|meta| meta.len())
        .unwrap_or(0);

    Ok(rows
        .into_iter()
        .map(|row| {
            let started_at = timestamp(row.time_created);
            let ended_at = timestamp(row.time_updated);
            SessionFileRef {
                path: db_path.to_path_buf(),
                session_id: row.id.clone(),
                agent: AgentKind::OpenCode,
                project_dir: row.directory.clone(),
                size_bytes,
                modified: ended_at,
                summary: Some(SessionSummary {
                    id: row.id,
                    agent: AgentKind::OpenCode,
                    title: row.title,
                    project_path: Some(row.directory),
                    git_branch: None,
                    message_count: row.message_count.max(0) as usize,
                    started_at,
                    ended_at,
                }),
            }
        })
        .collect())
}

pub fn parse_session(reference: &SessionFileRef) -> Result<Session> {
    if table_exists(&reference.path, "session_v2")? {
        let found: Vec<Value> = query(
            &reference.path,
            &format!(
                "SELECT id FROM session_v2 WHERE id = '{}'",
                sql_string(&reference.session_id)
            ),
        )?;
        if !found.is_empty() {
            return parse_v2_session(reference);
        }
    }
    let session_id = sql_string(&reference.session_id);
    let sql = format!(
        "SELECT 'session' AS row_kind, id AS row_id, NULL AS parent_id, id AS session_id, \
         time_created, time_updated, \
         json_object('id', id, 'directory', directory, 'title', title, 'version', version, \
                     'time', json_object('created', time_created, 'updated', time_updated)) AS data \
         FROM session WHERE id = '{session_id}' \
         UNION ALL \
         SELECT 'message', id, NULL, session_id, time_created, time_updated, data \
         FROM message WHERE session_id = '{session_id}' \
         UNION ALL \
         SELECT 'part', id, message_id, session_id, time_created, time_updated, data \
         FROM part WHERE session_id = '{session_id}'"
    );
    let rows: Vec<ExportRow> = query(&reference.path, &sql)?;
    export_from_rows(&reference.session_id, rows)
        .and_then(|export| parse_opencode_export(&export).map_err(anyhow::Error::from))
}

fn table_exists(path: &Path, name: &str) -> Result<bool> {
    let rows: Vec<Value> = query(
        path,
        &format!(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '{}'",
            sql_string(name)
        ),
    )?;
    Ok(!rows.is_empty())
}

fn parse_v2_session(reference: &SessionFileRef) -> Result<Session> {
    #[derive(Deserialize)]
    struct MessageRow {
        id: String,
        #[serde(rename = "type")]
        kind: String,
        time_created: i64,
        data: String,
    }
    let id = sql_string(&reference.session_id);
    let mut headers: Vec<Value> = query(&reference.path, &format!(
        "SELECT id, directory, title, version, time_created, time_updated FROM session_v2 WHERE id = '{id}'"
    ))?;
    let mut info = headers.pop().context("OpenCode V2 session not found")?;
    info["time"] = json!({"created":info["time_created"], "updated":info["time_updated"]});
    let rows: Vec<MessageRow> = query(
        &reference.path,
        &format!(
            "SELECT id, type, time_created, data FROM session_message WHERE session_id = '{id}'
         AND type IN ('user', 'assistant') ORDER BY seq, id"
        ),
    )?;
    let mut messages = Vec::new();
    for row in rows {
        let mut data: Value = serde_json::from_str(&row.data)
            .with_context(|| format!("decoding OpenCode V2 message {}", row.id))?;
        let mut parts = if row.kind == "user" {
            let mut parts = Vec::new();
            if let Some(text) = data.get("text").and_then(Value::as_str) {
                parts.push(json!({"type":"text", "text":text}));
            }
            parts.extend(
                data.get("files")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            parts
        } else {
            data.get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        for part in &mut parts {
            if part["type"] == "tool" {
                part["tool"] = part["name"].clone();
                // V2 tool results contain content blocks instead of a string.
                let output = part["state"]["content"].as_array().map(|content| {
                    content
                        .iter()
                        .filter_map(|block| {
                            (block["type"] == "text")
                                .then(|| block["text"].as_str())
                                .flatten()
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                });
                if let Some(output) = output {
                    part["state"]["output"] = Value::String(output);
                }
            }
        }
        let model_id = data["model"]["id"].as_str().map(str::to_owned);
        let object = object_mut(&mut data);
        object.insert("id".to_string(), Value::String(row.id));
        object.insert("role".to_string(), Value::String(row.kind));
        object
            .entry("time".to_string())
            .or_insert_with(|| json!({"created":row.time_created}));
        if let Some(model) = model_id {
            object.insert("modelID".to_string(), Value::String(model));
        }
        messages.push(json!({"info":data, "parts":parts}));
    }
    parse_opencode_export(&json!({"info":info,"messages":messages})).map_err(anyhow::Error::from)
}

fn export_from_rows(session_id: &str, rows: Vec<ExportRow>) -> Result<Value> {
    let mut info = None;
    let mut message_order = Vec::new();
    let mut messages: HashMap<String, Value> = HashMap::new();
    let mut parts: HashMap<String, Vec<(i64, String, Value)>> = HashMap::new();

    for row in rows {
        let mut data: Value = serde_json::from_str(&row.data)
            .with_context(|| format!("decoding OpenCode {} row {}", row.row_kind, row.row_id))?;
        match row.row_kind.as_str() {
            "session" => info = Some(data),
            "message" => {
                enrich_row(
                    &mut data,
                    &row.row_id,
                    &row.session_id,
                    row.parent_id.as_deref(),
                    row.time_created,
                    row.time_updated,
                );
                message_order.push((row.time_created, row.row_id.clone()));
                messages.insert(row.row_id, data);
            }
            "part" => {
                let Some(message_id) = row.parent_id.as_deref() else {
                    continue;
                };
                enrich_part(&mut data, &row.row_id, &row.session_id, message_id);
                parts.entry(message_id.to_string()).or_default().push((
                    row.time_created,
                    row.row_id,
                    data,
                ));
            }
            _ => {}
        }
    }

    let info = info.ok_or_else(|| anyhow::anyhow!("OpenCode session not found: {session_id}"))?;
    message_order.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let messages = message_order
        .into_iter()
        .filter_map(|(_, id)| {
            let info = messages.remove(&id)?;
            let mut message_parts = parts.remove(&id).unwrap_or_default();
            message_parts.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            Some(json!({
                "info": info,
                "parts": message_parts
                    .into_iter()
                    .map(|(_, _, part)| part)
                    .collect::<Vec<_>>(),
            }))
        })
        .collect::<Vec<_>>();
    Ok(json!({ "info": info, "messages": messages }))
}

fn enrich_row(
    value: &mut Value,
    id: &str,
    session_id: &str,
    parent_id: Option<&str>,
    created: i64,
    updated: i64,
) {
    let object = object_mut(value);
    object.insert("id".to_string(), Value::String(id.to_string()));
    object.insert(
        "sessionID".to_string(),
        Value::String(session_id.to_string()),
    );
    if let Some(parent_id) = parent_id {
        object
            .entry("parentID".to_string())
            .or_insert_with(|| Value::String(parent_id.to_string()));
    }
    let time = object
        .entry("time".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let time = object_mut(time);
    time.entry("created".to_string())
        .or_insert(Value::from(created));
    time.entry("updated".to_string())
        .or_insert(Value::from(updated));
}

fn enrich_part(value: &mut Value, id: &str, session_id: &str, message_id: &str) {
    let object = object_mut(value);
    object.insert("id".to_string(), Value::String(id.to_string()));
    object.insert(
        "sessionID".to_string(),
        Value::String(session_id.to_string()),
    );
    object.insert(
        "messageID".to_string(),
        Value::String(message_id.to_string()),
    );
}

fn object_mut(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value
        .as_object_mut()
        .expect("value was replaced with object")
}

fn query<T: for<'de> Deserialize<'de>>(db_path: &Path, sql: &str) -> Result<Vec<T>> {
    let connection = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening OpenCode database read-only")?;
    let mut statement = connection
        .prepare(sql)
        .context("preparing OpenCode database query")?;
    let columns = statement
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut rows = statement.query([]).context("querying OpenCode database")?;
    let mut values = Vec::new();

    while let Some(row) = rows.next().context("reading OpenCode database row")? {
        let mut object = Map::new();
        for (index, name) in columns.iter().enumerate() {
            let value = match row.get_ref(index).context("reading OpenCode column")? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => Value::from(value),
                ValueRef::Real(value) => Value::from(value),
                ValueRef::Text(value) => Value::String(
                    std::str::from_utf8(value)
                        .context("OpenCode database contained non-UTF-8 text")?
                        .to_string(),
                ),
                ValueRef::Blob(_) => anyhow::bail!("OpenCode query returned an unsupported blob"),
            };
            object.insert(name.clone(), value);
        }
        values.push(
            serde_json::from_value(Value::Object(object))
                .context("decoding OpenCode database row")?,
        );
    }
    Ok(values)
}

fn sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

fn timestamp(millis: i64) -> Option<chrono::DateTime<Utc>> {
    Utc.timestamp_millis_opt(millis).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversations_core::ContentBlock;
    use rusqlite::params;

    #[test]
    fn v2_sessions_are_discovered_and_ingested_alongside_legacy_sessions() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let connection = Connection::open(file.path()).unwrap();
        connection.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, version TEXT, time_created INTEGER, time_updated INTEGER);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
            CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
            CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, title TEXT, version TEXT, time_created INTEGER, time_updated INTEGER);
            CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT);
            INSERT INTO session VALUES ('legacy', '/repo', 'Old session', '1.18', 1, 2);
            INSERT INTO session_v2 VALUES ('native', '/repo', 'Timeout investigation', '2.0', 3, 4);").unwrap();
        let records = [
            ("user", json!({"text":"Fix readiness","files":[]})),
            (
                "assistant",
                json!({"time":{"created":3,"completed":4},"model":{"id":"gpt-6.1-sol"},"content":[
                    {"type":"text","text":"Pushed schema (https://github.com/Example-Org/example-platform/pull/986)."},
                    {"type":"tool","id":"call_1","name":"shell","state":{"status":"completed","input":{"command":"gh pr create"},"content":[{"type":"text","text":"tool output"}]}},
                    {"type":"reasoning","text":"private reasoning"}
                ]}),
            ),
            ("idle", json!({"outcome":"succeeded"})),
        ];
        for (seq, (kind, data)) in records.into_iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO session_message VALUES (?1, 'native', ?2, ?3, 3, 4, ?4)",
                    params![format!("msg_{seq}"), kind, seq as i64, data.to_string()],
                )
                .unwrap();
        }
        let refs = discover(file.path()).unwrap();
        assert_eq!(refs.len(), 2);
        let reference = refs
            .iter()
            .find(|r| r.session_id == "native")
            .expect("V2 native binding must resolve");
        assert_eq!(reference.summary.as_ref().unwrap().message_count, 2);
        let session = parse_session(reference).unwrap();
        assert_eq!(session.title, "Timeout investigation");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].text, "Fix readiness");
        let response = &session.messages[1];
        assert_eq!(response.uuid, "msg_1");
        assert_eq!(response.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(response.raw["info"]["time"]["completed"], 4);
        assert!(
            matches!(&response.blocks[0], ContentBlock::Text { text } if text.contains("/pull/986"))
        );
        assert!(
            matches!(&response.blocks[1], ContentBlock::ToolUse { name, .. } if name == "shell")
        );
        assert!(
            matches!(&response.blocks[2], ContentBlock::ToolResult { content, .. } if content == "tool output")
        );
        assert!(matches!(&response.blocks[3], ContentBlock::Thinking { .. }));
        for malformed in [
            json!("unexpected"),
            json!(42),
            json!(true),
            json!([]),
            Value::Null,
        ] {
            connection.execute(
                "INSERT OR REPLACE INTO session_message VALUES ('malformed', 'native', 'assistant', 4, 3, 4, ?1)",
                [malformed.to_string()],
            ).unwrap();
            let parsed = parse_session(reference).unwrap();
            assert_eq!(parsed.messages.len(), 2);
            assert_eq!(parsed.skipped_lines, 1);
        }
        connection
            .execute_batch("DROP TABLE part; DROP TABLE message; DROP TABLE session;")
            .unwrap();
        assert_eq!(discover(file.path()).unwrap().len(), 1);
        assert_eq!(parse_session(reference).unwrap().messages.len(), 2);
    }

    #[test]
    fn a_printed_path_is_accepted_and_a_usage_dump_falls_through() {
        assert_eq!(
            parse_db_path_output("/Users/me/.local/share/opencode/opencode.db\n"),
            Some(PathBuf::from("/Users/me/.local/share/opencode/opencode.db"))
        );

        // Verbatim stdout of `opencode db path` on v2, where `db` is absorbed
        // as the optional directory positional. Rejecting this is what lets the
        // v1 fallback probe run instead of resolving a bogus path.
        let usage = "DESCRIPTION\n  OpenCode command line interface\n\n\
                     USAGE\n  opencode <subcommand> [flags] [<directory>]\n";
        assert_eq!(parse_db_path_output(usage), None);
        assert_eq!(parse_db_path_output(""), None);
        assert_eq!(parse_db_path_output("relative/opencode.db\n"), None);
    }

    #[test]
    fn rows_reconstruct_the_public_export_shape() {
        let rows = vec![
            ExportRow {
                row_kind: "session".into(),
                row_id: "ses_1".into(),
                parent_id: None,
                session_id: "ses_1".into(),
                time_created: 1_700_000_000_000,
                time_updated: 1_700_000_001_000,
                data: r#"{"id":"ses_1","directory":"/repo","title":"Real title","version":"1.18.18","time":{"created":1700000000000,"updated":1700000001000}}"#.into(),
            },
            ExportRow {
                row_kind: "message".into(),
                row_id: "msg_1".into(),
                parent_id: None,
                session_id: "ses_1".into(),
                time_created: 1_700_000_000_100,
                time_updated: 1_700_000_000_100,
                data: r#"{"role":"user"}"#.into(),
            },
            ExportRow {
                row_kind: "part".into(),
                row_id: "prt_2".into(),
                parent_id: Some("msg_1".into()),
                session_id: "ses_1".into(),
                time_created: 1_700_000_000_300,
                time_updated: 1_700_000_000_300,
                data: r#"{"type":"text","text":"hello"}"#.into(),
            },
            // Deliberately returned after the later part: SQLite row order is
            // unspecified without an ORDER BY, while OpenCode content order is
            // chronological with the row id as a stable tie-breaker.
            ExportRow {
                row_kind: "part".into(),
                row_id: "prt_1".into(),
                parent_id: Some("msg_1".into()),
                session_id: "ses_1".into(),
                time_created: 1_700_000_000_200,
                time_updated: 1_700_000_000_200,
                data: r#"{"type":"text","text":"first "}"#.into(),
            },
        ];

        let export = export_from_rows("ses_1", rows).unwrap();
        let session = parse_opencode_export(&export).unwrap();
        assert_eq!(session.agent, AgentKind::OpenCode);
        assert_eq!(session.title, "Real title");
        assert_eq!(session.message_count(), 1);
        assert_eq!(session.messages[0].uuid, "msg_1");
        assert_eq!(session.messages[0].text, "first \nhello");
    }

    #[test]
    fn sql_ids_are_quoted_without_changing_normal_ids() {
        assert_eq!(sql_string("ses_abc"), "ses_abc");
        assert_eq!(sql_string("ses_'abc"), "ses_''abc");
    }

    #[test]
    fn parses_session_parts_larger_than_opencode_cli_output_limit() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let connection = Connection::open(file.path()).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY, directory TEXT, title TEXT, version TEXT,
                    time_created INTEGER, time_updated INTEGER
                );
                CREATE TABLE message (
                    id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
                    time_updated INTEGER, data TEXT
                );
                CREATE TABLE part (
                    id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                    time_created INTEGER, time_updated INTEGER, data TEXT
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session VALUES (?1, '/repo', 'Large session', '1.0', 1, 2)",
                ["ses_large"],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES ('msg_1', ?1, 1, 2, '{\"role\":\"user\"}')",
                ["ses_large"],
            )
            .unwrap();
        let large_url = format!("data:image/png;base64,{}", "a".repeat(70_000));
        let part = json!({
            "type": "file",
            "mime": "image/png",
            "filename": "image.png",
            "url": large_url,
        });
        connection
            .execute(
                "INSERT INTO part VALUES ('prt_1', 'msg_1', ?1, 1, 2, ?2)",
                params!["ses_large", part.to_string()],
            )
            .unwrap();
        drop(connection);

        let reference = SessionFileRef {
            path: file.path().to_path_buf(),
            session_id: "ses_large".into(),
            agent: AgentKind::OpenCode,
            project_dir: "/repo".into(),
            size_bytes: 0,
            modified: None,
            summary: None,
        };
        let session = parse_session(&reference).unwrap();

        assert_eq!(session.title, "Large session");
        assert_eq!(session.message_count(), 1);
        assert_eq!(session.messages[0].blocks.len(), 1);
        let ContentBlock::Other(part) = &session.messages[0].blocks[0] else {
            panic!("expected the file part to remain visible as other content");
        };
        assert_eq!(part["url"], large_url);
    }
}
