#![cfg(unix)]
//! `start-agent`, `start-agent --resume` and `upload`, as real processes
//! against the REAL central server router (every request recorded), in an
//! isolated user. A stub `claude` writes its transcript in Claude's own format
//! and, when told to locally, asks for a handoff through the real in-harness
//! MCP server, exactly as an agent does.
//!
//! What must hold: each started agent is a canonical local host registration
//! (`host status`), its history is in the local history database, local keys
//! and the in-harness handoff keep working, a resumed native session keeps its
//! one canonical record, and nothing private — transcript text, typed keys,
//! the handoff brief or the working directory — reaches central, in a request
//! or in its database. `upload` publishes through the signed publication
//! lifecycle, never the legacy upload, and refuses a session id that already
//! has legacy central data, leaving that data as it was.

#[path = "support/owned_process.rs"]
mod owned_process;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use conversations_server::{build_router_with_auth, AppState, Config, SqliteRepo};
use portable_pty::{CommandBuilder, PtySize};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_arrakis-conversations");
const ALICE: &str = "alice@example.com";
/// Written by the agent into its transcript only.
const TRANSCRIPT_MARK: &str = "transcriptmarkwolfsbane";
/// Typed only at the local terminal.
const TYPED_MARK: &str = "typedmarkcelandine";
/// Only in the brief the agent hands off locally.
const BRIEF_MARK: &str = "briefmarkhellebore";
/// A directory name that appears only in the working directory's path.
const CWD_MARK: &str = "cwdmarkfoxglove";

// ---------------------------------------------------------------- central --

/// `"METHOD uri"` and body of one request.
type Request = (String, Vec<u8>);

#[derive(Clone, Default)]
struct Recorded(Arc<Mutex<Vec<Request>>>);

impl Recorded {
    fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(l, _)| l.clone())
            .collect()
    }

    /// Requests that exposed `needle` in their URI, body, or any base64 payload
    /// inside the body.
    fn containing(&self, needle: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, body)| exposes(&exposed(line, body), needle))
            .map(|(line, _)| line.clone())
            .collect()
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The URI and body, plus every JSON string in the body decoded as base64.
fn exposed(line: &str, body: &[u8]) -> Vec<u8> {
    let mut out = line.as_bytes().to_vec();
    out.push(b'\n');
    out.extend_from_slice(body);
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        decode_strings(&value, 0, &mut out);
    }
    out
}

