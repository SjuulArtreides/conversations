#![cfg(unix)]
//! The real `arrakis-conversations` binary, end to end on loopback: a host with
//! the normal backend, the real headless wrapper it spawns, and a stub `claude`
//! that writes a transcript in Claude's own format. Nothing seeds the host's
//! databases: the session record, the native binding and the history all come
//! from production start, registration and ingestion.
//!
//! Prompts, keystrokes and starts are private: the host refuses them on the
//! central relay and executes them only on the authenticated, encrypted
//! browser channel. So each journey runs twice:
//!
//! - always, against a stand-in central that relays signed operations, which
//!   proves the relay is refused and what stays local (local-socket start,
//!   canonical record, history) works without it;
//! - opt in, against the REAL central server in process and a REAL headless
//!   Chromium running the production `privateChannel.ts` with a software
//!   P-256 key, which proves the remote journey itself. These are ignored by
//!   default and never fall back when the browser or node is missing:
//!
//! ```sh
//! PRIVATE_TRANSPORT_CHROME=/path/to/chrome PRIVATE_TRANSPORT_NODE=$(command -v node) \
//! cargo test -p conversations-cli --test host_lifecycle -- --ignored --test-threads=1
//! ```

#[path = "support/private_rtc.rs"]
#[allow(dead_code)]
mod rtc;

#[path = "support/owned_process.rs"]
mod owned_process;

use std::collections::HashMap;
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Value};

const SESSION: &str = "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11";
const BIN: &str = env!("CARGO_BIN_EXE_arrakis-conversations");

#[derive(Default)]
struct CentralState {
    queue: Vec<String>,
    replies: HashMap<String, Value>,
    attached: usize,
}

impl CentralState {
    /// Match the real relay: only the newest host stream owns queued operations.
    fn take_pending(&mut self, generation: usize) -> Option<Vec<String>> {
        (self.attached == generation).then(|| std::mem::take(&mut self.queue))
    }
}

#[test]
fn a_superseded_test_relay_stream_cannot_steal_the_next_hosts_operation() {
    let mut state = CentralState {
        attached: 2,
        queue: vec!["reopen on the new host".into()],
        ..Default::default()
    };
    assert!(state.take_pending(1).is_none());
    assert_eq!(
        state.take_pending(2),
        Some(vec!["reopen on the new host".into()])
    );
    assert_eq!(state.take_pending(2), Some(vec![]));
}

struct Central {
    url: String,
    state: Arc<Mutex<CentralState>>,
}

