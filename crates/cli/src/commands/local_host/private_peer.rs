//! The sending end of a private channel to ANOTHER host, for agent operations
//! (`agent.message`, `agent.status`). Same handshake, wire types and checks as
//! the browser end (`conversations_core::private_channel`), with this host's
//! agent key as the signer:
//!
//! 1. `peer_challenge`: a fresh nonce, answered with a challenge signed by the
//!    recipient host. Checked against the host key PINNED locally by
//!    `credential pair-peer`, never a key central supplies.
//! 2. `peer_offer`: a signed `peer.offer` binding that challenge, the SHA-256 of
//!    our SDP and our DTLS fingerprint. The signed `peer_answer` must bind our
//!    exact offer bytes, its own SDP and its DTLS fingerprint; that fingerprint
//!    is pinned before the answer is applied and checked again on connect.
//! 3. One signed operation, the EXACT stored bytes, on the channel. Only a
//!    `channel_reply` signed by the pinned key for this channel, key and request
//!    is believed. Then the channel closes.
//!
//! Central carries only signaling. Everything here is bounded in time; sockets,
//! readers and any TURN allocation end with the exchange, on every path.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use conversations_core::host_relay::{
    RelayReply, RelayRequest, Signed, HOST_DOMAIN, MAX_REPLY_BYTES, MAX_SDP_BYTES,
};
use conversations_core::private_channel::{
    sdp_fingerprint, ChannelReply, ChannelRequest, ReplyStatement, CHANNEL_LABEL, MAX_REPLY_MESSAGE,
};
use ring::{digest, signature};
use serde::Deserialize;
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use webrtc_util::Conn;

use super::agent_link::{self, Prepared};
use super::settings::{TransportSettings, TurnServer};

/// Getting a TURN allocation.
const ALLOCATE_DEADLINE: Duration = Duration::from_secs(10);
/// From the applied answer to an open data channel.
const CONNECT_DEADLINE: Duration = Duration::from_secs(15);
/// From the sent operation to its signed reply.
const REPLY_DEADLINE: Duration = Duration::from_secs(20);
/// The whole exchange, signaling included.
const EXCHANGE_DEADLINE: Duration = Duration::from_secs(60);
/// Most host candidates offered (one per route to the recipient's candidates).
const MAX_HOST_CANDIDATES: usize = 4;
/// Base64url (unpadded) of the largest signed payload a reply may carry.
const MAX_SIGNED_PAYLOAD_B64: usize = MAX_REPLY_MESSAGE * 4 / 3;
/// Base64url (unpadded) of a 64-byte Ed25519 signature.
const ED25519_SIG_B64: usize = 86;
/// The one request on a channel.
const REQUEST_ID: u32 = 1;

/// Where signaling goes: the relay this client already talks to.
pub struct Relay<'a> {
    pub http: &'a reqwest::Client,
    pub origin: &'a reqwest::Url,
    pub cookie: Option<&'a str>,
}

/// Send one ALREADY SIGNED, ALREADY STORED operation to its pinned host on a
/// fresh private channel and return the host's signed `(status, detail)` for
/// exactly it. Any failure is an error: the caller treats it as "no signed
/// answer", never as an outcome.
pub async fn exchange(
    relay: &Relay<'_>,
    dir: &Path,
    prepared: &Prepared,
    transport: &TransportSettings,
) -> Result<(String, Option<String>)> {
    let (turn, relay_only) = transport.native_ice()?;
    let mut owned = Owned::default();
    let result = tokio::time::timeout(
        EXCHANGE_DEADLINE,
        run(relay, dir, prepared, &turn, relay_only, &mut owned),
    )
    .await;
    owned.close().await;
    match result {
        Ok(outcome) => outcome,
        Err(_) => bail!("the private connection to the recipient host timed out"),
    }
}

/// Closing what an exchange started.
const CLEANUP_DEADLINE: Duration = Duration::from_secs(3);

/// Everything an exchange started that must not outlive it: UDP readers, TURN
/// clients (registered BEFORE `listen()` spawns their read loop) and the relay
/// allocation (whose refresh timers stop when it closes). `turn` 0.17 has no
/// `Drop` for either, so they are closed explicitly: bounded on every normal
/// path, and in the background when the exchange is cancelled.
#[derive(Default)]
struct Owned {
    readers: Vec<JoinHandle<()>>,
    clients: Vec<turn::client::Client>,
    relay: Option<Arc<dyn Conn + Send + Sync>>,
}

