#![cfg(unix)]
//! The real `conversations start-agent` under a real terminal, with a
//! real local host, against the REAL central server router. A logged-in web
//! viewer drives every old unsigned channel the server still exposes — reply
//! text, raw web keystrokes, resize, repaint, end, start-new, re-open, handoff
//! and account agent settings — and the recording stub agent proves none of
//! them reaches the local process, its terminal or its argv, while the same
//! session keeps taking local keyboard input. A started agent is a local host
//! registration, so central never even lists it.

#[path = "support/owned_process.rs"]
mod owned_process;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, Engine};
use conversations_server::{build_router_with_auth, AppState, Config, SqliteRepo};
use futures::SinkExt;
use portable_pty::{CommandBuilder, PtySize};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_conversations");
const ALICE: &str = "alice@example.com";

/// The real server router on loopback, with auth on so the per-user control
/// channel exists, and Alice's login cookie.
struct Central {
    runtime: tokio::runtime::Runtime,
    url: String,
    cookie: String,
    http: reqwest::blocking::Client,
}

impl Central {
    fn start(dir: &Path) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let database = format!("sqlite://{}?mode=rwc", dir.join("central.sqlite").display());
        let url = runtime.block_on(async {
            let repo = SqliteRepo::connect(&database).await.unwrap();
            let config = Config {
                auth_enabled: true,
                password_auth_enabled: true,
                cookie_secure: false,
                ..Config::default()
            };
            let app = build_router_with_auth(AppState::new(Arc::new(repo), Arc::new(config)))
                .await
                .unwrap();
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
        Self {
            runtime,
            url,
            cookie,
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

    /// Alice's live sessions that are still running.
    fn live_ids(&self) -> Vec<String> {
        let (_, body) = self.web("GET", "/api/live", Value::Null);
        let mut ids = Vec::new();
        collect_live(&body, &mut ids);
        ids
    }

    /// Type into the live terminal from a browser, over the web keyboard socket.
    fn web_keystrokes(&self, live_id: &str, text: &str) {
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
        let url = format!(
            "{}/api/live/{live_id}/pty/ws",
            self.url.replacen("http", "ws", 1)
        );
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("cookie", self.cookie.parse().unwrap());
        self.runtime.block_on(async {
            // A refused upgrade is an outcome too: nothing to type into.
            let Ok((mut ws, _)) = tokio_tungstenite::connect_async(request).await else {
                return;
            };
            let frame = json!({"type": "input", "b64": STANDARD.encode(text)});
            ws.send(Message::Text(frame.to_string())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = ws.close(None).await;
        });
    }
}

fn collect_live(value: &Value, ids: &mut Vec<String>) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| collect_live(item, ids)),
        Value::Object(map) => {
            if let (Some(Value::String(id)), Some(Value::String(status))) =
                (map.get("id"), map.get("status"))
            {
                if status == "live" {
                    ids.push(id.clone());
                }
            }
            map.values().for_each(|item| collect_live(item, ids));
        }
        _ => {}
    }
}

/// An isolated user: HOME, config and data directories, a PATH of recording
/// stubs plus the system directories, and nothing inherited from the caller's
/// agent or cmux environment.
struct Env {
    home: tempfile::TempDir,
    stubs: PathBuf,
    work: PathBuf,
    central: Central,
    host: Option<owned_process::Owned>,
    output: Arc<Mutex<Vec<u8>>>,
    terminal: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    _pty: Option<Box<dyn portable_pty::MasterPty + Send>>,
}

impl Env {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().canonicalize().unwrap();
        let (stubs, work) = (root.join("stubs"), root.join("work"));
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
            // Each stub records its pid and argv, samples its terminal size
            // while it lives, then appends the raw bytes its terminal delivers.
            // Both halves stop on their own once the test writes `stop` (or its
            // directory is gone), and the recorder reports `done.<pid>`: the
            // test never signals a pid read back from a log. The host wrapper's
            // Codex probes are answered without being recorded as agents: no
            // personal MCP registration, and a private app-server that only
            // listens on the requested socket until `stop` (bound relative to
            // its directory: the isolated HOME is longer than `sun_path`).
            write_executable(
                &stubs.join(agent),
                &format!(
                    r#"#!/bin/sh
case "$1 $2" in
"mcp get") exit 1 ;;
"app-server --listen")
    echo "$$" >> "{stubs}/servers"
    exec /usr/bin/perl -MIO::Socket::UNIX -e 'my ($p, $d) = @ARGV; $p =~ s{{^unix://}}{{}}; my ($dir, $name) = $p =~ m{{^(.*)/([^/]+)$}}; chdir $dir or die $!; my $l = IO::Socket::UNIX->new(Local => $name, Listen => 8) or die $!; $l->blocking(0); my @c; while (-d $d && !-e "$d/stop") {{ my $c = $l->accept; push @c, $c if $c; select(undef, undef, undef, 0.1); }} unlink $name; if (-d $d) {{ open my $done, ">", "$d/done.$$"; print $done "exited"; }}' "$3" "{stubs}" ;;
esac
echo "$$ {agent} $*" >> "{stubs}/runs"
( while [ -d "{stubs}" ] && [ ! -e "{stubs}/stop" ] && /bin/kill -0 $$ 2>/dev/null; do /bin/stty size </dev/tty >> "{stubs}/sizes" 2>/dev/null; /bin/sleep 0.2; done ) &
echo "$!" >> "{stubs}/samplers"
exec /usr/bin/perl -MIO::Select -MIO::Handle -e 'my $d=$ARGV[0]; my $s=IO::Select->new(\*STDIN); open my $f, ">>", "$d/stdin" or die $!; $f->autoflush(1); while (-d $d && !-e "$d/stop") {{ if ($s->can_read(0.1)) {{ my $n=sysread(STDIN,my $b,4096); last unless $n; print $f $b; }} }} if (-d $d) {{ open my $done, ">", "$d/done.$$"; print $done "exited"; }}' "{stubs}"
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
            host: None,
            output: Arc::default(),
            terminal: None,
            child: None,
            _pty: None,
        };
        env.write_config(
            "credentials.json",
            json!({"servers": {env.central.url.clone(): env.central.cookie.clone()}}),
        );
        env.write_settings(json!({}));
        env
    }

    /// The local host settings, without the web UI: parallel tests would
    /// contend for its port.
    fn write_settings(&self, mut settings: Value) {
        settings["web"] = json!({"enabled": false});
        self.write_config("local-host.json", settings);
    }

    /// Start the user's local host in the default runtime directory and wait
    /// until it answers.
    fn start_host(&mut self) {
        let root = self.home.path().canonicalize().unwrap();
        let log = std::fs::File::create(root.join("host.log")).unwrap();
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.stubs.display()))
            .current_dir(&self.work)
            .args(["--server", &self.central.url, "host"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        self.host = Some(owned_process::Owned::spawn(&mut command));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut status = Command::new(BIN);
            status
                .env_clear()
                .env("HOME", &root)
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_DATA_HOME", root.join("data"))
                .current_dir(&self.work)
                .args(["--server", &self.central.url, "host", "status"]);
            if owned_process::run(&mut status, b"", Duration::from_secs(10))
                .is_ok_and(|output| output.status.success())
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the host did not start: {}",
                std::fs::read_to_string(root.join("host.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `dirs::config_dir()` of the isolated user.
    fn config_dir(&self) -> PathBuf {
        let root = self.home.path().canonicalize().unwrap();
        if cfg!(target_os = "macos") {
            root.join("Library/Application Support/arrakis-conversations")
        } else {
            root.join("config/arrakis-conversations")
        }
    }

    fn write_config(&self, name: &str, value: Value) {
        let dir = self.config_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, value.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
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
        let root = self.home.path().canonicalize().unwrap();
        let mut command = CommandBuilder::new(BIN);
        command.env_clear();
        command.env("HOME", &root);
        command.env("XDG_CONFIG_HOME", root.join("config"));
        command.env("XDG_DATA_HOME", root.join("data"));
        command.env("PATH", format!("{}:/usr/bin:/bin", self.stubs.display()));
        command.env("TERM", "xterm-256color");
        command.cwd(&self.work);
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

    /// Keystrokes typed at the local terminal. A terminal whose program has
    /// exited refuses them; the agent's stdin record shows the outcome.
    fn type_locally(&mut self, text: &str) {
        let terminal = self.terminal.as_mut().unwrap();
        let _ = terminal
            .write_all(text.as_bytes())
            .and_then(|()| terminal.flush());
    }

    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.stubs.join(name)).unwrap_or_default()
    }

    /// `(pid, argv)` for every agent the stubs saw start, without the host
    /// wrapper's `--remote unix://<private app-server>` Codex prefix.
    fn runs(&self) -> Vec<(i32, String)> {
        self.file("runs")
            .lines()
            .map(|line| {
                let (pid, argv) = line.split_once(' ').unwrap();
                let argv = match argv.strip_prefix("codex --remote unix://") {
                    Some(rest) => format!("codex {}", rest.split_once(' ').map_or("", |r| r.1)),
                    None => argv.to_owned(),
                };
                (pid.parse().unwrap(), argv)
            })
            .collect()
    }

    /// Private Codex app-servers the host wrapper started.
    fn servers(&self) -> Vec<i32> {
        self.file("servers")
            .lines()
            .map(|pid| pid.parse().unwrap())
            .collect()
    }

    /// Every process this test started in the terminal: the binary, each
    /// agent stub (later its recorder) and each stub's terminal-size sampler.
    fn owned_pids(&self) -> Vec<i32> {
        let mut pids: Vec<i32> = self.runs().into_iter().map(|(pid, _)| pid).collect();
        pids.extend(self.servers());
        pids.extend(
            self.file("samplers")
                .lines()
                .map(|pid| pid.parse::<i32>().unwrap()),
        );
        pids.extend(
            self.child
                .as_ref()
                .and_then(|child| child.process_id())
                .map(|pid| pid as i32),
        );
        pids
    }

    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
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
            .map(|(pid, _)| *pid)
            .chain(self.servers())
            .any(|pid| !self.stubs.join(format!("done.{pid}")).exists())
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
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; terminal:\n{}",
            env.screen()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A test that fails (unwinds) with a live agent still running leaves none of
/// the processes it started behind, without signalling anything it did not
/// spawn itself.
#[test]
fn an_unwinding_test_leaves_none_of_its_processes_running() {
    let owned = Arc::new(Mutex::new(Vec::new()));
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut env = Env::new();
        env.start_host();
        env.run(&["start-agent", "claude"]);
        wait("the local agent and its sampler", &env, || {
            !env.runs().is_empty() && !env.file("samplers").is_empty()
        });
        let pids = env.owned_pids();
        assert!(pids.iter().all(|pid| alive(*pid)), "{pids:?}");
        *owned.lock().unwrap() = pids;
        panic!("intentional failure with a live agent");
    }));
    assert!(unwound.is_err());
    let owned = owned.lock().unwrap().clone();
    assert!(owned.len() >= 3, "binary, agent and sampler: {owned:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while owned.iter().any(|pid| alive(*pid)) {
        assert!(
            Instant::now() < deadline,
            "still running after the unwind: {:?}",
            owned.iter().filter(|pid| alive(**pid)).collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn web_channels_cannot_reach_a_started_agent_but_local_keys_still_do() {
    let mut env = Env::new();
    env.start_host();
    env.run(&["start-agent", "claude"]);
    wait("the local agent", &env, || !env.runs().is_empty());
    let (pid, argv) = env.runs()[0].clone();
    let live = argv
        .strip_prefix("claude --session-id ")
        .unwrap_or_else(|| panic!("{argv}"))
        .split_whitespace()
        .next()
        .expect("Claude session id")
        .to_owned();
    uuid::Uuid::parse_str(&live).expect("the recorded launch must carry a valid session id");
    // Give any per-session stream time to attach before the web acts.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(env.central.live_ids(), Vec::<String>::new());

    let mut web = Vec::new();
    for (method, path, body) in [
        (
            "POST",
            format!("/api/live/{live}/replies"),
            json!({"text": "remote-reply-needle"}),
        ),
        (
            "POST",
            format!("/api/live/{live}/resize"),
            json!({"cols": 77, "rows": 33}),
        ),
        ("POST", format!("/api/live/{live}/repaint"), Value::Null),
        (
            "POST",
            "/api/live/start-new".into(),
            json!({"cwd": env.work, "agent": "claude"}),
        ),
        ("POST", format!("/api/sessions/{live}/reopen"), json!({})),
        (
            "POST",
            format!("/api/agent-messaging/sessions/{live}/handoffs"),
            json!({"brief": "remote-handoff-needle", "idempotency_key": "h1"}),
        ),
    ] {
        web.push((path.clone(), env.central.web(method, &path, body)));
    }
    env.central.web_keystrokes(&live, "web-keyboard-needle\r");
    env.type_locally("local-keyboard-before-end\r");
    web.push((
        "complete".into(),
        env.central
            .web("POST", &format!("/api/live/{live}/complete"), Value::Null),
    ));
    // Every effect the old channels produced landed within a second; give
    // them three, then type locally again into the same session.
    std::thread::sleep(Duration::from_secs(3));
    env.type_locally("local-keyboard-after-end\r");
    std::thread::sleep(Duration::from_secs(1));

    let stdin = env.file("stdin");
    let observed = json!({
        "remote reply typed": stdin.contains("remote-reply-needle"),
        "remote keystrokes typed": stdin.contains("web-keyboard-needle"),
        "remote resize applied": env.file("sizes").lines().any(|l| l.trim() == "33 77"),
        "remote end killed the agent": !alive(pid),
        "remote start/re-open/handoff launched agents": env.runs().len() - 1,
        "local keys before": stdin.contains("local-keyboard-before-end"),
        "local keys after": stdin.contains("local-keyboard-after-end"),
    });
    assert_eq!(
        observed,
        json!({
            "remote reply typed": false,
            "remote keystrokes typed": false,
            "remote resize applied": false,
            "remote end killed the agent": false,
            "remote start/re-open/handoff launched agents": 0,
            "local keys before": true,
            "local keys after": true,
        }),
        "web responses: {web:?}\nagent stdin: {stdin:?}\nruns: {:?}",
        env.runs()
    );
    assert!(alive(pid));
}

/// Alice's central account asks for an unsandboxed Codex that never asks.
fn central_asks_for_unsandboxed_codex(env: &Env) {
    let (status, body) = env.central.web(
        "PUT",
        "/api/user/settings",
        json!({"codex_sandbox_mode": "danger-full-access", "codex_approval_policy": "never"}),
    );
    assert_eq!(status, 200, "{body}");
}

/// Resume one native Codex thread with `start-agent --resume` and return the
/// argv and selected folder, after proving it still takes local keys.
fn host_resume_argv(local_settings: Option<Value>) -> (String, PathBuf) {
    let mut env = Env::new();
    central_asks_for_unsandboxed_codex(&env);
    if let Some(settings) = local_settings {
        env.write_settings(settings);
    }
    env.start_host();
    env.run(&["start-agent", "codex", "--resume", "native-codex-thread"]);
    wait("the resumed agent", &env, || !env.runs().is_empty());
    let (pid, argv) = env.runs()[0].clone();
    // The resumed session still takes local keys.
    std::thread::sleep(Duration::from_secs(1));
    env.type_locally("local-host-keys\r");
    wait("local keys to reach the resumed agent", &env, || {
        env.file("stdin").contains("local-host-keys")
    });
    assert!(alive(pid));
    (argv, env.work.clone())
}

#[test]
fn resume_without_local_settings_ignores_central_account_settings() {
    let (argv, work) = host_resume_argv(None);
    assert_eq!(
        argv,
        format!(
            "codex --no-alt-screen resume native-codex-thread --cd {}",
            work.display()
        ),
        "central settings became agent argv"
    );
}

#[test]
fn resume_applies_current_local_host_settings_not_central_account_settings() {
    let local = json!({"launch": {"codex_sandbox_mode": "read-only"}});
    let (argv, work) = host_resume_argv(Some(local));
    assert_eq!(
        argv,
        format!(
            "codex --no-alt-screen resume native-codex-thread --sandbox read-only --cd {}",
            work.display()
        ),
    );
}

#[test]
fn new_local_sessions_launch_with_local_host_settings_not_central_account_settings() {
    let mut env = Env::new();
    central_asks_for_unsandboxed_codex(&env);
    env.write_settings(
        json!({"launch": {"codex_sandbox_mode": "workspace-write", "codex_approval_policy": "on-request"}}),
    );
    env.start_host();
    env.run(&["start-agent", "codex", "--", "--passthrough"]);
    wait("the started agent", &env, || !env.runs().is_empty());
    std::thread::sleep(Duration::from_secs(1));
    let runs = env.runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    let (pid, argv) = &runs[0];
    assert!(argv.starts_with("codex "), "{runs:?}");
    assert!(
        argv.contains("--sandbox workspace-write --ask-for-approval on-request --passthrough"),
        "{runs:?}"
    );
    assert!(!argv.contains("danger-full-access"), "{runs:?}");
    assert!(alive(*pid), "{runs:?}");
}
