#![cfg(unix)]
//! Files, images and the terminal over the private browser <-> host channel,
//! end to end on loopback: the REAL central server (router, relay, SQLite) in
//! process, the REAL `conversations` host binary with its normal
//! backend and wrapper, and a REAL headless Chromium running the production
//! `privateChannel.ts` (type-stripped by Node, not rebuilt). The coding agent is
//! a recording stub that also prints to its terminal. TURN is a loopback
//! node-turn. Shared test fixtures live under `support/`.
//!
//! The test needs a Chromium and a Node with `node-turn` available, so it is
//! ignored by default and refuses to run without them (it never falls back):
//!
//! ```sh
//! PRIVATE_TRANSPORT_CHROME=/path/to/chrome \
//! PRIVATE_TRANSPORT_NODE=$(command -v node) \
//! PRIVATE_TRANSPORT_NODE_MODULES=/path/containing/node-turn \
//! cargo test -p conversations-cli --test private_files_terminal -- --ignored --nocapture
//! ```
//!
//! It also prints a JSON evidence line (`EVIDENCE {...}`).

include!("support/files_terminal.rs");
use rtc::{exposes, free_udp_port, status, Turn};
use std::collections::BTreeMap;

#[test]
#[ignore = "needs PRIVATE_TRANSPORT_CHROME, PRIVATE_TRANSPORT_NODE and PRIVATE_TRANSPORT_NODE_MODULES"]
fn files_images_and_the_terminal_cross_only_the_private_channel_and_land_only_in_the_private_stage()
{
    let chrome = required_env("PRIVATE_TRANSPORT_CHROME");
    let node = required_env("PRIVATE_TRANSPORT_NODE");
    let modules = required_env("PRIVATE_TRANSPORT_NODE_MODULES");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(scenario(chrome, node, modules));
}

