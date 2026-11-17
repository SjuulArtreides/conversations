#![cfg(unix)]
//! Agent-to-agent messages between two REAL hosts travel on the encrypted
//! private channel, end to end: a REAL central server in process (router,
//! relay, SQLite, plus a recorder and optional hostile rewrites), REAL
//! `arrakis-conversations host` processes, REAL `mcp serve` processes as
//! the agents run them, and recording stub `claude` agents. Only the agents are
//! stubbed; relay, crypto, hosts, wrappers and local databases are real.
//!
//! The direct case needs nothing external. The forced-TURN case needs a coturn
//! `turnserver` and refuses to run without it (it never substitutes a browser
//! helper or another TURN implementation):
//!
//! ```sh
//! cargo test -p conversations-cli --test native_agent_messaging -- --nocapture
//! NATIVE_AGENT_TURNSERVER=/path/to/turnserver \
//!   cargo test -p conversations-cli --test native_agent_messaging -- --ignored --nocapture
//! ```

#[path = "support/private_rtc.rs"]
#[allow(dead_code)]
mod rtc;

#[path = "support/native_hosts.rs"]
#[allow(dead_code)]
mod native;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use native::{evidence, start_central, wait_until, NativeHost, OwnedChild};
use serde_json::{json, Value};

/// Sending session on host A.
const S: &str = "0a6f3c2e-5b1d-4e8a-9c7f-1d2e3f4a5b6c";
/// Recipient session on host B, granted to A's agent key.
const T: &str = "7c1e9a4b-2d3f-4a5b-8c6d-9e0f1a2b3c4d";
/// Another session on host B, granted to A's agent key and then ungranted.
const U: &str = "e4d3c2b1-a0f9-4e8d-b7c6-a5b4c3d2e1f0";
/// Only ever in message text.
const SECRET: &str = "nativesecretpangolin";
const REPLY: &str = "replysecretaxolotl";
const LOST: &str = "lostacksecretnarwhal";
const LATE: &str = "offlinesecretokapi";
const TAMPERED: &str = "tamperedsecretquokka";
const STOLEN: &str = "substitutedsecretdugong";
const REVOKED: &str = "revokedsecretsaola";
const EXPIRED: &str = "expiredsecretbilby";
const GRANTED: &str = "grantedsecretnumbat";
const UNGRANTED: &str = "ungrantedsecretquoll";
const TURN_USER: &str = "loopback";
const TURN_PASS: &str = "loopback-only-native-secret";

