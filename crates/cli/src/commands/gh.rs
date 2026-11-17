//! Fetch GitHub PR status via the local `gh` CLI.
//!
//! Rationale (roadmap Wave 2.1): the server holds ZERO GitHub credentials.
//! Instead the CLI — which runs per-user where `gh` is authed as that user —
//! shells out to `gh pr view <url> --json …` and pushes the resulting snapshot
//! up to the server. This module is the `gh`-on-the-CLI half of that.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

/// How many times a *transient* `gh` failure is retried before giving up.
/// Permanent failures (PR/repo not found, bad auth) are never retried.
const GH_MAX_ATTEMPTS: u32 = 4;

/// Base backoff before the first retry. Doubled each attempt, so with
/// [`GH_MAX_ATTEMPTS`] = 4 the waits are ~1s, 2s, 4s (≈7s worst case per call).
const GH_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// PR status snapshot pushed to the server. Mirrors the server-side `PrStatus`
/// DTO: `{ state, merged, draft, title }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatus {
    /// `gh`'s `state` field: `OPEN` / `CLOSED` / `MERGED`.
    pub state: String,
    /// Merged when `state == "MERGED"` OR `mergedAt` is non-null.
    pub merged: bool,
    /// `gh`'s `isDraft` field.
    pub draft: bool,
    /// PR title.
    pub title: String,
    pub review_decision: Option<String>,
}

/// The subset of `gh pr view … --json state,isDraft,title,mergedAt,number,reviewDecision`
/// that we map into [`PrStatus`].
#[derive(Debug, Deserialize)]
struct GhPrView {
    state: String,
    #[serde(rename = "reviewDecision", default)]
    review_decision: Option<String>,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    title: String,
    /// ISO-8601 timestamp string when merged; `null` otherwise.
    #[serde(rename = "mergedAt")]
    merged_at: Option<String>,
}

/// Run `gh pr view <number> --repo <owner>/<repo> --json …` and map the result to
/// a [`PrStatus`]. The argument vector is built from validated pieces; callers
/// pass an already validated target. A missing binary or non-zero exit is an error.
pub fn fetch_pr_status_for(owner: &str, repo: &str, number: i64) -> Result<PrStatus> {
    let stdout = run_gh(&[
        "pr",
        "view",
        &number.to_string(),
        "--repo",
        &format!("{owner}/{repo}"),
        "--json",
        "state,isDraft,title,mergedAt,number,reviewDecision",
    ])?;
    parse_gh_pr_json(&stdout)
        .with_context(|| format!("parsing `gh pr view {owner}/{repo}#{number}` output"))
}

/// Pure JSON → [`PrStatus`] mapping, split out so it is testable without
/// invoking `gh`.
///
/// Mapping:
/// - `state` ← `state` (`OPEN` / `CLOSED` / `MERGED`).
/// - `merged` ← `state == "MERGED"` OR `mergedAt` is non-null.
/// - `draft` ← `isDraft`.
/// - `title` ← `title`.
pub fn parse_gh_pr_json(json: &str) -> Result<PrStatus> {
    let view: GhPrView = serde_json::from_str(json).context("unexpected `gh pr view` JSON")?;
    let merged = view.state == "MERGED" || view.merged_at.is_some();
    Ok(PrStatus {
        state: view.state,
        merged,
        draft: view.is_draft,
        title: view.title,
        review_decision: view.review_decision,
    })
}

/// One PR comment pushed to the server. Mirrors the wire contract:
/// `{ comment_id, kind, author, body, html_url, created_at }` plus the optional
/// inline-review-comment context (`path`, `line`, `diff_hunk`, `resolved`),
/// populated only for `kind == "review_comment"`. The optional fields are
/// omitted from the JSON when `None` (`skip_serializing_if`), keeping issue /
/// review payloads lean and compatible with older servers.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct PrComment {
    /// Stable, source-unique id (prefixed by kind, e.g. `issue:<id>`).
    pub comment_id: String,
    /// `"issue"` | `"review"` | `"review_comment"`.
    pub kind: String,
    /// GitHub login of the comment author.
    pub author: String,
    /// Comment body (markdown).
    pub body: String,
    /// Permalink to the comment.
    pub html_url: String,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// File the inline review comment hangs on (`review_comment` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Line in `path` the comment hangs on (`line`, falling back to
    /// `originalLine` for an outdated thread). `review_comment` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    /// The diff snippet GitHub shows above the comment. `review_comment` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_hunk: Option<String>,
    /// Whether the review thread is resolved. `review_comment` only;
    /// `None` for issue / review kinds (and legacy pushes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<bool>,
}

