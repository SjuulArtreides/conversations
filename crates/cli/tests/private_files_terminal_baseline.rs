#![cfg(unix)]
#![allow(dead_code)]
//! A raw probe of the agreed private wire for files and the terminal, with no
//! page helper for either: a signed `session.send_files` and its raw `chunk`
//! messages, and a signed `terminal.subscribe`, sent on a real private channel
//! by the production `privateChannel.ts` of whichever source tree is built.
//! It asserts the effects the feature promises (the exact file staged and
//! pasted into the session's terminal; the terminal's screen on the channel)
//! within bounded waits, and prints what it observed (`EVIDENCE {...}`) first.
//! The same file runs against a pre-feature tree (expected to fail on those
//! effects, not on compilation) and against the integrated tree.
//! Shares its fixture with `private_files_terminal.rs`; run it the same way:
//!
//! ```sh
//! cargo test -p conversations-cli --test private_files_terminal_baseline -- --ignored --nocapture
//! ```

include!("support/files_terminal.rs");

const PROBE_NAME: &str = "probenamemarkchinquapin.txt";
const PROBE_MARK: &str = "probebytesmarkbunchberry";

#[test]
#[ignore = "needs PRIVATE_TRANSPORT_CHROME, PRIVATE_TRANSPORT_NODE and PRIVATE_TRANSPORT_NODE_MODULES"]
fn the_agreed_private_wire_stages_the_file_and_streams_the_terminal() {
    let chrome = required_env("PRIVATE_TRANSPORT_CHROME");
    let node = required_env("PRIVATE_TRANSPORT_NODE");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(probe(chrome, node));
}

/// Poll `ready` for at most `secs`; `true` if it became ready.
async fn within(secs: u64, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ready()
}

async fn probe(chrome: String, node: String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let static_dir = build_static(&node, &root);
    let central = start_central(&root, &static_dir).await;
    let mut host = Host::new(&root, &central.url);
    host.start(&central).await;
    let v: &[(&str, &str)] = &[
        ("S", SESSION),
        ("PROBE_NAME", PROBE_NAME),
        ("PROBE_MARK", PROBE_MARK),
    ];
    let mut browser = Browser::launch(&chrome, &root.join("chrome"), &central.url).await;
    browser
        .run(&format!(
            "pt.host = {{ id: {:?}, key: {:?} }}; return true;",
            host.id(),
            host.public_key()
        ))
        .await;
    let starter = browser.run("return await pt.extraKey('starter');").await;
    host.enroll(
        "es256",
        starter["public"].as_str().unwrap(),
        &["session.start".into()],
    );
    let started = browser
        .run(&format!(
            "const c = await pt.channel.openPrivateChannel({{host: pt.host, key: pt.keys.starter, dlg: null}});
             try {{ const r = await c.operation('session.start', {SESSION:?}, {{session: {SESSION:?}, agent: 'claude'}}); return {{status: r.status}}; }} finally {{c.close();}}"
        ))
        .await;
    assert_eq!(started["status"], "executed", "{started}");
    let ready = host.stubs.join(format!("claude.stdin.{SESSION}"));
    wait("the agent to start", || ready.exists()).await;
    let main = browser.run("return await pt.mainKey();").await;
    host.enroll(
        "es256",
        main["public"].as_str().unwrap(),
        &[
            format!("history.read:{SESSION}"),
            format!("session.write:{SESSION}"),
        ],
    );

    // Every data channel the page opens from here on, and every non-reply
    // message the host sends on it.
    assert_eq!(
        browser
            .run(
                "pt.raw = { dcs: [], seen: [] };
                 const create = RTCPeerConnection.prototype.createDataChannel;
                 RTCPeerConnection.prototype.createDataChannel = function (...a) {
                   const dc = create.apply(this, a);
                   pt.raw.dcs.push(dc);
                   dc.addEventListener('message', (e) => {
                     if (typeof e.data !== 'string') return;
                     try { const m = JSON.parse(e.data); if (m.t !== 'reply') pt.raw.seen.push(m); } catch (err) { pt.raw.seen.push({ unparsed: true }); }
                   });
                   return dc;
                 };
                 pt.channels.probe = await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.main, dlg: null });
                 return pt.raw.dcs.length === 1;",
            )
            .await,
        json!(true)
    );

    // Files: the signed operation, then the bytes as raw `chunk` messages.
    let sent = browser
        .run(&js(
            "const dc = pt.raw.dcs[0];
             const bytes = pt.bytes(%PROBE_MARK%);
             const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
             const sha256 = Array.from(digest, (b) => b.toString(16).padStart(2, '0')).join('');
             const transfer = Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, '0')).join('');
             let reply;
             try {
               reply = await pt.channels.probe.operation('session.send_files', %S%, { transfer, files: [{ name: %PROBE_NAME%, size: bytes.length, sha256 }] });
             } catch (e) { return { error: String(e && e.message || e), open: dc.readyState }; }
             if (dc.readyState === 'open') dc.send(JSON.stringify({ t: 'chunk', transfer, file: 0, offset: 0, data: pt.auth.toBase64Url(bytes) }));
             return { status: reply.status, detail: reply.detail ?? null, open: dc.readyState };",
            v,
        ))
        .await;
    let staged = within(15, || !pastes(&host, SESSION).0.is_empty()).await;
    let (pasted, rest) = pastes(&host, SESSION);
    let outcomes = browser
        .eval("JSON.stringify(pt.raw.seen.filter((m) => m.t === 'transfer'))")
        .await;

    // The terminal: a signed subscribe, then whatever the host streams.
    let subscribed = browser
        .run(
            &js(
                "const dc = pt.raw.dcs[0];
                 if (dc.readyState !== 'open') return { open: dc.readyState };
                 try {
                   const r = await pt.channels.probe.operation('terminal.subscribe', %S%, {});
                   return { status: r.status, detail: r.detail ?? null, body: r.body ?? null, open: dc.readyState };
                 } catch (e) { return { error: String(e && e.message || e), open: dc.readyState }; }",
                v,
            ),
        )
        .await;
    let screen = format!(
        "pt.raw.seen.filter((m) => m.t === 'terminal').map((m) => {{
           const b = (m.data || '').replace(/-/g, '+').replace(/_/g, '/');
           return atob(b + '='.repeat((4 - b.length % 4) % 4));
         }}).join('').includes({SCREEN_MARK:?})"
    );
    let mut streamed = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if browser.eval(&screen).await == json!(true) {
            streamed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let terminal_messages = browser
        .eval("pt.raw.seen.filter((m) => m.t === 'terminal').length")
        .await;
    let stage = files_under(&host.root.join("tmp"));
    let evidence = json!({
        "send_files": sent,
        "pasted_within_15s": staged,
        "pasted": pasted.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "other_input": rest,
        "staged_files": stage.len(),
        "transfer_outcomes": outcomes,
        "subscribe": subscribed,
        "screen_within_15s": streamed,
        "terminal_messages": terminal_messages,
        "channel": browser.eval("pt.raw.dcs[0].readyState").await,
    });
    println!("EVIDENCE {evidence}");

    assert!(staged, "no file pasted within 15 s: {evidence}");
    assert_eq!(pasted.len(), 1, "{evidence}");
    assert_eq!(rest, "", "{evidence}");
    assert_staged(&host, &pasted[0], PROBE_NAME, PROBE_MARK.as_bytes());
    assert!(streamed, "no terminal screen within 15 s: {evidence}");
    host.stop();
}
