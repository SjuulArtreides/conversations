//! Pairing the owner's own machines.
//!
//! Every host signs a machine card with its identity key: the agent key it
//! signs operations with, and its name. It presents the card when it attaches
//! to the relay, which lists it, opaque, to the same account. A card grants
//! nothing. Pairing does: the owner either compares the other machine's
//! fingerprint here and approves it (`conversations machine pair`, or the
//! local page), or signs `machine.pair` from a key that already has full
//! control on this machine. A paired machine's agent key is then an owner
//! credential with full control here, host-wide, for every session including
//! future ones.
//!
//! Pairing spreads: a machine that pairs a new machine passes the new card to
//! every machine already paired with it, and theirs to the new one, by signing
//! `machine.pair` with the full control it holds there. Unpairing revokes the
//! other machine's credential here and asks that machine to revoke this one.

use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use conversations_core::host_relay::{MachineCard, Signed, HOST_DOMAIN};
use ring::signature;
use serde::Serialize;

use super::authz::{
    self,
    envelope::{key_id, HOST_SCOPE},
    store::{now_secs, PairedMachine, Revocation, Store},
};

/// A machine card whose signature was checked against its identity key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub host_id: String,
    pub host_key: Vec<u8>,
    pub agent_key: Vec<u8>,
    pub label: String,
    /// The signed card as presented, to pass on unchanged.
    pub signed: Signed,
}

/// This machine's name: the computer's own name, else its host name.
pub fn machine_name() -> String {
    let run = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let name = if cfg!(target_os = "macos") {
        run("scutil", &["--get", "ComputerName"])
    } else {
        None
    }
    .or_else(|| run("hostname", &[]))
    .unwrap_or_default();
    authz::store::machine_label(name.trim_end_matches(".local"))
}

/// This machine's card, signed by its identity key.
pub fn card(dir: &Path) -> Result<Signed> {
    authz::ensure_dir(dir)?;
    let mut store = Store::open(dir)?;
    let host = store.host_id()?;
    let (_, agent) = store.agent_key()?;
    authz::attest::sign_value(
        &mut store,
        &MachineCard {
            v: 1,
            typ: "machine_card".into(),
            host,
            agent_key: B64.encode(agent),
            label: machine_name(),
        },
    )
}

/// Check `signed` against the identity key `host_key` the caller trusts.
pub fn verify_card(host_key: &[u8], signed: &Signed) -> Result<Card> {
    ensure!(host_key.len() == 32, "machine identity keys are Ed25519");
    let payload = B64
        .decode(&signed.p)
        .context("machine card is not base64url")?;
    let sig = B64
        .decode(&signed.s)
        .context("machine card is not base64url")?;
    let mut message = HOST_DOMAIN.to_vec();
    message.extend_from_slice(&payload);
    signature::UnparsedPublicKey::new(&signature::ED25519, host_key)
        .verify(&message, &sig)
        .map_err(|_| anyhow::anyhow!("machine card is not signed by that machine"))?;
    let card: MachineCard =
        serde_json::from_slice(&payload).context("machine card is malformed")?;
    let host_id = key_id(host_key);
    ensure!(
        card.v == 1 && card.typ == "machine_card" && card.host == host_id,
        "machine card is for another machine"
    );
    let agent_key = B64
        .decode(&card.agent_key)
        .ok()
        .filter(|k| k.len() == 32)
        .context("machine card names no Ed25519 agent key")?;
    Ok(Card {
        host_id,
        host_key: host_key.to_vec(),
        agent_key,
        label: authz::store::machine_label(&card.label),
        signed: signed.clone(),
    })
}

/// What a person compares between two machines: the first 20 characters of
/// the host id (120 bits) in groups of four.
pub fn fingerprint(host_id: &str) -> String {
    host_id
        .chars()
        .take(20)
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// The server this host talks to, and how it signs in there.
pub struct Net<'a> {
    pub http: &'a reqwest::Client,
    pub server: &'a str,
    pub cookie: Option<&'a str>,
}

/// One machine the account has attached right now, as this host sees it.
#[derive(Debug, Clone, Serialize)]
pub struct Listed {
    pub host_id: String,
    pub fingerprint: String,
    pub label: String,
    pub this_machine: bool,
    pub paired: bool,
    #[serde(skip)]
    pub card: Option<Card>,
}