// --- gh `pr view comments,reviews` shapes -----------------------------------

/// `gh pr view <n> --json comments,reviews` top-level.
#[derive(Debug, Deserialize)]
struct GhPrComments {
    #[serde(default)]
    comments: Vec<GhIssueComment>,
    #[serde(default)]
    reviews: Vec<GhReview>,
}

/// An issue comment from `gh pr view … --json comments`. `gh` emits `author`
/// as an object with a `login`, plus `body`, `url`, and `createdAt`.
#[derive(Debug, Deserialize)]
struct GhIssueComment {
    #[serde(default)]
    id: String,
    #[serde(default)]
    author: GhAuthor,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "createdAt", default)]
    created_at: String,
}

/// A review summary from `gh pr view … --json reviews`. Only reviews with a
/// non-empty `body` become a comment (kind `review`).
#[derive(Debug, Deserialize)]
struct GhReview {
    #[serde(default)]
    id: String,
    #[serde(default)]
    author: GhAuthor,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "submittedAt", default)]
    submitted_at: String,
}

/// `gh`'s author object: `{ "login": "<string>" }`.
#[derive(Debug, Default, Deserialize)]
struct GhAuthor {
    #[serde(default)]
    login: String,
}

// --- gh `api graphql` reviewThreads shapes ----------------------------------
//
// Inline review comments come from the GraphQL `reviewThreads` connection
// rather than the REST `pulls/<n>/comments` endpoint, because only GraphQL
// exposes the thread's `isResolved` state alongside each comment's `path`,
// `line` / `originalLine`, and `diffHunk`. We key the comment id on
// `databaseId` so it matches the numeric id the REST path used historically.

/// Top-level `gh api graphql` envelope: `{ "data": { ... } }`.
#[derive(Debug, Deserialize)]
struct GhGraphqlResponse {
    #[serde(default)]
    data: GhGraphqlData,
}

#[derive(Debug, Default, Deserialize)]
struct GhGraphqlData {
    #[serde(default)]
    repository: Option<GhRepository>,
}

#[derive(Debug, Deserialize)]
struct GhRepository {
    #[serde(rename = "pullRequest", default)]
    pull_request: Option<GhPullRequest>,
}

#[derive(Debug, Deserialize)]
struct GhPullRequest {
    #[serde(rename = "reviewThreads", default)]
    review_threads: GhReviewThreadConnection,
}

#[derive(Debug, Default, Deserialize)]
struct GhReviewThreadConnection {
    #[serde(default)]
    nodes: Vec<GhReviewThread>,
}

#[derive(Debug, Deserialize)]
struct GhReviewThread {
    #[serde(rename = "isResolved", default)]
    is_resolved: bool,
    #[serde(default)]
    comments: GhThreadCommentConnection,
}

#[derive(Debug, Default, Deserialize)]
struct GhThreadCommentConnection {
    #[serde(default)]
    nodes: Vec<GhThreadComment>,
}

/// One comment node inside a review thread. `author` may be `null` (a deleted /
/// ghost user). `line` is `null` for an outdated thread, in which case we fall
/// back to `originalLine`.
#[derive(Debug, Deserialize)]
struct GhThreadComment {
    #[serde(rename = "databaseId", default)]
    database_id: Option<i64>,
    #[serde(default)]
    author: Option<GhAuthor>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "createdAt", default)]
    created_at: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<i64>,
    #[serde(rename = "originalLine", default)]
    original_line: Option<i64>,
    #[serde(rename = "diffHunk", default)]
    diff_hunk: Option<String>,
}