async fn scenario(chrome: String, node: String, modules: String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let static_dir = build_static(&node, &root);
    let central = start_central(&root, &static_dir).await;
    let mut host = Host::new(&root, &central.url);
    host.start(&central).await;
    let host_id = host.id();
    let v: &[(&str, &str)] = &[
        ("SB", SESSION_B),
        ("S", SESSION),
        ("FILE_NAME", FILE_NAME),
        ("FILE_MARK", FILE_MARK),
        ("SECOND_NAME", SECOND_NAME),
        ("IMAGE_MARK", IMAGE_MARK),
        ("TAMPER_NAME", TAMPER_NAME),
        ("REVOKED_NAME", REVOKED_NAME),
        ("TURN_NAME", TURN_NAME),
        ("BETWEEN_NAME", BETWEEN_NAME),
        ("BETWEEN_MARK", BETWEEN_MARK),
        ("LOST_NAME", LOST_NAME),
        ("LOST_MARK", LOST_MARK),
        ("TURN_USER", TURN_USER),
        ("TURN_PASS", TURN_PASS),
    ];

    let mut browser = Browser::launch(&chrome, &root.join("chrome"), &central.url).await;
    browser
        .run(&format!(
            "pt.host = {{ id: {:?}, key: {:?} }}; return true;",
            host_id,
            host.public_key()
        ))
        .await;
    let starter = browser.run("return await pt.extraKey('starter');").await;
    host.enroll(
        "es256",
        starter["public"].as_str().unwrap(),
        &["session.start".into()],
    );
    for session in [SESSION, SESSION_B] {
        let started = browser
            .run(&format!(
                "const c = await pt.channel.openPrivateChannel({{host: pt.host, key: pt.keys.starter, dlg: null}});
                 try {{ const r = await c.operation('session.start', {session:?}, {{session: {session:?}, agent: 'claude'}}); return {{status: r.status}}; }} finally {{c.close();}}"
            ))
            .await;
        assert_eq!(started["status"], "executed", "{started}");
        let ready = host.stubs.join(format!("claude.stdin.{session}"));
        wait("the agent to start", || ready.exists()).await;
    }

    // ---- keys: the production key, a read-only follower, a writer to revoke ----
    let main = browser.run("return await pt.mainKey();").await;
    host.enroll(
        "es256",
        main["public"].as_str().unwrap(),
        &[
            format!("history.read:{SESSION}"),
            format!("session.write:{SESSION}"),
        ],
    );
    let follower = browser.run("return await pt.extraKey('follower');").await;
    let follower_kid = host.enroll(
        "es256",
        follower["public"].as_str().unwrap(),
        &[format!("history.read:{SESSION}")],
    );
    let revoked = browser.run("return await pt.extraKey('revoked');").await;
    let revoked_kid = host.enroll(
        "es256",
        revoked["public"].as_str().unwrap(),
        &[format!("session.write:{SESSION}")],
    );
    let finished = |mut child: Child| {
        let status = child.wait().unwrap();
        let mut err = String::new();
        std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).unwrap();
        assert!(status.success(), "authority command: {err}");
    };
    let grant_sql = |kid: &str, cap: &str, revoke: bool| {
        format!(
            "UPDATE grants SET revoked_at = {} WHERE cred_id = '{kid}' AND session_id = '{SESSION}' AND cap = '{cap}';",
            if revoke { "strftime('%s','now')" } else { "NULL" }
        )
    };
    let mut evidence = BTreeMap::new();

    // ---- direct channel: the terminal, its snapshot then its updates ----
    let opened = browser
        .run(
            "const c = await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.main, dlg: null });
             pt.channels.direct = c;
             return { peer: c.peer, pair: await c.selectedPair() };",
        )
        .await;
    assert!(opened["peer"].is_string(), "direct channel: {opened}");
    evidence.insert("direct_pair", opened["pair"].clone());
    let sub = browser
        .run(&js(
            "return await pt.follow(pt.channels.direct, 'direct', %S%);",
            v,
        ))
        .await;
    assert!(sub.is_u64(), "subscribe: {sub}");
    let direct_since = Instant::now();
    until(
        &mut browser,
        &format!("pt.seen('direct').text.includes({SCREEN_MARK:?})"),
        "the terminal's current screen",
    )
    .await;
    say(&host, SESSION, 1, UPDATE_MARK);
    until(
        &mut browser,
        &format!("pt.seen('direct').text.includes({UPDATE_MARK:?})"),
        "a terminal update",
    )
    .await;
    let direct = browser.run("return pt.seen('direct');").await;
    let kinds = direct["kinds"].as_array().unwrap();
    assert_eq!(kinds[0], "snapshot", "{direct}");
    assert!(
        kinds[1..].iter().all(|k| k == "update"),
        "an update arrived out of order or as a resync: {direct}"
    );
    assert!(direct["ended"].is_null(), "{direct}");
    let text = direct["text"].as_str().unwrap();
    assert!(
        text.contains(&format!("{SCREEN_MARK} {SESSION}")),
        "{direct}"
    );
    assert!(
        !text.contains(SESSION_B),
        "another session's terminal: {direct}"
    );
    assert!(
        text.find(SCREEN_MARK).unwrap() < text.find(UPDATE_MARK).unwrap(),
        "{direct}"
    );
    evidence.insert(
        "direct_terminal",
        json!({"kinds": kinds, "sizes": direct["sizes"]}),
    );

    // ---- files and an image: delivered only into the private stage ----
    let delivered = browser
        .run(&js(
            "return await pt.channels.direct.sendFiles(%S%, [
               { name: %FILE_NAME%, bytes: pt.bytes(%FILE_MARK%) },
               { name: %SECOND_NAME%, bytes: new Uint8Array(40000).fill(97) }]);",
            v,
        ))
        .await;
    assert_eq!(delivered, json!("delivered"), "{delivered}");
    let image: Vec<u8> = [
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..],
        IMAGE_MARK.as_bytes(),
    ]
    .concat();
    let pasted_image = browser
        .run(&js(
            "const header = Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a);
             const mark = pt.bytes(%IMAGE_MARK%);
             const image = new Uint8Array(header.length + mark.length);
             image.set(header); image.set(mark, header.length);
             return await pt.channels.direct.pasteImage(%S%, 'image/png', image);",
            v,
        ))
        .await;
    assert_eq!(pasted_image, json!("delivered"), "{pasted_image}");
    wait("three pastes", || pastes(&host, SESSION).0.len() >= 3).await;
    let (paths, rest) = pastes(&host, SESSION);
    assert_eq!(paths.len(), 3, "{paths:?}");
    assert_staged(
        &host,
        &paths[0],
        &format!("/0/{FILE_NAME}"),
        FILE_MARK.as_bytes(),
    );
    assert_staged(
        &host,
        &paths[1],
        &format!("/1/{SECOND_NAME}"),
        &[b'a'; 40000],
    );
    assert_staged(&host, &paths[2], ".png", &image);
    assert_eq!(
        rest, "",
        "something besides the pastes was typed (an Enter?)"
    );
    assert!(
        pastes(&host, SESSION_B).0.is_empty(),
        "a paste reached another session"
    );
    let staged = files_under(&host.root.join("tmp"));
    assert_eq!(staged.len(), 3, "{staged:?}");
    evidence.insert("staged", json!(staged.len()));

    // ---- every transfer is authorized again, and only once ----
    let replay = browser
        .run("return await pt.channels.direct.sendSigned(pt.wire.lastOp('session.send_files'));")
        .await;
    assert_eq!(status(&replay), "replayed", "{replay}");
    let altered = browser
        .run(&js(
            "const sha = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', pt.bytes('abc'))),
               (b) => b.toString(16).padStart(2, '0')).join('');
             const frame = await pt.auth.signPayload(pt.keys.main, pt.auth.operationPayload({
               hostId: pt.host.id, key: pt.keys.main, dlg: null, op: 'session.send_files', session: %S%,
               args: { transfer: pt.auth.randomHex(16), files: [{ name: %TAMPER_NAME%, size: 3, sha256: sha }] } }));
             const text = new TextDecoder().decode(pt.auth.fromBase64Url(frame.p)).replace('\"size\":3', '\"size\":4');
             return await pt.channels.direct.sendSigned({ ...frame, p: pt.auth.toBase64Url(pt.bytes(text)) });",
            v,
        ))
        .await;
    assert_eq!(status(&altered), "refused", "{altered}");
    let expired = browser
        .run(&js(
            "return await pt.channels.direct.operation('session.send_files', %S%,
               { transfer: pt.auth.randomHex(16), files: [{ name: %TAMPER_NAME%, size: 3, sha256: '00'.repeat(32) }] },
               { now: Math.floor(Date.now() / 1000) - 600, lifetime: 60 });",
            v,
        ))
        .await;
    assert_eq!(status(&expired), "refused", "{expired}");
    let wrong_session = browser
        .run(&js(
            "return await pt.channels.direct.sendFiles(%SB%, [{ name: %TAMPER_NAME%, bytes: pt.bytes('x') }]);",
            v,
        ))
        .await;
    assert_eq!(wrong_session["code"], "refused", "{wrong_session}");
    let flipped = browser
        .run(&js(
            "pt.wire.mode = 'flip';
             return await pt.channels.direct.sendFiles(%S%, [{ name: %TAMPER_NAME%, bytes: pt.bytes('tampered bytes') }]);",
            v,
        ))
        .await;
    assert_eq!(flipped, json!("refused"), "altered bytes: {flipped}");
    let duplicated = browser
        .run(&js(
            "pt.wire.mode = 'duplicate';
             return await pt.channels.direct.sendFiles(%S%, [{ name: %TAMPER_NAME%, bytes: new Uint8Array(20000) }]);",
            v,
        ))
        .await;
    assert_eq!(
        duplicated,
        json!("refused"),
        "duplicated chunk: {duplicated}"
    );
    let cancelled = browser
        .run(&js(
            "pt.wire.hold = true;
             const abort = new AbortController();
             const pending = pt.channels.direct.sendFiles(%S%, [{ name: %TAMPER_NAME%, bytes: pt.bytes('never') }], { signal: abort.signal });
             while (pt.wire.held.length === 0) await new Promise((r) => setTimeout(r, 20));
             abort.abort();
             await new Promise((r) => setTimeout(r, 200));
             pt.wire.release();
             return await pending;",
            v,
        ))
        .await;
    assert_eq!(cancelled, json!("cancelled"), "{cancelled}");

    // ---- revoked mid-transfer: after admission, before the bytes are complete ----
    let held = browser
        .run(&js(
            "const c = await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.revoked, dlg: null });
             pt.channels.revoked = c;
             pt.wire.hold = true;
             pt.pending = c.sendFiles(%S%, [{ name: %REVOKED_NAME%, bytes: pt.bytes('revoked bytes') }]);
             while (pt.wire.held.length === 0) await new Promise((r) => setTimeout(r, 20));
             return true;",
            v,
        ))
        .await;
    assert_eq!(held, json!(true), "{held}");
    finished(host.authz_sql(&grant_sql(&revoked_kid, "session.write", true)));
    let after_revoke = browser
        .run("pt.wire.release(); return await pt.pending;")
        .await;
    assert_eq!(after_revoke, json!("refused"), "{after_revoke}");
    browser
        .run("pt.channels.revoked.close(); return true;")
        .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (after, rest) = pastes(&host, SESSION);
    assert_eq!(
        after, paths,
        "a refused, altered or replayed transfer was pasted"
    );
    assert_eq!(rest, "");
    assert_eq!(
        files_under(&host.root.join("tmp")),
        staged,
        "a refused transfer was staged"
    );
    evidence.insert(
        "transfers",
        json!({"replay": status(&replay), "altered_op": status(&altered), "expired": status(&expired),
               "wrong_session": wrong_session["code"], "flipped": flipped, "duplicated": duplicated,
               "cancelled": cancelled, "revoked_mid_transfer": after_revoke}),
    );

    // ---- revoked mid-stream: the follower stops, the session does not ----
    browser
        .run(
            "pt.channels.follower = await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.follower, dlg: null });
             return true;",
        )
        .await;
    let other = browser
        .run(&js(
            "return await pt.follow(pt.channels.follower, 'wrong', %SB%);",
            v,
        ))
        .await;
    assert_eq!(other["code"], "refused", "wrong-session subscribe: {other}");
    browser
        .run(&js(
            "return await pt.follow(pt.channels.follower, 'follower', %S%);",
            v,
        ))
        .await;
    until(
        &mut browser,
        &format!("pt.seen('follower').text.includes({UPDATE_MARK:?})"),
        "the follower's snapshot",
    )
    .await;
    finished(host.authz_sql(&grant_sql(&follower_kid, "history.read", true)));
    say(&host, SESSION, 2, LATE_MARK);
    until(
        &mut browser,
        "pt.seen('follower').ended !== null",
        "the revoked follower to end",
    )
    .await;
    until(
        &mut browser,
        &format!("pt.seen('direct').text.includes({LATE_MARK:?})"),
        "the still-granted follower to see the update",
    )
    .await;
    let stopped = browser.run("return pt.seen('follower');").await;
    // A clean end from the host, at the next cursor: not a gap the page detected.
    assert_eq!(stopped["ended"]["reason"], "denied", "{stopped}");
    assert!(
        !stopped["text"].as_str().unwrap().contains(LATE_MARK),
        "an update reached a revoked follower: {stopped}"
    );
    // Granted again: the old subscription stays ended; only a fresh one reads.
    finished(host.authz_sql(&grant_sql(&follower_kid, "history.read", false)));
    say(&host, SESSION, 3, REGRANT_MARK);
    until(
        &mut browser,
        &format!("pt.seen('direct').text.includes({REGRANT_MARK:?})"),
        "the update after the regrant",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let still = browser.run("return pt.seen('follower');").await;
    assert_eq!(
        still["count"], stopped["count"],
        "a regrant revived an old stream: {still}"
    );
    browser
        .run(&js(
            "return await pt.follow(pt.channels.follower, 'fresh', %S%);",
            v,
        ))
        .await;
    until(
        &mut browser,
        &format!("pt.seen('fresh').text.includes({REGRANT_MARK:?})"),
        "a fresh subscription's snapshot",
    )
    .await;
    let fresh = browser.run("return pt.seen('fresh');").await;
    assert_eq!(fresh["kinds"][0], "snapshot", "{fresh}");
    assert!(
        fresh["text"].as_str().unwrap().contains(LATE_MARK),
        "the fresh snapshot is the current screen: {fresh}"
    );
    assert!(
        !host.stubs.join(format!("claude.done.{SESSION}")).exists(),
        "revocation ended the session"
    );
    browser
        .run("pt.channels.follower.close(); return true;")
        .await;
    evidence.insert(
        "mid_stream",
        json!({"ended": stopped["ended"], "frames_before_end": stopped["count"], "fresh": fresh["kinds"]}),
    );

    // ---- leases: a short signed lease renews, lapses, or is denied ----
    // Each subscribe is an ordinary signed op whose lifetime is the lease; the
    // host refuses every piece after its expiry unless a renewal was released.
    let subscribes = |kid: &str| {
        format!(
            "pt.wire.ops.filter((o) => o.k === {kid:?} && JSON.parse(new TextDecoder().decode(pt.auth.fromBase64Url(o.p))).op === 'terminal.subscribe').length"
        )
    };
    let main_kid = main["kid"].as_str().unwrap().to_owned();
    // The first follow has lived through its default lease's renewal so far;
    // this channel follows a terminal once, so it stops before the next ones.
    // Wait past half of the default 60 s lease, and then past the whole of it.
    let lease = Duration::from_secs(62);
    tokio::time::sleep(lease.saturating_sub(direct_since.elapsed())).await;
    let direct = browser.run("return pt.seen('direct');").await;
    assert!(
        direct["ended"].is_null(),
        "the default lease lapsed: {direct}"
    );
    browser
        .run("pt.follows.direct.handle.close(); return true;")
        .await;
    let before = browser.eval(&subscribes(&main_kid)).await.as_u64().unwrap();
    assert_eq!(
        browser
            .run(&js(
                "await pt.follow(pt.channels.direct, 'lease', %S%, { leaseSecs: 4 }); return true;",
                v,
            ))
            .await,
        json!(true)
    );
    tokio::time::sleep(Duration::from_secs(7)).await;
    say(&host, SESSION, 5, LEASE_MARK);
    until(
        &mut browser,
        &format!("pt.seen('lease').text.includes({LEASE_MARK:?})"),
        "an update past the first lease's expiry",
    )
    .await;
    let renewed = browser.run("return pt.seen('lease');").await;
    assert!(renewed["ended"].is_null(), "{renewed}");
    let renewals = browser.eval(&subscribes(&main_kid)).await.as_u64().unwrap() - before - 1;
    assert!(
        renewals >= 3,
        "renewed {renewals} times in 7 s of a 4 s lease"
    );
    browser
        .run("pt.follows.lease.handle.close(); return true;")
        .await;

    assert_eq!(
        browser
            .run(&js(
            "await pt.follow(pt.channels.direct, 'lapse', %S%, { leaseSecs: 3, autoRenew: false }); return true;",
            v,
            ))
            .await,
        json!(true)
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    say(&host, SESSION, 6, LAPSED_MARK);
    until(
        &mut browser,
        "pt.seen('lapse').ended !== null",
        "the lapsed lease to end",
    )
    .await;
    let lapsed = browser.run("return pt.seen('lapse');").await;
    assert_eq!(lapsed["ended"]["reason"], "expired", "{lapsed}");
    assert!(
        !lapsed["text"].as_str().unwrap().contains(LAPSED_MARK),
        "a piece was released after its lease expired: {lapsed}"
    );
    // One fresh signed subscribe: a new stream and a new snapshot.
    assert_eq!(
        browser
            .run(&js(
                "await pt.follow(pt.channels.direct, 'again', %S%); return true;",
                v,
            ))
            .await,
        json!(true)
    );
    until(
        &mut browser,
        &format!("pt.seen('again').text.includes({LAPSED_MARK:?})"),
        "the fresh subscription's snapshot",
    )
    .await;
    let again = browser.run("return pt.seen('again');").await;
    assert_eq!(again["kinds"][0], "snapshot", "{again}");
    browser
        .run("pt.follows.again.handle.close(); return true;")
        .await;

    // Denied at renewal: ends once as denied and is not retried.
    let follower_kid_js = follower["kid"].as_str().unwrap().to_owned();
    assert_eq!(
        browser
            .run(&js(
            "pt.channels.denied = await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.follower, dlg: null });
             await pt.follow(pt.channels.denied, 'deny', %S%, { leaseSecs: 4 }); return true;",
            v,
            ))
            .await,
        json!(true)
    );
    finished(host.authz_sql(&grant_sql(&follower_kid, "history.read", true)));
    until(
        &mut browser,
        "pt.seen('deny').ended !== null",
        "the denied renewal",
    )
    .await;
    let denied = browser.run("return pt.seen('deny');").await;
    assert_eq!(denied["ended"]["reason"], "denied", "{denied}");
    let tries = browser.eval(&subscribes(&follower_kid_js)).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        browser.eval(&subscribes(&follower_kid_js)).await,
        tries,
        "a denied lease was retried"
    );
    browser
        .run("pt.channels.denied.close(); return true;")
        .await;
    evidence.insert(
        "leases",
        json!({"default_lease_still_following": direct["count"], "renewals_in_7s_of_4s": renewals, "lapsed": lapsed["ended"], "fresh_after_lapse": again["kinds"], "denied": denied["ended"]}),
    );

    // ---- injected failures: after the host's hand-off, before the receipt ----
    // The terminal (wrapper) holds a claimed transfer at a debug-only barrier:
    // before staging, or after the paste and before its receipt.
    let hold = |name: &str| std::fs::write(host.barrier.join(format!("{name}-hold")), b"").unwrap();
    let release = |name: &str| {
        std::fs::remove_file(host.barrier.join(format!("{name}-hold"))).unwrap();
        std::fs::write(host.barrier.join(format!("{name}-release")), b"").unwrap();
    };
    let held = |name: &str| host.barrier.join(format!("{name}-held"));

    // Revoked between the host's final check and the terminal's staging: the
    // terminal re-checks, stages nothing, pastes nothing, and says so.
    let between = browser.run("return await pt.extraKey('between');").await;
    let between_kid = host.enroll(
        "es256",
        between["public"].as_str().unwrap(),
        &[format!("session.write:{SESSION}")],
    );
    let (pasted_before, _) = pastes(&host, SESSION);
    let staged_before = files_under(&host.root.join("tmp"));
    hold("stage");
    assert_eq!(
        browser
            .run(&js(
                "pt.channels.between = await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.between, dlg: null });
                 pt.pending = pt.channels.between.sendFiles(%S%, [{ name: %BETWEEN_NAME%, bytes: pt.bytes(%BETWEEN_MARK%) }]);
                 return true;",
                v,
            ))
            .await,
        json!(true)
    );
    let stage_held = held("stage");
    wait("the terminal to hold the claimed transfer", || {
        stage_held.exists()
    })
    .await;
    finished(host.authz_sql(&grant_sql(&between_kid, "session.write", true)));
    release("stage");
    let revoked_at_stage = browser.run("return await pt.pending;").await;
    assert_eq!(revoked_at_stage, json!("failed"), "{revoked_at_stage}");
    let (pasted_after, rest) = pastes(&host, SESSION);
    assert_eq!(
        pasted_after, pasted_before,
        "a transfer revoked before staging was pasted"
    );
    assert_eq!(rest, "");
    assert_eq!(
        files_under(&host.root.join("tmp")),
        staged_before,
        "a transfer revoked before staging was staged"
    );
    browser
        .run("pt.channels.between.close(); return true;")
        .await;

    // The receipt is lost: the terminal pasted, then went away before it
    // answered. The page is told `uncertain`, never `delivered`, and nothing
    // is sent again.
    let lost = browser.run("return await pt.extraKey('lost');").await;
    host.enroll(
        "es256",
        lost["public"].as_str().unwrap(),
        &[format!("session.write:{SESSION_B}")],
    );
    hold("receipt");
    assert_eq!(
        browser
            .run(&js(
                "pt.channels.lost = await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.lost, dlg: null });
                 pt.pending = pt.channels.lost.sendFiles(%SB%, [{ name: %LOST_NAME%, bytes: pt.bytes(%LOST_MARK%) }]);
                 return true;",
                v,
            ))
            .await,
        json!(true)
    );
    let receipt_held = held("receipt");
    wait("the terminal to hold its receipt", || receipt_held.exists()).await;
    let (lost_pasted, rest) = pastes(&host, SESSION_B);
    assert_eq!(
        lost_pasted.len(),
        1,
        "the held transfer was not pasted: {rest:?}"
    );
    assert_eq!(rest, "");
    assert_staged(&host, &lost_pasted[0], LOST_NAME, LOST_MARK.as_bytes());
    let sends = |kid_js: &str| {
        format!(
            "pt.wire.ops.filter((o) => o.k === pt.keys.{kid_js}.kid && JSON.parse(new TextDecoder().decode(pt.auth.fromBase64Url(o.p))).op === 'session.send_files').length"
        )
    };
    std::fs::write(host.stubs.join(format!("exit.{SESSION_B}")), b"").unwrap();
    let lost_outcome = browser.run("return await pt.pending;").await;
    assert_eq!(lost_outcome, json!("uncertain"), "{lost_outcome}");
    release("receipt");
    let tries = browser.eval(&sends("lost")).await;
    assert_eq!(tries, json!(1), "the uncertain transfer was sent again");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(browser.eval(&sends("lost")).await, tries);
    assert_eq!(pastes(&host, SESSION_B).0, lost_pasted, "pasted again");
    browser.run("pt.channels.lost.close(); return true;").await;
    evidence.insert(
        "injected",
        json!({"revoked_before_stage": revoked_at_stage, "receipt_lost_after_paste": lost_outcome,
               "lost_effect_present": true, "sends_of_uncertain": tries}),
    );

    // ---- forced relay through a loopback TURN server ----
    let turn_port = free_udp_port();
    let _turn = Turn::start(&node, &modules, turn_port, TURN_USER, TURN_PASS);
    let relayed = browser
        .run(&js(
            &format!(
                "const c = await pt.channel.openPrivateChannel({{ host: pt.host, key: pt.keys.main, dlg: null, relayOnly: true,
                   iceServers: [{{ urls: 'turn:127.0.0.1:{turn_port}?transport=udp', username: %TURN_USER%, credential: %TURN_PASS% }}] }});
                 pt.channels.turn = c;
                 const sent = await c.sendFiles(%S%, [{{ name: %TURN_NAME%, bytes: pt.bytes(%FILE_MARK%) }}]);
                 await pt.follow(c, 'turn', %S%);
                 return {{ sent, pair: await c.selectedPair() }};"
            ),
            v,
        ))
        .await;
    assert_eq!(relayed["sent"], "delivered", "{relayed}");
    assert_eq!(relayed["pair"]["localType"], "relay", "{relayed}");
    say(&host, SESSION, 4, TURN_MARK);
    until(
        &mut browser,
        &format!("pt.seen('turn').text.includes({TURN_MARK:?})"),
        "a terminal update over TURN",
    )
    .await;
    let turn = browser.run("return pt.seen('turn');").await;
    assert_eq!(turn["kinds"][0], "snapshot", "{turn}");
    assert!(
        turn["text"].as_str().unwrap().contains(SCREEN_MARK)
            || turn["text"].as_str().unwrap().contains(REGRANT_MARK),
        "{turn}"
    );
    let (paths, rest) = pastes(&host, SESSION);
    assert_eq!(paths.len(), 4, "{paths:?}");
    assert_staged(
        &host,
        &paths[3],
        &format!("/0/{TURN_NAME}"),
        FILE_MARK.as_bytes(),
    );
    assert_eq!(rest, "");
    browser.run("pt.channels.turn.close(); return true;").await;
    evidence.insert("relay_pair", relayed["pair"].clone());

    // ---- the machine clipboard was never touched ----
    assert!(
        !host.stubs.join("clipboard.used").exists(),
        "a clipboard tool ran: {}",
        std::fs::read_to_string(host.stubs.join("clipboard.used")).unwrap_or_default()
    );

    // ---- privacy: what the central server could see ----
    let mut central_bytes = std::fs::read(&central.db).unwrap();
    central_bytes
        .extend(std::fs::read(central.db.with_extension("sqlite-wal")).unwrap_or_default());
    let memory = central.memory(&[SESSION, SESSION_B]).await;
    let signaling: Vec<String> = central
        .recorded
        .bodies("/api/host-relay/")
        .into_iter()
        .filter(|b| b.contains("peer_"))
        .collect();
    assert!(!signaling.is_empty());
    let stage_root = host.root.join("tmp").display().to_string();
    for private in [
        SCREEN_MARK,
        UPDATE_MARK,
        LATE_MARK,
        REGRANT_MARK,
        TURN_MARK,
        LEASE_MARK,
        LAPSED_MARK,
        FILE_NAME,
        FILE_MARK,
        SECOND_NAME,
        IMAGE_MARK,
        TAMPER_NAME,
        REVOKED_NAME,
        TURN_NAME,
        BETWEEN_NAME,
        BETWEEN_MARK,
        LOST_NAME,
        LOST_MARK,
        "tampered bytes",
        "revoked bytes",
        &stage_root,
    ] {
        assert!(
            central.recorded.containing(private).is_empty(),
            "{private} reached central: {:?}",
            central.recorded.containing(private)
        );
        assert!(
            !exposes(&central_bytes, private),
            "{private} in the central database"
        );
        assert!(
            !exposes(&memory, private),
            "{private} in central's live terminal memory"
        );
    }
    // The 40000-byte second file is a run of 'a': no run of it went anywhere.
    let run = "a".repeat(64);
    assert!(central.recorded.containing(&run).is_empty());
    assert!(!exposes(&central_bytes, &run));
    let live_paths: Vec<String> = central
        .recorded
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|(line, _, _)| line.clone())
        .filter(|line| {
            line.contains("/api/live")
                || line.contains("/api/sessions/")
                || line.contains("/files")
                || line.contains("/clipboard")
        })
        .collect();
    assert!(
        live_paths.is_empty(),
        "a central terminal, file or clipboard path ran: {live_paths:?}"
    );
    evidence.insert(
        "central_requests",
        json!(central.recorded.0.lock().unwrap().len()),
    );
    evidence.insert("signaling_messages", json!(signaling.len()));
    println!("EVIDENCE {}", serde_json::to_string(&evidence).unwrap());
    browser
        .run("pt.channels.direct.close(); return true;")
        .await;
}
