//! Real-binary fixture for agent-to-agent messaging between two hosts: a REAL
//! central server in process (with a request/response recorder and optional
//! hostile rewrites of relay traffic), REAL `arrakis-conversations host run`
//! processes with isolated HOME/XDG/runtime directories, REAL `mcp serve`
//! processes speaking JSON-RPC on stdio, and a recording stub `claude`.
//!
//! Every child is owned (killed and reaped on drop); stub agents also stop on a
//! fixture-owned stop file. No PID is ever read back and signalled.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::rtc;

pub const BIN: &str = env!("CARGO_BIN_EXE_arrakis-conversations");

// ---------------------------------------------------------------- central --

/// `(method and URI, everything exposed by the request, everything exposed by the response)`.
type Exchange = (String, Vec<u8>, Vec<u8>);

/// Every request that reached central and every answer central returned,
/// decoded the ways any reader could (see `rtc::exposed`).
#[derive(Clone, Default)]
pub struct Recorded(Arc<Mutex<Vec<Exchange>>>);

impl Recorded {
    /// Exchanges whose request or response exposed `needle`.
    pub fn containing(&self, needle: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, request, response)| {
                rtc::exposes(request, needle) || rtc::exposes(response, needle)
            })
            .map(|(line, _, _)| line.clone())
            .collect()
    }

    pub fn lines(&self, part: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, _, _)| line.contains(part))
            .map(|(line, _, _)| line.clone())
            .collect()
    }

    /// Relay exchanges with their (truncated) answers, for failure messages.
    pub fn relay_log(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, _, _)| line.contains("/api/host-relay/") && line.ends_with("/request"))
            .map(|(line, _, answer)| {
                let answer = String::from_utf8_lossy(answer);
                format!("{line} -> {}", answer.chars().take(300).collect::<String>())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Relay requests of one `type` sent for one host id.
    pub fn relay_requests(&self, host: &str, kind: &str) -> usize {
        let path = format!("/api/host-relay/{host}/request");
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, request, _)| {
                line.contains(&path)
                    && rtc::contains(request, format!("\"type\":\"{kind}\"").as_bytes())
            })
            .count()
    }
}

type Rewrite = Box<dyn Fn(&mut Value) + Send + Sync>;

/// What a hostile central does to relay traffic. Off by default.
#[derive(Default)]
pub struct Hostile {
    /// Send peer-channel signaling meant for host `.0` to host `.1` instead.
    pub reroute_peer: Mutex<Option<(String, String)>>,
    /// Rewrite every relay ANSWER body before it reaches the caller.
    pub answer: Mutex<Option<Rewrite>>,
}

pub struct Central {
    pub url: String,
    pub db: PathBuf,
    pub recorded: Recorded,
    pub hostile: Arc<Hostile>,
    /// The in-process server ends with the fixture, on success or unwind.
    server: tokio::task::AbortHandle,
}

impl Drop for Central {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Central {
    /// Every byte central persisted (SQLite main file plus WAL), decoded.
    pub fn stored(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for suffix in ["", "-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{suffix}", self.db.display()));
            if let Ok(bytes) = std::fs::read(&path) {
                out.extend(rtc::exposed("", &bytes));
            }
        }
        out
    }
}

pub async fn start_central(root: &Path) -> Central {
    let db = root.join("central.sqlite");
    let static_dir = root.join("static");
    std::fs::create_dir_all(&static_dir).unwrap();
    std::fs::write(static_dir.join("index.html"), b"<!doctype html>").unwrap();
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
    let hostile = Arc::new(Hostile::default());
    let (log, evil) = (recorded.clone(), hostile.clone());
    // Outside the server's own router, so a rewritten path is routed afresh
    // (a `Router::layer` runs after routing).
    let server = conversations_server::build_router(state);
    let app = axum::Router::new()
        .fallback_service(server)
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let (log, evil) = (log.clone(), evil.clone());
                async move {
                    let (mut parts, body) = request.into_parts();
                    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
                        .await
                        .unwrap_or_default();
                    let line = format!("{} {}", parts.method, parts.uri);
                    let mut seen = rtc::exposed(&line, &bytes);
                    let relay = parts.uri.path().starts_with("/api/host-relay/")
                        && parts.uri.path().ends_with("/request");
                    if relay {
                        let peer = rtc::contains(&bytes, b"\"type\":\"peer_");
                        if let Some((from, to)) = evil.reroute_peer.lock().unwrap().clone() {
                            if peer && parts.uri.path().contains(&from) {
                                let rerouted = parts.uri.to_string().replace(&from, &to);
                                parts.uri = rerouted.parse().unwrap();
                            }
                        }
                    }
                    let response = next
                        .run(axum::extract::Request::from_parts(
                            parts,
                            axum::body::Body::from(bytes),
                        ))
                        .await;
                    if !relay {
                        // Streams (the host's relay feed, live sockets) never end;
                        // only relayed answers are buffered and recorded.
                        log.0.lock().unwrap().push((line, seen, Vec::new()));
                        return response;
                    }
                    let (parts, body) = response.into_parts();
                    let mut bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
                        .await
                        .unwrap_or_default()
                        .to_vec();
                    {
                        if let Some(rewrite) = evil.answer.lock().unwrap().as_ref() {
                            if let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) {
                                rewrite(&mut value);
                                bytes = serde_json::to_vec(&value).unwrap();
                            }
                        }
                    }
                    seen.push(b'\n');
                    let answered = rtc::exposed("", &bytes);
                    log.0.lock().unwrap().push((line, seen, answered));
                    let mut parts = parts;
                    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Central {
        url,
        db,
        recorded,
        hostile,
        server: server.abort_handle(),
    }
}

