//! Host-owned PR snapshots. Reads never invoke GitHub or the central server.
use std::path::Path;

use anyhow::{ensure, Context, Result};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use ring::digest;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::authz::{self, store::open_private};
use super::history::History;
use crate::commands::gh;
use std::collections::HashSet;

const CHUNK: usize = 24 * 1024;
fn open(dir: &Path) -> Result<Connection> {
    let conn = open_private(dir, "pr-cache.db")?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS cache(
        session TEXT NOT NULL, owner TEXT NOT NULL, repo TEXT NOT NULL, number INTEGER NOT NULL,
        owner_recorded INTEGER NOT NULL DEFAULT 0, unlinked INTEGER NOT NULL DEFAULT 0, status TEXT, comments TEXT NOT NULL DEFAULT '[]', fetched_at TEXT,
        PRIMARY KEY(session,owner,repo,number));
        CREATE TABLE IF NOT EXISTS snapshots(session TEXT PRIMARY KEY, scope TEXT NOT NULL, body BLOB NOT NULL, sha256 TEXT NOT NULL);")?;
    Ok(conn)
}

#[allow(clippy::too_many_arguments)]
pub fn save(
    dir: &Path,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
    status: Option<&gh::PrStatus>,
    comments: &[gh::PrComment],
    owner_recorded: bool,
) -> Result<()> {
    let mut conn = open(dir)?;
    let status = status.map(|s| json!({"state":s.state,"merged":s.merged,"draft":s.draft,"title":s.title,"review_decision":s.review_decision}).to_string());
    let now = chrono::Utc::now().to_rfc3339();
    let tx = conn.transaction()?;
    tx.execute("INSERT INTO cache(session,owner,repo,number,owner_recorded,status,comments,fetched_at)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(session,owner,repo,number) DO UPDATE SET
        unlinked=0,owner_recorded=MAX(cache.owner_recorded,excluded.owner_recorded),status=COALESCE(excluded.status,cache.status),
        comments=excluded.comments,fetched_at=excluded.fetched_at",
        params![session,owner,repo,number,owner_recorded,status,serde_json::to_string(comments)?,now])?;
    tx.execute("DELETE FROM snapshots WHERE session=?1", [session])?;
    tx.commit()?;
    snapshot(dir, session)?;
    Ok(())
}

