//! In-memory fan-out for live sessions.
//!
//! Each live session gets a [`LiveHub`] holding a [`tokio::sync::broadcast`]
//! sender. Browsers on `GET /api/live/{id}/stream` subscribe to the hub and
//! forward events as SSE frames; handlers publish a [`LiveEvent`] when session
//! metadata changes (PR/Linear links, PR comments, completion).
//!
//! The broadcast buffer is NOT the replay source — a mid-session subscriber
//! replays from the durable `live_messages` table first, then attaches to the
//! hub for new events. The buffer only needs to be large enough to bridge the
//! brief window between "replay finished" and "subscribed".
//!
//! Correct for the single-replica deployment (`replicas: 1`, Recreate, RWO
//! volume, SQLite single writer). Multi-replica would need a shared bus
//! (Postgres LISTEN/NOTIFY or pod-pinning) — out of scope, flagged in the spec.
//!
//! There is deliberately no browser→host or host→browser byte channel here:
//! the central live mirror and every central control channel (replies, raw
//! input, clipboard, files, PTY websockets, PR relays) were removed. Nothing
//! the server stores or relays may reach a session's process, terminal or
//! arguments on a host machine; interactive control exists only as signed
//! operations between a locally approved key and the host, end-to-end on the
//! private channel.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use tokio::sync::broadcast;

/// Broadcast channel capacity per live hub. Subscribers that lag past this many
/// buffered events get a `RecvError::Lagged`; the stream handler treats that as
/// non-fatal (the durable table remains the source of truth).
const HUB_CAPACITY: usize = 256;

/// One live session's in-memory event channel.
#[derive(Clone)]
pub enum LiveEvent {
    /// The live session was completed and graduated into a stored session.
    Complete { session_id: String },
    /// One or more new PR references were auto-detected and linked to the
    /// session (a genuine new link, not a dedup no-op). Browsers refetch the
    /// session's PR list when they see this, so the PR widgets update live
    /// without a manual "Sync now". Carries no PR payload — it's a refetch
    /// nudge; the authoritative list comes from `GET /api/sessions/{id}/prs`.
    PrsLinked { session_id: String },
    /// A small signal; viewers with an open panel fetch only changed rows.
    PrCommentsChanged,
    /// One or more new Linear references (document / issue / project) were
    /// auto-detected and linked to the session. The Linear analogue of
    /// [`LiveEvent::PrsLinked`] — a refetch nudge carrying no payload; browsers
    /// reload `GET /api/sessions/{id}/linear-docs` when they see it.
    LinearDocsLinked { session_id: String },
}

/// One live session's in-memory event hub (handlers → browsers).
#[derive(Clone)]
pub struct LiveHub {
    sender: broadcast::Sender<LiveEvent>,
}

impl LiveHub {
    fn new() -> Self {
        let (sender, _rx) = broadcast::channel(HUB_CAPACITY);
        LiveHub { sender }
    }

    /// Subscribe to this hub's event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.sender.subscribe()
    }

    /// Publish an event. Returns the number of subscribers it reached (0 when
    /// none are attached — that is fine, the durable tables remain the source
    /// of truth).
    pub fn publish(&self, event: LiveEvent) -> usize {
        self.sender.send(event).unwrap_or(0)
    }
}

/// All live-session hubs, keyed by live session id. In-memory only: hubs are
/// created on demand and dropped on completion; nothing here survives a
/// restart, and nothing here is needed for correctness (durable tables are the
/// source of truth).
#[derive(Default)]
pub struct LiveRegistry {
    hubs: Mutex<HashMap<String, LiveHub>>,
}

impl LiveRegistry {
    pub fn new() -> Self {
        LiveRegistry::default()
    }

    /// Get the existing hub for `live_id`, or create one. Used as a fallback by
    /// the stream handler (so a server restart that dropped the in-memory hub
    /// can still serve replay + future events).
    pub fn get_or_create(&self, live_id: &str) -> LiveHub {
        let mut hubs = self.hubs.lock().expect("live registry mutex poisoned");
        hubs.entry(live_id.to_string())
            .or_insert_with(LiveHub::new)
            .clone()
    }

    /// Subscribe to a live session's hub, creating the hub if needed.
    pub fn subscribe(&self, live_id: &str) -> broadcast::Receiver<LiveEvent> {
        self.get_or_create(live_id).subscribe()
    }

    /// Publish an event to a live session's hub, creating the hub if needed.
    /// Returns the number of subscribers reached.
    pub fn publish(&self, live_id: &str, event: LiveEvent) -> usize {
        self.get_or_create(live_id).publish(event)
    }

    /// Drop a live session's hub (on completion or deletion). Any still-attached
    /// receivers see the channel close after they drain buffered events.
    pub fn drop_hub(&self, live_id: &str) {
        self.hubs
            .lock()
            .expect("live registry mutex poisoned")
            .remove(live_id);
    }

    /// Every live-session id with a hub currently held. Used by the reaper to
    /// reconcile in-memory hubs against the set of still-`live` sessions in the
    /// database.
    pub fn session_hub_ids(&self) -> HashSet<String> {
        self.hubs
            .lock()
            .expect("live registry mutex poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// How many subscribers are currently attached to a live session's hub.
    pub fn session_subscriber_count(&self, live_id: &str) -> usize {
        self.hubs
            .lock()
            .expect("live registry mutex poisoned")
            .get(live_id)
            .map(|h| h.sender.receiver_count())
            .unwrap_or(0)
    }

    /// Tear down every hub at once, dropping all broadcast senders.
    ///
    /// Used on graceful shutdown: each open SSE stream forwards from a
    /// broadcast receiver, so dropping the senders makes those receivers see
    /// `Closed`, ending the forwarding tasks and closing the SSE connections —
    /// the same mechanism `drop_hub` uses on session completion, applied to
    /// every session at once. Idempotent; safe to call when empty.
    pub fn shutdown_all(&self) {
        self.hubs
            .lock()
            .expect("live registry mutex poisoned")
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_reaches_subscribers_and_drop_closes_the_channel() {
        let registry = LiveRegistry::new();
        let mut rx = registry.subscribe("s1");
        assert_eq!(
            registry.publish(
                "s1",
                LiveEvent::PrsLinked {
                    session_id: "s1".into()
                }
            ),
            1
        );
        assert!(matches!(rx.recv().await, Ok(LiveEvent::PrsLinked { .. })));
        registry.drop_hub("s1");
        // A new event after the drop has nowhere to go; the old receiver sees
        // the channel close once its buffer drains.
        assert_eq!(registry.publish("s1", LiveEvent::PrCommentsChanged), 0);
        assert!(rx.recv().await.is_err());
    }

    #[test]
    fn publish_with_no_subscriber_is_a_noop_not_an_error() {
        let registry = LiveRegistry::new();
        assert_eq!(registry.publish("nobody", LiveEvent::PrCommentsChanged), 0);
        assert_eq!(registry.session_subscriber_count("nobody"), 0);
    }

    #[test]
    fn session_hub_ids_reflects_current_hubs_only() {
        let registry = LiveRegistry::new();
        registry.get_or_create("a");
        registry.get_or_create("b");
        registry.drop_hub("a");
        let ids = registry.session_hub_ids();
        assert!(!ids.contains("a") && ids.contains("b"));
        registry.shutdown_all();
        assert!(registry.session_hub_ids().is_empty());
    }
}
