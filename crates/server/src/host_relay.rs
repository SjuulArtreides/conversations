//! Opaque browser -> server -> host relay for signed host control.
//!
//! The server forwards a closed set of requests (see
//! `conversations_core::host_relay`) to the host that holds the SSE stream for
//! a given host id and returns the host's answer. It holds no key and cannot
//! approve a credential or make a host act: the host checks every request
//! against locally stored keys. Account auth only decides who may use the
//! transport (nuisance control); it is never execution authority, so ANY
//! authenticated account may route an opaque signed request to a pinned host id
//! (a message from Alice's agent to Bob's host is admitted or refused by Bob's
//! host from its own keys and grants).
//!
//! The host id is public, so a stream is attached only with a proof that the
//! connecting party holds the host's private key: it signs a one-time server
//! nonce (the id is the SHA-256 of the public key it presents). Replies are
//! accepted only with a per-attachment token delivered over that proven stream.
//! A hostile server can still drop or delay traffic, but cannot forge a host
//! signature, and another ordinary account can neither claim the id nor answer
//! for it.
//!
//! State is in memory and bounded: one stream per host id (a newer proven
//! stream replaces the older one), [`MAX_HOSTS_PER_OWNER`] hosts per attaching
//! account, [`MAX_PENDING`] unanswered requests per host and
//! [`MAX_PER_REQUESTER`] per requesting account. Requests are never retried, so
//! a timeout never duplicates a side effect.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use conversations_core::host_relay::{
    valid_host_id, RelayAnswer, RelayEnvelope, RelayReply, RelayRequest, Signed,
    REQUEST_TIMEOUT_SECS,
};
use futures::Stream;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt as _;

use crate::api::sse_keep_alive;
use crate::identity::CurrentUser;
use crate::state::AppState;

pub const MAX_HOSTS_PER_OWNER: usize = 4;
pub const MAX_PENDING: usize = 8;
pub const MAX_PER_REQUESTER: usize = 4;
const CHALLENGE_TTL: Duration = Duration::from_secs(30);
const MAX_CHALLENGES: usize = 256;
const QUEUE: usize = 8;

type Key = String;

struct Channel {
    generation: u64,
    /// The account that attached (counts toward its host limit).
    owner: String,
    /// The public key the attachment proved possession of (id = its SHA-256).
    public: String,
    /// The machine card the host presented, opaque here (the reader verifies it
    /// against the identity key it trusts).
    card: Option<Signed>,
    /// Required on replies; known only to whoever holds the proven stream.
    token: String,
    to_host: mpsc::Sender<RelayEnvelope>,
    pending: HashMap<String, (String, oneshot::Sender<RelayReply>)>,
}

#[derive(Default)]
struct Inner {
    closed: bool,
    next_generation: u64,
    hosts: HashMap<Key, Channel>,
    challenges: Vec<(String, String, Instant)>,
}

/// Ephemeral relay registry. Nothing here is persisted.
#[derive(Default)]
pub struct HostRelay {
    inner: Mutex<Inner>,
    timeout_override: Mutex<Option<Duration>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RelayError {
    HostOffline,
    Busy,
    HostDisconnected,
    Timeout,
    BadRequest(&'static str),
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            RelayError::HostOffline => (
                StatusCode::SERVICE_UNAVAILABLE,
                "host_offline",
                "That host is not connected. Start it with `conversations host`.",
            ),
            RelayError::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "busy",
                "The host has too many requests in flight. Try again shortly.",
            ),
            RelayError::HostDisconnected => (
                StatusCode::BAD_GATEWAY,
                "host_disconnected",
                "The host disconnected before answering. The request was not retried.",
            ),
            RelayError::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "host_timeout",
                "The host did not answer in time. The request was not retried and may or may not have run.",
            ),
            RelayError::BadRequest(message) => (StatusCode::BAD_REQUEST, "bad_request", message),
        };
        (status, Json(json!({ "error": message, "code": code }))).into_response()
    }
}

/// A host's attachment. Dropping it detaches the host.
pub struct HostStream {
    relay: Arc<HostRelay>,
    key: Key,
    generation: u64,
    pub requests: mpsc::Receiver<RelayEnvelope>,
}

impl HostStream {
    /// Whether a newer stream replaced this one.
    pub fn superseded(&self) -> bool {
        let inner = self.relay.inner.lock().expect("host relay mutex poisoned");
        inner
            .hosts
            .get(&self.key)
            .is_some_and(|channel| channel.generation != self.generation)
    }
}

