//! Discovery, sorting, and selection of local session files.
//!
//! Wraps `conversations_core` discovery and adds the pure filtering/selection
//! logic the subcommands share. The selection logic is factored into
//! [`select_refs`] so it is unit-testable without touching the network or
//! filesystem.

use std::path::Path;

use anyhow::{Context, Result};
use conversations_core::{
    decode_project_dir, discover_sessions, discover_sessions_for_agent, parse_session_file,
    quick_summary, AgentKind, Session, SessionFileRef, SessionSummary,
};

/// Discover all session files for `agent` under `root`, sorted newest-first.
pub fn discover_sorted_for_agent(root: &Path, agent: AgentKind) -> Result<Vec<SessionFileRef>> {
    if matches!(agent, AgentKind::Zsh) {
        return Ok(Vec::new());
    }
    if !root.exists() {
        anyhow::bail!("session directory does not exist: {}", root.display());
    }
    let mut refs = match agent {
        AgentKind::Claude => discover_sessions(root),
        AgentKind::Codex => discover_sessions_for_agent(root, AgentKind::Codex),
        AgentKind::OpenCode => crate::opencode_store::discover(root)?,
        AgentKind::Zsh => unreachable!(),
    };
    sort_newest_first(&mut refs);
    Ok(refs)
}

/// Sort in place, newest `modified` first. Refs without a timestamp sort last.
pub fn sort_newest_first(refs: &mut [SessionFileRef]) {
    refs.sort_by(|a, b| match (a.modified, b.modified) {
        // Both dated: newer (larger) first.
        (Some(am), Some(bm)) => bm.cmp(&am).then_with(|| a.session_id.cmp(&b.session_id)),
        // Dated sorts before undated.
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.session_id.cmp(&b.session_id),
    });
}

/// True when `r`'s decoded project path contains `needle` (case-insensitive).
pub fn matches_project(r: &SessionFileRef, needle: &str) -> bool {
    display_project(r)
        .to_lowercase()
        .contains(&needle.to_lowercase())
}

/// Human-readable project path for every session source. Claude's directory
/// key is lossy dash-encoding; Codex/OpenCode already carry real paths.
pub fn display_project(r: &SessionFileRef) -> String {
    match r.agent {
        AgentKind::Claude => decode_project_dir(&r.project_dir),
        _ => r.project_dir.clone(),
    }
}

/// Summary from the source index when available, otherwise from the JSONL.
pub fn summary_for_ref(r: &SessionFileRef) -> Option<SessionSummary> {
    r.summary.clone().or_else(|| quick_summary(&r.path))
}

/// Extract `(title, project, msg_count)` display strings from a session ref.
/// Shared by `list` and `upload` command formatting.
pub fn session_display_parts(r: &SessionFileRef) -> (String, String, String) {
    let summary = summary_for_ref(r);
    let title = summary
        .as_ref()
        .map(|s| s.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| r.session_id.clone());
    let project = summary
        .as_ref()
        .and_then(|s| s.project_path.clone())
        .unwrap_or_else(|| display_project(r));
    let msgs = summary
        .as_ref()
        .map(|s| s.message_count.to_string())
        .unwrap_or_else(|| "?".to_string());
    (title, project, msgs)
}

/// Full session parse dispatched by the source kind.
pub fn parse_ref(r: &SessionFileRef) -> Result<Session> {
    match r.agent {
        AgentKind::OpenCode => crate::opencode_store::parse_session(r),
        _ => parse_session_file(&r.path).map_err(anyhow::Error::from),
    }
}

/// Pure selection logic shared by `upload`.
///
/// Selection rules, applied to the already-sorted `refs`:
/// - `project_filter`: keep only refs whose decoded project path matches.
/// - if `ids` is non-empty: keep only refs whose `session_id` is in `ids`
///   (after the project filter). `all` is ignored in this case.
/// - else if `all`: keep everything that passed the project filter.
/// - else: select nothing (the caller falls back to an interactive picker).
pub fn select_refs<'a>(
    refs: &'a [SessionFileRef],
    project_filter: Option<&str>,
    ids: &[String],
    all: bool,
) -> Vec<&'a SessionFileRef> {
    let filtered = refs.iter().filter(|r| {
        project_filter
            .map(|p| matches_project(r, p))
            .unwrap_or(true)
    });

    if !ids.is_empty() {
        filtered
            .filter(|r| ids.iter().any(|id| id == &r.session_id))
            .collect()
    } else if all {
        filtered.collect()
    } else {
        Vec::new()
    }
}