// ------------------------------------------------------------------ hosts --

/// Own subprocesses from spawn onward, including failed startup paths.
pub struct OwnedChild(pub Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The stub agent: records how it was run, keeps a transcript in Claude's
/// on-disk format that ends with completed-turn markers (so guarded delivery
/// may type), turns echo off, and records everything typed into it. Each
/// submitted line (`\r`) becomes a new completed turn, as a real agent's would.
const STUB: &str = r#"#!/usr/bin/python3
import json, os, select, subprocess, sys, time
from pathlib import Path
stubs = Path(__STUBS__)
args = sys.argv[1:]
session = next((args[i + 1] for i, arg in enumerate(args[:-1]) if arg in ('--session-id', '--resume')), '')
with (stubs / 'claude.runs').open('a') as runs:
    runs.write(' '.join(args) + '\n')
project = os.getcwd().replace('.', '-').replace('/', '-')
directory = Path(os.environ['HOME']) / '.claude' / 'projects' / project
directory.mkdir(parents=True, exist_ok=True)
transcript_path = directory / (session + '.jsonl')
turns = 0
def turn(text):
    global turns
    turns += 1
    with transcript_path.open('a') as transcript:
        for record in (
            {'type': 'user', 'uuid': f'{session}-u{turns}', 'sessionId': session,
             'timestamp': '2026-11-03T10:00:00.000Z', 'message': {'role': 'user', 'content': text}},
            {'type': 'assistant', 'uuid': f'{session}-a{turns}', 'sessionId': session,
             'timestamp': '2026-11-03T10:00:01.000Z', 'message': {'id': f'msg_{turns}', 'role': 'assistant',
             'model': 'stub', 'stop_reason': 'end_turn', 'content': [{'type': 'text', 'text': 'ok'}]}},
            {'type': 'system', 'subtype': 'turn_duration', 'uuid': f'{session}-s{turns}',
             'sessionId': session, 'timestamp': '2026-11-03T10:00:02.000Z'},
        ):
            transcript.write(json.dumps(record) + '\n')
turn('stub started')
subprocess.run(['/bin/stty', '-echo'], stderr=subprocess.DEVNULL)
deadline = time.monotonic() + 600
line = b''
try:
    with (stubs / ('claude.stdin.' + session)).open('ab', buffering=0) as received:
        while stubs.exists() and not (stubs / 'stop').exists() and time.monotonic() < deadline:
            ready, _, _ = select.select([0], [], [], 0.1)
            if ready:
                data = os.read(0, 4096)
                if not data:
                    break
                received.write(data)
                line += data
                while b'\r' in line or b'\n' in line:
                    cut = min(i for i in (line.find(b'\r'), line.find(b'\n')) if i >= 0)
                    submitted, line = line[:cut], line[cut + 1:]
                    if submitted.strip():
                        time.sleep(0.2)
                        turn('typed input')
finally:
    if stubs.exists():
        (stubs / ('claude.done.' + session)).write_text('exited')
"#;

pub struct NativeHost {
    pub root: PathBuf,
    pub runtime: PathBuf,
    pub work: PathBuf,
    pub stubs: PathBuf,
    pub settings: PathBuf,
    pub central: String,
    child: Option<OwnedChild>,
    sessions: Vec<String>,
}

impl NativeHost {
    /// `transport` is this host's local transport settings block.
    pub fn new(root: &Path, central: &str, transport: Value) -> Self {
        let (runtime, work, stubs) = (root.join("rt"), root.join("work"), root.join("stubs"));
        for dir in [&work, &stubs] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let settings = root.join("local-host.json");
        std::fs::write(&settings, json!({ "transport": transport }).to_string()).unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
        let stub = STUB.replace("__STUBS__", &serde_json::to_string(&stubs).unwrap());
        std::fs::write(stubs.join("claude"), stub).unwrap();
        std::fs::set_permissions(stubs.join("claude"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        Self {
            root: root.to_owned(),
            runtime,
            work,
            stubs,
            settings,
            central: central.to_owned(),
            child: None,
            sessions: Vec::new(),
        }
    }

    /// The isolated environment every process of this host runs in.
    pub fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        // Nothing inherited: no provider, cloud, database or harness settings.
        let tmp = self.root.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        command
            .env_clear()
            .env("HOME", &self.root)
            .env("TMPDIR", &tmp)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_RUNTIME_DIR", self.root.join("xdg-runtime"))
            .env("CONVERSATIONS_HOST_SETTINGS_FILE", &self.settings)
            .env("PATH", format!("{}:/usr/bin:/bin", self.stubs.display()))
            .current_dir(&self.work);
        command
    }

    pub async fn start(&mut self) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("host.log"))
            .unwrap();
        let child = self
            .command()
            .args(["--server", &self.central, "host", "--runtime-dir"])
            .arg(&self.runtime)
            .args(["run", "--backend", "normal"])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.child = Some(OwnedChild(child));
        let status = format!("{}/api/host-relay/{}/status", self.central, self.id());
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
                "the host did not attach to the relay: {}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Stop the host process (its wrappers and agents end with it).
    pub async fn stop(&mut self) {
        self.child = None;
        let deadline = Instant::now() + Duration::from_secs(20);
        while self
            .sessions
            .iter()
            .any(|s| !self.stubs.join(format!("claude.done.{s}")).exists())
            && Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("host.log")).unwrap_or_default()
    }

    fn cli(&self, args: &[&str]) -> String {
        let output = self
            .command()
            .args(["host", "--runtime-dir"])
            .arg(&self.runtime)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    pub fn id(&self) -> String {
        self.cli(&["credential", "identity"])
            .split_whitespace()
            .last()
            .unwrap()
            .to_owned()
    }

    pub fn descriptor(&self) -> String {
        self.cli(&["credential", "descriptor"]).trim().to_owned()
    }

    /// `(standard-base64 public key, key id)` of this host's agent key.
    pub fn agent_key(&self) -> (String, String) {
        let out = self.cli(&["credential", "agent-key"]);
        let field = |prefix: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(prefix))
                .unwrap()
                .trim()
                .to_owned()
        };
        (
            field("Agent public key (standard base64):"),
            field("Key id:"),
        )
    }

    /// The local user approves another host's agent key; returns the credential id.
    pub fn approve_agent(&self, public_key: &str, sessions: &[&str]) -> String {
        let mut args = vec![
            "credential".to_owned(),
            "add".into(),
            "--kind".into(),
            "agent".into(),
            "--alg".into(),
            "ed25519".into(),
            "--public-key".into(),
            public_key.into(),
            "--label".into(),
            "peer agent".into(),
        ];
        for session in sessions {
            args.extend(["--grant".into(), format!("session.write:{session}")]);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.cli(&args)
            .split_whitespace()
            .nth(2)
            .unwrap()
            .to_owned()
    }

    /// The local user pins a recipient session from that host's own descriptor.
    pub fn pair_peer(&self, descriptor: &str, session: &str) {
        self.cli(&[
            "credential",
            "pair-peer",
            "--host",
            descriptor,
            "--session",
            session,
            "--label",
            "peer",
        ]);
    }

    pub fn revoke(&self, credential: &str) {
        self.cli(&["credential", "revoke", credential, "--json"]);
    }

    /// Fixture-only change to the local authority database (grant removal).
    pub fn authz_sql(&self, sql: &str) {
        let conn = rusqlite::Connection::open(self.runtime.join("authz.db")).unwrap();
        conn.busy_timeout(Duration::from_secs(20)).unwrap();
        conn.execute_batch(sql).unwrap();
    }

    pub fn authz_count(&self, sql: &str) -> i64 {
        rusqlite::Connection::open_with_flags(
            self.runtime.join("authz.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
    }

    /// Start a session on the owner-only local socket and wait for its agent.
    pub async fn start_session(&mut self, session: &str) {
        let started = ask_host(
            &self.runtime.join("host.sock"),
            json!({"type": "start", "launch": {"id": session, "agent": "claude", "cwd": self.work, "command": ["claude"]}}),
        );
        assert_eq!(started["type"], "started", "{started}");
        let transcript = self.stubs.join(format!("claude.stdin.{session}"));
        let deadline = Instant::now() + Duration::from_secs(40);
        while !transcript.exists() {
            assert!(Instant::now() < deadline, "agent {session} never ran");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.sessions.push(session.to_owned());
    }

    /// After a host restart, make sure `session` has a running terminal: the
    /// host either starts it now or reports the one it already runs (resumed,
    /// or an agent child that outlived the old host). Which of these happened
    /// is timing-dependent; callers prove the effect by what the agent receives.
    pub async fn restart_session(&mut self, session: &str) {
        let started = ask_host(
            &self.runtime.join("host.sock"),
            json!({"type": "start", "launch": {"id": session, "agent": "claude", "cwd": self.work, "command": ["claude"]}}),
        );
        assert!(
            started["type"] == "started" || started["message"] == "terminal is already running",
            "{started}"
        );
        if !self.sessions.iter().any(|s| s == session) {
            self.sessions.push(session.to_owned());
        }
    }

    pub fn stdin_of(&self, session: &str) -> String {
        std::fs::read_to_string(self.stubs.join(format!("claude.stdin.{session}")))
            .unwrap_or_default()
    }

    /// A real `mcp serve` for `session`, as a wrapped agent on this host runs it.
    pub async fn mcp(&self, session: &str, delivery: bool, extra_env: &[(&str, &Path)]) -> Mcp {
        let mut command = self.command();
        command
            .args([
                "--server",
                &self.central,
                "mcp",
                "serve",
                "--session",
                session,
            ])
            .env("CONVERSATIONS_HOST_DIR", &self.runtime);
        if delivery {
            command.args(["--delivery", "conversations"]);
        }
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join(format!("mcp.{session}.log")))
            .unwrap();
        Mcp::start(command, log).await
    }
}

impl Drop for NativeHost {
    fn drop(&mut self) {
        let _ = std::fs::write(self.stubs.join("stop"), b"stop");
        self.child = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while self
            .sessions
            .iter()
            .any(|s| !self.stubs.join(format!("claude.done.{s}")).exists())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

pub fn ask_host(socket: &Path, request: Value) -> Value {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(40)))
        .unwrap();
    let bytes = serde_json::to_vec(&request).unwrap();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&bytes).unwrap();
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).unwrap();
    let mut body = vec![0u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

// -------------------------------------------------------------------- mcp --

/// A JSON-RPC client for one `mcp serve` process on stdio.
pub struct Mcp {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::sync::mpsc::UnboundedReceiver<Value>,
    next: u64,
}

impl Mcp {
    async fn start(command: Command, log: std::fs::File) -> Self {
        let mut command = tokio::process::Command::from(command);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if tx.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        let mut mcp = Self {
            child,
            stdin,
            lines,
            next: 0,
        };
        mcp.rpc(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "native-agent-it", "version": "0"}}),
        )
        .await
        .unwrap();
        mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        mcp
    }

    async fn send(&mut self, value: Value) {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let value = tokio::time::timeout_at(deadline, self.lines.recv())
                .await
                .map_err(|_| format!("{method}: no answer in time"))?
                .ok_or_else(|| format!("{method}: the MCP server exited"))?;
            if value["id"] == id {
                return match value.get("error") {
                    Some(error) => Err(error.to_string()),
                    None => Ok(value["result"].clone()),
                };
            }
        }
    }

    /// Call one tool: `Ok(structured result)` or `Err(tool error text)`.
    pub async fn tool(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let result = self
            .rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        if result["isError"] == true {
            return Err(result["content"][0]["text"]
                .as_str()
                .unwrap_or("tool error")
                .to_owned());
        }
        Ok(result["structuredContent"].clone())
    }

    pub async fn send_message(
        &mut self,
        target: &str,
        text: &str,
        idem: &str,
        reply_to: Option<&str>,
    ) -> Result<Value, String> {
        self.tool(
            "messages_send",
            json!({"recipient_session_id": target, "text": text, "idempotency_key": idem, "reply_to": reply_to}),
        )
        .await
    }

    pub async fn status(&mut self, id: &str) -> Result<Value, String> {
        self.tool("messages_status", json!({"message_id": id}))
            .await
    }

    pub async fn receive(&mut self, acknowledge: &[&str]) -> Result<Value, String> {
        self.tool(
            "messages_receive",
            json!({"acknowledge": acknowledge, "wait_seconds": 0}),
        )
        .await
    }

    pub async fn close(mut self) {
        let _ = self.child.kill().await;
    }
}

pub async fn wait_until(what: &str, seconds: u64, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Lines the native client wrote to its debug evidence file.
pub fn evidence(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}
