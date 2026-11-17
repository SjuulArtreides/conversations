#![cfg(unix)]
//! The private browser <-> host channel, end to end on loopback, with nothing
//! stubbed between the parts that matter: the REAL central server (router,
//! relay, SQLite) in process, the REAL `arrakis-conversations` host binary with
//! its normal backend and wrapper, and a REAL headless Chromium running the
//! production `privateChannel.ts` helper (type-stripped by Node, not rebuilt).
//! The coding agent is a recording stub. TURN is a loopback node-turn.
//!
//! The test needs a Chromium and a Node with `node-turn` available, so it is
//! ignored by default and refuses to run without them (it never falls back):
//!
//! ```sh
//! PRIVATE_TRANSPORT_CHROME=/path/to/chrome \
//! PRIVATE_TRANSPORT_NODE=$(command -v node) \
//! PRIVATE_TRANSPORT_NODE_MODULES=/path/containing/node-turn \
//! cargo test -p conversations-cli --test private_transport -- --ignored --nocapture
//! ```
//!
//! It also prints a JSON evidence line (`EVIDENCE {...}`) with the selected
//! candidate pairs and the privacy scan.

#[path = "support/private_rtc.rs"]
#[allow(dead_code)]
mod rtc;
use rtc::{
    contains, exposed, exposes, free_udp_port, required_env, start_central, status, wait, Browser,
    Central, OwnedChild, Recorded, Turn,
};

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};

const SESSION: &str = "5b0c7a52-93d4-4c61-8f0e-2a6c1d7e4f10";
const SESSION_B: &str = "c4e2a9d1-0b7f-4e38-a5d6-91f3b2c8e702";
const BIN: &str = env!("CARGO_BIN_EXE_arrakis-conversations");
/// In every transcript the stub agent writes (so it reaches local history).
const TRANSCRIPT_MARK: &str = "transcriptmarkzebrawood";
/// Added later to an ALREADY STORED message (same uuid, same seq): a reader
/// that only asks for newer positions would never see it.
const MUTATION_MARK: &str = "mutationmarkbristlecone";
/// Typed only through the private channel.
const INPUT_MARK: &str = "inputmarkquillwort";
/// Searched for only through the private channel.
const QUERY_MARK: &str = "querymarkhornbeam";
const TURN_USER: &str = "loopback";
const TURN_PASS: &str = "loopback-only-secret";

// ----------------------------------------------------------------- host --

struct Host {
    root: PathBuf,
    runtime: PathBuf,
    /// The host's debug-build delivery barrier (armed only by files in it).
    barrier: PathBuf,
    work: PathBuf,
    stubs: PathBuf,
    settings: PathBuf,
    central: String,
    child: Option<Child>,
}

