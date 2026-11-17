//! GitHub PR helpers — URL parsing + the `PrStatus` DTO.
//!
//! The server holds **zero** GitHub credentials. PR status is fetched by the
//! per-user CLI (via `gh`, authed as that user) and pushed up to the server,
//! which only caches it (see `session_prs` status columns) and serves the
//! cached snapshot. `PrStatus` is therefore a pure data-transfer object: it is
//! stored in / read from the DB and (de)serialized on the PR routes — nothing
//! in this crate fetches it over the network.

use serde::Deserialize;

use crate::store::SessionPr;

/// Cached state of a GitHub pull request, pushed by the CLI.
///
/// Matches the fields the UI needs. `state` is `"open"` or `"closed"`. This is
/// a pure DTO — the server never fetches it; it is stored in `session_prs` and
/// served back as the cached snapshot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, Deserialize)]
pub struct PrStatus {
    /// `"open"` or `"closed"`.
    pub state: String,
    /// `true` when the PR has been merged (GitHub sets state=closed+merged=true).
    pub merged: bool,
    /// `true` for a draft PR.
    pub draft: bool,
    /// PR title.
    pub title: String,
    /// GitHub review decision, absent for older clients.
    #[serde(default)]
    pub review_decision: Option<String>,
}

/// A single GitHub PR comment, fetched by the per-user CLI (via `gh`) and pushed
/// up to the server, which only caches it (see `session_pr_comments`) and serves
/// it back. Pure DTO — nothing in this crate fetches it over the network.
///
/// `kind` is a plain string with values `issue` | `review` | `review_comment`
/// (an issue-level PR comment, a review summary, or an inline review comment).
/// `created_at` is the verbatim RFC3339 timestamp `gh` reports for the comment.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, Deserialize)]
pub struct PrComment {
    /// Stable GitHub comment id (stringified — comment ids are large integers).
    pub comment_id: String,
    /// `issue` | `review` | `review_comment`.
    pub kind: String,
    /// Comment author login.
    pub author: String,
    /// Comment body (Markdown).
    pub body: String,
    /// Permalink to the comment on GitHub.
    pub html_url: String,
    /// RFC3339 timestamp `gh` reports for when the comment was created.
    pub created_at: String,
    /// File the inline review comment hangs on (`review_comment` only).
    /// `None` for issue / review kinds, and for pushes from older CLIs.
    #[serde(default)]
    pub path: Option<String>,
    /// Line in `path` the inline review comment hangs on. `review_comment` only.
    #[serde(default)]
    pub line: Option<i64>,
    /// The diff snippet GitHub shows above the inline review comment.
    /// `review_comment` only.
    #[serde(default)]
    pub diff_hunk: Option<String>,
    /// Whether the review thread is resolved. `review_comment` only; `None`
    /// for issue / review kinds and legacy pushes.
    #[serde(default)]
    pub resolved: Option<bool>,
}

