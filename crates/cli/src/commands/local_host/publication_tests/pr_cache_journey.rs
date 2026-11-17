//! The PR cache journey through the REAL authenticated router: local canonical
//! history that links a pull request, an explicit local share delivered by the
//! real publisher (so `publication_sent` holds the server's actual
//! acknowledgements), a really signed `pr.sync` admitted by the host's own
//! dispatcher with a real grant epoch, a recording `gh` for the locally
//! authorized target only, and the central status and comment caches as
//! readers see them. Only `gh` and the agents are stand-ins.
//!
//! Every request the host sends (publisher and PR producer alike) passes a
//! recorder that keeps its request line, full body and the server's answer
//! status, so "nothing left the host" is asserted on captured bytes, and every
//! cache claim is asserted on what the real server returns to a reader.
//!
//! Composition seam: signed PR frames enter `execute_signed` AFTER decryption.
//! These tests exercise real host authorization, publisher and central HTTP,
//! not the private network channel; the latter has separate Chromium coverage.

use super::*;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

const LINK: &str = "https://github.com/octo/repo/pull/7";

/// Re-run `name` in a child process with an empty environment: an isolated
/// HOME, XDG dirs, TMPDIR and settings file, and a PATH of one directory that
/// holds only the recording `gh` and inert agent stubs. No personal GitHub
/// login, config, agent or socket is reachable. Returns true in the child.
fn isolated_with_gh(name: &str) -> bool {
    crate::commands::local_host::test_support::isolated(
        &format!("commands::local_host::daemon::publication_tests::pr_cache_journey::{name}"),
        &[("gh", GH_STUB)],
    )
}

// --------------------------------------------------------------------- gh ----

/// Records each call (one directory per call, one file per argument) and
/// answers the three reads a PR sync makes. Every answer names its call index,
/// so a cache a reader sees identifies the exact `gh` read it came from. When
/// the test created `hold.<n>`, status read `n` creates `reached.<n>` and waits
/// for `release.<n>`, bounded to ~20 s; the stub always exits by itself.
const GH_STUB: &str = r#"#!/bin/sh
t="${0%/*}"
n=0
while [ -e "$t/calls/$n" ]; do n=$((n+1)); done
/bin/mkdir -p "$t/calls/$n"
i=0
for a in "$@"; do printf '%s' "$a" > "$t/calls/$n/$i"; i=$((i+1)); done
case "$*" in
  *"state,isDraft"*)
    if [ -e "$t/hold.$n" ]; then
      : > "$t/reached.$n"
      w=0
      while [ ! -e "$t/release.$n" ] && [ $w -lt 400 ]; do /bin/sleep 0.05; w=$((w+1)); done
    fi
    printf '{"state":"OPEN","isDraft":false,"title":"journey-title-%s","mergedAt":null,"number":7}' "$n" ;;
  *"comments,reviews"*)
    printf '{"comments":[{"id":"IC_1","author":{"login":"rev"},"body":"journey-review-%s","url":"https://github.com/octo/repo/pull/7#issuecomment-1","createdAt":"2026-11-03T10:00:00Z"}],"reviews":[]}' "$n" ;;
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

fn is_status_read(call: &[String]) -> bool {
    call.iter().any(|a| a.starts_with("state,isDraft"))
}

fn is_comments_read(call: &[String]) -> bool {
    call.iter().any(|a| a == "comments,reviews")
}

/// Index of the last `gh` call matching `pick`.
fn last_call(pick: fn(&[String]) -> bool) -> usize {
    gh_calls()
        .iter()
        .rposition(|c| pick(c))
        .expect("no such gh call")
}

/// Only the locally authorized `octo/repo#7` was ever read.
fn only_the_authorized_target_was_read() {
    for call in gh_calls() {
        let reads_seven = call.iter().any(|a| a == "7" || a == "number=7");
        let names_repo = call.iter().any(|a| a == "octo/repo" || a == "owner=octo");
        assert!(
            reads_seven && names_repo,
            "gh read another target: {call:?}"
        );
    }
}

/// Hold the next `gh` call (which must be the status read of the sync about to
/// start) until [`release`]. Returns that call's index.
fn hold_next_status_read() -> usize {
    let n = gh_calls().len();
    std::fs::write(tools().join(format!("hold.{n}")), "").unwrap();
    n
}