/// Resolve the projects directory: explicit override or the core default.
pub fn resolve_projects_dir(explicit: Option<&Path>) -> Result<std::path::PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    conversations_core::claude_projects_dir()
        .context("could not resolve ~/.claude/projects (set --projects-dir or CLAUDE_PROJECTS_DIR)")
}

/// Resolve the transcript root for the selected agent.
pub fn resolve_sessions_dir(
    agent: AgentKind,
    claude_explicit: Option<&Path>,
    codex_explicit: Option<&Path>,
    opencode_explicit: Option<&Path>,
) -> Result<std::path::PathBuf> {
    match agent {
        AgentKind::Claude => resolve_projects_dir(claude_explicit),
        AgentKind::Codex => {
            if let Some(p) = codex_explicit {
                return Ok(p.to_path_buf());
            }
            conversations_core::codex_sessions_dir().context(
                "could not resolve ~/.codex/sessions (set --codex-sessions-dir or CODEX_SESSIONS_DIR)",
            )
        }
        AgentKind::OpenCode => crate::opencode_store::resolve_db_path(opencode_explicit),
        AgentKind::Zsh => {
            std::env::current_dir().context("could not resolve current directory for zsh session")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;

    fn r(id: &str, project_dir: &str, mod_secs: Option<i64>) -> SessionFileRef {
        SessionFileRef {
            path: PathBuf::from(format!("/tmp/{project_dir}/{id}.jsonl")),
            session_id: id.to_string(),
            agent: conversations_core::AgentKind::Claude,
            project_dir: project_dir.to_string(),
            size_bytes: 1,
            modified: mod_secs.map(|s| Utc.timestamp_opt(s, 0).unwrap()),
            summary: None,
        }
    }

    #[test]
    fn sort_puts_newest_first_and_undated_last() {
        let mut refs = vec![
            r("old", "-a", Some(100)),
            r("new", "-a", Some(300)),
            r("mid", "-a", Some(200)),
            r("undated", "-a", None),
        ];
        sort_newest_first(&mut refs);
        let ids: Vec<_> = refs.iter().map(|x| x.session_id.as_str()).collect();
        assert_eq!(ids, vec!["new", "mid", "old", "undated"]);
    }

    #[test]
    fn select_all_returns_everything_when_no_filter() {
        let refs = vec![r("a", "-x", Some(1)), r("b", "-y", Some(2))];
        let sel = select_refs(&refs, None, &[], true);
        assert_eq!(sel.len(), 2);
    }

    #[test]
    fn select_filters_by_project_substring_case_insensitive() {
        let refs = vec![
            r("a", "-Users-developer-example", Some(1)),
            r("b", "-Users-developer-other", Some(2)),
        ];
        let sel = select_refs(&refs, Some("example"), &[], true);
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].session_id, "a");
    }

    #[test]
    fn select_by_ids_ignores_all_flag() {
        let refs = vec![
            r("a", "-x", Some(1)),
            r("b", "-x", Some(2)),
            r("c", "-x", Some(3)),
        ];
        let sel = select_refs(&refs, None, &["b".to_string(), "c".to_string()], false);
        let ids: Vec<_> = sel.iter().map(|x| x.session_id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c"]);
    }

    #[test]
    fn select_by_ids_respects_project_filter() {
        let refs = vec![r("a", "-keep", Some(1)), r("b", "-drop", Some(2))];
        // id "b" is excluded because its project doesn't match.
        let sel = select_refs(
            &refs,
            Some("keep"),
            &["a".to_string(), "b".to_string()],
            false,
        );
        let ids: Vec<_> = sel.iter().map(|x| x.session_id.as_str()).collect();
        assert_eq!(ids, vec!["a"]);
    }

    #[test]
    fn select_nothing_without_all_or_ids() {
        let refs = vec![r("a", "-x", Some(1))];
        let sel = select_refs(&refs, None, &[], false);
        assert!(sel.is_empty());
    }
}
