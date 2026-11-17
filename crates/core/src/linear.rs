//! Linear reference helpers — URL parsing + free-text detection.
//!
//! The server holds **zero** Linear credentials (the same "no external creds"
//! rule the GitHub PR pipeline follows — see `github.rs`). Detection is a pure
//! text-parse: we recognise Linear document / issue / project URLs in a
//! session's messages and store the canonical URL + a human-readable
//! identifier. Nothing here calls Linear over the network; we never fetch a
//! title or status. The UI shows the identifier and links out.

use serde::{Deserialize, Serialize};

/// A canonical Linear document, issue, or project link recorded in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLinearDoc {
    pub url: String,
    pub workspace: String,
    pub kind: String,
    pub identifier: String,
}

/// The Linear entity kinds we recognise. Stored verbatim in
/// `session_linear_docs.kind`.
const KINDS: [&str; 3] = ["document", "issue", "project"];

/// Parse a Linear web URL into a [`SessionLinearDoc`], or `None` if it is not a
/// recognised Linear entity URL.
///
/// Accepted forms (scheme optional), with or without a trailing slug / path /
/// query / fragment:
/// - `https://linear.app/{workspace}/document/{slug-id}`
/// - `https://linear.app/{workspace}/issue/{TEAM-123}[/{slug}]`
/// - `https://linear.app/{workspace}/project/{slug-id}`
///
/// The canonical URL keeps exactly `…/{kind}/{identifier}` (dropping any
/// trailing slug for issues and any further path segments), so the same entity
/// referenced with and without its title slug dedupes to one row. `identifier`
/// is the third path segment — `TEAM-123` for an issue, the `{slug-id}` segment
/// for a document/project. Any other `linear.app` path (or a non-Linear host)
/// returns `None`.
pub fn parse_linear_url(url: &str) -> Option<SessionLinearDoc> {
    // Backslashes delimit transcript/JSON escapes, never a Linear path segment.
    // Apply this here too so historical stored refs normalize on read.
    let url = url
        .split(['#', '\\'])
        .next()
        .unwrap_or(url)
        .trim()
        .trim_end_matches([',', '.', ';', ':', ')', ']', '*', '`', '_', '~', '"', '\'']);

    // Must start with the Linear host (scheme optional).
    let path = url
        .strip_prefix("https://linear.app/")
        .or_else(|| url.strip_prefix("http://linear.app/"))
        .or_else(|| url.strip_prefix("linear.app/"))?;

    // Drop any query string before splitting the path.
    let path = path.split('?').next().unwrap_or(path);

    // workspace / kind / identifier [/ optional slug…]
    let mut parts = path.split('/');
    let workspace = parts.next().filter(|s| !s.is_empty())?.to_string();
    let kind = parts.next().filter(|s| KINDS.contains(s))?.to_string();
    let identifier = parts.next().filter(|s| !s.is_empty())?.to_string();

    let canonical = format!("https://linear.app/{workspace}/{kind}/{identifier}");
    Some(SessionLinearDoc {
        url: canonical,
        workspace,
        kind,
        identifier,
    })
}