/// Parse a GitHub PR URL into `(owner, repo, number)`. Tolerates trailing
/// path segments (e.g. `/files`, `/commits/…`) and query strings.
///
/// Valid form: `https://github.com/{owner}/{repo}/pull/{number}[/…][?…]`
///
/// Returns `None` for anything that doesn't match (non-PR paths like `/issues/`,
/// garbage URLs, negative or zero numbers).
pub fn parse_pr_url(url: &str) -> Option<(String, String, i64)> {
    // Strip any fragment first.
    let url = url.split('#').next().unwrap_or(url).trim();

    // Must start with the GitHub host (http or https).
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;

    // Split off query string before splitting the path.
    let path = path.split('?').next().unwrap_or(path);

    // The path segments: owner / repo / pull / number [/ optional_extra…]
    let mut parts = path.splitn(5, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?.to_string();
    let repo = parts.next().filter(|s| !s.is_empty())?.to_string();
    let segment = parts.next().filter(|s| *s == "pull")?; // must be "pull"
    let _ = segment;
    let number_str = parts.next().filter(|s| !s.is_empty())?;
    // The number part may still have a trailing `/files` etc. from splitn(5, …)
    // only giving us 5 parts, so take until the first `/`.
    let number_str = number_str.split('/').next().unwrap_or(number_str);
    let number: i64 = number_str.parse().ok().filter(|&n| n > 0)?;

    Some((owner, repo, number))
}

/// Parse a free-form search query into a PR reference `(owner, repo, number)`
/// if it looks like one. Used by `list_sessions` so that pasting a PR URL (or
/// the `owner/repo#N` shorthand) into the search box matches sessions linked to
/// that PR in `session_prs`, not just text that happens to contain the string.
///
/// Recognized forms:
/// - a full GitHub PR URL (`https://github.com/{owner}/{repo}/pull/{n}`),
///   with or without the `https://` / `http://` scheme,
/// - the shorthand `{owner}/{repo}#{n}`.
///
/// Returns `None` for anything else, so the caller falls back to plain text
/// search.
pub fn parse_pr_query(query: &str) -> Option<(String, String, i64)> {
    let q = query.trim();

    // Full PR URL, with or without an explicit scheme.
    if let Some(parsed) = parse_pr_url(q) {
        return Some(parsed);
    }
    if let Some(rest) = q.strip_prefix("github.com/") {
        if let Some(parsed) = parse_pr_url(&format!("https://github.com/{rest}")) {
            return Some(parsed);
        }
    }

    // Shorthand: owner/repo#number (exactly two path segments before `#`).
    let (repo_part, number_str) = q.rsplit_once('#')?;
    let number: i64 = number_str.trim().parse().ok().filter(|&n| n > 0)?;
    let mut seg = repo_part.trim().split('/');
    let owner = seg.next().filter(|s| !s.is_empty())?.to_string();
    let repo = seg.next().filter(|s| !s.is_empty())?.to_string();
    if seg.next().is_some() {
        return None; // more than owner/repo before the `#`
    }
    Some((owner, repo, number))
}

/// Extract `(owner, repo)` from a git remote URL.
///
/// Accepts the common GitHub remote forms, with or without a trailing `.git`:
/// - `git@github.com:owner/repo.git` (scp-like SSH)
/// - `ssh://git@github.com/owner/repo.git`
/// - `https://github.com/owner/repo(.git)`
/// - `http://github.com/owner/repo`
///
/// Returns `None` for non-GitHub hosts or anything that doesn't yield exactly an
/// `owner/repo` pair. The returned `repo` has any trailing `.git` stripped.
pub fn parse_repo_remote(remote_url: &str) -> Option<(String, String)> {
    let url = remote_url.trim();

    // Reduce every accepted GitHub remote form to the `owner/repo[...]` tail
    // after the host. The `git@github.com:` (scp-like SSH) prefix uses a colon;
    // the rest are URL-shaped.
    const PREFIXES: [&str; 6] = [
        "git@github.com:",
        "ssh://git@github.com/",
        "ssh://github.com/",
        "https://github.com/",
        "http://github.com/",
        "git://github.com/",
    ];
    let tail = PREFIXES.iter().find_map(|p| url.strip_prefix(p))?;

    // Drop any query string / fragment.
    let tail = tail.split(['?', '#']).next().unwrap_or(tail);

    let mut parts = tail.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?.to_string();
    let repo = parts.next().filter(|s| !s.is_empty())?;
    // Strip a trailing `.git` (and any further path segments are ignored).
    let repo = repo.strip_suffix(".git").unwrap_or(repo).to_string();
    if repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

/// Scan free text for GitHub PR references and return the deduped set as
/// [`SessionPr`]s. Pure text-parse — performs **no** network I/O.
///
/// Two reference forms are detected:
/// - **Full GitHub PR URLs** (`https://github.com/{owner}/{repo}/pull/{n}`) are
///   always matched (via [`parse_pr_url`], which already rejects `/issues/`).
/// - **`PR #N` / `pull request #N`** (case-insensitive, optional whitespace
///   before `#`) are matched **only** when `repo_ctx` is `Some((owner, repo))`,
///   in which case the canonical
///   `https://github.com/{owner}/{repo}/pull/{n}` URL is built. With
///   `repo_ctx = None` these number-only refs are skipped (the caller passes
///   `None` when the session does not resolve to exactly one repo).
///
/// **Bare `#N` is intentionally NOT matched** (too noisy). The result is deduped
/// by `(owner, repo, number)`; the first-seen `url` for a given key wins (so a
/// full URL seen before a matching `PR #N` keeps the URL verbatim).
///
/// `allowed_owners` gates **full PR URLs** by their `owner`: a URL links only
/// when `allowed_owners` is empty (no known session repos ⇒ legacy
/// link-anything) or contains the URL's owner (case-insensitive). This stops a
/// transcript that merely *quotes* an unrelated PR URL — e.g. a doc example or
/// this repo's own `acme/widgets` / `o/r` test fixtures — from being linked as
/// a real PR, while still allowing any PR within the session's own org(s).
pub fn detect_pr_refs(
    text: &str,
    repo_ctx: Option<(&str, &str)>,
    allowed_owners: &[String],
) -> Vec<SessionPr> {
    let mut out: Vec<SessionPr> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, i64)> =
        std::collections::HashSet::new();

    let mut push = |pr: SessionPr| {
        let key = (pr.owner.clone(), pr.repo.clone(), pr.number);
        if seen.insert(key) {
            out.push(pr);
        }
    };

    // --- Full GitHub PR URLs (gated by allowed_owners). ---
    // Scan for the host marker and feed the surrounding token to parse_pr_url,
    // which tolerates trailing path/query and rejects non-PR paths.
    for marker in ["https://github.com/", "http://github.com/"] {
        let mut search_from = 0usize;
        while let Some(rel) = text[search_from..].find(marker) {
            let start = search_from + rel;
            // End at markup delimiters or JSON quotes/escapes so serialized
            // tool output can contain PR links without whitespace around them.
            let end = text[start..]
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, '<' | '>' | ')' | '*' | '`' | '"' | '\'' | '\\')
                })
                .map(|i| start + i)
                .unwrap_or(text.len());
            // Trim trailing punctuation / markdown wrappers the boundary scan
            // didn't already cut (e.g. a URL ending the text, or `[…]`/`_…_`).
            let token = text[start..end]
                .trim_end_matches([',', '.', ';', ':', ')', ']', '*', '`', '_', '~']);
            if let Some((owner, repo, number)) = parse_pr_url(token) {
                if owner_allowed(&owner, allowed_owners) {
                    let url = format!("https://github.com/{owner}/{repo}/pull/{number}");
                    push(SessionPr {
                        url,
                        owner,
                        repo,
                        number,
                    });
                }
            }
            search_from = end.max(start + marker.len());
        }
    }

    // --- `PR #N` / `pull request #N` (only with a repo context). ---
    if let Some((owner, repo)) = repo_ctx {
        for number in scan_pr_number_refs(text) {
            let url = format!("https://github.com/{owner}/{repo}/pull/{number}");
            push(SessionPr {
                url,
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
            });
        }
    }

    out
}