/// GraphQL query for a PR's review threads — the inline code-review comments,
/// each carrying its thread's `isResolved` state plus the comment's file
/// (`path`), `line` (`originalLine` when outdated), and `diffHunk`. Only GraphQL
/// exposes resolution + diff context together; the REST `pulls/<n>/comments`
/// endpoint does not.
const REVIEW_THREADS_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          isResolved
          comments(first: 100) {
            nodes {
              databaseId
              author { login }
              body
              url
              createdAt
              path
              line
              originalLine
              diffHunk
            }
          }
        }
      }
    }
  }
}
"#;

/// Fetch every comment on a PR via the local `gh` CLI and merge the three
/// sources into one `Vec<PrComment>`:
///   - issue comments (`gh pr view … --json comments`) → kind `issue`,
///   - reviews with a non-empty body (`… --json reviews`) → kind `review`,
///   - inline review comments (`gh api graphql` reviewThreads) → kind
///     `review_comment`, carrying `path` / `line` / `diff_hunk` / `resolved`.
///
/// Graceful degradation: if `gh` is missing or a single source fails, that
/// source contributes nothing and a warning is logged — the function returns
/// whatever it could gather (possibly empty) and never errors the whole sync.
pub fn fetch_pr_comments(owner: &str, repo: &str, number: i64) -> Result<Vec<PrComment>> {
    let mut out = Vec::new();
    let label = format!("{owner}/{repo}#{number}");

    // Source 1 + 2: issue comments and review summaries via `gh pr view`.
    match run_gh(&[
        "pr",
        "view",
        &number.to_string(),
        "--repo",
        &format!("{owner}/{repo}"),
        "--json",
        "comments,reviews",
    ]) {
        Ok(stdout) => match parse_gh_pr_comments_json(&stdout) {
            Ok(mut comments) => out.append(&mut comments),
            Err(e) => live_error!("  {label}: parsing `gh pr view` comments failed: {e:#}"),
        },
        Err(e) => live_error!("  {label}: `gh pr view` comments fetch failed: {e:#}"),
    }

    // Source 3: inline review comments via the GraphQL reviewThreads API (so we
    // get resolution state + file/line/diff context the REST endpoint omits).
    match run_gh(&[
        "api",
        "graphql",
        "-f",
        &format!("query={REVIEW_THREADS_QUERY}"),
        "-f",
        &format!("owner={owner}"),
        "-f",
        &format!("repo={repo}"),
        "-F",
        &format!("number={number}"),
    ]) {
        Ok(stdout) => match parse_gh_review_threads_json(&stdout) {
            Ok(mut comments) => out.append(&mut comments),
            Err(e) => live_error!("  {label}: parsing review threads failed: {e:#}"),
        },
        Err(e) => live_error!("  {label}: review threads fetch failed: {e:#}"),
    }

    Ok(out)
}

/// Classification of a failed `gh` invocation, deciding whether a retry could
/// plausibly succeed.
enum GhFailure {
    /// Worth retrying after a backoff: network blip, DNS hiccup, 5xx, or a
    /// (secondary) rate-limit / abuse throttle.
    Transient(String),
    /// Won't get better on retry: PR/repo doesn't exist, bad credentials,
    /// missing `gh` binary, malformed query.
    Permanent(String),
}

/// Run `gh` with the given args, retrying *transient* failures with exponential
/// backoff. Permanent failures (PR/repo not found, bad auth, missing binary)
/// return immediately so a non-existent PR never triggers a retry storm. Either
/// way the final error is an `anyhow` error so the caller can log-and-continue
/// per source.
fn run_gh(args: &[&str]) -> Result<String> {
    run_gh_with_backoff(args, GH_MAX_ATTEMPTS, GH_BACKOFF_BASE)
}