/// The scope is cheap to read on every chunk. A changed grant/recorded target
/// rebuilds and persists the snapshot once; unchanged chunks reuse its digest.
fn snapshot(dir: &Path, session: &str) -> Result<()> {
    let mut targets = authz::pr_targets(dir, session)?;
    let authoritative: HashSet<(String, String, i64)> = targets.iter().cloned().collect();
    let mut db = open(dir)?;
    let conn = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let owner_targets = conn.prepare("SELECT owner,repo,number FROM cache WHERE session=?1 AND owner_recorded=1 AND unlinked=0")?
        .query_map([session],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut manual: HashSet<(String, String, i64)> = authoritative;
    manual.extend(owner_targets.iter().cloned());
    targets.extend(owner_targets);
    // Agent-emitted PR URLs discovered in the transcript — surfaced as
    // auto-detected so a private session's Pull Requests card is never empty
    // when the agent itself has already referenced the PR. Full URLs only, from
    // the shared text scanner — no bare `#N` inference.
    let mut detected: HashSet<(String, String, i64)> = HashSet::new();
    if let Ok(history) = History::open(dir) {
        if let Ok(urls) = history.prs(session) {
            for url in urls {
                if let Some(target) = conversations_core::terminal_prs::parse_canonical(&url) {
                    detected.insert(target.clone());
                    targets.push(target);
                }
            }
        }
    }
    targets.sort();
    targets.dedup();
    let mut visible = Vec::new();
    for target in targets {
        let unlinked:Option<bool>=conn.query_row("SELECT unlinked FROM cache WHERE session=?1 AND owner=?2 AND repo=?3 AND number=?4",params![session,target.0,target.1,target.2],|r|r.get(0)).optional()?;
        if unlinked != Some(true) {
            visible.push(target);
        }
    }
    let targets = visible;
    let scope = serde_json::to_string(&targets)?;
    if conn
        .query_row(
            "SELECT 1 FROM snapshots WHERE session=?1 AND scope=?2",
            params![session, scope],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some()
    {
        return Ok(());
    }
    let mut prs = Vec::new();
    let mut comments = Vec::new();
    for (owner, repo, number) in targets {
        let row: Option<(Option<String>,String,Option<String>)> = conn.query_row(
            "SELECT status,comments,fetched_at FROM cache WHERE session=?1 AND owner=?2 AND repo=?3 AND number=?4",
            params![session,owner,repo,number],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let (status, rows, fetched_at) = row.unwrap_or((None, "[]".into(), None));
        let status = status
            .map(|value| serde_json::from_str::<Value>(&value))
            .transpose()?;
        let auto = detected.contains(&(owner.clone(), repo.clone(), number))
            && !manual.contains(&(owner.clone(), repo.clone(), number));
        prs.push(json!({"url":format!("https://github.com/{owner}/{repo}/pull/{number}"),"owner":owner,"repo":repo,"number":number,
            "status":status,"status_fetched_at":fetched_at,"detected":auto}));
        for mut comment in serde_json::from_str::<Vec<Value>>(&rows)? {
            let obj = comment
                .as_object_mut()
                .context("invalid cached PR comment")?;
            obj.insert("owner".into(), json!(owner));
            obj.insert("repo".into(), json!(repo));
            obj.insert("number".into(), json!(number));
            obj.insert("ingested_at".into(), json!(fetched_at));
            comments.push(comment);
        }
    }
    comments.sort_by(|a, b| {
        a["created_at"]
            .as_str()
            .cmp(&b["created_at"].as_str())
            .then_with(|| a["comment_id"].as_str().cmp(&b["comment_id"].as_str()))
    });
    let body = serde_json::to_vec(&json!({"prs":prs,"comments":comments}))?;
    let sha = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, &body).as_ref());
    conn.execute("INSERT INTO snapshots(session,scope,body,sha256) VALUES(?1,?2,?3,?4)
        ON CONFLICT(session) DO UPDATE SET scope=excluded.scope,body=excluded.body,sha256=excluded.sha256",params![session,scope,body,sha])?;
    conn.commit()?;
    Ok(())
}

pub fn chunk(
    dir: &Path,
    session: &str,
    offset: u64,
    expected: Option<&str>,
) -> Result<(String, String, Option<u64>)> {
    snapshot(dir, session)?;
    let conn = open(dir)?;
    let (len, sha, bytes): (u64, String, Vec<u8>) = conn.query_row(
        "SELECT length(body),sha256,substr(body,?2+1,?3) FROM snapshots WHERE session=?1",
        params![session, i64::try_from(offset)?, CHUNK],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(
        expected.is_none_or(|expected| expected == sha),
        "PR cache changed; restart from offset 0"
    );
    ensure!(
        offset == 0 || expected.is_some(),
        "PR cache digest required after first chunk"
    );
    ensure!(offset <= len, "invalid PR cache offset");
    let end = offset + bytes.len() as u64;
    Ok((STANDARD.encode(bytes), sha, (end < len).then_some(end)))
}

/// One canonical hash for PR metadata: given an already-shaped
/// `{"status": ..., "comments": [...]}` JSON value, returns the URL-safe
/// base64 of its SHA-256. Called by both the fetch-time path in `github.rs`
/// (which builds the shape from live `gh` output) and by [`local_fingerprint`]
/// (which builds the same shape from the SQLite cache), so a re-read of
/// unchanged data always produces the same hash as the original fetch.
pub fn pr_fingerprint_hash(canonical: &Value) -> String {
    URL_SAFE_NO_PAD
        .encode(digest::digest(&digest::SHA256, canonical.to_string().as_bytes()).as_ref())
}

/// Metadata-only unlink. Explicit sync clears it; polling never does.
pub fn unlink(dir: &Path, session: &str, owner: &str, repo: &str, number: i64) -> Result<()> {
    let _lock = super::github::PrSyncLock::acquire(dir, session, owner, repo, number, true)?;
    let mut conn = open(dir)?;
    let known = conn
        .query_row(
            "SELECT 1 FROM cache WHERE session=?1 AND owner=?2 AND repo=?3 AND number=?4",
            params![session, owner, repo, number],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some()
        || authz::pr_targets(dir, session)?.contains(&(owner.into(), repo.into(), number));
    ensure!(known, "Unknown recorded PR target");
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO cache(session,owner,repo,number,unlinked) VALUES(?1,?2,?3,?4,1)
        ON CONFLICT(session,owner,repo,number) DO UPDATE SET unlinked=1",
        params![session, owner, repo, number],
    )?;
    tx.execute("DELETE FROM snapshots WHERE session=?1", [session])?;
    tx.commit()?;
    Ok(())
}
/// SHA-256 of the meaningful PR metadata (status + comments, sorted) saved
/// locally for this target. Deliberately omits `fetched_at` so a successful
/// re-read with unchanged content produces the same fingerprint — the host
/// poll uses "fingerprint differs from last pass" to decide reset-vs-grow
/// backoff, and a timestamp that moves on every read would reset forever.
/// `None` when nothing has been saved yet (e.g. the first poll pass).
pub fn local_fingerprint(
    dir: &Path,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<Option<String>> {
    let conn = open(dir)?;
    let row: Option<(Option<String>, String)> = conn
        .query_row(
            "SELECT status, comments FROM cache
             WHERE session=?1 AND owner=?2 AND repo=?3 AND number=?4 AND unlinked=0",
            params![session, owner, repo, number],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((status_json, comments_json)) = row else {
        return Ok(None);
    };
    let status: Value = status_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or(Value::Null);
    Ok(Some(metadata_fingerprint(
        status,
        serde_json::from_str(&comments_json)?,
    )))
}

/// The same canonical metadata shape for freshly fetched and persisted snapshots.
pub(super) fn metadata_fingerprint(status: Value, comments: Value) -> String {
    let mut comments = comments.as_array().cloned().unwrap_or_default();
    comments.sort_by(|a, b| {
        let ka = a.get("comment_id").and_then(Value::as_str).unwrap_or("");
        let kb = b.get("comment_id").and_then(Value::as_str).unwrap_or("");
        ka.cmp(kb).then_with(|| {
            let ca = a.get("created_at").and_then(Value::as_str).unwrap_or("");
            let cb = b.get("created_at").and_then(Value::as_str).unwrap_or("");
            ca.cmp(cb)
        })
    });
    let shaped: Vec<Value> = comments
        .into_iter()
        .map(|c| {
            json!({
                "id": c.get("comment_id").cloned().unwrap_or(Value::Null),
                "kind": c.get("kind").cloned().unwrap_or(Value::Null),
                "body": c.get("body").cloned().unwrap_or(Value::Null),
                "created_at": c.get("created_at").cloned().unwrap_or(Value::Null),
                "resolved": c.get("resolved").cloned().unwrap_or(Value::Null),
                "line": c.get("line").cloned().unwrap_or(Value::Null),
                "path": c.get("path").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    let canonical = json!({ "status": status, "comments": shaped });
    pr_fingerprint_hash(&canonical)
}

pub fn is_unlinked(
    dir: &Path,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<bool> {
    Ok(open(dir)?
        .query_row(
            "SELECT unlinked FROM cache WHERE session=?1 AND owner=?2 AND repo=?3 AND number=?4",
            params![session, owner, repo, number],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use conversations_core::{AgentKind, Message};

    fn read_snapshot(dir: &Path, session: &str) -> Value {
        let (b64, _, _) = chunk(dir, session, 0, None).unwrap();
        serde_json::from_slice(&STANDARD.decode(b64).unwrap()).unwrap()
    }

    fn claude_messages(lines: &[Value]) -> Vec<Message> {
        lines
            .iter()
            .filter_map(conversations_core::parse_line)
            .collect()
    }

    fn history_source() -> super::super::history::Source {
        super::super::history::Source {
            agent: AgentKind::Claude,
            native_id: "native-1".into(),
            origin: "/projects/x/native-1.jsonl".into(),
        }
    }

    /// Regression for the "localhost /private fullscreen shows Linear but no PR
    /// panel" screenshot: a private Claude session whose transcript references
    /// GitHub PR URLs across platform and deploy must be visible in the Pull
    /// Requests card via `pr.cache.list`, without a signed `pr.sync` operation,
    /// and must be marked `detected: true` so the UI renders the auto-detected
    /// badge. Manual sync still wins on conflict; see the next test.
    #[test]
    fn transcript_pr_urls_surface_as_auto_detected_in_pr_cache_snapshot() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        authz::store::Store::open(dir.path()).unwrap();
        let session = "97485e40-8841-408b-bf6b-8bf008c39c15";

        let mut history = super::super::history::History::open(dir.path()).unwrap();
        let messages = claude_messages(&[
            serde_json::json!({"type":"user","uuid":"u1","sessionId":session,
                "timestamp":"2026-11-06T10:00:00.000Z",
                "message":{"role":"user","content":"Platform PRs: https://github.com/acme/platform/pull/1095, [platform#1078](https://github.com/acme/platform/pull/1078), **https://github.com/acme/platform/pull/1099**, https://github.com/acme/platform/pull/870, https://github.com/acme/platform/pull/1075."}}),
            serde_json::json!({"type":"user","uuid":"u2","parentUuid":"u1","sessionId":session,
                "timestamp":"2026-11-06T10:00:01.000Z",
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1",
                    "content":"{\"url\":\"https://github.com/acme/deploy/pull/7162\",\"next\":\"https://github.com/acme/deploy/pull/7078\"}\n{\\\"deploy\\\":\\\"https://github.com/acme/deploy/pull/7071\\\"}"}]}}),
        ]);
        history
            .ingest(session, &history_source(), &messages)
            .unwrap();
        drop(history);

        let value = read_snapshot(dir.path(), session);
        let prs = value["prs"].as_array().expect("prs array");
        let mut by_number: std::collections::HashMap<i64, &Value> =
            std::collections::HashMap::new();
        for pr in prs {
            by_number.insert(pr["number"].as_i64().unwrap(), pr);
        }
        for (owner, repo, number) in [
            ("acme", "platform", 1095),
            ("acme", "platform", 1078),
            ("acme", "platform", 1099),
            ("acme", "platform", 870),
            ("acme", "platform", 1075),
            ("acme", "deploy", 7162),
            ("acme", "deploy", 7078),
            ("acme", "deploy", 7071),
        ] {
            let pr = by_number
                .get(&number)
                .unwrap_or_else(|| panic!("missing PR #{number} in snapshot: {value:?}"));
            assert_eq!(pr["owner"], owner);
            assert_eq!(pr["repo"], repo);
            assert_eq!(
                pr["detected"], true,
                "{owner}/{repo}#{number} must be detected"
            );
            assert!(
                pr["status"].is_null(),
                "detected PRs have no status until sync"
            );
            assert_eq!(
                pr["url"],
                format!("https://github.com/{owner}/{repo}/pull/{number}")
            );
        }
        assert_eq!(prs.len(), 8, "no duplicates, nothing extra: {prs:?}");
    }

    /// An explicit `pr.sync` or a status refresh records the target under the
    /// signed authority and caches its status with `owner_recorded=1`. If the
    /// agent ALSO mentions the same PR in its transcript, the manual link wins:
    /// the snapshot entry shows the real status with `detected: false`, not an
    /// auto-detected badge on top of a manually-managed PR.
    #[test]
    fn manual_sync_wins_over_transcript_autodetect_for_the_same_pr() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        authz::store::Store::open(dir.path()).unwrap();
        let session = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

        let mut history = super::super::history::History::open(dir.path()).unwrap();
        history
            .ingest(
                session,
                &history_source(),
                &claude_messages(&[serde_json::json!({"type":"user","uuid":"u1","sessionId":session,
                    "timestamp":"2026-11-06T10:00:00.000Z",
                    "message":{"role":"user","content":"See https://github.com/acme/platform/pull/1095"}})]),
            )
            .unwrap();
        drop(history);

        let status = gh::PrStatus {
            state: "OPEN".into(),
            merged: false,
            draft: false,
            title: "Platform fix".into(),
            review_decision: None,
        };
        save(
            dir.path(),
            session,
            "acme",
            "platform",
            1095,
            Some(&status),
            &[],
            true,
        )
        .unwrap();

        let value = read_snapshot(dir.path(), session);
        let prs = value["prs"].as_array().unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0]["number"], 1095);
        assert_eq!(prs[0]["detected"], false, "manual link wins: {:?}", prs[0]);
        assert_eq!(prs[0]["status"]["title"], "Platform fix");
    }

    #[test]
    fn snapshot_chunks_preserve_large_unicode_comments_and_detect_changes_and_unlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        authz::store::Store::open(dir.path()).unwrap();
        let session = "00000000-0000-0000-0000-000000000001";
        let comment = gh::PrComment {
            body: "日本語👋".repeat(9000),
            comment_id: "issue:1".into(),
            created_at: "2026-11-05T10:00:00Z".into(),
            ..Default::default()
        };
        save(
            dir.path(),
            session,
            "octo",
            "repo",
            7,
            None,
            std::slice::from_ref(&comment),
            true,
        )
        .unwrap();
        let mut offset = 0;
        let mut hash = None;
        let mut bytes = Vec::new();
        loop {
            let (b64, sha, next) = chunk(dir.path(), session, offset, hash.as_deref()).unwrap();
            let data = STANDARD.decode(b64).unwrap();
            assert!(data.len() <= CHUNK);
            bytes.extend(data);
            hash = Some(sha);
            match next {
                Some(n) => offset = n,
                None => break,
            }
        }
        assert_eq!(
            hash.as_deref(),
            Some(
                URL_SAFE_NO_PAD
                    .encode(digest::digest(&digest::SHA256, &bytes))
                    .as_str()
            )
        );
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["comments"][0]["body"], comment.body);
        unlink(dir.path(), session, "octo", "repo", 7).unwrap();
        assert!(chunk(dir.path(), session, CHUNK as u64, hash.as_deref())
            .unwrap_err()
            .to_string()
            .contains("PR cache changed"));
        let (b64, _, _) = chunk(dir.path(), session, 0, None).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&STANDARD.decode(b64).unwrap()).unwrap(),
            json!({"prs":[],"comments":[]})
        );
        save(
            dir.path(),
            session,
            "octo",
            "repo",
            7,
            None,
            &[comment],
            true,
        )
        .unwrap();
        assert!(!is_unlinked(dir.path(), session, "octo", "repo", 7).unwrap());
    }
}