impl Drop for HostStream {
    fn drop(&mut self) {
        let mut inner = self.relay.inner.lock().expect("host relay mutex poisoned");
        // Only remove our own channel: a replacement must stay attached.
        if inner
            .hosts
            .get(&self.key)
            .is_some_and(|channel| channel.generation == self.generation)
        {
            inner.hosts.remove(&self.key);
        }
    }
}

impl HostRelay {
    /// End streams and pending requests before the HTTP server drains.
    pub fn close_all(&self) {
        let mut inner = self.inner.lock().expect("host relay mutex poisoned");
        inner.closed = true;
        inner.hosts.clear();
        inner.challenges.clear();
    }

    fn timeout(&self) -> Duration {
        self.timeout_override
            .lock()
            .expect("host relay mutex poisoned")
            .unwrap_or(Duration::from_secs(REQUEST_TIMEOUT_SECS))
    }

    /// Shorten the wait for tests.
    pub fn set_timeout(&self, timeout: Duration) {
        *self
            .timeout_override
            .lock()
            .expect("host relay mutex poisoned") = Some(timeout);
    }

    /// Attach a host. A newer attachment for the same key replaces the older
    /// one and fails everything the older one had not answered.
    /// Issue a one-time nonce the host must sign to attach as `host_id`.
    pub fn challenge(&self, host_id: &str) -> Result<String, RelayError> {
        let mut inner = self.inner.lock().expect("host relay mutex poisoned");
        let now = Instant::now();
        inner
            .challenges
            .retain(|(_, _, at)| now.duration_since(*at) < CHALLENGE_TTL);
        if inner.challenges.len() >= MAX_CHALLENGES
            || inner
                .challenges
                .iter()
                .filter(|(h, _, _)| h == host_id)
                .count()
                >= 4
        {
            return Err(RelayError::Busy);
        }
        let nonce = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        inner
            .challenges
            .push((host_id.to_owned(), nonce.clone(), now));
        Ok(nonce)
    }

    /// Consume a nonce issued for `host_id` (single use, short lived).
    fn take_challenge(&self, host_id: &str, nonce: &str) -> bool {
        let mut inner = self.inner.lock().expect("host relay mutex poisoned");
        let now = Instant::now();
        inner
            .challenges
            .retain(|(_, _, at)| now.duration_since(*at) < CHALLENGE_TTL);
        match inner
            .challenges
            .iter()
            .position(|(h, n, _)| h == host_id && n == nonce)
        {
            Some(index) => {
                inner.challenges.remove(index);
                true
            }
            None => false,
        }
    }

    /// Attach a host that has ALREADY proven key possession. Returns the stream
    /// and the reply token for this attachment.
    pub fn attach(
        self: &Arc<Self>,
        owner: &str,
        host_id: &str,
        public: &str,
        card: Option<Signed>,
    ) -> Result<(HostStream, String), RelayError> {
        let key = host_id.to_owned();
        let mut inner = self.inner.lock().expect("host relay mutex poisoned");
        if inner.closed {
            return Err(RelayError::HostOffline);
        }
        if !inner.hosts.contains_key(&key)
            && inner.hosts.values().filter(|c| c.owner == owner).count() >= MAX_HOSTS_PER_OWNER
        {
            return Err(RelayError::Busy);
        }
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        inner.next_generation += 1;
        let generation = inner.next_generation;
        let (to_host, requests) = mpsc::channel(QUEUE);
        // Replacing drops the old sender and every pending answer channel.
        inner.hosts.insert(
            key.clone(),
            Channel {
                generation,
                owner: owner.to_owned(),
                public: public.to_owned(),
                card,
                token: token.clone(),
                to_host,
                pending: HashMap::new(),
            },
        );
        Ok((
            HostStream {
                relay: self.clone(),
                key,
                generation,
                requests,
            },
            token,
        ))
    }

    /// The hosts `owner` has attached right now: id, public key and card.
    pub fn attached_by(&self, owner: &str) -> Vec<AttachedHost> {
        let inner = self.inner.lock().expect("host relay mutex poisoned");
        let mut hosts: Vec<_> = inner
            .hosts
            .iter()
            .filter(|(_, channel)| channel.owner == owner)
            .map(|(id, channel)| AttachedHost {
                id: id.clone(),
                key: channel.public.clone(),
                card: channel.card.clone(),
            })
            .collect();
        hosts.sort_by(|a, b| a.id.cmp(&b.id));
        hosts
    }

    pub fn is_online(&self, host_id: &str) -> bool {
        self.inner
            .lock()
            .expect("host relay mutex poisoned")
            .hosts
            .contains_key(host_id)
    }