impl Host {
    fn new(root: &Path, central: &str) -> Self {
        let (runtime, work, stubs) = (root.join("rt"), root.join("work"), root.join("stubs"));
        let barrier = root.join("barrier");
        for dir in [&work, &stubs, &barrier] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // Loopback only: the host offers exactly this candidate address.
        let settings = root.join("local-host.json");
        std::fs::write(
            &settings,
            json!({"transport": {"candidate_ips": ["127.0.0.1"]}}).to_string(),
        )
        .unwrap();
        // The stub agent records how it was run, writes a transcript in Claude's
        // on-disk format, turns off terminal echo (so typed input is never
        // mirrored back as output), then records everything it is sent.
        // A unique stop file controls this fixture's lifetime. Never signal
        // wrapper/agent PIDs read from an old log: those PIDs can be reused.
        let stub = r#"#!/usr/bin/python3
import json, os, select, subprocess, sys, time
from pathlib import Path
stubs = Path(__STUBS__)
args = sys.argv[1:]
session = next((args[i + 1] for i, arg in enumerate(args[:-1]) if arg in ('--session-id', '--resume')), '')
with (stubs / 'claude.runs').open('a') as runs:
    runs.write(f'{os.getpid()} {os.getppid()} ' + ' '.join(args) + '\n')
project = os.getcwd().replace('.', '-').replace('/', '-')
directory = Path(os.environ['HOME']) / '.claude' / 'projects' / project
directory.mkdir(parents=True, exist_ok=True)
transcript_path = directory / (session + '.jsonl')
def turn(extra):
    # A structured assistant turn: a tool call, then (once rewritten) its outcome.
    content = [{'type': 'text', 'text': 'checking'},
               {'type': 'tool_use', 'id': 'toolu_1', 'name': 'Bash',
                'input': {'command': 'grep -r __MARK__ .'}}] + extra
    return json.dumps({'type': 'assistant', 'uuid': session + '-a1', 'sessionId': session,
        'timestamp': '2026-11-03T10:00:01.000Z', 'message': {'id': 'msg_1', 'role': 'assistant',
        'model': 'stub', 'content': content}}) + '\n'
with transcript_path.open('a') as transcript:
    transcript.write(json.dumps({'type': 'user', 'uuid': session + '-u1', 'sessionId': session,
        'timestamp': '2026-11-03T10:00:00.000Z', 'message': {'role': 'user',
        'content': 'private transcript __MARK__ of ' + session}}) + '\n')
    transcript.write(turn([]))
subprocess.run(['/bin/stty', '-echo'], stderr=subprocess.DEVNULL)
deadline = time.monotonic() + 300
mutated = False
try:
    with (stubs / ('claude.stdin.' + session)).open('ab', buffering=0) as received:
        while stubs.exists() and not (stubs / 'stop').exists() and time.monotonic() < deadline:
            if not mutated and (stubs / ('mutate.' + session)).exists():
                mutated = True
                with transcript_path.open('a') as transcript:
                    transcript.write(turn([{'type': 'text', 'text': 'all green __MUTATION__'}]))
            ready, _, _ = select.select([0], [], [], 0.1)
            if ready:
                data = os.read(0, 4096)
                if not data:
                    break
                received.write(data)
finally:
    if stubs.exists():
        (stubs / ('claude.done.' + session)).write_text('exited')
"#
        .replace("__STUBS__", &serde_json::to_string(&stubs).unwrap())
        .replace("__MARK__", TRANSCRIPT_MARK)
        .replace("__MUTATION__", MUTATION_MARK);
        std::fs::write(stubs.join("claude"), stub).unwrap();
        std::fs::set_permissions(stubs.join("claude"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        Self {
            root: root.to_owned(),
            runtime,
            barrier,
            work,
            stubs,
            settings,
            central: central.to_owned(),
            child: None,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        // Inherited cmux and Conversations variables would point the host at a
        // real agent or another host.
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy();
            if key.starts_with("CMUX_") || key.starts_with("CONVERSATIONS_") {
                command.env_remove(&*key);
            }
        }
        command
            .env_remove("CLAUDE_PROJECTS_DIR")
            .env("HOME", &self.root)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("CONVERSATIONS_HOST_SETTINGS_FILE", &self.settings)
            .env("PATH", format!("{}:/usr/bin:/bin", self.stubs.display()))
            .current_dir(&self.work);
        command
    }

    async fn start(&mut self, central: &Central) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("host.log"))
            .unwrap();
        let child = self
            .command()
            .args(["--server", &self.central, "host", "--runtime-dir"])
            .arg(&self.runtime)
            .args(["--backend", "normal"])
            .env("CONVERSATIONS_TEST_DELIVERY_BARRIER", &self.barrier)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.child = Some(child);
        let status = format!("{}/api/host-relay/{}/status", central.url, self.id());
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let online = match reqwest::get(&status).await {
                Ok(response) => response.json::<Value>().await.ok(),
                Err(_) => None,
            };
            if online.is_some_and(|v| v["online"] == true) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the host did not attach to the real relay"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn id(&self) -> String {
        let output = self
            .command()
            .args(["credential", "--runtime-dir"])
            .arg(&self.runtime)
            .arg("identity")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .last()
            .unwrap()
            .to_owned()
    }

