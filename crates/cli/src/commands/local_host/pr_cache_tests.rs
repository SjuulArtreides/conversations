//! PR cache producers against a recording central server and a recording `gh`.
//!
//! Each test runs in a child process with an isolated HOME/XDG and a PATH that
//! holds only the recording `gh`, so no personal GitHub login, config or agent
//! is reachable. The central stand-in applies the server's source fence to
//! every push (exact `{rev,next_seq}` for a published session, no snapshot for
//! an unmanaged one) and records every request. The `gh` stand-in records its
//! argv and can hold the status read until the test releases it, so a test can
//! change the central or local state in the middle of a fetch.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::params;
use serde_json::{json, Value};

use super::authz::sessions::LocalSession;
use super::authz::store::{now_secs, Store};
use crate::client::ApiClient;

const SESSION: &str = "5f0c1d2e-3a4b-4c5d-8e6f-7a8b9c0d1e2f";
const OTHER: &str = "6a1b2c3d-4e5f-4a6b-9c7d-8e9f0a1b2c3d";

/// Re-run `name` with an isolated HOME/XDG and a PATH of one directory holding
/// only the recording `gh`. Returns true in the child, which runs the body.
fn isolated(name: &str) -> bool {
    crate::commands::local_host::test_support::isolated(
        &format!("commands::local_host::pr_cache_tests::{name}"),
        &[("gh", GH_STUB)],
    )
}

/// Records each call (one directory per call, one file per argument) and
/// answers the three reads the producers make. While `hold` exists, the status
/// read signals `reached` and waits for `release` (bounded to ~20 s).
const GH_STUB: &str = r#"#!/bin/sh
t="${0%/*}"
n=0
while [ -e "$t/calls/$n" ]; do n=$((n+1)); done
/bin/mkdir -p "$t/calls/$n"
i=0
for a in "$@"; do printf '%s' "$a" > "$t/calls/$n/$i"; i=$((i+1)); done
case "$*" in
  *"state,isDraft"*)
    if [ -e "$t/hold" ]; then
      : > "$t/reached"
      w=0
      while [ ! -e "$t/release" ] && [ $w -lt 400 ]; do /bin/sleep 0.05; w=$((w+1)); done
    fi
    printf '%s' '{"state":"OPEN","isDraft":false,"title":"Fix the build","mergedAt":null,"number":7}' ;;
  *"comments,reviews"*) printf '%s' '{"comments":[{"id":"IC_1","author":{"login":"rev"},"body":"private review text","url":"https://github.com/octo/repo/pull/7#issuecomment-1","createdAt":"2026-11-03T10:00:00Z"}],"reviews":[]}' ;;
  *graphql*) printf '%s' '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]}}}}}' ;;
esac
exit 0
"#;

fn tools() -> PathBuf {
    PathBuf::from(std::env::var_os("PATH").unwrap())
}

fn gh_calls() -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for n in 0.. {
        let call = tools().join("calls").join(n.to_string());
        if !call.exists() {
            break;
        }
        let mut args = Vec::new();
        while let Ok(text) = std::fs::read_to_string(call.join(args.len().to_string())) {
            args.push(text);
        }
        out.push(args);
    }
    out
}

/// Make the next status read wait for [`release_gh`].
fn hold_gh() {
    std::fs::write(tools().join("hold"), "").unwrap();
}

fn wait_for_held_gh() {
    for _ in 0..400 {
        if tools().join("reached").exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("gh status read never started");
}

fn release_gh() {
    std::fs::write(tools().join("release"), "").unwrap();
}

/// The central server as seen by a producer: the session's current
/// publication snapshot (`None` = unmanaged), the PR links it lists, and every
/// request it received as `(request line, JSON body)`.
struct Central {
    snapshot: Option<(u64, u64)>,
    prs: Value,
    log: Vec<(String, Value)>,
}

type Shared = Arc<Mutex<Central>>;

fn central(snapshot: Option<(u64, u64)>) -> (String, Shared) {
    use std::io::{Read, Write};
    let state = Arc::new(Mutex::new(Central {
        snapshot,
        prs: json!([
            {"owner": "octo", "repo": "repo", "number": 7, "url": "https://github.com/octo/repo/pull/7"},
            {"owner": "mallory", "repo": "evil", "number": 9, "url": "https://github.com/mallory/evil/pull/9"}
        ]),
        log: Vec::new(),
    }));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let shared = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let state = shared.clone();
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let length: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|v| v.trim().parse().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; length];
                let _ = stream.read_exact(&mut body);
                let line = head.lines().next().unwrap_or("").to_owned();
                let mut parts = line.split(' ');
                let (method, path) = (
                    parts.next().unwrap_or("").to_owned(),
                    parts.next().unwrap_or("").to_owned(),
                );
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let mut central = state.lock().unwrap();
                central.log.push((format!("{method} {path}"), body.clone()));
                let current = central
                    .snapshot
                    .map(|(rev, next_seq)| json!({"rev": rev, "next_seq": next_seq}));
                let (code, reply) = if method == "GET" && path.ends_with("/prs") {
                    (
                        "200 OK",
                        json!({"prs": central.prs, "publication": current}).to_string(),
                    )
                } else if method == "POST"
                    && (path.ends_with("/prs/status") || path.ends_with("/prs/comments"))
                {
                    // The server's fence: a published session takes only its exact
                    // snapshot; an unmanaged one only a revisionless push.
                    match (current, body.get("publication").filter(|v| !v.is_null())) {
                        (Some(now), Some(claimed)) if *claimed == now => {
                            ("204 No Content", String::new())
                        }
                        (None, None) => ("204 No Content", String::new()),
                        (Some(_), Some(_)) => {
                            ("409 Conflict", r#"{"error":"stale source"}"#.into())
                        }
                        _ => ("409 Conflict", r#"{"error":"managed"}"#.into()),
                    }
                } else {
                    ("404 Not Found", String::new())
                };
                drop(central);
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                );
            });
        }
    });
    (base, state)
}