/// Whether a full PR URL's `owner` is allowed to link. An empty `allowed_owners`
/// means "no known session repos" ⇒ don't restrict (legacy behavior). Otherwise
/// the owner must match one of the session's repo owners (case-insensitive, as
/// GitHub owners are).
fn owner_allowed(owner: &str, allowed_owners: &[String]) -> bool {
    allowed_owners.is_empty() || allowed_owners.iter().any(|o| o.eq_ignore_ascii_case(owner))
}

/// Find every `PR #N` / `pull request #N` reference (case-insensitive, optional
/// whitespace before `#`) in `text`, returning the parsed numbers in order.
///
/// Implements the pinned regex `(?i)\b(?:pull\s+request|pr)\s*#(\d+)\b` by hand
/// (no `regex` dependency). Bare `#N` is never matched — a `PR`/`pull request`
/// keyword must precede the `#`.
fn scan_pr_number_refs(text: &str) -> Vec<i64> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut nums = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        // Try to match a keyword at position `i`, but only on a word boundary
        // (the byte before `i` must not be alphanumeric/underscore).
        let boundary_before = i == 0 || !is_word_byte(bytes[i - 1]);
        if !boundary_before {
            i += 1;
            continue;
        }

        // `pull request` (with one-or-more whitespace between the two words)…
        let after_kw = if let Some(rest) = match_pull_request(bytes, i) {
            Some(rest)
        } else if bytes[i..].starts_with(b"pr") && !is_word_byte_at(bytes, i + 2) {
            // …or `pr` as a standalone word (byte compare — `i` may be inside a
            // multi-byte char, so never slice the &str here).
            Some(i + 2)
        } else {
            None
        };

        let Some(mut j) = after_kw else {
            i += 1;
            continue;
        };

        // Optional whitespace, then a literal `#`, then ≥1 digits.
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'#' {
            i += 1;
            continue;
        }
        j += 1;
        let digit_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j == digit_start {
            i += 1;
            continue;
        }
        // `\b` after the digits: the next byte must not be a word byte.
        if is_word_byte_at(bytes, j) {
            i += 1;
            continue;
        }
        if let Ok(n) = lower[digit_start..j].parse::<i64>() {
            if n > 0 {
                nums.push(n);
            }
        }
        i = j;
    }

    nums
}