    /// The host's identity public key, as the user would pin it when pairing.
    fn public_key(&self) -> String {
        let key: Vec<u8> = rusqlite::Connection::open_with_flags(
            self.runtime.join("authz.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap()
        .query_row(
            "SELECT public_key FROM host_identity WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
        URL_SAFE_NO_PAD.encode(key)
    }

    /// Approve a key locally; returns its credential id.
    fn enroll(&self, alg: &str, public_key: &str, grants: &[String]) -> String {
        let mut command = self.command();
        command
            .args(["credential", "--runtime-dir"])
            .arg(&self.runtime)
            .args([
                "add",
                "--kind",
                "owner",
                "--alg",
                alg,
                "--public-key",
                public_key,
                "--label",
                "it",
            ]);
        for grant in grants {
            command.args(["--grant", grant]);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .nth(2)
            .unwrap()
            .to_owned()
    }

    /// Fixture-only grant removal/reactivation from another process. Credential
    /// revocation below uses the actual local CLI, not this SQL adapter.
    fn authz_sql(&self, sql: &str) -> Child {
        Command::new(
            std::env::var("PRIVATE_TRANSPORT_SQLITE3").unwrap_or("/usr/bin/sqlite3".into()),
        )
        .args(["-cmd", ".timeout 20000"])
        .arg(self.runtime.join("authz.db"))
        .arg(sql)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
    }

    fn revoke_credential(&self, kid: &str) -> Child {
        self.command()
            .args(["credential", "--runtime-dir"])
            .arg(&self.runtime)
            .args(["revoke", kid, "--json"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn stdin_of(&self, session: &str) -> String {
        std::fs::read_to_string(self.stubs.join(format!("claude.stdin.{session}")))
            .unwrap_or_default()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::write(self.stubs.join("stop"), b"stop");
        self.stop();
        // Observe completion through fixture-owned files, not process IDs.
        // The production wrapper exits when its recording agent exits.
        let sessions: Vec<_> = [SESSION, SESSION_B]
            .into_iter()
            .filter(|session| self.stubs.join(format!("claude.stdin.{session}")).exists())
            .collect();
        let deadline = Instant::now() + Duration::from_secs(5);
        while sessions
            .iter()
            .any(|session| !self.stubs.join(format!("claude.done.{session}")).exists())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

// --------------------------------------------------------------- browser --

/// Own subprocesses from spawn onward, including failed startup paths.
fn build_static(node: &str, root: &Path) -> PathBuf {
    let dir = root.join("static");
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    let lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../frontend/src/lib");
    for module in ["hostAuth", "hostStatements", "types", "privateChannel"] {
        let source = lib.join(format!("{module}.ts"));
        let output = Command::new(node)
            .args([
                "--input-type=module",
                "-e",
                "import { stripTypeScriptTypes } from 'node:module'; import { readFileSync } from 'node:fs'; process.stdout.write(stripTypeScriptTypes(readFileSync(process.argv[1], 'utf8'), { mode: 'strip' }));",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut js = String::from_utf8(output.stdout).unwrap();
        for import in ["hostAuth", "hostStatements", "types"] {
            js = js.replace(
                &format!("from \"./{import}\""),
                &format!("from \"./{import}.js\""),
            );
        }
        std::fs::write(dir.join("lib").join(format!("{module}.js")), js).unwrap();
    }
    std::fs::write(
        dir.join("harness.js"),
        r#"
import * as channel from "./lib/privateChannel.js";
import * as auth from "./lib/hostAuth.js";
const keys = {};
const channels = {};
// Extra synthetic browser keys, made the same way the production key is.
async function extraKey(name) {
  const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, ["sign", "verify"]);
  const publicRaw = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
  const kid = auth.toBase64Url(new Uint8Array(await crypto.subtle.digest("SHA-256", publicRaw)));
  keys[name] = { privateKey: pair.privateKey, publicRaw, kid };
  return { kid, public: auth.toBase64(publicRaw) };
}
async function mainKey() {
  keys.main = await auth.loadBrowserKey();
  return { kid: keys.main.kid, public: auth.toBase64(keys.main.publicRaw) };
}
window.pt = { channel, auth, keys, channels, extraKey, mainKey, host: null };
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("index.html"),
        r#"<!doctype html><meta charset="utf-8"><title>private transport</title><script type="module" src="/harness.js"></script>"#,
    )
    .unwrap();
    dir
}

// ------------------------------------------------------------------ TURN --

// ------------------------------------------------------------------ test --

#[test]
fn recording_agent_stops_when_fixture_unwinds_without_signalling_logged_pids() {
    let temp = tempfile::tempdir().unwrap();
    let fixture = Host::new(temp.path(), "http://127.0.0.1:1");
    let done = fixture.stubs.join(format!("claude.done.{SESSION}"));
    let ready = fixture.stubs.join(format!("claude.stdin.{SESSION}"));
    let mut child = OwnedChild(
        Command::new(fixture.stubs.join("claude"))
            .args(["--session-id", SESSION])
            .env("HOME", temp.path())
            .current_dir(&fixture.work)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(ready.exists(), "recording fixture did not start");
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _fixture = fixture;
        panic!("intentional fixture failure");
    }));
    assert!(failure.is_err());
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        child.0.try_wait().unwrap().is_some(),
        "recording agent leaked"
    );
    assert!(
        done.exists(),
        "agent did not exit through its owned stop signal"
    );
}

#[test]
#[ignore = "needs PRIVATE_TRANSPORT_CHROME, PRIVATE_TRANSPORT_NODE and PRIVATE_TRANSPORT_NODE_MODULES"]
fn history_search_and_input_cross_only_the_private_channel_and_every_operation_is_authorized_by_the_host(
) {
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

    // The owner starts through the private channel too: even a signed prompt
    // or cwd must not become plaintext central relay traffic.
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
    }
    let history = host.runtime.join("history.db");
    wait("local history of both sessions", || {
        history.exists()
            && rusqlite::Connection::open_with_flags(
                &history,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .and_then(|c| {
                c.query_row(
                    "SELECT COUNT(DISTINCT session_id) FROM messages_fts WHERE messages_fts MATCH ?1",
                    [TRANSCRIPT_MARK],
                    |r| r.get::<_, i64>(0),
                )
            })
            .is_ok_and(|n| n == 2)
    })
    .await;

    // ---- the browser: production key, pinned host ----
    let main = browser.run("return await pt.mainKey();").await;
    let reader_grants = [
        format!("history.read:{SESSION}"),
        format!("session.write:{SESSION}"),
    ];
    let main_kid = host.enroll("es256", main["public"].as_str().unwrap(), &reader_grants);
    assert_eq!(main_kid, main["kid"].as_str().unwrap());
    let other = browser.run("return await pt.extraKey('other');").await;
    let mut other_grants = reader_grants.to_vec();
    other_grants.push("host.settings_read".into());
    host.enroll("es256", other["public"].as_str().unwrap(), &other_grants);
    let barrier = browser.run("return await pt.extraKey('barrier');").await;
    let barrier_kid = host.enroll(
        "es256",
        barrier["public"].as_str().unwrap(),
        &[format!("history.read:{SESSION}")],
    );
    let writer = browser.run("return await pt.extraKey('writer');").await;
    host.enroll(
        "es256",
        writer["public"].as_str().unwrap(),
        &[format!("session.write:{SESSION}")],
    );
    browser.run("return await pt.extraKey('unknown');").await;

    let mut evidence = BTreeMap::new();

    // ---- direct channel: private history, search and input ----
    let opened = browser
        .run(
            "const c = await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.main, dlg: null });
             pt.channels.direct = c;
             return { peer: c.peer, pair: await c.selectedPair() };",
        )
        .await;
    assert!(opened["peer"].is_string(), "direct channel: {opened}");
    evidence.insert("direct_pair", opened["pair"].clone());

    let page = browser
        .run(&format!(
            "return await pt.channels.direct.historyGet({SESSION:?});"
        ))
        .await;
    assert_eq!(status(&page), "executed", "{page}");
    // Complete canonical messages, structure included, read through the actual
    // channel by the production reader.
    let messages = browser
        .run(&format!(
            "pt.known = await pt.channel.readHistory(pt.channels.direct, {SESSION:?}); return pt.known;"
        ))
        .await;
    let messages = messages.as_array().unwrap_or_else(|| panic!("{messages}"));
    let turn_uuid = format!("{SESSION}-a1");
    let turn = messages
        .iter()
        .find(|m| m["uuid"] == turn_uuid.as_str())
        .unwrap_or_else(|| panic!("no assistant turn: {messages:?}"));
    let tool = turn["message"]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .unwrap_or_else(|| panic!("no tool call survived: {turn}"));
    assert_eq!(tool["name"], "Bash", "{tool}");
    assert_eq!(tool["id"], "toolu_1", "{tool}");
    assert!(
        tool["input"]["command"]
            .as_str()
            .unwrap()
            .contains(TRANSCRIPT_MARK),
        "{tool}"
    );
    assert!(
        messages.iter().any(|m| m["message"]["role"] == "user"
            && m["message"]["text"]
                .as_str()
                .unwrap_or("")
                .contains(TRANSCRIPT_MARK)),
        "{messages:?}"
    );

    // The agent rewrites that SAME message (a streamed turn finishing): it keeps
    // its uuid and position, and the update-aware reader still finds it.
    std::fs::write(host.stubs.join(format!("mutate.{SESSION}")), b"").unwrap();
    wait("the rewrite to reach local history", || {
        rusqlite::Connection::open_with_flags(&history, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                    [MUTATION_MARK],
                    |r| r.get::<_, i64>(0),
                )
            })
            .is_ok_and(|n| n > 0)
    })
    .await;
    let refreshed = browser
        .run(&format!(
            "return await pt.channel.refreshHistory(pt.channels.direct, {SESSION:?}, pt.known);"
        ))
        .await;
    assert!(
        refreshed["changed"].as_u64().unwrap_or(0) >= 1,
        "{refreshed}"
    );
    let after = refreshed["messages"].as_array().unwrap();
    let order = |list: &[Value]| {
        list.iter()
            .map(|m| (m["seq"].clone(), m["uuid"].clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        order(after),
        order(messages),
        "the rewrite moved or duplicated a message"
    );
    let rewritten = after
        .iter()
        .find(|m| m["uuid"] == turn_uuid.as_str())
        .unwrap();
    assert_eq!(rewritten["seq"], turn["seq"]);
    assert_ne!(rewritten["sha256"], turn["sha256"]);
    let blocks = rewritten["message"]["blocks"].as_array().unwrap();
    assert!(
        blocks
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "Bash"),
        "{rewritten}"
    );
    assert!(
        blocks.iter().any(|b| b["text"]
            .as_str()
            .is_some_and(|t| t.contains(MUTATION_MARK))),
        "{rewritten}"
    );

    // The session list: only what this key may read, private fields on the channel only.
    let listed = browser
        .run("return await pt.channel.listSessions(pt.channels.direct);")
        .await;
    let listed = listed.as_array().unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(
        listed
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![SESSION],
        "{listed:?}"
    );
    assert_eq!(
        listed[0]["cwd"].as_str().unwrap(),
        host.work.display().to_string()
    );

    // Settings: a body, on the channel, only for a key that may read them.
    let no_settings = browser
        .run("return await pt.channels.direct.settingsGet();")
        .await;
    assert_eq!(status(&no_settings), "refused", "{no_settings}");
    let settings = browser
        .run(
            "const c = await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.other, dlg: null });
             try { return await pt.channel.readSettings(c); } finally { c.close(); }",
        )
        .await;
    assert!(settings["default_launcher"].is_string(), "{settings}");
    assert!(
        settings["claude_skip_permissions"].is_boolean(),
        "{settings}"
    );

    let found = browser
        .run(&format!(
            "return await pt.channels.direct.historySearch({SESSION:?}, {TRANSCRIPT_MARK:?});"
        ))
        .await;
    assert_eq!(status(&found), "executed", "{found}");
    assert!(
        !found["body"]["hits"].as_array().unwrap().is_empty(),
        "{found}"
    );
    let private_query = browser
        .run(&format!(
            "return await pt.channels.direct.historySearch({SESSION:?}, {QUERY_MARK:?});"
        ))
        .await;
    assert_eq!(status(&private_query), "executed", "{private_query}");
    assert!(private_query["body"]["hits"].as_array().unwrap().is_empty());

    // Input: delivered once, and an exact replay of the same signed frame is
    // recognised and not repeated.
    let input = browser
        .run(&format!(
            "const frame = await pt.auth.signPayload(pt.keys.main, pt.auth.operationPayload({{
               hostId: pt.host.id, key: pt.keys.main, dlg: null, op: 'session.reply',
               session: {SESSION:?}, args: {{ session: {SESSION:?}, text: {INPUT_MARK:?} }} }}));
             const first = await pt.channels.direct.sendSigned(frame);
             const again = await pt.channels.direct.sendSigned(frame);
             return {{ first, again }};"
        ))
        .await;
    assert_eq!(status(&input["first"]), "executed", "{input}");
    assert_eq!(status(&input["again"]), "replayed", "{input}");
    wait("the input to reach the agent", || {
        host.stdin_of(SESSION).contains(INPUT_MARK)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(host.stdin_of(SESSION).matches(INPUT_MARK).count(), 1);

    // ---- every message is authorized again by the host ----
    let altered = browser
        .run(&format!(
            "const frame = await pt.auth.signPayload(pt.keys.main, pt.auth.operationPayload({{
               hostId: pt.host.id, key: pt.keys.main, dlg: null, op: 'history.get',
               session: {SESSION:?}, args: {{ session: {SESSION:?}, after: 0, limit: 1 }} }}));
             const text = new TextDecoder().decode(pt.auth.fromBase64Url(frame.p)).replace('\"limit\":1', '\"limit\":2');
             const p = pt.auth.toBase64Url(new TextEncoder().encode(text));
             return await pt.channels.direct.sendSigned({{ ...frame, p }});"
        ))
        .await;
    assert_eq!(status(&altered), "refused", "{altered}");
    let expired = browser
        .run(&format!(
            "return await pt.channels.direct.operation('history.get', {SESSION:?}, {{ session: {SESSION:?}, after: 0, limit: 1 }},
               {{ now: Math.floor(Date.now() / 1000) - 600, lifetime: 60 }});"
        ))
        .await;
    assert_eq!(status(&expired), "refused", "{expired}");
    let wrong_peer = browser
        .run(&format!(
            "const frame = await pt.auth.signPayload(pt.keys.other, pt.auth.operationPayload({{
               hostId: pt.host.id, key: pt.keys.other, dlg: null, op: 'history.get',
               session: {SESSION:?}, args: {{ session: {SESSION:?}, after: 0, limit: 1 }} }}));
             return await pt.channels.direct.sendSigned(frame);"
        ))
        .await;
    assert_eq!(status(&wrong_peer), "refused", "{wrong_peer}");
    let wrong_session = browser
        .run(&format!(
            "return await pt.channels.direct.historyGet({SESSION_B:?});"
        ))
        .await;
    assert_eq!(status(&wrong_session), "refused", "{wrong_session}");
    assert!(wrong_session["body"].is_null(), "{wrong_session}");
    let replayed_read = browser
        .run(&format!(
            "const frame = await pt.auth.signPayload(pt.keys.main, pt.auth.operationPayload({{
               hostId: pt.host.id, key: pt.keys.main, dlg: null, op: 'history.search',
               session: {SESSION:?}, args: {{ session: {SESSION:?}, query: {TRANSCRIPT_MARK:?}, limit: 5 }} }}));
             const first = await pt.channels.direct.sendSigned(frame);
             const again = await pt.channels.direct.sendSigned(frame);
             return {{ first, again }};"
        ))
        .await;
    assert_eq!(status(&replayed_read["first"]), "executed");
    assert_eq!(status(&replayed_read["again"]), "replayed");
    assert!(replayed_read["again"]["body"].is_null(), "{replayed_read}");

    // The same read over the plaintext relay is refused before admission, even
    // though this key holds the grant: content is answered only on the channel.
    let over_relay = browser
        .run(&format!(
            "const frame = await pt.auth.signPayload(pt.keys.main, pt.auth.operationPayload({{
               hostId: pt.host.id, key: pt.keys.main, dlg: null, op: 'history.get',
               session: {SESSION:?}, args: {{ session: {SESSION:?}, after: 0, limit: 1 }} }}));
             const response = await fetch(`/api/host-relay/${{pt.host.id}}/request`, {{ method: 'POST',
               headers: {{ 'Content-Type': 'application/json' }}, body: JSON.stringify({{ type: 'signed_op', ...frame }}) }});
             const reply = await response.json();
             return JSON.parse(new TextDecoder().decode(pt.auth.fromBase64Url(reply.statement.p)));"
        ))
        .await;
    assert_eq!(status(&over_relay), "refused", "{over_relay}");
    assert!(
        over_relay["detail"]
            .as_str()
            .unwrap()
            .contains("private connection"),
        "{over_relay}"
    );
    // Nor does a prompt travel over it: refused before admission, never typed.
    let reply_over_relay = browser
        .run(&format!(
            "const frame = await pt.auth.signPayload(pt.keys.main, pt.auth.operationPayload({{
               hostId: pt.host.id, key: pt.keys.main, dlg: null, op: 'session.reply',
               session: {SESSION:?}, args: {{ session: {SESSION:?}, text: 'typed-over-the-relay' }} }}));
             const response = await fetch(`/api/host-relay/${{pt.host.id}}/request`, {{ method: 'POST',
               headers: {{ 'Content-Type': 'application/json' }}, body: JSON.stringify({{ type: 'signed_op', ...frame }}) }});
             const reply = await response.json();
             return JSON.parse(new TextDecoder().decode(pt.auth.fromBase64Url(reply.statement.p)));"
        ))
        .await;
    assert_eq!(status(&reply_over_relay), "refused", "{reply_over_relay}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!host.stdin_of(SESSION).contains("typed-over-the-relay"));

    // A key without the read grant gets a channel but no history.
    let writer_read = browser
        .run(&format!(
            "const c = await pt.channel.openPrivateChannel({{ host: pt.host, key: pt.keys.writer, dlg: null }});
             const r = await c.historyGet({SESSION:?}); c.close(); return r;"
        ))
        .await;
    assert_eq!(status(&writer_read), "refused", "{writer_read}");
    // An unknown key gets no channel at all.
    let unknown = browser
        .run("await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.unknown, dlg: null }); return { opened: true };")
        .await;
    assert_eq!(unknown["code"], "refused", "{unknown}");

    // ---- forced relay through a loopback TURN server ----
    let turn_port = free_udp_port();
    let _turn = Turn::start(&node, &modules, turn_port, TURN_USER, TURN_PASS);
    let relayed = browser
        .run(&format!(
            "const c = await pt.channel.openPrivateChannel({{ host: pt.host, key: pt.keys.main, dlg: null, relayOnly: true,
               iceServers: [{{ urls: 'turn:127.0.0.1:{turn_port}?transport=udp', username: {TURN_USER:?}, credential: {TURN_PASS:?} }}] }});
             const page = await c.historyGet({SESSION:?});
             const pair = await c.selectedPair(); c.close();
             return {{ page, pair }};"
        ))
        .await;
    assert_eq!(status(&relayed["page"]), "executed", "{relayed}");
    assert_eq!(relayed["pair"]["localType"], "relay", "{relayed}");
    evidence.insert("relay_pair", relayed["pair"].clone());

    // ---- the final delivery gate holds across processes ----
    delivery_barrier(&mut browser, &host, &barrier_kid, &mut evidence).await;

    // ---- challenges are single use and do not survive a restart ----
    let reuse = browser
        .run(
            "const ch = await pt.channel.requestPeerChallenge(pt.host);
             const first = await pt.channel.connectWithChallenge({ host: pt.host, key: pt.keys.main, dlg: null }, ch);
             first.close();
             try { await pt.channel.connectWithChallenge({ host: pt.host, key: pt.keys.main, dlg: null }, ch); return { reused: true }; }
             catch (e) { return { code: e.code }; }",
        )
        .await;
    assert_eq!(reuse["code"], "refused", "{reuse}");
    browser
        .run("pt.saved = await pt.channel.requestPeerChallenge(pt.host); return true;")
        .await;
    host.stop();
    host.start(&central).await;
    let after_restart = browser
        .run("await pt.channel.connectWithChallenge({ host: pt.host, key: pt.keys.main, dlg: null }, pt.saved); return { opened: true };")
        .await;
    assert_eq!(after_restart["code"], "refused", "{after_restart}");

    // ---- local history outlives the host process, structure and rewrite included ----
    let persisted = browser
        .run(&format!(
            "const c = await pt.channel.openPrivateChannel({{ host: pt.host, key: pt.keys.main, dlg: null }});
             try {{ return await pt.channel.readHistory(c, {SESSION:?}); }} finally {{ c.close(); }}"
        ))
        .await;
    let persisted = persisted
        .as_array()
        .unwrap_or_else(|| panic!("{persisted}"));
    assert_eq!(
        order(persisted),
        order(after),
        "history changed across a restart"
    );
    let kept = persisted
        .iter()
        .find(|m| m["uuid"] == turn_uuid.as_str())
        .unwrap();
    assert_eq!(kept["sha256"], rewritten["sha256"], "{kept}");

    // ---- host gone: unavailable, and nothing falls back to the relay ----
    let before = central.recorded.bodies("/request").len();
    let live = browser
        .run("const c = await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.main, dlg: null }); pt.channels.last = c; return true;")
        .await;
    assert_eq!(live, json!(true), "{live}");
    host.stop();
    let dropped = browser
        .run(&format!(
            "await new Promise(r => setTimeout(r, 1500));
             return await pt.channels.last.historyGet({SESSION:?});"
        ))
        .await;
    assert_eq!(dropped["code"], "unavailable", "{dropped}");
    let offline = browser
        .run("await pt.channel.openPrivateChannel({ host: pt.host, key: pt.keys.main, dlg: null }); return { opened: true };")
        .await;
    assert_eq!(offline["code"], "unavailable", "{offline}");
    let fallback: Vec<String> = central.recorded.bodies("/request")[before..]
        .iter()
        .filter(|body| !body.contains("\"peer_challenge\"") && !body.contains("\"peer_offer\""))
        .cloned()
        .collect();
    assert!(fallback.is_empty(), "plaintext fallback: {fallback:?}");

    // ---- privacy: what the central server could see ----
    let mut central_bytes = std::fs::read(&central.db).unwrap();
    central_bytes
        .extend(std::fs::read(central.db.with_extension("sqlite-wal")).unwrap_or_default());
    let signaling: Vec<String> = central
        .recorded
        .bodies("/api/host-relay/")
        .into_iter()
        .filter(|b| b.contains("peer_"))
        .collect();
    assert!(!signaling.is_empty());
    let cwd = host.work.display().to_string();
    let memory = central.memory(&[SESSION, SESSION_B]).await;
    for private in [INPUT_MARK, QUERY_MARK] {
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
    for secret in [
        TRANSCRIPT_MARK,
        INPUT_MARK,
        QUERY_MARK,
        SESSION,
        SESSION_B,
        &cwd,
    ] {
        for body in &signaling {
            assert!(
                !exposes(&exposed("", body.as_bytes()), secret),
                "{secret} in signaling: {body}"
            );
        }
    }
    // Nothing private reached central by ANY path: not the transcript (with its
    // structured turn and the later rewrite), not the folder. The managed host
    // runs no legacy live producer (registration, PTY mirror, heartbeat,
    // transcript upload).
    for private in [TRANSCRIPT_MARK, MUTATION_MARK, &cwd] {
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
            "{private} in central's live memory"
        );
    }
    // Session ids are accepted central metadata (ID only) where a signer named
    // them in a signed frame's envelope on the relay: the owner's starts, and
    // this test's own probes of reads and prompts the host refused there.
    for session in [SESSION, SESSION_B] {
        for (line, body, seen) in central.recorded.0.lock().unwrap().iter() {
            if !exposes(seen, session) {
                continue;
            }
            let frame: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let op: Value = frame["p"]
                .as_str()
                .and_then(|p| URL_SAFE_NO_PAD.decode(p).ok())
                .and_then(|p| serde_json::from_slice(&p).ok())
                .unwrap_or(Value::Null);
            assert!(
                line.ends_with("/request") && frame["type"] == "signed_op" && op["sess"] == session,
                "{session} reached central outside a signed envelope: {line}"
            );
        }
        assert!(
            !exposes(&central_bytes, session),
            "{session} in the central database"
        );
    }
    let live_paths: Vec<String> = central
        .recorded
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|(line, _, _)| line.clone())
        .filter(|line| line.contains("/api/live") || line.contains("/api/sessions/"))
        .collect();
    assert!(
        live_paths.is_empty(),
        "legacy producers ran: {live_paths:?}"
    );
    evidence.insert(
        "central_requests",
        json!(central.recorded.0.lock().unwrap().len()),
    );
    evidence.insert("signaling_messages", json!(signaling.len()));
    // Central's own search, asked last (the probe itself is a recorded request
    // that names the marker, so it comes after the request scan).
    for private in [INPUT_MARK, QUERY_MARK] {
        let searched: Value = reqwest::get(format!("{}/api/sessions?q={private}", central.url))
            .await
            .unwrap()
            .json()
            .await
            .unwrap_or(Value::Null);
        assert!(
            !exposes(searched.to_string().as_bytes(), private),
            "{searched}"
        );
    }
    println!("EVIDENCE {}", serde_json::to_string(&evidence).unwrap());
    browser
        .run("pt.channels.direct.close(); return true;")
        .await;
}