fn requests(central: &Shared) -> Vec<(String, Value)> {
    central.lock().unwrap().log.clone()
}

fn pushes(central: &Shared) -> Vec<(String, Value)> {
    requests(central)
        .into_iter()
        .filter(|(line, _)| line.starts_with("POST "))
        .collect()
}

fn reads(central: &Shared) -> usize {
    requests(central)
        .iter()
        .filter(|(line, _)| line.starts_with("GET ") && line.ends_with("/prs"))
        .count()
}

/// The host directory `pr sync` and `host` use.
fn host_dir() -> PathBuf {
    let dir = dirs::home_dir().unwrap().join(".conversations-host-local");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A session this host runs (a canonical local record) with one PR target
/// recorded by a signed operation.
fn local_session(dir: &Path, session: &str) {
    let mut store = Store::open(dir).unwrap();
    store
        .insert_session(
            &LocalSession {
                session_id: session.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: "/work".into(),
                resume_id: None,
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
            "signed_start",
        )
        .unwrap();
    record_target(&mut store, session);
}

/// A PR target with no canonical local session behind it (an unmanaged,
/// legacy central session).
fn legacy_target(dir: &Path, session: &str) {
    record_target(&mut Store::open(dir).unwrap(), session);
}

/// Cache fixtures use an actual locally approved grant activation. Old targets
/// without provenance remain refused; composition must not weaken revocation.
fn record_target(store: &mut Store, session: &str) {
    use super::authz::envelope::Alg;
    use super::authz::store::{Grant, Kind};
    use ring::signature::{Ed25519KeyPair, KeyPair};
    let key = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let key = Ed25519KeyPair::from_pkcs8(key.as_ref()).unwrap();
    let kid = store
        .enroll(
            Kind::Owner,
            Alg::Ed25519,
            key.public_key().as_ref(),
            "cache fixture",
            &[Grant::parse(&format!("session.write:{session}")).unwrap()],
        )
        .unwrap();
    // PR refresh now follows session.write; the recording authority is
    // captured under that cap, so the poll re-checks live write access.
    let mut authority = super::authz::cutover::Authority {
        kid,
        dlg: None,
        session: session.into(),
        cap: "session.write".into(),
        epoch: None,
        exp: None,
    };
    authority.epoch = store.current_epoch(&authority, now_secs()).unwrap();
    assert!(authority.epoch.is_some());
    store
        .record_pr_target(session, "octo", "repo", 7, Some(&authority), now_secs())
        .unwrap();
}

/// The local publisher's intent for `session`, in its `history.db` table.
pub(super) fn set_local_publication(
    dir: &Path,
    session: &str,
    phase: &str,
    rev: u64,
    server: &str,
) {
    // As the publisher does: the history schema (and owner-only file) first.
    drop(super::history::History::open(dir).unwrap());
    let conn = rusqlite::Connection::open(dir.join("history.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS publications(
           session_id TEXT PRIMARY KEY,
           phase TEXT NOT NULL CHECK(phase IN ('publishing','removing','private','error')),
           rev INTEGER NOT NULL CHECK(rev >= 1),
           audience TEXT, owner TEXT, server TEXT NOT NULL,
           announced INTEGER NOT NULL DEFAULT 0,
           next_seq INTEGER NOT NULL DEFAULT 0 CHECK(next_seq >= 0),
           error TEXT, last_attempt TEXT, updated_at INTEGER NOT NULL);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO publications(session_id, phase, rev, audience, owner, server, announced, updated_at)
         VALUES(?1, ?2, ?3, '{\"kind\":\"org\"}', NULL, ?4, 1, 0)
         ON CONFLICT(session_id) DO UPDATE SET phase = ?2, rev = ?3, server = ?4",
        params![session, phase, rev as i64, server],
    )
    .unwrap();
}

/// A message in the local history of `session` mentioning `text`; when
/// `acked`, the publisher recorded that the server holds this exact version.
pub(super) fn history_message(dir: &Path, session: &str, uuid: &str, text: &str, acked: bool) {
    drop(super::history::History::open(dir).unwrap());
    let conn = rusqlite::Connection::open(dir.join("history.db")).unwrap();
    let message = json!({"uuid": uuid, "role": "assistant", "text": text}).to_string();
    let seq: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO messages(session_id, uuid, seq, message_json, sha256, agent, native_id, source, first_ingested, updated_at)
         VALUES(?1, ?2, ?3, ?4, X'01', 'claude', 'native', 'test', 0, 0)
         ON CONFLICT(session_id, uuid) DO UPDATE SET message_json = ?4, sha256 = X'02'",
        params![session, uuid, seq, message],
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS publication_sent(
           session_id TEXT NOT NULL, uuid TEXT NOT NULL, sha256 BLOB NOT NULL,
           PRIMARY KEY(session_id, uuid));",
    )
    .unwrap();
    if acked {
        conn.execute(
            "INSERT OR REPLACE INTO publication_sent(session_id, uuid, sha256)
             SELECT session_id, uuid, sha256 FROM messages WHERE session_id = ?1 AND uuid = ?2",
            params![session, uuid],
        )
        .unwrap();
    }
}

/// The local target `octo/repo#7`, linked by a message the server holds.
pub(super) fn published_link(dir: &Path, session: &str) {
    history_message(
        dir,
        session,
        "m-link",
        "opened https://github.com/octo/repo/pull/7 for review",
        true,
    );
}

fn client(base: &str) -> ApiClient {
    ApiClient::with_cookie(base, None).unwrap()
}

fn sync(dir: &Path, base: &str, session: &str) -> usize {
    super::github::sync_recorded(dir, &client(base), session)
        .unwrap()
        .pushed
}

fn snapshot(rev: u64, next_seq: u64) -> Value {
    json!({"rev": rev, "next_seq": next_seq})
}

fn only_local_targets_were_read() {
    let calls = gh_calls();
    assert!(!calls.is_empty(), "the local target was not read");
    assert!(
        calls.iter().all(|c| !c
            .iter()
            .any(|a| a.contains("mallory") || a.contains("evil") || a == "9")),
        "a PR the central list named was read: {calls:?}"
    );
}

/// A locally shared session: the snapshot is read from `GET /prs` BEFORE `gh`
/// runs and both pushes carry it unchanged. The central PR list never adds a
/// `gh` target.
#[test]
fn shared_session_pushes_carry_the_exact_snapshot_read_before_gh() {
    if !isolated("shared_session_pushes_carry_the_exact_snapshot_read_before_gh") {
        return;
    }
    let (base, central) = central(Some((3, 5)));
    let dir = host_dir();
    local_session(&dir, SESSION);
    published_link(&dir, SESSION);
    set_local_publication(&dir, SESSION, "publishing", 3, &base);
    hold_gh();
    let worker = {
        let (dir, base) = (dir.clone(), base.clone());
        std::thread::spawn(move || sync(&dir, &base, SESSION))
    };
    wait_for_held_gh();
    assert_eq!(
        requests(&central)
            .iter()
            .map(|(line, _)| line.clone())
            .collect::<Vec<_>>(),
        vec![format!("GET /api/sessions/{SESSION}/prs")],
        "the snapshot must be read before gh, and nothing pushed yet"
    );
    release_gh();
    assert_eq!(worker.join().unwrap(), 1);
    let pushed = pushes(&central);
    assert_eq!(
        pushed
            .iter()
            .map(|(line, _)| line.as_str())
            .collect::<Vec<_>>(),
        [
            format!("POST /api/sessions/{SESSION}/prs/status"),
            format!("POST /api/sessions/{SESSION}/prs/comments"),
        ]
    );
    for (line, body) in &pushed {
        assert_eq!(body["publication"], snapshot(3, 5), "{line}: {body}");
        assert_eq!(
            (body["owner"].as_str(), body["number"].as_i64()),
            (Some("octo"), Some(7))
        );
    }
    assert_eq!(pushed[1].1["comments"][0]["body"], "private review text");
    assert_eq!(reads(&central), 1);
    only_local_targets_were_read();
}

/// A chunk lands while `gh` runs: the push under the old snapshot is refused
/// and the producer neither re-reads the latest snapshot nor retries.
#[test]
fn chunk_landing_mid_fetch_is_refused_as_stale_and_never_retried_under_latest() {
    if !isolated("chunk_landing_mid_fetch_is_refused_as_stale_and_never_retried_under_latest") {
        return;
    }
    let (base, central) = central(Some((3, 5)));
    let dir = host_dir();
    local_session(&dir, SESSION);
    published_link(&dir, SESSION);
    set_local_publication(&dir, SESSION, "publishing", 3, &base);
    hold_gh();
    let worker = {
        let (dir, base) = (dir.clone(), base.clone());
        std::thread::spawn(move || sync(&dir, &base, SESSION))
    };
    wait_for_held_gh();
    central.lock().unwrap().snapshot = Some((3, 6));
    release_gh();
    assert_eq!(
        worker.join().unwrap(),
        0,
        "a refused push is not a synced PR"
    );
    let pushed = pushes(&central);
    assert_eq!(
        pushed.len(),
        1,
        "after a stale refusal nothing else is pushed: {pushed:?}"
    );
    assert_eq!(
        pushed[0].0,
        format!("POST /api/sessions/{SESSION}/prs/status")
    );
    assert_eq!(pushed[0].1["publication"], snapshot(3, 5));
    assert_eq!(
        reads(&central),
        1,
        "the producer refreshed to the latest snapshot"
    );
    assert!(
        !gh_calls()
            .iter()
            .any(|c| c.iter().any(|a| a.contains("comments,reviews"))),
        "comments were fetched for a push that cannot land"
    );
}

/// A private host session: a signed target lets the host read the PR with
/// `gh` INTO ITS LOCAL CACHE, so the owner's dashboard sees fresh metadata,
/// but no PR content (or even a read of the central PR list) leaves the
/// machine, whatever the central server claims about the session.
#[test]
fn private_host_session_sends_no_pr_content_centrally() {
    if !isolated("private_host_session_sends_no_pr_content_centrally") {
        return;
    }
    for claimed in [None, Some((1, 0))] {
        let (base, central) = central(claimed);
        let dir = host_dir();
        local_session(&dir, SESSION);
        let recorded = super::github::sync_recorded(&dir, &client(&base), SESSION).unwrap();
        assert_eq!(recorded.pushed, 0, "{claimed:?}");
        // The private session's PR refreshed INTO THE LOCAL pr-cache; that
        // is the new "private refresh" semantic. Nothing central changed.
        assert_eq!(recorded.cached_locally, 1, "{claimed:?}");
        assert_eq!(requests(&central), vec![], "{claimed:?}");
        // Subsequent background polls within the same tick are off-schedule
        // (backoff), so they run zero gh calls and return zero candidates.
        let before_gh = gh_calls().len();
        assert_eq!(sync(&dir, &base, SESSION), 0, "{claimed:?}");
        assert_eq!(
            gh_calls().len(),
            before_gh,
            "{claimed:?}: backed-off poll must not fetch"
        );
        // Unshared after an earlier share: still private.
        set_local_publication(&dir, SESSION, "private", 2, &base);
        assert_eq!(sync(&dir, &base, SESSION), 0, "{claimed:?}");
        set_local_publication(&dir, SESSION, "error", 2, &base);
        assert_eq!(sync(&dir, &base, SESSION), 0, "{claimed:?}");
        assert_eq!(requests(&central), vec![], "{claimed:?}");
    }
    assert!(
        !gh_calls().is_empty(),
        "a private session's recorded PR must refresh into the local cache",
    );
}

/// Unsharing locally while `gh` runs: the old snapshot is still current at the
/// (offline) central server, but nothing is written after the local unshare.
#[test]
fn local_unshare_during_fetch_writes_nothing() {
    if !isolated("local_unshare_during_fetch_writes_nothing") {
        return;
    }
    let (base, central) = central(Some((3, 5)));
    let dir = host_dir();
    local_session(&dir, SESSION);
    published_link(&dir, SESSION);
    set_local_publication(&dir, SESSION, "publishing", 3, &base);
    hold_gh();
    let worker = {
        let (dir, base) = (dir.clone(), base.clone());
        std::thread::spawn(move || sync(&dir, &base, SESSION))
    };
    wait_for_held_gh();
    set_local_publication(&dir, SESSION, "removing", 4, &base);
    release_gh();
    assert_eq!(worker.join().unwrap(), 0);
    assert_eq!(pushes(&central), vec![], "a stale write after unshare");
}

/// Local sharing is required but not sufficient: the central copy must be at
/// the local share revision, under the server this share is bound to. Each
/// shared-but-not-matching case refuses before `gh` runs; only the fourth
/// case (publication bound to another server) is a Private intent for THIS
/// server, and private sessions legitimately refresh into the local cache.
#[test]
fn shared_session_without_a_matching_central_snapshot_pushes_nothing() {
    if !isolated("shared_session_without_a_matching_central_snapshot_pushes_nothing") {
        return;
    }
    for (claimed, bound) in [
        (None, true),          // central says unmanaged: no revisionless fallback
        (Some((2, 9)), true),  // an older central revision
        (Some((9, 0)), true),  // a revision this host never published
        (Some((3, 5)), false), // the share is bound to another server
    ] {
        let gh_before = gh_calls().len();
        let (base, central) = central(claimed);
        let dir = host_dir();
        local_session(&dir, SESSION);
        let server = if bound {
            base.clone()
        } else {
            "https://other.example".into()
        };
        published_link(&dir, SESSION);
        set_local_publication(&dir, SESSION, "publishing", 3, &server);
        assert_eq!(sync(&dir, &base, SESSION), 0, "{claimed:?} {bound}");
        assert_eq!(pushes(&central), vec![], "{claimed:?} {bound}");
        if bound {
            assert_eq!(
                gh_calls().len(),
                gh_before,
                "{claimed:?} {bound}: `gh` ran for a push that cannot land",
            );
        } else {
            // Bound to another server → Private intent for THIS client.
            // That is the private-refresh path: `gh` runs, pr-cache.db gets
            // fresh metadata, nothing central moves.
            assert!(
                gh_calls().len() > gh_before,
                "{claimed:?} {bound}: private sessions must refresh locally",
            );
        }
    }
}

/// A target without a canonical local session keeps the legacy behaviour: a
/// revisionless push while the central session is unmanaged. If the central
/// server says the session is managed, nothing is pushed.
#[test]
fn unmanaged_legacy_session_keeps_revisionless_sync() {
    if !isolated("unmanaged_legacy_session_keeps_revisionless_sync") {
        return;
    }
    let (base, central) = central(None);
    let dir = host_dir();
    legacy_target(&dir, SESSION);
    assert_eq!(sync(&dir, &base, SESSION), 1);
    let pushed = pushes(&central);
    assert_eq!(pushed.len(), 2, "{pushed:?}");
    for (line, body) in &pushed {
        assert!(body.get("publication").is_none(), "{line}: {body}");
    }
    only_local_targets_were_read();

    let reads_before = gh_calls().len();
    let (base, central) = self::central(Some((1, 0)));
    legacy_target(&dir, OTHER);
    assert_eq!(sync(&dir, &base, OTHER), 0);
    assert_eq!(
        pushes(&central),
        vec![],
        "a revisionless push to a managed session"
    );
    assert_eq!(gh_calls().len(), reads_before);
}

/// Sharing a session publishes the PR caches of the pull requests its
/// published history links, not of every PR the host may read: a locally
/// authorized target that no message the server holds links sends nothing,
/// even when the central list claims the link. A central list may only narrow.
#[test]
fn shared_session_sends_nothing_for_a_target_absent_from_published_history() {
    if !isolated("shared_session_sends_nothing_for_a_target_absent_from_published_history") {
        return;
    }
    use super::github::{sync_pr, PrCache, Withheld};
    // `(uuid, text, acknowledged by the server)` of each local message.
    type Messages<'a> = &'a [(&'a str, &'a str, bool)];
    let cases: [(&str, Messages); 4] = [
        // Every case also has another session's published message link it.
        ("no published history of its own", &[]),
        (
            "linked only in a message not yet acknowledged",
            &[("m1", "see https://github.com/octo/repo/pull/7", false)],
        ),
        (
            "only similar PRs published",
            &[(
                "m1",
                "https://github.com/octo/repo/pull/70 and https://github.com/octo/repo2/pull/7 \
                 and https://github.com/octo/repo/issues/7 and github.com/octo/repo/pull/7",
                true,
            )],
        ),
        (
            "the published version did not link it; the local edit does",
            &[
                ("m1", "nothing here", true),
                ("m1", "https://github.com/octo/repo/pull/7", false),
            ],
        ),
    ];
    for (n, (case, messages)) in cases.into_iter().enumerate() {
        let dir = dirs::home_dir().unwrap().join(format!("host-{n}"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (base, central) = central(Some((3, 5)));
        local_session(&dir, SESSION);
        set_local_publication(&dir, SESSION, "publishing", 3, &base);
        for (uuid, text, acked) in messages {
            history_message(&dir, SESSION, uuid, text, *acked);
        }
        published_link(&dir, OTHER);
        let outcome = sync_pr(&dir, &client(&base), SESSION, "octo", "repo", 7).unwrap();
        assert_eq!(outcome, PrCache::Withheld(Withheld::NotPublished), "{case}");
        assert!(
            Withheld::NotPublished.sync_detail().contains("not linked"),
            "the caller must learn why"
        );
        assert_eq!(
            pushes(&central),
            vec![],
            "{case}: a PR cache body left the host"
        );
        assert_eq!(
            super::github::sync_recorded(&dir, &client(&base), SESSION)
                .unwrap()
                .withheld,
            vec![Withheld::NotPublished],
            "{case}"
        );
        assert_eq!(pushes(&central), vec![], "{case}");
    }
    assert!(
        gh_calls().is_empty(),
        "gh read a PR no one may see: {:?}",
        gh_calls()
    );

    // Published locally, but the central list does not link it: the server
    // would refuse it, so it is not read or sent either.
    let dir = host_dir();
    let (base, central) = central(Some((3, 5)));
    central.lock().unwrap().prs = json!([]);
    local_session(&dir, SESSION);
    set_local_publication(&dir, SESSION, "publishing", 3, &base);
    published_link(&dir, SESSION);
    let outcome = sync_pr(&dir, &client(&base), SESSION, "octo", "repo", 7).unwrap();
    assert_eq!(outcome, PrCache::Withheld(Withheld::NotPublished));
    assert_eq!(pushes(&central), vec![]);
    assert!(gh_calls().is_empty(), "{:?}", gh_calls());
}

/// `pr sync --session` goes through the same producer.
#[test]
fn pr_sync_command_carries_the_snapshot() {
    if !isolated("pr_sync_command_carries_the_snapshot") {
        return;
    }
    let (base, central) = central(Some((3, 5)));
    let dir = host_dir();
    local_session(&dir, SESSION);
    published_link(&dir, SESSION);
    set_local_publication(&dir, SESSION, "publishing", 3, &base);
    let global = crate::cli::GlobalOpts {
        server_arg: Some(base.clone()),
        server: base.clone(),
        projects_dir: None,
        codex_sessions_dir: None,
        opencode_db: None,
    };
    let args = crate::cli::PrArgs {
        command: crate::cli::PrCommand::Sync(crate::cli::PrSyncArgs {
            session: Some(SESSION.into()),
        }),
    };
    crate::commands::pr::run(&global, &args).unwrap();
    let pushed = pushes(&central);
    assert_eq!(pushed.len(), 2, "{pushed:?}");
    assert!(
        pushed
            .iter()
            .all(|(_, body)| body["publication"] == snapshot(3, 5)),
        "{pushed:?}"
    );

    // The same command for a private session sends nothing centrally but
    // DOES refresh the PR into the local pr-cache (the user asked for fresh
    // data for a session they own locally; no central upload is needed).
    // Explicit `pr sync` bypasses backoff so this fetch runs even though the
    // shared run above just polled the same target.
    let (base, central) = self::central(None);
    let global = crate::cli::GlobalOpts {
        server_arg: Some(base.clone()),
        server: base,
        ..global
    };
    set_local_publication(&dir, SESSION, "private", 4, &global.server);
    let gh_before = gh_calls().len();
    crate::commands::pr::run(&global, &args).unwrap();
    assert_eq!(requests(&central), vec![]);
    assert!(
        gh_calls().len() > gh_before,
        "explicit `pr sync` for a private session must refresh into the local cache",
    );
}

#[test]
fn explicit_private_sync_reads_literal_target_without_any_central_request() {
    if !isolated("explicit_private_sync_reads_literal_target_without_any_central_request") {
        return;
    }
    let (base, central) = central(None);
    let dir = host_dir();
    local_session(&dir, SESSION);
    super::github::sync_pr_explicit(&dir, &client(&base), SESSION, "octo", "repo", 7).unwrap();
    let calls = gh_calls();
    super::github::sync_recorded(&dir, &client(&base), SESSION).unwrap();
    assert_eq!(
        gh_calls().len(),
        calls.len(),
        "singular manual refresh must postpone background poll"
    );
    assert_eq!(
        calls.len(),
        3,
        "explicit sync must fetch private status and comments locally"
    );
    assert_eq!(&calls[0][..5], &["pr", "view", "7", "--repo", "octo/repo"]);
    assert!(
        requests(&central).is_empty(),
        "private cache must make no central request"
    );
    use base64::Engine;
    let (b64, _, _) = super::pr_metadata::chunk(&dir, SESSION, 0, None).unwrap();
    let cached: Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cached["prs"][0]["status"]["title"], "Fix the build");
    assert_eq!(cached["comments"][0]["body"], "private review text");
    super::pr_metadata::unlink(&dir, SESSION, "octo", "repo", 7).unwrap();
    assert!(super::pr_metadata::is_unlinked(&dir, SESSION, "octo", "repo", 7).unwrap());
    super::github::sync_pr_explicit(&dir, &client(&base), SESSION, "octo", "repo", 7).unwrap();
    assert!(!super::pr_metadata::is_unlinked(&dir, SESSION, "octo", "repo", 7).unwrap());
    let kid: String = rusqlite::Connection::open(dir.join("authz.db"))
        .unwrap()
        .query_row("SELECT cred_id FROM credentials LIMIT 1", [], |r| r.get(0))
        .unwrap();
    Store::open(&dir)
        .unwrap()
        .revoke_credential(&kid, now_secs())
        .unwrap();
    let (b64, _, _) = super::pr_metadata::chunk(&dir, SESSION, 0, None).unwrap();
    let revoked: Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(revoked, json!({"prs":[],"comments":[]}));
    let before = gh_calls().len();
    sync(&dir, &base, SESSION);
    assert_eq!(
        gh_calls().len(),
        before,
        "background poll must still not read private PRs"
    );
}

/// Separate daemon and web/CLI processes must serialize the same PR target.
#[test]
fn concurrent_background_and_explicit_sync_for_same_target_are_serialized() {
    if !isolated("concurrent_background_and_explicit_sync_for_same_target_are_serialized") {
        return;
    }
    let (base, _) = central(None); // private session: no central snapshot matters
    let dir = host_dir();
    local_session(&dir, SESSION);

    hold_gh();
    let spawn = |mode: &str| {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::local_host::pr_cache_tests::cross_process_refresh_child",
                "--nocapture",
            ])
            .env("PR_REFRESH_CHILD", mode)
            .env("PR_REFRESH_RUNTIME", &dir)
            .env("PR_REFRESH_BASE", &base)
            .spawn()
            .unwrap()
    };
    let mut first = spawn("background");
    wait_for_held_gh();
    let mut second = spawn("explicit");
    for _ in 0..200 {
        if tools().join("second-started").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(tools().join("second-started").exists());
    std::thread::sleep(Duration::from_millis(250));
    let before_release = gh_calls().len();
    release_gh();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    assert_eq!(
        before_release, 1,
        "second process started gh before the first released its target"
    );
    assert_eq!(
        gh_calls().len(),
        6,
        "both processes must finish their refresh"
    );
}

/// The helper used by owner HTTP refresh records and schedules the local target.
#[test]
fn owner_local_pr_target_is_picked_up_by_automatic_background_poll() {
    if !isolated("owner_local_pr_target_is_picked_up_by_automatic_background_poll") {
        return;
    }
    let (base, central) = central(None);
    let dir = host_dir();
    // A local session WITH a local sessions record but WITHOUT a signed
    // session_pr_targets row (no call to record_target / record_pr_target).
    Store::open(&dir)
        .unwrap()
        .insert_session(
            &super::authz::sessions::LocalSession {
                session_id: SESSION.into(),
                agent: conversations_core::AgentKind::Claude,
                cwd: "/work".into(),
                resume_id: None,
                launcher: conversations_core::AgentLauncher::default(),
                adk_statusline: false,
            },
            "signed_start",
        )
        .unwrap();
    // Call the same helper as the owner HTTP handler.
    super::github::refresh_local(&dir, SESSION, "octo", "repo", 7, true).unwrap();
    let gh_before = gh_calls().len();
    assert!(gh_before > 0, "refresh_local must have called gh");

    // Immediate background polling must observe the completed manual refresh.
    let recorded = super::github::sync_recorded(&dir, &client(&base), SESSION).unwrap();
    assert_eq!(
        recorded.cached_locally, 0,
        "manual owner refresh must advance the schedule"
    );
    assert_eq!(
        recorded.pushed, 0,
        "private session must not push centrally"
    );
    assert_eq!(
        requests(&central),
        vec![],
        "no central requests for private session"
    );
    assert!(
        gh_calls().len() == gh_before,
        "immediate background poll must not repeat owner refresh"
    );
}

#[test]
fn cross_process_refresh_child() {
    let Ok(mode) = std::env::var("PR_REFRESH_CHILD") else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os("PR_REFRESH_RUNTIME").unwrap());
    let base = std::env::var("PR_REFRESH_BASE").unwrap();
    if mode == "explicit" {
        std::fs::write(tools().join("second-started"), "").unwrap();
        super::github::sync_pr_explicit(&dir, &client(&base), SESSION, "octo", "repo", 7).unwrap();
    } else {
        super::github::sync_recorded(&dir, &client(&base), SESSION).unwrap();
    }
}