impl Owned {
    async fn close(&mut self) {
        let readers: Vec<_> = self.readers.drain(..).collect();
        for reader in &readers {
            reader.abort();
        }
        let closing = shut(self.relay.take(), std::mem::take(&mut self.clients));
        let _ = tokio::time::timeout(CLEANUP_DEADLINE, async {
            closing.await;
            for reader in readers {
                let _ = reader.await;
            }
        })
        .await;
    }
}

async fn shut(relay: Option<Arc<dyn Conn + Send + Sync>>, clients: Vec<turn::client::Client>) {
    if let Some(relay) = relay {
        let _ = relay.close().await;
    }
    for client in clients {
        let _ = client.close().await;
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        for reader in self.readers.drain(..) {
            reader.abort();
        }
        let (relay, clients) = (self.relay.take(), std::mem::take(&mut self.clients));
        if relay.is_some() || !clients.is_empty() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(tokio::time::timeout(CLEANUP_DEADLINE, shut(relay, clients)));
            }
        }
    }
}

async fn run(
    relay: &Relay<'_>,
    dir: &Path,
    prepared: &Prepared,
    turn: &[TurnServer],
    relay_only: bool,
    owned: &mut Owned,
) -> Result<(String, Option<String>)> {
    static CRYPTO: std::sync::Once = std::sync::Once::new();
    CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());

    // 1. A challenge from the pinned host.
    let nonce = random_hex()?;
    let challenge = match signal(
        relay,
        &prepared.host_id,
        &RelayRequest::PeerChallenge {
            nonce: nonce.clone(),
        },
    )
    .await?
    {
        RelayReply::PeerChallenge { statement } => verify_challenge(prepared, &statement, &nonce)?,
        other => bail!(
            "the recipient host refused a private connection{}",
            refusal(&other)
        ),
    };

    // 2. Our end, with a relayed candidate when TURN is configured.
    let mut rtc = Rtc::builder()
        .set_sctp_max_message_size(MAX_REPLY_MESSAGE as u32)
        .build(Instant::now());
    let (packets_tx, mut packets) = mpsc::channel::<(SocketAddr, SocketAddr, Vec<u8>)>(256);
    let mut io = Io::default();
    // With policy `all` a TURN server that grants nothing still leaves the
    // direct path; with `relay` it is the only path, so it must work.
    if !turn.is_empty() {
        match allocate(turn, owned).await {
            Ok((conn, relayed, local)) => {
                rtc.add_local_candidate(
                    Candidate::relayed(relayed, local, "udp").context("relayed candidate")?,
                );
                owned
                    .readers
                    .push(read_relay(conn.clone(), relayed, packets_tx.clone()));
                io.relay = Some((relayed, conn));
            }
            Err(error) if relay_only => return Err(error),
            Err(_) => {}
        }
    }
    let mut api = rtc.sdp_api();
    let channel = api.add_channel(CHANNEL_LABEL.into());
    let (offer, pending) = api.apply().context("no offer could be made")?;
    let sdp = offer.to_sdp_string();
    ensure!(
        sdp.len() <= MAX_SDP_BYTES,
        "the connection offer is too large"
    );
    let fingerprint = rtc.direct_api().local_dtls_fingerprint().to_string();
    ensure!(
        sdp_fingerprint(&sdp).as_deref() == Some(fingerprint.as_str()),
        "the offer does not carry the local fingerprint"
    );

    // 3. The signed offer, and the signed answer for exactly it.
    let (kid, payload, sig) = agent_link::sign_peer_offer(
        dir,
        &prepared.host_id,
        serde_json::json!({
            "challenge": challenge,
            "sdp_sha256": sha256_b64(sdp.as_bytes()),
            "fingerprint": fingerprint,
        }),
    )?;
    ensure!(
        kid == prepared.kid,
        "the agent key changed since this message was signed"
    );
    let offer_request = RelayRequest::PeerOffer {
        p: B64.encode(&payload),
        s: B64.encode(&sig),
        k: kid,
        sdp,
    };
    let (answer, remote_fingerprint) =
        match signal(relay, &prepared.host_id, &offer_request).await? {
            RelayReply::PeerAnswer { sdp, statement } => {
                let fingerprint = verify_answer(prepared, &statement, &challenge, &payload, &sdp)?;
                (sdp, fingerprint)
            }
            other => bail!(
                "the recipient host refused the private connection{}",
                refusal(&other)
            ),
        };
    rtc.direct_api().set_remote_fingerprint(
        remote_fingerprint
            .parse()
            .map_err(|_| anyhow::anyhow!("unreadable signed fingerprint"))?,
    );
    accept(&mut rtc, pending, &answer)?;

    // 4. Direct candidates: one per route toward the recipient's addresses.
    if !relay_only {
        for local in routes_to(&answer) {
            let socket = Arc::new(UdpSocket::bind(SocketAddr::new(local, 0)).await?);
            let address = socket.local_addr()?;
            rtc.add_local_candidate(Candidate::host(address, "udp").context("host candidate")?);
            owned
                .readers
                .push(read_socket(socket.clone(), packets_tx.clone()));
            io.sockets.push((address, socket));
        }
    }
    ensure!(
        !io.sockets.is_empty() || io.relay.is_some(),
        "no route to the recipient host and no TURN server configured"
    );
    drop(packets_tx);

    // 5. Connect, send the stored operation, take only its signed reply.
    let request = serde_json::to_vec(&ChannelRequest::Op {
        id: REQUEST_ID,
        p: B64.encode(&prepared.payload),
        s: B64.encode(&prepared.sig),
        k: prepared.kid.clone(),
    })?;
    let start = Instant::now();
    let mut sent_at: Option<Instant> = None;
    let mut connected = false;
    let outcome = loop {
        let timeout = match drive(&mut rtc, &mut io).await {
            Ok(Drive { timeout, events }) => {
                let mut done = None;
                for event in events {
                    match event {
                        Event::Connected => {
                            let seen = rtc
                                .direct_api()
                                .remote_dtls_fingerprint()
                                .map(|f| f.to_string());
                            ensure!(
                                seen.as_deref() == Some(remote_fingerprint.as_str()),
                                "the recipient's certificate is not the signed one"
                            );
                            connected = true;
                        }
                        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                            bail!("the private connection dropped")
                        }
                        Event::ChannelOpen(id, label) => {
                            ensure!(
                                connected && id == channel && label == CHANNEL_LABEL,
                                "unexpected data channel"
                            );
                            let mut open = rtc.channel(channel).context("channel vanished")?;
                            open.write(false, &request)
                                .context("could not send the operation")?;
                            sent_at = Some(Instant::now());
                        }
                        Event::ChannelClose(_) => bail!("the recipient host closed the channel"),
                        Event::ChannelData(data) => {
                            ensure!(
                                sent_at.is_some()
                                    && data.id == channel
                                    && !data.binary
                                    && data.data.len() <= MAX_REPLY_MESSAGE,
                                "unexpected data on the private channel"
                            );
                            done = Some(read_reply(prepared, &challenge, &data.data)?);
                        }
                        _ => {}
                    }
                }
                if let Some(done) = done {
                    break done;
                }
                timeout
            }
            Err(error) => return Err(error),
        };
        let now = Instant::now();
        match sent_at {
            None => ensure!(
                now - start < CONNECT_DEADLINE,
                "the private connection did not open in time"
            ),
            Some(sent) => ensure!(
                now - sent < REPLY_DEADLINE,
                "no reply on the private connection in time"
            ),
        }
        let wait = timeout
            .saturating_duration_since(now)
            .min(Duration::from_millis(100));
        tokio::select! {
            Some((local, source, data)) = packets.recv() => {
                if is_dtls(&data) {
                    io.dtls_in = Some((local, source));
                }
                let Ok(contents) = data.as_slice().try_into() else { continue };
                rtc.handle_input(Input::Receive(now, Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    contents,
                }))
                .context("private connection input")?;
            }
            _ = tokio::time::sleep(wait) => {
                rtc.handle_input(Input::Timeout(Instant::now())).context("private connection timer")?;
            }
        }
    };
    test_hooks::evidence(&io);
    rtc.direct_api().close_data_channel(channel);
    let _ = drive(&mut rtc, &mut io).await;
    rtc.disconnect();
    test_hooks::drop_reply()?;
    Ok(outcome)
}