/// Match the keyword `pull` + whitespace + `request` at `start` (lowercased
/// input), returning the byte index just past `request`, or `None`.
fn match_pull_request(bytes: &[u8], start: usize) -> Option<usize> {
    let rest = bytes.get(start..)?;
    let after_pull = rest.strip_prefix(b"pull")?;
    // Require at least one whitespace byte between the words.
    let ws_len = after_pull
        .iter()
        .take_while(|b| b.is_ascii_whitespace())
        .count();
    if ws_len == 0 {
        return None;
    }
    let after_ws = &after_pull[ws_len..];
    let after_request = after_ws.strip_prefix(b"request")?;
    let _ = after_request;
    Some(start + 4 + ws_len + 7)
}

/// A "word" byte for `\b` boundary checks: ASCII alphanumeric or underscore.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Word-byte test at an index, treating out-of-bounds as a (non-word) boundary.
fn is_word_byte_at(bytes: &[u8], idx: usize) -> bool {
    bytes.get(idx).is_some_and(|&b| is_word_byte(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_pr_url unit tests ---

    #[test]
    fn parse_valid_pr_url() {
        let (o, r, n) = parse_pr_url("https://github.com/owner/myrepo/pull/42").unwrap();
        assert_eq!(o, "owner");
        assert_eq!(r, "myrepo");
        assert_eq!(n, 42);
    }

    #[test]
    fn parse_pr_url_trailing_files() {
        let (owner, repo, n) =
            parse_pr_url("https://github.com/owner/myrepo/pull/123/files").unwrap();
        assert_eq!(n, 123);
        assert_eq!(owner, "owner");
        assert_eq!(repo, "myrepo");
    }

    #[test]
    fn parse_pr_url_with_query_string() {
        let (_owner, repo, n) = parse_pr_url("https://github.com/owner/myrepo/pull/7?w=1").unwrap();
        assert_eq!(n, 7);
        assert_eq!(repo, "myrepo");
    }

    #[test]
    fn parse_pr_url_rejects_issues() {
        assert!(
            parse_pr_url("https://github.com/owner/myrepo/issues/42").is_none(),
            "issues path must not parse as PR"
        );
    }

    #[test]
    fn parse_pr_url_rejects_garbage() {
        assert!(parse_pr_url("not a url").is_none());
        assert!(parse_pr_url("https://gitlab.com/owner/repo/pull/1").is_none());
        assert!(parse_pr_url("https://github.com/owner/myrepo/pull/").is_none());
        assert!(parse_pr_url("https://github.com/owner/myrepo/pull/0").is_none());
        assert!(parse_pr_url("https://github.com/owner/myrepo/pull/abc").is_none());
    }

    #[test]
    fn parse_pr_url_http_scheme() {
        // Also accept http:// (uncommon but shouldn't explode).
        let result = parse_pr_url("http://github.com/owner/repo/pull/5");
        assert!(result.is_some());
        let (_, _, n) = result.unwrap();
        assert_eq!(n, 5);
    }

    // --- parse_pr_query unit tests ---

    #[test]
    fn parse_pr_query_full_url() {
        assert_eq!(
            parse_pr_query("https://github.com/Example-Org/product-demo/pull/409"),
            Some(("Example-Org".into(), "product-demo".into(), 409))
        );
    }

    #[test]
    fn parse_pr_query_url_without_scheme() {
        assert_eq!(
            parse_pr_query("github.com/owner/repo/pull/12/files"),
            Some(("owner".into(), "repo".into(), 12))
        );
    }

    #[test]
    fn parse_pr_query_shorthand() {
        assert_eq!(
            parse_pr_query("owner/repo#42"),
            Some(("owner".into(), "repo".into(), 42))
        );
        // Tolerates surrounding whitespace.
        assert_eq!(
            parse_pr_query("  acme/widgets#7  "),
            Some(("acme".into(), "widgets".into(), 7))
        );
    }

    #[test]
    fn parse_pr_query_rejects_non_pr() {
        assert!(parse_pr_query("review pull request").is_none());
        assert!(parse_pr_query("just some text").is_none());
        assert!(parse_pr_query("#409").is_none()); // no owner/repo
        assert!(parse_pr_query("owner/repo#0").is_none()); // non-positive
        assert!(parse_pr_query("owner/repo/extra#1").is_none()); // too many segments
        assert!(parse_pr_query("https://github.com/owner/repo/issues/3").is_none());
    }

    // --- parse_repo_remote unit tests ---

    #[test]
    fn parse_repo_remote_ssh_scp() {
        assert_eq!(
            parse_repo_remote("git@github.com:owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
        assert_eq!(
            parse_repo_remote("git@github.com:owner/repo"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn parse_repo_remote_https() {
        assert_eq!(
            parse_repo_remote("https://github.com/owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
        assert_eq!(
            parse_repo_remote("https://github.com/owner/repo"),
            Some(("owner".into(), "repo".into()))
        );
        assert_eq!(
            parse_repo_remote("http://github.com/owner/repo"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn parse_repo_remote_ssh_url_and_git_scheme() {
        assert_eq!(
            parse_repo_remote("ssh://git@github.com/owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
        assert_eq!(
            parse_repo_remote("git://github.com/owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
    }

    #[test]
    fn parse_repo_remote_rejects_non_github() {
        assert!(parse_repo_remote("git@gitlab.com:owner/repo.git").is_none());
        assert!(parse_repo_remote("https://gitlab.com/owner/repo").is_none());
        assert!(parse_repo_remote("not a url").is_none());
        assert!(parse_repo_remote("https://github.com/owner").is_none());
        assert!(parse_repo_remote("https://github.com/").is_none());
    }

    // --- detect_pr_refs unit tests ---

    #[test]
    fn detect_full_url_always_matched() {
        let refs = detect_pr_refs(
            "see https://github.com/owner/repo/pull/42 thanks",
            None,
            &[],
        );
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].owner, "owner");
        assert_eq!(refs[0].repo, "repo");
        assert_eq!(refs[0].number, 42);
        assert_eq!(refs[0].url, "https://github.com/owner/repo/pull/42");
    }

    #[test]
    fn detect_full_url_with_trailing_path_and_punctuation() {
        let refs = detect_pr_refs("(https://github.com/owner/repo/pull/7/files).", None, &[]);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].number, 7);
        // Canonicalized URL drops the trailing /files.
        assert_eq!(refs[0].url, "https://github.com/owner/repo/pull/7");
    }

    #[test]
    fn detect_full_url_wrapped_in_markdown() {
        // A URL wrapped in markdown bold / italic / code / strikethrough must
        // still link — the trailing `**`, `*`, `` ` ``, `~~`, `_` etc. are not
        // part of the URL. Regression test for PR links silently dropped when
        // the URL was pasted with markdown emphasis.
        for text in [
            "**https://github.com/owner/repo/pull/42**",
            "*https://github.com/owner/repo/pull/42*",
            "`https://github.com/owner/repo/pull/42`",
            "~~https://github.com/owner/repo/pull/42~~",
            "see _https://github.com/owner/repo/pull/42_ here",
            "[https://github.com/owner/repo/pull/42]",
        ] {
            let refs = detect_pr_refs(text, None, &[]);
            assert_eq!(refs.len(), 1, "expected exactly one ref for {text:?}");
            assert_eq!(refs[0].number, 42, "wrong number for {text:?}");
            assert_eq!(
                refs[0].url, "https://github.com/owner/repo/pull/42",
                "wrong canonical url for {text:?}"
            );
        }
    }

    #[test]
    fn detect_pr_n_and_pull_request_n_with_repo_ctx() {
        let ctx = Some(("acme", "widgets"));
        let refs = detect_pr_refs("fixed in PR #193 and pull request #194", ctx, &[]);
        let nums: Vec<i64> = refs.iter().map(|r| r.number).collect();
        assert!(nums.contains(&193), "PR #193 must match, got {nums:?}");
        assert!(
            nums.contains(&194),
            "pull request #194 must match, got {nums:?}"
        );
        let one = refs.iter().find(|r| r.number == 193).unwrap();
        assert_eq!(one.owner, "acme");
        assert_eq!(one.repo, "widgets");
        assert_eq!(one.url, "https://github.com/acme/widgets/pull/193");
    }

    #[test]
    fn detect_pr_n_case_insensitive_and_spacing() {
        let ctx = Some(("acme", "widgets"));
        // Mixed case + no space before `#` + extra whitespace between words.
        let refs = detect_pr_refs("pR#5 and Pull   Request #6", ctx, &[]);
        let nums: Vec<i64> = refs.iter().map(|r| r.number).collect();
        assert!(nums.contains(&5), "got {nums:?}");
        assert!(nums.contains(&6), "got {nums:?}");
    }

    #[test]
    fn detect_bare_hash_not_matched() {
        let ctx = Some(("acme", "widgets"));
        let refs = detect_pr_refs("see #193 over there", ctx, &[]);
        assert!(refs.is_empty(), "bare #193 must not match, got {refs:?}");
    }

    #[test]
    fn detect_urls_in_serialized_tool_output() {
        for text in [
            r#"{"url":"https://github.com/acme/platform/pull/948","deploy":"https://github.com/acme/deploy/pull/6073"}"#,
            r#"{\"url\":\"https://github.com/acme/platform/pull/948\",\"deploy\":\"https://github.com/acme/deploy/pull/6073\"}"#,
        ] {
            let refs = detect_pr_refs(text, None, &["acme".to_string()]);
            assert_eq!(refs.len(), 2, "{text}");
            assert_eq!(refs[0].number, 948);
            assert_eq!(refs[1].number, 6073);
        }
    }

    #[test]
    fn detect_issue_url_not_matched() {
        let refs = detect_pr_refs(
            "https://github.com/owner/repo/issues/42",
            Some(("owner", "repo")),
            &[],
        );
        assert!(refs.is_empty(), "issue URL must not match, got {refs:?}");
    }

    #[test]
    fn detect_repo_ctx_none_only_urls() {
        // Without repo context, PR #N is skipped but a full URL still links.
        let text = "PR #193 — https://github.com/owner/repo/pull/8";
        let refs = detect_pr_refs(text, None, &[]);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].number, 8);
    }

    #[test]
    fn detect_dedupes_url_and_matching_pr_n() {
        // A full URL and a matching `PR #42` for the same repo ⇒ one entry.
        let ctx = Some(("owner", "repo"));
        let text = "https://github.com/owner/repo/pull/42 — see PR #42";
        let refs = detect_pr_refs(text, ctx, &[]);
        assert_eq!(
            refs.len(),
            1,
            "URL + matching PR #N must dedupe, got {refs:?}"
        );
        assert_eq!(refs[0].number, 42);
        // First-seen wins: the URL entry's canonical url.
        assert_eq!(refs[0].url, "https://github.com/owner/repo/pull/42");
    }

    #[test]
    fn detect_pr_word_does_not_overmatch() {
        let ctx = Some(("o", "r"));
        // "prepared" should not trigger on a "pr" prefix; "#5" with no keyword
        // is bare and must not match.
        let refs = detect_pr_refs("prepared the change, see #5", ctx, &[]);
        assert!(refs.is_empty(), "got {refs:?}");
    }

    // --- full-URL owner gating (Fix 2) ---

    #[test]
    fn detect_full_url_blocked_when_owner_not_in_allowlist() {
        // A PR URL whose owner isn't one of the session's repo owners is NOT
        // linked — e.g. this repo's own `acme/widgets` test fixtures appearing
        // in a transcript while working on example code.
        let allowed = vec!["Example-Org".to_string()];
        let refs = detect_pr_refs(
            "see https://github.com/acme/widgets/pull/42 in the test",
            None,
            &allowed,
        );
        assert!(refs.is_empty(), "off-org URL must not link, got {refs:?}");
    }

    #[test]
    fn detect_full_url_allowed_for_same_org_cross_repo() {
        // A PR URL in the session's org links even if it's a different repo than
        // any the session touched (intra-org cross-repo paste stays useful).
        let allowed = vec!["example-org".to_string()]; // case-insensitive
        let refs = detect_pr_refs(
            "depends on https://github.com/Example-Org/example-deploy/pull/739",
            None,
            &allowed,
        );
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].owner, "Example-Org");
        assert_eq!(refs[0].repo, "example-deploy");
        assert_eq!(refs[0].number, 739);
    }

    #[test]
    fn detect_empty_allowlist_links_any_owner() {
        // Empty allowlist ⇒ no known session repos ⇒ legacy link-anything.
        let refs = detect_pr_refs("see https://github.com/acme/widgets/pull/42", None, &[]);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].owner, "acme");
    }

    #[test]
    fn owner_allowed_matches_case_insensitively() {
        let allowed = vec!["Example-Org".to_string()];
        assert!(owner_allowed("example-org", &allowed));
        assert!(owner_allowed("Example-Org", &allowed));
        assert!(!owner_allowed("acme", &allowed));
        // Empty allowlist never restricts.
        assert!(owner_allowed("anyone", &[]));
    }
}