fn decode_strings(value: &Value, depth: usize, out: &mut Vec<u8>) {
    if depth > 4 {
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

/// `needle` and its base64 forms at every alignment.
fn exposes(haystack: &[u8], needle: &str) -> bool {
    let mut forms = vec![needle.as_bytes().to_vec()];
    for engine in [&URL_SAFE_NO_PAD, &STANDARD] {
        for offset in 0..3 {
            let mut padded = vec![0u8; offset];
            padded.extend_from_slice(needle.as_bytes());
            let encoded = engine.encode(&padded);
            let encoded = encoded.trim_end_matches('=');
            let core = &encoded[if offset == 0 { 0 } else { 4 }..encoded.len() - 4];
            forms.push(core.as_bytes().to_vec());
        }
    }
    forms.iter().any(|form| contains(haystack, form))
}

/// The real server router on loopback with auth on, every request recorded,
/// and Alice's login cookie.
struct Central {
    /// Serves the router; dropping it stops the server.
    _runtime: tokio::runtime::Runtime,
    url: String,
    cookie: String,
    db: PathBuf,
    recorded: Recorded,
    faults: Arc<Mutex<Faults>>,
    http: reqwest::blocking::Client,
}

/// Network faults on `POST /api/publications`, applied around the REAL router.
#[derive(Default)]
struct Faults {
    /// Hold each publication request this long before the server sees it.
    delay: Duration,
    /// Let the server apply this many appends, then lose each answer (502).
    lose_append_acks: usize,
    in_flight: usize,
    /// Most publication requests the server was handling at once.
    max_in_flight: usize,
}

impl Central {
    fn start(dir: &Path) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = dir.join("central.sqlite");
        let recorded = Recorded::default();
        let log = recorded.clone();
        let faults = Arc::new(Mutex::new(Faults::default()));
        let fault = faults.clone();
        let url = runtime.block_on(async {
            let repo = SqliteRepo::connect(&format!("sqlite://{}?mode=rwc", db.display()))
                .await
                .unwrap();
            let config = Config {
                auth_enabled: true,
                password_auth_enabled: true,
                cookie_secure: false,
                ..Config::default()
            };
            let app = build_router_with_auth(AppState::new(Arc::new(repo), Arc::new(config)))
                .await
                .unwrap()
                .layer(axum::middleware::from_fn(
                    move |request: axum::extract::Request, next: axum::middleware::Next| {
                        let log = log.clone();
                        let fault = fault.clone();
                        async move {
                            let (parts, body) = request.into_parts();
                            let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
                                .await
                                .unwrap_or_default();
                            log.0
                                .lock()
                                .unwrap()
                                .push((format!("{} {}", parts.method, parts.uri), bytes.to_vec()));
                            let publication = parts.method == axum::http::Method::POST
                                && parts.uri.path() == "/api/publications";
                            let append = publication
                                && serde_json::from_slice::<Value>(&bytes)
                                    .is_ok_and(|body| body["chunk"].is_string());
                            let delay = if publication {
                                let mut f = fault.lock().unwrap();
                                f.in_flight += 1;
                                f.max_in_flight = f.max_in_flight.max(f.in_flight);
                                f.delay
                            } else {
                                Duration::ZERO
                            };
                            tokio::time::sleep(delay).await;
                            let response = next
                                .run(axum::extract::Request::from_parts(
                                    parts,
                                    axum::body::Body::from(bytes),
                                ))
                                .await;
                            if !publication {
                                return response;
                            }
                            let mut f = fault.lock().unwrap();
                            f.in_flight -= 1;
                            if append && response.status().is_success() && f.lose_append_acks > 0 {
                                f.lose_append_acks -= 1;
                                return axum::response::IntoResponse::into_response(
                                    axum::http::StatusCode::BAD_GATEWAY,
                                );
                            }
                            response
                        }
                    },
                ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            url
        });
        let http = reqwest::blocking::Client::new();
        let response = http
            .post(format!("{url}/auth/register"))
            .json(
                &json!({"email": ALICE, "password": "test-password-123", "display_name": "Alice"}),
            )
            .send()
            .unwrap();
        assert_eq!(response.status(), 201);
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        recorded.0.lock().unwrap().clear();
        Self {
            _runtime: runtime,
            url,
            cookie,
            db,
            recorded,
            faults,
            http,
        }
    }

    /// One request as Alice's browser: `(status, body)`.
    fn web(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let response = self
            .http
            .request(method, format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .json(&body)
            .send()
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().unwrap_or(Value::Null))
    }

    /// The server's database files (and journal), as bytes.
    fn stored(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for suffix in ["", "-wal", "-journal"] {
            let path = PathBuf::from(format!("{}{suffix}", self.db.display()));
            out.extend(std::fs::read(path).unwrap_or_default());
        }
        out
    }

    /// Everything private this test produced that central saw or stored.
    fn private_exposure(&self) -> Value {
        let stored = self.stored();
        let mut found = serde_json::Map::new();
        for mark in [TRANSCRIPT_MARK, TYPED_MARK, BRIEF_MARK, CWD_MARK] {
            let requests = self.recorded.containing(mark);
            if !requests.is_empty() || exposes(&stored, mark) {
                found.insert(
                    mark.into(),
                    json!({"requests": requests, "stored": exposes(&stored, mark)}),
                );
            }
        }
        Value::Object(found)
    }

    /// Requests on the old live-mirror, transcript-upload and PTY paths. The
    /// host's inbound signed-operation stream is not one of them.
    fn legacy_channels(&self) -> Vec<String> {
        self.recorded
            .lines()
            .into_iter()
            .filter(|line| {
                (line.contains("/api/live") && line != "GET /api/live/control/stream")
                    || line.starts_with("POST /api/sessions")
                    || line.contains("/host/ws")
                    || line.contains("/pty/")
            })
            .collect()
    }
}

// ------------------------------------------------------------------- user --

/// An isolated user: HOME, config and data directories, a PATH of recording
/// stubs plus the system directories, and nothing inherited from the caller's
/// agent, host or cmux environment.
struct Env {
    home: tempfile::TempDir,
    stubs: PathBuf,
    work: PathBuf,
    central: Central,
    output: Arc<Mutex<Vec<u8>>>,
    terminal: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Runtime directory of the host the sessions are expected in; `None` is
    /// the default `~/.conversations-host-local`.
    runtime: Option<PathBuf>,
    /// The user's local host, once started.
    host: Option<owned_process::Owned>,
    _pty: Option<Box<dyn portable_pty::MasterPty + Send>>,
}

impl Env {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().canonicalize().unwrap();
        let (stubs, work) = (root.join("stubs"), root.join(CWD_MARK).join("work"));
        for dir in [&stubs, &work] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for agent in ["claude", "codex", "opencode"] {
            for system in ["/usr/bin", "/bin"] {
                // A missing stub must never fall back to an installed agent.
                assert!(
                    !Path::new(system).join(agent).exists(),
                    "refusing to run: {system}/{agent} would shadow a missing stub"
                );
            }
        }
        // The stub records `pid wrapped-id argv`, writes a two-message Claude
        // transcript for the session it was told it is, then appends the raw
        // bytes its terminal delivers. Typing `please-handoff` makes it ask for
        // a local handoff through the real MCP server named by its wrapper,
        // as an agent's harness does. It stops on its own once the test
        // writes `stop` (or its directory is gone) and reports `done.<pid>`.
        // The OpenCode stub fails the bridge's version probe, so the bridge
        // starts it as given, and every subcommand probe such as `db path`:
        // it has no database of its own.
        for agent in ["claude", "opencode"] {
            write_executable(
                &stubs.join(agent),
                &format!(
                    r#"#!/bin/sh
[ "$1" = --version ] && exit 1
[ "{agent}" = opencode ] && case "$1" in -*) ;; *) exit 1 ;; esac
echo "$$ ${{CONVERSATIONS_WRAPPED:--}} {agent} $*" >> "{stubs}/runs"
id=""; prev=""
for a in "$@"; do
  case "$prev" in --session-id|--resume) id="$a";; esac
  prev="$a"
done
if [ "{agent}" = claude ] && [ -n "$id" ]; then
  dir="$HOME/.claude/projects/$(pwd -P | /usr/bin/tr './' '--')"
  /bin/mkdir -p "$dir"
  printf '%s\n' '{{"type":"user","uuid":"'"$id"'-u1","sessionId":"'"$id"'","timestamp":"2026-11-02T10:00:00.000Z","message":{{"role":"user","content":"neutral opener"}}}}' >> "$dir/$id.jsonl"
  printf '%s\n' '{{"type":"user","uuid":"'"$id"'-u2","sessionId":"'"$id"'","timestamp":"2026-11-02T10:00:01.000Z","message":{{"role":"user","content":"{TRANSCRIPT_MARK} private turn"}}}}' >> "$dir/$id.jsonl"
fi
exec /usr/bin/perl -MIO::Select -MIO::Handle -MIPC::Open2 -e '
my ($d) = @ARGV; my $s = IO::Select->new(\*STDIN);
open my $f, ">>", "$d/stdin" or die $!; $f->autoflush(1);
my $seen = ""; my $asked = 0;
while (-d $d && !-e "$d/stop") {{
  if ($s->can_read(0.1)) {{
    my $n = sysread(STDIN, my $b, 4096); last unless $n;
    print $f $b; $seen .= $b;
  }}
  if (!$asked && $seen =~ /please-handoff/) {{
    $asked = 1;
    my $pid = open2(my $out, my $in, $ENV{{CONVERSATIONS_CLI}}, "--server", $ENV{{CONVERSATIONS_SERVER}}, "mcp", "serve");
    $in->autoflush(1);
    print $in qq({{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"stub","version":"0"}}}}}}\n);
    my $init = <$out>;
    print $in qq({{"jsonrpc":"2.0","method":"notifications/initialized"}}\n);
    print $in qq({{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"sessions_handoff","arguments":{{"brief":"{BRIEF_MARK} continue here","idempotency_key":"stub-handoff-1"}}}}}}\n);
    my $result = "";
    while (my $line = <$out>) {{ if ($line =~ /"id":2/) {{ $result = $line; last; }} }}
    close $in; waitpid($pid, 0);
    open my $h, ">", "$d/handoff.$$" or die $!; print $h $result; close $h;
  }}
}}
if (-d $d) {{ open my $done, ">", "$d/done.$$"; print $done "exited"; }}
' "{stubs}"
"#,
                    stubs = stubs.display()
                ),
            );
        }
        let central = Central::start(&root);
        let env = Self {
            home,
            stubs,
            work,
            central,
            output: Arc::default(),
            terminal: None,
            child: None,
            runtime: None,
            host: None,
            _pty: None,
        };
        env.write_config(
            "credentials.json",
            json!({"servers": {env.central.url.clone(): env.central.cookie.clone()}}),
        );
        env
    }

    fn root(&self) -> PathBuf {
        self.home.path().canonicalize().unwrap()
    }

    /// `dirs::config_dir()` of the isolated user.
    fn config_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.root()
                .join("Library/Application Support/arrakis-conversations")
        } else {
            self.root().join("config/arrakis-conversations")
        }
    }

    fn write_config(&self, name: &str, value: Value) {
        let dir = self.config_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, value.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// The default host runtime directory of this user.
    fn host_dir(&self) -> PathBuf {
        self.runtime
            .clone()
            .unwrap_or_else(|| self.root().join(".conversations-host-local"))
    }

    /// `host --runtime-dir <host_dir> <args>`.
    /// `<group> --runtime-dir <host dir> <args>`, e.g. `host status`.
    fn runtime_cli(&self, group: &str, args: &[&str]) -> (bool, String, String) {
        let dir = self.host_dir().display().to_string();
        let mut full = vec![group, "--runtime-dir", dir.as_str()];
        full.extend_from_slice(args);
        self.cli(&full)
    }

    fn isolate(&self, command: &mut CommandBuilder) {
        let root = self.root();
        command.env_clear();
        command.env("HOME", &root);
        command.env("XDG_CONFIG_HOME", root.join("config"));
        command.env("XDG_DATA_HOME", root.join("data"));
        command.env("PATH", format!("{}:/usr/bin:/bin", self.stubs.display()));
        command.env("TERM", "xterm-256color");
        command.cwd(&self.work);
    }

    /// One non-interactive run of the real binary: `(success, stdout, stderr)`.
    fn cli(&self, args: &[&str]) -> (bool, String, String) {
        let output =
            owned_process::run(&mut self.command(args), b"", Duration::from_secs(60)).unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// The real binary in the isolated user, without a terminal.
    fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.stubs.display()))
            .current_dir(&self.work)
            .args(["--server", &self.central.url])
            .args(args);
        command
    }

    /// Run the real binary under a real 40x120 terminal.
    fn run(&mut self, args: &[&str]) {
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(BIN);
        self.isolate(&mut command);
        command.args(["--server", &self.central.url]);
        command.args(args);
        self.child = Some(pty.slave.spawn_command(command).unwrap());
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().unwrap();
        let output = Arc::clone(&self.output);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                output.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        self.terminal = Some(pty.master.take_writer().unwrap());
        self._pty = Some(pty.master);
    }

    fn type_locally(&mut self, text: &str) {
        let terminal = self.terminal.as_mut().unwrap();
        let _ = terminal
            .write_all(text.as_bytes())
            .and_then(|()| terminal.flush());
    }

    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.stubs.join(name)).unwrap_or_default()
    }

    /// `(pid, wrapper session id or "-", argv)` for every agent started.
    /// An argv with newlines (a handoff prompt) continues on the next lines.
    fn runs(&self) -> Vec<(i32, String, String)> {
        let mut runs: Vec<(i32, String, String)> = Vec::new();
        for line in self.file("runs").lines() {
            let mut parts = line.splitn(3, ' ');
            match (parts.next().unwrap().parse(), parts.next(), runs.last_mut()) {
                (Ok(pid), Some(wrapped), _) if wrapped.len() == 36 || wrapped == "-" => {
                    runs.push((
                        pid,
                        wrapped.to_owned(),
                        parts.next().unwrap_or_default().to_owned(),
                    ))
                }
                (_, _, Some(last)) => {
                    last.2.push('\n');
                    last.2.push_str(line);
                }
                _ => panic!("unexpected run record {line:?}"),
            }
        }
        runs
    }

    fn handoff_result(&self) -> Option<Value> {
        let entries = std::fs::read_dir(&self.stubs).ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("handoff.") {
                let text = std::fs::read_to_string(entry.path()).ok()?;
                if !text.trim().is_empty() {
                    return serde_json::from_str(&text).ok();
                }
            }
        }
        None
    }

    /// The running local host's registered terminals: `(id, agent)`.
    fn host_sessions(&self) -> Vec<(String, String)> {
        let (ok, stdout, _) = self.runtime_cli("host", &["status", "--json"]);
        if !ok {
            return Vec::new();
        }
        let status: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        status["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| {
                let launch = &s["launch"];
                (
                    launch["id"].as_str().unwrap_or_default().to_owned(),
                    launch["agent"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }

    /// The local history of `session` as text.
    fn history(&self, session: &str) -> String {
        let (_, stdout, _) = self.runtime_cli("history", &["get", session, "--json"]);
        stdout
    }

    /// The host's canonical local record for `session`: `(agent, cwd,
    /// native binding, origin)`.
    fn local_record(&self, session: &str) -> Option<(String, String, Option<String>, String)> {
        let db = rusqlite::Connection::open(self.host_dir().join("authz.db")).ok()?;
        db.query_row(
            "SELECT agent, cwd, resume_id, origin FROM sessions WHERE session_id = ?1",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok()
    }

    fn local_record_count(&self) -> i64 {
        rusqlite::Connection::open(self.host_dir().join("authz.db"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap()
    }

    /// Start the user's local host in `host_dir` with the normal backend and
    /// wait until it answers.
    fn start_host(&mut self) {
        let log = std::fs::File::create(self.root().join("host.log")).unwrap();
        let mut command = self.command(&["host", "--runtime-dir"]);
        command
            .arg(self.host_dir())
            .args(["--backend", "normal"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log);
        self.host = Some(owned_process::Owned::spawn(&mut command));
        wait("the user's host", self, || self.host_reachable());
    }

    fn host_reachable(&self) -> bool {
        std::os::unix::net::UnixStream::connect(self.host_dir().join("host.sock")).is_ok()
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Write one Claude transcript under the isolated projects directory.
    fn transcript(&self, session: &str, lines: &[&str]) -> PathBuf {
        let encoded: String = self
            .work
            .to_string_lossy()
            .chars()
            .map(|c| if c == '/' || c == '.' { '-' } else { c })
            .collect();
        let dir = self.root().join(".claude/projects").join(encoded);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session}.jsonl"));
        let mut text = String::new();
        for (n, line) in lines.iter().enumerate() {
            text.push_str(
                &json!({"type": "user", "uuid": format!("{session}-m{n}"), "sessionId": session,
                    "cwd": self.work, "timestamp": format!("2026-11-02T10:00:0{n}.000Z"),
                    "message": {"role": "user", "content": line}})
                .to_string(),
            );
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Env {
    /// An OpenCode (v2 schema) database under the isolated user holding one
    /// session `native` in the working directory with `texts` as user turns.
    fn opencode_db(&self, native: &str, texts: &[&str]) -> PathBuf {
        let path = self.root().join("opencode.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, title TEXT, version TEXT,
               time_created INTEGER, time_updated INTEGER);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
               time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO session_v2 VALUES (?1, ?2, 'neutral title', '2.0', 1762077600000, 1762077600000)",
            rusqlite::params![native, self.work.display().to_string()],
        )
        .unwrap();
        drop(db);
        for text in texts {
            Self::opencode_turn(&path, native, text);
        }
        path
    }

    /// Append one user turn to `native` in the OpenCode database at `path`.
    fn opencode_turn(path: &Path, native: &str, text: &str) {
        let db = rusqlite::Connection::open(path).unwrap();
        let seq: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM session_message WHERE session_id = ?1",
                [native],
                |r| r.get(0),
            )
            .unwrap();
        db.execute(
            "INSERT INTO session_message VALUES (?1, ?2, 'user', ?3, ?4, ?4, ?5)",
            rusqlite::params![
                format!("msg_{native}_{seq}"),
                native,
                seq,
                1762077601000i64 + seq,
                json!({"text": text, "files": []}).to_string()
            ],
        )
        .unwrap();
    }
}

impl Drop for Env {
    /// Stops what this test started without signalling any pid read from a
    /// file: the binary is its own child, the host's whole process group goes
    /// with it, and the agent stubs stop themselves on `stop` (bounded wait
    /// for each recorder's `done.<pid>`).
    fn drop(&mut self) {
        let _ = std::fs::write(self.stubs.join("stop"), b"stop");
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.host = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while self
            .runs()
            .iter()
            .any(|(pid, _, _)| !self.stubs.join(format!("done.{pid}")).exists())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn wait(what: &str, env: &Env, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; terminal:\n{}\nruns: {:?}\ncentral: {:?}\nhost: {:?}\nhost log: {}",
            env.screen(),
            env.runs(),
            env.central.recorded.lines(),
            env.cli(&["host", "status", "--json"]),
            std::fs::read_to_string(env.root().join("host.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

// ------------------------------------------------------------------ tests --

/// A process this test started, stopped when the test ends however it ends.
struct Owned(std::process::Child);

impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn start_agent_registers_with_the_running_host_and_leaves_it_running() {
    let mut env = Env::new();
    // Not the default runtime directory: `start-agent` has to find this host.
    env.runtime = Some(env.root().join("user-host"));
    env.start_host();
    env.run(&["start-agent", "claude"]);
    wait("the agent", &env, || !env.runs().is_empty());
    let (_, wrapped, argv) = env.runs()[0].clone();
    assert!(argv.starts_with("claude --session-id "), "{argv}");
    let session = argv.split_whitespace().nth(2).unwrap().to_owned();
    assert_eq!(wrapped, session, "{argv}");
    wait("the registration with the running host", &env, || {
        env.host_sessions().iter().any(|(id, _)| id == &session)
    });
    wait("the transcript in local history", &env, || {
        env.history(&session).contains(TRANSCRIPT_MARK)
    });
    assert!(
        !env.root().join(".conversations-host-local").exists(),
        "start-agent used a runtime of its own"
    );

    let mut started = env.child.take().unwrap();
    let _ = started.kill();
    let _ = started.wait();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !env.host.as_ref().unwrap().exited(),
        "start-agent stopped the user's host"
    );
    assert!(env.host_reachable());
    assert_eq!(env.central.legacy_channels(), Vec::<String>::new());
    assert_eq!(env.central.private_exposure(), json!({}));
}

#[test]
fn started_agents_are_local_host_registrations_with_local_history_and_handoff() {
    let mut env = Env::new();
    env.start_host();
    env.run(&["start-agent", "claude"]);
    wait("the agent", &env, || !env.runs().is_empty());
    let (pid, wrapped, argv) = env.runs()[0].clone();
    assert!(argv.starts_with("claude --session-id "), "{argv}");
    let session = argv.split_whitespace().nth(2).unwrap().to_owned();
    // The agent runs in a real host wrapper under its own session id …
    assert_eq!(wrapped, session, "the agent is not host-wrapped: {argv}");
    // … registered with this machine's host, with its history kept locally.
    wait("the local host registration", &env, || {
        env.host_sessions()
            .iter()
            .any(|(id, agent)| id == &session && agent == "claude")
    });
    wait("the transcript in local history", &env, || {
        env.history(&session).contains(TRANSCRIPT_MARK)
    });

    // Local keys reach the agent.
    std::thread::sleep(Duration::from_millis(500));
    env.type_locally(&format!("{TYPED_MARK}\r"));
    wait("local keys to reach the agent", &env, || {
        env.file("stdin").contains(TYPED_MARK)
    });

    // The agent's own harness hands off on this machine.
    env.type_locally("please-handoff\r");
    wait("the in-harness handoff result", &env, || {
        env.handoff_result().is_some()
    });
    let handoff = env.handoff_result().unwrap();
    let text = handoff["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    let reply: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    assert_eq!(reply["status"], "started", "{handoff}");
    let child = reply["session_id"].as_str().unwrap().to_owned();
    wait("the handed-off agent", &env, || {
        env.runs().iter().any(|(_, wrapped, _)| wrapped == &child)
    });
    let handed = env
        .runs()
        .into_iter()
        .find(|(_, wrapped, _)| wrapped == &child)
        .unwrap();
    assert!(handed.2.contains(BRIEF_MARK), "{handed:?}");
    assert!(alive(pid));

    assert_eq!(env.central.legacy_channels(), Vec::<String>::new());
    assert_eq!(env.central.private_exposure(), json!({}));

    // Leaving the agent's terminal leaves the user's host running.
    let mut started = env.child.take().unwrap();
    let _ = started.kill();
    let _ = started.wait();
    std::thread::sleep(Duration::from_secs(2));
    assert!(env.host_reachable(), "start-agent stopped the user's host");
}

#[test]
fn start_agent_resume_reuses_the_native_session_record_and_registers_locally() {
    let mut env = Env::new();
    let session = "6c0f3f9e-3d0b-4a57-9f41-2d6c3a0e5b21";
    env.transcript(session, &["neutral opener", TRANSCRIPT_MARK]);
    env.start_host();
    env.run(&["start-agent", "claude", "--resume", session]);
    wait("the resumed agent", &env, || !env.runs().is_empty());
    let (_, wrapped, argv) = env.runs()[0].clone();
    let args: Vec<_> = argv.split_whitespace().collect();
    assert_eq!(args.len(), 5, "{argv}");
    assert_eq!(&args[..4], &["claude", "--resume", session, "--settings"]);
    let settings: Value = serde_json::from_slice(&std::fs::read(args[4]).unwrap()).unwrap();
    assert!(
        settings["hooks"]["UserPromptSubmit"].is_array(),
        "{settings}"
    );
    // A Claude session id is its own canonical id.
    assert_eq!(wrapped, session);
    wait("the local host registration", &env, || {
        env.host_sessions().iter().any(|(id, _)| id == session)
    });
    wait("the resumed transcript in local history", &env, || {
        env.history(session).contains(TRANSCRIPT_MARK)
    });
    let record = env.local_record(session).unwrap();
    assert_eq!(
        (&record.0, &record.1, &record.2),
        (
            &"claude".to_owned(),
            &env.work.display().to_string(),
            &Some(session.to_owned())
        )
    );
    assert_eq!(env.local_record_count(), 1);
    std::thread::sleep(Duration::from_millis(500));
    env.type_locally(&format!("{TYPED_MARK}\r"));
    wait("local keys to reach the resumed agent", &env, || {
        env.file("stdin").contains(TYPED_MARK)
    });
    assert_eq!(env.central.legacy_channels(), Vec::<String>::new());
    assert_eq!(env.central.private_exposure(), json!({}));
}

/// An OpenCode `ses_…` id is resumed under the canonical record `upload`
/// already made for it, with its local history; one bound to another native
/// session is refused before anything starts.
#[test]
fn start_agent_resumes_opencode_under_its_canonical_record_and_refuses_a_conflict() {
    let mut env = Env::new();
    env.write_config(
        "local-host.json",
        json!({"publication": {"account": ALICE}}),
    );
    let (native, conflict) = ("ses_4b8e1d7aResumeMe", "ses_9c2f5a3bConflicting");
    let db = env.opencode_db(native, &["neutral opener", TRANSCRIPT_MARK]);
    {
        let opencode = rusqlite::Connection::open(&db).unwrap();
        opencode
            .execute(
                "INSERT INTO session_v2 VALUES (?1, ?2, 'neutral title', '2.0', 1762077600000, 1762077600000)",
                rusqlite::params![conflict, env.work.display().to_string()],
            )
            .unwrap();
    }
    Env::opencode_turn(&db, conflict, "conflict turn");
    let db_arg = db.display().to_string();
    let adopt = |env: &Env, native: &str| {
        let (ok, stdout, stderr) = env.cli(&[
            "--opencode-db",
            &db_arg,
            "upload",
            "--agent",
            "opencode",
            native,
        ]);
        assert!(ok, "{stdout}\n{stderr}");
        canonical_upload_id(&stdout, native)
    };
    let canonical = adopt(&env, native);
    let conflict_id = adopt(&env, conflict);
    rusqlite::Connection::open(env.host_dir().join("authz.db"))
        .unwrap()
        .execute(
            "UPDATE sessions SET resume_id = 'ses_someoneElse' WHERE session_id = ?1",
            [&conflict_id],
        )
        .unwrap();
    env.start_host();

    // The conflicting binding is refused before anything starts.
    let (ok, stdout, stderr) = env.cli(&["start-agent", "opencode", "--resume", conflict]);
    assert!(!ok, "a conflicting record was retargeted: {stdout}");
    assert!(
        stderr.contains("already bound to another native session"),
        "{stdout}\n{stderr}"
    );
    assert_eq!(
        env.local_record(&conflict_id).map(|r| r.2),
        Some(Some("ses_someoneElse".to_owned()))
    );

    env.run(&["start-agent", "opencode", "--resume", native]);
    wait("the resumed agent", &env, || !env.runs().is_empty());
    let (_, wrapped, argv) = env.runs()[0].clone();
    assert_eq!(wrapped, canonical, "{argv}");
    assert!(argv.starts_with("opencode "), "{argv}");
    assert!(argv.contains(&format!("--session {native}")), "{argv}");
    wait("the local host registration", &env, || {
        env.host_sessions()
            .iter()
            .any(|(id, agent)| id == &canonical && agent == "opencode")
    });
    assert!(env.history(&canonical).contains(TRANSCRIPT_MARK));
    assert_eq!(
        env.local_record(&canonical).map(|r| r.2),
        Some(Some(native.to_owned()))
    );
    assert_eq!(env.local_record_count(), 2);
    assert_eq!(env.runs().len(), 1, "{:?}", env.runs());
    assert_eq!(env.central.legacy_channels(), Vec::<String>::new());
}

#[test]
fn upload_publishes_through_the_signed_lifecycle_and_refuses_legacy_collisions() {
    let env = Env::new();
    env.write_config(
        "local-host.json",
        json!({"publication": {"account": ALICE}}),
    );
    let fresh = "7a1c2e3d-4b5f-4a6b-8c7d-9e0f1a2b3c4d";
    env.transcript(fresh, &["neutral opener", "published turn"]);
    let (ok, stdout, stderr) = env.cli(&["upload", fresh]);
    assert!(ok, "{stdout}\n{stderr}");
    let (status, body) = env
        .central
        .web("GET", &format!("/api/sessions/{fresh}"), Value::Null);
    assert_eq!(status, 200, "{body}");
    assert!(body.to_string().contains("published turn"), "{body}");
    assert!(env
        .central
        .recorded
        .lines()
        .iter()
        .any(|line| line.starts_with("POST /api/publications")));
    assert!(
        env.history(fresh).contains("published turn"),
        "the published session is not in local history"
    );

    // A session id that already has legacy central data is refused, and the
    // legacy row is left exactly as it was.
    let legacy = "5d4c3b2a-1f0e-4d9c-8b7a-6f5e4d3c2b1a";
    let (status, body) = env.central.web(
        "POST",
        "/api/sessions",
        json!({"uploaded_by": ALICE, "source_host": null, "session": {
            "id": legacy, "agent": "claude", "title": "legacy row",
            "project_path": null, "cwd": null, "git_branch": null, "claude_version": null,
            "started_at": null, "ended_at": null, "messages": [], "skipped_lines": 0}}),
    );
    assert!(status == 200 || status == 201, "{status} {body}");
    let (_, before) = env
        .central
        .web("GET", &format!("/api/sessions/{legacy}"), Value::Null);
    env.transcript(legacy, &["neutral opener", TRANSCRIPT_MARK]);
    let (ok, stdout, stderr) = env.cli(&["upload", legacy]);
    assert!(
        !ok,
        "a legacy collision was not refused:\n{stdout}\n{stderr}"
    );
    assert!(
        format!("{stdout}{stderr}").contains("already has central data"),
        "{stdout}\n{stderr}"
    );
    let (_, after) = env
        .central
        .web("GET", &format!("/api/sessions/{legacy}"), Value::Null);
    assert_eq!(before, after);
    // Only this test's own legacy row used the legacy upload endpoint.
    let legacy_uploads = env
        .central
        .recorded
        .lines()
        .into_iter()
        .filter(|line| line == "POST /api/sessions")
        .count();
    assert_eq!(legacy_uploads, 1, "{:?}", env.central.recorded.lines());
    // The refused session's content never left this machine.
    assert_eq!(
        env.central.recorded.containing(TRANSCRIPT_MARK),
        Vec::<String>::new()
    );
    assert!(!exposes(&env.central.stored(), TRANSCRIPT_MARK));
}

/// `upload` of an OpenCode session: its native id (`ses_…`) maps to ONE
/// stable canonical local session id, the same for every run, its history is
/// kept locally under that id, and it is published under that id only.
#[test]
fn opencode_sessions_publish_under_one_stable_canonical_id() {
    let env = Env::new();
    env.write_config(
        "local-host.json",
        json!({"publication": {"account": ALICE}}),
    );
    let native = "ses_2f9a6c1d8e7bLocalOnly";
    let db = env.opencode_db(native, &["neutral opener", TRANSCRIPT_MARK]);
    let db_arg = db.display().to_string();
    let upload = || {
        env.cli(&[
            "--opencode-db",
            &db_arg,
            "upload",
            "--agent",
            "opencode",
            native,
        ])
    };

    let (ok, first, stderr) = upload();
    assert!(ok, "{first}\n{stderr}");
    let local = canonical_upload_id(&first, native);
    assert!(
        uuid::Uuid::parse_str(&local).is_ok() && local != native,
        "not a canonical local id: {local}"
    );
    assert!(env.history(&local).contains(TRANSCRIPT_MARK), "{first}");
    // The selection created the canonical local record the signed read path
    // lists and reopens: metadata only, bound to the native session.
    let record = env.local_record(&local);
    assert_eq!(
        record,
        Some((
            "opencode".to_owned(),
            env.work.display().to_string(),
            Some(native.to_owned()),
            "adopted".to_owned()
        ))
    );
    assert!(env.runs().is_empty(), "no agent is launched");

    // A later run publishes the same canonical session, including turns
    // written after the first run.
    Env::opencode_turn(&db, native, "appended later turn");
    let (ok, second, stderr) = upload();
    assert!(ok, "{second}\n{stderr}");
    assert_eq!(canonical_upload_id(&second, native), local);
    let (status, body) = env
        .central
        .web("GET", &format!("/api/sessions/{local}"), Value::Null);
    assert_eq!(status, 200, "{body}");
    assert!(body.to_string().contains("appended later turn"), "{body}");
    let (status, _) = env
        .central
        .web("GET", &format!("/api/sessions/{native}"), Value::Null);
    assert_ne!(status, 200, "published under the native id");
    // Every run reused the one record and stored each turn once.
    assert_eq!(env.local_record(&local), record);
    assert_eq!(env.local_record_count(), 1);
    let history = env.history(&local);
    assert_eq!(history.lines().count(), 3, "{history}");
}

/// The canonical local id `upload` reports for a native session it published
/// under another id (`<native> as <id>`).
fn canonical_upload_id(stdout: &str, native: &str) -> String {
    let marker = format!("{native} as ");
    let at = stdout
        .find(&marker)
        .unwrap_or_else(|| panic!("no canonical id in: {stdout}"));
    stdout[at + marker.len()..]
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

/// A transcript of `count` user turns, so it takes several publication chunks.
fn long_transcript(env: &Env, session: &str, count: usize) {
    let turns: Vec<String> = (0..count).map(|n| format!("turn number {n}")).collect();
    let turns: Vec<&str> = turns.iter().map(String::as_str).collect();
    env.transcript(session, &turns);
}

/// Central's copy of `session`: its message ids, in order.
fn published_ids(env: &Env, session: &str) -> Vec<String> {
    let (status, body) = env
        .central
        .web("GET", &format!("/api/sessions/{session}"), Value::Null);
    assert_eq!(status, 200, "{body}");
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["uuid"].as_str().unwrap().to_owned())
        .collect()
}

/// The server applies an append but its answer is lost: `upload` resends the
/// IDENTICAL signed request, the server answers it idempotently, and central
/// holds every message exactly once.
#[test]
fn upload_resends_the_identical_request_after_a_lost_acknowledgement() {
    let env = Env::new();
    env.write_config(
        "local-host.json",
        json!({"publication": {"account": ALICE}}),
    );
    let session = "2e8d4c6a-0b1f-4e3d-9c5a-7f6e5d4c3b2a";
    long_transcript(&env, session, 1200);
    env.central.faults.lock().unwrap().lose_append_acks = 2;

    let (ok, stdout, stderr) = env.cli(&["upload", session]);
    assert!(ok, "{stdout}\n{stderr}");
    let appends: Vec<Vec<u8>> = env
        .central
        .recorded
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(line, body)| {
            line == "POST /api/publications"
                && serde_json::from_slice::<Value>(body).is_ok_and(|b| b["chunk"].is_string())
        })
        .map(|(_, body)| body.clone())
        .collect();
    // Three chunks. The first chunk's answer was lost twice, so it was sent
    // three times, byte for byte; the others once each.
    assert_eq!(appends.len(), 5, "{}", appends.len());
    assert_eq!(appends[0], appends[1]);
    assert_eq!(appends[1], appends[2]);
    assert_ne!(appends[2], appends[3]);
    assert_ne!(appends[3], appends[4]);
    let ids = published_ids(&env, session);
    let expected: Vec<String> = (0..1200).map(|n| format!("{session}-m{n}")).collect();
    assert_eq!(ids, expected);
    let (_, status, _) = env.runtime_cli("history", &["status", session]);
    assert!(status.contains("shared"), "{status}");
}

/// `upload` while the user's host is running: both would publish the same
/// session. Publication is serialized per runtime directory, so the server
/// never handles two of its requests at once, and the result is one complete,
/// exactly-once copy with no error.
#[test]
fn upload_and_the_running_host_publish_one_request_at_a_time() {
    let mut env = Env::new();
    env.write_config(
        "local-host.json",
        json!({"publication": {"account": ALICE}}),
    );
    let runtime = env.root().join("user-host");
    env.runtime = Some(runtime.clone());
    let _host = Owned(
        env.command(&["host", "--runtime-dir"])
            .arg(&runtime)
            .args(["--backend", "normal"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait("the user's host", &env, || env.host_reachable());
    env.central.faults.lock().unwrap().delay = Duration::from_millis(400);

    let session = "6b5a4c3d-2e1f-4a0b-8c9d-0e1f2a3b4c5d";
    long_transcript(&env, session, 2400);
    let (ok, stdout, stderr) = env.cli(&["upload", session]);
    assert!(ok, "{stdout}\n{stderr}");
    let expected: Vec<String> = (0..2400).map(|n| format!("{session}-m{n}")).collect();
    assert_eq!(published_ids(&env, session), expected);
    // Let the host's loop run over the finished publication too.
    std::thread::sleep(Duration::from_secs(3));
    let (_, status, _) = env.runtime_cli("history", &["status", session]);
    assert!(
        status.contains("shared") && !status.contains("error"),
        "{status}"
    );
    assert_eq!(
        env.central.faults.lock().unwrap().max_in_flight,
        1,
        "two publication requests were in flight at once"
    );
}

/// Another process holds the runtime directory's publication: `upload`
/// returns within its bound without sending anything or waiting on the lock.
/// The holder then crashes (SIGKILL releases the lock), and the next `upload`
/// resends the request frozen before, byte for byte, with every message
/// published exactly once.
#[test]
fn upload_returns_while_another_process_publishes_then_resends_the_frozen_request() {
    let env = Env::new();
    env.write_config(
        "local-host.json",
        json!({"publication": {"account": ALICE}}),
    );
    let session = "4c7a2e9d-1b3f-4a5e-8d6c-9e0f1a2b3c4d";
    long_transcript(&env, session, 1200);
    let appends = || -> Vec<Vec<u8>> {
        env.central
            .recorded
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, body)| {
                line == "POST /api/publications"
                    && serde_json::from_slice::<Value>(body).is_ok_and(|b| b["chunk"].is_string())
            })
            .map(|(_, body)| body.clone())
            .collect()
    };
    let publications = || {
        env.central
            .recorded
            .lines()
            .iter()
            .filter(|line| line.as_str() == "POST /api/publications")
            .count()
    };

    // Every answer is lost: the first chunk is applied, and stays frozen.
    env.central.faults.lock().unwrap().lose_append_acks = usize::MAX;
    let (ok, stdout, stderr) = env.cli(&["upload", session]);
    assert!(!ok, "{stdout}");
    assert!(stderr.contains("run `upload` again"), "{stderr}");
    // (The round that also sent the share is not an unanswered one.)
    let lost = appends();
    assert!((3..=4).contains(&lost.len()), "{}", lost.len());
    assert!(lost.iter().all(|a| a == &lost[0]));
    env.central.faults.lock().unwrap().lose_append_acks = 0;

    // A second process holds the lock.
    let lock = env.host_dir().join("publication.lock");
    let mut holder = Owned(
        Command::new("/usr/bin/perl")
            .args([
                "-e",
                "use Fcntl qw(:flock); open(my $f, '>>', $ARGV[0]) or die; \
                 flock($f, LOCK_EX) or die; $| = 1; print \"held\\n\"; sleep 600;",
            ])
            .arg(&lock)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(holder.0.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    assert_eq!(line.trim(), "held");
    let before = publications();
    let started = Instant::now();
    let (ok, stdout, stderr) = env.cli(&["upload", session]);
    let took = started.elapsed();
    assert!(!ok, "{stdout}");
    assert!(
        stderr.contains("another publisher") && stderr.contains("run `upload` again"),
        "{stderr}"
    );
    assert!(took < Duration::from_secs(30), "{took:?}");
    assert_eq!(
        publications(),
        before,
        "nothing is sent while the lock is held"
    );

    // The holder crashes; its lock goes with it.
    holder.0.kill().unwrap();
    holder.0.wait().unwrap();
    let (ok, stdout, stderr) = env.cli(&["upload", session]);
    assert!(ok, "{stdout}\n{stderr}");
    let all = appends();
    let n = lost.len();
    assert_eq!(all.len(), n + 3, "{}", all.len());
    assert_eq!(all[n], lost[0], "the frozen request is resent unchanged");
    assert_ne!(all[n], all[n + 1]);
    assert_ne!(all[n + 1], all[n + 2]);
    let expected: Vec<String> = (0..1200).map(|n| format!("{session}-m{n}")).collect();
    assert_eq!(published_ids(&env, session), expected);
}

/// Canonical OpenCode ids through the real CLI when local state already
/// exists: history stored under a session id without a record is kept and
/// gets its record; history an older build keyed by the raw native id is left
/// alone and not used as an id; a record with that id bound to another native
/// session is refused, never retargeted, and nothing is published.
#[test]
fn opencode_canonical_ids_reuse_existing_history_and_refuse_a_conflicting_record() {
    let env = Env::new();
    env.write_config(
        "local-host.json",
        json!({"publication": {"account": ALICE}}),
    );
    let (kept, raw, conflict) = (
        "ses_kept1History",
        "ses_raw2NativeKeyed",
        "ses_cnf3Conflict",
    );
    let db = env.opencode_db(kept, &["kept history turn"]);
    {
        let opencode = rusqlite::Connection::open(&db).unwrap();
        for native in [raw, conflict] {
            opencode
                .execute(
                    "INSERT INTO session_v2 VALUES (?1, ?2, 'neutral title', '2.0', 1762077600000, 1762077600000)",
                    rusqlite::params![native, env.work.display().to_string()],
                )
                .unwrap();
        }
    }
    Env::opencode_turn(&db, raw, "raw keyed turn");
    Env::opencode_turn(&db, conflict, "conflict turn");
    let db_arg = db.display().to_string();
    let upload = |native: &str| {
        env.cli(&[
            "--opencode-db",
            &db_arg,
            "upload",
            "--agent",
            "opencode",
            native,
        ])
    };
    let history_db = || rusqlite::Connection::open(env.host_dir().join("history.db")).unwrap();
    let authz_db = || rusqlite::Connection::open(env.host_dir().join("authz.db")).unwrap();
    let rows_under = |session: &str| -> i64 {
        history_db()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                [session],
                |r| r.get(0),
            )
            .unwrap()
    };
    let rekey = |from: &str, to: &str| {
        let history = history_db();
        for table in ["messages", "messages_fts"] {
            history
                .execute(
                    &format!("UPDATE {table} SET session_id = ?2 WHERE session_id = ?1"),
                    [from, to],
                )
                .unwrap();
        }
        authz_db()
            .execute("DELETE FROM sessions WHERE session_id = ?1", [from])
            .unwrap();
    };

    // History already under a session id, with no record: that id, recorded.
    let (ok, stdout, stderr) = upload(kept);
    assert!(ok, "{stdout}\n{stderr}");
    let derived = canonical_upload_id(&stdout, kept);
    let existing = "6d5c4b3a-2f1e-4d0c-9b8a-7f6e5d4c3b2a";
    rekey(&derived, existing);
    let (ok, stdout, stderr) = upload(kept);
    assert!(ok, "{stdout}\n{stderr}");
    assert_eq!(canonical_upload_id(&stdout, kept), existing);
    assert_eq!(
        env.local_record(existing),
        Some((
            "opencode".to_owned(),
            env.work.display().to_string(),
            Some(kept.to_owned()),
            "adopted".to_owned()
        ))
    );
    assert_eq!(env.local_record(&derived), None);
    assert_eq!(rows_under(existing), 1);

    // History an older build keyed by the raw native id: not an id; left as is.
    let (ok, stdout, stderr) = upload(raw);
    assert!(ok, "{stdout}\n{stderr}");
    let raw_id = canonical_upload_id(&stdout, raw);
    rekey(&raw_id, raw);
    let (ok, stdout, stderr) = upload(raw);
    assert!(ok, "{stdout}\n{stderr}");
    assert_eq!(canonical_upload_id(&stdout, raw), raw_id);
    assert_eq!(
        env.local_record(&raw_id).map(|r| r.2),
        Some(Some(raw.to_owned()))
    );
    assert_eq!(rows_under(raw), 1, "older rows are not deleted");
    assert_eq!(rows_under(&raw_id), 1);

    // The canonical record now belongs to another native session: refused.
    let (ok, stdout, stderr) = upload(conflict);
    assert!(ok, "{stdout}\n{stderr}");
    let conflict_id = canonical_upload_id(&stdout, conflict);
    authz_db()
        .execute(
            "UPDATE sessions SET resume_id = 'ses_someoneElse' WHERE session_id = ?1",
            [&conflict_id],
        )
        .unwrap();
    let publications_before = env.central.recorded.lines().len();
    let (ok, stdout, stderr) = upload(conflict);
    assert!(!ok, "a conflicting record was retargeted: {stdout}");
    assert!(
        format!("{stdout}{stderr}").contains("already bound to another native session"),
        "{stdout}\n{stderr}"
    );
    assert_eq!(
        env.local_record(&conflict_id).map(|r| r.2),
        Some(Some("ses_someoneElse".to_owned()))
    );
    assert_eq!(rows_under(&conflict_id), 1);
    assert!(
        env.central.recorded.lines()[publications_before..]
            .iter()
            .all(|line| line != "POST /api/publications"),
        "a refused session is not published"
    );
}
