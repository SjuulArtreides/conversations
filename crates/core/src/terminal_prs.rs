//! Bounded PR discovery from retained terminal rows, never raw escape bytes.
use std::collections::HashSet;

pub const MAX_TERMINAL_PR_URLS: usize = 100;

pub fn discover(mut normal: vt100::Screen, alternate: Option<vt100::Screen>) -> Vec<String> {
    let mut text = String::new();
    let mut targets = HashSet::new();
    let mut append = |screen: &vt100::Screen, row: u16| {
        let (_, cols) = screen.size();
        if let Some(line) = screen.rows(0, cols).nth(usize::from(row)) {
            text.push_str(&line);
            if !screen.row_wrapped(row) {
                text.push('\n');
            }
        }
        for col in 0..cols {
            if let Some(uri) = screen.cell(row, col).and_then(|cell| cell.hyperlink()) {
                targets.insert(uri.to_owned());
            }
        }
    };
    normal.set_scrollback(usize::MAX);
    for offset in (1..=normal.scrollback()).rev() {
        normal.set_scrollback(offset);
        append(&normal, 0);
    }
    normal.set_scrollback(0);
    for row in 0..normal.size().0 {
        append(&normal, row);
    }
    if let Some(screen) = alternate {
        for row in 0..screen.size().0 {
            append(&screen, row);
        }
    }
    for target in targets {
        text.push('\n');
        text.push_str(&target);
    }
    discover_in_text(&text)
}

/// Scan free text for canonical GitHub PR URLs, capped at
/// [`MAX_TERMINAL_PR_URLS`]. Same URL-extraction rules as the screen scanner —
/// so markdown emphasis, trailing punctuation, JSON tool output (`{"url":"…"}`),
/// backslash-escaped JSON (`{\"url\":\"…\"}`), and `/files`/`?w=1` tails all
/// resolve to the canonical `https://github.com/{owner}/{repo}/pull/{n}`.
///
/// The function matches **full URLs only** — bare `PR #N` or `#N` references
/// are not inferred, since the caller has no verified owner/repo to attach to
/// a number-only mention without guessing.
pub fn discover_in_text(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for marker in ["https://github.com/", "http://github.com/"] {
        for suffix in text.split(marker).skip(1) {
            let token = suffix
                .split(|c: char| {
                    c.is_whitespace()
                        || matches!(c, '<' | '>' | ')' | '*' | '`' | '"' | '\'' | '\\')
                })
                .next()
                .unwrap_or("")
                .trim_end_matches([',', '.', ';', ':', ')', ']', '*', '`', '_', '~']);
            let mut parts = token.split(['/', '?', '#']);
            let (Some(owner), Some(repo), Some("pull"), Some(number)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Ok(number) = number.parse::<i64>() else {
                continue;
            };
            if owner.is_empty() || repo.is_empty() || number <= 0 {
                continue;
            }
            let url = format!("https://github.com/{owner}/{repo}/pull/{number}");
            if !urls.contains(&url) {
                urls.push(url);
            }
            if urls.len() == MAX_TERMINAL_PR_URLS {
                return urls;
            }
        }
    }
    urls
}

/// Parse a canonical `https://github.com/{owner}/{repo}/pull/{n}` URL (as
/// produced by [`discover_in_text`] / [`discover`]) into its components.
pub fn parse_canonical(url: &str) -> Option<(String, String, i64)> {
    let path = url.strip_prefix("https://github.com/")?;
    let mut parts = path.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?.to_string();
    let repo = parts.next().filter(|s| !s.is_empty())?.to_string();
    (parts.next()? == "pull").then_some(())?;
    let number: i64 = parts.next()?.parse().ok().filter(|&n| n > 0)?;
    Some((owner, repo, number))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_normal_history_and_alternate_links_survive_repaint() {
        let mut parser = vt100::Parser::new(2, 80, 20);
        parser.process(b"https://github.com/acme/history/pull/3\r\nmore\r\nlast\r\n\x1b[?1049hhttps://github.com/acme/alternate/pull/4");
        let refs = discover(
            parser.screen().normal_screen(),
            Some(parser.screen().clone()),
        );
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&"https://github.com/acme/history/pull/3".into()));
        assert!(refs.contains(&"https://github.com/acme/alternate/pull/4".into()));
        let repaint = crate::terminal::replay_snapshot(&mut parser);
        let mut restored = vt100::Parser::new(2, 80, 20);
        restored.process(&repaint);
        let again = discover(
            restored.screen().normal_screen(),
            restored
                .screen()
                .alternate_screen()
                .then(|| restored.screen().clone()),
        );
        for url in refs {
            assert!(again.contains(&url));
        }
    }

    #[test]
    fn text_scan_matches_markdown_and_json_tool_output_across_repos() {
        // The actual pattern from the screenshot: a Tenant Route Operator
        // Claude session that mentions several PRs across platform and
        // deploy, inline in text and inside serialized tool output. The text
        // scanner must find every canonical URL once, regardless of the
        // markdown wrapper or JSON escape it was pasted inside.
        let text = "\
            Platform PRs so far:\n\
            - [platform#1095](https://github.com/acme/platform/pull/1095)\n\
            - **https://github.com/acme/platform/pull/1078/files**\n\
            - `https://github.com/acme/platform/pull/1099`\n\
            - review https://github.com/acme/platform/pull/870.\n\
            \n\
            Deploy follow-ups:\n\
            {\"url\":\"https://github.com/acme/deploy/pull/7162\",\n\
             \"next\":\"https://github.com/acme/deploy/pull/7078\"}\n\
            {\\\"deploy\\\":\\\"https://github.com/acme/deploy/pull/7071\\\"}\n\
            and https://github.com/acme/platform/pull/1075 (already open).";
        let urls = discover_in_text(text);
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
                urls.contains(&expected.to_string()),
                "missing {expected} in {urls:?}"
            );
        }
        assert_eq!(urls.len(), 8, "no duplicates: {urls:?}");
    }

    #[test]
    fn text_scan_rejects_non_pr_paths_and_canonicalizes() {
        let urls = discover_in_text(
            "issue: https://github.com/o/r/issues/1 \
             pr: https://github.com/o/r/pull/9/files?w=1#diff-abc",
        );
        assert_eq!(urls, vec!["https://github.com/o/r/pull/9"]);
    }

    #[test]
    fn parse_canonical_round_trips_discover() {
        let urls = discover_in_text("see https://github.com/acme/widgets/pull/42");
        let (owner, repo, number) = parse_canonical(&urls[0]).unwrap();
        assert_eq!(
            (owner.as_str(), repo.as_str(), number),
            ("acme", "widgets", 42)
        );
        assert!(parse_canonical("https://github.com/acme/widgets/issues/1").is_none());
        assert!(parse_canonical("https://github.com/acme/widgets/pull/0").is_none());
    }

    #[test]
    fn split_wrapped_links_and_osc8_are_discovered_once() {
        let mut parser = vt100::Parser::new(5, 30, 20);
        parser.process(b"https://github.com/acme/project/pu");
        assert!(discover(parser.screen().normal_screen(), None).is_empty());
        parser.process(b"ll/23\r\n\x1b]8;;https://github.com/acme/other/pull/4\x07label\x1b]8;;\x07\r\nhttps://github.com/acme/project/pull/23/files");
        let refs = discover(parser.screen().normal_screen(), None);
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&"https://github.com/acme/project/pull/23".into()));
        assert!(refs.contains(&"https://github.com/acme/other/pull/4".into()));
    }
}