/// The account's attached machines with a valid card (cards that do not verify
/// against the listed identity key are left out), plus the paired ones that
/// are offline.
pub async fn account_machines(net: &Net<'_>, dir: &Path) -> Result<Vec<Listed>> {
    #[derive(serde::Deserialize)]
    struct Listing {
        hosts: Vec<Attached>,
    }
    #[derive(serde::Deserialize)]
    struct Attached {
        id: String,
        key: String,
        card: Option<Signed>,
    }
    let mut request = net.http.get(format!("{}/api/host-relay", net.server));
    if let Some(cookie) = net.cookie {
        request = request.header(reqwest::header::COOKIE, cookie);
    }
    let listing: Listing = request
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let (own, paired) = {
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<(String, Vec<PairedMachine>)> {
            authz::ensure_dir(&dir)?;
            let mut store = Store::open(&dir)?;
            Ok((store.host_id()?, store.paired_machines()?))
        })
        .await??
    };
    let mut machines: Vec<Listed> = listing
        .hosts
        .into_iter()
        .filter_map(|host| {
            let key = B64.decode(&host.key).ok()?;
            let card = verify_card(&key, host.card.as_ref()?).ok()?;
            (card.host_id == host.id).then(|| Listed {
                fingerprint: fingerprint(&card.host_id),
                this_machine: card.host_id == own,
                paired: paired.iter().any(|m| m.host_id == card.host_id),
                host_id: card.host_id.clone(),
                label: card.label.clone(),
                card: Some(card),
            })
        })
        .collect();
    for machine in paired {
        if !machines.iter().any(|m| m.host_id == machine.host_id) {
            machines.push(Listed {
                fingerprint: fingerprint(&machine.host_id),
                host_id: machine.host_id,
                label: machine.label,
                this_machine: false,
                paired: true,
                card: None,
            });
        }
    }
    machines.sort_by(|a, b| {
        (!a.this_machine, &a.label, &a.host_id).cmp(&(!b.this_machine, &b.label, &b.host_id))
    });
    Ok(machines)
}

/// Pair `card` here (its identity key is already trusted by the caller).
pub fn pair_card(dir: &Path, card: &Card) -> Result<String> {
    authz::ensure_dir(dir)?;
    Store::open(dir)?.pair_machine(&card.host_key, &card.agent_key, &card.label, now_secs())
}

