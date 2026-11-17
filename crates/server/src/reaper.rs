//! Background reaper for abandoned live sessions.
//!
//! A live session is created (a `live_sessions` row + in-memory broadcast hubs)
//! when a CLI registers it, and torn down when the owning CLI POSTs `/complete`
//! (or a viewer dismisses it). If the CLI process dies without graduating —
//! crash, network drop, killed pod — neither happens: the DB row stays
//! `status = 'live'` (a ghost in the live list) and the in-memory hubs leak (the
//! registry maps grow unbounded over the process lifetime).
//!
//! This reaper closes both leaks on a timer. Each pass:
//!   1. marks `live` rows whose `last_event_at` predates the abandon cutoff as
//!      `abandoned` (so they leave the live list), and
//!   2. reconciles the in-memory hubs against the database: any hub whose
//!      session is no longer `live` (just-abandoned, or graduated with a missed
//!      drop) is dropped, freeing its broadcast channels.
//!
//! Dropping a hub is safe even if a CLI later reconnects: the registry's
//! `get_or_create*` paths rebuild it on the next subscribe/publish. The reaper
//! therefore only reclaims memory — it never severs an active stream. Two guards
//! enforce that: it skips a hub whose row re-read as `live` (a reconnect), and it
//! never drops a hub that still has an attached receiver
//! ([`LiveRegistry::session_subscriber_count`] > 0). The second is the
//! definitive one — a zero-receiver hub cannot disconnect anyone — so the
//! liveness re-read is just an optimisation that also preserves the rebuilt
//! hub's PTY snapshot for an imminent viewer.
//!
//! The CLI sends a `/heartbeat` every 30s, so the abandon threshold MUST stay
//! comfortably above that or an idle-but-live interactive session (a turn with
//! no streamed output) gets reaped mid-turn. The default (5 min) is 10× the
//! heartbeat interval.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::error::Result;
use crate::live::LiveRegistry;
use crate::store::{Repository, LIVE_STATUS_LIVE};

/// Run one reaper pass: mark stale `live` rows abandoned, then drop any
/// in-memory hub whose session is no longer `live`. Returns
/// `(rows_marked_abandoned, hubs_dropped)` for logging and tests.
pub async fn reap_once(
    repo: &dyn Repository,
    live: &LiveRegistry,
    abandon_after: Duration,
) -> Result<(u64, usize)> {
    // `chrono::Duration::from_std` only fails for absurdly large values; fall
    // back to the 5-minute default rather than skewing the cutoff.
    let window = chrono::Duration::from_std(abandon_after)
        .unwrap_or_else(|_| chrono::Duration::seconds(300));
    let cutoff = Utc::now() - window;
    let marked = repo.mark_abandoned_live_sessions(cutoff).await?;

    // Reconcile in-memory hubs against the database's still-`live` set. viewer =
    // None ⇒ every live session regardless of auth, which is what a global
    // reconcile needs.
    let live_ids: HashSet<String> = repo
        .list_live_sessions(Some(LIVE_STATUS_LIVE), None)
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect();

    let mut dropped = 0;
    for id in live.session_hub_ids() {
        if live_ids.contains(&id) {
            continue;
        }
        // `live_ids` was snapshotted before this loop, but a session can
        // legitimately transition back to `live` in the window — a CLI reconnect
        // re-registers the row (`ON CONFLICT … SET status='live'`) and rebuilds
        // its hub. Re-read liveness immediately before dropping and skip if it is
        // live again; this also preserves the rebuilt hub's PTY snapshot for a
        // viewer that is about to connect.
        if let Some(s) = repo.get_live_session(&id).await? {
            if s.status == LIVE_STATUS_LIVE {
                continue;
            }
        }
        // Definitive anti-sever guard, closing the residual recheck→drop race:
        // never drop a hub that still has an attached receiver. A web viewer's
        // SSE stream / an attached CLI consumer each hold a `broadcast::Receiver`
        // (`Closed` on drop ends their stream), so reclaiming is only safe when
        // the count is zero — and a zero-receiver hub cannot sever anyone even if
        // a reconnect rebuilt it microseconds ago (it is rebuilt again on the
        // next subscribe/publish). A leaked hub a stale viewer still watches is
        // therefore retained until that viewer leaves, then reclaimed next pass.
        if live.session_subscriber_count(&id) > 0 {
            continue;
        }
        live.drop_hub(&id);
        dropped += 1;
    }
    Ok((marked, dropped))
}