async fn wait_until_held(n: usize) {
    for _ in 0..400 {
        if tools().join(format!("reached.{n}")).exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("gh status read {n} never started");
}

fn release(n: usize) {
    std::fs::write(tools().join(format!("release.{n}")), "").unwrap();
}

// --------------------------------------------------------------- recorder ----

#[derive(Clone, Debug)]
struct Exchange {
    /// `METHOD /path`.
    line: String,
    body: Vec<u8>,
    /// The server's answer status; `None` when no answer came back.
    status: Option<u16>,
}

/// Loopback recorder between the host and `upstream` (the central proxy, which
/// can take the host offline). Keeps every request the host sent, in order.
#[derive(Clone)]
struct Recorder {
    url: String,
    log: Arc<Mutex<Vec<Exchange>>>,
}

impl Recorder {
    async fn start(upstream: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let recorder = Recorder {
            url: format!("http://{}", listener.local_addr().unwrap()),
            log: Arc::default(),
        };
        let upstream = upstream.trim_start_matches("http://").to_owned();
        let r = recorder.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let (r, upstream) = (r.clone(), upstream.clone());
                tokio::spawn(async move {
                    let _ = r.serve(socket, &upstream).await;
                });
            }
        });
        recorder
    }

    async fn serve(&self, mut socket: tokio::net::TcpStream, upstream: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        loop {
            let Some((head, body)) = read_http(&mut socket).await? else {
                return Ok(());
            };
            let line = String::from_utf8_lossy(&head)
                .lines()
                .next()
                .unwrap_or_default()
                .trim_end_matches(" HTTP/1.1")
                .to_owned();
            let index = {
                let mut log = self.log.lock().unwrap();
                log.push(Exchange {
                    line,
                    body: body.clone(),
                    status: None,
                });
                log.len() - 1
            };
            let mut up = tokio::net::TcpStream::connect(upstream).await?;
            up.write_all(&head).await?;
            up.write_all(&body).await?;
            let Some((rhead, rbody)) = read_http(&mut up).await? else {
                return Ok(());
            };
            self.log.lock().unwrap()[index].status = String::from_utf8_lossy(&rhead)
                .split_whitespace()
                .nth(1)
                .and_then(|code| code.parse().ok());
            socket.write_all(&rhead).await?;
            socket.write_all(&rbody).await?;
        }
    }

    fn len(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    fn since(&self, mark: usize) -> Vec<Exchange> {
        self.log.lock().unwrap()[mark..].to_vec()
    }

    /// The PR cache traffic since `mark`, as `(METHOD /path, answer status)`.
    fn prs_since(&self, mark: usize) -> Vec<(String, Option<u16>)> {
        self.since(mark)
            .into_iter()
            .filter(|e| e.line.contains("/prs"))
            .map(|e| (e.line, e.status))
            .collect()
    }

    /// The full body of the last request to `line`.
    fn last_body(&self, line: &str) -> Value {
        let log = self.log.lock().unwrap();
        let exchange = log.iter().rev().find(|e| e.line == line).unwrap();
        serde_json::from_slice(&exchange.body).unwrap()
    }
}

// ---------------------------------------------------------------- journey ----

fn prs_path() -> String {
    format!("/api/sessions/{S}/prs")
}

fn comments_path() -> String {
    format!("/api/sessions/{S}/prs/comments")
}

fn status_line() -> String {
    format!("POST /api/sessions/{S}/prs/status")
}

fn comments_line() -> String {
    format!("POST /api/sessions/{S}/prs/comments")
}

