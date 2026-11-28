//! The host's side of the opaque browser relay.
//!
//! The host dials the server for requests addressed to ITS id and answers each
//! one. Only these request types exist (`conversations_core::host_relay`):
//! finishing a locally started registration, starting and finishing a
//! delegation, a signed operation, and the signaling for a private channel. Registration start and approval are not
//! reachable here: `passkey::handle` is called with no local-socket trust, and
//! the request type has no variant for them. Everything the server relays is
//! untrusted input; authority comes only from locally stored keys.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use conversations_core::host_relay::{
    clean_text, RelayAnswer, RelayEnvelope, RelayReply, RelayRequest, MAX_REQUEST_BYTES,
};
use futures::StreamExt;

use super::{dispatch_signed, Host};
use crate::commands::local_host::authz::{
    self,
    passkey::{PasskeyRequest, PasskeyResponse},
};
use crate::commands::local_host::settings::Settings;

/// Requests handled at once. The server caps in-flight requests per host at 8.
const CONCURRENT: usize = 1;
const MAX_LINE: usize = MAX_REQUEST_BYTES * 2;

pub(super) async fn relay_loop(host: Arc<Host>) {
    let host_id = match host_id_blocking(&host).await {
        Ok(id) => id,
        Err(error) => {
            eprintln!("Browser relay disabled: {error:#}");
            return;
        }
    };
    let mut delay = Duration::from_secs(2);
    loop {
        match consume(&host, &host_id).await {
            Ok(()) => return, // superseded; do not start an eviction war
            Err(error) => {
                eprintln!("Browser relay: {error:#}");
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
        tokio::time::sleep(delay).await;
    }
}

async fn host_id_blocking(host: &Host) -> Result<String> {
    let dir = host.dir.clone();
    tokio::task::spawn_blocking(move || authz::host_id(&dir)).await?
}

pub(super) async fn consume(host: &Arc<Host>, host_id: &str) -> Result<()> {
    // Prove possession of the host key (a one-time server nonce, signed) before
    // the server will attach this stream under the public host id.
    let nonce: serde_json::Value = host
        .auth(host.client.post(format!(
            "{}/api/host-relay/{host_id}/challenge",
            host.server
        )))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let nonce = nonce["nonce"]
        .as_str()
        .context("relay gave no challenge")?
        .to_owned();
    let (public, sig, card) = {
        let (dir, nonce) = (host.dir.clone(), nonce.clone());
        tokio::task::spawn_blocking(move || -> Result<_> {
            let (public, sig) = authz::attest::attach_proof(&dir, &nonce)?;
            Ok((
                public,
                sig,
                crate::commands::local_host::machines::card(&dir)?,
            ))
        })
        .await??
    };
    let response = host
        .auth(
            host.client
                // hex and base64url only: nothing here needs escaping.
                .get(format!(
                    "{}/api/host-relay/{host_id}/stream?nonce={nonce}&pub={public}&sig={sig}&card_p={}&card_s={}",
                    host.server, card.p, card.s
                )),
        )
        .send()
        .await?
        .error_for_status()?;
    let reply_token = Arc::new(std::sync::Mutex::new(String::new()));
    let permits = Arc::new(tokio::sync::Semaphore::new(CONCURRENT));
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    while let Some(bytes) = stream.next().await {
        buffer.extend_from_slice(&bytes?);
        ensure!(buffer.len() < MAX_LINE, "oversized relay event");
        while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = buffer.drain(..=end).collect();
            let Ok(line) = std::str::from_utf8(&line) else {
                continue;
            };
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.contains(r#""attached""#) {
                if let Some(token) = serde_json::from_str::<serde_json::Value>(data)
                    .ok()
                    .and_then(|v| v["reply_token"].as_str().map(str::to_owned))
                {
                    *reply_token.lock().unwrap() = token;
                }
                continue;
            }
            if data.contains(r#""superseded""#) {
                eprintln!("Browser relay superseded by a newer host connection.");
                return Ok(());
            }
            let Ok(envelope) = serde_json::from_str::<RelayEnvelope>(data) else {
                continue;
            };
            // One request at a time, in arrival order: starts and replays of the
            // same operation must not race each other. The server bounds how
            // many can queue.
            let permit = permits.clone().acquire_owned().await?;
            let (host, host_id) = (host.clone(), host_id.to_owned());
            let token = reply_token.lock().unwrap().clone();
            tokio::spawn(async move {
                let reply = match envelope.request {
                    // The peer it starts outlives this request.
                    RelayRequest::PeerOffer { p, s, k, sdp } => {
                        super::private_channel::offer(&host, &p, &s, &k, sdp).await
                    }
                    request => respond(&host, request).await,
                };
                if let Err(error) = answer(&host, &host_id, &token, envelope.id, reply).await {
                    eprintln!("Browser relay answer not delivered: {error:#}");
                }
                drop(permit);
            });
        }
    }
    anyhow::bail!("host relay stream ended")
}

async fn answer(
    host: &Host,
    host_id: &str,
    token: &str,
    id: String,
    reply: RelayReply,
) -> Result<()> {
    host.auth(
        host.client
            .post(format!("{}/api/host-relay/{host_id}/reply", host.server))
            .header("x-relay-token", token),
    )
    .json(&RelayAnswer { id, reply })
    .timeout(Duration::from_secs(5))
    .send()
    .await?
    .error_for_status()
    .context("server refused the answer")?;
    Ok(())
}

/// Turn one relayed request into a reply that is safe to send back.
pub(super) async fn respond(host: &Host, request: RelayRequest) -> RelayReply {
    match request {
        RelayRequest::SignedOp { p, s, k } => {
            let Some(frame) = authz::SignedFrame::from_wire(&p, &s, &k) else {
                return RelayReply::Error {
                    message: "malformed signed operation".into(),
                };
            };
            let (payload, kid) = (frame.payload.clone(), frame.kid.clone());
            let (status, detail) = dispatch_signed(host, frame).await;
            let dir = host.dir.clone();
            let signed = tokio::task::spawn_blocking(move || {
                authz::attest::op_result(&dir, &kid, &payload, status.as_str(), detail.as_deref())
            })
            .await;
            match signed {
                Ok(Ok(statement)) => RelayReply::Op { statement },
                _ => RelayReply::Error {
                    message: "the host could not sign its answer".into(),
                },
            }
        }
        RelayRequest::EnrollFinish {
            challenge,
            response,
            nonce,
            vouch,
        } => {
            if nonce.len() != 32
                || !nonce
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            {
                return RelayReply::Error {
                    message: "malformed registration request".into(),
                };
            }
            enroll_finish(host, challenge, response, nonce, vouch).await
        }
        RelayRequest::DelegateStart {
            browser_pub,
            dlg,
            caps,
            nonce,
        } => {
            passkey(
                host,
                PasskeyRequest::DelegateStart {
                    browser_pub,
                    dlg,
                    caps,
                    nonce,
                },
            )
            .await
        }
        RelayRequest::DelegateFinish {
            challenge,
            response,
        } => {
            passkey(
                host,
                PasskeyRequest::DelegateFinish {
                    challenge,
                    response,
                },
            )
            .await
        }
        RelayRequest::PeerChallenge { nonce } => {
            super::private_channel::challenge(host, nonce).await
        }
        // Answered by the relay loop, which owns the shared host the peer needs.
        RelayRequest::PeerOffer { .. } => RelayReply::Error {
            message: "private connection offers are not accepted here".into(),
        },
    }
}

/// Finish a registration and sign what the host actually verified and stored.
/// The reply carries no credential id of its own: the browser derives the id it
/// expects from its authenticator and accepts only a matching signed statement.
async fn enroll_finish(
    host: &Host,
    challenge: String,
    response: serde_json::Value,
    nonce: String,
    vouch: Option<conversations_core::host_relay::VouchProof>,
) -> RelayReply {
    let dir = host.dir.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let settings = Settings::load(&dir)
            .map_err(|_| anyhow::anyhow!("the host's settings are unreadable"))?;
        let request = PasskeyRequest::EnrollFinish {
            challenge: challenge.clone(),
            response,
            vouch,
        };
        match authz::passkey::handle(&dir, &settings, request, None, authz::store::now_secs())? {
            PasskeyResponse::Registered {
                cred_id,
                response_digest,
            } => authz::attest::enroll_done(&dir, &challenge, &nonce, &cred_id, &response_digest),
            _ => anyhow::bail!("unexpected host result"),
        }
    })
    .await;
    match outcome {
        Ok(Ok(statement)) => RelayReply::Registered { statement },
        Ok(Err(error)) => RelayReply::Error {
            message: clean_text(&error.to_string()),
        },
        Err(_) => RelayReply::Error {
            message: "the host could not process the request".into(),
        },
    }
}

async fn passkey(host: &Host, request: PasskeyRequest) -> RelayReply {
    let dir = host.dir.clone();
    // `None`: a relayed request never carries local-socket trust.
    let outcome = tokio::task::spawn_blocking(move || {
        // A settings error names a local path; keep it out of the reply.
        let settings = Settings::load(&dir)
            .map_err(|_| anyhow::anyhow!("the host's settings are unreadable"))?;
        authz::passkey::handle(&dir, &settings, request, None, authz::store::now_secs())
    })
    .await;
    match outcome {
        Ok(Ok(PasskeyResponse::Offered {
            challenge,
            options,
            statement,
        })) => RelayReply::Started {
            challenge,
            options,
            statement,
        },
        Ok(Ok(PasskeyResponse::Delegated { statement, .. })) => RelayReply::Delegated { statement },
        Ok(Ok(_)) => RelayReply::Error {
            message: "unexpected host result".into(),
        },
        Ok(Err(error)) => RelayReply::Error {
            // Outermost message only: inner contexts may name local paths.
            message: clean_text(&error.to_string()),
        },
        Err(_) => RelayReply::Error {
            message: "the host could not process the request".into(),
        },
    }
}