/// Spawn the reaper loop as a detached task: run [`reap_once`] every `interval`,
/// logging failures and carrying on. The task lives for the process lifetime;
/// the runtime drops it on shutdown. A zero `interval` disables the reaper
/// (returns without spawning) — handy for tests and single-shot CLIs.
pub fn spawn(
    repo: Arc<dyn Repository>,
    live: Arc<LiveRegistry>,
    interval: Duration,
    abandon_after: Duration,
) {
    if interval.is_zero() {
        tracing::info!("live reaper disabled (interval = 0)");
        return;
    }
    tracing::info!(
        interval_secs = interval.as_secs(),
        abandon_after_secs = abandon_after.as_secs(),
        "spawning live-session reaper"
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // `interval` fires immediately on the first `tick`; consume it so the
        // first real pass waits a full window (a session registered moments
        // before startup gets time for its CLI to attach).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match reap_once(repo.as_ref(), live.as_ref(), abandon_after).await {
                Ok((marked, dropped)) if marked > 0 || dropped > 0 => {
                    tracing::info!(marked, dropped, "live reaper pass");
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, "live reaper pass failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewLiveSession, SqliteRepo};
    use conversations_core::AgentKind;

    /// A unique temp-file-backed SqliteRepo (in-memory sqlite is per-connection,
    /// so a temp file is used to survive across the pool's connections).
    async fn temp_repo() -> (SqliteRepo, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("conv-reaper-{nanos}-{n}.sqlite"));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let repo = SqliteRepo::connect(&url).await.expect("connect + migrate");
        (repo, path)
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    fn new_live(id: &str) -> NewLiveSession {
        NewLiveSession {
            id: id.to_string(),
            session_id: id.to_string(),
            agent: AgentKind::Claude,
            title: Some(format!("live {id}")),
            project_path: Some("/tmp/proj".to_string()),
            cwd: Some("/tmp/proj".to_string()),
            git_branch: Some("main".to_string()),
            owner_email: Some("owner@x".to_string()),
            source_host: Some("host-1".to_string()),
            remote_url: None,
            mode: "view".to_string(),
        }
    }

    #[tokio::test]
    async fn reaps_abandoned_session_and_drops_its_hub() {
        let (repo, path) = temp_repo().await;
        let live = LiveRegistry::new();

        repo.create_live_session(&new_live("stale")).await.unwrap();
        // Simulate the CLI having attached: a hub exists in memory.
        live.get_or_create("stale");
        assert!(live.session_hub_ids().contains("stale"));

        // Zero window ⇒ cutoff = now; the row's `last_event_at` (set at create,
        // moments ago) is older, so it is marked abandoned and its hub dropped.
        let (marked, dropped) = reap_once(&repo, &live, Duration::ZERO).await.unwrap();
        assert_eq!((marked, dropped), (1, 1));

        let row = repo.get_live_session("stale").await.unwrap().unwrap();
        assert_eq!(row.status, "abandoned");
        assert!(
            live.session_hub_ids().is_empty(),
            "abandoned session's hub is reclaimed"
        );
        cleanup(&path);
    }

    #[tokio::test]
    async fn leaves_a_fresh_live_session_and_its_hub_alone() {
        let (repo, path) = temp_repo().await;
        let live = LiveRegistry::new();

        repo.create_live_session(&new_live("fresh")).await.unwrap();
        live.get_or_create("fresh");

        // A generous window: the just-created row is well within it, so nothing
        // is marked and the hub of a still-`live` session is retained.
        let (marked, dropped) = reap_once(&repo, &live, Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!((marked, dropped), (0, 0));
        assert!(live.session_hub_ids().contains("fresh"));

        let row = repo.get_live_session("fresh").await.unwrap().unwrap();
        assert_eq!(row.status, "live");
        cleanup(&path);
    }

    #[tokio::test]
    async fn reconcile_drops_a_graduated_sessions_leaked_hub() {
        let (repo, path) = temp_repo().await;
        let live = LiveRegistry::new();

        repo.create_live_session(&new_live("grad")).await.unwrap();
        live.get_or_create("grad");

        // Graduate the session WITHOUT calling drop_hub — simulate a missed
        // teardown (the leak the reconcile pass is meant to self-correct).
        let graduated = repo.complete_live_session("grad").await.unwrap();
        assert_eq!(graduated.as_deref(), Some("grad"));
        assert!(live.session_hub_ids().contains("grad"), "hub still leaked");

        // A generous window so the abandon step is a no-op; the reconcile step
        // still drops the hub because the session is no longer `live`.
        let (marked, dropped) = reap_once(&repo, &live, Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!((marked, dropped), (0, 1));
        assert!(live.session_hub_ids().is_empty());
        cleanup(&path);
    }

    /// A non-`live` session whose hub still has an attached receiver (a viewer
    /// mid-stream, e.g. a reconnect that landed in the recheck→drop window) is
    /// NOT dropped — that would sever the stream. Once the receiver goes away
    /// the next pass reclaims it.
    #[tokio::test]
    async fn does_not_drop_a_hub_with_an_attached_subscriber() {
        let (repo, path) = temp_repo().await;
        let live = LiveRegistry::new();

        repo.create_live_session(&new_live("watched"))
            .await
            .unwrap();
        // A viewer is attached: hold a live receiver on the session's hub.
        let rx = live.subscribe("watched");
        assert_eq!(live.session_subscriber_count("watched"), 1);

        // Graduate it (no longer `live`) — but the attached receiver must keep
        // the hub alive through the reconcile pass.
        repo.complete_live_session("watched").await.unwrap();
        let (_, dropped) = reap_once(&repo, &live, Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(dropped, 0, "a hub with a live subscriber is never severed");
        assert!(live.session_hub_ids().contains("watched"));

        // The viewer leaves; the now-idle hub is reclaimed on the next pass.
        drop(rx);
        let (_, dropped) = reap_once(&repo, &live, Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(dropped, 1);
        assert!(live.session_hub_ids().is_empty());
        cleanup(&path);
    }
}