/// Scan free text for Linear entity URLs and return the deduped set as
/// [`SessionLinearDoc`]s. Pure text-parse — performs **no** network I/O.
///
/// Full URLs with an explicit `https://` / `http://` scheme are matched (the
/// same scan strategy `github::detect_pr_refs` uses for PR URLs, including
/// tolerating markdown-wrapped URLs). Scheme-less `linear.app/…` mentions are
/// intentionally not detected from message text (mirrors the PR scanner, which
/// only links full URLs). The result is deduped by canonical url; the first
/// occurrence wins.
pub fn detect_linear_refs(text: &str) -> Vec<SessionLinearDoc> {
    let mut out: Vec<SessionLinearDoc> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for marker in ["https://linear.app/", "http://linear.app/"] {
        let mut search_from = 0usize;
        while let Some(rel) = text[search_from..].find(marker) {
            let start = search_from + rel;
            // A URL token ends at the first whitespace, angle bracket, closing
            // paren, markdown emphasis/code char, or quote (none of `*`, `` ` ``,
            // `"`, `'` can appear in a Linear URL, so they reliably terminate a
            // token even when the URL is wrapped with no surrounding space —
            // e.g. `"https://linear.app/ws/issue/ENG-7"`).
            let end = text[start..]
                .find(|c: char| {
                    c.is_whitespace()
                        || c == '\\'
                        || c == '<'
                        || c == '>'
                        || c == ')'
                        || c == '*'
                        || c == '`'
                        || c == '"'
                        || c == '\''
                })
                .map(|i| start + i)
                .unwrap_or(text.len());
            // Trim trailing punctuation / markdown wrappers / quotes the boundary
            // scan didn't already cut.
            let token = text[start..end]
                .trim_end_matches([',', '.', ';', ':', ')', ']', '*', '`', '_', '~', '"', '\'']);
            if let Some(doc) = parse_linear_url(token) {
                if seen.insert(doc.url.clone()) {
                    out.push(doc);
                }
            }
            search_from = end.max(start + marker.len());
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_transcript_boundaries_do_not_become_linear_identifiers() {
        for suffix in [
            r"\",
            r"\\\",
            r"\n",
            r"\\\n\n",
            r".\\\",
            r".\\\\n\\\\n",
            r#"\"}"#,
        ] {
            let url = format!("https://linear.app/example/issue/ENG-2422{suffix}");
            let parsed = parse_linear_url(&url).unwrap();
            assert_eq!(parsed.identifier, "ENG-2422", "suffix {suffix:?}");
            assert_eq!(parsed.url, "https://linear.app/example/issue/ENG-2422");
            let text = format!("{url} https://linear.app/example/issue/ENG-2422/title");
            let refs = detect_linear_refs(&text);
            assert_eq!(refs.len(), 1, "suffix {suffix:?}: {refs:?}");
            assert_eq!(refs[0].identifier, "ENG-2422");
        }
        let refs = detect_linear_refs(
            r"https://linear.app/ws/document/spec-1\nhttps://linear.app/ws/project/plan-2\n",
        );
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].identifier, "spec-1");
        assert_eq!(refs[1].identifier, "plan-2");
    }

    #[test]
    fn parse_document_url() {
        let d =
            parse_linear_url("https://linear.app/example/document/my-design-doc-abc123").unwrap();
        assert_eq!(d.workspace, "example");
        assert_eq!(d.kind, "document");
        assert_eq!(d.identifier, "my-design-doc-abc123");
        assert_eq!(
            d.url,
            "https://linear.app/example/document/my-design-doc-abc123"
        );
    }

    #[test]
    fn parse_issue_url_drops_slug() {
        let d = parse_linear_url("https://linear.app/example/issue/ENG-123/fix-the-thing").unwrap();
        assert_eq!(d.kind, "issue");
        assert_eq!(d.identifier, "ENG-123");
        // Canonical form drops the trailing title slug.
        assert_eq!(d.url, "https://linear.app/example/issue/ENG-123");
    }

    #[test]
    fn parse_issue_url_without_slug() {
        let d = parse_linear_url("https://linear.app/example/issue/OCEO-42").unwrap();
        assert_eq!(d.identifier, "OCEO-42");
        assert_eq!(d.url, "https://linear.app/example/issue/OCEO-42");
    }

    #[test]
    fn parse_project_url() {
        let d = parse_linear_url("https://linear.app/example/project/roadmap-q3-def456").unwrap();
        assert_eq!(d.kind, "project");
        assert_eq!(d.identifier, "roadmap-q3-def456");
    }

    #[test]
    fn parse_tolerates_query_and_scheme_and_fragment() {
        let d = parse_linear_url("http://linear.app/ws/document/doc-1?foo=bar#section").unwrap();
        assert_eq!(d.kind, "document");
        assert_eq!(d.url, "https://linear.app/ws/document/doc-1");
        // Scheme-less still parses (used by parse, not by the message scanner).
        let d2 = parse_linear_url("linear.app/ws/issue/ENG-9").unwrap();
        assert_eq!(d2.identifier, "ENG-9");
    }

    #[test]
    fn parse_rejects_non_entities_and_garbage() {
        assert!(parse_linear_url("https://linear.app/example/settings").is_none());
        assert!(parse_linear_url("https://linear.app/example/team/ENG").is_none());
        assert!(parse_linear_url("https://linear.app/example/document/").is_none());
        assert!(parse_linear_url("https://linear.app/example").is_none());
        assert!(parse_linear_url("https://example.com/example/document/x").is_none());
        assert!(parse_linear_url("not a url").is_none());
        // A bare ticket reference is not a URL — must not parse.
        assert!(parse_linear_url("ENG-123").is_none());
    }

    #[test]
    fn detect_full_urls_of_each_kind() {
        let text = "design in https://linear.app/ws/document/spec-1, tracked by \
                    https://linear.app/ws/issue/ENG-7/title and \
                    https://linear.app/ws/project/q3-9";
        let refs = detect_linear_refs(text);
        let kinds: Vec<&str> = refs.iter().map(|r| r.kind.as_str()).collect();
        assert!(kinds.contains(&"document"));
        assert!(kinds.contains(&"issue"));
        assert!(kinds.contains(&"project"));
        assert_eq!(refs.len(), 3);
    }

    #[test]
    fn detect_wrapped_in_markdown_and_punctuation() {
        for text in [
            "**https://linear.app/ws/document/spec-1**",
            "(https://linear.app/ws/document/spec-1).",
            "`https://linear.app/ws/document/spec-1`",
            "see [https://linear.app/ws/document/spec-1]",
        ] {
            let refs = detect_linear_refs(text);
            assert_eq!(refs.len(), 1, "expected one ref for {text:?}");
            assert_eq!(refs[0].url, "https://linear.app/ws/document/spec-1");
        }
    }

    #[test]
    fn detect_quoted_urls() {
        // Single- and double-quoted URLs must parse to the clean canonical form
        // (the closing quote is a token boundary, not part of the identifier).
        for text in [
            r#""https://linear.app/ws/issue/ENG-7""#,
            r#"'https://linear.app/ws/issue/ENG-7'"#,
            r#"see "https://linear.app/ws/issue/ENG-7" here"#,
        ] {
            let refs = detect_linear_refs(text);
            assert_eq!(refs.len(), 1, "expected one ref for {text:?}");
            assert_eq!(refs[0].identifier, "ENG-7", "wrong identifier for {text:?}");
            assert_eq!(refs[0].url, "https://linear.app/ws/issue/ENG-7");
        }
    }

    #[test]
    fn detect_dedupes_issue_with_and_without_slug() {
        // The same issue referenced twice (once with a slug) ⇒ one entry.
        let text = "https://linear.app/ws/issue/ENG-7/title and again \
                    https://linear.app/ws/issue/ENG-7";
        let refs = detect_linear_refs(text);
        assert_eq!(refs.len(), 1, "got {refs:?}");
        assert_eq!(refs[0].url, "https://linear.app/ws/issue/ENG-7");
    }

    #[test]
    fn detect_ignores_bare_ticket_and_non_entity_paths() {
        let refs = detect_linear_refs("ticket ENG-123 and https://linear.app/ws/settings/members");
        assert!(refs.is_empty(), "got {refs:?}");
    }
}
