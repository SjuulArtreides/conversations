//! Shared real central server, privacy scanner and Chromium fixture for the
//! private-channel integration tests. Each journey keeps its own host, TURN
//! configuration and browser page so its wire assertions stay explicit.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};

pub fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is not set; this test never falls back to an installed browser or node")
    })
}

// ---------------------------------------------------------------- central --

/// Every request that reached the real central server: its method and URI, and
/// everything it exposed (see [`exposed`]).
#[derive(Clone, Default)]
pub struct Recorded(pub(super) Arc<Mutex<Vec<Request>>>);

/// `(method and URI, raw body, everything exposed)`.
type Request = (String, Vec<u8>, Vec<u8>);

impl Recorded {
    pub(super) fn push(&self, line: String, body: Vec<u8>) {
        let uri = line.split_once(' ').map_or(line.as_str(), |(_, uri)| uri);
        let seen = exposed(uri, &body);
        self.0.lock().unwrap().push((line, body, seen));
    }

    /// Requests that exposed `needle` in their URI, body, or any base64 payload
    /// inside the body.
    pub fn containing(&self, needle: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, seen)| exposes(seen, needle))
            .map(|(line, _, _)| line.clone())
            .collect()
    }

    pub fn bodies(&self, path_part: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, _, _)| line.contains(path_part))
            .map(|(_, body, _)| String::from_utf8_lossy(body).into_owned())
            .collect()
    }
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Encoding is not encryption: what one request showed central, decoded the
/// ways any reader could. The percent-decoded URI (path and query), the raw
/// body, and every JSON string in the body that decodes as base64 or base64url,
/// recursively into decoded JSON (bounded depth and size).
pub fn exposed(uri: &str, body: &[u8]) -> Vec<u8> {
    let mut out = percent_decode(uri);
    out.push(b'\n');
    out.extend_from_slice(body);
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        decode_strings(&value, 0, &mut out);
    }
    out
}

pub fn decode_strings(value: &Value, depth: usize, out: &mut Vec<u8>) {
    const MAX_DEPTH: usize = 4;
    const MAX_EXPOSED: usize = 64 * 1024 * 1024;
    if depth > MAX_DEPTH || out.len() > MAX_EXPOSED {
        return;
    }
    match value {
        Value::String(text) => {
            let trimmed = text.trim_end_matches('=');
            for engine in [
                &URL_SAFE_NO_PAD,
                &base64::engine::general_purpose::STANDARD_NO_PAD,
            ] {
                if let Ok(bytes) = engine.decode(trimmed) {
                    out.push(b'\n');
                    out.extend_from_slice(&bytes);
                    if let Ok(inner) = serde_json::from_slice::<Value>(&bytes) {
                        decode_strings(&inner, depth + 1, out);
                    }
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| decode_strings(v, depth, out)),
        Value::Object(map) => map.values().for_each(|v| decode_strings(v, depth, out)),
        _ => {}
    }
}

pub fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            byte => out.push(byte),
        }
        i += 1;
    }
    out
}

/// `needle` itself and its base64/base64url forms at every alignment, so an
/// encoded copy stored as opaque bytes is found too.
pub fn needles(needle: &str) -> Vec<Vec<u8>> {
    let mut out = vec![needle.as_bytes().to_vec()];
    for engine in [&URL_SAFE_NO_PAD, &STANDARD] {
        for offset in 0..3 {
            let mut padded = vec![0u8; offset];
            padded.extend_from_slice(needle.as_bytes());
            let encoded = engine.encode(&padded);
            let encoded = encoded.trim_end_matches('=');
            // Drop the characters the prefix or the unknown suffix can change.
            let core = &encoded[if offset == 0 { 0 } else { 4 }..encoded.len() - 4];
            out.push(core.as_bytes().to_vec());
        }
    }
    out
}

pub fn exposes(haystack: &[u8], needle: &str) -> bool {
    needles(needle).iter().any(|n| contains(haystack, n))
}

