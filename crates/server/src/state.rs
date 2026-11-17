//! Shared application state passed to every handler via axum's `State`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::Config;
use crate::host_relay::HostRelay;
use crate::live::LiveRegistry;
use crate::llm::LlmClient;
use crate::store::Repository;

/// Cloneable application state.
///
/// Fields are `Arc`-wrapped so cloning the state (which axum does per request)
/// is cheap. The repository is a trait object so the storage backend can be
/// swapped without touching handlers. `llm` is `Some` only when an OpenRouter
/// key is configured — handlers treat `None` as "LLM features disabled".
#[derive(Clone)]
pub struct AppState {
    pub repo: Arc<dyn Repository>,
    pub config: Arc<Config>,
    pub llm: Option<Arc<LlmClient>>,
    /// In-memory fan-out hubs for live sessions (broadcast; replay comes from the
    /// durable `live_messages` table, not this).
    pub live: Arc<LiveRegistry>,
    /// Ephemeral opaque relay for signed host control (browser -> host).
    pub host_relay: Arc<HostRelay>,
    /// Set once the process receives a termination signal. While set, `/readyz`
    /// reports unready (503) so the load balancer drains this pod out of
    /// rotation before connections are torn down. See `main::shutdown_signal`.
    shutting_down: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(repo: Arc<dyn Repository>, config: Arc<Config>) -> Self {
        let llm = LlmClient::from_config(&config).map(Arc::new);
        AppState {
            repo,
            config,
            llm,
            live: Arc::new(LiveRegistry::new()),
            host_relay: Arc::new(HostRelay::default()),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the process has begun graceful shutdown. Drives the readiness
    /// probe so a terminating pod is pulled from the Service.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Mark the process as shutting down. Idempotent. After this, `/readyz`
    /// returns 503 regardless of backend health.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }
}