#[derive(Default)]
struct Io {
    sockets: Vec<(SocketAddr, Arc<UdpSocket>)>,
    relay: Option<(SocketAddr, Arc<dyn Conn + Send + Sync>)>,
    /// The last DTLS record each way (the channel itself, not ICE checks or
    /// consent probes): the pair the connection actually carried data on.
    dtls_out: Option<(SocketAddr, SocketAddr)>,
    dtls_in: Option<(SocketAddr, SocketAddr)>,
}

struct Drive {
    timeout: Instant,
    events: Vec<Event>,
}

/// Send everything str0m wants sent from the socket it chose, and collect its
/// events, until it asks to be woken later.
async fn drive(rtc: &mut Rtc, io: &mut Io) -> Result<Drive> {
    let mut events = Vec::new();
    loop {
        match rtc.poll_output().context("private connection error")? {
            Output::Timeout(timeout) => return Ok(Drive { timeout, events }),
            Output::Transmit(transmit) => {
                if is_dtls(&transmit.contents) {
                    io.dtls_out = Some((transmit.source, transmit.destination));
                }
                if let Some((relayed, conn)) = &io.relay {
                    if *relayed == transmit.source {
                        let _ = conn.send_to(&transmit.contents, transmit.destination).await;
                        continue;
                    }
                }
                if let Some((_, socket)) = io.sockets.iter().find(|(a, _)| *a == transmit.source) {
                    let _ = socket.try_send_to(&transmit.contents, transmit.destination);
                }
            }
            Output::Event(event) => events.push(event),
        }
    }
}