pub struct Central {
    pub url: String,
    pub db: PathBuf,
    pub recorded: Recorded,
    /// The server's own state, to read what it holds only in memory (the PTY
    /// mirror travels over a WebSocket the request recorder does not see).
    pub state: conversations_server::AppState,
}

impl Central {
    /// Everything the central server received for any session, encoded or not.
    /// The central server no longer retains any per-session terminal or message
    /// state of its own — the PTY replay hubs, text snapshots and live message
    /// feeds were removed — so this is exactly the recorded request log
    /// (headers + bodies); the caller adds the database bytes.
    pub async fn memory(&self, _known_sessions: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for (line, _, body) in self.recorded.0.lock().unwrap().iter() {
            out.extend(exposed(line, body));
        }
        out
    }
}

pub async fn start_central(root: &Path, static_dir: &Path) -> Central {
    let db = root.join("central.sqlite");
    let repo =
        conversations_server::SqliteRepo::connect(&format!("sqlite://{}?mode=rwc", db.display()))
            .await
            .unwrap();
    let config = conversations_server::Config {
        auth_enabled: false,
        static_dir: Some(static_dir.display().to_string()),
        ..conversations_server::Config::default()
    };
    let state = conversations_server::AppState::new(Arc::new(repo), Arc::new(config));
    let recorded = Recorded::default();
    let log = recorded.clone();
    let app = conversations_server::build_router(state.clone()).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let log = log.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
                    .await
                    .unwrap_or_default();
                log.push(format!("{} {}", parts.method, parts.uri), bytes.to_vec());
                next.run(axum::extract::Request::from_parts(
                    parts,
                    axum::body::Body::from(bytes),
                ))
                .await
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Central {
        url,
        db,
        recorded,
        state,
    }
}

/// Own subprocesses from spawn onward, including failed startup paths.
pub struct OwnedChild(pub(super) Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub struct Browser {
    _child: OwnedChild,
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next: u64,
}

impl Browser {
    pub async fn launch(chrome: &str, profile: &Path, url: &str) -> Self {
        let mut browser = Self::start(chrome, profile, url).await;
        let deadline = Instant::now() + Duration::from_secs(30);
        while browser.eval("typeof window.pt === 'object'").await != json!(true) {
            assert!(Instant::now() < deadline, "the harness page did not load");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        browser
    }

    async fn start(chrome: &str, profile: &Path, url: &str) -> Self {
        let child = OwnedChild(
            Command::new(chrome)
                .args([
                    "--headless=new",
                    "--remote-debugging-port=0",
                    "--remote-debugging-address=127.0.0.1",
                    "--no-first-run",
                    "--no-default-browser-check",
                    "--disable-background-networking",
                    "--disable-component-update",
                    "--disable-sync",
                    "--disable-extensions",
                    "--password-store=basic",
                    "--use-mock-keychain",
                    "--allow-loopback-in-peer-connection",
                    "--disable-features=WebRtcHideLocalIpsWithMdns",
                ])
                .arg(format!("--user-data-dir={}", profile.display()))
                .arg("about:blank")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let port_file = profile.join("DevToolsActivePort");
        wait("Chromium's DevTools port", || port_file.exists()).await;
        let port = std::fs::read_to_string(&port_file)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned();
        let target: Value = reqwest::Client::new()
            .put(format!("http://127.0.0.1:{port}/json/new?{url}"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let (socket, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(target["webSocketDebuggerUrl"].as_str().unwrap()),
        )
        .await
        .expect("CDP connection timed out")
        .unwrap();
        Self {
            _child: child,
            socket,
            next: 1,
        }
    }

    /// Evaluate an async expression in the page and return its JSON value.
    pub async fn eval(&mut self, expression: &str) -> Value {
        let reply = self
            .cdp(
                "Runtime.evaluate",
                json!({"expression": expression, "awaitPromise": true, "returnByValue": true}),
            )
            .await;
        if let Some(exception) = reply.get("exceptionDetails") {
            return json!({"exception": exception});
        }
        reply["result"]["value"].clone()
    }

    /// Send one DevTools command to the page and return its `result`.
    pub async fn cdp(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let message = json!({"id": id, "method": method, "params": params});
        self.socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                message.to_string(),
            ))
            .await
            .unwrap();
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(90), self.socket.next())
                .await
                .expect("the browser did not answer")
                .unwrap()
                .unwrap();
            let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
                continue;
            };
            let reply: Value = serde_json::from_str(&text).unwrap();
            if reply["id"] != id {
                continue;
            }
            assert!(reply.get("error").is_none(), "{method}: {reply}");
            return reply["result"].clone();
        }
    }

    /// Run `body` (an async function body using `pt`) and return its result, or
    /// `{error, code}` if it threw.
    pub async fn run(&mut self, body: &str) -> Value {
        self.eval(&format!(
            "(async () => {{ try {{ {body} }} catch (e) {{ return {{error: String(e && e.message || e), code: e && e.code || null}}; }} }})()"
        ))
        .await
    }
}

