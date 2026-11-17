//! `arrakis-conversations upload` — discover, select, and publish sessions.
//!
//! Each selected session is ingested into local history (the canonical copy)
//! and published to the server through the signed publication lifecycle, as
//! visible to the publishing account only; `history share` widens it and
//! `history unshare` withdraws it. A session id that already has legacy
//! central data is refused, and that data is left exactly as it was.

use std::io::IsTerminal;

use anyhow::{Context, Result};
use conversations_core::SessionFileRef;
use dialoguer::{theme::ColorfulTheme, MultiSelect};
use indicatif::{ProgressBar, ProgressStyle};

use crate::cli::{GlobalOpts, UploadArgs};
use crate::commands::host_owner::HostOwner;
use crate::commands::local_host::history::{History, Source};
use crate::commands::local_host::publication::{self, Audience, Destination, State, Status};
use crate::commands::local_host::{agent_link, authz, settings::Settings};
use crate::discover::{
    discover_sorted_for_agent, parse_ref, resolve_sessions_dir, select_refs, session_display_parts,
};

pub fn run(global: &GlobalOpts, args: &UploadArgs) -> Result<()> {
    let root = resolve_sessions_dir(
        args.agent,
        global.projects_dir.as_deref(),
        global.codex_sessions_dir.as_deref(),
        global.opencode_db.as_deref(),
    )?;
    let refs = discover_sorted_for_agent(&root, args.agent)?;

    if refs.is_empty() {
        println!("No sessions found under {}", root.display());
        return Ok(());
    }

    // Session ids can come in positionally or via `--session`; treat them as one
    // set so `upload <id>` and `upload --session <id>` behave identically.
    let requested: Vec<String> = args
        .ids
        .iter()
        .chain(args.sessions.iter())
        .cloned()
        .collect();

    // Determine the selection. The non-flag path is interactive; we reuse that
    // signal to decide whether to do the (slower, network) `gh` PR lookup.
    let interactive = !args.all && requested.is_empty();
    let selected: Vec<&SessionFileRef> = if !interactive {
        let sel = select_refs(&refs, args.project.as_deref(), &requested, args.all);
        warn_unknown_ids(&sel, &requested);
        sel
    } else {
        // Interactive path requires a TTY.
        if !std::io::stdout().is_terminal() {
            anyhow::bail!(
                "no session selector given and stdout is not a TTY.\n\
                 Pass --all (optionally with --project <substr>) or --session <id> (repeatable)."
            );
        }
        interactive_select(&refs, args.project.as_deref())?
    };

    if selected.is_empty() {
        println!("Nothing selected — nothing to upload.");
        return Ok(());
    }

    if args.dry_run {
        return dry_run(&selected);
    }

    do_upload(global, &selected)
}

fn warn_unknown_ids(selected: &[&SessionFileRef], requested: &[String]) {
    for id in requested {
        if !selected.iter().any(|r| &r.session_id == id) {
            eprintln!("warning: requested session id not found (or filtered out): {id}");
        }
    }
}

fn interactive_select<'a>(
    refs: &'a [SessionFileRef],
    project_filter: Option<&str>,
) -> Result<Vec<&'a SessionFileRef>> {
    let candidates: Vec<&SessionFileRef> = select_refs(refs, project_filter, &[], true);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let labels: Vec<String> = candidates.iter().map(|r| label_for(r)).collect();
    let chosen = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select sessions to upload (space to toggle, enter to confirm)")
        .items(&labels)
        .interact()?;

    Ok(chosen.into_iter().map(|i| candidates[i]).collect())
}

fn label_for(r: &SessionFileRef) -> String {
    let (title, project, msgs) = session_display_parts(r);
    format!("{title}  [{project}]  ({msgs} msgs)")
}

fn dry_run(selected: &[&SessionFileRef]) -> Result<()> {
    println!(
        "Dry run — {} session(s) would be published:\n",
        selected.len()
    );
    let mut total_msgs = 0usize;
    let mut total_skipped = 0usize;
    let mut parse_failures = 0usize;
    for r in selected {
        match parse_ref(r) {
            Ok(session) => {
                let n = session.message_count();
                total_msgs += n;
                total_skipped += session.skipped_lines;
                let skip = if session.skipped_lines > 0 {
                    format!(", {} malformed line(s) skipped", session.skipped_lines)
                } else {
                    String::new()
                };
                println!(
                    "  {}  \"{}\"  {} msgs{}",
                    r.session_id, session.title, n, skip
                );
            }
            Err(e) => {
                parse_failures += 1;
                eprintln!("  {}  PARSE FAILED: {e}", r.session_id);
            }
        }
    }
    println!(
        "\nTotals: {} session(s), {} message(s), {} skipped line(s), {} parse failure(s).",
        selected.len(),
        total_msgs,
        total_skipped,
        parse_failures
    );
    if parse_failures > 0 {
        anyhow::bail!("{parse_failures} session(s) failed to parse");
    }
    Ok(())
}