/// [`run_gh`] with the retry budget and base delay injected, so the policy is
/// testable and tunable. Sleeps on the calling thread between attempts — every
/// `gh` call already runs on a blocking thread (`spawn_blocking` / the sync
/// `pr sync` path), so a short sleep here is fine.
fn run_gh_with_backoff(args: &[&str], max_attempts: u32, base: Duration) -> Result<String> {
    let mut attempt: u32 = 1;
    loop {
        match gh_attempt(args) {
            Ok(stdout) => return Ok(stdout),
            // Permanent: don't waste retries — surface it immediately.
            Err(GhFailure::Permanent(msg)) => return Err(gh_error(args, &msg)),
            Err(GhFailure::Transient(msg)) => {
                if attempt >= max_attempts {
                    return Err(gh_error(args, &msg)
                        .context(format!("gave up after {max_attempts} attempt(s)")));
                }
                // Exponential backoff: base * 2^(attempt-1) → base, 2·base, 4·base…
                let delay = base.saturating_mul(1u32 << (attempt - 1));
                live_error!(
                    "  `gh {}` transient failure (attempt {attempt}/{max_attempts}): {msg} — retrying in {:.1}s",
                    gh_operation(args),
                    delay.as_secs_f64()
                );
                std::thread::sleep(delay);
                attempt += 1;
            }
        }
    }
}

/// A single `gh` invocation, classifying any failure as transient or permanent.
fn gh_attempt(args: &[&str]) -> std::result::Result<String, GhFailure> {
    let output = match std::process::Command::new("gh").args(args).output() {
        Ok(o) => o,
        Err(e) => {
            // A missing binary / spawn failure won't fix itself — permanent.
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                "`gh` not found on PATH — install the GitHub CLI and run `gh auth login`"
                    .to_string()
            } else {
                format!("failed to invoke `gh`: {e}")
            };
            return Err(GhFailure::Permanent(msg));
        }
    };

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if is_permanent_gh_error(&stderr) {
        Err(GhFailure::Permanent(stderr))
    } else {
        Err(GhFailure::Transient(stderr))
    }
}

/// Identify the operation without printing query bodies, fields, or other arguments.
fn gh_operation(args: &[&str]) -> String {
    args.iter()
        .take(2)
        .take_while(|arg| !arg.starts_with('-'))
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format a `gh` failure into the `anyhow` error the callers print.
fn gh_error(args: &[&str], stderr: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`gh {}` failed{}{}",
        gh_operation(args),
        if stderr.is_empty() { "" } else { ": " },
        stderr
    )
}

/// Whether a failed `gh` invocation is permanent — retrying it would only spam
/// the same error. Everything not matched here defaults to *transient* so a
/// genuine network/server blip (even a novel one) still gets a bounded retry.
///
/// Deliberately does NOT treat a bare HTTP 403 as permanent: GitHub serves
/// rate-limit / abuse throttling as 403, which IS retryable and is caught by
/// the transient default.
fn is_permanent_gh_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    const PERMANENT: &[&str] = &[
        // GraphQL "Could not resolve to a PullRequest/Repository/…" — the
        // object doesn't exist; this is the #739 case.
        "could not resolve to a",
        // `gh pr view <branch>` when the branch has no PR.
        "no pull requests found",
        "http 404",
        "http 401",
        "http 422",
        "bad credentials",
        "authentication required",
        "gh auth login",
        "no commits between",
    ];
    PERMANENT.iter().any(|needle| s.contains(needle))
}

/// Pure mapping for `gh pr view … --json comments,reviews` → issue + review
/// [`PrComment`]s. Reviews with an empty body are dropped (they carry no text,
/// only an approval/changes state). Split out so it is testable without `gh`.
pub fn parse_gh_pr_comments_json(json: &str) -> Result<Vec<PrComment>> {
    let view: GhPrComments =
        serde_json::from_str(json).context("unexpected `gh pr view` comments JSON")?;
    let mut out = Vec::new();
    for c in view.comments {
        out.push(PrComment {
            comment_id: format!("issue:{}", c.id),
            kind: "issue".to_string(),
            author: c.author.login,
            body: c.body,
            html_url: c.url,
            created_at: c.created_at,
            ..Default::default()
        });
    }
    for r in view.reviews {
        // Reviews with no body are pure state changes (approve/request-changes)
        // — skip them; only commented reviews carry text.
        if r.body.trim().is_empty() {
            continue;
        }
        out.push(PrComment {
            comment_id: format!("review:{}", r.id),
            kind: "review".to_string(),
            author: r.author.login,
            body: r.body,
            html_url: r.url,
            created_at: r.submitted_at,
            ..Default::default()
        });
    }
    Ok(out)
}

