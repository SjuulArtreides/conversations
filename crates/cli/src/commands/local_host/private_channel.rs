//! The host's end of the private browser channel: a WebRTC data channel
//! (DTLS + SCTP, `str0m`, sans-IO) whose signaling crosses the opaque relay.
//! Wire types and the handshake: `conversations_core::private_channel`.
//!
//! The channel is transport, not authority. An offer is accepted only from a
//! currently approved key (or live delegation) with a fresh one-use challenge,
//! and the browser's DTLS fingerprint it signed is pinned before the SDP is
//! applied. Every message on the channel is a signed operation, signed by the
//! same key and delegation as the offer, and verified and authorized against
//! current local state exactly like any other; reads of local history are
//! answered here and nowhere else. Any bound violation closes the channel.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use conversations_core::host_relay::{clean_text, OpStatus, RelayReply, MAX_SDP_BYTES};
use conversations_core::private_channel::{
    sdp_fingerprint, ChannelReply, ChannelRequest, ReplyBody, ReplyStatement, SearchHit,
    SessionEntry, TerminalEnd, TerminalFrame, TerminalFrameKind, TransferStatus, CHANNEL_LABEL,
    MAX_LIST_TITLE, MAX_REPLY_PAYLOAD, MAX_REQUEST_MESSAGE,
};
use conversations_core::terminal::PtyFrameKind;
use conversations_core::terminal_files::TerminalFileStatus;
use ring::digest;
use serde::Serialize;
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::{execute_signed, replay_outcome, Host};
use crate::commands::local_host::authz::{
    self,
    dispatch::Decision,
    envelope::{
        claimed_delegation, claimed_operation, private_only, Operation, PeerOffer, Verified,
    },
    store::Store,
    SignedFrame,
};
use crate::commands::local_host::history::History;
use crate::commands::local_host::protocol::{Frame, Screen, SharedScreen};
use crate::commands::local_host::settings::Settings;

/// Channels open at once on this host.
const MAX_PEERS: usize = 4;
/// Most candidate addresses one answer offers.
const MAX_CANDIDATES: usize = 4;
const CONNECT_DEADLINE: Duration = Duration::from_secs(15);
const IDLE_LIMIT: Duration = Duration::from_secs(5 * 60);
const PEER_LIFETIME: Duration = Duration::from_secs(60 * 60);
/// Requests waiting behind the one being handled; more closes the channel.
const MAX_QUEUED: usize = 4;
/// Unsent reply bytes the channel may hold before it is closed as stuck.
const MAX_BUFFERED: usize = 256 * 1024;
/// Room for the statement envelope around a history page.
const PAGE_BYTES: usize = MAX_REPLY_PAYLOAD - 4 * 1024;

static PEERS: AtomicUsize = AtomicUsize::new(0);

/// Holds one of the `MAX_PEERS` slots until dropped.
struct Slot;

impl Slot {
    fn take() -> Option<Self> {
        PEERS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_PEERS).then_some(n + 1)
            })
            .ok()
            .map(|_| Slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        PEERS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn refused(message: &str) -> RelayReply {
    RelayReply::Error {
        message: message.into(),
    }
}

fn sha256_b64(bytes: &[u8]) -> String {
    B64.encode(digest::digest(&digest::SHA256, bytes))
}

#[derive(Serialize)]
struct ChallengeStatement<'a> {
    v: u8,
    typ: &'static str,
    host: &'a str,
    nonce: &'a str,
    challenge: &'a str,
    exp: i64,
}

/// `peer_challenge`: a fresh one-use challenge, stored durably, signed back with
/// the browser's nonce.
pub(super) async fn challenge(host: &Host, nonce: String) -> RelayReply {
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return refused("malformed private connection request");
    }
    let dir = host.dir.clone();
    let issued = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<_>> {
        let mut store = Store::open(&dir)?;
        let host_id = store.host_id()?;
        let Some((challenge, exp)) = store.issue_peer_challenge(authz::store::now_secs())? else {
            return Ok(None);
        };
        Ok(Some(authz::attest::sign_value(
            &mut store,
            &ChallengeStatement {
                v: 1,
                typ: "peer_challenge",
                host: &host_id,
                nonce: &nonce,
                challenge: &challenge,
                exp,
            },
        )?))
    })
    .await;
    match issued {
        Ok(Ok(Some(statement))) => RelayReply::PeerChallenge { statement },
        Ok(Ok(None)) => refused("too many private connection requests are open"),
        _ => refused("the host could not issue a challenge"),
    }
}

#[derive(Serialize)]
struct AnswerStatement<'a> {
    v: u8,
    typ: &'static str,
    host: &'a str,
    kid: &'a str,
    dlg: Option<&'a str>,
    challenge: &'a str,
    offer: String,
    answer: String,
    fingerprint: &'a str,
}

/// Who may use one channel: exactly the key and delegation that signed its offer.
struct Binding {
    kid: String,
    dlg: Option<String>,
    /// The consumed challenge; names the channel in every reply.
    peer: String,
    /// The browser DTLS fingerprint the offer signed.
    fingerprint: String,
}

/// `peer_offer`: verify and admit the signed offer, answer it, and run the peer.
pub(super) async fn offer(host: &Arc<Host>, p: &str, s: &str, k: &str, sdp: String) -> RelayReply {
    if sdp.len() > MAX_SDP_BYTES {
        return refused("the connection offer is too large");
    }
    let Some(frame) = SignedFrame::from_wire(p, s, k) else {
        return refused("malformed private connection offer");
    };
    let dir = host.dir.clone();
    let admitted = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let now = authz::store::now_secs();
        let mut store = Store::open(&dir)?;
        let host_id = store.host_id()?;
        let Some(dlg) = claimed_delegation(&frame.payload) else {
            return Ok(Err("malformed private connection offer".to_owned()));
        };
        let Some((alg, key)) = store.signing_key(&frame.kid, dlg.as_deref(), now)? else {
            return Ok(Err(
                "unknown, revoked or expired credential or delegation".to_owned()
            ));
        };
        let offer = match authz::envelope::verify_peer_offer(
            &frame,
            alg,
            &key,
            &host_id,
            now,
            dlg.as_deref(),
        ) {
            Ok(offer) => offer,
            Err(reject) => return Ok(Err(reject.to_string())),
        };
        if let Some(reason) = store.admit_peer_offer(&offer, now)? {
            return Ok(Err(reason.to_owned()));
        }
        let settings = Settings::load(&dir).map_err(|_| anyhow::anyhow!("settings"))?;
        Ok(Ok((offer, host_id, settings.transport.candidate_ips)))
    })
    .await;
    let (offer, host_id, configured) = match admitted {
        Ok(Ok(Ok(admitted))) => admitted,
        Ok(Ok(Err(reason))) => return refused(&clean_text(&reason)),
        _ => return refused("the host could not check the offer"),
    };
    if sha256_b64(sdp.as_bytes()) != offer.sdp_sha256
        || sdp_fingerprint(&sdp).as_deref() != Some(offer.fingerprint.as_str())
    {
        return refused("the offer does not match what was signed");
    }
    // A data channel only: no audio or video is ever negotiated.
    if sdp
        .lines()
        .any(|line| line.starts_with("m=") && !line.starts_with("m=application "))
    {
        return refused("only a data connection is accepted");
    }
    let Some(slot) = Slot::take() else {
        return refused("too many private connections are open");
    };
    let (rtc, sockets, answer, local) = match build_peer(&sdp, &offer, &configured) {
        Ok(built) => built,
        Err(error) => {
            eprintln!("Private connection not set up: {error:#}");
            return refused("the host could not set up the connection");
        }
    };
    let dir = host.dir.clone();
    let statement = {
        let (offer, answer, local) = (offer.clone(), answer.clone(), local.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let mut store = Store::open(&dir)?;
            authz::attest::sign_value(
                &mut store,
                &AnswerStatement {
                    v: 1,
                    typ: "peer_answer",
                    host: &host_id,
                    kid: &offer.kid,
                    dlg: offer.dlg.as_deref(),
                    challenge: &offer.challenge,
                    offer: B64.encode(&offer.payload_sha256),
                    answer: sha256_b64(answer.as_bytes()),
                    fingerprint: &local,
                },
            )
        })
        .await
    };
    let Ok(Ok(statement)) = statement else {
        return refused("the host could not sign its answer");
    };
    let binding = Binding {
        kid: offer.kid,
        dlg: offer.dlg,
        peer: offer.challenge,
        fingerprint: offer.fingerprint,
    };
    let host = host.clone();
    tokio::spawn(async move {
        let reason = run(host, rtc, sockets, Arc::new(binding)).await;
        eprintln!("Private connection closed: {reason}");
        drop(slot);
    });
    RelayReply::PeerAnswer {
        sdp: answer,
        statement,
    }
}

type Sockets = Vec<(SocketAddr, Arc<UdpSocket>)>;

fn build_peer(
    sdp: &str,
    offer: &PeerOffer,
    configured: &[IpAddr],
) -> anyhow::Result<(Rtc, Sockets, String, String)> {
    static CRYPTO: std::sync::Once = std::sync::Once::new();
    CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
    let addresses: Vec<IpAddr> = if configured.is_empty() {
        interface_addresses()
    } else {
        configured.to_vec()
    };
    anyhow::ensure!(!addresses.is_empty(), "no candidate address");
    let parsed = str0m::change::SdpOffer::from_sdp_string(sdp)?;
    let mut rtc = Rtc::builder()
        .set_sctp_max_message_size(MAX_REQUEST_MESSAGE as u32)
        .set_sctp_max_buffered_amount(MAX_BUFFERED)
        .build(Instant::now());
    let mut sockets = Vec::new();
    for ip in addresses.into_iter().take(MAX_CANDIDATES) {
        let socket = std::net::UdpSocket::bind(SocketAddr::new(ip, 0))?;
        socket.set_nonblocking(true)?;
        let address = socket.local_addr()?;
        rtc.add_local_candidate(Candidate::host(address, "udp")?);
        sockets.push((address, Arc::new(UdpSocket::from_std(socket)?)));
    }
    // Pin the SIGNED fingerprint before the SDP is applied: str0m adopts the
    // SDP's `a=fingerprint` only when none is set, and checks the DTLS peer
    // certificate against it.
    rtc.direct_api()
        .set_remote_fingerprint(offer.fingerprint.parse().map_err(anyhow::Error::msg)?);
    let answer = rtc.sdp_api().accept_offer(parsed)?.to_sdp_string();
    let local = rtc.direct_api().local_dtls_fingerprint().to_string();
    anyhow::ensure!(
        sdp_fingerprint(&answer).as_deref() == Some(local.as_str()),
        "the answer does not carry the local fingerprint"
    );
    Ok((rtc, sockets, answer, local))
}

/// This machine's up, non-loopback IPv4 interface addresses.
#[cfg(unix)]
fn interface_addresses() -> Vec<IpAddr> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs/freeifaddrs pair; entries are read only while the list lives.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cursor = head;
        while !cursor.is_null() {
            let entry = &*cursor;
            let up = entry.ifa_flags & libc::IFF_UP as u32 != 0;
            let loopback = entry.ifa_flags & libc::IFF_LOOPBACK as u32 != 0;
            if up
                && !loopback
                && !entry.ifa_addr.is_null()
                && i32::from((*entry.ifa_addr).sa_family) == libc::AF_INET
            {
                let address = &*(entry.ifa_addr as *const libc::sockaddr_in);
                out.push(IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(
                    address.sin_addr.s_addr,
                ))));
            }
            cursor = entry.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    out
}

#[cfg(not(unix))]
fn interface_addresses() -> Vec<IpAddr> {
    Vec::new()
}

/// What arrived on the channel, in order.
enum Arrival {
    Op(u32, SignedFrame),
    Chunk {
        transfer: String,
        file: u32,
        offset: u64,
        data: Vec<u8>,
    },
    Cancel(String),
    Unsubscribe(u32),
}

/// The screen of `session`'s current terminal connection, held weakly: when
/// that connection ends, so does every subscription to it, and a reconnecting
/// terminal needs a fresh subscribe.
fn current_screen(host: &Host, session: &str) -> Option<std::sync::Weak<std::sync::Mutex<Screen>>> {
    host.sessions
        .lock()
        .unwrap()
        .get(session)
        .and_then(|s| s.screen.as_ref())
        .map(Arc::downgrade)
}

/// One terminal subscription of this channel.
struct Subscription {
    session: String,
    /// The signed subscribe (or its latest renewal) every frame is released under.
    admitted: Verified,
    seq: u64,
    task: tokio::task::JoinHandle<()>,
    /// Pieces that were drafted at `seq` but could not hand off to the data
    /// channel yet — `str0m::channel::Channel::write` returns `Ok(false)` when
    /// the SCTP send window cannot take the fragment, even with
    /// `buffered_amount()` still under `MAX_BUFFERED`. Each entry carries the
    /// admission it was drafted under, so authz revalidates at the actual
    /// hand-off, not at draft. The outer run loop drains this queue before
    /// taking any new frame on this subscription, so the browser's
    /// `Following.take` sees gapless `seq` continuity.
    pending: VecDeque<(String, Verified)>,
}

/// Withheld frames while the connection has this many unsent bytes.
const TERMINAL_BACKLOG: usize = MAX_BUFFERED / 2;

/// Include the next piece: terminal data leaves the upper half for replies.
fn room_for_terminal_piece(buffered: usize, next_piece: usize) -> bool {
    buffered + next_piece <= TERMINAL_BACKLOG
}

/// Per-subscription safety cap on the pending-piece backlog. The browser
/// accepts terminal snapshots up to the per-frame limit set by
/// `conversations_core::private_channel`; a snapshot of a 200×60 PTY with a
/// full scrollback can legitimately split into a few hundred pieces (each
/// carrying up to `MAX_TERMINAL_DATA` raw bytes, ~35 KiB serialized text),
/// and the SCTP sender needs multiple drain cycles to push a single such
/// frame through. The cap must sit well above a single valid frame or we
/// would reject legitimate large sessions with a spurious `Expired` — the
/// bug `MAX_PENDING_PIECES = 64` would reintroduce. 512 pieces caps per-
/// subscription memory near 18 MiB (serialized) — generous headroom above
/// the browser's single-frame budget, bounded for safety across the
/// `MAX_TERMINAL_SUBSCRIPTIONS` channels. A subscription whose backlog
/// would exceed this cap is ended with `TerminalEnd::Expired`: the channel
/// is genuinely wedged, not just momentarily back-pressured.
const MAX_PENDING_PIECES: usize = 512;