/// Serve the production helper to the browser exactly as written: Node only
/// erases the type annotations (no bundling, no rewriting of logic).
pub fn build_static(node: &str, root: &Path) -> PathBuf {
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

pub async fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn status(value: &Value) -> &str {
    value["status"]
        .as_str()
        .unwrap_or_else(|| panic!("no status in {value}"))
}

// ------------------------------------------------- not in private_transport --

/// Pin the host in the page as pairing records it: the id the browser dials and
/// the identity key it verifies.
pub async fn pin_host(browser: &mut Browser, id: &str, key: &str) {
    let pinned = browser
        .run(&format!(
            "pt.host = {{ id: {id:?}, key: {key:?} }}; return true;"
        ))
        .await;
    assert_eq!(pinned, json!(true), "{pinned}");
}

/// Open a private channel for the browser key `key` and keep it as `name`.
pub async fn open_channel(browser: &mut Browser, name: &str, key: &str) -> Value {
    browser
        .run(&format!(
            "const c = await pt.channel.openPrivateChannel({{ host: pt.host, key: pt.keys[{key:?}], dlg: null }});
             pt.channels[{name:?}] = c;
             return {{ peer: c.peer }};"
        ))
        .await
}

/// One signed operation on the open channel `name`: `{status, detail}`, or
/// `{error, code}` if the channel failed.
pub async fn operate(
    browser: &mut Browser,
    name: &str,
    op: &str,
    session: &str,
    args: &Value,
) -> Value {
    browser
        .run(&format!(
            "const r = await pt.channels[{name:?}].operation({op:?}, {session:?}, {args});
             return {{ status: r.status, detail: r.detail }};"
        ))
        .await
}

pub async fn close_channel(browser: &mut Browser, name: &str) {
    browser
        .run(&format!("pt.channels[{name:?}].close(); return true;"))
        .await;
}

/// One signed operation on a fresh channel for `key`, closed afterwards.
pub async fn private_op(
    browser: &mut Browser,
    key: &str,
    op: &str,
    session: &str,
    args: &Value,
) -> Value {
    browser
        .run(&format!(
            "const c = await pt.channel.openPrivateChannel({{ host: pt.host, key: pt.keys[{key:?}], dlg: null }});
             try {{ const r = await c.operation({op:?}, {session:?}, {args}); return {{ status: r.status, detail: r.detail }}; }}
             finally {{ c.close(); }}"
        ))
        .await
}

pub struct Turn {
    _child: OwnedChild,
}

impl Turn {
    pub fn start(node: &str, modules: &str, port: u16, user: &str, password: &str) -> Self {
        let script = format!(
            r#"const Turn = require("node-turn");
const server = new Turn({{ authMech: "long-term", credentials: {{ "{user}": "{password}" }},
  listeningIps: ["127.0.0.1"], relayIps: ["127.0.0.1"], listeningPort: {port}, debugLevel: "ERROR" }});
server.start();
console.log("ready");
setInterval(() => {{}}, 1 << 30);"#
        );
        let mut child = OwnedChild(
            Command::new(node)
                .env("NODE_PATH", modules)
                .args(["-e", &script])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.0.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        assert_eq!(line.trim(), "ready", "node-turn did not start");
        Self { _child: child }
    }
}

pub fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
