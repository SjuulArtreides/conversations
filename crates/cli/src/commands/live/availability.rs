//! Host-wide network and Web-server availability coordination.
//!
//! A laptop sleep can leave several per-session TCP sockets looking established
//! even though the underlying route has disappeared. An idle socket may not
//! discover that until it next writes, which used to make mux sessions recover
//! one-by-one as the operator focused and typed into them.
//!
//! The host owns one coordinator instead. It separately probes (1) whether the
//! configured server's network endpoint is reachable and (2) whether the Web
//! application reports ready. Every transition back to `ServerReady` advances a
//! generation delivered over a Tokio watch channel. All session sockets observe
//! the same generation and reconnect together. A wall-clock polling gap also
//! invalidates the current state before probing, so a sleep/wake cycle forces a
//! fresh generation even when the network returned before the first probe ran.

use std::time::{Duration, SystemTime};

use tokio::net::TcpStream;
use tokio::sync::watch;

const PROBE_INTERVAL: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const WAKE_GAP: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reachability {
    /// Initial state, or a forced re-check immediately after a sleep/wake gap.
    Unknown,
    /// DNS/TCP cannot reach the configured Web-server endpoint.
    NetworkUnavailable,
    /// The endpoint is reachable, but `/readyz` is unavailable or not ready.
    ServerUnavailable,
    /// Both the endpoint and the Web application are available.
    ServerReady,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Availability {
    pub(crate) reachability: Reachability,
    /// Advances each time the host observes a transition into `ServerReady`.
    /// Session sockets use it as a host-wide reconnect epoch.
    pub(crate) ready_generation: u64,
}

#[derive(Clone)]
pub(crate) struct AvailabilitySignal {
    tx: watch::Sender<Availability>,
}

impl AvailabilitySignal {
    pub(crate) fn new() -> Self {
        let (tx, _rx) = watch::channel(Availability {
            reachability: Reachability::Unknown,
            ready_generation: 0,
        });
        Self { tx }
    }

    #[cfg(test)]
    pub(crate) fn subscribe(&self) -> watch::Receiver<Availability> {
        self.tx.subscribe()
    }

    /// Run the central reachability/readiness loop for the lifetime of the host.
    pub(crate) async fn monitor(self, server: String) {
        let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
            Ok(client) => client,
            Err(e) => {
                live_error!("conversations live: cannot build availability probe: {e}");
                return;
            }
        };
        let mut tick = tokio::time::interval(PROBE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_probe = SystemTime::now();

        loop {
            tick.tick().await;
            let now = SystemTime::now();
            if wall_clock_gap(last_probe, now) {
                live_error!(
                    "conversations live: system wake detected — checking network and Web server"
                );
                self.publish(Reachability::Unknown);
            }
            last_probe = now;

            self.publish(probe(&client, &server).await);
        }
    }

    fn publish(&self, reachability: Reachability) {
        let current = *self.tx.borrow();
        if current.reachability == reachability {
            return;
        }
        let recovered = reachability == Reachability::ServerReady
            && current.reachability != Reachability::ServerReady;
        let next = Availability {
            reachability,
            ready_generation: current.ready_generation + u64::from(recovered),
        };
        self.tx.send_replace(next);

        match reachability {
            Reachability::Unknown => {}
            Reachability::NetworkUnavailable => {
                live_error!("conversations live: network path to Web server unavailable");
            }
            Reachability::ServerUnavailable => {
                live_error!("conversations live: network available; Web server unavailable");
            }
            Reachability::ServerReady if recovered => {
                live_error!(
                    "conversations live: network and Web server available — reconnecting all sessions"
                );
            }
            Reachability::ServerReady => {}
        }
    }
}

fn wall_clock_gap(previous: SystemTime, now: SystemTime) -> bool {
    now.duration_since(previous)
        .map(|elapsed| elapsed > WAKE_GAP)
        .unwrap_or(true)
}

async fn probe(client: &reqwest::Client, server: &str) -> Reachability {
    let Ok(url) = reqwest::Url::parse(server) else {
        return Reachability::ServerUnavailable;
    };
    let Some(host) = url.host_str() else {
        return Reachability::ServerUnavailable;
    };
    let Some(port) = url.port_or_known_default() else {
        return Reachability::ServerUnavailable;
    };

    match tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect((host, port))).await {
        Ok(Ok(_)) => {}
        Ok(Err(_)) | Err(_) => return Reachability::NetworkUnavailable,
    }

    let ready_url = format!("{}/readyz", server.trim_end_matches('/'));
    match client.get(ready_url).send().await {
        Ok(response) if response.status().is_success() => Reachability::ServerReady,
        Ok(_) | Err(_) => Reachability::ServerUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn probe_server_with_status(status: u16) -> Reachability {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            // The coordinator intentionally separates the TCP reachability probe
            // from the application-readiness request, so accept both sockets.
            let (network_probe, _) = listener.accept().await.unwrap();
            drop(network_probe);
            let (mut ready_probe, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = ready_probe.read(&mut request).await.unwrap();
            let reason = if status == 200 {
                "OK"
            } else {
                "Service Unavailable"
            };
            ready_probe
                .write_all(
                    format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let client = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .unwrap();

        let result = probe(&client, &format!("http://{address}")).await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn server_recovery_wakes_every_session_generation() {
        let signal = AvailabilitySignal::new();
        let mut first_session = signal.subscribe();
        let mut second_session = signal.subscribe();

        signal.publish(Reachability::NetworkUnavailable);
        signal.publish(Reachability::ServerReady);

        first_session.changed().await.unwrap();
        second_session.changed().await.unwrap();
        let first = *first_session.borrow_and_update();
        let second = *second_session.borrow_and_update();

        assert_eq!(first.reachability, Reachability::ServerReady);
        assert_eq!(second.reachability, Reachability::ServerReady);
        assert_eq!(first.ready_generation, 1);
        assert_eq!(second.ready_generation, 1);
    }

    #[test]
    fn wake_gap_forces_a_fresh_readiness_transition() {
        let signal = AvailabilitySignal::new();
        signal.publish(Reachability::ServerReady);
        assert_eq!(signal.tx.borrow().ready_generation, 1);

        signal.publish(Reachability::Unknown);
        signal.publish(Reachability::ServerReady);
        assert_eq!(signal.tx.borrow().ready_generation, 2);
    }

    #[test]
    fn detects_wall_clock_gap_after_laptop_sleep() {
        let before = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        assert!(!wall_clock_gap(before, before + PROBE_INTERVAL));
        assert!(wall_clock_gap(
            before,
            before + WAKE_GAP + Duration::from_secs(1)
        ));
    }

    #[tokio::test]
    async fn probe_distinguishes_network_from_server_readiness() {
        assert_eq!(
            probe_server_with_status(503).await,
            Reachability::ServerUnavailable
        );
        assert_eq!(
            probe_server_with_status(200).await,
            Reachability::ServerReady
        );
    }
}
