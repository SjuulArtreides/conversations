//! `conversations pr sync` — re-read the pull requests this machine has
//! been told about for a session, with the local `gh`, and push the snapshots
//! when local intent allows it (see `local_host::github`): a private host
//! session's PR content stays on this machine.
//!
//! The pull requests come from the host's OWN record (`session_pr_targets`, filled
//! only by signed `pr.sync` / `pr.comment` operations). A list supplied by the
//! server is never consulted, so the server cannot make this machine read a
//! pull request: the old server-driven sync stream and watch-targets poll were
//! removed. Polling now runs inside `host`.

use anyhow::{Context, Result};

use crate::cli::{GlobalOpts, PrArgs, PrCommand, PrSyncArgs};
use crate::client::ApiClient;

pub fn run(global: &GlobalOpts, args: &PrArgs) -> Result<()> {
    match &args.command {
        PrCommand::Sync(sync) => sync_run(global, sync),
    }
}

fn sync_run(global: &GlobalOpts, args: &PrSyncArgs) -> Result<()> {
    let session_id = args
        .session
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("`pr sync` needs a session: pass --session <id>"))?;
    let dir = std::path::absolute(
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".conversations-host-local"),
    )?;
    let client = ApiClient::new(&global.server)?;
    let recorded =
        crate::commands::local_host::github::sync_recorded_explicit(&dir, &client, session_id)
            .context("syncing the locally recorded pull requests")?;
    let summary = summary(session_id, &recorded);
    // A private session's pull requests refresh into the local pr-cache
    // (cached_locally) and legitimately never reach the central server,
    // so they do NOT constitute failure: the user asked for fresh data
    // locally and got it.
    if recorded.pushed + recorded.cached_locally < recorded.targets {
        anyhow::bail!(summary);
    }
    if recorded.failed > 0 {
        anyhow::bail!(summary);
    }
    live_info!("{summary}");
    Ok(())
}

/// What `pr sync` tells the user. It never says "synced" for a pull request
/// whose cache stayed on this host or whose push failed.
fn summary(session: &str, recorded: &crate::commands::local_host::github::Recorded) -> String {
    use crate::commands::local_host::github::Withheld;
    let mut parts = vec![format!(
        "session {session}: {} recorded PR(s), {} published",
        recorded.targets, recorded.pushed
    )];
    if recorded.cached_locally > 0 {
        parts.push(format!(
            "{} PR(s) refreshed into the local pr-cache (private session: not uploaded)",
            recorded.cached_locally
        ));
    }
    let private = recorded
        .withheld
        .iter()
        .filter(|w| **w == Withheld::Private)
        .count();
    if private > 0 {
        parts.push(format!(
            "the session is private on this host, so its PR cache is unavailable: {private} \
             PR(s) not fetched or uploaded (share the session to publish PR status)"
        ));
    }
    let unlinked = recorded
        .withheld
        .iter()
        .filter(|w| **w == Withheld::NotPublished)
        .count();
    if unlinked > 0 {
        parts.push(format!(
            "{unlinked} PR(s) not linked in the session's published history, so not fetched or \
             uploaded"
        ));
    }
    let other = recorded.withheld.len() - private - unlinked;
    if other > 0 {
        parts.push(format!(
            "{other} PR(s) not uploaded: the central copy does not match this host's sharing"
        ));
    }
    if recorded.failed > 0 {
        parts.push(format!("{} PR(s) failed (see above)", recorded.failed));
    }
    parts.join("; ")
}
