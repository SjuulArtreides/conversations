//! Filesystem discovery of agent sessions.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use walkdir::WalkDir;

use crate::model::{AgentKind, SessionFileRef};

/// The default Claude Code projects directory: `~/.claude/projects`.
/// Returns `None` if the home directory cannot be resolved.
pub fn claude_projects_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// The default Codex sessions directory: `$CODEX_HOME/sessions`, or
/// `~/.codex/sessions` when `CODEX_HOME` is unset.
pub fn codex_sessions_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        if !home.trim().is_empty() {
            return Some(PathBuf::from(home).join("sessions"));
        }
    }
    dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
}

/// Best-effort decode of a dash-encoded project directory name back to a
/// displayable path.
///
/// Claude Code encodes the absolute cwd by replacing both `/` and `.` with `-`,
/// so the mapping is **lossy and not reversible**: a single `-` could have been
/// a path separator, a `.`, or a literal hyphen in the original path. We
/// reconstruct a readable approximation by turning the leading dash into `/`
/// (absolute paths start with `/`) and remaining dashes into `/`. We do NOT try
/// to recover `.` characters — the result is for display only.
pub fn decode_project_dir(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    // Leading dash represents the root `/`.
    let body = name.strip_prefix('-').unwrap_or(name);
    let mut decoded = String::with_capacity(name.len());
    decoded.push('/');
    decoded.push_str(&body.replace('-', "/"));
    decoded
}

/// Walk `root/*/*.jsonl` and return a reference to every session file found.
/// Non-`.jsonl` files and anything deeper or shallower than one level under
/// `root` are ignored. Unreadable entries are skipped.
pub fn discover_sessions(root: &Path) -> Vec<SessionFileRef> {
    discover_sessions_for_agent(root, AgentKind::Claude)
}

/// Walk `root` and return every session file for the given agent.
///
/// Claude stores `root/*/*.jsonl`; Codex stores dated nested paths under
/// `root/YYYY/MM/DD/*.jsonl`. OpenCode is indexed from its SQLite database by
/// the CLI and therefore has no filesystem walk here. We intentionally allow
/// Codex files at any depth below `root` so future date layout changes do not
/// break discovery.
pub fn discover_sessions_for_agent(root: &Path, agent: AgentKind) -> Vec<SessionFileRef> {
    if matches!(agent, AgentKind::OpenCode | AgentKind::Zsh) {
        return Vec::new();
    }
    let mut out = Vec::new();

    let mut walk = WalkDir::new(root).min_depth(1);
    if agent == AgentKind::Claude {
        walk = walk.min_depth(2).max_depth(2);
    }

    for entry in walk.into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }

        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let project_dir = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();

        let (size_bytes, modified) = match entry.metadata() {
            Ok(meta) => {
                let modified = meta.modified().ok().map(DateTime::<Utc>::from);
                (meta.len(), modified)
            }
            Err(_) => (0, None),
        };

        out.push(SessionFileRef {
            path: path.to_path_buf(),
            session_id,
            agent,
            project_dir,
            size_bytes,
            modified,
            summary: None,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_simple_path() {
        assert_eq!(
            decode_project_dir("-Users-developer-example"),
            "/Users/developer/example"
        );
    }

    #[test]
    fn decode_dotted_path_is_lossy() {
        // `/Users/x/a.b` encodes to `-Users-x-a-b`; the dot is unrecoverable.
        assert_eq!(decode_project_dir("-Users-x-a-b"), "/Users/x/a/b");
    }

    #[test]
    fn decode_empty() {
        assert_eq!(decode_project_dir(""), "");
    }

    #[test]
    fn projects_dir_under_home() {
        if let Some(dir) = claude_projects_dir() {
            assert!(dir.ends_with("projects"));
            assert!(dir.to_string_lossy().contains(".claude"));
        }
    }

    #[test]
    fn discover_finds_jsonl_two_levels_deep() {
        let tmp = std::env::temp_dir().join(format!("conv-disc-{}", std::process::id()));
        let proj = tmp.join("-Users-x-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let file = proj.join("sess-xyz.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        // A non-jsonl file and a top-level file should be ignored.
        std::fs::write(proj.join("notes.txt"), "x").unwrap();
        std::fs::write(tmp.join("top.jsonl"), "x").unwrap();

        let refs = discover_sessions(&tmp);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].session_id, "sess-xyz");
        assert_eq!(refs[0].agent, AgentKind::Claude);
        assert_eq!(refs[0].project_dir, "-Users-x-proj");
        assert!(refs[0].size_bytes > 0);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn discover_codex_finds_nested_jsonl() {
        let tmp = std::env::temp_dir().join(format!("codex-disc-{}", std::process::id()));
        let day = tmp.join("2026").join("06").join("20");
        std::fs::create_dir_all(&day).unwrap();
        let file = day.join("rollout-2026-06-20T11-00-00-abc.jsonl");
        std::fs::write(&file, "{}\n").unwrap();

        let refs = discover_sessions_for_agent(&tmp, AgentKind::Codex);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].session_id, "rollout-2026-06-20T11-00-00-abc");
        assert_eq!(refs[0].agent, AgentKind::Codex);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn codex_sessions_dir_honors_codex_home_with_fallback() {
        // This test exclusively owns `CODEX_HOME` mutation, so it can't race
        // other tests in this binary (none of the others read it). It restores
        // the original value before returning.
        let original = std::env::var_os("CODEX_HOME");

        // An explicit `CODEX_HOME` wins and gets `/sessions` appended.
        std::env::set_var("CODEX_HOME", "/tmp/custom-codex");
        assert_eq!(
            codex_sessions_dir(),
            Some(PathBuf::from("/tmp/custom-codex").join("sessions"))
        );

        // An empty / whitespace-only value is ignored: fall back to `~/.codex`.
        std::env::set_var("CODEX_HOME", "   ");
        if let Some(dir) = codex_sessions_dir() {
            assert!(dir.ends_with("sessions"));
            assert!(dir.to_string_lossy().contains(".codex"));
        }

        // Unset behaves the same as the whitespace fallback.
        std::env::remove_var("CODEX_HOME");
        if let Some(dir) = codex_sessions_dir() {
            assert!(dir.ends_with("sessions"));
            assert!(dir.to_string_lossy().contains(".codex"));
        }

        // Restore whatever the environment had so later tests are unaffected.
        match original {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }
    }

    #[test]
    fn claude_is_depth_locked_while_codex_walks_any_depth() {
        // Pin the documented contract: Claude reads exactly `root/*/*.jsonl`
        // (depth 2), while Codex follows its dated `YYYY/MM/DD/*.jsonl` layout
        // at depth 3 — and any future depth.
        let tmp = std::env::temp_dir().join(format!("conv-depth-{}", std::process::id()));
        let deep = tmp.join("2026").join("06").join("20");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("rollout-deep.jsonl"), "{}\n").unwrap();

        let codex = discover_sessions_for_agent(&tmp, AgentKind::Codex);
        assert_eq!(codex.len(), 1, "codex must find the depth-3 dated file");
        assert_eq!(codex[0].session_id, "rollout-deep");

        let claude = discover_sessions_for_agent(&tmp, AgentKind::Claude);
        assert!(
            claude.is_empty(),
            "claude walker is depth-locked to 2 and must not descend to depth 3"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