    /// Deliver `request` to the host and wait for its answer.
    pub async fn submit(
        &self,
        requester: &str,
        host_id: &str,
        request: RelayRequest,
    ) -> Result<RelayReply, RelayError> {
        let key = host_id.to_owned();
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (answer_tx, answer_rx) = oneshot::channel();
        {
            let mut inner = self.inner.lock().expect("host relay mutex poisoned");
            let channel = inner.hosts.get_mut(&key).ok_or(RelayError::HostOffline)?;
            if channel.pending.len() >= MAX_PENDING
                || channel
                    .pending
                    .values()
                    .filter(|(who, _)| who == requester)
                    .count()
                    >= MAX_PER_REQUESTER
            {
                return Err(RelayError::Busy);
            }
            channel
                .to_host
                .try_send(RelayEnvelope {
                    id: id.clone(),
                    request,
                })
                .map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => RelayError::Busy,
                    mpsc::error::TrySendError::Closed(_) => RelayError::HostOffline,
                })?;
            channel
                .pending
                .insert(id.clone(), (requester.to_owned(), answer_tx));
        }
        let outcome = tokio::time::timeout(self.timeout(), answer_rx).await;
        match outcome {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(RelayError::HostDisconnected),
            Err(_) => {
                if let Some(channel) = self
                    .inner
                    .lock()
                    .expect("host relay mutex poisoned")
                    .hosts
                    .get_mut(&key)
                {
                    channel.pending.remove(&id);
                }
                Err(RelayError::Timeout)
            }
        }
    }

    /// Accept a host's answer for a request it was sent. An unknown, stale or
    /// already-answered id, another host's, or one without this attachment's
    /// token is refused.
    pub fn answer(&self, host_id: &str, token: &str, answer: RelayAnswer) -> bool {
        let Some(reply) = answer.reply.sanitized() else {
            return false;
        };
        let waiter = {
            let mut inner = self.inner.lock().expect("host relay mutex poisoned");
            inner
                .hosts
                .get_mut(host_id)
                .filter(|channel| constant_eq(&channel.token, token))
                .and_then(|channel| channel.pending.remove(&answer.id))
        };
        waiter.is_some_and(|(_, waiter)| waiter.send(reply).is_ok())
    }
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Whether `sig` over the attach message proves possession of the key whose
/// SHA-256 is `host_id`.
fn proves_host(host_id: &str, public: &str, nonce: &str, sig: &str) -> bool {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use base64::Engine;
    let (Ok(public), Ok(sig)) = (B64.decode(public), B64.decode(sig)) else {
        return false;
    };
    if public.len() != 32
        || B64.encode(ring::digest::digest(&ring::digest::SHA256, &public).as_ref()) != host_id
    {
        return false;
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &public)
        .verify(
            &conversations_core::host_relay::attach_message(host_id, nonce),
            &sig,
        )
        .is_ok()
}

#[derive(serde::Deserialize)]
pub struct AttachParams {
    nonce: String,
    #[serde(rename = "pub")]
    public: String,
    sig: String,
    /// The host's machine card (payload and signature, base64url). Optional:
    /// older hosts attach without one.
    card_p: Option<String>,
    card_s: Option<String>,
}

/// A host the caller's account has attached, as `GET /api/host-relay` lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AttachedHost {
    pub id: String,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<Signed>,
}