/// One peer, until it closes. Returns why it closed (no content).
async fn run(host: Arc<Host>, mut rtc: Rtc, sockets: Sockets, binding: Arc<Binding>) -> String {
    let start = Instant::now();
    let (packets_tx, mut packets) = mpsc::channel::<(SocketAddr, SocketAddr, Vec<u8>)>(256);
    let readers: Vec<_> = sockets
        .iter()
        .map(|(local, socket)| {
            let (local, socket, tx) = (*local, socket.clone(), packets_tx.clone());
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 2048];
                while let Ok((n, source)) = socket.recv_from(&mut buffer).await {
                    // A full queue drops the datagram, as the network would.
                    let _ = tx.try_send((local, source, buffer[..n].to_vec()));
                }
            })
        })
        .collect();
    // Opened before any reply is handed off: the final check must not run schema
    // setup while it holds the database write lock.
    let mut store = match Store::open(&host.dir) {
        Ok(store) => store,
        Err(_) => return "the host could not open its authority store".into(),
    };
    let (results_tx, mut results) = mpsc::channel::<Pending>(MAX_QUEUED + 1);
    let (followed_tx, mut followed) = mpsc::channel::<Followed>(8);
    let mut channel: Option<ChannelId> = None;
    let mut queue: VecDeque<(u32, SignedFrame)> = VecDeque::new();
    let mut arrived: Vec<Arrival> = Vec::new();
    let mut inbound = Inbound::default();
    let mut subs: std::collections::HashMap<u32, Subscription> = Default::default();
    let mut next_sub: u32 = 1;
    let mut busy = false;
    // Transfers handed to the terminal, until it reports what happened.
    let (outcomes_tx, mut outcomes) =
        mpsc::channel::<ChannelReply>(conversations_core::private_channel::MAX_INBOUND_TRANSFERS);
    let mut awaiting: Vec<(String, tokio::task::JoinHandle<()>)> = Vec::new();
    let mut active = Instant::now();
    let reason = loop {
        let timeout = match drain(
            &mut rtc,
            &sockets,
            &binding,
            &mut channel,
            &mut arrived,
            queue.len() + usize::from(busy),
        ) {
            Ok(timeout) => timeout,
            Err(reason) => break reason,
        };
        let mut notices: Vec<ChannelReply> = Vec::new();
        for arrival in arrived.drain(..) {
            active = Instant::now();
            match arrival {
                Arrival::Op(id, frame) => queue.push_back((id, frame)),
                Arrival::Chunk {
                    transfer,
                    file,
                    offset,
                    data,
                } => match inbound.feed(&transfer, file, offset, &data, Instant::now()) {
                    Some(Fed::More) => {}
                    Some(Fed::Complete(done)) => {
                        notices.extend(hand_off(
                            &host,
                            &mut store,
                            *done,
                            &outcomes_tx,
                            &mut awaiting,
                        ));
                    }
                    Some(Fed::Refused(reason)) => {
                        eprintln!("Private transfer refused: {reason}");
                        notices.push(ChannelReply::Transfer {
                            transfer,
                            status: TransferStatus::Refused,
                        });
                    }
                    None => notices.push(ChannelReply::Transfer {
                        transfer,
                        status: TransferStatus::Refused,
                    }),
                },
                Arrival::Cancel(transfer) => {
                    if inbound.cancel(&transfer) {
                        notices.push(ChannelReply::Transfer {
                            transfer,
                            status: TransferStatus::Cancelled,
                        });
                    }
                }
                Arrival::Unsubscribe(sub) => {
                    if let Some(gone) = subs.remove(&sub) {
                        gone.task.abort();
                        notices.push(ChannelReply::Terminal(end_piece(
                            sub,
                            gone.seq,
                            TerminalEnd::Unsubscribed,
                        )));
                    }
                }
            }
        }
        for transfer in inbound.expire(Instant::now()) {
            notices.push(ChannelReply::Transfer {
                transfer,
                status: TransferStatus::Refused,
            });
        }
        if !notices.is_empty() {
            let Some(mut open) = channel.and_then(|id| rtc.channel(id)) else {
                break "no channel".into();
            };
            if !notices.iter().all(|n| plain(&mut open, n)) {
                break "reply could not be sent".into();
            }
        }
        if !queue.is_empty() {
            active = Instant::now();
        }
        let now = Instant::now();
        if channel.is_none() && now - start > CONNECT_DEADLINE {
            break "not connected in time".into();
        }
        if now - start > PEER_LIFETIME {
            break "lifetime reached".into();
        }
        if !busy && subs.is_empty() && now - active > IDLE_LIMIT {
            break "idle".into();
        }
        if !rtc.is_alive() {
            break "connection ended".into();
        }
        // Flush per-sub pending backlog first: `drain(…)` above has just
        // pumped `rtc.poll_output`, so the SCTP sender may have freed send
        // window for pieces that `followed.recv()` stashed on the sub's
        // queue last tick. `flush_pending` revalidates each piece's authz
        // at its actual hand-off, not at draft.
        let mut ending: Vec<(u32, u64, TerminalEnd)> = Vec::new();
        let reply_waiting = !results.is_empty();
        // A completed reply takes precedence over more terminal data.
        if !reply_waiting {
            if let Some(mut open) = channel.and_then(|id| rtc.channel(id)) {
                for (sub_id, current) in subs.iter_mut() {
                    if current.pending.is_empty() {
                        continue;
                    }
                    let now = authz::store::now_secs();
                    let cause = flush_pending(
                        &mut store,
                        &mut current.pending,
                        &mut current.seq,
                        now,
                        |text| {
                            if !room_for_terminal_piece(open.buffered_amount(), text.len()) {
                                return false;
                            }
                            matches!(open.write(false, text.as_bytes()), Ok(true))
                        },
                    );
                    active = Instant::now();
                    if let Some(cause) = cause {
                        ending.push((*sub_id, current.seq, cause));
                    }
                }
            }
        }
        let mut fatal: Option<String> = None;
        for (sub_id, seq_for_end, cause) in ending {
            if let Some(gone) = subs.remove(&sub_id) {
                gone.task.abort();
                match channel.and_then(|id| rtc.channel(id)) {
                    Some(mut open) => {
                        if !plain(
                            &mut open,
                            &ChannelReply::Terminal(end_piece(sub_id, seq_for_end, cause)),
                        ) {
                            fatal = Some("reply could not be sent".into());
                            break;
                        }
                    }
                    None => {
                        fatal = Some("no channel".into());
                        break;
                    }
                }
            }
        }
        if let Some(reason) = fatal {
            break reason;
        }
        if !busy {
            if let Some((id, frame)) = queue.pop_front() {
                busy = true;
                // A subscribe for a session this channel already follows renews
                // that subscription; a new one needs a free slot.
                let sub = subscribe_target(&frame).map(|session| {
                    subs.iter()
                        .find(|(_, s)| s.session == session)
                        .map(|(sub, _)| *sub)
                        .or_else(|| {
                            (subs.len()
                                < conversations_core::private_channel::MAX_TERMINAL_SUBSCRIPTIONS)
                                .then(|| {
                                    next_sub += 1;
                                    next_sub - 1
                                })
                        })
                });
                let (host, binding, tx) = (host.clone(), binding.clone(), results_tx.clone());
                tokio::spawn(async move {
                    let _ = tx.send(answer(&host, &binding, id, frame, sub).await).await;
                });
            }
        }
        let room = channel
            .and_then(|id| rtc.channel(id))
            .is_some_and(|mut open| open.buffered_amount() < TERMINAL_BACKLOG);
        let wait = timeout
            .saturating_duration_since(now)
            .min(Duration::from_millis(250));
        let input = tokio::select! {
            Some((local, source, data)) = packets.recv() => Some((local, source, data)),
            Some(notice) = outcomes.recv() => {
                awaiting.retain(|(_, task)| !task.is_finished());
                let Some(mut open) = channel.and_then(|id| rtc.channel(id)) else { break "no channel".into() };
                if !plain(&mut open, &notice) {
                    break "reply could not be sent".into();
                }
                None
            }
            Some(reply) = results.recv(), if room => {
                busy = false;
                active = Instant::now();
                let Some(id) = channel else { break "no channel".into() };
                if reply.admitted.is_some() {
                    test_hook::before_release().await;
                }
                // An executed transfer expects its bytes before its reply goes out.
                let mut early: Option<ChannelReply> = None;
                if let Some(op) = &reply.transfer {
                    awaiting.retain(|(_, task)| !task.is_finished());
                    let left = (op.exp - authz::store::now_secs()).clamp(0, super::super::transfers::AUTH_TTL.as_secs() as i64);
                    let admitted = if awaiting.len() + inbound.open.len() >= conversations_core::private_channel::MAX_INBOUND_TRANSFERS {
                        Err("too many transfers in progress")
                    } else {
                        inbound.admit(op, Instant::now() + Duration::from_secs(left as u64))
                    };
                    match admitted {
                        Ok(None) => {}
                        Ok(Some(done)) => {
                            early = hand_off(&host, &mut store, *done, &outcomes_tx, &mut awaiting);
                        }
                        Err(reason) => {
                            eprintln!("Private transfer not expected: {reason}");
                            early = Expected::of(&op.op).map(|(_, transfer, _)| ChannelReply::Transfer { transfer, status: TransferStatus::Refused });
                        }
                    }
                }
                let Some(mut open) = rtc.channel(id) else { break "channel gone".into() };
                let subscribe = reply.subscribe.clone();
                // The final check and the hand-off, in one authz.db write
                // transaction with nothing awaited between them.
                match release(&mut store, reply, |text| {
                    let sent = open.write(false, text.as_bytes());
                    test_hook::inside_release();
                    matches!(sent, Ok(true))
                }) {
                    Ok(Some(true)) => {}
                    Ok(Some(false)) => break "reply could not be sent".into(),
                    Ok(None) => break "read authority withdrawn before delivery".into(),
                    Err(_) => break "the host could not check authority before delivery".into(),
                }
                if let Some(notice) = early {
                    if !plain(&mut open, &notice) {
                        break "reply could not be sent".into();
                    }
                }
                if let Some((sub, session, op, screen)) = subscribe {
                    match subs.get_mut(&sub) {
                        Some(current) => current.admitted = op,
                        None => {
                            let task = tokio::spawn(follow(screen, sub, followed_tx.clone()));
                            subs.insert(
                                sub,
                                Subscription {
                                    session,
                                    admitted: op,
                                    seq: 0,
                                    task,
                                    pending: VecDeque::new(),
                                },
                            );
                        }
                    }
                }
                None
            }
            Some((sub, frame)) = followed.recv(), if room && !reply_waiting => {
                let Some(mut open) = channel.and_then(|id| rtc.channel(id)) else { break "no channel".into() };
                let Some(current) = subs.get_mut(&sub) else { continue };
                let Some(frame) = frame else {
                    let gone = subs.remove(&sub).expect("present");
                    if !plain(&mut open, &ChannelReply::Terminal(end_piece(sub, gone.seq, TerminalEnd::Gone))) {
                        break "reply could not be sent".into();
                    }
                    continue;
                };
                // Draft pieces past any pieces already queued on this
                // subscription, then push through `push_and_flush`. If
                // pending is non-empty, the first pending piece has
                // `seq = current.seq`, the last has
                // `seq = current.seq + pending.len() - 1`; new pieces for
                // this frame must continue at `current.seq + pending.len()`
                // or their seqs collide with queued ones and the browser's
                // `Following.take` surfaces the overlap as a protocol gap.
                //
                // Any piece the SCTP send window can't take now
                // (`release_at` returns `Ok(Some(false))`) is appended to
                // the sub's pending queue; the outer-loop `flush_pending`
                // pass retries it on the next tick after `drain(…)` pumps
                // `rtc.poll_output`. Only successful hand-off advances
                // `current.seq`.
                let admitted_now = current.admitted.clone();
                let mut draft = current.seq + current.pending.len() as u64;
                let drafts = pieces(sub, &mut draft, &frame);
                let texts: Vec<String> = drafts
                    .into_iter()
                    .map(|p| serde_json::to_string(&ChannelReply::Terminal(p)).unwrap_or_default())
                    .collect();
                let now = authz::store::now_secs();
                let withheld = push_and_flush(
                    &mut store,
                    &admitted_now,
                    texts,
                    &mut current.pending,
                    &mut current.seq,
                    MAX_PENDING_PIECES,
                    now,
                    |text| {
                        if !room_for_terminal_piece(open.buffered_amount(), text.len()) {
                            return false;
                        }
                        matches!(open.write(false, text.as_bytes()), Ok(true))
                    },
                );
                let delivered = current.seq;
                if let Some(cause) = withheld {
                    // Authority is gone (or could not be checked, or the
                    // pending backlog saturated): stop this subscription
                    // only; the session and its agent are untouched.
                    let gone = subs.remove(&sub).expect("present");
                    gone.task.abort();
                    if !plain(&mut open, &ChannelReply::Terminal(end_piece(sub, delivered, cause))) {
                        break "reply could not be sent".into();
                    }
                }
                None
            }
            _ = tokio::time::sleep(wait) => None,
        };
        let fed = match input {
            Some((local, source, data)) => match data.as_slice().try_into() {
                Ok(contents) => rtc.handle_input(Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: local,
                        contents,
                    },
                )),
                Err(_) => Ok(()),
            },
            None => rtc.handle_input(Input::Timeout(Instant::now())),
        };
        if fed.is_err() {
            break "connection error".into();
        }
    };
    for (_, sub) in subs.drain() {
        sub.task.abort();
    }
    // Their outcome is no longer reported to anyone; the terminal still applies
    // or refuses what it was handed.
    for (transfer, task) in awaiting {
        task.abort();
        forget_receipt(&host.dir, &transfer);
    }
    rtc.disconnect();
    for reader in readers {
        reader.abort();
    }
    reason
}

/// Send a message that carries no private content (a transfer outcome, the end
/// of a subscription). `false`: the connection is stuck or gone.
fn plain(open: &mut str0m::channel::Channel<'_>, reply: &ChannelReply) -> bool {
    let text = serde_json::to_string(reply).unwrap_or_default();
    matches!(open.write(false, text.as_bytes()), Ok(true)) && open.buffered_amount() <= MAX_BUFFERED
}

/// Hand a complete transfer to its terminal. A transfer that was not handed
/// off is answered at once; otherwise its terminal's receipt is awaited (at
/// most until its permit expires, plus the time to stage and paste) and
/// answered later on `outcomes`.
fn hand_off(
    host: &Arc<Host>,
    store: &mut Store,
    done: Complete,
    outcomes: &mpsc::Sender<ChannelReply>,
    awaiting: &mut Vec<(String, tokio::task::JoinHandle<()>)>,
) -> Option<ChannelReply> {
    let transfer = done.transfer.clone();
    let permit = (done.admitted.exp - authz::store::now_secs())
        .clamp(0, super::super::transfers::AUTH_TTL.as_secs() as i64) as u64;
    let wait = Duration::from_secs(permit) + RECEIPT_GRACE;
    match forward(host, store, done) {
        Err(status) => Some(ChannelReply::Transfer { transfer, status }),
        Ok(receipt) => {
            let (dir, outcomes, id) = (host.dir.clone(), outcomes.clone(), transfer.clone());
            let task = tokio::spawn(async move {
                let status = outcome(&dir, &id, receipt, wait).await;
                let _ = outcomes
                    .send(ChannelReply::Transfer {
                        transfer: id,
                        status,
                    })
                    .await;
            });
            awaiting.push((transfer, task));
            None
        }
    }
}

/// How long a terminal may take to stage and paste after its permit expired.
const RECEIPT_GRACE: Duration = Duration::from_secs(30);

/// Hand a complete, hash-checked transfer to its session's terminal. The check
/// that its signer still holds exactly the authority it was admitted under and
/// the enqueue to the terminal share one immediate `authz.db` transaction; the
/// terminal checks the authority again before staging and before it writes.
fn forward(host: &Host, store: &mut Store, done: Complete) -> Result<Receipt, TransferStatus> {
    let session = done.session.clone();
    let Some(connection) = current_screen(host, &session) else {
        return Err(TransferStatus::Failed);
    };
    forward_to(&host.dir, &connection, store, done, |message| {
        enqueue_on(&host.sessions, &session, &connection, message)
    })
}

/// Enqueue `message` to `session`'s terminal connection `connection` (the
/// address of its screen), as [`Host::deliver_effect`] does. Fails, enqueuing
/// nothing, if a reconnect replaced that connection: only it can answer the
/// receipt. Runs inside the authority transaction (`authz.db`, then `sessions`).
pub(super) fn enqueue_on(
    sessions: &std::sync::Mutex<std::collections::HashMap<String, super::Session>>,
    session: &str,
    connection: &Connection,
    message: super::ServerMessage,
) -> anyhow::Result<()> {
    let sessions = sessions.lock().unwrap();
    let current = sessions
        .get(session)
        .context("session is not running on this host")?;
    if !current.screen.as_ref().is_some_and(|s| is(connection, s)) {
        anyhow::bail!("the terminal reconnected before the hand-off");
    }
    if !current.registration.effect_gate {
        return Err(super::UpgradeRequired.into());
    }
    current
        .commands
        .try_send(message)
        .map_err(|_| anyhow::anyhow!("terminal control queue unavailable"))
}

/// [`forward`], handing the content to `deliver`. `Ok`: enqueued; the
/// terminal's receipt says what happened. `Err`: nothing was enqueued.
fn forward_to(
    dir: &Path,
    connection: &Connection,
    store: &mut Store,
    done: Complete,
    deliver: impl FnOnce(super::ServerMessage) -> anyhow::Result<()>,
) -> Result<Receipt, TransferStatus> {
    use super::super::protocol::TransferContent;
    use base64::engine::general_purpose::STANDARD;
    let content = match done.content {
        Content::Image { mime, bytes } => TransferContent::Image {
            mime,
            b64: STANDARD.encode(bytes),
        },
        Content::Files(files) => TransferContent::Files {
            files: files
                .into_iter()
                .map(
                    |(name, bytes)| conversations_core::terminal_files::TerminalFile {
                        name,
                        b64: STANDARD.encode(bytes),
                    },
                )
                .collect(),
        },
    };
    // Waiting before the hand-off: a terminal can answer as soon as it has it.
    let Some(receipt) = expect_receipt(dir, &done.session, connection, &done.transfer) else {
        return Err(TransferStatus::Refused);
    };
    let message = super::ServerMessage::TransferContent {
        transfer: done.transfer.clone(),
        content,
    };
    let handed = store.with_current_authority(&done.admitted, authz::store::now_secs(), || {
        deliver(message)
    });
    let failed = match handed {
        Ok(Some(Ok(()))) => return Ok(receipt),
        Ok(None) => TransferStatus::Refused,
        Ok(Some(Err(error))) | Err(error) => {
            eprintln!("Private transfer not handed to the terminal: {error:#}");
            TransferStatus::Failed
        }
    };
    forget_receipt(dir, &done.transfer);
    Err(failed)
}

/// The terminal's answer to one transfer it was handed.
type Receipt = tokio::sync::oneshot::Receiver<TerminalFileStatus>;

/// Transfers handed to a terminal and not answered yet, by host directory and
/// transfer: the session, the terminal connection it was handed to and the
/// waiter. Every entry is removed when its outcome is known, its wait ends or
/// that connection ends.
type Receipts = std::collections::HashMap<
    (PathBuf, String),
    (
        String,
        Connection,
        tokio::sync::oneshot::Sender<TerminalFileStatus>,
    ),
>;

/// A terminal connection, by its screen, held weakly: that keeps the screen's
/// allocation, so a later connection's screen can never take its address
/// while a transfer is bound to it.
type Connection = std::sync::Weak<std::sync::Mutex<Screen>>;

/// Whether `screen` is the screen of `connection`.
fn is(connection: &Connection, screen: &SharedScreen) -> bool {
    std::ptr::eq(connection.as_ptr(), Arc::as_ptr(screen))
}

/// At most this many transfers wait for a receipt at once, on all channels.
const MAX_AWAITED_RECEIPTS: usize = 64;

fn receipts() -> &'static std::sync::Mutex<Receipts> {
    static RECEIPTS: std::sync::OnceLock<std::sync::Mutex<Receipts>> = std::sync::OnceLock::new();
    RECEIPTS.get_or_init(Default::default)
}

