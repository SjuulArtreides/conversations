//! conversations-server binary entry point.
//!
//! Loads config from the environment, connects (and migrates) the SQLite
//! store, builds the router via the single [`build_router`] composition seam,
//! and serves it with graceful shutdown on SIGTERM / ctrl-c.
//!
//! [`build_router`]: conversations_server::build_router

use std::sync::Arc;
use std::time::Duration;

use conversations_server::{store, AppState, Config};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Tracing: respect RUST_LOG, default to `info`.
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer())
        .init();

    let config = Config::from_env();
    config.ensure_sqlite_parent_dir()?;

    // Fail fast when auth is enabled but its required settings are missing.
    if let Err(msg) = config.validate_auth() {
        anyhow::bail!(msg);
    }

    // Select the storage backend from the DATABASE_URL scheme (sqlite | postgres).
    let repo = store::connect(&config.database_url).await?;
    let auth_enabled = config.auth_enabled;
    let state = AppState::new(repo, Arc::new(config.clone()));
    // Keep a handle for the shutdown future; `build_router*` consumes `state`.
    let shutdown_state = state.clone();

    // Reap abandoned live sessions on a timer: mark stale `live` rows abandoned
    // and drop their leaked in-memory hubs. Detached; dies with the process.
    conversations_server::reaper::spawn(
        state.repo.clone(),
        state.live.clone(),
        Duration::from_secs(config.live_reap_interval_secs),
        Duration::from_secs(config.live_abandon_after_secs),
    );

    // Auth on → discover OIDC + layer the auth pieces; off → zero auth surface.
    let app = if auth_enabled {
        tracing::info!("AUTH_ENABLED: wiring Google OIDC auth");
        conversations_server::build_router_with_auth(state).await?
    } else {
        conversations_server::build_router(state)
    };

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "conversations-server listening");

    let drain = Duration::from_secs(config.shutdown_drain_secs);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_state, drain))
        .await?;

    tracing::info!("conversations-server stopped");
    Ok(())
}

/// Drive graceful shutdown when a termination signal arrives.
///
/// On Kubernetes a terminating pod is sent SIGTERM (ctrl-c covers local runs).
/// The sequence, in order:
///
/// 1. Flip the readiness flag so `/readyz` returns 503 — load balancers and
///    kube-proxy stop routing new traffic to this pod.
/// 2. Sleep `drain` seconds so that change propagates before we tear anything
///    down (this replaces a `preStop: sleep` and works on any base image).
/// 3. Close the live-session hubs, ending the long-lived SSE streams that would
///    otherwise never complete on their own and would block the drain.
///
/// Returning from this future is the signal to axum to stop accepting new
/// connections and drain remaining in-flight requests before `serve` resolves.
async fn shutdown_signal(state: AppState, drain: Duration) {
    wait_for_signal().await;
    tracing::info!("shutdown signal received; reporting unready and draining");

    // 1. Fail readiness immediately so the pod leaves the Service rotation.
    state.begin_shutdown();

    // 2. Give the load balancer / kube-proxy time to observe the failing probe
    //    and stop routing before we start closing connections.
    if !drain.is_zero() {
        tokio::time::sleep(drain).await;
    }

    // 3. Close open live (SSE) connections so the drain doesn't hang on streams
    //    that never end by themselves.
    state.live.shutdown_all();
    state.host_relay.close_all();
    tracing::info!("drain window elapsed; closing in-flight connections");
}

/// Resolve on the first SIGTERM (Unix) or ctrl-c. Errors installing a handler
/// are logged and treated as "never fires" rather than panicking — a missing
/// handler must not bring the process down.
async fn wait_for_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(%err, "failed to install ctrl-c handler");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::error!(%err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
