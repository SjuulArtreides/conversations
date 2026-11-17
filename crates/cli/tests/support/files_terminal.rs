// Shared fixture for the file/terminal acceptance journey and raw-wire baseline.
#[path = "private_rtc.rs"]
#[allow(dead_code)]
mod rtc;
use rtc::{required_env, start_central, wait, Browser, Central};

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};

const SESSION: &str = "5b0c7a52-93d4-4c61-8f0e-2a6c1d7e4f10";
const SESSION_B: &str = "c4e2a9d1-0b7f-4e38-a5d6-91f3b2c8e702";
const BIN: &str = env!("CARGO_BIN_EXE_conversations");
/// Printed by the agent when it starts: only in the terminal's snapshot.
const SCREEN_MARK: &str = "screenmarkhackberry";
/// Printed later: a terminal update.
const UPDATE_MARK: &str = "updatemarkspicebush";
/// Printed after one follower's read grant was removed.
const LATE_MARK: &str = "latemarkwitchhazel";
/// Printed after that grant was given again.
const REGRANT_MARK: &str = "regrantmarkbayberry";
/// Printed while a forced-TURN follower watches.
const TURN_MARK: &str = "turnmarkserviceberry";
/// Printed while a short renewed lease is followed.
const LEASE_MARK: &str = "leasemarkredbud";
/// Printed after a lease lapsed unrenewed.
const LAPSED_MARK: &str = "lapsedmarksassafras";
const FILE_NAME: &str = "filenamemarkalderwood.txt";
const FILE_MARK: &str = "filebytesmarkchokecherry";
const SECOND_NAME: &str = "secondnamemarkelderberry.md";
const IMAGE_MARK: &str = "imagebytesmarkbuckthorn";
const TAMPER_NAME: &str = "tampernamemarkhazelnut.txt";
const REVOKED_NAME: &str = "revokednamemarkpawpaw.txt";
const TURN_NAME: &str = "turnnamemarkpersimmon.txt";
/// A transfer whose authority is revoked while the terminal holds it.
const BETWEEN_NAME: &str = "betweennamemarkhackmatack.txt";
const BETWEEN_MARK: &str = "betweenbytesmarkbearberry";
/// A transfer whose terminal goes away before it answers.
const LOST_NAME: &str = "lostnamemarkcrowberry.txt";
const LOST_MARK: &str = "lostbytesmarkpartridgeberry";
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
        for dir in [&work, &stubs, &barrier, &root.join("tmp")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // Loopback only: the host offers exactly this candidate address.
        let settings = root.join("local-host.json");
        std::fs::write(
            &settings,
            json!({"transport": {"candidate_ips": ["127.0.0.1"]}}).to_string(),
        )
        .unwrap();
        // The stub agent records how it was run, turns off terminal echo (so a
        // paste is never mirrored back as output) and line buffering (so a
        // paste without Enter is still read), prints a screen mark, then
        // prints each `say.<session>.<n>` file once and records everything it
        // is sent. Machine-clipboard tools record any use: there must be none.
        // A unique stop file controls this fixture's lifetime. Never signal
        // wrapper/agent PIDs read from an old log: those PIDs can be reused.
        let stub = r#"#!/usr/bin/python3
import os, select, subprocess, sys, time
from pathlib import Path
stubs = Path(__STUBS__)
args = sys.argv[1:]
session = next((args[i + 1] for i, arg in enumerate(args[:-1]) if arg in ('--session-id', '--resume')), '')
with (stubs / 'claude.runs').open('a') as runs:
    runs.write(f'{os.getpid()} {os.getppid()} ' + ' '.join(args) + '\n')
subprocess.run(['/bin/stty', '-echo', '-icanon', 'min', '1', 'time', '0'], stderr=subprocess.DEVNULL)
os.write(1, ('__SCREEN__ ' + session + '\r\n').encode())
said = set()
deadline = time.monotonic() + 300
try:
    with (stubs / ('claude.stdin.' + session)).open('ab', buffering=0) as received:
        while stubs.exists() and not (stubs / 'stop').exists() and not (stubs / ('exit.' + session)).exists() and time.monotonic() < deadline:
            for line in sorted(stubs.glob('say.' + session + '.*')):
                if line.name not in said:
                    said.add(line.name)
                    os.write(1, line.read_bytes() + b'\r\n')
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
        .replace("__SCREEN__", SCREEN_MARK);
        std::fs::write(stubs.join("claude"), stub).unwrap();
        std::fs::set_permissions(stubs.join("claude"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        for tool in ["pbcopy", "pbpaste", "osascript", "xclip", "xsel", "wl-copy"] {
            let path = stubs.join(tool);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\necho {tool} >> {:?}\n",
                    stubs.join("clipboard.used")
                ),
            )
            .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
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
            // File drops are staged under the temp dir: this fixture's own.
            .env("TMPDIR", self.root.join("tmp"))
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
// Test-only view of the wire: every signed op the page sent (to replay one),
// and chunks that can be held back, flipped or duplicated in flight.
const realSend = RTCDataChannel.prototype.send;
const wire = { hold: false, held: [], mode: null, ops: [] };
RTCDataChannel.prototype.send = function (data) {
  if (typeof data === "string" && data.startsWith("{")) {
    let m = null;
    try { m = JSON.parse(data); } catch (e) { m = null; }
    if (m && m.t === "op") wire.ops.push({ p: m.p, s: m.s, k: m.k });
    if (m && m.t === "chunk") {
      if (wire.mode === "flip") {
        const bytes = auth.fromBase64Url(m.data);
        bytes[0] ^= 1;
        m.data = auth.toBase64Url(bytes);
        data = JSON.stringify(m);
        wire.mode = null;
      } else if (wire.mode === "duplicate") {
        wire.mode = null;
        realSend.call(this, data);
      }
      if (wire.hold) { wire.held.push([this, data]); return; }
    }
  }
  return realSend.call(this, data);
};
wire.release = () => {
  wire.hold = false;
  for (const [dc, d] of wire.held.splice(0)) realSend.call(dc, d);
  return true;
};
wire.lastOp = (name) => wire.ops.filter((o) =>
  JSON.parse(new TextDecoder().decode(auth.fromBase64Url(o.p))).op === name).pop();
// Terminal followers, rendered as decoded text per frame.
const follows = {};
async function follow(c, name, session, lease) {
  const f = { frames: [], ended: null, handle: null };
  follows[name] = f;
  f.handle = await c.subscribeTerminal(session, {
    onFrame(frame) {
      f.frames.push({ kind: frame.kind, cols: frame.cols, rows: frame.rows,
        text: new TextDecoder().decode(frame.data) });
    },
    onEnd(end) { f.ended = end; },
  }, lease);
  return f.handle.sub;
}
function seen(name) {
  const f = follows[name];
  return { kinds: f.frames.map((x) => x.kind), sizes: f.frames.map((x) => [x.cols, x.rows]),
    text: f.frames.map((x) => x.text).join(""), ended: f.ended, count: f.frames.length };
}
const bytes = (text) => new TextEncoder().encode(text);
window.pt = { channel, auth, keys, channels, extraKey, mainKey, host: null, wire, follow, follows, seen, bytes };
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

/// `body` with each `%NAME%` replaced by its value as a JS string literal.
fn js(body: &str, vars: &[(&str, &str)]) -> String {
    vars.iter().fold(body.to_owned(), |out, (name, value)| {
        out.replace(&format!("%{name}%"), &serde_json::to_string(value).unwrap())
    })
}

/// Poll a page expression until it is `true`.
async fn until(browser: &mut Browser, expression: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while browser.eval(expression).await != json!(true) {
        if Instant::now() >= deadline {
            let followers = browser
                .eval("JSON.stringify(Object.fromEntries(Object.keys(pt.follows).map((k) => [k, pt.seen(k)])))")
                .await;
            panic!("timed out waiting for {what}; followers: {followers}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Make the agent of `session` print `text` on its terminal.
fn say(host: &Host, session: &str, n: u32, text: &str) {
    std::fs::write(host.stubs.join(format!("say.{session}.{n:03}")), text).unwrap();
}

/// The paths pasted into a session's agent, in order, and whatever else it
/// was sent besides those bracketed pastes.
fn pastes(host: &Host, session: &str) -> (Vec<PathBuf>, String) {
    let stdin = host.stdin_of(session);
    let mut paths = Vec::new();
    let mut rest = String::new();
    let mut tail = stdin.as_str();
    while let Some(start) = tail.find("\x1b[200~") {
        rest.push_str(&tail[..start]);
        let after = &tail[start + 6..];
        let end = after.find("\x1b[201~").expect("an unterminated paste");
        paths.push(PathBuf::from(&after[..end]));
        tail = &after[end + 6..];
    }
    rest.push_str(tail);
    (paths, rest)
}

/// Every regular file under `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                todo.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// A pasted path: in this session's private stage under the host's temp dir,
/// owner-only, holding exactly `bytes`.
fn assert_staged(host: &Host, path: &Path, name_suffix: &str, bytes: &[u8]) {
    let tmp = host.root.join("tmp");
    assert!(
        path.starts_with(&tmp),
        "{path:?} is not in the host's temp dir"
    );
    let stage = path
        .strip_prefix(&tmp)
        .unwrap()
        .components()
        .next()
        .unwrap()
        .as_os_str()
        .to_string_lossy()
        .into_owned();
    assert!(stage.starts_with("conv-drop-"), "{path:?}");
    assert!(
        path.to_string_lossy().ends_with(name_suffix),
        "{path:?} does not end with {name_suffix}"
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes, "{path:?}");
    assert_eq!(mode(path), 0o600, "{path:?}");
    assert_eq!(mode(&tmp.join(&stage)), 0o700, "{stage}");
}