/// Pure mapping for `gh api graphql` reviewThreads → inline review
/// [`PrComment`]s (kind `review_comment`), each carrying its thread's
/// `resolved` state and the comment's `path` / `line` / `diff_hunk`. Comments
/// with no `databaseId` (shouldn't happen, but the field is nullable in the
/// schema) are skipped. Split out so it is testable without `gh`.
pub fn parse_gh_review_threads_json(json: &str) -> Result<Vec<PrComment>> {
    let resp: GhGraphqlResponse =
        serde_json::from_str(json).context("unexpected `gh api graphql` reviewThreads JSON")?;
    let mut out = Vec::new();
    let Some(pr) = resp.data.repository.and_then(|r| r.pull_request) else {
        // No repository / PR in the payload (e.g. an errors-only response) —
        // contribute nothing rather than erroring the whole sync.
        return Ok(out);
    };
    for thread in pr.review_threads.nodes {
        let resolved = thread.is_resolved;
        for c in thread.comments.nodes {
            let Some(id) = c.database_id else { continue };
            out.push(PrComment {
                comment_id: format!("review_comment:{id}"),
                kind: "review_comment".to_string(),
                author: c.author.map(|a| a.login).unwrap_or_default(),
                body: c.body,
                html_url: c.url,
                created_at: c.created_at,
                path: c.path,
                // `line` is null for an outdated thread — fall back to the
                // original line so the comment still shows a location.
                line: c.line.or(c.original_line),
                diff_hunk: c.diff_hunk,
                resolved: Some(resolved),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn review_decision_roundtrip() {
        for decision in ["APPROVED", "CHANGES_REQUESTED", "REVIEW_REQUIRED", ""] {
            let json = serde_json::json!({"state":"OPEN", "isDraft":false, "title":"Test", "mergedAt":null, "reviewDecision":decision});
            assert_eq!(
                parse_gh_pr_json(&json.to_string())
                    .unwrap()
                    .review_decision
                    .as_deref(),
                Some(decision)
            );
        }
        let legacy = r#"{"state":"OPEN","isDraft":false,"title":"Test","mergedAt":null}"#;
        assert_eq!(parse_gh_pr_json(legacy).unwrap().review_decision, None);
    }

    #[test]
    fn graphql_failure_keeps_diagnostic_without_dumping_query() {
        let error = super::gh_error(
            &[
                "api",
                "graphql",
                "-f",
                "query=query {\n repository {\n id\n }\n}",
                "-f",
                "owner=example",
            ],
            "gh: HTTP 502: Bad Gateway\nTry again later",
        );
        assert_eq!(
            error.to_string(),
            "`gh api graphql` failed: gh: HTTP 502: Bad Gateway\nTry again later"
        );
    }

    use super::*;

    #[test]
    fn parses_open_pr() {
        let json =
            r#"{"state":"OPEN","isDraft":false,"title":"Add gh.rs","mergedAt":null,"number":42}"#;
        let s = parse_gh_pr_json(json).unwrap();
        assert_eq!(s.state, "OPEN");
        assert!(!s.merged);
        assert!(!s.draft);
        assert_eq!(s.title, "Add gh.rs");
    }

    #[test]
    fn parses_open_draft_pr() {
        let json = r#"{"state":"OPEN","isDraft":true,"title":"WIP","mergedAt":null,"number":7}"#;
        let s = parse_gh_pr_json(json).unwrap();
        assert_eq!(s.state, "OPEN");
        assert!(!s.merged);
        assert!(s.draft);
        assert_eq!(s.title, "WIP");
    }

    #[test]
    fn parses_closed_unmerged_pr() {
        let json =
            r#"{"state":"CLOSED","isDraft":false,"title":"Rejected","mergedAt":null,"number":9}"#;
        let s = parse_gh_pr_json(json).unwrap();
        assert_eq!(s.state, "CLOSED");
        assert!(!s.merged, "closed-but-not-merged must not be merged");
        assert!(!s.draft);
    }

    #[test]
    fn parses_merged_pr_via_state() {
        let json = r#"{"state":"MERGED","isDraft":false,"title":"Shipped","mergedAt":"2026-06-07T12:00:00Z","number":11}"#;
        let s = parse_gh_pr_json(json).unwrap();
        assert_eq!(s.state, "MERGED");
        assert!(s.merged);
        assert!(!s.draft);
        assert_eq!(s.title, "Shipped");
    }

    #[test]
    fn merged_at_set_implies_merged_even_if_state_lags() {
        // Defensive: mergedAt non-null counts as merged regardless of `state`.
        let json = r#"{"state":"CLOSED","isDraft":false,"title":"Edge","mergedAt":"2026-06-07T12:00:00Z","number":12}"#;
        let s = parse_gh_pr_json(json).unwrap();
        assert!(s.merged, "non-null mergedAt must imply merged");
    }

    #[test]
    fn rejects_garbage_json() {
        assert!(parse_gh_pr_json("not json").is_err());
        assert!(parse_gh_pr_json("{}").is_err());
    }

    // --- PR comment mapping tests -------------------------------------------

    #[test]
    fn parses_issue_comments_and_review_bodies() {
        let json = r#"{
            "comments": [
                {"id":"IC_1","author":{"login":"alice"},"body":"first thought",
                 "url":"https://github.com/o/r/pull/1#issuecomment-1","createdAt":"2026-06-01T10:00:00Z"}
            ],
            "reviews": [
                {"id":"RV_2","author":{"login":"bob"},"body":"LGTM with a nit",
                 "url":"https://github.com/o/r/pull/1#pullrequestreview-2","submittedAt":"2026-06-02T11:00:00Z"},
                {"id":"RV_3","author":{"login":"carol"},"body":"",
                 "url":"https://github.com/o/r/pull/1#pullrequestreview-3","submittedAt":"2026-06-02T12:00:00Z"}
            ]
        }"#;
        let got = parse_gh_pr_comments_json(json).unwrap();
        // Two kept: one issue, one review-with-body. The empty-body review is dropped.
        assert_eq!(got.len(), 2);

        let issue = &got[0];
        assert_eq!(issue.comment_id, "issue:IC_1");
        assert_eq!(issue.kind, "issue");
        assert_eq!(issue.author, "alice");
        assert_eq!(issue.body, "first thought");
        assert_eq!(
            issue.html_url,
            "https://github.com/o/r/pull/1#issuecomment-1"
        );
        assert_eq!(issue.created_at, "2026-06-01T10:00:00Z");

        let review = &got[1];
        assert_eq!(review.comment_id, "review:RV_2");
        assert_eq!(review.kind, "review");
        assert_eq!(review.author, "bob");
        assert_eq!(review.body, "LGTM with a nit");
        assert_eq!(review.created_at, "2026-06-02T11:00:00Z");
    }

    #[test]
    fn parses_review_threads_with_resolution_and_line_context() {
        // One unresolved thread (a live comment on line 41) and one resolved
        // thread whose comment is outdated (`line: null`, falls back to
        // `originalLine`). Both become `review_comment`s carrying their state.
        let json = r#"{
            "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [
                { "isResolved": false, "comments": { "nodes": [
                    {"databaseId":12345,"author":{"login":"coderabbitai"},
                     "body":"This assertion is off by one.",
                     "url":"https://github.com/o/r/pull/1#discussion_r12345",
                     "createdAt":"2026-06-03T09:00:00Z",
                     "path":"crates/app-operator/tests/service_test.rs",
                     "line":41,"originalLine":40,
                     "diffHunk":"@@ -38,4 +38,4 @@\n-  old\n+  new"}
                ] } },
                { "isResolved": true, "comments": { "nodes": [
                    {"databaseId":999,"author":{"login":"dave"},
                     "body":"nit: rename","url":"https://github.com/o/r/pull/1#discussion_r999",
                     "createdAt":"2026-06-02T08:00:00Z",
                     "path":"src/lib.rs","line":null,"originalLine":12,"diffHunk":"@@ x @@"}
                ] } }
            ] } } } }
        }"#;
        let got = parse_gh_review_threads_json(json).unwrap();
        assert_eq!(got.len(), 2);

        let open = &got[0];
        assert_eq!(open.comment_id, "review_comment:12345");
        assert_eq!(open.kind, "review_comment");
        assert_eq!(open.author, "coderabbitai");
        assert_eq!(
            open.path.as_deref(),
            Some("crates/app-operator/tests/service_test.rs")
        );
        assert_eq!(open.line, Some(41));
        assert_eq!(open.resolved, Some(false));
        assert!(open.diff_hunk.as_deref().unwrap().contains("@@"));

        let resolved = &got[1];
        assert_eq!(resolved.comment_id, "review_comment:999");
        assert_eq!(resolved.resolved, Some(true));
        // `line` was null → falls back to originalLine.
        assert_eq!(resolved.line, Some(12));
    }

    #[test]
    fn empty_comment_sources_yield_empty_vecs() {
        let view = parse_gh_pr_comments_json(r#"{"comments":[],"reviews":[]}"#).unwrap();
        assert!(view.is_empty());
        // Empty reviewThreads, and a missing repository/PR, both yield nothing.
        let threads = parse_gh_review_threads_json(
            r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]}}}}}"#,
        )
        .unwrap();
        assert!(threads.is_empty());
        let null_pr =
            parse_gh_review_threads_json(r#"{"data":{"repository":{"pullRequest":null}}}"#)
                .unwrap();
        assert!(null_pr.is_empty());
    }

    // --- gh failure classification -----------------------------------------

    #[test]
    fn not_found_failures_are_permanent() {
        // The #739 case: GraphQL can't resolve the PR → never retry.
        assert!(is_permanent_gh_error(
            "GraphQL: Could not resolve to a PullRequest with the number of 739. (repository.pullRequest)"
        ));
        assert!(is_permanent_gh_error(
            "GraphQL: Could not resolve to a Repository with the name 'nope'."
        ));
        assert!(is_permanent_gh_error("no pull requests found for branch"));
        assert!(is_permanent_gh_error("HTTP 404: Not Found"));
    }

    #[test]
    fn auth_failures_are_permanent() {
        assert!(is_permanent_gh_error("HTTP 401: Bad credentials"));
        assert!(is_permanent_gh_error(
            "To get started with GitHub CLI, please run: gh auth login"
        ));
    }

    #[test]
    fn network_and_rate_limit_failures_are_transient() {
        // Default-transient: anything not on the permanent list is retried.
        assert!(!is_permanent_gh_error(
            "error connecting to api.github.com: dial tcp: i/o timeout"
        ));
        assert!(!is_permanent_gh_error("dial tcp: connection reset by peer"));
        assert!(!is_permanent_gh_error("HTTP 502: Bad Gateway"));
        assert!(!is_permanent_gh_error("HTTP 503: Service Unavailable"));
        // GitHub serves rate limiting as 403 — must NOT be permanent.
        assert!(!is_permanent_gh_error(
            "HTTP 403: API rate limit exceeded for user"
        ));
        assert!(!is_permanent_gh_error(
            "You have exceeded a secondary rate limit"
        ));
        // An unknown/empty failure defaults to transient (bounded retry).
        assert!(!is_permanent_gh_error(
            "some brand new error we've never seen"
        ));
        assert!(!is_permanent_gh_error(""));
    }

    #[test]
    fn classification_is_case_insensitive() {
        assert!(is_permanent_gh_error("COULD NOT RESOLVE TO A PULLREQUEST"));
        assert!(is_permanent_gh_error("http 404"));
    }

    #[test]
    fn rejects_garbage_comment_json() {
        assert!(parse_gh_pr_comments_json("not json").is_err());
        assert!(parse_gh_review_threads_json("not json").is_err());
        // A `pr view` comments object is not a graphql envelope.
        assert!(parse_gh_review_threads_json(r#"{"comments":[]}"#)
            .unwrap()
            .is_empty());
    }
}