/// Pair the listed machine `host_id` after its owner compared its fingerprint,
/// then pass the pairing on to the machines already paired here.
pub async fn pair_listed(net: &Net<'_>, dir: &Path, host_id: &str) -> Result<PairOutcome> {
    let listed = account_machines(net, dir).await?;
    let machine = listed
        .into_iter()
        .find(|m| m.host_id == host_id || m.fingerprint == host_id)
        .context("that machine is not online on this account")?;
    ensure!(!machine.this_machine, "that is this machine");
    let card = machine
        .card
        .context("that machine is not online on this account")?;
    let credential = {
        let (dir, card) = (dir.to_path_buf(), card.clone());
        tokio::task::spawn_blocking(move || pair_card(&dir, &card)).await??
    };
    let spread = after_owner_pairing(net, dir, &card).await;
    Ok(PairOutcome {
        credential,
        label: card.label,
        spread,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct PairOutcome {
    pub credential: String,
    pub label: String,
    /// What each other machine said when the pairing was passed on.
    pub spread: Vec<Spread>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Spread {
    pub machine: String,
    pub outcome: String,
}

/// After the owner paired `new` here: introduce this machine to `new` (so a
/// `new` that already paired this machine passes its own machines on), then
/// pass the pairing on to the machines paired here.
pub async fn after_owner_pairing(net: &Net<'_>, dir: &Path, new: &Card) -> Vec<Spread> {
    let introduced = match own_card(dir) {
        Ok(own) => send_pair(net, dir, &new.host_key, &own).await,
        Err(error) => format!("unreachable: {error:#}"),
    };
    let mut results = vec![Spread {
        machine: format!("{} learns this machine", new.label),
        outcome: introduced,
    }];
    results.extend(spread(net, dir, new).await);
    results
}

/// This machine's own card, verified like any other.
fn own_card(dir: &Path) -> Result<Card> {
    let signed = card(dir)?;
    let key = Store::open(dir)?.host_public_key()?;
    verify_card(&key, &signed)
}

/// Pass a new pairing on: tell every other paired machine about `new`, and
/// `new` about each of them. Each side checks the card itself and accepts it
/// only because this machine has full control there. Best effort: a machine
/// that is offline is reported and can be paired again later.
pub async fn spread(net: &Net<'_>, dir: &Path, new: &Card) -> Vec<Spread> {
    let others: Vec<PairedMachine> = {
        let dir = dir.to_path_buf();
        match tokio::task::spawn_blocking(move || Store::open(&dir)?.paired_machines()).await {
            Ok(Ok(machines)) => machines,
            _ => return Vec::new(),
        }
    };
    let cards: Vec<Listed> = account_machines(net, dir).await.unwrap_or_default();
    let mut results = Vec::new();
    for other in others.iter().filter(|m| m.host_id != new.host_id) {
        // `other` learns about `new`.
        let outcome = send_pair(net, dir, &other.host_key, new).await;
        results.push(Spread {
            machine: format!("{} learns {}", other.label, new.label),
            outcome,
        });
        // `new` learns about `other`, from the card `other` presents now.
        let Some(card) = cards
            .iter()
            .find(|m| m.host_id == other.host_id)
            .and_then(|m| m.card.clone())
            .filter(|c| c.host_key == other.host_key)
        else {
            results.push(Spread {
                machine: format!("{} learns {}", new.label, other.label),
                outcome: "offline".into(),
            });
            continue;
        };
        let outcome = send_pair(net, dir, &new.host_key, &card).await;
        results.push(Spread {
            machine: format!("{} learns {}", new.label, other.label),
            outcome,
        });
    }
    results
}

async fn send_pair(net: &Net<'_>, dir: &Path, to: &[u8], card: &Card) -> String {
    let args = serde_json::json!({
        "host_key": B64.encode(&card.host_key),
        "card": card.signed,
    });
    match send(net, dir, to, HOST_SCOPE, "machine.pair", args, 60).await {
        Ok((status, _)) => status,
        Err(error) => format!("unreachable: {error:#}"),
    }
}

/// Sign `op` for the machine with identity key `to` and send it on a private
/// channel. Returns that machine's signed `(status, detail)`.
pub async fn send(
    net: &Net<'_>,
    dir: &Path,
    to: &[u8],
    sess: &str,
    op: &str,
    args: serde_json::Value,
    lifetime: i64,
) -> Result<(String, Option<String>)> {
    let prepared = {
        let (dir, to, sess, op) = (
            dir.to_path_buf(),
            to.to_vec(),
            sess.to_owned(),
            op.to_owned(),
        );
        tokio::task::spawn_blocking(move || {
            super::agent_link::prepare_op(&dir, &to, &sess, &op, args, lifetime)
        })
        .await??
    };
    let origin = reqwest::Url::parse(net.server)?;
    let relay = super::private_peer::Relay {
        http: net.http,
        origin: &origin,
        cookie: net.cookie,
    };
    let transport = super::settings::Settings::load(dir)?.transport;
    super::private_peer::exchange(&relay, dir, &prepared, &transport).await
}

/// What unpairing did here and on the other machine.
#[derive(Debug)]
pub struct Unpaired {
    pub label: String,
    pub revoked: Revocation,
    /// The other machine's answer, or why it could not be asked.
    pub other: std::result::Result<String, String>,
}

/// Unpair `host_id` here (revoking its credential at once), then ask it to
/// drop this machine too while this machine can still reach it.
pub async fn unpair(net: &Net<'_>, dir: &Path, host_id: &str) -> Result<Unpaired> {
    let machine = {
        let (dir, host_id) = (dir.to_path_buf(), host_id.to_owned());
        tokio::task::spawn_blocking(move || -> Result<Option<PairedMachine>> {
            authz::ensure_dir(&dir)?;
            let store = Store::open(&dir)?;
            Ok(store
                .paired_machines()?
                .into_iter()
                .find(|m| m.host_id == host_id || fingerprint(&m.host_id) == host_id))
        })
        .await??
    };
    let Some(machine) = machine else {
        bail!("that machine is not paired here");
    };
    let revoked = {
        let (dir, host_id) = (dir.to_path_buf(), machine.host_id.clone());
        tokio::task::spawn_blocking(move || Store::open(&dir)?.unpair_machine(&host_id, now_secs()))
            .await??
            .context("that machine is not paired here")?
    };
    let other = match send(
        net,
        dir,
        &machine.host_key,
        HOST_SCOPE,
        "machine.unpair",
        serde_json::json!({}),
        60,
    )
    .await
    {
        Ok((status, _)) => Ok(status),
        Err(error) => Err(format!("{error:#}")),
    };
    Ok(Unpaired {
        label: machine.label,
        revoked,
        other,
    })
}