/// Wait for the terminal connection `connection` of `session` to answer
/// `transfer`. `None`: already waiting for it, or too many waits.
pub(super) fn expect_receipt(
    dir: &Path,
    session: &str,
    connection: &Connection,
    transfer: &str,
) -> Option<Receipt> {
    let mut all = receipts().lock().unwrap();
    let key = (dir.to_owned(), transfer.to_owned());
    if all.contains_key(&key) || all.len() >= MAX_AWAITED_RECEIPTS {
        return None;
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    all.insert(key, (session.to_owned(), connection.clone(), tx));
    Some(rx)
}

pub(super) fn forget_receipt(dir: &Path, transfer: &str) {
    receipts()
        .lock()
        .unwrap()
        .remove(&(dir.to_owned(), transfer.to_owned()));
}

/// The terminal connection that shows `own` for `session` reports what
/// happened to `transfer`. A report from another session, or from an earlier
/// or later connection of the same session, settles nothing.
pub(super) fn transfer_receipt(
    dir: &Path,
    session: &str,
    own: &SharedScreen,
    transfer: &str,
    status: TerminalFileStatus,
) {
    let mut all = receipts().lock().unwrap();
    let key = (dir.to_owned(), transfer.to_owned());
    if all
        .get(&key)
        .is_some_and(|(owner, on, _)| owner == session && is(on, own))
    {
        let (_, _, tx) = all.remove(&key).expect("present");
        let _ = tx.send(status);
    } else {
        eprintln!("Ignored a transfer receipt that is not this terminal connection's");
    }
}

/// The terminal connection that showed `own` ended: what it was handed and did
/// not answer stays uncertain. Other connections' waits are untouched.
pub(super) fn abandon_receipts(dir: &Path, session: &str, own: &SharedScreen) {
    receipts()
        .lock()
        .unwrap()
        .retain(|(d, _), (owner, on, _)| !(d == dir && owner == session && is(on, own)));
}

/// What happened to a transfer handed to its terminal, waiting at most `wait`.
/// Never `Delivered` without the terminal's own success report.
pub(super) async fn outcome(
    dir: &Path,
    transfer: &str,
    receipt: Receipt,
    wait: Duration,
) -> TransferStatus {
    let answered = tokio::time::timeout(wait, receipt).await;
    forget_receipt(dir, transfer);
    match answered {
        Ok(Ok(TerminalFileStatus::Sent)) => TransferStatus::Delivered,
        Ok(Ok(TerminalFileStatus::Failed)) => TransferStatus::Failed,
        Ok(Err(_)) | Err(_) => TransferStatus::Uncertain,
    }
}

/// The session a frame claims to subscribe to, if it is a `terminal.subscribe`.
/// Untrusted: only used to pick a subscription slot before verification.
fn subscribe_target(frame: &SignedFrame) -> Option<String> {
    if claimed_operation(&frame.payload).as_deref() != Some("terminal.subscribe") {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct Claimed {
        sess: String,
    }
    serde_json::from_slice::<Claimed>(&frame.payload)
        .ok()
        .map(|c| c.sess)
}

/// Send what str0m wants sent and act on its events, until it next needs time.
fn drain(
    rtc: &mut Rtc,
    sockets: &Sockets,
    binding: &Binding,
    channel: &mut Option<ChannelId>,
    arrived: &mut Vec<Arrival>,
    in_flight: usize,
) -> Result<Instant, String> {
    let mut ops = in_flight;
    loop {
        match rtc
            .poll_output()
            .map_err(|_| "connection error".to_owned())?
        {
            Output::Timeout(at) => return Ok(at),
            Output::Transmit(transmit) => {
                if let Some((_, socket)) = sockets.iter().find(|(a, _)| *a == transmit.source) {
                    let _ = socket.try_send_to(&transmit.contents, transmit.destination);
                }
            }
            Output::Event(event) => match event {
                Event::Connected => {
                    // The certificate the DTLS peer presented is the one the offer signed.
                    let seen = rtc
                        .direct_api()
                        .remote_dtls_fingerprint()
                        .map(|f| f.to_string());
                    if seen.as_deref() != Some(binding.fingerprint.as_str()) {
                        return Err("peer certificate is not the signed one".into());
                    }
                }
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    return Err("disconnected".into())
                }
                Event::ChannelOpen(id, label) => {
                    if channel.is_some() || label != CHANNEL_LABEL {
                        return Err("unexpected data channel".into());
                    }
                    *channel = Some(id);
                }
                Event::ChannelClose(_) => return Err("closed by the browser".into()),
                Event::ChannelData(data) => {
                    if Some(data.id) != *channel
                        || data.binary
                        || data.data.len() > MAX_REQUEST_MESSAGE
                    {
                        return Err("message outside the channel's bounds".into());
                    }
                    let Ok(request) = serde_json::from_slice::<ChannelRequest>(&data.data) else {
                        return Err("malformed message".into());
                    };
                    arrived.push(match request {
                        ChannelRequest::Op { id, p, s, k } => {
                            let Some(frame) = SignedFrame::from_wire(&p, &s, &k) else {
                                return Err("malformed operation".into());
                            };
                            if ops >= MAX_QUEUED {
                                return Err("too many requests in flight".into());
                            }
                            ops += 1;
                            Arrival::Op(id, frame)
                        }
                        ChannelRequest::Chunk {
                            transfer,
                            file,
                            offset,
                            data,
                        } => {
                            let Ok(data) = B64.decode(data) else {
                                return Err("malformed transfer bytes".into());
                            };
                            Arrival::Chunk {
                                transfer,
                                file,
                                offset,
                                data,
                            }
                        }
                        ChannelRequest::Cancel { transfer } => Arrival::Cancel(transfer),
                        ChannelRequest::Unsubscribe { sub } => Arrival::Unsubscribe(sub),
                    });
                }
                _ => {}
            },
        }
    }
}

/// A signed reply the host has not handed to the connection yet. A reply that
/// carries private content keeps the EXACT admitted operation that produced it
/// (with the authority rows it was admitted under), so its final check compares
/// against that admission, never merely against whatever grant exists now: a
/// grant removed and given again does not release a reply read before.
struct Pending {
    text: String,
    admitted: Option<Verified>,
    /// An executed transfer: its bytes are expected on this channel.
    transfer: Option<Verified>,
    /// An admitted terminal subscribe (or renewal): sub id, session, the
    /// admitted operation and the screen to follow.
    subscribe: Option<(
        u32,
        String,
        Verified,
        std::sync::Weak<std::sync::Mutex<Screen>>,
    )>,
}

/// Hand `reply` to `deliver`, or withhold it. A reply with private content is
/// handed off only inside [`Store::with_current_authority`]: the check that its
/// reader still holds exactly the authority it was admitted under, and the
/// hand-off, share one immediate `authz.db` transaction, so an authority change
/// committed by any process is either seen (and the reply withheld) or waits
/// until the reply is handed off. `Ok(None)`: withheld.
fn release<R>(
    store: &mut Store,
    reply: Pending,
    deliver: impl FnOnce(&str) -> R,
) -> anyhow::Result<Option<R>> {
    release_at(store, reply, authz::store::now_secs(), deliver)
}

/// [`release`] at the given time, so a caller can tell why it was withheld.
fn release_at<R>(
    store: &mut Store,
    reply: Pending,
    now: i64,
    deliver: impl FnOnce(&str) -> R,
) -> anyhow::Result<Option<R>> {
    match &reply.admitted {
        None => Ok(Some(deliver(&reply.text))),
        Some(op) => store.with_current_authority(op, now, || deliver(&reply.text)),
    }
}

/// Why a piece checked at `now` under an op expiring at `exp` was withheld:
/// the same `now > exp` the verifier and the store treat as expired.
fn end_cause(now: i64, exp: i64) -> TerminalEnd {
    if now > exp {
        TerminalEnd::Expired
    } else {
        TerminalEnd::Withdrawn
    }
}

/// Drain `pending` against `deliver`. Each delivered piece advances
/// `*cursor` by one. Stops on the first `Ok(Some(false))` (leaving the
/// remainder in `pending` for a later tick). On `Ok(None)` or `Err(_)` the
/// authority check went wrong: clears `pending` and returns the end cause.
///
/// The authz revalidation at each hand-off is preserved — every piece is
/// released under the admission it was drafted with (stored alongside its
/// serialized text).
fn flush_pending<D>(
    store: &mut Store,
    pending: &mut VecDeque<(String, Verified)>,
    cursor: &mut u64,
    now: i64,
    mut deliver: D,
) -> Option<TerminalEnd>
where
    D: FnMut(&str) -> bool,
{
    while let Some(front) = pending.front() {
        let text = front.0.clone();
        let admitted = front.1.clone();
        let reply = Pending {
            text,
            admitted: Some(admitted.clone()),
            transfer: None,
            subscribe: None,
        };
        let outcome = release_at(store, reply, now, |t| deliver(t));
        match outcome {
            Ok(Some(true)) => {
                pending.pop_front();
                *cursor += 1;
            }
            Ok(Some(false)) => return None,
            Ok(None) => {
                let exp = admitted.exp;
                pending.clear();
                return Some(end_cause(now, exp));
            }
            Err(_) => {
                pending.clear();
                return Some(TerminalEnd::Withdrawn);
            }
        }
    }
    None
}

/// Append `texts` to `pending` under `admitted`, then flush. If appending
/// would exceed `cap`, returns `Some(Expired)` without touching `pending`
/// or `cursor`: the sub's channel is genuinely wedged, not just
/// momentarily back-pressured, and the browser's auto-resubscribe gives
/// the viewer a fresh snapshot.
#[allow(clippy::too_many_arguments)]
fn push_and_flush<D>(
    store: &mut Store,
    admitted: &Verified,
    texts: Vec<String>,
    pending: &mut VecDeque<(String, Verified)>,
    cursor: &mut u64,
    cap: usize,
    now: i64,
    deliver: D,
) -> Option<TerminalEnd>
where
    D: FnMut(&str) -> bool,
{
    if pending.len() + texts.len() > cap {
        return Some(TerminalEnd::Expired);
    }
    for text in texts {
        pending.push_back((text, admitted.clone()));
    }
    flush_pending(store, pending, cursor, now, deliver)
}

/// Debug-build test barriers for the delivery boundary, armed only by
/// `CONVERSATIONS_TEST_DELIVERY_BARRIER=<dir>`. With `<dir>/hold` present,
/// `before_release` holds a read reply after it was read and signed, before its
/// final check, until `<dir>/release` exists; with `<dir>/hold-inside` present, `inside_release`
/// holds INSIDE the final transaction (after the hand-off) until
/// `<dir>/release-inside` exists. Release builds compile both to nothing.
mod test_hook {
    #[cfg(debug_assertions)]
    fn dir() -> Option<std::path::PathBuf> {
        std::env::var_os("CONVERSATIONS_TEST_DELIVERY_BARRIER").map(Into::into)
    }

    #[cfg(debug_assertions)]
    pub(super) async fn before_release() {
        let Some(dir) = dir() else { return };
        if !dir.join("hold").exists() {
            return;
        }
        let _ = std::fs::write(dir.join("held"), b"");
        let release = dir.join("release");
        for _ in 0..600 {
            if std::fs::remove_file(&release).is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = std::fs::remove_file(dir.join("held"));
    }

    #[cfg(debug_assertions)]
    pub(super) fn inside_release() {
        let Some(dir) = dir() else { return };
        if !dir.join("hold-inside").exists() {
            return;
        }
        let _ = std::fs::write(dir.join("inside"), b"");
        let release = dir.join("release-inside");
        for _ in 0..600 {
            if std::fs::remove_file(&release).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(dir.join("inside"));
    }

    #[cfg(not(debug_assertions))]
    pub(super) async fn before_release() {}

    #[cfg(not(debug_assertions))]
    pub(super) fn inside_release() {}
}

/// Handle one operation and return the signed `reply` message for it.
/// Answer one signed operation. `sub` is the subscription slot a
/// `terminal.subscribe` takes (`Some(None)`: this channel has none free).
async fn answer(
    host: &Arc<Host>,
    binding: &Binding,
    id: u32,
    frame: SignedFrame,
    sub: Option<Option<u32>>,
) -> Pending {
    let payload = frame.payload.clone();
    let bound = frame.kid == binding.kid
        && claimed_delegation(&frame.payload).as_ref() == Some(&binding.dlg);
    let claimed = claimed_operation(&frame.payload);
    let mut transfer = None;
    let (mut status, mut detail, body): (OpStatus, Option<String>, Option<(ReplyBody, Verified)>) =
        if !bound {
            // Not admitted at all: this channel belongs to another key.
            (
                OpStatus::Refused,
                Some("signed by a key other than this connection's".to_owned()),
                None,
            )
        } else if sub == Some(None) {
            (
                OpStatus::Refused,
                Some("too many terminal subscriptions on this connection".to_owned()),
                None,
            )
        } else if claimed.as_deref().is_some_and(private_only) {
            read(host, frame.clone()).await
        } else if matches!(
            claimed.as_deref(),
            Some("session.paste_image" | "session.send_files")
        ) {
            let (status, detail, op) = transfer_answer(host, frame.clone()).await;
            transfer = op;
            (status, detail, None)
        } else {
            let (status, detail) = execute_signed(host, frame.clone()).await;
            (status, detail, None)
        };
    let (mut body, admitted) = match body {
        Some((body, op)) => (Some(body), Some(op)),
        None => (None, None),
    };
    // An admitted subscribe follows the screen of the session's CURRENT
    // connection; a reconnect replaces that screen and needs a fresh subscribe.
    let mut subscribe = None;
    if let (Some(op), Some(Some(sub))) = (&admitted, sub) {
        if let Operation::TerminalSubscribe { session } = &op.op {
            let screen = current_screen(host, session);
            match screen {
                Some(screen) => {
                    body = Some(ReplyBody::Terminal { sub });
                    subscribe = Some((sub, session.clone(), op.clone(), screen));
                }
                None => {
                    status = OpStatus::Failed;
                    detail = Some("the session's terminal is not attached".into());
                    body = None;
                }
            }
        }
    }
    let admitted = if body.is_some() { admitted } else { None };
    let dir = host.dir.clone();
    let (kid, peer) = (binding.kid.clone(), binding.peer.clone());
    let signed = tokio::task::spawn_blocking(move || {
        signed_reply(&dir, peer, kid, &payload, status, detail, body)
    })
    .await;
    let text = match signed {
        Ok(Ok(statement)) => {
            serde_json::to_string(&ChannelReply::Reply { id, statement }).unwrap_or_default()
        }
        // An unsigned answer would be rejected by the browser anyway; send none,
        // and the browser reports the request as unanswered.
        _ => String::new(),
    };
    if text.is_empty() || status != OpStatus::Executed {
        subscribe = None;
    }
    if status != OpStatus::Executed {
        transfer = None;
    }
    Pending {
        text,
        admitted,
        transfer,
        subscribe,
    }
}

/// The host-signed statement answering the operation `payload` on a channel.
fn signed_reply(
    dir: &Path,
    peer: String,
    kid: String,
    payload: &[u8],
    status: OpStatus,
    detail: Option<String>,
    body: Option<ReplyBody>,
) -> anyhow::Result<conversations_core::host_relay::Signed> {
    let mut store = Store::open(dir)?;
    let mut statement = ReplyStatement {
        v: 1,
        typ: "channel_reply".into(),
        host: store.host_id()?,
        peer,
        kid,
        request: sha256_b64(payload),
        status: status.as_str().into(),
        // Whole: only the encrypted channel carries it, and the size bound below
        // replaces an oversized answer rather than cutting a structured detail.
        detail,
        body,
    };
    if serde_json::to_vec(&statement)?.len() > MAX_REPLY_PAYLOAD {
        statement.status = OpStatus::Failed.as_str().into();
        statement.detail = Some("the answer is too large".into());
        statement.body = None;
    }
    authz::attest::sign_value(&mut store, &statement)
}

/// A signed transfer on the channel: admitted and journaled like any
/// operation, its terminal told what to accept. Returns the admitted operation
/// when the bytes may follow on this channel.
async fn transfer_answer(
    host: &Host,
    frame: SignedFrame,
) -> (OpStatus, Option<String>, Option<Verified>) {
    let dir = host.dir.clone();
    let decided = tokio::task::spawn_blocking(move || {
        authz::dispatch::decide(&dir, &frame, authz::store::now_secs())
    })
    .await;
    let op = match decided {
        Ok(Ok(Decision::Execute(op))) => op,
        // A replay never accepts bytes again.
        Ok(Ok(Decision::Replay { state, .. })) => {
            let (status, detail) = super::replay_outcome(&state);
            return (status, Some(detail.into()), None);
        }
        Ok(Ok(Decision::Refused(reason))) => return (OpStatus::Refused, Some(reason), None),
        Ok(Ok(Decision::Stream(_) | Decision::Applied(_))) => {
            return (OpStatus::Refused, Some("not a transfer".into()), None)
        }
        Ok(Err(error)) => {
            eprintln!("Signed transfer not admitted: {error:#}");
            return (
                OpStatus::Failed,
                Some("the host could not check the operation".into()),
                None,
            );
        }
        Err(_) => {
            return (
                OpStatus::Failed,
                Some("the host could not check the operation".into()),
                None,
            )
        }
    };
    let outcome = super::transfer_signed(host, &op).await;
    let dir = host.dir.clone();
    let recorded = {
        let (op, text) = (
            op.clone(),
            outcome
                .as_ref()
                .map(|_| ())
                .map_err(authz::dispatch::outcome_error),
        );
        tokio::task::spawn_blocking(move || authz::dispatch::finish(&dir, &op, &text)).await
    };
    if !matches!(recorded, Ok(Ok(()))) {
        eprintln!("Cannot record signed transfer outcome: {recorded:?}");
    }
    match outcome {
        Ok(()) => (OpStatus::Executed, None, Some(op)),
        Err(error)
            if authz::dispatch::is_revoked(&error)
                || error.downcast_ref::<super::UpgradeRequired>().is_some() =>
        {
            (OpStatus::Refused, Some(error.to_string()), None)
        }
        Err(error) => {
            eprintln!("Signed transfer failed: {error:#}");
            (
                OpStatus::Failed,
                Some("the terminal could not accept the transfer".into()),
                None,
            )
        }
    }
}

type ReadOutcome = (OpStatus, Option<String>, Option<(ReplyBody, Verified)>);

/// A private read: verified, authorized and journaled like any operation, then
/// answered from local state only. The admitted operation travels with the body
/// to the final check.
async fn read(host: &Host, frame: SignedFrame) -> ReadOutcome {
    let dir = host.dir.clone();
    let (running, readiness, screens) = {
        let sessions = host.sessions.lock().unwrap();
        (
            sessions.keys().cloned().collect::<Vec<_>>(),
            sessions
                .iter()
                .map(|(id, session)| (id.clone(), *session.readiness.lock().unwrap()))
                .collect::<std::collections::HashMap<_, _>>(),
            sessions
                .iter()
                .filter_map(|(id, session)| {
                    session.screen.clone().map(|screen| (id.clone(), screen))
                })
                .collect::<std::collections::HashMap<_, _>>(),
        )
    };
    let outcome = tokio::task::spawn_blocking(move || {
        let mut result = read_now(&dir, &frame, &running, authz::store::now_secs())?;
        if let Some((ReplyBody::Sessions { sessions, .. }, op)) = &mut result.2 {
            attach_readiness(sessions, op, &readiness);
            let history = History::open(&dir)?;
            for session in sessions {
                if session.agent != "zsh"
                    || session.prs.len() >= conversations_core::terminal_prs::MAX_TERMINAL_PR_URLS
                {
                    continue;
                }
                if let Some(screen) = screens.get(&session.id) {
                    let (normal, alternate) = screen.lock().unwrap().discovery_screens();
                    let urls = conversations_core::terminal_prs::discover(normal, alternate);
                    history.remember_prs(&session.id, &urls)?;
                    session.prs = history.listed_prs(&session.id)?;
                }
            }
        }
        Ok::<_, anyhow::Error>(result)
    })
    .await;
    match outcome {
        Ok(Ok(result)) => result,
        _ => (
            OpStatus::Failed,
            Some("the host could not check the operation".into()),
            None,
        ),
    }
}

fn attach_readiness(
    sessions: &mut [SessionEntry],
    op: &Verified,
    evidence: &std::collections::HashMap<
        String,
        conversations_core::private_channel::AgentReadiness,
    >,
) {
    if matches!(
        op.op,
        Operation::SessionsList {
            readiness: true,
            ..
        }
    ) {
        for session in sessions {
            session.readiness = evidence.get(&session.id).copied();
        }
    }
}

/// The blocking part of [`read`]. Authority is checked against the CURRENT
/// grants, parent and delegation when the answer is formed, for a new read and
/// for a replayed one: an earlier journal row is never enough to answer. This
/// is not the delivery boundary: [`release`] checks again at hand-off, against
/// the authority this read was admitted under.
fn read_now(
    dir: &std::path::Path,
    frame: &SignedFrame,
    running: &[String],
    now: i64,
) -> anyhow::Result<ReadOutcome> {
    let op = match authz::dispatch::decide(dir, frame, now)? {
        Decision::Execute(op) => op,
        Decision::Replay { state, .. } => {
            // A replay never carries content, and is answered only while the
            // signer still holds the read authority now.
            if let Some(reason) = authz::dispatch::current_authority(dir, frame, now)? {
                return Ok((OpStatus::Refused, Some(reason), None));
            }
            let (status, detail) = replay_outcome(&state);
            return Ok((status, Some(detail.to_owned()), None));
        }
        Decision::Refused(reason) => return Ok((OpStatus::Refused, Some(reason), None)),
        Decision::Stream(_) | Decision::Applied(_) => {
            return Ok((OpStatus::Refused, Some("not a read".into()), None))
        }
    };
    authz::dispatch::begin(dir, &op)?;
    let body = answer_read(dir, &op, running, now);
    let finished = body
        .as_ref()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{e:#}"));
    // The journal records only that the read ran, never what it returned.
    authz::dispatch::finish(dir, &op, &finished)?;
    // An authority change while the read ran stops the content here; `release`
    // checks again at hand-off.
    if !Store::open(dir)?.still_admitted(&op, authz::store::now_secs())? {
        return Ok((
            OpStatus::Refused,
            Some("authority changed during the read".into()),
            None,
        ));
    }
    Ok(match body {
        Ok(body) => (OpStatus::Executed, None, Some((body, op))),
        Err(error) if error.to_string() == "PR cache changed; restart from offset 0" => {
            (OpStatus::Refused, Some(error.to_string()), None)
        }
        Err(error) => {
            eprintln!("Private read failed: {error:#}");
            (
                OpStatus::Failed,
                Some("the host could not read its history".into()),
                None,
            )
        }
    })
}

/// Form the body of one admitted private read from local state only.
fn answer_read(
    dir: &std::path::Path,
    op: &Verified,
    running: &[String],
    now: i64,
) -> anyhow::Result<ReplyBody> {
    Ok(match &op.op {
        Operation::MigrationExport {
            session,
            transfer,
            offset,
        } => super::super::migration::export(dir, &op.kid, session, transfer, *offset)?,
        Operation::HistoryGet {
            session,
            after,
            limit,
            digest,
        } => {
            let (items, next) =
                History::open(dir)?.page(session, *after, *limit, PAGE_BYTES, *digest)?;
            ReplyBody::History { items, next }
        }
        Operation::PrCacheList {
            session,
            offset,
            sha256,
        } => {
            let (b64, sha256, next) =
                super::super::pr_metadata::chunk(dir, session, *offset, sha256.as_deref())?;
            ReplyBody::PrCache { b64, sha256, next }
        }
        Operation::HistoryFragment {
            session,
            seq,
            offset,
        } => {
            let piece = History::open(dir)?
                .fragment(session, *seq, *offset)?
                .context("no such message piece")?;
            ReplyBody::Fragment {
                seq: *seq,
                offset: *offset,
                data: B64.encode(&piece.data),
                size: piece.size,
                sha256: piece.sha256,
                next: piece.next,
            }
        }
        Operation::HistorySearch {
            session,
            query,
            limit,
        } => ReplyBody::Search {
            hits: History::open(dir)?
                .search(query, Some(session), *limit as usize)?
                .into_iter()
                .map(|hit| SearchHit {
                    seq: hit.seq,
                    uuid: hit.uuid,
                    snippet: hit.snippet,
                })
                .collect(),
        },
        Operation::SessionsList {
            after,
            limit,
            running_only,
            ..
        } => {
            let (listed, next) = Store::open(dir)?.readable_sessions(
                op,
                after.as_deref(),
                *limit,
                now,
                running_only.then_some(running),
            )?;
            let history = History::open(dir)?;
            let store = Store::open(dir)?;
            let mut sessions = Vec::with_capacity(listed.len());
            for row in listed {
                let summary = history.summary(&row.session_id)?;
                sessions.push(SessionEntry {
                    readiness: None,
                    running: running.contains(&row.session_id),
                    reopenable: Some(store.can_reopen_import(&row.session_id)?),
                    resume_mode: store.import_resume_mode(&row.session_id)?,
                    prs: history.listed_prs(&row.session_id)?,
                    id: row.session_id,
                    agent: row.agent,
                    cwd: row.cwd,
                    title: summary.title.map(|t| cut_utf8(&t, MAX_LIST_TITLE)),
                    messages: summary.messages,
                    last_activity: summary
                        .last_updated
                        .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
                        .map(|at| at.to_rfc3339()),
                });
            }
            ReplyBody::Sessions { sessions, next }
        }
        Operation::TerminalSubscribe { session } => {
            anyhow::ensure!(
                running.contains(session),
                "session is not running on this host"
            );
            // The slot is filled in by the channel once the screen is found.
            ReplyBody::Terminal { sub: 0 }
        }
        Operation::LinearList {
            session,
            after,
            limit,
        } => {
            let (docs, next) =
                History::open(dir)?.linear_page(session, after.as_deref(), *limit)?;
            ReplyBody::Linear { docs, next }
        }
        Operation::CommunicationsList {
            session,
            after,
            limit,
        } => {
            let mut authority = Store::open(dir)?;
            let page = super::super::communication::list(
                dir,
                session,
                after.as_deref(),
                *limit,
                |peer| authority.can_read_session(op, peer, now),
                Some(running),
            )?;
            ReplyBody::Communications {
                peers: page.peers,
                exchanges: page.exchanges,
                next: page.next,
            }
        }
        Operation::AttentionRecipients { session } => {
            let store = Store::open(dir)?;
            let sender = store.human_subject(op, now)?;
            ReplyBody::AttentionRecipients {
                recipients: store.attention_recipients(session, &sender)?,
            }
        }
        Operation::NotificationsList { after, limit } => {
            let mut store = Store::open(dir)?;
            let viewer = store.human_subject(op, now)?;
            let mut authority = Store::open(dir)?;
            let (mut notifications, next) =
                store.human_notifications(&viewer, *after, *limit, |session| {
                    authority.can_read_session(op, session, now)
                })?;
            let history = History::open(dir)?;
            for notification in &mut notifications {
                if let Some(title) = history.summary(&notification.session_id)?.title {
                    notification.session_title = title.chars().take(200).collect();
                }
            }
            ReplyBody::Notifications {
                notifications,
                next,
            }
        }
        Operation::AccessList => ReplyBody::Access {
            credentials: super::super::access_metadata::list(dir)?,
        },
        Operation::ProjectsList => ReplyBody::Projects {
            projects: super::super::projects::Projects::open(dir)?.list()?,
        },
        Operation::SessionProjectGet { session } => ReplyBody::ProjectMembership {
            project_id: super::super::projects::Projects::open(dir)?.membership(session)?,
        },
        Operation::SettingsGet => ReplyBody::Settings {
            launch: serde_json::to_value(&Settings::load(dir)?.launch)?,
        },
        _ => anyhow::bail!("not a read"),
    })
}

/// The launch block a `settings.get` frame receives on a private channel, or
/// `None` when it is refused.
#[cfg(test)]
pub(super) fn channel_settings(
    dir: &std::path::Path,
    frame: &SignedFrame,
) -> Option<serde_json::Value> {
    match read_now(dir, frame, &[], authz::store::now_secs()).ok()? {
        (OpStatus::Executed, _, Some((ReplyBody::Settings { launch }, _))) => Some(launch),
        _ => None,
    }
}

/// At most `max` bytes of `text`, never inside a character.
fn cut_utf8(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The exact bytes one admitted transfer must consist of.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Expected {
    Image {
        mime: String,
        size: u64,
        sha256: String,
    },
    Files(Vec<(String, u64, String)>),
}

impl Expected {
    fn of(op: &Operation) -> Option<(String, String, Self)> {
        match op {
            Operation::SessionPasteImage {
                session,
                transfer,
                mime,
                size,
                sha256,
            } => Some((
                session.clone(),
                transfer.clone(),
                Expected::Image {
                    mime: mime.clone(),
                    size: *size,
                    sha256: sha256.clone(),
                },
            )),
            Operation::SessionSendFiles {
                session,
                transfer,
                files,
            } => Some((
                session.clone(),
                transfer.clone(),
                Expected::Files(
                    files
                        .iter()
                        .map(|f| (f.name.clone(), f.size, f.sha256.clone()))
                        .collect(),
                ),
            )),
            _ => None,
        }
    }

    fn sizes(&self) -> Vec<u64> {
        match self {
            Expected::Image { size, .. } => vec![*size],
            Expected::Files(files) => files.iter().map(|(_, size, _)| *size).collect(),
        }
    }

    fn sha256(&self, file: usize) -> &str {
        match self {
            Expected::Image { sha256, .. } => sha256,
            Expected::Files(files) => &files[file].2,
        }
    }
}

/// Verified content of one transfer, ready for its session's terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Content {
    Image { mime: String, bytes: Vec<u8> },
    Files(Vec<(String, Vec<u8>)>),
}

/// A transfer whose bytes all arrived and match what was signed.
pub(super) struct Complete {
    pub transfer: String,
    pub session: String,
    /// The admitted signed operation, checked again before the hand-off.
    pub admitted: Verified,
    pub content: Content,
}

/// What one chunk did.
pub(super) enum Fed {
    /// Accepted; more bytes are due.
    More,
    Complete(Box<Complete>),
    /// Not acceptable: the transfer is dropped and nothing is applied.
    Refused(&'static str),
}

struct Incoming {
    transfer: String,
    session: String,
    admitted: Verified,
    expected: Expected,
    sizes: Vec<u64>,
    received: Vec<Vec<u8>>,
    /// The file being received and its running hash.
    file: usize,
    hash: digest::Context,
    deadline: Instant,
}

/// The bytes of transfers this channel admitted, until they are complete. A
/// transfer is accepted only after its signed operation was executed on this
/// channel; its bytes come in order, file by file, never more than signed, and
/// each file must hash to the signed SHA-256. Anything else drops the transfer.
#[derive(Default)]
pub(super) struct Inbound {
    open: Vec<Incoming>,
}

impl Inbound {
    /// Expect the bytes of the admitted transfer `op` until `deadline`. A
    /// transfer of only empty files is complete at once.
    pub(super) fn admit(
        &mut self,
        op: &Verified,
        deadline: Instant,
    ) -> Result<Option<Box<Complete>>, &'static str> {
        use conversations_core::private_channel::MAX_INBOUND_TRANSFERS;
        let Some((session, transfer, expected)) = Expected::of(&op.op) else {
            return Err("not a transfer");
        };
        let now = Instant::now();
        self.open.retain(|o| o.deadline > now);
        if self.open.iter().any(|o| o.transfer == transfer) {
            return Err("transfer id already used");
        }
        if self.open.len() >= MAX_INBOUND_TRANSFERS {
            return Err("too many transfers in progress");
        }
        let sizes = expected.sizes();
        let mut incoming = Incoming {
            transfer,
            session,
            admitted: op.clone(),
            received: Vec::with_capacity(sizes.len()),
            sizes,
            expected,
            file: 0,
            hash: digest::Context::new(&digest::SHA256),
            deadline,
        };
        incoming.received.push(Vec::new());
        match Self::advance(&mut incoming) {
            Fed::More => {
                self.open.push(incoming);
                Ok(None)
            }
            Fed::Complete(_) => Ok(Some(Box::new(Self::finish(incoming)))),
            Fed::Refused(reason) => Err(reason),
        }
    }

    /// Drop a transfer. `true` when it was open.
    pub(super) fn cancel(&mut self, transfer: &str) -> bool {
        let before = self.open.len();
        self.open.retain(|o| o.transfer != transfer);
        before != self.open.len()
    }

    /// Accept the next bytes of `transfer`. `None`: no such open transfer.
    pub(super) fn feed(
        &mut self,
        transfer: &str,
        file: u32,
        offset: u64,
        data: &[u8],
        now: Instant,
    ) -> Option<Fed> {
        let at = self.open.iter().position(|o| o.transfer == transfer)?;
        let fed = Self::accept(&mut self.open[at], file, offset, data, now);
        if !matches!(fed, Fed::More) {
            let incoming = self.open.remove(at);
            if let Fed::Complete(_) = fed {
                return Some(Fed::Complete(Box::new(Self::finish(incoming))));
            }
        }
        Some(fed)
    }

    fn accept(incoming: &mut Incoming, file: u32, offset: u64, data: &[u8], now: Instant) -> Fed {
        use conversations_core::private_channel::MAX_CHUNK_BYTES;
        if now >= incoming.deadline {
            return Fed::Refused("the transfer expired");
        }
        if file as usize != incoming.file
            || offset != incoming.received[incoming.file].len() as u64
            || data.is_empty()
            || data.len() > MAX_CHUNK_BYTES
        {
            return Fed::Refused("bytes out of order");
        }
        if offset + data.len() as u64 > incoming.sizes[incoming.file] {
            return Fed::Refused("more bytes than were signed");
        }
        incoming.received[incoming.file].extend_from_slice(data);
        incoming.hash.update(data);
        Self::advance(incoming)
    }

    /// Close every file that is complete (an empty one is complete at once).
    fn advance(incoming: &mut Incoming) -> Fed {
        while incoming.received[incoming.file].len() as u64 == incoming.sizes[incoming.file] {
            let hash = std::mem::replace(&mut incoming.hash, digest::Context::new(&digest::SHA256))
                .finish();
            let hex: String = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();
            if hex != incoming.expected.sha256(incoming.file) {
                return Fed::Refused("the bytes do not match what was signed");
            }
            if incoming.file + 1 == incoming.sizes.len() {
                return Fed::Complete(Box::new(Complete {
                    transfer: String::new(),
                    session: String::new(),
                    admitted: incoming.admitted.clone(),
                    content: Content::Files(Vec::new()),
                }));
            }
            incoming.file += 1;
            incoming.received.push(Vec::new());
        }
        Fed::More
    }

    fn finish(incoming: Incoming) -> Complete {
        let content = match incoming.expected {
            Expected::Image { mime, .. } => Content::Image {
                mime,
                bytes: incoming.received.into_iter().next().unwrap_or_default(),
            },
            Expected::Files(files) => Content::Files(
                files
                    .into_iter()
                    .map(|(name, _, _)| name)
                    .zip(incoming.received)
                    .collect(),
            ),
        };
        Complete {
            transfer: incoming.transfer,
            session: incoming.session,
            admitted: incoming.admitted,
            content,
        }
    }

    /// Transfers whose deadline passed; they are dropped.
    pub(super) fn expire(&mut self, now: Instant) -> Vec<String> {
        let (gone, kept) = std::mem::take(&mut self.open)
            .into_iter()
            .partition(|o| o.deadline <= now);
        self.open = kept;
        gone.into_iter().map(|o: Incoming| o.transfer).collect()
    }
}

/// What a terminal forwarder hands the run loop: the next screen frame of
/// subscription `sub`, or `None` when the screen is gone.
type Followed = (u32, Option<Frame>);

/// Split one screen frame into channel pieces numbered from `*seq`. An empty
/// update yields nothing; a snapshot always yields at least one piece.
fn pieces(sub: u32, seq: &mut u64, frame: &Frame) -> Vec<TerminalFrame> {
    use base64::engine::general_purpose::STANDARD;
    use conversations_core::private_channel::MAX_TERMINAL_DATA;
    let bytes = STANDARD.decode(&frame.b64).unwrap_or_default();
    let kind = match frame.kind {
        PtyFrameKind::Snapshot => TerminalFrameKind::Snapshot,
        PtyFrameKind::Output => TerminalFrameKind::Update,
    };
    if bytes.is_empty() && kind == TerminalFrameKind::Update {
        return Vec::new();
    }
    let chunks: Vec<&[u8]> = if bytes.is_empty() {
        vec![&[]]
    } else {
        bytes.chunks(MAX_TERMINAL_DATA).collect()
    };
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let piece = TerminalFrame {
                sub,
                seq: *seq,
                kind,
                cols: frame.cols,
                rows: frame.rows,
                data: B64.encode(chunk),
                fin: i == last,
                end: None,
            };
            *seq += 1;
            piece
        })
        .collect()
}

/// The closing piece of a subscription. Carries no content.
fn end_piece(sub: u32, seq: u64, cause: TerminalEnd) -> TerminalFrame {
    TerminalFrame {
        sub,
        seq,
        kind: TerminalFrameKind::End,
        cols: 0,
        rows: 0,
        data: String::new(),
        fin: true,
        end: Some(cause),
    }
}

/// Raw bytes coalesced into one update.
const COALESCE_BYTES: usize = 16 * 1024;

/// Follow one screen for subscription `sub`: its snapshot and every later
/// update, taken atomically (`Screen::subscribe`), so nothing is missed or sent
/// twice. A viewer that falls behind gets a fresh snapshot rather than a gap.
/// Holds the screen only weakly: when the terminal goes, so does the follow.
async fn follow(
    screen: std::sync::Weak<std::sync::Mutex<Screen>>,
    sub: u32,
    out: mpsc::Sender<Followed>,
) {
    use base64::engine::general_purpose::STANDARD;
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};
    let subscribe = |screen: &std::sync::Weak<std::sync::Mutex<Screen>>| {
        screen.upgrade().map(|s| s.lock().unwrap().subscribe())
    };
    let Some((first, mut updates)) = subscribe(&screen) else {
        let _ = out.send((sub, None)).await;
        return;
    };
    if out.send((sub, Some(first))).await.is_err() {
        return;
    }
    let mut held: Option<Frame> = None;
    loop {
        let frame = match held.take() {
            Some(frame) => frame,
            None => match updates.recv().await {
                Ok(frame) => frame,
                Err(RecvError::Lagged(_)) => match subscribe(&screen) {
                    Some((snapshot, fresh)) => {
                        updates = fresh;
                        snapshot
                    }
                    None => break,
                },
                Err(RecvError::Closed) => break,
            },
        };
        let mut frame = frame;
        if frame.kind == PtyFrameKind::Output {
            let mut bytes = STANDARD.decode(&frame.b64).unwrap_or_default();
            while bytes.len() < COALESCE_BYTES {
                match updates.try_recv() {
                    Ok(next)
                        if next.kind == PtyFrameKind::Output
                            && (next.cols, next.rows) == (frame.cols, frame.rows) =>
                    {
                        bytes.extend(STANDARD.decode(&next.b64).unwrap_or_default());
                    }
                    Ok(next) => {
                        held = Some(next);
                        break;
                    }
                    Err(TryRecvError::Lagged(_)) => {
                        // Rebuilt from a fresh snapshot below; these bytes are in it.
                        bytes.clear();
                        match subscribe(&screen) {
                            Some((snapshot, fresh)) => {
                                updates = fresh;
                                held = Some(snapshot);
                            }
                            None => {
                                let _ = out.send((sub, None)).await;
                                return;
                            }
                        }
                        break;
                    }
                    Err(_) => break,
                }
            }
            if bytes.is_empty() {
                continue;
            }
            frame.b64 = STANDARD.encode(bytes);
        }
        if out.send((sub, Some(frame))).await.is_err() {
            return;
        }
    }
    let _ = out.send((sub, None)).await;
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use base64::engine::general_purpose::STANDARD;
    use conversations_core::AgentKind;

    use super::*;
    use crate::commands::local_host::authz::envelope::test_signing::Signer;
    use crate::commands::local_host::authz::sessions::LocalSession;
    use crate::commands::local_host::history::Source;

    const SESSION: &str = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";
    const OTHER: &str = "5e1aeb93-5aba-419e-8856-a7c721406279";
    const DLG: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SECRET: &str = "private-history-marker";

    fn insert(dir: &Path, session: &str) {
        Store::open(dir)
            .unwrap()
            .insert_session(
                &LocalSession {
                    session_id: session.into(),
                    agent: AgentKind::Claude,
                    cwd: format!("/w/{session}"),
                    resume_id: None,
                    launcher: Default::default(),
                    adk_statusline: false,
                },
                "signed_start",
            )
            .unwrap();
    }

    fn ingest(dir: &Path, session: &str, lines: &[serde_json::Value]) {
        let messages: Vec<_> = lines
            .iter()
            .filter_map(conversations_core::parse_line)
            .collect();
        History::open(dir)
            .unwrap()
            .ingest(
                session,
                &Source {
                    agent: AgentKind::Claude,
                    native_id: "native-1".into(),
                    origin: "/projects/x/native-1.jsonl".into(),
                },
                &messages,
            )
            .unwrap();
    }

    fn user_line(uuid: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"type":"user","uuid":uuid,"sessionId":"s",
            "timestamp":"2026-11-02T10:00:00.000Z","message":{"role":"user","content":text}})
    }

    fn assistant_line(uuid: &str, blocks: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type":"assistant","uuid":uuid,"parentUuid":"u1","sessionId":"s",
            "timestamp":"2026-11-02T10:00:01.000Z",
            "message":{"role":"assistant","model":"claude-x","content":blocks}})
    }

    /// A host directory with two local sessions (SESSION holds one private
    /// message, OTHER another) and an owner key enrolled for `grants`.
    fn host(owner: &Signer, grants: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        authz::enroll_from_cli(
            dir.path(),
            "owner",
            "ed25519",
            &STANDARD.encode(&owner.public),
            "test key",
            &grants.iter().map(|g| g.to_string()).collect::<Vec<_>>(),
        )
        .unwrap();
        insert(dir.path(), SESSION);
        insert(dir.path(), OTHER);
        ingest(dir.path(), SESSION, &[user_line("u1", SECRET)]);
        ingest(dir.path(), OTHER, &[user_line("o1", "other session")]);
        dir
    }

    fn jti() -> String {
        let mut bytes = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).unwrap();
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn op(
        dir: &Path,
        signer: &Signer,
        name: &str,
        sess: &str,
        args: &str,
        now: i64,
    ) -> SignedFrame {
        let host_id = authz::host_id(dir).unwrap();
        let payload = signer.op_payload(&host_id, name, sess, args, &jti(), now, now + 60);
        signer.sign_bytes(payload.as_bytes())
    }

    fn history_get(dir: &Path, signer: &Signer, now: i64) -> SignedFrame {
        op(
            dir,
            signer,
            "history.get",
            SESSION,
            &format!(r#"{{"session":"{SESSION}","after":0,"limit":32}}"#),
            now,
        )
    }

    /// A browser's delegated key: ES256, as WebCrypto makes it.
    struct Browser {
        pair: ring::signature::EcdsaKeyPair,
        public: Vec<u8>,
        kid: String,
    }

    impl Browser {
        fn new() -> Self {
            use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            let pair =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            let public = pair.public_key().as_ref().to_vec();
            let kid = authz::envelope::key_id(&public);
            Self { pair, public, kid }
        }

        fn op(&self, dir: &Path, name: &str, sess: &str, args: &str, now: i64) -> SignedFrame {
            let host_id = authz::host_id(dir).unwrap();
            let payload = format!(
                r#"{{"v":1,"typ":"op","aud":"{host_id}","kid":"{}","dlg":"{DLG}","op":"{name}","sess":"{sess}","jti":"{}","iat":{now},"exp":{},"args":{args}}}"#,
                self.kid,
                jti(),
                now + 60
            );
            let mut signed = authz::envelope::DOMAIN_OP.to_vec();
            signed.extend_from_slice(payload.as_bytes());
            let rng = ring::rand::SystemRandom::new();
            SignedFrame {
                payload: payload.into_bytes(),
                sig: self.pair.sign(&rng, &signed).unwrap().as_ref().to_vec(),
                kid: self.kid.clone(),
            }
        }

        fn history_get(&self, dir: &Path, now: i64) -> SignedFrame {
            self.op(
                dir,
                "history.get",
                SESSION,
                &format!(r#"{{"session":"{SESSION}","after":0,"limit":32}}"#),
                now,
            )
        }
    }

    /// Enroll `parent` as a passkey and delegate `caps` to a browser key.
    fn delegate_caps(dir: &Path, parent: &Signer, caps: &str, now: i64) -> Browser {
        let browser = Browser::new();
        let db = db(dir);
        db.execute("UPDATE credentials SET auth = 'passkey'", [])
            .unwrap();
        db.execute(
            "INSERT INTO delegations(dlg_id, parent_cred, browser_kid, browser_pub, alg, caps, issued_at, not_after)
             VALUES(?1, ?2, ?3, ?4, 'es256', ?5, ?6, ?7)",
            rusqlite::params![DLG, parent.kid, browser.kid, browser.public, caps, now, now + 600],
        )
        .unwrap();
        browser
    }

    fn delegate(dir: &Path, parent: &Signer, now: i64) -> Browser {
        delegate_caps(dir, parent, &format!("history.read:{SESSION}"), now)
    }

    fn read(dir: &Path, frame: &SignedFrame) -> ReadOutcome {
        read_now(dir, frame, &[], authz::store::now_secs()).unwrap()
    }

    fn carries_secret(body: &Option<(ReplyBody, Verified)>) -> bool {
        serde_json::to_string(&body.as_ref().map(|(b, _)| b))
            .unwrap()
            .contains(SECRET)
    }

    fn pending(outcome: &ReadOutcome) -> Pending {
        let (body, op) = outcome.2.clone().expect("a body");
        Pending {
            text: serde_json::to_string(&body).unwrap(),
            admitted: Some(op),
            transfer: None,
            subscribe: None,
        }
    }

    /// `Some(text)` when the reply was handed off, `None` when withheld.
    fn deliver(dir: &Path, reply: Pending) -> Option<String> {
        let mut store = Store::open(dir).unwrap();
        release(&mut store, reply, |text| text.to_owned()).unwrap()
    }

    fn db(dir: &Path) -> rusqlite::Connection {
        rusqlite::Connection::open(dir.join("authz.db")).unwrap()
    }

    #[test]
    fn private_pr_cache_chunks_require_current_history_authority() {
        let reader = Signer::new();
        let dir = host(&reader, &[&format!("history.read:{SESSION}")]);
        let status = crate::commands::gh::PrStatus {
            state: "OPEN".into(),
            merged: false,
            draft: false,
            title: "Cached private PR".into(),
            review_decision: None,
        };
        let comment = crate::commands::gh::PrComment {
            comment_id: "issue:1".into(),
            body: "x".repeat(90_000),
            created_at: "2026-11-05T10:00:00Z".into(),
            ..Default::default()
        };
        super::super::super::pr_metadata::save(
            dir.path(),
            SESSION,
            "octo",
            "repo",
            7,
            Some(&status),
            &[comment],
            true,
        )
        .unwrap();
        let frame = op(
            dir.path(),
            &reader,
            "pr.cache.list",
            SESSION,
            &serde_json::json!({"session":SESSION,"offset":0}).to_string(),
            authz::store::now_secs(),
        );
        let outcome = read(dir.path(), &frame);
        assert_eq!(outcome.0, OpStatus::Executed);
        let (ReplyBody::PrCache { b64, sha256, next }, _) = outcome.2.as_ref().unwrap() else {
            panic!("expected cache")
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap()
                .len(),
            24 * 1024
        );
        assert_eq!(*next, Some(24 * 1024));
        assert_eq!(sha256.len(), 43);
        assert!(
            serde_json::to_vec(&outcome.2.as_ref().unwrap().0)
                .unwrap()
                .len()
                < MAX_REPLY_PAYLOAD
        );
        let queued = pending(&outcome);
        db(dir.path())
            .execute("UPDATE grants SET revoked_at=1", [])
            .unwrap();
        assert!(deliver(dir.path(), queued).is_none());
        assert_eq!(read(dir.path(), &frame).0, OpStatus::Refused);
    }

    #[test]
    fn a_replayed_read_is_refused_once_the_history_grant_is_gone() {
        let reader = Signer::new();
        let dir = host(&reader, &[&format!("history.read:{SESSION}")]);
        let frame = history_get(dir.path(), &reader, authz::store::now_secs());
        let (status, _, body) = read(dir.path(), &frame);
        assert_eq!(status, OpStatus::Executed);
        assert!(carries_secret(&body), "the first read answers");
        let (status, _, body) = read(dir.path(), &frame);
        assert_eq!(status, OpStatus::Replayed);
        assert!(body.is_none(), "a replay never carries content");
        db(dir.path())
            .execute("UPDATE grants SET revoked_at = 1", [])
            .unwrap();
        let (status, _, body) = read(dir.path(), &frame);
        assert_eq!(
            status,
            OpStatus::Refused,
            "same jti after the grant is gone"
        );
        assert!(body.is_none());
        let journal: String = db(dir.path())
            .query_row("SELECT COALESCE(result, '') FROM op_journal", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            !journal.contains(SECRET),
            "read bodies stay out of the journal"
        );
    }

    #[test]
    fn a_replayed_delegated_read_is_refused_once_the_parent_or_delegation_is_gone() {
        for cut in [
            "UPDATE credentials SET revoked_at = 1",
            "UPDATE grants SET revoked_at = 1",
            "UPDATE delegations SET revoked_at = 1",
            "UPDATE delegations SET caps = 'session.write'",
        ] {
            let parent = Signer::new();
            let dir = host(&parent, &[&format!("history.read:{SESSION}")]);
            let now = authz::store::now_secs();
            let browser = delegate(dir.path(), &parent, now);
            let frame = browser.history_get(dir.path(), now);
            let (status, _, body) = read(dir.path(), &frame);
            assert_eq!(status, OpStatus::Executed, "{cut}");
            assert!(carries_secret(&body));
            db(dir.path()).execute(cut, []).unwrap();
            let (status, _, body) = read(dir.path(), &frame);
            assert_eq!(status, OpStatus::Refused, "{cut}");
            assert!(body.is_none(), "{cut}");
        }
    }

    #[test]
    fn a_read_and_signed_reply_is_withheld_when_authority_goes_before_hand_off() {
        let reader = Signer::new();
        let dir = host(&reader, &[&format!("history.read:{SESSION}")]);
        let outcome = read(
            dir.path(),
            &history_get(dir.path(), &reader, authz::store::now_secs()),
        );
        assert_eq!(outcome.0, OpStatus::Executed);
        assert!(deliver(dir.path(), pending(&outcome)).is_some_and(|t| t.contains(SECRET)));
        // Barrier: read and signed, not yet handed to the connection.
        db(dir.path())
            .execute("UPDATE grants SET revoked_at = 1", [])
            .unwrap();
        assert_eq!(deliver(dir.path(), pending(&outcome)), None);
        let control = Pending {
            text: "no private content".into(),
            admitted: None,
            transfer: None,
            subscribe: None,
        };
        assert!(deliver(dir.path(), control).is_some());
    }

    /// A grant removed and given again is a NEW authority: a reply read under
    /// the old one stays withheld, and only a newly signed read is answered.
    #[test]
    fn a_reply_read_before_a_remove_and_regrant_is_never_released() {
        for delegated in [false, true] {
            let reader = Signer::new();
            let dir = host(&reader, &[&format!("history.read:{SESSION}")]);
            let now = authz::store::now_secs();
            let browser = delegated.then(|| delegate(dir.path(), &reader, now));
            let get = |now| match &browser {
                Some(b) => b.history_get(dir.path(), now),
                None => history_get(dir.path(), &reader, now),
            };
            let held = read(dir.path(), &get(now));
            assert_eq!(held.0, OpStatus::Executed);
            db(dir.path())
                .execute("UPDATE grants SET revoked_at = ?1", [now])
                .unwrap();
            // Re-activate without changing the timestamp: this deterministically
            // models a remove/re-grant in the same second, without clock sleeps.
            db(dir.path())
                .execute("UPDATE grants SET revoked_at = NULL", [])
                .unwrap();
            assert_eq!(
                deliver(dir.path(), pending(&held)),
                None,
                "delegated={delegated}: the pre-removal reply was released"
            );
            let fresh = read(dir.path(), &get(authz::store::now_secs()));
            assert_eq!(fresh.0, OpStatus::Executed);
            assert!(
                deliver(dir.path(), pending(&fresh)).is_some_and(|t| t.contains(SECRET)),
                "delegated={delegated}: a new signed read works"
            );
        }
    }

    /// The final check holds the authz.db write lock across the hand-off, so a
    /// revocation committed through ANOTHER connection (as the CLI's would be)
    /// cannot land between check and hand-off: it waits, then the next reply is
    /// withheld.
    #[test]
    fn a_revocation_from_another_connection_waits_for_the_hand_off_and_then_applies() {
        let reader = Signer::new();
        let dir = host(&reader, &[&format!("history.read:{SESSION}")]);
        let first = read(
            dir.path(),
            &history_get(dir.path(), &reader, authz::store::now_secs()),
        );
        let second = read(
            dir.path(),
            &history_get(dir.path(), &reader, authz::store::now_secs()),
        );
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut store = Store::open(dir.path()).unwrap();
        let path = dir.path().to_owned();
        let mut revoker = None;
        let handed = release(&mut store, pending(&first), |text| {
            let flag = revoked.clone();
            let path = path.clone();
            revoker = Some(std::thread::spawn(move || {
                let conn = rusqlite::Connection::open(path.join("authz.db")).unwrap();
                conn.busy_timeout(Duration::from_secs(10)).unwrap();
                conn.execute("UPDATE grants SET revoked_at = 1", [])
                    .unwrap();
                flag.store(true, Ordering::SeqCst);
            }));
            std::thread::sleep(Duration::from_millis(500));
            assert!(
                !revoked.load(Ordering::SeqCst),
                "a revocation committed between the final check and the hand-off"
            );
            text.to_owned()
        })
        .unwrap();
        assert!(handed.is_some_and(|t| t.contains(SECRET)));
        revoker.unwrap().join().unwrap();
        assert!(revoked.load(Ordering::SeqCst));
        assert_eq!(
            release(&mut store, pending(&second), |t| t.to_owned()).unwrap(),
            None
        );
    }

    #[test]
    fn a_replayed_delegated_read_is_refused_after_the_delegation_expires() {
        let parent = Signer::new();
        let dir = host(&parent, &[&format!("history.read:{SESSION}")]);
        let now = authz::store::now_secs();
        let browser = delegate(dir.path(), &parent, now);
        let frame = browser.history_get(dir.path(), now);
        assert_eq!(read(dir.path(), &frame).0, OpStatus::Executed);
        db(dir.path())
            .execute("UPDATE delegations SET not_after = ?1", [now - 1])
            .unwrap();
        let (status, _, body) = read(dir.path(), &frame);
        assert_eq!(status, OpStatus::Refused);
        assert!(body.is_none());
    }

    /// History crosses the channel as complete canonical messages: structured
    /// blocks inline, a large message through hash-bound fragments, and a
    /// message rewritten in place shows up in a digest page at its old position.
    #[test]
    fn private_history_carries_complete_structured_messages_and_their_updates() {
        let reader = Signer::new();
        let dir = host(&reader, &[&format!("history.read:{SESSION}")]);
        let tool = serde_json::json!([
            {"type":"text","text":"checking"},
            {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test --workspace"}}
        ]);
        ingest(dir.path(), SESSION, &[assistant_line("a1", tool)]);
        ingest(
            dir.path(),
            SESSION,
            &[assistant_line(
                "big",
                serde_json::json!([{"type":"text","text":"y".repeat(60_000)}]),
            )],
        );
        let page = |args: &str| {
            let frame = op(
                dir.path(),
                &reader,
                "history.get",
                SESSION,
                args,
                authz::store::now_secs(),
            );
            match read(dir.path(), &frame) {
                (OpStatus::Executed, _, Some((ReplyBody::History { items, next }, _))) => {
                    (items, next)
                }
                other => panic!("{:?}", other.1),
            }
        };
        let (items, next) = page(&format!(
            r#"{{"session":"{SESSION}","after":0,"limit":32}}"#
        ));
        assert_eq!(next, None);
        assert_eq!(items.len(), 3);
        let blocks = &items[1].message.as_ref().unwrap()["blocks"];
        assert!(
            blocks
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["type"] == "tool_use"
                    && b["input"]["command"] == "cargo test --workspace"),
            "a tool call survived the channel: {blocks}"
        );
        let big = &items[2];
        assert!(big.message.is_none() && big.size > 60_000);
        let (mut offset, mut bytes) = (Some(0u64), Vec::new());
        while let Some(at) = offset {
            let frame = op(
                dir.path(),
                &reader,
                "history.fragment",
                SESSION,
                &format!(
                    r#"{{"session":"{SESSION}","seq":{},"offset":{at}}}"#,
                    big.seq
                ),
                authz::store::now_secs(),
            );
            let (
                OpStatus::Executed,
                _,
                Some((
                    ReplyBody::Fragment {
                        data, sha256, next, ..
                    },
                    _,
                )),
            ) = read(dir.path(), &frame)
            else {
                panic!("fragment refused")
            };
            assert_eq!(sha256, big.sha256);
            bytes.extend(B64.decode(data).unwrap());
            offset = next;
        }
        assert_eq!(sha256_b64(&bytes), big.sha256);
        let whole: conversations_core::Message = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(whole.text.len(), 60_000);

        // A tool result rewrites a1 in place.
        let digest = format!(r#"{{"session":"{SESSION}","after":0,"limit":256,"digest":true}}"#);
        let (before, _) = page(&digest);
        ingest(
            dir.path(),
            SESSION,
            &[assistant_line(
                "a1",
                serde_json::json!([
                    {"type":"text","text":"checking"},
                    {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test --workspace"}},
                    {"type":"text","text":"all green"}
                ]),
            )],
        );
        let (after, _) = page(&digest);
        assert!(after.iter().all(|i| i.message.is_none()));
        assert_eq!(before[1].seq, after[1].seq);
        assert_ne!(before[1].sha256, after[1].sha256);
        let (again, _) = page(&format!(
            r#"{{"session":"{SESSION}","after":{},"limit":1}}"#,
            after[1].seq - 1
        ));
        assert!(serde_json::to_string(&again[0].message)
            .unwrap()
            .contains("all green"));
    }

    fn list(dir: &Path, frame: &SignedFrame) -> Option<Vec<String>> {
        match read(dir, frame) {
            (OpStatus::Executed, _, Some((ReplyBody::Sessions { sessions, .. }, _))) => {
                Some(sessions.into_iter().map(|s| s.id).collect())
            }
            _ => None,
        }
    }

    #[test]
    fn communications_requires_owner_read_and_revocation_withholds_peer_titles() {
        let owner = Signer::new();
        let dir = host(
            &owner,
            &[
                &format!("history.read:{SESSION}"),
                &format!("history.read:{OTHER}"),
            ],
        );
        let now = authz::store::now_secs();
        let host_id = Store::open(dir.path()).unwrap().host_id().unwrap();
        let payload = serde_json::json!({"iat":now,"args":{"text":"Check result"}}).to_string();
        db(dir.path()).execute("INSERT INTO agent_sends(source_session,target_session,idem,request_sha256,attempt,message_id,payload,sig,kid,host_id,exp,state) VALUES(?1,?2,'exchange',x'00',1,'exchange',?3,x'00','fixture-key',?4,1,'uncertain')",rusqlite::params![SESSION,OTHER,payload.as_bytes(),host_id]).unwrap();
        let args = serde_json::json!({"session":SESSION,"after":null,"limit":32}).to_string();
        let response = read(
            dir.path(),
            &op(
                dir.path(),
                &owner,
                "communications.list",
                SESSION,
                &args,
                now,
            ),
        );
        assert_eq!(response.0, OpStatus::Executed);
        let ReplyBody::Communications {
            peers, exchanges, ..
        } = response.2.as_ref().unwrap().0.clone()
        else {
            panic!("wrong body")
        };
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].status, "uncertain");
        assert_eq!(peers[0].title, "other session");
        assert!(peers[0].readable);
        db(dir.path())
            .execute(
                "UPDATE grants SET revoked_at=?1 WHERE cred_id=?2 AND session_id=?3",
                rusqlite::params![now, owner.kid, OTHER],
            )
            .unwrap();
        assert!(
            deliver(dir.path(), pending(&response)).is_none(),
            "peer grant revocation must withhold a prepared title despite unchanged source grant"
        );
        let masked = read(
            dir.path(),
            &op(
                dir.path(),
                &owner,
                "communications.list",
                SESSION,
                &args,
                now,
            ),
        );
        let ReplyBody::Communications { peers, .. } = masked.2.unwrap().0 else {
            panic!("wrong body")
        };
        assert!(!peers[0].readable);
        assert_ne!(peers[0].title, "other session");
        let agent = Signer::new();
        Store::open(dir.path())
            .unwrap()
            .enroll(
                authz::store::Kind::Agent,
                authz::envelope::Alg::Ed25519,
                &agent.public,
                "Agent",
                &[authz::store::Grant::parse(&format!("history.read:{SESSION}")).unwrap()],
            )
            .unwrap();
        assert_eq!(
            read(
                dir.path(),
                &op(
                    dir.path(),
                    &agent,
                    "communications.list",
                    SESSION,
                    &args,
                    now
                )
            )
            .0,
            OpStatus::Refused
        );
        let browser = delegate_caps(dir.path(), &owner, &format!("history.read:{SESSION}"), now);
        assert_eq!(
            read(
                dir.path(),
                &browser.op(dir.path(), "communications.list", SESSION, &args, now)
            )
            .0,
            OpStatus::Executed
        );
        let other = serde_json::json!({"session":OTHER,"after":null,"limit":32}).to_string();
        assert_eq!(
            read(
                dir.path(),
                &browser.op(dir.path(), "communications.list", OTHER, &other, now)
            )
            .0,
            OpStatus::Refused
        );
    }

    #[test]
    fn attention_signed_send_and_inbox_are_separate_scoped_and_revocable() {
        let owner = Signer::new();
        let recipient = Signer::new();
        let dir = host(&owner, &["session.write", "history.read"]);
        let now = authz::store::now_secs();
        let mut store = Store::open(dir.path()).unwrap();
        store
            .enroll(
                authz::store::Kind::Owner,
                authz::envelope::Alg::Ed25519,
                &recipient.public,
                "Reader",
                &[authz::store::Grant::parse(&format!("history.read:{SESSION}")).unwrap()],
            )
            .unwrap();
        let args=serde_json::json!({"session":SESSION,"recipient":{"kind":"credential","kid":recipient.kid},"message":"Please inspect","idempotency_key":uuid::Uuid::new_v4().to_string()}).to_string();
        for _ in 0..2 {
            assert!(matches!(
                authz::dispatch::decide(
                    dir.path(),
                    &op(dir.path(), &owner, "attention.request", SESSION, &args, now),
                    now
                )
                .unwrap(),
                Decision::Applied(_)
            ));
        }
        assert_eq!(
            db(dir.path())
                .query_row("SELECT count(*) FROM human_notifications", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db(dir.path())
                .query_row("SELECT count(*) FROM agent_inbox", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &op(
                    dir.path(),
                    &recipient,
                    "attention.request",
                    SESSION,
                    &args,
                    now
                ),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        db(dir.path()).execute_batch("CREATE TRIGGER fail_attention_journal BEFORE INSERT ON op_journal BEGIN SELECT RAISE(ABORT,'fixture journal failure'); END;").unwrap();
        let mut atomic: serde_json::Value = serde_json::from_str(&args).unwrap();
        atomic["idempotency_key"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
        assert!(authz::dispatch::decide(
            dir.path(),
            &op(
                dir.path(),
                &owner,
                "attention.request",
                SESSION,
                &atomic.to_string(),
                now
            ),
            now
        )
        .is_err());
        db(dir.path())
            .execute_batch("DROP TRIGGER fail_attention_journal;")
            .unwrap();
        assert_eq!(
            db(dir.path())
                .query_row("SELECT count(*) FROM human_notifications", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1,
            "notification must roll back when its operation journal fails"
        );
        let agent = Signer::new();
        store
            .enroll(
                authz::store::Kind::Agent,
                authz::envelope::Alg::Ed25519,
                &agent.public,
                "Agent",
                &[authz::store::Grant::parse(&format!("session.write:{SESSION}")).unwrap()],
            )
            .unwrap();
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &op(dir.path(), &agent, "attention.request", SESSION, &args, now),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        let browser = delegate_caps(dir.path(), &owner, &format!("session.write:{SESSION}"), now);
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &browser.op(dir.path(), "attention.request", SESSION, &args, now),
                now
            )
            .unwrap(),
            Decision::Applied(_)
        ));
        assert_eq!(
            db(dir.path())
                .query_row("SELECT count(*) FROM human_notifications", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1,
            "browser must derive parent identity for idempotency"
        );
        let mut outside: serde_json::Value = serde_json::from_str(&args).unwrap();
        outside["session"] = serde_json::json!(OTHER);
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &browser.op(
                    dir.path(),
                    "attention.request",
                    OTHER,
                    &outside.to_string(),
                    now
                ),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        db(dir.path())
            .execute("UPDATE credentials SET auth='raw_key'", [])
            .unwrap();
        let listargs = r#"{"after":null,"limit":20}"#;
        let received = read(
            dir.path(),
            &op(
                dir.path(),
                &recipient,
                "notifications.list",
                "host",
                listargs,
                now,
            ),
        );
        assert_eq!(received.0, OpStatus::Executed);
        let ReplyBody::Notifications { notifications, .. } = received.2.as_ref().unwrap().0.clone()
        else {
            panic!("wrong body")
        };
        assert_eq!(notifications.len(), 1);
        assert_eq!(
            notifications[0].sender.subject,
            conversations_core::attention::Subject::Credential {
                kid: owner.kid.clone()
            }
        );
        let wrong = read(
            dir.path(),
            &op(
                dir.path(),
                &owner,
                "notifications.list",
                "host",
                listargs,
                now,
            ),
        );
        let ReplyBody::Notifications {
            notifications: wrong,
            ..
        } = wrong.2.unwrap().0
        else {
            panic!("wrong body")
        };
        assert!(wrong.is_empty());
        let mark = serde_json::json!({"session":SESSION,"id":notifications[0].id}).to_string();
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &op(
                    dir.path(),
                    &owner,
                    "notifications.resolve",
                    SESSION,
                    &mark,
                    now
                ),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &op(
                    dir.path(),
                    &recipient,
                    "notifications.resolve",
                    SESSION,
                    &mark,
                    now
                ),
                now
            )
            .unwrap(),
            Decision::Applied(_)
        ));
        db(dir.path())
            .execute(
                "UPDATE grants SET revoked_at=?1 WHERE cred_id=?2",
                rusqlite::params![now, recipient.kid],
            )
            .unwrap();
        assert!(deliver(dir.path(), pending(&received)).is_none());
        let revoked = read(
            dir.path(),
            &op(
                dir.path(),
                &recipient,
                "notifications.list",
                "host",
                listargs,
                now,
            ),
        );
        let ReplyBody::Notifications { notifications, .. } = revoked.2.unwrap().0 else {
            panic!("wrong body")
        };
        assert!(notifications.is_empty());
    }

    #[test]
    fn linear_read_is_session_scoped_and_unlink_requires_current_owner_write() {
        let owner = Signer::new();
        let dir = host(&owner, &["history.read", "session.write"]);
        let now = authz::store::now_secs();
        ingest(
            dir.path(),
            SESSION,
            &[user_line("linear", "https://linear.app/ws/issue/ENG-1")],
        );
        let args = serde_json::json!({"session":SESSION,"after":null,"limit":32}).to_string();
        let (status, _, response) = read(
            dir.path(),
            &op(dir.path(), &owner, "linear.list", SESSION, &args, now),
        );
        assert_eq!(status, OpStatus::Executed);
        let (body, admitted) = response.unwrap();
        assert_eq!(
            serde_json::to_value(body).unwrap()["docs"][0]["identifier"],
            "ENG-1"
        );
        db(dir.path())
            .execute(
                "UPDATE grants SET revoked_at=?1 WHERE cap='history.read'",
                [now],
            )
            .unwrap();
        assert!(Store::open(dir.path())
            .unwrap()
            .with_current_authority(&admitted, now, || ())
            .unwrap()
            .is_none());
        let other = serde_json::json!({"session":OTHER,"after":null,"limit":32}).to_string();
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &op(dir.path(), &owner, "linear.list", SESSION, &other, now),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        let unlink_args =
            serde_json::json!({"session":SESSION,"url":"https://linear.app/ws/issue/ENG-1"})
                .to_string();
        let Decision::Execute(unlink) = authz::dispatch::decide(
            dir.path(),
            &op(
                dir.path(),
                &owner,
                "linear.unlink",
                SESSION,
                &unlink_args,
                now,
            ),
            now,
        )
        .unwrap() else {
            panic!("owner unlink denied");
        };
        crate::commands::local_host::linear_metadata::unlink_signed(dir.path(), &unlink).unwrap();
        assert!(History::open(dir.path())
            .unwrap()
            .linear_page(SESSION, None, 32)
            .unwrap()
            .0
            .is_empty());
        let read_only = host(&owner, &["history.read"]);
        assert!(matches!(
            authz::dispatch::decide(
                read_only.path(),
                &op(
                    read_only.path(),
                    &owner,
                    "linear.unlink",
                    SESSION,
                    &unlink_args,
                    now
                ),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        let scoped = host(&owner, &[&format!("history.read:{SESSION}")]);
        assert!(matches!(
            authz::dispatch::decide(
                scoped.path(),
                &op(scoped.path(), &owner, "linear.list", OTHER, &other, now),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        ingest(
            dir.path(),
            SESSION,
            &[user_line("linear2", "https://linear.app/ws/issue/ENG-2")],
        );
        let browser = delegate_caps(dir.path(), &owner, &format!("session.write:{SESSION}"), now);
        let args = serde_json::json!({"session":SESSION,"url":"https://linear.app/ws/issue/ENG-2"})
            .to_string();
        let Decision::Execute(unlink) = authz::dispatch::decide(
            dir.path(),
            &browser.op(dir.path(), "linear.unlink", SESSION, &args, now),
            now,
        )
        .unwrap() else {
            panic!("delegated owner unlink denied");
        };
        db(dir.path())
            .execute(
                "UPDATE delegations SET revoked_at=?1 WHERE dlg_id=?2",
                rusqlite::params![now, DLG],
            )
            .unwrap();
        assert!(
            crate::commands::local_host::linear_metadata::unlink_signed(dir.path(), &unlink)
                .is_err()
        );
        assert_eq!(
            History::open(dir.path())
                .unwrap()
                .linear_page(SESSION, None, 32)
                .unwrap()
                .0[0]
                .identifier,
            "ENG-2"
        );
        db(dir.path())
            .execute("UPDATE credentials SET kind='agent',auth='raw_key'", [])
            .unwrap();
        assert!(
            matches!(authz::dispatch::decide(dir.path(),&op(dir.path(),&owner,"linear.unlink",SESSION,&args,now),now).unwrap(),Decision::Refused(reason) if reason.contains("owner"))
        );
    }

    #[test]
    fn access_list_requires_wildcard_owner_authority_and_rechecks_before_delivery() {
        let owner = Signer::new();
        let dir = host(&owner, &["session.write"]);
        let now = authz::store::now_secs();
        let frame = op(dir.path(), &owner, "access.list", "host", "{}", now);
        let (status, _, response) = read(dir.path(), &frame);
        assert_eq!(
            status,
            OpStatus::Executed,
            "approved global owner can inspect enrolled keys"
        );
        let (body, admitted) = response.unwrap();
        let body = serde_json::to_value(body).unwrap();
        assert_eq!(body["kind"], "access");
        assert_eq!(
            body["credentials"][0]["kid"],
            authz::envelope::key_id(&owner.public)
        );
        assert_eq!(body["credentials"][0]["label"], "test key");
        assert_eq!(
            body["credentials"][0]["grants"][0],
            serde_json::json!({"cap":"session.write","session":"*"})
        );
        assert!(body["credentials"][0].get("public_key").is_none());
        assert!(body["credentials"][0].get("private_key").is_none());
        db(dir.path())
            .execute("UPDATE grants SET revoked_at=?1", [now])
            .unwrap();
        assert!(Store::open(dir.path())
            .unwrap()
            .with_current_authority(&admitted, now, || ())
            .unwrap()
            .is_none());
        for grants in [
            &["history.read"][..],
            &["ceiling:session.write"][..],
            &["session.write:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"][..],
        ] {
            let restricted = host(&owner, grants);
            assert!(matches!(
                authz::dispatch::decide(
                    restricted.path(),
                    &op(restricted.path(), &owner, "access.list", "host", "{}", now),
                    now
                )
                .unwrap(),
                Decision::Refused(_)
            ));
        }
        let delegated = host(&owner, &["session.write"]);
        let browser = delegate_caps(
            delegated.path(),
            &owner,
            &format!("session.write:{SESSION}"),
            now,
        );
        assert!(matches!(
            authz::dispatch::decide(
                delegated.path(),
                &browser.op(delegated.path(), "access.list", "host", "{}", now),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        db(delegated.path())
            .execute("UPDATE delegations SET caps='session.write'", [])
            .unwrap();
        assert!(matches!(
            read(
                delegated.path(),
                &browser.op(delegated.path(), "access.list", "host", "{}", now)
            ),
            (OpStatus::Executed, _, Some(_))
        ));
        db(delegated.path())
            .execute("UPDATE credentials SET kind='agent',auth='raw_key'", [])
            .unwrap();
        assert!(
            matches!(authz::dispatch::decide(delegated.path(), &op(delegated.path(), &owner,"access.list","host","{}",now),now).unwrap(), Decision::Refused(reason) if reason.contains("owner"))
        );
    }

    #[test]
    fn project_operations_require_explicit_global_or_matching_session_owner_authority() {
        let owner = Signer::new();
        let dir = host(&owner, &["session.write"]);
        let now = authz::store::now_secs();
        let frame = op(dir.path(), &owner, "projects.list", "host", "{}", now);
        assert!(
            matches!(read(dir.path(), &frame), (OpStatus::Executed, _, Some(_))),
            "host-wide owner can list project metadata"
        );
        let narrow = delegate_caps(dir.path(), &owner, &format!("session.write:{SESSION}"), now);
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &narrow.op(
                    dir.path(),
                    "projects.create",
                    "host",
                    r#"{"name":"Private project"}"#,
                    now
                ),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        let args = serde_json::json!({"session":SESSION}).to_string();
        assert!(matches!(
            read(
                dir.path(),
                &narrow.op(dir.path(), "session.project.get", SESSION, &args, now)
            ),
            (OpStatus::Executed, _, Some(_))
        ));
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &narrow.op(
                    dir.path(),
                    "session.project.get",
                    OTHER,
                    &serde_json::json!({"session":OTHER}).to_string(),
                    now
                ),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        assert!(
            matches!(
                authz::dispatch::decide(
                    dir.path(),
                    &op(dir.path(), &owner, "projects.list", "*", "{}", now),
                    now
                )
                .unwrap(),
                Decision::Refused(_)
            ),
            "grant wildcard is not the host wire target"
        );
    }

    #[test]
    fn signed_projects_share_owner_storage_and_recheck_revocation_without_touching_history() {
        use crate::commands::local_host::projects::{self, Projects};
        let owner = Signer::new();
        let dir = host(&owner, &["session.write"]);
        let now = authz::store::now_secs();
        let admit = |name: &str, target: &str, args: &str| {
            let decision = authz::dispatch::decide(
                dir.path(),
                &op(dir.path(), &owner, name, target, args, now),
                now,
            )
            .unwrap();
            match decision {
                Decision::Execute(op) => op,
                other => panic!("project {name} not admitted: {other:?}"),
            }
        };
        let created = admit(
            "projects.create",
            "host",
            r#"{"name":"Shared local metadata","description":null}"#,
        );
        let project: conversations_core::Project =
            serde_json::from_str(&projects::execute(dir.path(), &created).unwrap()).unwrap();
        authz::dispatch::finish(dir.path(), &created, &Ok(())).unwrap();
        let assignment = admit(
            "session.project.set",
            SESSION,
            &serde_json::json!({"session":SESSION,"project_id":project.id}).to_string(),
        );
        projects::execute(dir.path(), &assignment).unwrap();
        authz::dispatch::finish(dir.path(), &assignment, &Ok(())).unwrap();
        assert_eq!(
            Projects::open(dir.path())
                .unwrap()
                .membership(SESSION)
                .unwrap()
                .as_deref(),
            Some(project.id.as_str())
        );
        assert_eq!(
            Projects::open(dir.path()).unwrap().list().unwrap()[0].session_count,
            1
        );
        let invalid = admit(
            "session.project.set",
            SESSION,
            &serde_json::json!({"session":SESSION,"project_id":OTHER}).to_string(),
        );
        assert!(
            projects::execute(dir.path(), &invalid).is_err(),
            "unknown project is not created by assignment"
        );
        let browser = delegate_caps(dir.path(), &owner, "session.write", now);
        assert!(
            matches!(read(dir.path(), &browser.op(dir.path(), "projects.list", "host", "{}", now)), (OpStatus::Executed, _, Some((ReplyBody::Projects{projects}, _))) if projects.len()==1)
        );
        let Decision::Execute(revoked) = authz::dispatch::decide(
            dir.path(),
            &browser.op(
                dir.path(),
                "session.project.set",
                SESSION,
                &serde_json::json!({"session":SESSION,"project_id":null}).to_string(),
                now,
            ),
            now,
        )
        .unwrap() else {
            panic!("delegated owner assignment refused");
        };
        db(dir.path())
            .execute(
                "UPDATE delegations SET revoked_at=?1 WHERE dlg_id=?2",
                rusqlite::params![now, DLG],
            )
            .unwrap();
        assert!(projects::execute(dir.path(), &revoked).is_err());
        assert_eq!(
            Projects::open(dir.path())
                .unwrap()
                .membership(SESSION)
                .unwrap()
                .as_deref(),
            Some(project.id.as_str())
        );
        assert_eq!(
            History::open(dir.path())
                .unwrap()
                .summary(SESSION)
                .unwrap()
                .messages,
            1
        );
        for grants in [
            &["ceiling:session.write"][..],
            &["history.read"][..],
            &["session.write:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"][..],
        ] {
            let restricted = host(&owner, grants);
            assert!(matches!(
                authz::dispatch::decide(
                    restricted.path(),
                    &op(
                        restricted.path(),
                        &owner,
                        "projects.list",
                        "host",
                        "{}",
                        now
                    ),
                    now
                )
                .unwrap(),
                Decision::Refused(_)
            ));
        }
        db(dir.path())
            .execute("UPDATE credentials SET kind='agent',auth='raw_key'", [])
            .unwrap();
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &op(
                    dir.path(),
                    &owner,
                    "session.project.get",
                    SESSION,
                    &serde_json::json!({"session":SESSION}).to_string(),
                    now
                ),
                now
            )
            .unwrap(),
            Decision::Refused(reason) if reason.contains("owner")
        ));
    }

    #[test]
    fn rename_delegation_retains_session_scope_and_is_rechecked_before_effect() {
        let owner = Signer::new();
        let dir = host(&owner, &["session.write"]);
        let now = authz::store::now_secs();
        let browser = delegate_caps(dir.path(), &owner, &format!("session.write:{SESSION}"), now);
        let args = |id: &str| serde_json::json!({"session":id,"title":"Renamed"}).to_string();
        assert!(matches!(
            authz::dispatch::decide(
                dir.path(),
                &browser.op(dir.path(), "session.rename", OTHER, &args(OTHER), now),
                now
            )
            .unwrap(),
            Decision::Refused(_)
        ));
        let accepted = authz::dispatch::decide(
            dir.path(),
            &browser.op(dir.path(), "session.rename", SESSION, &args(SESSION), now),
            now,
        )
        .unwrap();
        let Decision::Execute(op) = accepted else {
            panic!("valid owner delegation refused")
        };
        db(dir.path())
            .execute(
                "UPDATE delegations SET revoked_at=?1 WHERE dlg_id=?2",
                rusqlite::params![now, DLG],
            )
            .unwrap();
        assert!(
            authz::dispatch::begin(dir.path(), &op).is_err(),
            "a revoked delegation cannot mutate history after admission"
        );
    }

    #[test]
    fn running_only_skips_large_archive_and_retains_per_session_authorization() {
        let reader = Signer::new();
        let dir = host(&reader, &["history.read"]);
        let mut store = Store::open(dir.path()).unwrap();
        for n in 0..2429 {
            store
                .insert_session(
                    &LocalSession {
                        session_id: format!("10000000-0000-0000-0000-{n:012}"),
                        agent: AgentKind::Claude,
                        cwd: "/archive".into(),
                        resume_id: None,
                        launcher: Default::default(),
                        adk_statusline: false,
                    },
                    "adopted",
                )
                .unwrap();
        }
        let running: Vec<_> = (0..10)
            .map(|n| format!("20000000-0000-0000-0000-{n:012}"))
            .collect();
        for id in &running {
            insert(dir.path(), id);
        }
        let now = authz::store::now_secs();
        let request = op(
            dir.path(),
            &reader,
            "sessions.list",
            "host",
            r#"{"after":null,"limit":50,"running_only":true}"#,
            now,
        );
        let (_, _, body) = read_now(dir.path(), &request, &running, now).unwrap();
        let Some((ReplyBody::Sessions { sessions, next }, _)) = body else {
            panic!("running-only listing refused")
        };
        assert_eq!(sessions.len(), 10);
        assert!(next.is_none());
        assert!(sessions
            .iter()
            .all(|s| s.running && running.contains(&s.id)));
        let narrow = Signer::new();
        authz::enroll_from_cli(
            dir.path(),
            "owner",
            "ed25519",
            &STANDARD.encode(&narrow.public),
            "narrow",
            &[format!("history.read:{}", running[3])],
        )
        .unwrap();
        let request = op(
            dir.path(),
            &narrow,
            "sessions.list",
            "host",
            r#"{"after":null,"limit":50,"running_only":true}"#,
            now,
        );
        let (_, _, body) = read_now(dir.path(), &request, &running, now).unwrap();
        let Some((ReplyBody::Sessions { sessions, next }, _)) = body else {
            panic!("narrow listing refused")
        };
        assert_eq!(
            sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
            vec![&running[3]]
        );
        assert!(next.is_none());
        let request = op(
            dir.path(),
            &reader,
            "sessions.list",
            "host",
            r#"{"after":null,"limit":50,"running_only":true}"#,
            now,
        );
        let (_, _, body) = read_now(dir.path(), &request, &[], now).unwrap();
        assert!(
            matches!(body,Some((ReplyBody::Sessions{sessions,next:None},_)) if sessions.is_empty())
        );
    }

    #[test]
    fn discovered_prs_are_returned_only_for_readable_sessions() {
        let reader = Signer::new();
        let dir = host(&reader, &[&format!("history.read:{SESSION}")]);
        let history = History::open(dir.path()).unwrap();
        history
            .remember_prs(SESSION, &["https://github.com/acme/visible/pull/1".into()])
            .unwrap();
        history
            .remember_prs(OTHER, &["https://github.com/acme/secret/pull/2".into()])
            .unwrap();
        let frame = op(
            dir.path(),
            &reader,
            "sessions.list",
            "host",
            r#"{"after":null,"limit":50}"#,
            authz::store::now_secs(),
        );
        let (_, _, Some((ReplyBody::Sessions { sessions, .. }, _))) = read(dir.path(), &frame)
        else {
            panic!("read refused")
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, SESSION);
        assert_eq!(sessions[0].prs, ["https://github.com/acme/visible/pull/1"]);
    }

    #[test]
    fn readiness_is_only_returned_to_a_client_that_requested_it() {
        use conversations_core::private_channel::AgentReadiness;
        let owner = Signer::new();
        let dir = host(&owner, &[&format!("history.read:{SESSION}")]);
        for include in [false, true] {
            let args = if include {
                r#"{"after":null,"limit":50,"readiness":true}"#
            } else {
                r#"{"after":null,"limit":50}"#
            };
            let frame = op(
                dir.path(),
                &owner,
                "sessions.list",
                "host",
                args,
                authz::store::now_secs(),
            );
            let (_, _, Some((ReplyBody::Sessions { mut sessions, .. }, admitted))) =
                read(dir.path(), &frame)
            else {
                panic!("listing refused");
            };
            super::attach_readiness(
                &mut sessions,
                &admitted,
                &std::collections::HashMap::from([(SESSION.to_owned(), AgentReadiness::Working)]),
            );
            let wire = serde_json::to_value(&sessions[0]).unwrap();
            assert_eq!(
                wire.get("readiness").and_then(serde_json::Value::as_str),
                include.then_some("working")
            );
        }
    }

    /// The listing is filtered per session by the signer's CURRENT history.read
    /// authority; it never needs or invents a host-wide grant.
    #[test]
    fn the_private_session_list_shows_only_sessions_the_signer_may_read() {
        let args = r#"{"after":null,"limit":50}"#;
        let one = Signer::new();
        let dir = host(&one, &[&format!("history.read:{SESSION}")]);
        let now = authz::store::now_secs();
        assert_eq!(
            list(
                dir.path(),
                &op(dir.path(), &one, "sessions.list", "host", args, now)
            ),
            Some(vec![SESSION.to_owned()])
        );

        let agent = Signer::new();
        authz::enroll_from_cli(
            dir.path(),
            "agent",
            "ed25519",
            &STANDARD.encode(&agent.public),
            "session reader",
            &[format!("history.read:{SESSION}")],
        )
        .unwrap();
        assert_eq!(
            list(
                dir.path(),
                &op(dir.path(), &agent, "sessions.list", "host", args, now)
            ),
            Some(vec![SESSION.to_owned()]),
            "an agent's narrow grant must not expose the other session"
        );

        let all = Signer::new();
        let dir = host(&all, &["history.read"]);
        let mut both = list(
            dir.path(),
            &op(dir.path(), &all, "sessions.list", "host", args, now),
        )
        .unwrap();
        both.sort();
        assert_eq!(both, {
            let mut v = vec![SESSION.to_owned(), OTHER.to_owned()];
            v.sort();
            v
        });
        // A delegation narrowed to one session lists only that session.
        let browser = delegate(dir.path(), &all, now);
        assert_eq!(
            list(
                dir.path(),
                &browser.op(dir.path(), "sessions.list", "host", args, now)
            ),
            Some(vec![SESSION.to_owned()])
        );

        let writer = Signer::new();
        let dir = host(&writer, &[&format!("session.write:{SESSION}")]);
        assert_eq!(
            list(
                dir.path(),
                &op(dir.path(), &writer, "sessions.list", "host", args, now)
            ),
            None,
            "a session.write-only key lists nothing"
        );
    }

    #[test]
    fn host_settings_are_read_only_on_the_channel_and_carry_the_launch_block() {
        let owner = Signer::new();
        let dir = host(&owner, &["host.settings_read"]);
        let now = authz::store::now_secs();
        let frame = op(dir.path(), &owner, "settings.get", "host", "{}", now);
        let (status, _, body) = read(dir.path(), &frame);
        assert_eq!(status, OpStatus::Executed);
        let Some((ReplyBody::Settings { launch }, _)) = body else {
            panic!("no settings body")
        };
        assert_eq!(launch["claude_skip_permissions"], false);
        assert!(launch.get("origins").is_none() && launch.get("rp_id").is_none());
        let reader = Signer::new();
        let dir = host(&reader, &[&format!("history.read:{SESSION}")]);
        let frame = op(dir.path(), &reader, "settings.get", "host", "{}", now);
        assert_eq!(read(dir.path(), &frame).0, OpStatus::Refused);
    }

    fn hex(bytes: &[u8]) -> String {
        crate::commands::local_host::transfers::sha256_hex(bytes)
    }

    /// An admitted `session.send_files` for `files` (name, bytes).
    fn send_files(
        dir: &Path,
        writer: &Signer,
        transfer: &str,
        files: &[(&str, &[u8])],
    ) -> Verified {
        let list: Vec<_> = files
            .iter()
            .map(|(name, bytes)| {
                serde_json::json!({"name": name, "size": bytes.len(), "sha256": hex(bytes)})
            })
            .collect();
        let args = serde_json::json!({"session": SESSION, "transfer": transfer, "files": list});
        let frame = op(
            dir,
            writer,
            "session.send_files",
            SESSION,
            &args.to_string(),
            authz::store::now_secs(),
        );
        match authz::dispatch::decide(dir, &frame, authz::store::now_secs()).unwrap() {
            Decision::Execute(op) => op,
            _ => panic!("send_files not admitted"),
        }
    }

    fn completed(dir: &Path, writer: &Signer, transfer: &str, bytes: &[u8]) -> Complete {
        let admitted = send_files(dir, writer, transfer, &[("notes.txt", bytes)]);
        let mut inbound = Inbound::default();
        assert!(inbound.admit(&admitted, later()).unwrap().is_none());
        match feed_all(&mut inbound, transfer, 0, bytes) {
            Some(Fed::Complete(done)) => *done,
            _ => panic!("transfer not complete"),
        }
    }

    #[test]
    fn a_complete_transfer_reaches_its_terminal_only_under_the_exact_admitted_authority() {
        let (writer, dir) = writer_host();
        let done = completed(dir.path(), &writer, T1, b"private bytes");
        let mut store = Store::open(dir.path()).unwrap();
        let mut sent = Vec::new();
        let status = forward_to(
            dir.path(),
            &Connection::new(),
            &mut store,
            done,
            |message| {
                sent.push(message);
                Ok(())
            },
        );
        assert!(status.is_ok());
        forget_receipt(dir.path(), T1);
        match sent.as_slice() {
            [super::super::ServerMessage::TransferContent { transfer, content }] => {
                assert_eq!(transfer, T1);
                let text = serde_json::to_string(content).unwrap();
                assert!(text.contains("notes.txt"));
                assert!(text.contains(&STANDARD.encode(b"private bytes")));
            }
            other => panic!("unexpected hand-off: {}", other.len()),
        }

        // Removed and re-granted after the bytes arrived: the old admission
        // never revives, and nothing reaches the terminal.
        let done = completed(dir.path(), &writer, T2, b"more bytes");
        let now = authz::store::now_secs();
        db(dir.path())
            .execute("UPDATE grants SET revoked_at = ?1", [now])
            .unwrap();
        db(dir.path())
            .execute("UPDATE grants SET revoked_at = NULL", [])
            .unwrap();
        let mut called = false;
        let status = forward_to(dir.path(), &Connection::new(), &mut store, done, |_| {
            called = true;
            Ok(())
        });
        assert_eq!(status.err(), Some(TransferStatus::Refused));
        assert!(!called);
    }

    #[test]
    fn a_failed_hand_off_is_reported_as_failed_not_delivered() {
        let (writer, dir) = writer_host();
        let done = completed(dir.path(), &writer, T1, b"bytes");
        let mut store = Store::open(dir.path()).unwrap();
        let status = forward_to(dir.path(), &Connection::new(), &mut store, done, |_| {
            anyhow::bail!("queue full")
        });
        assert_eq!(status.err(), Some(TransferStatus::Failed));
    }

    fn connection(screen: &SharedScreen) -> Connection {
        Arc::downgrade(screen)
    }

    /// `SESSION`'s terminal, connected with `screen`, as a wrapper leaves it.
    fn attached(
        screen: &SharedScreen,
    ) -> (
        std::sync::Mutex<std::collections::HashMap<String, super::super::Session>>,
        tokio::sync::mpsc::Receiver<super::super::ServerMessage>,
    ) {
        let (commands, rx) = tokio::sync::mpsc::channel(4);
        let session = super::super::Session {
            screen: Some(screen.clone()),
            readiness: Default::default(),
            local_input_bytes: 0,
            local_input_dropped_chunks: 0,
            registration: super::super::Registration {
                launch: super::super::Launch {
                    id: SESSION.into(),
                    agent: conversations_core::AgentKind::Claude,
                    cwd: "/work".into(),
                    command: vec!["claude".into()],
                },
                workspace: None,
                surface: None,
                local_terminal: true,
                reconnecting: false,
                file_receiver: true,
                transfer_gate: true,
                effect_gate: true,
            },
            resume_published: None,
            opencode_title_published: None,
            commands,
            pid: None,
        };
        let sessions = std::sync::Mutex::new([(SESSION.to_owned(), session)].into());
        (sessions, rx)
    }

    #[tokio::test]
    async fn a_transfer_bound_to_a_replaced_connection_never_reaches_its_replacement() {
        const T3: &str = "00000000000000000000000000000003";
        let (writer, dir) = writer_host();
        let (old, replacement) = (Screen::new(80, 24), Screen::new(80, 24));
        let mut store = Store::open(dir.path()).unwrap();

        // Bound to `old`, but `replacement` reconnected before the hand-off:
        // its receipt could never settle the wait, so it must not get the bytes.
        let (sessions, mut rx) = attached(&replacement);
        let status = forward_to(
            dir.path(),
            &connection(&old),
            &mut store,
            completed(dir.path(), &writer, T1, b"a"),
            |message| enqueue_on(&sessions, SESSION, &connection(&old), message),
        );
        assert_eq!(status.err(), Some(TransferStatus::Failed));
        assert!(
            rx.try_recv().is_err(),
            "the replacement was handed the transfer"
        );
        assert!(!receipts()
            .lock()
            .unwrap()
            .contains_key(&(dir.path().to_owned(), T1.to_owned())));

        // The connection it was bound to gets it. Replaced AFTER the hand-off,
        // the old connection's end leaves it uncertain; the replacement's
        // receipt settles nothing.
        let bound = connection(&replacement);
        let receipt = forward_to(
            dir.path(),
            &bound,
            &mut store,
            completed(dir.path(), &writer, T2, b"b"),
            |message| enqueue_on(&sessions, SESSION, &bound, message),
        )
        .unwrap();
        assert!(matches!(
            rx.try_recv(),
            Ok(super::super::ServerMessage::TransferContent { .. })
        ));
        let newer = Screen::new(80, 24);
        transfer_receipt(dir.path(), SESSION, &newer, T2, TerminalFileStatus::Sent);
        abandon_receipts(dir.path(), SESSION, &replacement);
        assert_eq!(
            outcome(dir.path(), T2, receipt, Duration::from_secs(5)).await,
            TransferStatus::Uncertain
        );

        // While a transfer is bound to a connection whose screen is gone, no
        // later screen can take its address and pass for it.
        let receipt = forward_to(
            dir.path(),
            &bound,
            &mut store,
            completed(dir.path(), &writer, T3, b"c"),
            |_| Ok(()),
        )
        .unwrap();
        drop((sessions, replacement));
        assert!(bound.upgrade().is_none());
        let later: Vec<_> = (0..256).map(|_| Screen::new(80, 24)).collect();
        assert!(!later.iter().any(|screen| is(&bound, screen)));
        for screen in &later {
            transfer_receipt(dir.path(), SESSION, screen, T3, TerminalFileStatus::Sent);
        }
        assert_eq!(
            outcome(dir.path(), T3, receipt, Duration::from_millis(50)).await,
            TransferStatus::Uncertain
        );
    }

    #[tokio::test]
    async fn a_transfer_is_delivered_only_on_the_receiving_connection_s_success_receipt() {
        use conversations_core::terminal_files::TerminalFileStatus::{Failed, Sent};
        let (writer, dir) = writer_host();
        let (old, current, stranger) = (
            Screen::new(80, 24),
            Screen::new(80, 24),
            Screen::new(80, 24),
        );
        let mut store = Store::open(dir.path()).unwrap();
        let wait = Duration::from_millis(200);
        let handed = |store: &mut Store, transfer, bytes: &[u8]| {
            forward_to(
                dir.path(),
                &connection(&current),
                store,
                completed(dir.path(), &writer, transfer, bytes),
                |_| Ok(()),
            )
            .unwrap()
        };

        let receipt = handed(&mut store, T1, b"a");
        // Another session's terminal, an unrelated connection, or an earlier
        // connection of the same session cannot settle it.
        transfer_receipt(dir.path(), OTHER, &current, T1, Sent);
        transfer_receipt(dir.path(), SESSION, &stranger, T1, Sent);
        transfer_receipt(dir.path(), SESSION, &old, T1, Sent);
        transfer_receipt(dir.path(), SESSION, &current, T1, Sent);
        assert_eq!(
            outcome(dir.path(), T1, receipt, wait).await,
            TransferStatus::Delivered
        );

        let receipt = handed(&mut store, T2, b"b");
        transfer_receipt(dir.path(), SESSION, &old, T2, Sent);
        assert_eq!(
            outcome(dir.path(), T2, receipt, wait).await,
            TransferStatus::Uncertain,
            "a stale connection's receipt settled the wait"
        );

        const T3: &str = "00000000000000000000000000000003";
        let receipt = handed(&mut store, T3, b"c");
        transfer_receipt(dir.path(), SESSION, &current, T3, Failed);
        assert_eq!(
            outcome(dir.path(), T3, receipt, wait).await,
            TransferStatus::Failed
        );
        assert!(!receipts()
            .lock()
            .unwrap()
            .keys()
            .any(|(d, _)| d == dir.path()));
    }

    #[tokio::test]
    async fn a_lost_or_late_receipt_is_uncertain_never_delivered() {
        let (writer, dir) = writer_host();
        let (ended, replacement) = (Screen::new(80, 24), Screen::new(80, 24));
        let mut store = Store::open(dir.path()).unwrap();

        // The receiving connection ended before it answered; a replacement
        // connection ending abandons only its own waits.
        let receipt = forward_to(
            dir.path(),
            &connection(&ended),
            &mut store,
            completed(dir.path(), &writer, T1, b"a"),
            |_| Ok(()),
        )
        .unwrap();
        abandon_receipts(dir.path(), SESSION, &replacement);
        assert!(receipts()
            .lock()
            .unwrap()
            .contains_key(&(dir.path().to_owned(), T1.to_owned())));
        abandon_receipts(dir.path(), SESSION, &ended);
        assert_eq!(
            outcome(dir.path(), T1, receipt, Duration::from_secs(5)).await,
            TransferStatus::Uncertain
        );
        // No answer in time; a later receipt changes nothing.
        let receipt = forward_to(
            dir.path(),
            &connection(&replacement),
            &mut store,
            completed(dir.path(), &writer, T2, b"b"),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            outcome(dir.path(), T2, receipt, Duration::from_millis(50)).await,
            TransferStatus::Uncertain
        );
        transfer_receipt(
            dir.path(),
            SESSION,
            &replacement,
            T2,
            conversations_core::terminal_files::TerminalFileStatus::Sent,
        );
        assert!(!receipts()
            .lock()
            .unwrap()
            .keys()
            .any(|(d, _)| d == dir.path()));
    }

    #[test]
    fn a_transfer_withheld_or_not_enqueued_leaves_no_waiter() {
        let (writer, dir) = writer_host();
        let mut store = Store::open(dir.path()).unwrap();
        let failed = forward_to(
            dir.path(),
            &Connection::new(),
            &mut store,
            completed(dir.path(), &writer, T1, b"a"),
            |_| anyhow::bail!("queue full"),
        );
        assert_eq!(failed.err(), Some(TransferStatus::Failed));
        assert!(!receipts()
            .lock()
            .unwrap()
            .keys()
            .any(|(d, _)| d == dir.path()));
    }

    fn subscribe(dir: &Path, signer: &Signer, session: &str) -> SignedFrame {
        op(
            dir,
            signer,
            "terminal.subscribe",
            session,
            &format!(r#"{{"session":"{session}"}}"#),
            authz::store::now_secs(),
        )
    }

    #[test]
    fn a_withheld_piece_is_expired_exactly_when_the_verifier_would_call_its_op_expired() {
        use conversations_core::private_channel::TerminalEnd;
        let exp = 1_760_000_060;
        // `now > exp` is expired for the envelope verifier and the store alike.
        assert_eq!(end_cause(exp - 1, exp), TerminalEnd::Withdrawn);
        assert_eq!(end_cause(exp, exp), TerminalEnd::Withdrawn);
        assert_eq!(end_cause(exp + 1, exp), TerminalEnd::Expired);
        let ended = end_piece(3, 7, TerminalEnd::Withdrawn);
        assert_eq!(
            (ended.sub, ended.seq, ended.end),
            (3, 7, Some(TerminalEnd::Withdrawn))
        );
        assert!(ended.data.is_empty() && ended.fin);
    }

    #[test]
    fn a_terminal_subscribe_reads_one_running_session_and_its_frames_stop_at_revocation() {
        let reader = Signer::new();
        let dir = host(&reader, &[&format!("history.read:{SESSION}")]);
        let running = [SESSION.to_owned(), OTHER.to_owned()];
        let now = authz::store::now_secs();
        let held = read_now(
            dir.path(),
            &subscribe(dir.path(), &reader, SESSION),
            &running,
            now,
        )
        .unwrap();
        assert_eq!(held.0, OpStatus::Executed);
        assert!(matches!(held.2, Some((ReplyBody::Terminal { .. }, _))));
        // No widening from history.read:S to another session.
        let other = read_now(
            dir.path(),
            &subscribe(dir.path(), &reader, OTHER),
            &running,
            now,
        )
        .unwrap();
        assert_eq!(other.0, OpStatus::Refused);
        assert!(other.2.is_none());
        // A session that is not running has no terminal to follow.
        let stopped = read_now(
            dir.path(),
            &subscribe(dir.path(), &reader, SESSION),
            &[],
            now,
        )
        .unwrap();
        assert_eq!(stopped.0, OpStatus::Failed);
        assert!(stopped.2.is_none());

        let (_, admitted) = held.2.clone().unwrap();
        let frame = |seq| Pending {
            text: format!("terminal frame {seq}"),
            admitted: Some(admitted.clone()),
            transfer: None,
            subscribe: None,
        };
        assert!(deliver(dir.path(), frame(1)).is_some());
        db(dir.path())
            .execute("UPDATE grants SET revoked_at = ?1", [now])
            .unwrap();
        db(dir.path())
            .execute("UPDATE grants SET revoked_at = NULL", [])
            .unwrap();
        assert_eq!(
            deliver(dir.path(), frame(2)),
            None,
            "a re-grant revived the old stream"
        );
    }

    fn statement(signed: &conversations_core::host_relay::Signed) -> ReplyStatement {
        serde_json::from_slice(&B64.decode(&signed.p).unwrap()).unwrap()
    }

    #[test]
    fn a_private_reply_keeps_a_long_status_detail_whole_and_bounds_only_its_size() {
        let reader = Signer::new();
        let dir = host(&reader, &[&format!("history.read:{SESSION}")]);
        let audience: Vec<String> = (0..12)
            .map(|i| format!("reader-number-{i}@example.test"))
            .collect();
        let detail = serde_json::json!({
            "state": "published", "rev": 7, "audience": audience,
            "published": true, "pending": null,
        })
        .to_string();
        assert!(detail.chars().count() > 160);
        let signed = signed_reply(
            dir.path(),
            "peer".into(),
            "kid".into(),
            b"op",
            OpStatus::Executed,
            Some(detail.clone()),
            None,
        )
        .unwrap();
        let answered = statement(&signed);
        assert_eq!(answered.status, "executed");
        assert_eq!(answered.detail.as_deref(), Some(detail.as_str()));
        let parsed: serde_json::Value =
            serde_json::from_str(answered.detail.as_deref().unwrap()).unwrap();
        assert_eq!(parsed["audience"], serde_json::json!(audience));

        let oversized = "x".repeat(MAX_REPLY_PAYLOAD);
        let signed = signed_reply(
            dir.path(),
            "peer".into(),
            "kid".into(),
            b"op",
            OpStatus::Executed,
            Some(oversized),
            None,
        )
        .unwrap();
        let answered = statement(&signed);
        assert_eq!(answered.status, "failed");
        assert_eq!(answered.detail.as_deref(), Some("the answer is too large"));
        assert!(answered.body.is_none());
    }

    fn writer_host() -> (Signer, tempfile::TempDir) {
        let writer = Signer::new();
        let dir = host(&writer, &[&format!("session.write:{SESSION}")]);
        (writer, dir)
    }

    const T1: &str = "0123456789abcdef0123456789abcdef";
    const T2: &str = "fedcba9876543210fedcba9876543210";

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn feed_all(inbound: &mut Inbound, transfer: &str, file: u32, bytes: &[u8]) -> Option<Fed> {
        use conversations_core::private_channel::MAX_CHUNK_BYTES;
        let mut last = None;
        for (i, piece) in bytes.chunks(MAX_CHUNK_BYTES).enumerate() {
            last = inbound.feed(
                transfer,
                file,
                (i * MAX_CHUNK_BYTES) as u64,
                piece,
                Instant::now(),
            );
            if !matches!(last, Some(Fed::More)) {
                break;
            }
        }
        last
    }

    /// Exact signed bytes, file by file and in order, complete a transfer; the
    /// content is exactly what was signed, bound to its session and operation.
    #[test]
    fn signed_files_arrive_in_chunks_and_complete_only_when_every_hash_matches() {
        let (writer, dir) = writer_host();
        let big: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let op = send_files(
            dir.path(),
            &writer,
            T1,
            &[
                ("notes.txt", b"private-file-marker"),
                ("empty", b""),
                ("big.bin", &big),
            ],
        );
        let mut inbound = Inbound::default();
        assert!(inbound.admit(&op, later()).unwrap().is_none());
        assert!(matches!(
            feed_all(&mut inbound, T1, 0, b"private-file-marker"),
            Some(Fed::More)
        ));
        let Some(Fed::Complete(done)) = feed_all(&mut inbound, T1, 2, &big) else {
            panic!("not complete")
        };
        assert_eq!(done.transfer, T1);
        assert_eq!(done.session, SESSION);
        assert_eq!(done.admitted.payload_sha256, op.payload_sha256);
        assert_eq!(
            done.content,
            Content::Files(vec![
                ("notes.txt".into(), b"private-file-marker".to_vec()),
                ("empty".into(), Vec::new()),
                ("big.bin".into(), big.clone()),
            ])
        );
        assert!(
            inbound.feed(T1, 0, 0, b"x", Instant::now()).is_none(),
            "one use"
        );
    }

    type Feeding = Box<dyn Fn(&mut Inbound) -> Option<Fed>>;

    #[test]
    fn altered_reordered_oversized_or_late_bytes_drop_the_transfer() {
        let (writer, dir) = writer_host();
        let cases: Vec<(&str, Feeding)> = vec![
            (
                "altered",
                Box::new(|i: &mut Inbound| {
                    i.feed(T1, 0, 0, b"private-file-markeR", Instant::now())
                }),
            ),
            (
                "wrong file",
                Box::new(|i: &mut Inbound| i.feed(T1, 1, 0, b"x", Instant::now())),
            ),
            (
                "gap",
                Box::new(|i: &mut Inbound| i.feed(T1, 0, 1, b"rivate", Instant::now())),
            ),
            (
                "too long",
                Box::new(|i: &mut Inbound| {
                    i.feed(T1, 0, 0, b"private-file-marker!", Instant::now())
                }),
            ),
            (
                "empty chunk",
                Box::new(|i: &mut Inbound| i.feed(T1, 0, 0, b"", Instant::now())),
            ),
            (
                "expired",
                Box::new(|i: &mut Inbound| {
                    i.feed(
                        T1,
                        0,
                        0,
                        b"private-file-marker",
                        Instant::now() + Duration::from_secs(61),
                    )
                }),
            ),
        ];
        for (name, bad) in cases {
            let op = send_files(
                dir.path(),
                &writer,
                T1,
                &[("a.txt", b"private-file-marker")],
            );
            let mut inbound = Inbound::default();
            inbound.admit(&op, later()).unwrap();
            assert!(matches!(bad(&mut inbound), Some(Fed::Refused(_))), "{name}");
            assert!(
                inbound
                    .feed(T1, 0, 0, b"private-file-marker", Instant::now())
                    .is_none(),
                "{name}: a refused transfer cannot be completed afterwards"
            );
        }
    }

    #[test]
    fn transfers_are_bounded_per_channel_and_ids_cannot_be_reused_or_unknown() {
        let (writer, dir) = writer_host();
        let mut inbound = Inbound::default();
        let first = send_files(dir.path(), &writer, T1, &[("a", b"1")]);
        inbound.admit(&first, later()).unwrap();
        assert_eq!(
            inbound.admit(&first, later()).err(),
            Some("transfer id already used")
        );
        inbound
            .admit(
                &send_files(dir.path(), &writer, T2, &[("b", b"2")]),
                later(),
            )
            .unwrap();
        let third = send_files(
            dir.path(),
            &writer,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &[("c", b"3")],
        );
        assert_eq!(
            inbound.admit(&third, later()).err(),
            Some("too many transfers in progress")
        );
        assert!(inbound
            .feed(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                0,
                0,
                b"1",
                Instant::now()
            )
            .is_none());
        assert!(inbound.cancel(T1));
        assert!(
            inbound.feed(T1, 0, 0, b"1", Instant::now()).is_none(),
            "cancelled"
        );
        assert_eq!(
            inbound.expire(Instant::now() + Duration::from_secs(61)),
            vec![T2.to_owned()]
        );
    }

    fn joined(pieces: &[TerminalFrame]) -> Vec<u8> {
        pieces
            .iter()
            .flat_map(|p| B64.decode(&p.data).unwrap())
            .collect()
    }

    /// A viewer sees the snapshot taken at subscribe time, then every later
    /// update once, in order, numbered without gaps; output from before the
    /// subscription is in the snapshot and is not sent again.
    #[tokio::test]
    async fn a_terminal_follow_sends_one_snapshot_then_each_update_once_in_order() {
        let screen = Screen::new(40, 6);
        screen
            .lock()
            .unwrap()
            .output(b"before-subscribe-marker\r\n");
        let (tx, mut rx) = mpsc::channel(8);
        let task = tokio::spawn(follow(Arc::downgrade(&screen), 7, tx));
        let (sub, Some(first)) = rx.recv().await.unwrap() else {
            panic!("no snapshot")
        };
        assert_eq!(sub, 7);
        assert_eq!(first.kind, PtyFrameKind::Snapshot);
        let mut seq = 0;
        let snapshot = pieces(7, &mut seq, &first);
        assert!(String::from_utf8_lossy(&joined(&snapshot)).contains("before-subscribe-marker"));
        assert!(snapshot.last().unwrap().fin && snapshot[0].kind == TerminalFrameKind::Snapshot);
        screen.lock().unwrap().output(b"after-1 ");
        screen.lock().unwrap().output(b"after-2");
        let mut updates = Vec::new();
        while !String::from_utf8_lossy(&joined(&updates)).contains("after-2") {
            let (_, Some(frame)) = rx.recv().await.unwrap() else {
                panic!("ended")
            };
            updates.extend(pieces(7, &mut seq, &frame));
        }
        let text = String::from_utf8_lossy(&joined(&updates)).into_owned();
        assert!(
            !text.contains("before-subscribe-marker"),
            "no duplicate replay: {text}"
        );
        assert!(text.find("after-1").unwrap() < text.find("after-2").unwrap());
        let seqs: Vec<u64> = snapshot.iter().chain(&updates).map(|p| p.seq).collect();
        assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>(), "no gaps");
        drop(screen);
        assert!(
            matches!(rx.recv().await, Some((7, None))),
            "the follow ends with the terminal"
        );
        task.await.unwrap();
    }

    /// A viewer that falls behind the broadcast is rebuilt from a fresh
    /// snapshot that contains everything it missed, never left with a gap.
    #[tokio::test]
    async fn a_lagging_terminal_follow_resyncs_from_a_fresh_snapshot() {
        let screen = Screen::new(40, 6);
        let (tx, mut rx) = mpsc::channel(1);
        let task = tokio::spawn(follow(Arc::downgrade(&screen), 1, tx));
        let (_, Some(_)) = rx.recv().await.unwrap() else {
            panic!()
        };
        for i in 0..400 {
            screen
                .lock()
                .unwrap()
                .output(format!("line-{i}\r\n").as_bytes());
        }
        screen.lock().unwrap().output(b"final-marker");
        let mut saw_snapshot = false;
        let mut text = String::new();
        while !text.contains("final-marker") {
            let (_, Some(frame)) = rx.recv().await.unwrap() else {
                panic!("ended")
            };
            if frame.kind == PtyFrameKind::Snapshot {
                saw_snapshot = true;
                text.clear();
            }
            text.push_str(&String::from_utf8_lossy(&joined(&pieces(
                1, &mut 0, &frame,
            ))));
        }
        assert!(saw_snapshot, "lag is answered with a snapshot");
        drop(screen);
        task.abort();
    }

    /// Drafting `pieces` into a local cursor leaves the subscription cursor
    /// untouched until each piece is actually released. A partial release
    /// (e.g. the SCTP channel rejected one of the pieces in the middle of a
    /// frame while the buffer stayed below the ceiling) must leave the sub's
    /// `seq` at the last delivered piece's `seq + 1`, so the NEXT frame on
    /// the same sub starts at a `seq` the browser's `Following.take` still
    /// accepts. The previous implementation advanced `current.seq` for every
    /// piece in the drafted `Vec`, which turned a partial release into a
    /// "The terminal stream skipped or repeated a frame." protocol error on
    /// the next frame.
    #[test]
    fn a_partial_release_leaves_the_sub_cursor_on_the_last_delivered_piece() {
        use base64::engine::general_purpose::STANDARD;
        use conversations_core::private_channel::MAX_TERMINAL_DATA;
        let big = vec![b'x'; MAX_TERMINAL_DATA * 3 + 5];
        let frame = Frame {
            cols: 80,
            rows: 24,
            b64: STANDARD.encode(&big),
            kind: PtyFrameKind::Snapshot,
        };
        let mut sub_seq: u64 = 7;
        let mut draft = sub_seq;
        let drafted = pieces(1, &mut draft, &frame);
        assert_eq!(drafted.len(), 4, "a four-chunk snapshot");
        assert_eq!(draft, sub_seq + 4, "drafting advances the local cursor");
        assert_eq!(sub_seq, 7, "drafting leaves the sub cursor untouched");
        // Deliver only the first two pieces: the next two "failed" to reach
        // the wire (mirroring the `Ok(Some(false))` branch of `release_at`).
        let mut delivered_count = 0;
        for piece in &drafted {
            if delivered_count >= 2 {
                break;
            }
            sub_seq = piece.seq + 1;
            delivered_count += 1;
        }
        assert_eq!(delivered_count, 2);
        assert_eq!(
            sub_seq, 9,
            "sub cursor stays on the last delivered piece's seq + 1"
        );
        // A follow-up frame drafted from the sub cursor renumbers from there,
        // so the browser's `Following` sees `9, 10` next — not `11, 12`.
        let next = Frame {
            cols: 80,
            rows: 24,
            b64: STANDARD.encode(b"tail"),
            kind: PtyFrameKind::Output,
        };
        let mut next_draft = sub_seq;
        let follow_up = pieces(1, &mut next_draft, &next);
        assert_eq!(
            follow_up.iter().map(|p| p.seq).collect::<Vec<_>>(),
            vec![9],
            "the next frame starts at the sub cursor, not past the undelivered pieces"
        );
    }

    /// An oversized snapshot that the SCTP send window cannot take in one
    /// pass must still land in the browser gaplessly. In production
    /// `str0m::channel::Channel::write` returns `Ok(false)` when the fragment
    /// exceeds `sctp.available()`, even with `buffered_amount()` still under
    /// `MAX_BUFFERED`. The per-sub pending queue (drained between SCTP pump
    /// cycles) must carry the leftover pieces through; the subscription
    /// must NOT be ended with `Expired` just because the window closed.
    ///
    /// This test drives the exact helpers (`push_and_flush`, `flush_pending`)
    /// that `run()` calls, against a fake-channel whose `deliver` returns
    /// `false` once the window is exhausted and `true` again after
    /// `drain()`. Real `Verified` admission exercises the authz revalidation
    /// path.
    #[test]
    fn an_oversized_snapshot_delivers_through_backpressure_without_ending_the_sub() {
        use base64::engine::general_purpose::STANDARD;
        use conversations_core::private_channel::MAX_TERMINAL_DATA;
        let owner = Signer::new();
        let dir = host(&owner, &[&format!("history.read:{SESSION}")]);
        let mut store = Store::open(dir.path()).unwrap();
        let now = authz::store::now_secs();
        let signed = op(
            dir.path(),
            &owner,
            "history.get",
            SESSION,
            &format!(r#"{{"session":"{SESSION}","after":0,"limit":32}}"#),
            now,
        );
        let Ok(Decision::Execute(admitted)) = authz::dispatch::decide(dir.path(), &signed, now)
        else {
            panic!("op not admitted")
        };

        // This encoded snapshot exceeds the real SCTP send capacity.
        let payload = vec![b'x'; MAX_TERMINAL_DATA * 10 + 5];
        let frame = Frame {
            cols: 120,
            rows: 40,
            b64: STANDARD.encode(&payload),
            kind: PtyFrameKind::Snapshot,
        };
        let mut draft = 0u64;
        let drafted = pieces(1, &mut draft, &frame);
        assert_eq!(drafted.len(), 11);
        let texts: Vec<String> = drafted
            .into_iter()
            .map(|p| serde_json::to_string(&ChannelReply::Terminal(p)).unwrap())
            .collect();

        struct Fake {
            buffered: std::cell::Cell<usize>,
            delivered: std::cell::RefCell<Vec<String>>,
        }
        impl Fake {
            fn write(&self, bytes: usize) -> bool {
                if self.buffered.get() + bytes > MAX_BUFFERED {
                    return false;
                }
                self.buffered.set(self.buffered.get() + bytes);
                true
            }
            fn deliver(&self, text: &str) -> bool {
                if !room_for_terminal_piece(self.buffered.get(), text.len())
                    || !self.write(text.len())
                {
                    return false;
                }
                self.delivered.borrow_mut().push(text.to_owned());
                true
            }
            fn drain(&self) {
                self.buffered.set(0);
            }
        }
        let fake = Fake {
            buffered: std::cell::Cell::new(0),
            delivered: Default::default(),
        };

        let mut pending: VecDeque<(String, Verified)> = VecDeque::new();
        let mut cursor: u64 = 0;
        let ended = push_and_flush(
            &mut store,
            &admitted,
            texts.clone(),
            &mut pending,
            &mut cursor,
            MAX_PENDING_PIECES,
            now,
            |t| fake.deliver(t),
        );
        assert!(
            ended.is_none(),
            "SCTP backpressure must not end the subscription"
        );
        assert!(cursor > 0 && cursor < 11, "snapshot must span drain cycles");
        let mut replies = 0;
        loop {
            assert!(
                fake.write(conversations_core::private_channel::MAX_REPLY_MESSAGE),
                "a full-size signed reply must fit after a terminal flush"
            );
            replies += 1;
            if pending.is_empty() {
                break;
            }
            fake.drain();
            let ended = flush_pending(&mut store, &mut pending, &mut cursor, now, |t| {
                fake.deliver(t)
            });
            assert!(ended.is_none(), "ordinary pressure must not end the stream");
        }
        assert!(replies > 1, "control replies must continue between drains");
        assert_eq!(cursor, 11);

        let delivered = fake.delivered.borrow();
        assert_eq!(delivered.len(), 11);
        assert_eq!(*delivered, texts, "pieces delivered in draft order");
        let seqs: Vec<u64> = delivered
            .iter()
            .map(|t| {
                let v: serde_json::Value = serde_json::from_str(t).unwrap();
                v["seq"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(
            seqs,
            (0..11).collect::<Vec<u64>>(),
            "the browser's `Following.take` sees gapless seqs"
        );
    }

    /// A screen update that arrives while the subscription's pending queue
    /// still holds leftover pieces of a prior frame must be numbered past
    /// the queued seqs. If the daemon started a new frame's `seq` at
    /// `current.seq` while pending was non-empty, the new pieces would
    /// collide with queued pieces (same `seq` on different payloads) and
    /// the browser's `Following.take` would surface the overlap as a
    /// protocol gap.
    #[test]
    fn an_update_arriving_during_backpressure_is_numbered_past_queued_pieces() {
        use base64::engine::general_purpose::STANDARD;
        use conversations_core::private_channel::MAX_TERMINAL_DATA;
        let owner = Signer::new();
        let dir = host(&owner, &[&format!("history.read:{SESSION}")]);
        let mut store = Store::open(dir.path()).unwrap();
        let now = authz::store::now_secs();
        let signed = op(
            dir.path(),
            &owner,
            "history.get",
            SESSION,
            &format!(r#"{{"session":"{SESSION}","after":0,"limit":32}}"#),
            now,
        );
        let Ok(Decision::Execute(admitted)) = authz::dispatch::decide(dir.path(), &signed, now)
        else {
            panic!("op not admitted")
        };

        // Fake channel: deliver until a tight window closes, then reopen.
        struct Fake {
            window: usize,
            remaining: std::cell::Cell<usize>,
            delivered: std::cell::RefCell<Vec<String>>,
        }
        impl Fake {
            fn new(window: usize) -> Self {
                Self {
                    window,
                    remaining: std::cell::Cell::new(window),
                    delivered: Default::default(),
                }
            }
            fn deliver(&self, text: &str) -> bool {
                if self.remaining.get() == 0 {
                    return false;
                }
                self.remaining.set(self.remaining.get() - 1);
                self.delivered.borrow_mut().push(text.to_string());
                true
            }
            fn drain(&self) {
                self.remaining.set(self.window);
            }
        }
        let fake = Fake::new(2);

        let mut pending: VecDeque<(String, Verified)> = VecDeque::new();
        let mut cursor: u64 = 0;

        // Frame A — four-piece snapshot; window closes after piece 2.
        let payload_a = vec![b'a'; MAX_TERMINAL_DATA * 4];
        let snap = Frame {
            cols: 200,
            rows: 60,
            b64: STANDARD.encode(&payload_a),
            kind: PtyFrameKind::Snapshot,
        };
        let mut draft = cursor + pending.len() as u64;
        let drafts_a = pieces(1, &mut draft, &snap);
        let texts_a: Vec<String> = drafts_a
            .into_iter()
            .map(|p| serde_json::to_string(&ChannelReply::Terminal(p)).unwrap())
            .collect();
        push_and_flush(
            &mut store,
            &admitted,
            texts_a.clone(),
            &mut pending,
            &mut cursor,
            MAX_PENDING_PIECES,
            now,
            |t| fake.deliver(t),
        );
        assert_eq!(cursor, 2, "snapshot: two pieces reached the wire");
        assert_eq!(pending.len(), 2, "two pieces queued");

        // Frame B — a two-piece update arrives mid-backpressure; must be
        // numbered past the queued seqs.
        let payload_b = vec![b'b'; MAX_TERMINAL_DATA + 1];
        let upd = Frame {
            cols: 200,
            rows: 60,
            b64: STANDARD.encode(&payload_b),
            kind: PtyFrameKind::Output,
        };
        let mut draft = cursor + pending.len() as u64;
        let drafts_b = pieces(1, &mut draft, &upd);
        assert_eq!(drafts_b.len(), 2, "two-piece update");
        let seqs_b: Vec<u64> = drafts_b.iter().map(|p| p.seq).collect();
        assert_eq!(
            seqs_b,
            vec![4, 5],
            "update pieces take seqs past the queued snapshot pieces"
        );
        let texts_b: Vec<String> = drafts_b
            .into_iter()
            .map(|p| serde_json::to_string(&ChannelReply::Terminal(p)).unwrap())
            .collect();
        push_and_flush(
            &mut store,
            &admitted,
            texts_b.clone(),
            &mut pending,
            &mut cursor,
            MAX_PENDING_PIECES,
            now,
            |t| fake.deliver(t),
        );
        assert_eq!(pending.len(), 4, "snapshot leftover + update in order");

        // Repeated drain/flush cycles (mirroring the outer loop's tick
        // cadence) drain the queue; the browser sees gapless `seq` across
        // the frame boundary.
        for _ in 0..8 {
            if pending.is_empty() {
                break;
            }
            fake.drain();
            let withheld = flush_pending(&mut store, &mut pending, &mut cursor, now, |t| {
                fake.deliver(t)
            });
            assert!(withheld.is_none(), "backpressure must not end the sub");
        }
        assert_eq!(cursor, 6);
        assert!(pending.is_empty());

        let delivered = fake.delivered.borrow();
        let seqs: Vec<u64> = delivered
            .iter()
            .map(|t| {
                let v: serde_json::Value = serde_json::from_str(t).unwrap();
                v["seq"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(
            seqs,
            vec![0, 1, 2, 3, 4, 5],
            "gapless seqs across the backpressure boundary"
        );
    }

    /// A small snapshot that fits the SCTP send window reaches the browser
    /// in one `push_and_flush` pass, leaves no pending queue, and does not
    /// touch the end-piece path. Mirrors the small-session control case
    /// (session `34a9642d-…`, ~3.5 KiB snapshot) that works in the hosted
    /// browser today.
    #[test]
    fn a_small_snapshot_delivers_in_one_pass_and_leaves_no_pending() {
        use base64::engine::general_purpose::STANDARD;
        let owner = Signer::new();
        let dir = host(&owner, &[&format!("history.read:{SESSION}")]);
        let mut store = Store::open(dir.path()).unwrap();
        let now = authz::store::now_secs();
        let signed = op(
            dir.path(),
            &owner,
            "history.get",
            SESSION,
            &format!(r#"{{"session":"{SESSION}","after":0,"limit":32}}"#),
            now,
        );
        let Ok(Decision::Execute(admitted)) = authz::dispatch::decide(dir.path(), &signed, now)
        else {
            panic!("op not admitted")
        };

        // ~3.5 KiB decoded — a single piece.
        let payload = vec![b's'; 3508];
        let frame = Frame {
            cols: 80,
            rows: 24,
            b64: STANDARD.encode(&payload),
            kind: PtyFrameKind::Snapshot,
        };
        let mut draft = 0u64;
        let drafts = pieces(1, &mut draft, &frame);
        assert_eq!(drafts.len(), 1);
        let texts: Vec<String> = drafts
            .into_iter()
            .map(|p| serde_json::to_string(&ChannelReply::Terminal(p)).unwrap())
            .collect();

        let mut pending: VecDeque<(String, Verified)> = VecDeque::new();
        let mut cursor: u64 = 0;
        let delivered = std::cell::RefCell::new(Vec::<String>::new());
        let withheld = push_and_flush(
            &mut store,
            &admitted,
            texts.clone(),
            &mut pending,
            &mut cursor,
            MAX_PENDING_PIECES,
            now,
            |t| {
                delivered.borrow_mut().push(t.to_string());
                true
            },
        );
        assert!(withheld.is_none());
        assert_eq!(cursor, 1);
        assert!(pending.is_empty());
        assert_eq!(*delivered.borrow(), texts);
    }

    /// A pending backlog that would exceed `MAX_PENDING_PIECES` indicates
    /// the channel is genuinely wedged rather than momentarily
    /// back-pressured. Ending the sub with `Expired` lets the browser's
    /// `openPrivatePty.onEnd` auto-resubscribe once instead of stalling the
    /// whole peer.
    #[test]
    fn a_pending_backlog_that_would_overflow_the_cap_ends_the_sub_expired() {
        let owner = Signer::new();
        let dir = host(&owner, &[&format!("history.read:{SESSION}")]);
        let mut store = Store::open(dir.path()).unwrap();
        let now = authz::store::now_secs();
        let signed = op(
            dir.path(),
            &owner,
            "history.get",
            SESSION,
            &format!(r#"{{"session":"{SESSION}","after":0,"limit":32}}"#),
            now,
        );
        let Ok(Decision::Execute(admitted)) = authz::dispatch::decide(dir.path(), &signed, now)
        else {
            panic!("op not admitted")
        };
        let mut pending: VecDeque<(String, Verified)> = VecDeque::new();
        let mut cursor: u64 = 0;
        // Pre-fill pending to the cap; a new frame of any size would push
        // past it.
        for i in 0..MAX_PENDING_PIECES {
            pending.push_back((format!("piece-{i}"), admitted.clone()));
        }
        let overflow = vec!["one more".into()];
        let ended = push_and_flush(
            &mut store,
            &admitted,
            overflow,
            &mut pending,
            &mut cursor,
            MAX_PENDING_PIECES,
            now,
            |_| false,
        );
        assert_eq!(
            ended,
            Some(TerminalEnd::Expired),
            "overflow is treated as a wedged channel"
        );
        assert_eq!(cursor, 0, "cursor must not advance on an overflow end");
        assert_eq!(
            pending.len(),
            MAX_PENDING_PIECES,
            "overflow leaves pending untouched"
        );
    }

    #[test]
    fn large_snapshots_are_split_into_bounded_pieces_and_empty_updates_send_nothing() {
        use base64::engine::general_purpose::STANDARD;
        use conversations_core::private_channel::MAX_TERMINAL_DATA;
        let big = vec![b'x'; MAX_TERMINAL_DATA * 2 + 5];
        let frame = Frame {
            cols: 80,
            rows: 24,
            b64: STANDARD.encode(&big),
            kind: PtyFrameKind::Snapshot,
        };
        let mut seq = 10;
        let out = pieces(2, &mut seq, &frame);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.iter().map(|p| p.seq).collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
        assert_eq!(
            out.iter().map(|p| p.fin).collect::<Vec<_>>(),
            vec![false, false, true]
        );
        assert_eq!(joined(&out), big);
        let empty = Frame {
            cols: 80,
            rows: 24,
            b64: String::new(),
            kind: PtyFrameKind::Output,
        };
        assert!(pieces(2, &mut seq, &empty).is_empty());
        assert_eq!(seq, 13);
    }
}