enum Mode {
    Direct,
    Turn { port: u16 },
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_agent_messages_cross_only_the_private_channel_and_are_typed_once() {
    journey(Mode::Direct, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs NATIVE_AGENT_TURNSERVER (a coturn turnserver binary)"]
async fn forced_turn_agent_messages_cross_only_the_private_channel_and_are_typed_once() {
    let turnserver = rtc::required_env("NATIVE_AGENT_TURNSERVER");
    let temp = tempfile::tempdir().unwrap();
    let port = free_udp_port();
    let turn = Turn::start(&turnserver, temp.path(), port).await;
    journey(Mode::Turn { port }, Some(&turn)).await;
    let log = std::fs::read_to_string(&turn.log).unwrap_or_default();
    println!(
        "COTURN allocations: {}",
        log.lines()
            .filter(|l| l.contains("ALLOCATE") || l.contains("allocation"))
            .count()
    );
    assert!(
        log.contains("ALLOCATE") && log.contains("success"),
        "coturn never granted the native client an allocation:\n{log}"
    );
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

async fn journey(mode: Mode, _turn: Option<&Turn>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let central = start_central(&root.join("central")).await;
    let sender_transport = match mode {
        // No candidate addresses configured: the native client finds its own.
        Mode::Direct => json!({}),
        Mode::Turn { port } => json!({
            "ice_servers": [{
                "urls": [format!("turn:127.0.0.1:{port}?transport=udp")],
                "username": TURN_USER,
                "credential": TURN_PASS,
            }],
            "ice_transport_policy": "relay",
        }),
    };
    let expected_kind = match mode {
        Mode::Direct => "host",
        Mode::Turn { .. } => "relay",
    };
    let loopback = json!({"candidate_ips": ["127.0.0.1"]});
    let mut a = NativeHost::new(&root.join("a"), &central.url, sender_transport);
    let mut b = NativeHost::new(&root.join("b"), &central.url, loopback.clone());
    let mut c = NativeHost::new(&root.join("c"), &central.url, loopback);
    a.start().await;
    b.start().await;
    c.start().await;
    a.start_session(S).await;
    b.start_session(T).await;
    b.start_session(U).await;
    let (a_id, b_id, c_id) = (a.id(), b.id(), c.id());

    // Explicit LOCAL approvals, both directions, from each host's own CLI.
    let (a_public, _a_kid) = a.agent_key();
    let a_on_b = b.approve_agent(&a_public, &[T, U]);
    // C (the impostor) would accept A as well: only the pinned key tells them apart.
    c.approve_agent(&a_public, &[T]);
    a.pair_peer(&b.descriptor(), T);
    a.pair_peer(&b.descriptor(), U);
    let (b_public, _) = b.agent_key();
    a.approve_agent(&b_public, &[S]);
    b.pair_peer(&a.descriptor(), S);

    let evidence_a = root.join("evidence-a.jsonl");
    let drop_dir = root.join("drop-a");
    std::fs::create_dir_all(&drop_dir).unwrap();
    let sender_env = [
        ("CONVERSATIONS_TEST_PEER_EVIDENCE", evidence_a.as_path()),
        ("CONVERSATIONS_TEST_PEER_DROP_REPLY", drop_dir.as_path()),
    ];
    let mut sender = a.mcp(S, false, &sender_env).await;
    let mut receiver = b.mcp(T, true, &[]).await;
    let u_delivery = b.mcp(U, true, &[]).await;

    // 1. A real send from S's agent: queued by B after its own checks, then
    //    typed into T's terminal exactly once by guarded delivery.
    let text = format!("please review {SECRET}");
    let sent = sender
        .send_message(T, &text, "k1", None)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "send failed: {e}\nrelay: {}\nA: {}\nB: {}",
                central.recorded.relay_log(),
                a.log(),
                b.log()
            )
        });
    assert_eq!(sent["status"], "queued", "{sent}");
    let id = sent["id"].as_str().unwrap().to_owned();
    wait_until("the message typed into T", 90, || {
        b.stdin_of(T).contains(SECRET)
    })
    .await;

    // Privacy first: central relayed only signaling.
    let leaks: Vec<String> = [SECRET, &a.work.display().to_string()]
        .iter()
        .flat_map(|needle| central.recorded.containing(needle))
        .collect();
    assert!(
        leaks.is_empty(),
        "central saw the private message or the source working directory: {leaks:?}"
    );
    assert_eq!(
        central.recorded.relay_requests(&b_id, "signed_op"),
        0,
        "an agent operation went through the relay in the clear"
    );

    // 2. The same idempotency key resends the same stored bytes: same id, no
    //    second message.
    let again = sender.send_message(T, &text, "k1", None).await.unwrap();
    assert_eq!(again["id"], id.as_str(), "{again}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(occurrences(&b.stdin_of(T), SECRET), 1);

    // 3. T replies. The reply's admission on A acknowledges the original on B
    //    in the same transaction, and S's status check (a fresh signed question
    //    on a new private channel) reports it.
    let reply = receiver
        .send_message(S, &format!("done {REPLY}"), "r1", Some(&id))
        .await
        .unwrap_or_else(|e| panic!("reply failed: {e}"));
    assert_eq!(reply["status"], "queued", "{reply}");
    let status = sender.status(&id).await.unwrap();
    assert_eq!(status["status"], "acknowledged", "{status}");
    assert_eq!(status["stale"], false, "{status}");
    let inbox = sender.receive(&[]).await.unwrap();
    let got = &inbox["messages"][0];
    assert!(
        got["text"].as_str().unwrap_or("").contains(REPLY),
        "{inbox}"
    );
    assert_eq!(got["sender_session_id"], T, "{inbox}");

    // 4. Lost answer: B admits and executes, the sender never sees B's signed
    //    reply. The outcome is uncertain, never success; B's effect is observed
    //    independently, and a retry with the same key types nothing new.
    std::fs::write(drop_dir.join("drop"), b"once").unwrap();
    let lost_text = format!("second {LOST}");
    let lost = sender.send_message(T, &lost_text, "k2", None).await;
    let error = lost.expect_err("a dropped answer must not read as success");
    assert!(error.contains("no signed answer"), "{error}");
    assert!(!drop_dir.join("drop").exists(), "the drop hook never fired");
    wait_until("the lost-answer message typed into T", 90, || {
        b.stdin_of(T).contains(LOST)
    })
    .await;
    // The sending agent's MCP restarts before the retry: the retry comes from
    // the durable send row, so B holds exactly the operation it already ran.
    sender.close().await;
    let mut sender = a.mcp(S, false, &sender_env).await;
    let retried = sender
        .send_message(T, &lost_text, "k2", None)
        .await
        .unwrap();
    assert!(
        ["queued", "received", "acknowledged"].contains(&retried["status"].as_str().unwrap()),
        "{retried}"
    );
    let lost_id = retried["id"].as_str().unwrap().to_owned();
    assert_eq!(
        b.authz_count(&format!(
            "SELECT COUNT(*) FROM agent_inbox WHERE message_id = '{lost_id}' AND text LIKE '%{LOST}%'"
        )),
        1,
        "the retry after a sender restart is not the operation B ran"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(occurrences(&b.stdin_of(T), LOST), 1);

    // 4b. Lost answer, then expiry: retry must neither resend nor silently
    //     query B. The caller explicitly checks status before deciding whether
    //     a new idempotency key is appropriate. Here B already received it.
    std::fs::write(drop_dir.join("drop"), b"once").unwrap();
    let expired_text = format!("third {EXPIRED}");
    let error = sender
        .send_message(T, &expired_text, "k8", None)
        .await
        .expect_err("a dropped answer must not read as success");
    assert!(error.contains("no signed answer"), "{error}");
    assert!(!drop_dir.join("drop").exists(), "the drop hook never fired");
    wait_until("the expiring message typed into T", 90, || {
        b.stdin_of(T).contains(EXPIRED)
    })
    .await;
    let stored = || -> (String, Vec<u8>, Vec<u8>) {
        rusqlite::Connection::open_with_flags(
            a.runtime.join("authz.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap()
        .query_row(
            "SELECT message_id,payload,sig FROM agent_sends
             WHERE source_session=?1 AND target_session=?2 AND idem='k8'",
            [S, T],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    };
    let before = stored();
    tokio::time::sleep(Duration::from_secs(62)).await;
    let relay_path = format!("/api/host-relay/{b_id}/request");
    let requests = central.recorded.lines(&relay_path).len();
    let unknown = sender
        .send_message(T, &expired_text, "k8", None)
        .await
        .expect_err("an expired unanswered send requires explicit status");
    assert!(
        unknown.contains("delivery unknown")
            && unknown.contains(&before.0)
            && unknown.contains("messages_status"),
        "{unknown}"
    );
    assert_eq!(
        central.recorded.lines(&relay_path).len(),
        requests,
        "expired retry made a network request"
    );
    assert_eq!(
        stored(),
        before,
        "expired retry changed the signed operation"
    );
    let status = sender.status(&before.0).await.unwrap();
    assert!(
        ["queued", "received", "acknowledged"].contains(&status["status"].as_str().unwrap()),
        "{status}"
    );
    assert_eq!(status["stale"], false, "{status}");
    let requests = central.recorded.lines(&relay_path).len();
    let recovered = sender
        .send_message(T, &expired_text, "k8", None)
        .await
        .unwrap();
    assert_eq!(recovered["id"], before.0);
    assert_eq!(
        central.recorded.lines(&relay_path).len(),
        requests,
        "retry after explicit status resent the known message"
    );
    assert_eq!(stored(), before);
    assert_eq!(
        b.authz_count(&format!(
            "SELECT COUNT(*) FROM agent_inbox WHERE message_id='{}'",
            before.0
        )),
        1
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(occurrences(&b.stdin_of(T), EXPIRED), 1);

    // 5. B's owner removes ONE session grant (the key stays trusted): U was
    //    reachable, now B signs a refusal for it, nothing more is typed into U,
    //    and T, still granted, keeps working (steps 6-9).
    let granted = sender
        .send_message(U, &format!("for u {GRANTED}"), "k9", None)
        .await
        .unwrap_or_else(|e| panic!("the granted session failed: {e}"));
    assert_eq!(granted["status"], "queued", "{granted}");
    wait_until("the granted message typed into U", 90, || {
        b.stdin_of(U).contains(GRANTED)
    })
    .await;
    b.authz_sql(&format!(
        "UPDATE grants SET revoked_at = strftime('%s','now') \
         WHERE cred_id = '{a_on_b}' AND session_id = '{U}'"
    ));
    let refused = sender
        .send_message(U, &format!("not for you {UNGRANTED}"), "k3", None)
        .await
        .expect_err("an ungranted session must refuse");
    assert!(refused.contains("refused"), "{refused}");
    assert_eq!(
        b.authz_count(&format!(
            "SELECT COUNT(*) FROM agent_inbox WHERE text LIKE '%{UNGRANTED}%'"
        )),
        0,
        "an ungranted message was admitted"
    );
    u_delivery.close().await;

    // 6. Central rewrites the host's answer SDP: the signed answer no longer
    //    matches, so no channel opens and nothing is sent or typed.
    *central.hostile.answer.lock().unwrap() = Some(Box::new(|value: &mut Value| {
        if value["type"] == "peer_answer" {
            let sdp = value["sdp"].as_str().unwrap_or("").to_owned();
            value["sdp"] = json!(sdp.replacen("a=", "a=x-tampered:1\r\na=", 1));
        }
    }));
    let tampered = sender
        .send_message(T, &format!("x {TAMPERED}"), "k4", None)
        .await;
    *central.hostile.answer.lock().unwrap() = None;
    assert!(tampered.is_err(), "a tampered answer was accepted");

    // 7. Central routes B's signaling to C, which signs with its own key: the
    //    PINNED key from the local pairing refuses it.
    *central.hostile.reroute_peer.lock().unwrap() = Some((b_id.clone(), c_id.clone()));
    let stolen = sender
        .send_message(T, &format!("x {STOLEN}"), "k5", None)
        .await;
    *central.hostile.reroute_peer.lock().unwrap() = None;
    assert!(stolen.is_err(), "an impostor host was accepted");
    assert_eq!(
        c.authz_count("SELECT COUNT(*) FROM agent_inbox"),
        0,
        "the impostor received a message"
    );

    // 8. B's owner revokes A's agent key: B refuses the handshake itself. That
    //    refusal is unsigned, so the sender reports no signed answer rather
    //    than an outcome; B's inbox shows nothing was admitted. B's own sending
    //    to A is unaffected.
    b.revoke(&a_on_b);
    let revoked = sender
        .send_message(T, &format!("x {REVOKED}"), "k6", None)
        .await
        .expect_err("a revoked key must be refused");
    assert!(revoked.contains("no signed answer"), "{revoked}");
    assert_eq!(
        b.authz_count(&format!(
            "SELECT COUNT(*) FROM agent_inbox WHERE text LIKE '%{REVOKED}%'"
        )),
        0,
        "a revoked key was admitted"
    );
    let still = receiver
        .send_message(S, "still here", "r2", None)
        .await
        .unwrap_or_else(|e| panic!("the unaffected direction failed: {e}"));
    assert_eq!(still["status"], "queued", "{still}");

    // 9. Recipient host offline: T's reply to S finds no signed answer and
    //    stays unconfirmed (truthfully, not "absent" or "queued"). Once A is
    //    back, the same key admits it exactly once, from the stored bytes.
    let _ = a_id;
    a.stop().await;
    let late_text = format!("later {LATE}");
    let offline = receiver
        .send_message(S, &late_text, "r3", None)
        .await
        .expect_err("an offline recipient cannot confirm anything");
    assert!(offline.contains("no signed answer"), "{offline}");
    a.start().await;
    let late = receiver
        .send_message(S, &late_text, "r3", None)
        .await
        .unwrap_or_else(|e| panic!("retry after restart failed: {e}"));
    assert_eq!(late["status"], "queued", "{late}");
    let held = a.authz_count(&format!(
        "SELECT COUNT(*) FROM agent_inbox WHERE text LIKE '%{LATE}%'"
    ));
    assert_eq!(held, 1, "the retried message was not admitted exactly once");
    // ...and S's restarted agent has it typed into its terminal exactly once.
    a.restart_session(S).await;
    let s_delivery = a.mcp(S, true, &[]).await;
    wait_until("the late message typed into S", 90, || {
        a.stdin_of(S).contains(LATE)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(occurrences(&a.stdin_of(S), LATE), 1);
    s_delivery.close().await;

    for needle in [
        SECRET, REPLY, LOST, EXPIRED, GRANTED, UNGRANTED, TAMPERED, STOLEN, REVOKED, LATE,
    ] {
        let leaks = central.recorded.containing(needle);
        assert!(leaks.is_empty(), "central saw {needle}: {leaks:?}");
        assert!(
            !rtc::exposes(&central.stored(), needle),
            "central stored {needle}"
        );
    }
    for needle in [TAMPERED, STOLEN, REVOKED] {
        assert!(!b.stdin_of(T).contains(needle), "{needle} was typed");
    }
    assert!(
        !b.stdin_of(U).contains(UNGRANTED),
        "an ungranted message was typed"
    );
    assert_eq!(occurrences(&b.stdin_of(U), GRANTED), 1);

    // Evidence from the native client's own transport, not configuration.
    let lines = evidence(&evidence_a);
    println!(
        "EVIDENCE {}",
        json!({"mode": expected_kind, "pairs": lines})
    );
    assert!(!lines.is_empty(), "the native client recorded no channel");
    assert!(
        lines.iter().all(|l| l["local_kind"] == expected_kind),
        "{lines:?}"
    );
    sender.close().await;
    receiver.close().await;
}

// ------------------------------------------------------------------- turn --

struct Turn {
    _child: OwnedChild,
    log: std::path::PathBuf,
}

impl Turn {
    async fn start(binary: &str, root: &Path, port: u16) -> Self {
        let log = root.join("coturn.log");
        let child = std::process::Command::new(binary)
            .args([
                "-n",
                "-v",
                "--listening-ip=127.0.0.1",
                "--relay-ip=127.0.0.1",
                &format!("--listening-port={port}"),
                "--lt-cred-mech",
                &format!("--user={TURN_USER}:{TURN_PASS}"),
                "--realm=loopback",
                "--allow-loopback-peers",
                "--no-tls",
                "--no-dtls",
                "--no-tcp-relay",
                "--min-port=50000",
                "--max-port=50100",
                "--no-cli",
                &format!("--pidfile={}", root.join("turnserver.pid").display()),
                &format!("--userdb={}", root.join("turndb").display()),
                "--log-file=stdout",
                "--simple-log",
            ])
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let child = OwnedChild(child);
        wait_until("coturn listening", 20, || {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains("Relay ports initialization done")
        })
        .await;
        Self { _child: child, log }
    }
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