fn do_upload(global: &GlobalOpts, selected: &[&SessionFileRef]) -> Result<()> {
    let dir = agent_link::runtime_for(&global.server);
    authz::ensure_dir(&dir)?;
    let owner =
        publication::configured_owner(Settings::load(&dir)?.publication.account.as_deref())?;
    let cookie = crate::credentials::cookie_for(&global.server);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let to = Destination {
        server: &global.server,
        client: &client,
        cookie: cookie.as_deref(),
    };

    println!(
        "Publishing {} session(s) to {} as {}.",
        selected.len(),
        global.server,
        owner.as_deref().unwrap_or("no account")
    );

    let pb = ProgressBar::new(selected.len() as u64);
    pb.set_style(
        ProgressStyle::with_template("{bar:40.cyan/blue} {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("##-"),
    );

    let mut published = 0usize;
    // Failure detail lines, replayed to stderr at the end so they survive a
    // non-TTY run (where `pb.println` draws nothing).
    let mut failures: Vec<String> = Vec::new();

    for r in selected {
        pb.set_message(r.session_id.clone());
        let result = publish_one(&dir, r, owner.as_deref(), &runtime, &to);
        match result {
            Ok((local_id, status, skipped)) => {
                published += 1;
                // A native id that is not a session id is published under the
                // stable local one it maps to.
                let name = if local_id == r.session_id {
                    local_id
                } else {
                    format!("{} as {local_id}", r.session_id)
                };
                let warn = if skipped > 0 {
                    format!("  (warning: {skipped} malformed line(s) skipped)")
                } else {
                    String::new()
                };
                let line = format!(
                    "  OK    {name}  shared with you only ({} msgs){warn}",
                    status.published
                );
                // `pb.println` draws nothing without a TTY.
                if pb.is_hidden() {
                    println!("{line}");
                } else {
                    pb.println(line);
                }
            }
            Err(e) => {
                let line = format!("  FAIL  {}  {e:#}", r.session_id);
                pb.println(&line);
                failures.push(line);
            }
        }
        pb.inc(1);
    }
    pb.finish_and_clear();

    // Replay failures to stderr — `pb.println` is a no-op without a TTY, so this
    // is what makes the per-session reason visible in piped / CI runs.
    for line in &failures {
        eprintln!("{line}");
    }

    println!(
        "\nDone: {} published, {} failed (of {} selected).",
        published,
        failures.len(),
        selected.len()
    );
    if published > 0 {
        println!("Widen with `history share <id>`; withdraw with `history unshare <id>`.");
    }

    if !failures.is_empty() {
        anyhow::bail!("{} session(s) failed to publish", failures.len());
    }
    Ok(())
}

/// Most publication requests one upload sends for a session before reporting
/// it as still syncing.
const MAX_ROUNDS: usize = 64;
/// Consecutive rounds whose request got no answer (it may or may not have been
/// applied) before upload stops; each resends the same frozen request.
const MAX_UNANSWERED_ROUNDS: u32 = 3;
/// Consecutive rounds another publisher held the runtime directory before
/// upload stops, half a second apart.
const MAX_BUSY_ROUNDS: u32 = 20;

/// Ingest one session into local history, share it with the owner only, and
/// publish until the server holds every message or refuses.
fn publish_one(
    dir: &std::path::Path,
    r: &SessionFileRef,
    owner: Option<&str>,
    runtime: &tokio::runtime::Runtime,
    to: &Destination<'_>,
) -> Result<(String, Status, usize)> {
    let session = parse_ref(r).context("parse error")?;
    let local_id = authz::adopt_native(dir, r.agent, &r.session_id, session.cwd.as_deref())?;
    History::open(dir)?.ingest(
        &local_id,
        &Source {
            agent: r.agent,
            native_id: r.session_id.clone(),
            origin: r.path.display().to_string(),
        },
        &session.messages,
    )?;
    let current = publication::status(dir, &local_id)?;
    // An existing audience stays: upload never narrows a wider share.
    let audience = current
        .audience
        .filter(|_| current.state != State::Private)
        .unwrap_or(Audience::Restricted {
            emails: Vec::new(),
            write_pending: Vec::new(),
        });
    publication::share(dir, &local_id, audience, owner, to.server)?;
    let retry = || {
        if HostOwner::running_runtime(to.server).is_some() {
            "the running host retries it"
        } else {
            "run `upload` again to resend it"
        }
    };
    let (mut unanswered, mut busy) = (0, 0);
    for _ in 0..MAX_ROUNDS {
        let pumped = runtime.block_on(publication::pump(dir, to))?;
        let status = publication::status(dir, &local_id)?;
        busy = if pumped.busy { busy + 1 } else { 0 };
        match status.state {
            State::Shared if status.pending == 0 => {
                return Ok((local_id, status, session.skipped_lines))
            }
            State::Error => anyhow::bail!(
                "{}",
                status.error.as_deref().unwrap_or("publication stopped")
            ),
            _ if pumped.busy => {
                if busy == MAX_BUSY_ROUNDS {
                    anyhow::bail!(
                        "not published yet: another publisher is using this host's history; {}",
                        retry()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            _ if pumped.sent == 0 => {
                unanswered += 1;
                if unanswered == MAX_UNANSWERED_ROUNDS {
                    anyhow::bail!(
                        "not published yet ({}); {}",
                        status
                            .last_attempt
                            .as_deref()
                            .unwrap_or("no request was acknowledged"),
                        retry()
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(u64::from(unanswered)));
            }
            _ => unanswered = 0,
        }
    }
    let status = publication::status(dir, &local_id)?;
    anyhow::bail!(
        "still syncing ({} published, {} pending); {}",
        status.published,
        status.pending,
        retry()
    )
}