#[test]
fn singular_refresh_discards_metadata_when_authority_is_revoked_during_fetch() {
    if !isolated("singular_refresh_discards_metadata_when_authority_is_revoked_during_fetch") {
        return;
    }
    let (base, central) = central(None);
    let dir = host_dir();
    local_session(&dir, SESSION);
    hold_gh();
    let thread_dir = dir.clone();
    let fetch = std::thread::spawn(move || {
        super::github::sync_pr_explicit(&thread_dir, &client(&base), SESSION, "octo", "repo", 7)
    });
    wait_for_held_gh();
    let kid: String = rusqlite::Connection::open(dir.join("authz.db"))
        .unwrap()
        .query_row("SELECT cred_id FROM credentials LIMIT 1", [], |r| r.get(0))
        .unwrap();
    Store::open(&dir)
        .unwrap()
        .revoke_credential(&kid, now_secs())
        .unwrap();
    release_gh();
    assert!(fetch.join().unwrap().is_err());
    assert_eq!(
        super::pr_metadata::local_fingerprint(&dir, SESSION, "octo", "repo", 7).unwrap(),
        None
    );
    assert!(requests(&central).is_empty());
}

#[test]
fn manual_unchanged_refresh_resets_floor_and_matches_persisted_fingerprint() {
    if !isolated("manual_unchanged_refresh_resets_floor_and_matches_persisted_fingerprint") {
        return;
    }
    let (base, _) = central(None);
    let dir = host_dir();
    local_session(&dir, SESSION);
    super::github::sync_recorded(&dir, &client(&base), SESSION).unwrap();
    let db = rusqlite::Connection::open(dir.join("authz.db")).unwrap();
    let fingerprint: String = db
        .query_row("SELECT fingerprint FROM session_pr_targets", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        Some(fingerprint),
        super::pr_metadata::local_fingerprint(&dir, SESSION, "octo", "repo", 7).unwrap()
    );
    db.execute(
        "UPDATE session_pr_targets SET current_delay_secs=900, next_poll_at=?1",
        [now_secs() + 900],
    )
    .unwrap();
    super::github::sync_pr_explicit(&dir, &client(&base), SESSION, "octo", "repo", 7).unwrap();
    let (delay, due): (i64, i64) = db
        .query_row(
            "SELECT current_delay_secs,next_poll_at FROM session_pr_targets",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(delay, 60);
    assert!(due >= now_secs() + 59);
    let before = gh_calls().len();
    super::github::sync_recorded(&dir, &client(&base), SESSION).unwrap();
    assert_eq!(gh_calls().len(), before);
}

#[test]
fn unlink_while_fetching_cannot_be_resurrected() {
    if !isolated("unlink_while_fetching_cannot_be_resurrected") {
        return;
    }
    let (base, _) = central(None);
    let dir = host_dir();
    local_session(&dir, SESSION);
    hold_gh();
    let thread_dir = dir.clone();
    let fetch = std::thread::spawn(move || {
        super::github::sync_recorded(&thread_dir, &client(&base), SESSION).unwrap()
    });
    wait_for_held_gh();
    let unlink_dir = dir.clone();
    let unlink = std::thread::spawn(move || {
        super::pr_metadata::unlink(&unlink_dir, SESSION, "octo", "repo", 7).unwrap()
    });
    release_gh();
    fetch.join().unwrap();
    unlink.join().unwrap();
    assert!(super::pr_metadata::is_unlinked(&dir, SESSION, "octo", "repo", 7).unwrap());
    assert_eq!(
        super::pr_metadata::local_fingerprint(&dir, SESSION, "octo", "repo", 7).unwrap(),
        None
    );
}

#[test]
fn target_lock_is_private_rejects_symlinks_and_keeps_targets_independent() {
    use super::github::PrSyncLock;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let lock = PrSyncLock::acquire(temp.path(), SESSION, "octo", "repo", 7, false)
        .unwrap()
        .unwrap();
    assert!(
        PrSyncLock::acquire(temp.path(), SESSION, "octo", "repo", 7, false)
            .unwrap()
            .is_none()
    );
    assert!(
        PrSyncLock::acquire(temp.path(), SESSION, "octo", "repo", 8, false)
            .unwrap()
            .is_some()
    );
    drop(lock);
    let hash = super::pr_metadata::pr_fingerprint_hash(&json!([SESSION, "octo", "repo", 7]));
    let path = temp.path().join(format!("pr-sync-{hash}.lock"));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(temp.path().join("outside"), &path).unwrap();
    assert!(PrSyncLock::acquire(temp.path(), SESSION, "octo", "repo", 7, false).is_err());
}

#[test]
fn private_transcript_discovery_refreshes_without_resetting_existing_authority_or_schedule() {
    if !isolated(
        "private_transcript_discovery_refreshes_without_resetting_existing_authority_or_schedule",
    ) {
        return;
    }
    let (base, central) = central(None);
    let dir = host_dir();
    local_session(&dir, SESSION);
    super::history::History::open(&dir)
        .unwrap()
        .remember_prs(SESSION, &["https://github.com/octo/repo/pull/8".into()])
        .unwrap();
    let result = super::github::sync_recorded(&dir, &client(&base), SESSION).unwrap();
    assert_eq!(result.cached_locally, 2);
    let before = gh_calls().len();
    super::github::sync_recorded(&dir, &client(&base), SESSION).unwrap();
    assert_eq!(
        gh_calls().len(),
        before,
        "discovery must preserve existing schedules"
    );
    let db = rusqlite::Connection::open(dir.join("authz.db")).unwrap();
    let kid: Option<String> = db
        .query_row(
            "SELECT kid FROM session_pr_targets WHERE number=7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        kid.is_some(),
        "discovery must not replace signed provenance"
    );
    super::history::History::open(&dir)
        .unwrap()
        .remember_prs(SESSION, &["https://github.com/octo/repo/pull/7".into()])
        .unwrap();
    Store::open(&dir)
        .unwrap()
        .revoke_credential(&kid.unwrap(), now_secs())
        .unwrap();
    super::pr_metadata::unlink(&dir, SESSION, "octo", "repo", 8).unwrap();
    db.execute("UPDATE session_pr_targets SET next_poll_at=0", [])
        .unwrap();
    super::github::sync_recorded(&dir, &client(&base), SESSION).unwrap();
    assert_eq!(
        gh_calls().len(),
        before,
        "discovery must not resurrect revoked or unlinked targets"
    );
    assert!(super::pr_metadata::is_unlinked(&dir, SESSION, "octo", "repo", 8).unwrap());
    assert!(requests(&central).is_empty());
}

#[test]
fn explicit_refresh_ignores_deliberately_unlinked_targets() {
    if !isolated("explicit_refresh_ignores_deliberately_unlinked_targets") {
        return;
    }
    let (base, _) = central(None);
    let dir = host_dir();
    local_session(&dir, SESSION);
    super::pr_metadata::unlink(&dir, SESSION, "octo", "repo", 7).unwrap();
    let result = super::github::sync_recorded_explicit(&dir, &client(&base), SESSION).unwrap();
    assert_eq!(
        result.targets, 0,
        "an unlinked target is not work requested by pr sync"
    );
    assert_eq!(result.failed, 0);
    assert!(gh_calls().is_empty());
}

#[test]
fn owner_refresh_preserves_signed_provenance_and_revocation() {
    if !isolated("owner_refresh_preserves_signed_provenance_and_revocation") {
        return;
    }
    let dir = host_dir();
    local_session(&dir, SESSION);
    let db = rusqlite::Connection::open(dir.join("authz.db")).unwrap();
    let provenance = || {
        db.query_row(
            "SELECT kid,dlg,cap,epoch FROM session_pr_targets WHERE number=7",
            [],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .unwrap()
    };
    let before = provenance();
    super::github::refresh_local(&dir, SESSION, "octo", "repo", 7, true).unwrap();
    assert_eq!(
        provenance(),
        before,
        "owner refresh must not adopt signed authority"
    );
    Store::open(&dir)
        .unwrap()
        .revoke_credential(before.0.as_deref().unwrap(), now_secs())
        .unwrap();
    assert!(super::authz::pr_poll_candidates(&dir, SESSION, true)
        .unwrap()
        .is_empty());
}

#[test]
fn revoked_during_private_fetch_is_skipped_without_backoff_or_private_reason() {
    if !isolated("revoked_during_private_fetch_is_skipped_without_backoff_or_private_reason") {
        return;
    }
    let (base, _) = central(None);
    let dir = host_dir();
    local_session(&dir, SESSION);
    let db = rusqlite::Connection::open(dir.join("authz.db")).unwrap();
    let before: (i64, i64) = db
        .query_row(
            "SELECT current_delay_secs,next_poll_at FROM session_pr_targets",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    hold_gh();
    let thread_dir = dir.clone();
    let fetch = std::thread::spawn(move || {
        super::github::sync_recorded_explicit(&thread_dir, &client(&base), SESSION).unwrap()
    });
    wait_for_held_gh();
    let kid: String = db
        .query_row("SELECT kid FROM session_pr_targets", [], |r| r.get(0))
        .unwrap();
    Store::open(&dir)
        .unwrap()
        .revoke_credential(&kid, now_secs())
        .unwrap();
    release_gh();
    let result = fetch.join().unwrap();
    assert!(
        result.withheld.is_empty(),
        "revocation is not a private-sharing refusal"
    );
    assert_eq!(
        result.targets, 0,
        "target withdrawn during fetch is no longer requested"
    );
    let after: (i64, i64) = db
        .query_row(
            "SELECT current_delay_secs,next_poll_at FROM session_pr_targets",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        after, before,
        "withdrawn target must not mutate its schedule"
    );
    assert_eq!(
        super::pr_metadata::local_fingerprint(&dir, SESSION, "octo", "repo", 7).unwrap(),
        None
    );
}
