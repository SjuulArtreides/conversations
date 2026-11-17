//! `arrakis-conversations list` — show discovered local sessions in a table.

use anyhow::Result;
use conversations_core::SessionFileRef;

use crate::cli::{GlobalOpts, ListArgs};
use crate::discover::{
    discover_sorted_for_agent, matches_project, resolve_sessions_dir, session_display_parts,
};

pub fn run(global: &GlobalOpts, args: &ListArgs) -> Result<()> {
    let root = resolve_sessions_dir(
        args.agent,
        global.projects_dir.as_deref(),
        global.codex_sessions_dir.as_deref(),
        global.opencode_db.as_deref(),
    )?;
    let mut refs = discover_sorted_for_agent(&root, args.agent)?;

    if let Some(p) = args.project.as_deref() {
        refs.retain(|r| matches_project(r, p));
    }
    if let Some(limit) = args.limit {
        refs.truncate(limit);
    }

    if refs.is_empty() {
        println!("No sessions found under {}", root.display());
        return Ok(());
    }

    // Build rows first so we can size columns.
    let rows: Vec<Row> = refs.iter().map(build_row).collect();

    let title_w = rows
        .iter()
        .map(|r| r.title.len())
        .max()
        .unwrap_or(5)
        .clamp(5, 50);
    let proj_w = rows
        .iter()
        .map(|r| r.project.len())
        .max()
        .unwrap_or(7)
        .clamp(7, 40);

    println!(
        "{:>3}  {:<title_w$}  {:<proj_w$}  {:>4}  {:<20}  SESSION",
        "#",
        "TITLE",
        "PROJECT",
        "MSGS",
        "MODIFIED",
        title_w = title_w,
        proj_w = proj_w,
    );
    for (i, row) in rows.iter().enumerate() {
        println!(
            "{:>3}  {:<title_w$}  {:<proj_w$}  {:>4}  {:<20}  {}",
            i + 1,
            truncate(&row.title, title_w),
            truncate(&row.project, proj_w),
            row.msgs,
            row.modified,
            short_id(&row.session_id),
            title_w = title_w,
            proj_w = proj_w,
        );
    }
    println!("\n{} session(s).", rows.len());
    Ok(())
}

struct Row {
    title: String,
    project: String,
    msgs: String,
    modified: String,
    session_id: String,
}

fn build_row(r: &SessionFileRef) -> Row {
    let (title, project, msgs) = session_display_parts(r);
    let modified = r
        .modified
        .map(|m| m.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "-".to_string());
    Row {
        title,
        project,
        msgs,
        modified,
        session_id: r.session_id.clone(),
    }
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width <= 1 {
        "…".to_string()
    } else {
        let kept: String = s.chars().take(width - 1).collect();
        format!("{kept}…")
    }
}

/// First segment of a UUID-ish session id (up to the first `-`, capped at 8).
fn short_id(id: &str) -> String {
    let head = id.split('-').next().unwrap_or(id);
    head.chars().take(8).collect()
}