impl Central {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(CentralState::default()));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || serve(stream, state));
            }
        });
        Self { url, state }
    }

    /// Send one signed operation to the host through the relay stream and wait
    /// for its signed answer; returns the status the host reports.
    fn relay(
        &self,
        signer: &Signer,
        op: &str,
        session: &str,
        args: Value,
        host_id: &str,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let (p, s, k) = signer.sign(op, session, args, host_id);
        let event = json!({"id": id, "request": {"type": "signed_op", "p": p, "s": s, "k": k}});
        self.state.lock().unwrap().queue.push(event.to_string());
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            if let Some(reply) = self.state.lock().unwrap().replies.remove(&id) {
                let statement = &reply["statement"];
                let payload = URL_SAFE_NO_PAD
                    .decode(statement["p"].as_str().unwrap_or_else(|| panic!("{reply}")))
                    .unwrap();
                let payload: Value = serde_json::from_slice(&payload).unwrap();
                return payload["status"].as_str().unwrap().to_owned();
            }
            assert!(
                Instant::now() < deadline,
                "no answer from the host for {op}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn serve(mut stream: TcpStream, state: Arc<Mutex<CentralState>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    let json_response = |stream: &mut TcpStream, text: &str| {
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            )
            .as_bytes(),
        );
    };
    if request_line.contains("/challenge ") {
        json_response(&mut stream, r#"{"nonce":"integration"}"#);
    } else if request_line.contains("/host-relay/") && request_line.contains("/stream") {
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.write_all(b"data: {\"type\":\"attached\",\"reply_token\":\"t\"}\n\n");
        let generation = {
            let mut current = state.lock().unwrap();
            current.attached += 1;
            current.attached
        };
        loop {
            let Some(pending) = state.lock().unwrap().take_pending(generation) else {
                return;
            };
            for event in pending {
                if stream
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .is_err()
                {
                    return;
                }
            }
            if stream.write_all(b":\n\n").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    } else if request_line.contains("/reply ") {
        if let Ok(answer) = serde_json::from_slice::<Value>(&body) {
            if let Some(id) = answer["id"].as_str() {
                state
                    .lock()
                    .unwrap()
                    .replies
                    .insert(id.to_owned(), answer["reply"].clone());
            }
        }
        let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
    } else if request_line.starts_with("GET /api/live ") {
        json_response(&mut stream, r#"{"live_sessions":[]}"#);
    } else {
        let _ = stream.write_all(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    }
}

struct Signer {
    pair: Ed25519KeyPair,
    kid: String,
}

impl Signer {
    fn new() -> Self {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Self {
            pair: Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap(),
            kid: String::new(),
        }
    }

    fn public(&self) -> String {
        STANDARD.encode(self.pair.public_key().as_ref())
    }

    fn sign(
        &self,
        op: &str,
        session: &str,
        args: Value,
        host_id: &str,
    ) -> (String, String, String) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let payload = serde_json::to_vec(&json!({
            "v": 1, "typ": "op", "aud": host_id, "kid": self.kid, "dlg": null, "op": op,
            "sess": session, "jti": uuid::Uuid::new_v4().simple().to_string(),
            "iat": now, "exp": now + 60, "args": args,
        }))
        .unwrap();
        let mut signed = b"conversations-op/v1\n".to_vec();
        signed.extend_from_slice(&payload);
        let sig = self.pair.sign(&signed);
        (
            URL_SAFE_NO_PAD.encode(&payload),
            URL_SAFE_NO_PAD.encode(sig.as_ref()),
            self.kid.clone(),
        )
    }
}

struct Env {
    home: tempfile::TempDir,
    runtime: PathBuf,
    work: PathBuf,
    stubs: PathBuf,
    central: Central,
    /// The central the host attaches to: the stand-in above, or the real one.
    server: String,
    /// Host transport settings (loopback candidates) for the real channel.
    settings: Option<PathBuf>,
    /// The loopback port this host's settings file assigns to its web UI.
    web_port: u16,
    host: Option<Child>,
}

/// A loopback port nothing holds at call time, for the settings file the
/// host reads when it starts.
fn free_loopback_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Env {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().canonicalize().unwrap();
        let (runtime, work, stubs) = (root.join("rt"), root.join("work"), root.join("stubs"));
        for dir in [&work, &stubs] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(stubs.join("recorder.pl"), RECORDER).unwrap();
        write_executable(&stubs.join("browser"), OPEN_STUB);
        // The stub agent records how it was run, then writes a transcript in
        // Claude's own on-disk format for the session it was told it is, then
        // keeps reading its terminal.
        write_executable(
            &stubs.join("claude"),
            &format!(
                r#"#!/bin/sh
echo "$$ $PPID $*" >> "{stubs}/claude.runs"
id=""; prev=""
for a in "$@"; do
  case "$prev" in --session-id|--resume) id="$a";; esac
  prev="$a"
done
dir="$HOME/.claude/projects/$(pwd -P | /usr/bin/tr './' '--')"
/bin/mkdir -p "$dir"
printf '%s\n' '{{"type":"user","uuid":"'"$id"'-u1","sessionId":"'"$id"'","timestamp":"2026-11-02T10:00:00.000Z","message":{{"role":"user","content":"integration transcript marker xylophone"}}}}' >> "$dir/$id.jsonl"
exec /usr/bin/perl "{stubs}/recorder.pl" "{stubs}"
"#,
                stubs = stubs.display()
            ),
        );
        let web_port = free_loopback_port();
        let settings = home.path().join("local-host.json");
        std::fs::write(&settings, json!({"web": {"port": web_port}}).to_string()).unwrap();
        let central = Central::start();
        Self {
            home,
            runtime,
            work,
            stubs,
            server: central.url.clone(),
            central,
            settings: Some(settings),
            web_port,
            host: None,
        }
    }

    /// The same host, attached to the real central at `server`, offering only
    /// a loopback candidate on its private channel.
    fn private(server: &str) -> Self {
        let mut env = Self::new();
        std::fs::write(
            env.settings.as_ref().unwrap(),
            json!({"web": {"port": env.web_port}, "transport": {"candidate_ips": ["127.0.0.1"]}})
                .to_string(),
        )
        .unwrap();
        env.server = server.to_owned();
        env
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        // cmux exports agent overrides (`CMUX_CUSTOM_CLAUDE_PATH`) that the host
        // honors before PATH: inherited, they would launch the real agent.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("CMUX_") {
                command.env_remove(key);
            }
        }
        command
            .env_remove("CONVERSATIONS_WRAPPED")
            .env_remove("CONVERSATIONS_WRAPPER_PID")
            .env_remove("CONVERSATIONS_HOST_CONTROL")
            .env_remove("CONVERSATIONS_HOST_DIR")
            .env_remove("CMUX_WORKSPACE_ID")
            .env_remove("CMUX_SURFACE_ID")
            .env_remove("CLAUDE_PROJECTS_DIR")
            .env("HOME", self.home.path())
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.stubs.display()))
            .env("CONVERSATIONS_BROWSER", self.stubs.join("browser"))
            .current_dir(&self.work);
        if let Some(settings) = &self.settings {
            command.env("CONVERSATIONS_HOST_SETTINGS_FILE", settings);
        }
        command
    }

    fn spawn_host(&mut self) {
        let child = self
            .command()
            .args(["--server", &self.server, "host", "--runtime-dir"])
            .arg(&self.runtime)
            .args(["--backend", "normal"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.host = Some(child);
    }

    /// The same host, keeping its stderr for assertions on what it reports.
    fn spawn_host_logged(&mut self, log: &Path) {
        let file = std::fs::File::create(log).unwrap();
        let child = self
            .command()
            .args(["--server", &self.server, "host", "--runtime-dir"])
            .arg(&self.runtime)
            .args(["--backend", "normal"])
            .stdout(Stdio::null())
            .stderr(Stdio::from(file))
            .spawn()
            .unwrap();
        self.host = Some(child);
    }

    /// `arrakis-conversations <group> --runtime-dir <runtime> <args>` as the
    /// owner would run it, bounded: a command that starts serving instead of
    /// returning fails the test, and its process group ends with it.
    fn management(&self, group: &str, args: &[&str]) -> std::process::Output {
        let mut command = self.command();
        command
            .args(["--server", &self.server, group, "--runtime-dir"])
            .arg(&self.runtime)
            .args(args);
        owned_process::run(&mut command, b"", Duration::from_secs(20))
            .unwrap_or_else(|error| panic!("{group} {args:?}: {error}"))
    }

    /// `arrakis-conversations open` as the owner would run it.
    fn open(&self, extra: &[&str]) -> std::process::Output {
        self.management("open", extra)
    }

    /// A successful `open`, ending its process group only once the stub
    /// opener has recorded the link: `open` does not wait for the launcher it
    /// spawns, and ending the group as soon as `open` exits would kill it.
    fn open_in_browser(&self, extra: &[&str]) -> std::process::Output {
        use std::io::Seek;
        let (mut stdout, mut stderr) =
            (tempfile::tempfile().unwrap(), tempfile::tempfile().unwrap());
        let open = owned_process::Owned::spawn(
            self.command()
                .args(["--server", &self.server, "open", "--runtime-dir"])
                .arg(&self.runtime)
                .args(extra)
                .stdin(Stdio::null())
                .stdout(stdout.try_clone().unwrap())
                .stderr(stderr.try_clone().unwrap()),
        );
        wait("the opener to record the owner link", || {
            open.exited() && self.home.path().join("open.args").exists()
        });
        let status = open.finish(Duration::from_secs(1)).unwrap();
        let read = |file: &mut std::fs::File| {
            let mut bytes = Vec::new();
            file.rewind().unwrap();
            file.read_to_end(&mut bytes).unwrap();
            bytes
        };
        std::process::Output {
            status,
            stdout: read(&mut stdout),
            stderr: read(&mut stderr),
        }
    }

    fn start_host(&mut self) {
        let before = self.central.state.lock().unwrap().attached;
        self.spawn_host();
        wait("the host to attach to the relay", || {
            self.central.state.lock().unwrap().attached > before
                && self.runtime.join("host.sock").exists()
        });
    }

    /// Start the host against the real central and wait until it is online.
    async fn start_private_host(&mut self) {
        let socket = self.runtime.join("host.sock");
        let _ = std::fs::remove_file(&socket);
        self.spawn_host();
        rtc::wait("the host socket", || socket.exists()).await;
        let status = format!("{}/api/host-relay/{}/status", self.server, self.host_id());
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
                "the host did not attach to the real central"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Approve a key locally with the real CLI; returns its stdout.
    fn add_credential(&self, alg: Option<&str>, public_key: &str, grants: &[String]) -> String {
        let mut args = vec!["add", "--kind", "owner"];
        if let Some(alg) = alg {
            args.extend(["--alg", alg]);
        }
        args.extend(["--public-key", public_key, "--label", "it"]);
        for grant in grants {
            args.extend(["--grant", grant]);
        }
        let output = self.management("credential", &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn enroll(&self, signer: &mut Signer, grants: &[String]) -> String {
        let text = self.add_credential(None, &signer.public(), grants);
        signer.kid = text.split_whitespace().nth(2).unwrap().to_owned();
        text.lines()
            .find_map(|l| l.strip_prefix("Host audience (aud): "))
            .unwrap()
            .trim()
            .to_owned()
    }

    /// Approve a browser key (`{kid, public}` from the page); returns its kid.
    fn enroll_browser(&self, key: &Value, grants: &[String]) -> String {
        let text = self.add_credential(Some("es256"), key["public"].as_str().unwrap(), grants);
        let kid = text.split_whitespace().nth(2).unwrap().to_owned();
        assert_eq!(kid, key["kid"].as_str().unwrap(), "{text}");
        kid
    }

    fn host_id(&self) -> String {
        let output = self.management("credential", &["identity"]);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .last()
            .unwrap()
            .to_owned()
    }

    /// The host's identity public key, as the user pins it when pairing.
    fn host_public_key(&self) -> String {
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

    fn sessions(&self) -> i64 {
        if !self.runtime.join("authz.db").exists() {
            return 0;
        }
        self.authz()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap()
    }

    fn runs(&self) -> Vec<String> {
        std::fs::read_to_string(self.stubs.join("claude.runs"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn pids(&self) -> Vec<i32> {
        self.runs()
            .iter()
            .flat_map(|line| {
                line.split(' ')
                    .take(2)
                    .filter_map(|p| p.parse().ok())
                    .collect::<Vec<i32>>()
            })
            .collect()
    }

    /// Ends only what this test owns, by ownership, never by a recorded PID:
    /// the host through its `Child` handle; every agent stub its wrappers ran
    /// through the `stop` control file, which each stub answers in
    /// `claude.stopped` before exiting (its wrapper then ends on its own).
    /// Recorded PIDs are only observed, with bounded waits.
    fn stop_everything(&mut self) {
        if let Some(mut host) = self.host.take() {
            let _ = host.kill();
            let _ = host.wait();
        }
        let runs = self.runs().len();
        if runs == 0 {
            return;
        }
        let stop = self.stubs.join("stop");
        std::fs::write(&stop, b"").unwrap();
        let stopped = || {
            std::fs::read_to_string(self.stubs.join("claude.stopped"))
                .unwrap_or_default()
                .lines()
                .count()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while stopped() < runs && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: signal 0 only probes for existence (observation).
        let running = |pid: &i32| unsafe { libc::kill(*pid, 0) == 0 };
        while self.pids().iter().any(running) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if stopped() < runs {
            eprintln!(
                "{} of {runs} agent stubs did not acknowledge stop",
                runs - stopped()
            );
        }
        let _ = std::fs::remove_file(stop);
    }

    fn authz(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.runtime.join("authz.db")).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop_everything();
    }
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The asset table this package's build script generated: the web-lifecycle
/// proof asserts against the real embedded SPA only when this binary has one.
mod embedded {
    include!(concat!(env!("OUT_DIR"), "/frontend_assets.rs"));
}

fn has_shared_frontend() -> bool {
    !embedded::SPA_ASSETS.is_empty()
}

struct WebResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

/// One raw HTTP/1.1 exchange on loopback with `Connection: close`, so reads
/// end at the response end without content-length parsing.
fn web_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Option<WebResponse> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        request.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (key, value) in headers {
        request.push_str(&format!("{key}: {value}\r\n"));
    }
    if let Some(body) = body {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    request.push_str("\r\n");
    if let Some(body) = body {
        request.push_str(body);
    }
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n")?;
    let mut lines = head.lines();
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let headers = lines
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    Some(WebResponse {
        status,
        headers,
        body: body.to_owned(),
    })
}

fn web_get(port: u16, path: &str) -> Option<WebResponse> {
    web_request(port, "GET", path, &[], None)
}

/// The bootstrap URL the stub browser-opener recorded, and its token.
fn recorded_owner_link(home: &Path, port: u16) -> (String, String) {
    let args = std::fs::read_to_string(home.join("open.args")).unwrap();
    let link = args
        .lines()
        .find(|line| {
            line.starts_with(&format!("http://localhost:{port}/#token="))
                && !line[format!("http://localhost:{port}/#token=").len()..].is_empty()
        })
        .unwrap_or_else(|| panic!("no owner link recorded"));
    let token = link.rsplit("#token=").next().unwrap().to_owned();
    (link.to_owned(), token)
}

/// The stub opener: records every URL it was given, opens nothing.
const OPEN_STUB: &str = "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$HOME/open.args\"\n";

#[test]
fn the_loopback_web_ui_starts_with_the_host_keeps_owner_auth_and_returns_after_a_restart() {
    if !has_shared_frontend() {
        eprintln!(
            "skipped: this binary embeds no shared frontend (npm run build in frontend/, then rebuild)"
        );
        return;
    }
    let mut env = Env::new();
    env.start_host();
    let port = env.web_port;
    // The UI answers on loopback as soon as host startup completes.
    wait("the local web UI", || {
        web_get(port, "/api/local").is_some_and(|r| {
            r.status == 200
                && r.body.contains("\"local\":true")
                && r.body.contains("\"authenticated\":false")
        })
    });
    // Owner authentication still gates every other endpoint.
    assert_eq!(web_get(port, "/api/status").unwrap().status, 401);
    // Loopback only: a foreign Host is refused, and a mutation without the
    // page's own Origin never reaches a handler.
    let foreign_host = format!("10.9.9.9:{port}");
    assert_eq!(
        web_request(port, "GET", "/api/local", &[("Host", &foreign_host)], None)
            .unwrap()
            .status,
        403
    );
    assert_eq!(
        web_request(port, "POST", "/api/unlock", &[], Some("{}"))
            .unwrap()
            .status,
        403
    );
    // Starting the host never launches a browser, even on a fresh
    // installation; only an explicit `open` does.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !env.home.path().join("open.args").exists(),
        "host startup opened a browser"
    );
    let output = env.open_in_browser(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("#token="),
        "the owner link reached stdout"
    );
    let (_link, token) = recorded_owner_link(env.home.path(), port);
    // The one-time token unlocks this browser exactly once.
    let origin = format!("http://127.0.0.1:{port}");
    let unlock_body = json!({"token": token}).to_string();
    let unlock = web_request(
        port,
        "POST",
        "/api/unlock",
        &[("Origin", &origin)],
        Some(&unlock_body),
    )
    .unwrap();
    assert_eq!(unlock.status, 200);
    let cookie = unlock
        .headers
        .iter()
        .find(|(key, _)| key == "set-cookie")
        .map(|(_, value)| value.clone())
        .unwrap();
    assert!(
        cookie.contains("ac_local_owner=")
            && cookie.contains("HttpOnly")
            && cookie.contains("SameSite=Strict")
            && cookie.contains("Max-Age=31536000")
    );
    let cookie = cookie.split(';').next().unwrap().to_owned();
    assert_eq!(
        web_request(
            port,
            "POST",
            "/api/unlock",
            &[("Origin", &origin)],
            Some(&unlock_body),
        )
        .unwrap()
        .status,
        401,
        "the spent token unlocked a second browser"
    );
    let status = web_request(port, "GET", "/api/status", &[("Cookie", &cookie)], None).unwrap();
    assert_eq!(status.status, 200);
    assert!(status.body.contains("chd1."), "{}", status.body);

    // Restart keeps browser access and opens no browser either.
    std::fs::remove_file(env.home.path().join("open.args")).unwrap();
    env.stop_everything();
    wait("the web port to be released", || {
        web_get(port, "/api/local").is_none()
    });
    env.start_host();
    wait("the local web UI after the restart", || {
        web_get(port, "/api/local")
            .is_some_and(|r| r.status == 200 && r.body.contains("\"local\":true"))
    });
    assert_eq!(
        web_request(port, "GET", "/api/status", &[("Cookie", &cookie)], None)
            .unwrap()
            .status,
        200,
        "the remembered browser lost access after restart"
    );
    assert!(
        !env.home.path().join("open.args").exists(),
        "host restart opened a browser"
    );
    let output = env.open_in_browser(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(said.contains(&format!("Local host management: http://localhost:{port}/")));
    assert!(!said.contains("#token="), "the owner link reached stdout");
    let (_link, token) = recorded_owner_link(env.home.path(), port);
    let unlock_body = json!({"token": token}).to_string();
    let unlock = web_request(
        port,
        "POST",
        "/api/unlock",
        &[("Origin", &origin)],
        Some(&unlock_body),
    )
    .unwrap();
    assert_eq!(unlock.status, 200, "no fresh owner link after the restart");
    std::fs::remove_file(env.home.path().join("open.args")).unwrap();
    let reset = env.open_in_browser(&["--reset-browser-access"]);
    assert!(reset.status.success());
    assert_eq!(
        web_request(port, "GET", "/api/status", &[("Cookie", &cookie)], None)
            .unwrap()
            .status,
        401
    );
    let (_, replacement) = recorded_owner_link(env.home.path(), port);
    assert_eq!(
        web_request(
            port,
            "POST",
            "/api/unlock",
            &[("Origin", &origin)],
            Some(&json!({"token": replacement}).to_string())
        )
        .unwrap()
        .status,
        200
    );
}

#[test]
fn a_missing_browser_does_not_prevent_host_or_web_startup() {
    if !has_shared_frontend() {
        return;
    }
    let mut env = Env::new();
    std::fs::remove_file(env.stubs.join("browser")).unwrap();
    let log = env.home.path().join("host.log");
    env.spawn_host_logged(&log);
    wait("the web UI without a browser launcher", || {
        web_get(env.web_port, "/api/local").is_some_and(|r| r.status == 200)
    });
    assert_eq!(web_get(env.web_port, "/api/status").unwrap().status, 401);
    // Only an explicit `open` needs the launcher, and it says how to fix it.
    let output = env.open(&[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("CONVERSATIONS_BROWSER"), "{stderr}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("#token="),
        "the owner link reached stdout"
    );
    let host_log = std::fs::read_to_string(&log).unwrap();
    assert!(
        !host_log.contains("CONVERSATIONS_BROWSER"),
        "host startup tried to open a browser: {host_log}"
    );
}

#[test]
fn an_occupied_web_port_neither_blocks_host_startup_nor_is_taken_over() {
    let mut env = Env::new();
    let port = env.web_port;
    // Something else owns the configured port before the host starts.
    let foreign = TcpListener::bind(("127.0.0.1", port)).unwrap();
    std::thread::spawn(move || {
        for stream in foreign.incoming().flatten() {
            let body = "FOREIGN SERVICE 3f9c2e7d11a4b";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
        }
    });
    let foreign_alive = || {
        web_get(port, "/")
            .is_some_and(|r| r.status == 200 && r.body.contains("FOREIGN SERVICE 3f9c2e7d11a4b"))
    };
    // A single probe can lose to nextest scheduling on a loaded runner; a
    // takeover could never recover, so retrying keeps the proof strict.
    let assert_foreign_alive = |what: &str| wait(what, foreign_alive);

    // Host startup completes anyway and the host answers on its socket.
    let log = env.home.path().join("host.log");
    env.spawn_host_logged(&log);
    wait("the host socket", || env.runtime.join("host.sock").exists());
    let status = ask_host(&env.runtime.join("host.sock"), json!({"type": "status"}));
    assert_eq!(status["type"], "status", "{status}");

    // The port still belongs to its original owner, and the host said why it
    // is not serving the UI: clear guidance, no silent takeover.
    assert_foreign_alive("the foreign service to keep its port");
    wait("the host to report the port conflict", || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("Local web UI not started")
    });
    let logged = std::fs::read_to_string(&log).unwrap();
    if has_shared_frontend() {
        assert!(logged.contains(&format!("localhost:{port}")), "{logged}");
    } else {
        assert!(logged.contains("no shared frontend"), "{logged}");
    }
    assert!(logged.contains("host configure --web-port"), "{logged}");

    // `open` asks the running host, which serves no UI here: it says how to
    // get one and never serves the port itself or opens a browser.
    let output = env.open(&[]);
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(stderr.contains("not serving its local web UI"), "{stderr}");
    assert!(stderr.contains("host configure --web"), "{stderr}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("#token="));
    assert!(
        !env.home.path().join("open.args").exists(),
        "a browser was opened"
    );
    assert_foreign_alive("the foreign service to outlive the `open` attempt");
    env.stop_everything();
}

#[test]
fn open_without_a_served_ui_fails_clearly_and_never_serves_one_itself() {
    let mut env = Env::new();
    std::fs::write(
        env.settings.as_ref().unwrap(),
        json!({"web": {"enabled": false, "port": env.web_port}}).to_string(),
    )
    .unwrap();
    env.start_host();
    for extra in [&[][..], &["--reset-browser-access"]] {
        let output = env.open(extra);
        assert!(!output.status.success(), "{extra:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("not serving its local web UI"), "{stderr}");
        assert!(stderr.contains("host configure --web true"), "{stderr}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("#token="));
    }
    assert!(
        !env.home.path().join("open.args").exists(),
        "a browser was opened"
    );
    assert!(
        web_get(env.web_port, "/api/local").is_none(),
        "something serves the disabled UI's port"
    );
}

#[test]
fn management_commands_select_the_runtime_and_never_start_a_host() {
    let env = Env::new();
    let socket = env.runtime.join("host.sock");
    // Offline approval state is read from the selected runtime, chosen by flag
    // or by environment, without a host.
    let identity = env.host_id();
    let mut command = env.command();
    command
        .env("CONVERSATIONS_HOST_DIR", &env.runtime)
        .args(["credential", "identity"]);
    let output = owned_process::run(&mut command, b"", Duration::from_secs(20)).unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), identity);
    // Commands answered by the host say how to start it, and start nothing.
    for (group, args) in [
        ("host", &["status"][..]),
        ("passkey", &["pending"]),
        ("open", &[]),
        ("open", &["--reset-browser-access"]),
    ] {
        let output = env.management(group, args);
        assert!(!output.status.success(), "{group} {args:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("start it with `arrakis-conversations host`"),
            "{group} {args:?}: {stderr}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("#token="));
        assert!(!socket.exists(), "{group} {args:?} started a host");
    }
    assert!(
        !env.home.path().join("open.args").exists(),
        "a browser was opened"
    );
    assert!(
        web_get(env.web_port, "/api/local").is_none(),
        "a management command served the web UI"
    );
    assert!(!env.home.path().join(".conversations-host-local").exists());
}

#[test]
fn hoisted_management_reaches_a_running_host_in_a_custom_runtime() {
    let mut env = Env::new();
    env.start_host();
    start_locally(&env);
    wait("local history", || history_holds_the_transcript(&env));
    let get = env.management("history", &["get", SESSION, "--json"]);
    assert!(get.status.success(), "{get:?}");
    assert!(String::from_utf8_lossy(&get.stdout).contains("xylophone"));
    let mut command = env.command();
    command
        .env("CONVERSATIONS_HOST_DIR", &env.runtime)
        .args(["history", "search", "xylophone"]);
    let search = owned_process::run(&mut command, b"", Duration::from_secs(20)).unwrap();
    assert!(search.status.success(), "{search:?}");
    assert!(String::from_utf8_lossy(&search.stdout).contains("xylophone"));
    // The running host itself answers: this fixture configures no passkeys.
    let pending = env.management("passkey", &["pending"]);
    assert!(
        String::from_utf8_lossy(&pending.stderr).contains("passkeys are not configured"),
        "{pending:?}"
    );
    let status = env.management("host", &["status", "--json"]);
    assert!(status.status.success(), "{status:?}");
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(status.to_string().contains(SESSION), "{status}");
    // Nothing fell back to the default runtime.
    assert!(!env.home.path().join(".conversations-host-local").exists());
}

/// Run an opt-in journey over the real private channel. A missing browser or
/// node fails the test; it never skips or falls back to the relay.
fn over_the_private_channel<F: Future<Output = ()>>(journey: impl FnOnce(Private) -> F) {
    let chrome = rtc::required_env("PRIVATE_TRANSPORT_CHROME");
    let node = rtc::required_env("PRIVATE_TRANSPORT_NODE");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move { journey(Private::start(&chrome, &node).await).await });
}

/// The real central server in process and a headless Chromium on its page,
/// running the production private channel helper.
struct Private {
    browser: rtc::Browser,
    central: rtc::Central,
    _root: tempfile::TempDir,
}

impl Private {
    async fn start(chrome: &str, node: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let static_dir = rtc::build_static(node, &root);
        let central = rtc::start_central(&root, &static_dir).await;
        let browser = rtc::Browser::launch(chrome, &root.join("chrome"), &central.url).await;
        Self {
            browser,
            central,
            _root: temp,
        }
    }

    /// A fresh non-extractable P-256 software key in the page: `{kid, public}`.
    async fn key(&mut self, name: &str) -> Value {
        let key = self
            .browser
            .run(&format!("return await pt.extraKey({name:?});"))
            .await;
        assert!(key["kid"].is_string(), "{key}");
        key
    }

    async fn pin(&mut self, env: &Env) {
        rtc::pin_host(&mut self.browser, &env.host_id(), &env.host_public_key()).await;
    }

    /// One operation signed by `key` on its own fresh channel; returns the status.
    async fn op(&mut self, key: &str, op: &str, session: &str, args: Value) -> String {
        let result = rtc::private_op(&mut self.browser, key, op, session, &args).await;
        rtc::status(&result).to_owned()
    }

    /// Nothing the central server received exposed `needle`, encoded or not.
    fn never_exposed(&self, needle: &str) {
        let seen = self.central.recorded.containing(needle);
        assert!(seen.is_empty(), "central saw {needle:?} in {seen:?}");
    }
}

#[test]
fn a_signed_start_reply_or_reopen_over_the_plaintext_relay_launches_nothing_and_creates_no_record()
{
    let mut env = Env::new();
    env.start_host();
    let mut owner = Signer::new();
    let host_id = env.enroll(
        &mut owner,
        &[
            "session.start".into(),
            "session.reopen".into(),
            format!("session.write:{SESSION}"),
        ],
    );
    assert_eq!(env.sessions(), 0);

    // Every grant is in place, yet central can read these: the host refuses them.
    let started = env.central.relay(
        &owner,
        "session.start",
        SESSION,
        json!({"session": SESSION, "agent": "claude"}),
        &host_id,
    );
    assert_eq!(started, "refused");
    let reply = env.central.relay(
        &owner,
        "session.reply",
        SESSION,
        json!({"session": SESSION, "text": "hello from the owner"}),
        &host_id,
    );
    assert_eq!(reply, "refused");
    env.stop_everything();
    env.start_host();
    let reopened = env.central.relay(
        &owner,
        "session.reopen",
        SESSION,
        json!({"session": SESSION}),
        &host_id,
    );
    assert_eq!(reopened, "refused");
    std::thread::sleep(Duration::from_millis(300));
    assert!(env.runs().is_empty(), "{:?}", env.runs());
    assert_eq!(env.sessions(), 0, "a refused start created a record");
}

#[test]
#[ignore = "needs PRIVATE_TRANSPORT_CHROME and PRIVATE_TRANSPORT_NODE (real central, Chromium, production privateChannel)"]
fn a_fresh_normal_backend_signed_start_registers_binds_and_records_history_then_reopens_after_a_restart(
) {
    over_the_private_channel(|mut private| async move {
        let mut env = Env::private(&private.central.url);
        env.start_private_host().await;
        let owner = private.key("owner").await;
        env.enroll_browser(
            &owner,
            &[
                "session.start".into(),
                "session.reopen".into(),
                format!("session.write:{SESSION}"),
            ],
        );
        private.pin(&env).await;

        // No session record exists until the production start creates it.
        assert_eq!(env.sessions(), 0);
        let started = private
            .op(
                "owner",
                "session.start",
                SESSION,
                json!({"session": SESSION, "agent": "claude"}),
            )
            .await;
        assert_eq!(started, "executed");
        rtc::wait("the stub agent to run", || env.runs().len() == 1).await;
        assert!(
            env.runs()[0].contains(&format!("--session-id {SESSION}")),
            "{:?}",
            env.runs()
        );

        // The registration produced the canonical record with the native binding.
        let (agent, resume): (String, Option<String>) = env
            .authz()
            .query_row(
                "SELECT agent, resume_id FROM sessions WHERE session_id = ?1",
                [SESSION],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (agent.as_str(), resume.as_deref()),
            ("claude", Some(SESSION))
        );

        // A signed write works at once, with no restart.
        let reply = private
            .op(
                "owner",
                "session.reply",
                SESSION,
                json!({"session": SESSION, "text": "hello from the owner"}),
            )
            .await;
        assert_eq!(reply, "executed");
        rtc::wait("the reply to reach the agent's terminal", || {
            std::fs::read_to_string(env.stubs.join("claude.stdin"))
                .is_ok_and(|text| text.contains("hello from the owner"))
        })
        .await;

        // The transcript the stub wrote is ingested into local history (and FTS) by
        // the host itself, central being unable to store anything.
        let history = env.runtime.join("history.db");
        rtc::wait("local history to hold the transcript", || {
            history.exists()
                && rusqlite::Connection::open(&history)
                    .and_then(|c| c.query_row("SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'xylophone' AND session_id = ?1", [SESSION], |r| r.get::<_, i64>(0)))
                    .is_ok_and(|n| n >= 1)
        })
        .await;
        let (agent, native): (String, String) = rusqlite::Connection::open(&history)
            .unwrap()
            .query_row(
                "SELECT agent, native_id FROM messages WHERE session_id = ?1 LIMIT 1",
                [SESSION],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((agent.as_str(), native.as_str()), ("claude", SESSION));

        // Restart the host and its terminal. Reopen needs nothing but the canonical
        // record: this backend never wrote a checkpoint.
        env.stop_everything();
        assert!(
            !env.home.path().join("data").exists()
                || walk(&env.home.path().join("data"))
                    .iter()
                    .all(|p| !p.to_string_lossy().contains("open")),
            "a restore checkpoint exists"
        );
        env.start_private_host().await;
        let reopened = private
            .op(
                "owner",
                "session.reopen",
                SESSION,
                json!({"session": SESSION}),
            )
            .await;
        assert_eq!(reopened, "executed");
        rtc::wait("the reopened agent", || env.runs().len() == 2).await;
        assert!(
            env.runs()[1].contains(&format!("--resume {SESSION}")),
            "{:?}",
            env.runs()
        );

        // The prompt and the transcript never reached central, in any encoding.
        private.never_exposed("hello from the owner");
        private.never_exposed("xylophone");
    });
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            out.push(path.clone());
            if path.is_dir() {
                out.extend(walk(&path));
            }
        }
    }
    out
}

/// One framed request on the owner-only host socket (u32 length + JSON).
fn ask_host(socket: &Path, request: Value) -> Value {
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

/// Start the session on the owner-only local socket (no signed start, no seeded
/// rows) and return once the registration made its canonical record.
fn start_locally(env: &Env) {
    let started = ask_host(
        &env.runtime.join("host.sock"),
        json!({"type": "start", "launch": {"id": SESSION, "agent": "claude", "cwd": env.work, "command": ["claude"]}}),
    );
    assert_eq!(started["type"], "started", "{started}");
    let (agent, resume): (String, Option<String>) = env
        .authz()
        .query_row(
            "SELECT agent, resume_id FROM sessions WHERE session_id = ?1",
            [SESSION],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the registration created no canonical record");
    assert_eq!(
        (agent.as_str(), resume.as_deref()),
        ("claude", Some(SESSION))
    );
}

fn history_holds_the_transcript(env: &Env) -> bool {
    let path = env.runtime.join("history.db");
    // Read-only, and only once the host created it: a stray writer must not
    // pre-create the private database with the wrong mode.
    path.exists()
        && rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'xylophone'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .is_ok_and(|n| n >= 1)
}

fn stdin_of(env: &Env) -> String {
    std::fs::read_to_string(env.stubs.join("claude.stdin")).unwrap_or_default()
}

#[test]
fn a_locally_started_session_gets_its_canonical_record_and_history_and_the_relay_still_cannot_write_to_it(
) {
    let mut env = Env::new();
    env.start_host();
    let mut owner = Signer::new();
    // The credential is approved for a session that does not exist yet.
    let host_id = env.enroll(&mut owner, &[format!("session.write:{SESSION}")]);
    let args = json!({"session": SESSION, "text": "hello before the session exists"});
    assert_eq!(
        env.central
            .relay(&owner, "session.reply", SESSION, args, &host_id),
        "refused"
    );

    // Local provenance only: this proves nothing about a remote start.
    start_locally(&env);

    // The record exists and the grant matches, yet central can read the text.
    let args = json!({"session": SESSION, "text": "hello over the relay"});
    assert_eq!(
        env.central
            .relay(&owner, "session.reply", SESSION, args, &host_id),
        "refused"
    );
    // Its native transcript reaches local history through the canonical binding.
    wait("local history", || history_holds_the_transcript(&env));
    assert!(!stdin_of(&env).contains("hello"), "{}", stdin_of(&env));
}

#[test]
#[ignore = "needs PRIVATE_TRANSPORT_CHROME and PRIVATE_TRANSPORT_NODE (real central, Chromium, production privateChannel)"]
fn a_locally_started_session_gets_its_canonical_record_so_signed_grants_work_without_a_restart() {
    over_the_private_channel(|mut private| async move {
        let mut env = Env::private(&private.central.url);
        env.start_private_host().await;
        let owner = private.key("owner").await;
        // The credential is approved for a session that does not exist yet.
        env.enroll_browser(&owner, &[format!("session.write:{SESSION}")]);
        private.pin(&env).await;
        let args = json!({"session": SESSION, "text": "hello before the session exists"});
        assert_eq!(
            private.op("owner", "session.reply", SESSION, args).await,
            "refused"
        );

        start_locally(&env);

        let args = json!({"session": SESSION, "text": "hello after the local start"});
        assert_eq!(
            private.op("owner", "session.reply", SESSION, args).await,
            "executed"
        );
        rtc::wait("the write to reach the terminal", || {
            stdin_of(&env).contains("hello after the local start")
        })
        .await;
        rtc::wait("local history", || history_holds_the_transcript(&env)).await;
        assert!(!stdin_of(&env).contains("before the session exists"));
        private.never_exposed("hello after the local start");
    });
}

/// The real host, the real wrapper it spawns, and a strict stateful `claude`
/// stub that models ONE running operation (system Perl; no agent is installed
/// or reached). `WORK <name>` + Enter starts work that appends a tick every
/// 50 ms and finishes after 60; Escape while working is its non-exiting cancel.
/// Anything else (Ctrl-C, Escape while idle, other control bytes) is logged as
/// a violation; it never exits on its own.
const WORK_STUB: &str = r#"use strict; use warnings; use IO::Select; use Cwd 'getcwd';
my ($stubs, @args) = @ARGV;
my $id = ''; for my $i (0..$#args-1) { $id = $args[$i+1] if $args[$i] =~ /^--(session-id|resume)$/; }
sub append { my ($file, $line) = @_; open(my $f, '>>', $file) or die; print $f "$line\n"; close $f; }
sub event { append("$stubs/work.events", $_[0]); }
append("$stubs/claude.runs", "$$ " . getppid() . " @args");
(my $project = getcwd()) =~ tr{./}{--};
my $dir = "$ENV{HOME}/.claude/projects/$project"; system('/bin/mkdir', '-p', $dir);
my $transcript = "$dir/$id.jsonl";
sub end_turn {
  append($transcript, '{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[]}}');
  append($transcript, '{"type":"system","subtype":"turn_duration"}');
}
append($transcript, '{"type":"user","message":{"role":"user","content":"hi"}}'); end_turn();
system('/bin/stty', 'raw', '-echo');
my ($line, $job, $ticks) = ('', undef, 0);
my $select = IO::Select->new(\*STDIN);
while (!-e "$stubs/stop") {
  if ($select->can_read(0.05)) {
    my $n = sysread(STDIN, my $byte, 1);
    last unless $n;
    if ($byte eq "\e") {
      if (defined $job) { event("cancel $job"); end_turn(); undef $job; } else { event("VIOLATION escape while idle"); }
    } elsif ($byte eq "\r") {
      if ($line =~ /^WORK (\w+)$/) {
        if (defined $job) { event("VIOLATION work while busy: $1"); }
        else { $job = $1; $ticks = 0; append($transcript, '{"type":"user","message":{"role":"user","content":"work"}}'); event("start $job"); }
      } elsif (length $line) { event("VIOLATION line: $line"); }
      $line = '';
    } elsif ($byte =~ /[\x20-\x7e]/) { $line .= $byte; }
    else { event(sprintf("VIOLATION byte 0x%02x", ord $byte)); }
  }
  if (defined $job) {
    $ticks++;
    append("$stubs/work.ticks", "tick $job");
    if ($ticks >= 60) { event("done $job"); end_turn(); undef $job; }
  }
}
append("$stubs/claude.stopped", $$);
"#;

/// The recording stub's terminal loop: every byte, unchanged, appended to
/// `claude.stdin` until `stop` (or hang-up); then it acknowledges and exits.
const RECORDER: &str = r#"use strict; use warnings; use IO::Select;
my ($stubs) = @ARGV;
open(my $out, '>>', "$stubs/claude.stdin") or die; binmode $out; $out->autoflush(1);
binmode STDIN;
my $select = IO::Select->new(\*STDIN);
while (!-e "$stubs/stop") {
  next unless $select->can_read(0.05);
  my $n = sysread(STDIN, my $bytes, 4096);
  last unless $n;
  print $out $bytes;
}
open(my $done, '>>', "$stubs/claude.stopped") or die; print $done "$$\n"; close $done;
"#;

/// Install [`WORK_STUB`] as the agent.
fn work_stub(env: &Env) {
    std::fs::write(env.stubs.join("work.pl"), WORK_STUB).unwrap();
    write_executable(
        &env.stubs.join("claude"),
        &format!(
            "#!/bin/sh\nexec /usr/bin/perl '{s}/work.pl' '{s}' \"$@\"\n",
            s = env.stubs.display()
        ),
    );
}

fn events(env: &Env) -> String {
    std::fs::read_to_string(env.stubs.join("work.events")).unwrap_or_default()
}

fn ticks(env: &Env, job: &str) -> usize {
    std::fs::read_to_string(env.stubs.join("work.ticks"))
        .unwrap_or_default()
        .lines()
        .filter(|l| *l == format!("tick {job}"))
        .count()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for existence (observation).
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn work_signed_over_the_plaintext_relay_never_reaches_a_running_agent() {
    let mut env = Env::new();
    work_stub(&env);
    env.start_host();
    let (mut owner, mut guest) = (Signer::new(), Signer::new());
    let host_id = env.enroll(
        &mut owner,
        &["session.start".into(), format!("session.write:{SESSION}")],
    );
    env.enroll(&mut guest, &[format!("session.write:{SESSION}")]);
    start_locally(&env);
    wait("the stub agent to run", || env.runs().len() == 1);
    let pid: i32 = env.runs()[0].split(' ').next().unwrap().parse().unwrap();
    for (who, name) in [(&guest, "guest"), (&owner, "owner")] {
        let relayed = env.central.relay(
            who,
            "session.reply",
            SESSION,
            json!({"session": SESSION, "text": format!("WORK {name}")}),
            &host_id,
        );
        assert_eq!(relayed, "refused", "{name}");
    }
    std::thread::sleep(Duration::from_millis(400));
    assert!(events(&env).is_empty(), "{}", events(&env));
    assert_eq!((ticks(&env, "guest"), ticks(&env, "owner")), (0, 0));
    assert!(alive(pid));
    assert_eq!(env.runs().len(), 1, "{:?}", env.runs());
}

#[test]
#[ignore = "needs PRIVATE_TRANSPORT_CHROME and PRIVATE_TRANSPORT_NODE (real central, Chromium, production privateChannel)"]
fn revoking_a_credential_cancels_its_running_work_and_the_same_agent_takes_a_later_turn() {
    over_the_private_channel(|mut private| async move {
        let mut env = Env::private(&private.central.url);
        work_stub(&env);
        env.start_private_host().await;
        let owner = private.key("owner").await;
        let guest = private.key("guest").await;
        env.enroll_browser(
            &owner,
            &["session.start".into(), format!("session.write:{SESSION}")],
        );
        let guest_kid = env.enroll_browser(&guest, &[format!("session.write:{SESSION}")]);
        private.pin(&env).await;
        let started = private
            .op(
                "owner",
                "session.start",
                SESSION,
                json!({"session": SESSION, "agent": "claude"}),
            )
            .await;
        assert_eq!(started, "executed");
        rtc::wait("the stub agent to run", || env.runs().len() == 1).await;
        let pid: i32 = env.runs()[0].split(' ').next().unwrap().parse().unwrap();

        // The guest's channel stays open across the revocation.
        let opened = rtc::open_channel(&mut private.browser, "guest", "guest").await;
        assert!(opened["peer"].is_string(), "{opened}");
        let work = |name: &str| json!({"session": SESSION, "text": format!("WORK {name}")});
        let guest_work = rtc::operate(
            &mut private.browser,
            "guest",
            "session.reply",
            SESSION,
            &work("guest"),
        )
        .await;
        assert_eq!(rtc::status(&guest_work), "executed", "{guest_work}");
        rtc::wait("the guest's operation to run", || ticks(&env, "guest") >= 3).await;

        let output = env
            .command()
            .args(["--server", &env.server, "credential", "--runtime-dir"])
            .arg(&env.runtime)
            .args(["revoke", &guest_kid])
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            output.status.success(),
            "{said}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            said.contains(&format!("Session {SESSION}: interrupt requested")),
            "{said}"
        );
        rtc::wait("the running operation to be cancelled", || {
            events(&env).lines().any(|l| l == "cancel guest")
        })
        .await;
        let stopped = ticks(&env, "guest");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            ticks(&env, "guest"),
            stopped,
            "the revoked operation kept running"
        );
        assert!(!events(&env).contains("done guest"));
        assert!(alive(pid), "the agent exited on revocation");
        let late = rtc::operate(
            &mut private.browser,
            "guest",
            "session.reply",
            SESSION,
            &work("late"),
        )
        .await;
        assert_eq!(late["status"], "refused", "{late}");
        rtc::close_channel(&mut private.browser, "guest").await;

        // A later authorized turn completes in the SAME agent process.
        assert_eq!(
            private
                .op("owner", "session.reply", SESSION, work("owner"))
                .await,
            "executed"
        );
        rtc::wait("the owner's turn to complete", || {
            events(&env).lines().any(|l| l == "done owner")
        })
        .await;
        assert_eq!(ticks(&env, "owner"), 60);
        assert!(alive(pid));
        assert_eq!(
            env.runs().len(),
            1,
            "the agent was relaunched: {:?}",
            env.runs()
        );
        assert!(!events(&env).contains("VIOLATION"), "{}", events(&env));
        assert!(!events(&env).contains("late"));
        private.never_exposed("WORK guest");
        private.never_exposed("WORK owner");
    });
}

/// The first fixed seed whose Ed25519 public key gets a credential id with
/// `prefix`: the host derives ids as base64url(SHA-256(public key)), so about
/// one key in 64 starts with `-` and one in 4096 with `--`.
fn key_with_id_prefix(prefix: &str) -> (String, String) {
    (0u64..)
        .map(|n| {
            let mut seed = [0u8; 32];
            seed[..8].copy_from_slice(&n.to_le_bytes());
            let pair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
            let public = pair.public_key().as_ref().to_vec();
            let id = URL_SAFE_NO_PAD.encode(ring::digest::digest(&ring::digest::SHA256, &public));
            (STANDARD.encode(public), id)
        })
        .find(|(_, id)| id.starts_with(prefix) && !id.starts_with(&format!("{prefix}-")))
        .unwrap()
}

#[test]
fn credentials_whose_ids_start_with_hyphens_can_be_revoked() {
    let env = Env::new();
    let revoke = |args: &[&str]| -> Value {
        let output = env.management("credential", &[&["revoke"][..], args].concat());
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("{args:?}: {}", String::from_utf8_lossy(&output.stdout)))
    };
    for prefix in ["-", "--"] {
        let (public, expected) = key_with_id_prefix(prefix);
        let approved = env.add_credential(None, &public, &["session.start".into()]);
        let id = approved.split_whitespace().nth(2).unwrap();
        assert_eq!(id, expected, "{approved}");
        let first = revoke(&[id, "--json"]);
        assert_eq!(first["credential"], id, "{first}");
        assert_eq!(first["already_revoked"], false, "{first}");
        assert_eq!(first["grants"], 1, "{first}");
        let again = revoke(&["--json", id]);
        assert_eq!(again["credential"], id, "{again}");
        assert_eq!(again["already_revoked"], true, "{again}");
    }
}

/// Run one hook command the way Claude does, through a shell with the event
/// on stdin, bounded by the hook's own configured timeout. Anything the hook
/// leaves running in its process group ends with it.
fn run_hook(
    command: &str,
    event: &Value,
    timeout: Duration,
    env: &Env,
) -> Result<std::process::Output, String> {
    owned_process::run(
        Command::new("/bin/sh")
            .args(["-c", command])
            .env_remove("CONVERSATIONS_HOST_DIR")
            .env("HOME", env.home.path())
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&env.work),
        event.to_string().as_bytes(),
        timeout,
    )
}

#[test]
fn hook_callbacks_are_bounded_and_their_forked_processes_end_with_them() {
    let env = Env::new();
    let pid = |name: &str| -> i32 {
        std::fs::read_to_string(env.home.path().join(name))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let gone = |name: &str| {
        let pid = pid(name);
        // A killed child stays a zombie until its new parent reaps it.
        wait(&format!("{name} to be gone"), || !alive(pid));
    };
    // The shell forks the long-running child rather than exec-ing it, and
    // records both PIDs.
    let forks = |prefix: &str, then: &str| {
        let file = |name: &str| env.home.path().join(format!("{prefix}.{name}.pid"));
        format!(
            "echo $$ > '{}.tmp' && mv '{0}.tmp' '{0}'; /bin/sleep 60 & echo $! > '{}.tmp' && mv '{1}.tmp' '{1}'; {then}",
            file("shell").display(),
            file("fork").display(),
        )
    };

    // A callback that never finishes is stopped at its deadline, with its fork.
    let started = Instant::now();
    let error = run_hook(
        &forks("hung", "wait"),
        &json!({}),
        Duration::from_secs(2),
        &env,
    )
    .unwrap_err();
    assert!(error.contains("did not finish"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(15));
    gone("hung.shell.pid");
    gone("hung.fork.pid");

    // A callback that returns while a fork it started is still running.
    let output = run_hook(
        &forks("returned", "exit 0"),
        &json!({}),
        Duration::from_secs(10),
        &env,
    )
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    gone("returned.fork.pid");

    // A failing assertion while the callback still runs does not leak it.
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let shell = owned_process::Owned::spawn(
            Command::new("/bin/sh").args(["-c", &forks("unwound", "wait")]),
        );
        wait("the callback to fork", || {
            env.home.path().join("unwound.fork.pid").exists() || shell.exited()
        });
        panic!("assertion failed while the callback was running");
    }));
    assert!(unwound.is_err());
    gone("unwound.shell.pid");
    gone("unwound.fork.pid");
}

#[test]
fn native_hooks_keep_a_quiet_session_busy_until_background_work_finishes() {
    // The normal stub agent runs under the real wrapper. It records its parent
    // (the wrapper) and the arguments the wrapper gave it, including the hook
    // settings the wrapper generated for its own lifetime. Each lifecycle event
    // is delivered the way Claude delivers it: every hook command those
    // settings configure for the event, run by a shell with the event on stdin.
    // This proves the callback-to-host seam, not that a vendor process runs
    // its configured hooks.
    let mut env = Env::new();
    env.start_host();
    start_locally(&env);
    wait("the agent to start", || !env.runs().is_empty());
    let run = env.runs().remove(0);
    let words: Vec<&str> = run.split(' ').collect();
    let wrapper_pid: i32 = words[1].parse().unwrap();
    assert!(alive(wrapper_pid), "{run}");
    let settings_path = words
        .iter()
        .position(|word| *word == "--settings")
        .map(|index| PathBuf::from(words[index + 1]))
        .unwrap_or_else(|| panic!("the wrapper installed no hook settings: {run}"));
    assert_eq!(
        settings_path.file_name().unwrap().to_string_lossy(),
        format!("{SESSION}-{wrapper_pid}.settings.json"),
        "the hook settings are not scoped to the agent's wrapper: {run}"
    );
    let settings: Value = serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    let deliver = |event: &Value| {
        let entries = settings["hooks"][event["hook_event_name"].as_str().unwrap()]
            .as_array()
            .unwrap_or_else(|| panic!("no hooks configured for {event}"));
        assert!(!entries.is_empty(), "no hooks configured for {event}");
        for hook in entries
            .iter()
            .flat_map(|entry| entry["hooks"].as_array().unwrap())
        {
            let command = hook["command"].as_str().unwrap();
            assert!(
                command.ends_with(&format!(
                    " agent-hook --launch-id '{SESSION}' --wrapper-pid {wrapper_pid}"
                )),
                "{command}"
            );
            let timeout = Duration::from_secs(hook["timeout"].as_u64().unwrap());
            let output = run_hook(command, event, timeout, &env)
                .unwrap_or_else(|error| panic!("{command}: {error}"));
            assert!(output.status.success(), "{command}: {output:?}");
            assert!(output.stdout.is_empty(), "{command}: {output:?}");
        }
    };
    let events = [
        (json!({"hook_event_name":"UserPromptSubmit"}), "working"),
        (
            json!({"hook_event_name":"SubagentStart","agent_id":"child"}),
            "working",
        ),
        (
            json!({"hook_event_name":"Stop","background_tasks":[{"type":"subagent","id":"child"}],"session_crons":[]}),
            "background",
        ),
        (
            json!({"hook_event_name":"PostToolUse","agent_id":"child"}),
            "background",
        ),
        (
            json!({"hook_event_name":"SubagentStop","agent_id":"child","background_tasks":[{"type":"shell","id":"shell"}],"session_crons":[]}),
            "background",
        ),
        (
            json!({"hook_event_name":"Stop","background_tasks":[],"session_crons":[]}),
            "ready",
        ),
        (json!({"hook_event_name":"Stop"}), "unknown"),
    ];
    for (event, expected) in events {
        deliver(&event);
        wait("native readiness on the host status transport", || {
            let response = ask_host(&env.runtime.join("host.sock"), json!({"type":"status"}));
            response["sessions"].as_array().is_some_and(|sessions| {
                sessions.iter().any(|session| {
                    session["launch"]["id"] == SESSION && session["readiness"] == expected
                })
            })
        });
    }
    env.stop_everything();
    // No disconnected terminal can retain a completed turn.
    env.start_host();
    let response = ask_host(&env.runtime.join("host.sock"), json!({"type":"status"}));
    assert!(response["sessions"].as_array().unwrap().is_empty());
}