/// A read reply that was admitted, read and signed is handed to the channel
/// only inside one immediate `authz.db` transaction that re-checks the exact
/// authority it was admitted under. A grant change committed by ANOTHER
/// process is either seen there and the reply withheld, or waits until the
/// hand-off is done. The final cutover uses the actual local revoke command.
async fn delivery_barrier(
    browser: &mut Browser,
    host: &Host,
    kid: &str,
    evidence: &mut BTreeMap<&'static str, Value>,
) {
    let barrier = &host.barrier;
    let revoke = format!(
        "UPDATE grants SET revoked_at = strftime('%s','now') WHERE cred_id = '{kid}' AND session_id = '{SESSION}' AND cap = 'history.read';"
    );
    let regrant = format!(
        "UPDATE grants SET revoked_at = NULL WHERE cred_id = '{kid}' AND session_id = '{SESSION}' AND cap = 'history.read';"
    );
    let open = |name: &str| {
        format!(
            "const c = await pt.channel.openPrivateChannel({{ host: pt.host, key: pt.keys.barrier, dlg: null }});
             pt.channels.{name} = c;
             pt.pending = c.historyGet({SESSION:?}).then(r => ({{ status: r.status, items: r.body && r.body.items.length }}), e => ({{ code: e.code }}));
             return true;"
        )
    };
    let finished = |child: &mut Child| {
        let status = child.wait().unwrap();
        let mut err = String::new();
        std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).unwrap();
        assert!(status.success(), "authority command: {err}");
    };

    // Revoked while the reply is held: withheld, and the channel closes.
    std::fs::write(barrier.join("hold"), b"").unwrap();
    assert_eq!(browser.run(&open("held")).await, json!(true));
    wait("the reply to be held", || barrier.join("held").exists()).await;
    finished(&mut host.authz_sql(&revoke));
    std::fs::remove_file(barrier.join("hold")).unwrap();
    std::fs::write(barrier.join("release"), b"").unwrap();
    let withheld = browser.run("return await pt.pending;").await;
    assert_eq!(
        withheld["code"], "unavailable",
        "a reply read before the revoke was released: {withheld}"
    );

    // Removed and granted again (a NEW epoch, with unchanged timestamp) while
    // held: still the old admission, still withheld. A new signed read works.
    finished(&mut host.authz_sql(&regrant));
    std::fs::write(barrier.join("hold"), b"").unwrap();
    assert_eq!(browser.run(&open("regrant")).await, json!(true));
    wait("the reply to be held", || barrier.join("held").exists()).await;
    finished(&mut host.authz_sql(&revoke));
    finished(&mut host.authz_sql(&regrant));
    std::fs::remove_file(barrier.join("hold")).unwrap();
    std::fs::write(barrier.join("release"), b"").unwrap();
    let old_epoch = browser.run("return await pt.pending;").await;
    assert_eq!(
        old_epoch["code"], "unavailable",
        "remove and regrant released an old reply: {old_epoch}"
    );
    let fresh = browser
        .run(&format!(
            "const c = await pt.channel.openPrivateChannel({{ host: pt.host, key: pt.keys.barrier, dlg: null }});
             try {{ const r = await c.historyGet({SESSION:?}); return {{ status: r.status }}; }} finally {{ c.close(); }}"
        ))
        .await;
    assert_eq!(status(&fresh), "executed", "{fresh}");

    // Revoked DURING the hand-off: the other process waits for the host's
    // transaction, so the reply already handed off is the last one.
    std::fs::write(barrier.join("hold-inside"), b"").unwrap();
    assert_eq!(browser.run(&open("inside")).await, json!(true));
    wait("the hand-off to be held", || {
        barrier.join("inside").exists()
    })
    .await;
    let mut revoker = host.revoke_credential(kid);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let waited = revoker.try_wait().unwrap().is_none();
    assert!(
        waited,
        "another process changed authority inside the delivery transaction"
    );
    std::fs::remove_file(barrier.join("hold-inside")).unwrap();
    std::fs::write(barrier.join("release-inside"), b"").unwrap();
    finished(&mut revoker);
    let delivered = browser.run("return await pt.pending;").await;
    assert_eq!(delivered["status"], "executed", "{delivered}");
    let next = browser
        .run(&format!(
            "return await pt.channels.inside.historyGet({SESSION:?}).then(r => ({{ status: r.status }}), e => ({{ code: e.code }}));"
        ))
        .await;
    assert_eq!(next["status"], "refused", "{next}");
    browser
        .run("pt.channels.inside.close(); return true;")
        .await;
    evidence.insert(
        "delivery_barrier",
        json!({"withheld": withheld, "old_epoch": old_epoch, "revoker_waited": waited, "after_revoke": next}),
    );
}