/// A DTLS record by its first byte (RFC 7983 demultiplexing: 20..=63).
fn is_dtls(datagram: &[u8]) -> bool {
    matches!(datagram.first(), Some(20..=63))
}

fn read_socket(
    socket: Arc<UdpSocket>,
    tx: mpsc::Sender<(SocketAddr, SocketAddr, Vec<u8>)>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(local) = socket.local_addr() else {
            return;
        };
        let mut buffer = vec![0u8; 2048];
        while let Ok((n, source)) = socket.recv_from(&mut buffer).await {
            // A full queue drops the datagram, as the network would.
            let _ = tx.try_send((local, source, buffer[..n].to_vec()));
        }
    })
}

fn read_relay(
    conn: Arc<dyn Conn + Send + Sync>,
    relayed: SocketAddr,
    tx: mpsc::Sender<(SocketAddr, SocketAddr, Vec<u8>)>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 2048];
        while let Ok((n, source)) = conn.recv_from(&mut buffer).await {
            let _ = tx.try_send((relayed, source, buffer[..n].to_vec()));
        }
    })
}

/// A TURN allocation on the first configured server that grants one. Every
/// client is in `owned` before it can spawn anything; a failed or timed-out
/// attempt is closed before the next.
async fn allocate(
    servers: &[TurnServer],
    owned: &mut Owned,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr, SocketAddr)> {
    let mut last = None;
    for server in servers {
        let (clients, slot) = (&mut owned.clients, &mut owned.relay);
        let attempt =
            tokio::time::timeout(ALLOCATE_DEADLINE, async {
                let target = tokio::net::lookup_host(&server.address)
                    .await?
                    .next()
                    .context("the TURN server name did not resolve")?;
                let wildcard = match target.ip() {
                    IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                };
                let bind = SocketAddr::new(route_to(target)?.unwrap_or(wildcard), 0);
                let socket = Arc::new(UdpSocket::bind(bind).await?);
                let local = socket.local_addr()?;
                let client = turn::client::Client::new(turn::client::ClientConfig {
                    stun_serv_addr: target.to_string(),
                    turn_serv_addr: target.to_string(),
                    username: server.username.clone(),
                    password: server.credential.expose().to_owned(),
                    realm: String::new(),
                    software: String::new(),
                    rto_in_ms: 0,
                    conn: socket,
                    vnet: None,
                })
                .await
                .map_err(|_| anyhow::anyhow!("the TURN client could not start"))?;
                clients.push(client.clone());
                // No server message in errors: it could echo what we sent.
                client
                    .listen()
                    .await
                    .map_err(|_| anyhow::anyhow!("the TURN client could not start"))?;
                let conn: Arc<dyn Conn + Send + Sync> =
                    Arc::new(client.allocate().await.map_err(|_| {
                        anyhow::anyhow!("the TURN server did not grant an allocation")
                    })?);
                *slot = Some(conn.clone());
                let relayed = conn.local_addr().context("the allocation has no address")?;
                anyhow::Ok((conn, relayed, local))
            })
            .await;
        match attempt {
            Ok(Ok(allocated)) => return Ok(allocated),
            Ok(Err(error)) => last = Some(error),
            Err(_) => last = Some(anyhow::anyhow!("the TURN server did not answer in time")),
        }
        // This server's client (and any allocation) ends before the next try.
        let _ = tokio::time::timeout(
            CLEANUP_DEADLINE,
            shut(owned.relay.take(), std::mem::take(&mut owned.clients)),
        )
        .await;
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no TURN server configured")))
}

/// The local address the OS would use to reach `target` (no packet is sent).
fn route_to(target: SocketAddr) -> Result<Option<IpAddr>> {
    let wildcard = match target {
        SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let probe = std::net::UdpSocket::bind(wildcard)?;
    Ok(probe
        .connect(target)
        .ok()
        .and_then(|_| probe.local_addr().ok())
        .map(|a| a.ip())
        .filter(|ip| !ip.is_unspecified()))
}

/// Distinct local addresses that route to the answer's host candidates.
fn routes_to(answer: &str) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for line in answer.lines() {
        let Some(candidate) = line.trim_end_matches('\r').strip_prefix("a=candidate:") else {
            continue;
        };
        let fields: Vec<&str> = candidate.split_whitespace().collect();
        let (Some(ip), Some(port)) = (fields.get(4), fields.get(5)) else {
            continue;
        };
        let (Ok(ip), Ok(port)) = (ip.parse::<IpAddr>(), port.parse::<u16>()) else {
            continue;
        };
        if let Ok(Some(local)) = route_to(SocketAddr::new(ip, port)) {
            if !out.contains(&local) && out.len() < MAX_HOST_CANDIDATES {
                out.push(local);
            }
        }
    }
    out
}

fn accept(rtc: &mut Rtc, pending: SdpPendingOffer, answer: &str) -> Result<()> {
    let answer = SdpAnswer::from_sdp_string(answer).context("unreadable answer")?;
    rtc.sdp_api()
        .accept_answer(pending, answer)
        .context("the answer could not be applied")
}

async fn signal(relay: &Relay<'_>, host_id: &str, request: &RelayRequest) -> Result<RelayReply> {
    let mut url = relay.origin.clone();
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid origin"))?
        .extend(["api", "host-relay", host_id, "request"]);
    let mut post = relay.http.post(url).json(request);
    if let Some(cookie) = relay.cookie {
        post = post.header(reqwest::header::COOKIE, cookie);
    }
    let mut response = post.send().await.context("the relay is unreachable")?;
    ensure!(
        response.status().is_success(),
        "the relay refused the request (HTTP {}); the recipient host may be offline",
        response.status().as_u16()
    );
    // Read at most what a host may answer, whatever the length header says.
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("the relay answer broke off")?
    {
        ensure!(
            body.len() + chunk.len() <= MAX_REPLY_BYTES,
            "the relay answer is too large"
        );
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("unreadable relay answer")
}

fn refusal(reply: &RelayReply) -> String {
    match reply {
        RelayReply::Error { message } => {
            format!(": {}", conversations_core::host_relay::clean_text(message))
        }
        _ => String::new(),
    }
}

/// The statement's payload, if the PINNED recipient host key signed it.
fn signed_by_pinned(prepared: &Prepared, statement: &Signed) -> Result<Vec<u8>> {
    ensure!(
        statement.p.len() <= MAX_SIGNED_PAYLOAD_B64 && statement.s.len() == ED25519_SIG_B64,
        "malformed signed statement"
    );
    let payload = B64
        .decode(&statement.p)
        .context("statement is not base64url")?;
    let sig = B64
        .decode(&statement.s)
        .context("signature is not base64url")?;
    let mut message = HOST_DOMAIN.to_vec();
    message.extend_from_slice(&payload);
    signature::UnparsedPublicKey::new(&signature::ED25519, &prepared.host_key)
        .verify(&message, &sig)
        .map_err(|_| anyhow::anyhow!("not signed by the paired recipient host"))?;
    Ok(payload)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChallengeStatement {
    v: u8,
    typ: String,
    host: String,
    nonce: String,
    challenge: String,
    exp: i64,
}

fn verify_challenge(prepared: &Prepared, statement: &Signed, nonce: &str) -> Result<String> {
    let payload = signed_by_pinned(prepared, statement)?;
    let value: ChallengeStatement =
        serde_json::from_slice(&payload).context("unexpected challenge statement")?;
    ensure!(
        value.v == 1
            && value.typ == "peer_challenge"
            && value.host == prepared.host_id
            && value.nonce == nonce
            && value.challenge.len() == 32
            && value
                .challenge
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && value.exp >= super::authz::store::now_secs(),
        "the challenge is not for this request"
    );
    Ok(value.challenge)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerStatement {
    v: u8,
    typ: String,
    host: String,
    kid: String,
    dlg: Option<String>,
    challenge: String,
    offer: String,
    answer: String,
    fingerprint: String,
}

/// Returns the recipient's signed DTLS fingerprint.
fn verify_answer(
    prepared: &Prepared,
    statement: &Signed,
    challenge: &str,
    offer_payload: &[u8],
    sdp: &str,
) -> Result<String> {
    ensure!(sdp.len() <= MAX_SDP_BYTES, "the answer is too large");
    let payload = signed_by_pinned(prepared, statement)?;
    let value: AnswerStatement =
        serde_json::from_slice(&payload).context("unexpected answer statement")?;
    ensure!(
        value.v == 1
            && value.typ == "peer_answer"
            && value.host == prepared.host_id
            && value.kid == prepared.kid
            && value.dlg.is_none()
            && value.challenge == challenge
            && value.offer == sha256_b64(offer_payload),
        "the answer is not for this offer"
    );
    ensure!(
        value.answer == sha256_b64(sdp.as_bytes())
            && sdp_fingerprint(sdp).as_deref() == Some(value.fingerprint.as_str())
            && conversations_core::private_channel::valid_fingerprint(&value.fingerprint),
        "the answer does not match what the recipient host signed"
    );
    ensure!(
        sdp.lines()
            .all(|line| !line.starts_with("m=") || line.starts_with("m=application ")),
        "only a data connection is accepted"
    );
    Ok(value.fingerprint)
}

/// The signed outcome of exactly our request on exactly this channel.
fn read_reply(
    prepared: &Prepared,
    challenge: &str,
    data: &[u8],
) -> Result<(String, Option<String>)> {
    let ChannelReply::Reply { id, statement } =
        serde_json::from_slice(data).context("unreadable reply")?
    else {
        bail!("unexpected message on the private channel");
    };
    ensure!(id == REQUEST_ID, "a reply for another request");
    let payload = signed_by_pinned(prepared, &statement)?;
    let value: ReplyStatement = serde_json::from_slice(&payload).context("unexpected reply")?;
    ensure!(
        value.v == 1
            && value.typ == "channel_reply"
            && value.host == prepared.host_id
            && value.peer == challenge
            && value.kid == prepared.kid
            && value.request == sha256_b64(&prepared.payload)
            && value.body.is_none(),
        "the reply is not for this request"
    );
    Ok((value.status, value.detail))
}

fn sha256_b64(bytes: &[u8]) -> String {
    B64.encode(digest::digest(&digest::SHA256, bytes))
}

fn random_hex() -> Result<String> {
    let mut bytes = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| anyhow::anyhow!("no randomness"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Debug-build test hooks. They observe or withhold; they never authorize,
/// carry a payload or turn a missing reply into an outcome.
mod test_hooks {
    use super::Io;

    /// `CONVERSATIONS_TEST_PEER_EVIDENCE=<file>`: one JSON line per channel with
    /// the addresses its last datagrams actually used each way.
    pub(super) fn evidence(io: &Io) {
        #[cfg(debug_assertions)]
        if let Some(path) = std::env::var_os("CONVERSATIONS_TEST_PEER_EVIDENCE") {
            use std::io::Write;
            let relayed = io.relay.as_ref().map(|(a, _)| *a);
            let kind = |local: std::net::SocketAddr| {
                if Some(local) == relayed {
                    "relay"
                } else {
                    "host"
                }
            };
            let line = serde_json::json!({
                "local_kind": io.dtls_out.map(|(local, _)| kind(local)),
                "inbound_kind": io.dtls_in.map(|(local, _)| kind(local)),
                "local": io.dtls_out.map(|(local, _)| local.to_string()),
                "remote": io.dtls_out.map(|(_, remote)| remote.to_string()),
                "inbound_from": io.dtls_in.map(|(_, source)| source.to_string()),
            });
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
        #[cfg(not(debug_assertions))]
        let _ = io;
    }

    /// `CONVERSATIONS_TEST_PEER_DROP_REPLY=<dir>`: while `<dir>/drop` exists,
    /// discard one verified reply (the host has acted; we never learn how).
    pub(super) fn drop_reply() -> anyhow::Result<()> {
        #[cfg(debug_assertions)]
        if let Some(dir) = std::env::var_os("CONVERSATIONS_TEST_PEER_DROP_REPLY") {
            let marker = std::path::Path::new(&dir).join("drop");
            if std::fs::remove_file(&marker).is_ok() {
                anyhow::bail!("the reply was lost");
            }
        }
        Ok(())
    }
}

/// The real application-reply verifier against genuinely host-signed channel
/// replies. These are verifier component proofs: what a hostile central or a
/// stale channel could present after decryption, not reads of encrypted wire
/// bytes (which a relay cannot make or alter).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::local_host::authz::{attest, store::Store};
    use conversations_core::private_channel::ReplyBody;

    const KID: &str = "agent-key-1";
    const CHALLENGE: &str = "0123456789abcdef0123456789abcdef";
    const OLD_CHALLENGE: &str = "fedcba9876543210fedcba9876543210";

    struct HostKey {
        _dir: tempfile::TempDir,
        store: Store,
    }

    impl HostKey {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let store = Store::open(dir.path()).unwrap();
            Self { _dir: dir, store }
        }

        /// A `channel_reply` signed by this host, exactly as the host signs it.
        fn sign(&mut self, statement: &ReplyStatement) -> Signed {
            attest::sign_value(&mut self.store, statement).unwrap()
        }
    }

    fn prepared(host: &mut HostKey, payload: &[u8]) -> Prepared {
        Prepared {
            message_id: "m1".into(),
            host_id: host.store.host_id().unwrap(),
            host_key: host.store.host_public_key().unwrap(),
            kid: KID.into(),
            payload: payload.to_vec(),
            sig: vec![0; 64],
        }
    }

    fn statement(
        prepared: &Prepared,
        peer: &str,
        request: &[u8],
        status: &str,
        detail: &str,
    ) -> ReplyStatement {
        ReplyStatement {
            v: 1,
            typ: "channel_reply".into(),
            host: prepared.host_id.clone(),
            peer: peer.into(),
            kid: prepared.kid.clone(),
            request: sha256_b64(request),
            status: status.into(),
            detail: Some(detail.into()),
            body: None,
        }
    }

    fn reply(statement: Signed) -> Vec<u8> {
        serde_json::to_vec(&ChannelReply::Reply {
            id: REQUEST_ID,
            statement,
        })
        .unwrap()
    }

    fn rejected(prepared: &Prepared, challenge: &str, data: &[u8], what: &str) -> anyhow::Error {
        match read_reply(prepared, challenge, data) {
            Ok(answer) => panic!("{what} was accepted: {answer:?}"),
            Err(error) => error,
        }
    }

    #[test]
    fn a_reply_signed_by_the_pinned_host_for_exactly_this_request_and_channel_is_read() {
        let mut host = HostKey::new();
        let ours = prepared(&mut host, b"status of message 1");
        let signed = host.sign(&statement(
            &ours,
            CHALLENGE,
            &ours.payload,
            "executed",
            "acknowledged",
        ));
        assert_eq!(
            read_reply(&ours, CHALLENGE, &reply(signed)).unwrap(),
            ("executed".to_owned(), Some("acknowledged".to_owned()))
        );
    }

    #[test]
    fn a_genuine_reply_cannot_be_replayed_swapped_forged_or_altered() {
        let mut host = HostKey::new();
        let ours = prepared(&mut host, b"status of message 1");
        let other = prepared(&mut host, b"status of message 2");

        // Replay: a valid reply from an earlier channel never satisfies this one.
        let old = reply(host.sign(&statement(
            &ours,
            OLD_CHALLENGE,
            &ours.payload,
            "executed",
            "queued",
        )));
        assert!(
            read_reply(&ours, OLD_CHALLENGE, &old).is_ok(),
            "control: valid on its own channel"
        );
        rejected(&ours, CHALLENGE, &old, "an old channel's reply");

        // Swap: a genuine reply about another request (another message), on
        // this very channel, including a correctly signed refusal.
        for (status, detail) in [("executed", "acknowledged"), ("refused", "not granted")] {
            let swapped =
                reply(host.sign(&statement(&ours, CHALLENGE, &other.payload, status, detail)));
            assert!(
                read_reply(&other, CHALLENGE, &swapped).is_ok(),
                "control: valid for its own request"
            );
            rejected(&ours, CHALLENGE, &swapped, "another request's reply");
        }

        // Forged: a correctly formed statement signed by another host's key.
        let mut rogue = HostKey::new();
        let forged = reply(rogue.sign(&statement(
            &ours,
            CHALLENGE,
            &ours.payload,
            "executed",
            "acknowledged",
        )));
        let error = rejected(&ours, CHALLENGE, &forged, "a rogue host's reply");
        assert!(
            format!("{error:#}").contains("not signed by the paired recipient host"),
            "{error:#}"
        );

        // Altered: the signed bytes changed under the original signature.
        let genuine = host.sign(&statement(
            &ours,
            CHALLENGE,
            &ours.payload,
            "executed",
            "queued",
        ));
        let bytes = B64.decode(&genuine.p).unwrap();
        let altered = String::from_utf8(bytes.clone())
            .unwrap()
            .replace("queued", "acknowledged");
        assert_ne!(altered.as_bytes(), bytes.as_slice());
        let tampered = reply(Signed {
            p: B64.encode(altered),
            s: genuine.s.clone(),
        });
        let error = rejected(&ours, CHALLENGE, &tampered, "altered signed bytes");
        assert!(
            format!("{error:#}").contains("not signed by the paired recipient host"),
            "{error:#}"
        );
        // A reply for another request id on the channel.
        let wrong_id = serde_json::to_vec(&ChannelReply::Reply {
            id: REQUEST_ID + 1,
            statement: genuine,
        })
        .unwrap();
        rejected(&ours, CHALLENGE, &wrong_id, "another request id");
    }

    #[test]
    fn every_binding_of_a_signed_reply_is_checked() {
        let mut host = HostKey::new();
        let ours = prepared(&mut host, b"status of message 1");
        let base = statement(&ours, CHALLENGE, &ours.payload, "executed", "acknowledged");
        type Mutation = fn(&mut ReplyStatement);
        let cases: [(&str, Mutation); 6] = [
            ("version", |s| s.v = 2),
            ("type", |s| s.typ = "challenge".into()),
            ("host", |s| s.host = "another-host".into()),
            ("kid", |s| s.kid = "another-key".into()),
            ("request", |s| s.request = sha256_b64(b"other bytes")),
            ("body", |s| {
                s.body = Some(ReplyBody::History {
                    items: vec![],
                    next: None,
                })
            }),
        ];
        for (what, change) in cases {
            let mut changed = base.clone();
            change(&mut changed);
            // Genuinely signed by the pinned host, yet not for this request.
            let error = rejected(&ours, CHALLENGE, &reply(host.sign(&changed)), what);
            assert!(
                format!("{error:#}").contains("the reply is not for this request"),
                "{what}: {error:#}"
            );
        }
    }

    #[test]
    fn a_rejected_reply_never_moves_the_senders_known_state() {
        let mut host = HostKey::new();
        let ours = prepared(&mut host, b"status of message 1");
        let other = prepared(&mut host, b"status of message 2");
        let row = SendRowFixture::received(&ours);
        let dir = tempfile::tempdir().unwrap();
        // A genuine "acknowledged" answer about ANOTHER request, presented on
        // this channel: the real verifier's rejection goes to the real
        // after_query, which keeps the last vouched state and reports why.
        let swapped = reply(host.sign(&statement(
            &ours,
            CHALLENGE,
            &other.payload,
            "executed",
            "acknowledged",
        )));
        let answer = read_reply(&ours, CHALLENGE, &swapped);
        assert!(answer.is_err());
        let status = agent_link::after_query(dir.path(), row.clone(), answer, 2_000).unwrap();
        assert_eq!(
            (status.state.as_str(), status.checked_at),
            ("received", Some(1_000))
        );
        assert!(status
            .unavailable
            .is_some_and(|reason| reason.contains("not for this request")));
    }

    struct SendRowFixture;

    impl SendRowFixture {
        /// The sender's record of `prepared`: last vouched `received` at t=1000.
        fn received(prepared: &Prepared) -> crate::commands::local_host::authz::store::SendRow {
            crate::commands::local_host::authz::store::SendRow {
                source: "source".into(),
                target: "target".into(),
                idem: "k1".into(),
                request_sha256: vec![],
                attempt: 1,
                message_id: prepared.message_id.clone(),
                payload: prepared.payload.clone(),
                sig: prepared.sig.clone(),
                kid: prepared.kid.clone(),
                host_id: prepared.host_id.clone(),
                exp: 0,
                state: "received".into(),
                detail: None,
                checked_at: Some(1_000),
            }
        }
    }
}