fn target(number: i64) -> String {
    format!(r#"{{"session":"{S}","owner":"octo","repo":"repo","number":{number}}}"#)
}

/// The real central router (auth on) behind its proxy, the recorder the host
/// talks to, and a host directory whose canonical local session `S` has
/// history linking [`LINK`]. Nothing is shared yet.
struct Journey {
    central: Central,
    recorder: Recorder,
    host_dir: tempfile::TempDir,
}

impl Journey {
    async fn start() -> Self {
        let central = Central::start().await;
        let recorder = Recorder::start(central.server()).await;
        let host = private_dir();
        Store::open(host.path())
            .unwrap()
            .insert_session(
                &authz::sessions::LocalSession {
                    session_id: S.into(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/work".into(),
                    resume_id: None,
                    launcher: conversations_core::AgentLauncher::default(),
                    adk_statusline: false,
                },
                "signed_start",
            )
            .unwrap();
        ingest(
            host.path(),
            S,
            &[
                user("m1", &format!("journey-needle opened {LINK} for review")),
                user("m2", "nothing else here"),
            ],
        );
        Journey {
            central,
            recorder,
            host_dir: host,
        }
    }

    fn dir(&self) -> &Path {
        self.host_dir.path()
    }

    /// Alice's host, publishing and syncing through the recorder.
    fn host(&self) -> Host {
        host_on(
            self.dir(),
            &self.recorder.url,
            Some(self.central.cookies[ALICE].clone()),
        )
    }

    fn share(&self) -> publication::Status {
        publication::share(
            self.dir(),
            S,
            people(&[BOB]),
            Some(ALICE),
            &self.recorder.url,
        )
        .unwrap()
    }

    /// The real publisher, run until it has nothing left to send.
    async fn pump(&self) {
        let destination = Destination {
            server: &self.recorder.url,
            client: &self.central.http,
            cookie: Some(&self.central.cookies[ALICE]),
        };
        publication::pump(self.dir(), &destination).await.unwrap();
    }

    /// The `publication` snapshot the real server reports for `S`.
    async fn snapshot(&self) -> Value {
        let (status, body) = self.central.get(ALICE, &prs_path()).await;
        assert_eq!(status, 200, "{body}");
        body["publication"].clone()
    }

    /// What `who` reads: `(GET /prs status, its prs, GET /prs/comments status,
    /// its comments)`.
    async fn read(&self, who: &str) -> (u16, Value, u16, Value) {
        let (prs_status, prs) = self.central.get(who, &prs_path()).await;
        let (comments_status, comments) = self.central.get(who, &comments_path()).await;
        (
            prs_status,
            prs["prs"].clone(),
            comments_status,
            comments["comments"].clone(),
        )
    }

    /// `who` reads exactly the link, with this cached status title and comment
    /// body (`None`: not cached).
    async fn assert_reads(&self, who: &str, title: Option<&str>, comment: Option<&str>) {
        let (prs_status, prs, comments_status, comments) = self.read(who).await;
        assert_eq!((prs_status, comments_status), (200, 200), "{who}");
        let prs = prs.as_array().unwrap();
        assert_eq!(prs.len(), 1, "{who}: {prs:?}");
        assert_eq!(prs[0]["url"], LINK, "{who}");
        assert_eq!(prs[0]["status"]["title"].as_str(), title, "{who}: {prs:?}");
        assert_eq!(
            prs[0]["status_fetched_at"].is_null(),
            title.is_none(),
            "{who}: {prs:?}"
        );
        let bodies: Vec<&str> = comments
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["body"].as_str().unwrap())
            .collect();
        assert_eq!(bodies, comment.into_iter().collect::<Vec<_>>(), "{who}");
    }

    /// `who` can read neither the link list nor the cached comments.
    async fn assert_denied(&self, who: &str) {
        let (prs_status, prs, comments_status, comments) = self.read(who).await;
        assert_eq!(
            (prs_status, comments_status),
            (404, 404),
            "{who} read {prs} {comments}"
        );
        assert!(!prs.to_string().contains("journey-"), "{who}: {prs}");
        assert!(
            !comments.to_string().contains("journey-"),
            "{who}: {comments}"
        );
    }
}

/// Nothing in `exchanges` carried a PR result: no PR cache write and no body
/// naming a fetched title or comment. (Reading the link list sends none.)
fn no_pr_result_left(exchanges: &[Exchange], what: &str) {
    for exchange in exchanges {
        assert!(
            !(exchange.line.starts_with("POST ") && exchange.line.contains("/prs")),
            "{what}: {}",
            exchange.line
        );
        let body = String::from_utf8_lossy(&exchange.body);
        assert!(
            !body.contains("journey-title") && !body.contains("journey-review"),
            "{what}: {} carried {body}",
            exchange.line
        );
    }
}

// ----------------------------------------------------------------- proofs ----

#[test]
fn a_signed_pr_sync_publishes_only_a_published_link_to_its_readers_and_unshare_deletes_it() {
    if !isolated_with_gh(
        "a_signed_pr_sync_publishes_only_a_published_link_to_its_readers_and_unshare_deletes_it",
    ) {
        return;
    }
    runtime().block_on(async {
        let journey = Journey::start().await;
        let dir = journey.dir();
        let host = journey.host();
        let signed = Signed {
            dir,
            host_id: authz::host_id(dir).unwrap(),
            jti: Cell::new(0),
        };
        let owner = Signer::new();
        // PR refresh now follows session.write; the owner key holds write
        // on the session it refreshes (the obsolete standalone pr_sync cap
        // no longer authorizes pr.sync by itself).
        signed.enroll("owner", &owner, &[format!("session.write:{S}")]);
        let sync = |number| signed.frame(&owner, "pr.sync", &target(number));

        // Private: the signed target is recorded under its real grant epoch,
        // the cache stays local, and nothing leaves.
        let mark = journey.recorder.len();
        let (status, detail) = execute_signed(&host, sync(7)).await;
        assert_eq!(status, OpStatus::Executed, "{detail:?}");
        assert_eq!(
            authz::pr_targets(dir, S).unwrap(),
            vec![("octo".to_owned(), "repo".to_owned(), 7)]
        );
        assert!(
            journey.recorder.since(mark).is_empty(),
            "a private sync sent {:?}",
            journey.recorder.since(mark)
        );
        assert_eq!(
            gh_calls().len(),
            3,
            "private explicit sync reads only the recorded PR"
        );
        journey.assert_denied(ALICE).await;

        // Shared locally, but the server has not acknowledged the linking
        // message yet: still nothing is read or sent.
        journey.share();
        let (status, detail) = execute_signed(&host, sync(7)).await;
        assert_eq!(status, OpStatus::Failed, "{detail:?}");
        assert!(
            detail.as_deref().is_some_and(|d| d.contains("not linked")),
            "{detail:?}"
        );
        assert!(journey.recorder.since(mark).is_empty());
        assert_eq!(
            gh_calls().len(),
            3,
            "private explicit sync reads only the recorded PR"
        );

        // The real publisher delivers it; Bob reads the link, uncached.
        journey.pump().await;
        assert_eq!(state(dir, S), State::Shared);
        journey.assert_reads(BOB, None, None).await;
        let snapshot = journey.snapshot().await;
        assert!(
            snapshot["rev"].is_u64() && snapshot["next_seq"].is_u64(),
            "{snapshot}"
        );

        // The signed sync: the snapshot is read before gh, both caches are
        // pushed under it, and the real server stores them.
        let mark = journey.recorder.len();
        let (status, detail) = execute_signed(&host, sync(7)).await;
        assert_eq!(status, OpStatus::Executed, "{detail:?}");
        assert_eq!(
            journey.recorder.prs_since(mark),
            vec![
                (format!("GET {}", prs_path()), Some(200)),
                (status_line(), Some(204)),
                (comments_line(), Some(204)),
            ]
        );
        let title = format!("journey-title-{}", last_call(is_status_read));
        let comment = format!("journey-review-{}", last_call(is_comments_read));
        let pushed_status = journey.recorder.last_body(&status_line());
        let pushed_comments = journey.recorder.last_body(&comments_line());
        assert_eq!(pushed_status["publication"], snapshot, "{pushed_status}");
        assert_eq!(pushed_status["status"]["title"], title.as_str());
        assert_eq!(
            pushed_comments["publication"], snapshot,
            "{pushed_comments}"
        );
        assert_eq!(pushed_comments["comments"][0]["body"], comment.as_str());
        only_the_authorized_target_was_read();

        journey
            .assert_reads(BOB, Some(&title), Some(&comment))
            .await;
        journey
            .assert_reads(ALICE, Some(&title), Some(&comment))
            .await;
        journey.assert_denied(EVE).await;
        // The caches are central: with the host offline the reader still reads.
        journey.central.proxy.set(Mode::Offline);
        journey
            .assert_reads(BOB, Some(&title), Some(&comment))
            .await;
        journey.central.proxy.set(Mode::Pass);

        // A target the published history never links: recorded, refused with
        // its reason, not read, and nothing is sent.
        let reads = gh_calls().len();
        let mark = journey.recorder.len();
        let (status, detail) = execute_signed(&host, sync(9)).await;
        assert_eq!(status, OpStatus::Failed, "{detail:?}");
        assert!(
            detail.as_deref().is_some_and(|d| d.contains("not linked")),
            "{detail:?}"
        );
        assert!(authz::pr_targets(dir, S).unwrap().contains(&(
            "octo".to_owned(),
            "repo".to_owned(),
            9
        )));
        assert!(
            journey.recorder.since(mark).is_empty(),
            "an unlinked sync sent {:?}",
            journey.recorder.since(mark)
        );
        assert_eq!(gh_calls().len(), reads);
        journey
            .assert_reads(BOB, Some(&title), Some(&comment))
            .await;

        // Unshare: the real tombstone deletes the link and both caches for
        // every reader, also while the host is offline.
        publication::unshare(dir, S).unwrap();
        journey.pump().await;
        assert_eq!(state(dir, S), State::Private);
        for who in [BOB, ALICE, EVE] {
            journey.assert_denied(who).await;
        }
        journey.central.proxy.set(Mode::Offline);
        journey.assert_denied(BOB).await;
        journey.central.proxy.set(Mode::Pass);

        // Private again: a sync sends nothing, and the bodies the host sent
        // while shared cannot write the caches back.
        let mark = journey.recorder.len();
        let (status, detail) = execute_signed(&host, sync(7)).await;
        assert_eq!(status, OpStatus::Executed, "{detail:?}");
        no_pr_result_left(&journey.recorder.since(mark), "a sync after unshare");
        assert!(journey.recorder.since(mark).is_empty());
        assert_eq!(gh_calls().len(), reads + 3);
        for (path, body) in [
            (format!("{}/status", prs_path()), &pushed_status),
            (comments_path(), &pushed_comments),
        ] {
            let (status, answer) = journey
                .central
                .post(ALICE, &path, serde_json::to_vec(body).unwrap())
                .await;
            assert!(
                status == 404 || status == 409,
                "a replayed cache body was accepted after unshare: {status} {answer}"
            );
        }
        for who in [BOB, ALICE] {
            journey.assert_denied(who).await;
        }
        assert_eq!(local_count(dir, S), 2, "local history must survive");
    });
}

#[test]
fn a_sync_crossing_a_chunk_advance_or_an_unshare_and_reshare_writes_no_stale_cache() {
    if !isolated_with_gh(
        "a_sync_crossing_a_chunk_advance_or_an_unshare_and_reshare_writes_no_stale_cache",
    ) {
        return;
    }
    runtime().block_on(async {
        let journey = Journey::start().await;
        let dir = journey.dir();
        let host = journey.host();
        let signed = Signed {
            dir,
            host_id: authz::host_id(dir).unwrap(),
            jti: Cell::new(0),
        };
        let owner = Signer::new();
        // PR refresh now follows session.write; the owner key holds write
        // on the session it refreshes (the obsolete standalone pr_sync cap
        // no longer authorizes pr.sync by itself).
        signed.enroll("owner", &owner, &[format!("session.write:{S}")]);
        let sync = || signed.frame(&owner, "pr.sync", &target(7));

        journey.share();
        journey.pump().await;
        assert_eq!(state(dir, S), State::Shared);
        let (status, detail) = execute_signed(&host, sync()).await;
        assert_eq!(status, OpStatus::Executed, "{detail:?}");
        let title = format!("journey-title-{}", last_call(is_status_read));
        let comment = format!("journey-review-{}", last_call(is_comments_read));
        journey
            .assert_reads(BOB, Some(&title), Some(&comment))
            .await;

        // A chunk lands while gh reads: the push under the snapshot read
        // before gh is refused by the real server, nothing is re-read or
        // retried, comments are not fetched, and the reader keeps the old cache.
        let held = hold_next_status_read();
        let mark = journey.recorder.len();
        let (outcome, (before, after)) = tokio::join!(execute_signed(&host, sync()), async {
            wait_until_held(held).await;
            let before = journey.snapshot().await;
            ingest(dir, S, &[user("m3", "a chunk advance while gh reads")]);
            journey.pump().await;
            assert_eq!(state(dir, S), State::Shared);
            let after = journey.snapshot().await;
            release(held);
            (before, after)
        });
        assert_eq!(after["rev"], before["rev"]);
        assert!(
            after["next_seq"].as_u64() > before["next_seq"].as_u64(),
            "{before} -> {after}"
        );
        assert_eq!(outcome.0, OpStatus::Failed, "{outcome:?}");
        assert_eq!(
            journey.recorder.prs_since(mark),
            vec![
                (format!("GET {}", prs_path()), Some(200)),
                (status_line(), Some(409)),
            ]
        );
        let stale = journey.recorder.last_body(&status_line());
        assert_eq!(stale["publication"], before, "{stale}");
        assert_eq!(
            stale["status"]["title"],
            format!("journey-title-{held}").as_str()
        );
        assert_eq!(gh_calls().len(), held + 1, "{:?}", gh_calls());
        journey
            .assert_reads(BOB, Some(&title), Some(&comment))
            .await;

        // A fresh sync publishes under the advanced snapshot.
        let (status, detail) = execute_signed(&host, sync()).await;
        assert_eq!(status, OpStatus::Executed, "{detail:?}");
        let title = format!("journey-title-{}", last_call(is_status_read));
        let comment = format!("journey-review-{}", last_call(is_comments_read));
        assert_eq!(
            journey.recorder.last_body(&status_line())["publication"],
            after
        );
        journey
            .assert_reads(BOB, Some(&title), Some(&comment))
            .await;
        let old_rev_status = journey.recorder.last_body(&status_line());
        let old_rev_comments = journey.recorder.last_body(&comments_line());

        // Unshare and re-share while gh reads: the re-share re-publishes the
        // link with empty caches (they were deleted, not hidden), and the job
        // that read before it writes nothing.
        let held = hold_next_status_read();
        let mark = journey.recorder.len();
        let (outcome, reshared) = tokio::join!(execute_signed(&host, sync()), async {
            wait_until_held(held).await;
            let old = publication::status(dir, S).unwrap().rev;
            publication::unshare(dir, S).unwrap();
            journey.pump().await;
            assert_eq!(state(dir, S), State::Private);
            journey.assert_denied(BOB).await;
            let reshared = journey.share();
            assert!(reshared.rev > old);
            journey.pump().await;
            assert_eq!(state(dir, S), State::Shared);
            journey.assert_reads(BOB, None, None).await;
            release(held);
            reshared
        });
        assert_eq!(outcome.0, OpStatus::Failed, "{outcome:?}");
        assert!(
            outcome
                .1
                .as_deref()
                .is_some_and(|d| d.contains("unshared while the PR was read")),
            "{outcome:?}"
        );
        assert_eq!(
            journey.recorder.prs_since(mark),
            vec![(format!("GET {}", prs_path()), Some(200))]
        );
        no_pr_result_left(&journey.recorder.since(mark), "the stale job");
        journey.assert_reads(BOB, None, None).await;

        // The real server refuses the bodies of the earlier revision as well.
        for (path, body) in [
            (format!("{}/status", prs_path()), &old_rev_status),
            (comments_path(), &old_rev_comments),
        ] {
            let (status, answer) = journey
                .central
                .post(ALICE, &path, serde_json::to_vec(body).unwrap())
                .await;
            assert_eq!(status, 409, "{path}: {answer}");
        }
        journey.assert_reads(BOB, None, None).await;

        // A sync admitted under the new revision publishes again.
        let current = journey.snapshot().await;
        assert_eq!(current["rev"].as_u64(), Some(reshared.rev));
        let (status, detail) = execute_signed(&host, sync()).await;
        assert_eq!(status, OpStatus::Executed, "{detail:?}");
        let title = format!("journey-title-{}", last_call(is_status_read));
        let comment = format!("journey-review-{}", last_call(is_comments_read));
        for line in [status_line(), comments_line()] {
            assert_eq!(
                journey.recorder.last_body(&line)["publication"],
                current,
                "{line}"
            );
        }
        journey
            .assert_reads(BOB, Some(&title), Some(&comment))
            .await;
        journey.assert_denied(EVE).await;
        only_the_authorized_target_was_read();
    });
}