/// A presented card, if it is well-formed base64url within the size limit.
/// Its signature is not checked here: the server is not trusted with it.
fn presented_card(p: Option<String>, s: Option<String>) -> Result<Option<Signed>, RelayError> {
    use conversations_core::host_relay::MAX_MACHINE_CARD_BYTES;
    let (Some(p), Some(s)) = (p, s) else {
        return Ok(None);
    };
    let base64url = |v: &str| {
        v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if p.len() + s.len() > MAX_MACHINE_CARD_BYTES || !base64url(&p) || !base64url(&s) {
        return Err(RelayError::BadRequest("invalid machine card"));
    }
    Ok(Some(Signed { p, s }))
}

/// `POST /api/host-relay/{host_id}/challenge`: a one-time nonce to sign.
pub async fn challenge(
    State(state): State<AppState>,
    Path(host_id): Path<String>,
) -> Result<Json<serde_json::Value>, RelayError> {
    checked_host(&host_id)?;
    Ok(Json(
        json!({ "nonce": state.host_relay.challenge(&host_id)? }),
    ))
}

fn owner_of(viewer: Option<String>) -> String {
    // Auth off: one anonymous namespace. This only scopes the transport; the
    // host still verifies every operation against its own keys.
    viewer.unwrap_or_default()
}

fn checked_host(host_id: &str) -> Result<(), RelayError> {
    valid_host_id(host_id)
        .then_some(())
        .ok_or(RelayError::BadRequest("invalid host id"))
}

/// `GET /api/host-relay/{host_id}/stream` (host -> server): requests for this
/// host. The first event on a replaced stream is `{"type":"superseded"}`.
pub async fn stream(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(host_id): Path<String>,
    Query(proof): Query<AttachParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, RelayError> {
    checked_host(&host_id)?;
    // Consume the nonce first so a failed proof cannot be retried with it.
    let fresh = state.host_relay.take_challenge(&host_id, &proof.nonce);
    if !fresh || !proves_host(&host_id, &proof.public, &proof.nonce, &proof.sig) {
        return Err(RelayError::BadRequest("host identity proof failed"));
    }
    let card = presented_card(proof.card_p, proof.card_s)?;
    let (mut host, token) =
        state
            .host_relay
            .attach(&owner_of(viewer), &host_id, &proof.public, card)?;
    let (tx, rx) = mpsc::channel::<Event>(QUEUE);
    let first = serde_json::json!({"type": "attached", "reply_token": token}).to_string();
    let _ = tx.send(Event::default().data(first)).await;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tx.closed() => return,
                next = host.requests.recv() => match next {
                    Some(envelope) => {
                        let Ok(data) = serde_json::to_string(&envelope) else { continue };
                        if tx.send(Event::default().data(data)).await.is_err() {
                            return;
                        }
                    }
                    None => {
                        if host.superseded() {
                            let _ = tx.send(Event::default().data(r#"{"type":"superseded"}"#)).await;
                        }
                        return;
                    }
                }
            }
        }
    });
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok);
    Ok(Sse::new(stream).keep_alive(sse_keep_alive()))
}

/// `POST /api/host-relay/{host_id}/reply` (host -> server).
pub async fn reply(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(host_id): Path<String>,
    Json(answer): Json<RelayAnswer>,
) -> Result<StatusCode, RelayError> {
    checked_host(&host_id)?;
    let token = headers
        .get("x-relay-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if state.host_relay.answer(&host_id, token, answer) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(RelayError::BadRequest("no matching request"))
    }
}

/// `POST /api/host-relay/{host_id}/request` (browser -> server -> host).
pub async fn request(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
    Path(host_id): Path<String>,
    Json(request): Json<RelayRequest>,
) -> Result<Json<RelayReply>, RelayError> {
    checked_host(&host_id)?;
    state
        .host_relay
        .submit(&owner_of(viewer), &host_id, request)
        .await
        .map(Json)
}

/// `GET /api/host-relay/{host_id}/status` -> `{ "online": bool }`.
pub async fn status(
    State(state): State<AppState>,
    Path(host_id): Path<String>,
) -> Result<Json<serde_json::Value>, RelayError> {
    checked_host(&host_id)?;
    Ok(Json(json!({
        "online": state.host_relay.is_online(&host_id)
    })))
}

/// `GET /api/host-relay` -> `{ "hosts": [{ "id", "key", "card"? }] }`: the
/// hosts the caller's own account has attached. Each key was proven at attach
/// time; a card is passed through unchecked. A browser uses this to find which
/// of its account's machines to ask whether they approved its key, and a host
/// to offer its owner the machines to pair; signatures and the owner decide.
pub async fn attached(
    State(state): State<AppState>,
    CurrentUser(viewer): CurrentUser,
) -> Json<serde_json::Value> {
    if viewer.is_none() && state.config.auth_enabled {
        return Json(json!({ "hosts": [] }));
    }
    let hosts: Vec<_> = state
        .host_relay
        .attached_by(&owner_of(viewer))
        .into_iter()
        .map(|host| json!(host))
        .collect();
    Json(json!({ "hosts": hosts }))
}

/// The relay's routes. Registered once, from `api::routes`.
pub fn routes() -> axum::Router<AppState> {
    use axum::extract::DefaultBodyLimit;
    use axum::routing::{get, post};
    use conversations_core::host_relay::MAX_REQUEST_BYTES;
    axum::Router::new()
        .route("/api/host-relay", get(attached))
        .route("/api/host-relay/{host_id}/stream", get(stream))
        .route("/api/host-relay/{host_id}/status", get(status))
        .route("/api/host-relay/{host_id}/challenge", post(challenge))
        .route(
            "/api/host-relay/{host_id}/request",
            post(request).layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES)),
        )
        .route(
            "/api/host-relay/{host_id}/reply",
            post(reply).layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES)),
        )
}