/// The scanner itself, mutation-checked: a marker routed through the legacy
/// plaintext paths in ENCODED form (a base64url `signed_op` payload, a
/// percent-encoded search URL, a nested base64 chunk) is still found.
#[test]
fn the_privacy_scanner_sees_through_encodings() {
    let recorded = Recorded::default();
    let marker = "scannermarkjuniper";
    let payload = serde_json::to_vec(
        &json!({"op": "session.reply", "args": {"text": format!("say {marker} now")}}),
    )
    .unwrap();
    recorded.push(
        "POST /api/host-relay/h/request".into(),
        json!({"type": "signed_op", "p": URL_SAFE_NO_PAD.encode(&payload), "s": "x", "k": "y"})
            .to_string()
            .into_bytes(),
    );
    recorded.push(
        "GET /api/sessions?q=scanner%6Darkjuniper".into(),
        Vec::new(),
    );
    let chunk =
        STANDARD.encode(json!({"data": STANDARD.encode(format!("..{marker}.."))}).to_string());
    recorded.push(
        "POST /api/live/x/pty".into(),
        json!({"chunk": chunk}).to_string().into_bytes(),
    );
    recorded.push(
        "POST /api/live/x/events".into(),
        b"{\"text\":\"nothing\"}".to_vec(),
    );
    assert_eq!(
        recorded.containing(marker),
        vec![
            "POST /api/host-relay/h/request".to_owned(),
            "GET /api/sessions?q=scanner%6Darkjuniper".to_owned(),
            "POST /api/live/x/pty".to_owned(),
        ]
    );
    // A raw scan of the request bodies alone would have missed all three.
    for (_, body, _) in recorded.0.lock().unwrap().iter() {
        assert!(!contains(body, marker.as_bytes()));
    }
    // Opaque stored bytes: a base64 copy at any alignment is found.
    for offset in 0..3 {
        let stored = STANDARD.encode(format!("{}{marker} tail", "z".repeat(offset)));
        assert!(exposes(stored.as_bytes(), marker), "offset {offset}");
    }
    assert!(!exposes(b"an unrelated database page", marker));
}
