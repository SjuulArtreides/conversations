#![cfg(unix)]
//! `conversations start-agent` as a real process under a real
//! terminal, against a real local host and the real central server router, in
//! an isolated user. A stub agent records its argv, the bytes its terminal
//! delivers and its terminal size, and exits with a chosen code.
//!
//! What must hold: without a running host the command fails before anything
//! starts; with one, the agent runs in the invoking terminal (input, output,
//! resize, exit code, terminal restored) as a canonical host registration with
//! local history; inside cmux it stays in the invoking surface, through a host
//! restart, while an ordinary wrapper there is still relocated.

#[path = "support/owned_process.rs"]
mod owned_process;

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use conversations_server::{build_router_with_auth, AppState, Config, SqliteRepo};
use portable_pty::{CommandBuilder, PtySize};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_conversations");
const ALICE: &str = "alice@example.com";
/// Written by the stub agent into its transcript only.
const TRANSCRIPT_MARK: &str = "transcriptmarkbryony";

/// The real server router on loopback with auth on, and Alice's login cookie.
struct Central {
    _runtime: tokio::runtime::Runtime,
    url: String,
    cookie: String,
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
        let response = reqwest::blocking::Client::new()
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
            _runtime: runtime,
            url,
            cookie,
        }
    }
}

/// The fake cmux: one window with the host's workspace (the host's own
/// surface and another one) and a second workspace of three surfaces. It
/// records every call, creates `ws-new` on `workspace.create` and moves a
/// surface there on `move-surface`, so relocation is observable. A surface's
/// resume binding is the file `binding.<surface>`, as cmux persists it.
const FAKE_CMUX: &str = r#"#!/usr/bin/perl
my $d = $ENV{FAKE_CMUX_STATE};
open my $log, ">>", "$d/cmux.log" or die $!; print $log join(" ", @ARGV), "\n"; close $log;
my $args = join(" ", @ARGV);
my %moved; if (open my $m, "<", "$d/moved") { while (<$m>) { chomp; $moved{$_} = 1 } }
sub ws { my ($id, @s) = @_; '{"id":"' . $id . '","pinned":false,"panes":[{"surfaces":[' . join(",", map { '{"id":"' . $_ . '","type":"terminal"}' } @s) . ']}]}' }
if ($args =~ /tree --all/) {
  my @all = qw(s-host s-side s-busy-a s-busy-b s-busy-c);
  my @ws = (ws("ws-host", grep { !$moved{$_} } qw(s-host s-side)), ws("ws-busy", grep { !$moved{$_} } qw(s-busy-a s-busy-b s-busy-c)));
  push @ws, ws("ws-new", (grep { $moved{$_} } @all), "s-placeholder") if -e "$d/created";
  print '{"windows":[{"id":"w1","workspaces":[' . join(",", @ws) . ']}]}'; exit 0;
}
if ($args =~ /^rpc workspace\.create/) { open my $c, ">", "$d/created"; close $c; print '{"ok":true,"result":{"workspace_id":"ws-new","surface_id":"s-placeholder"}}'; exit 0 }
if ($args =~ /^move-surface --surface (\S+)/) { open my $m, ">>", "$d/moved"; print $m "$1\n"; close $m; exit 0 }
if ($args =~ /^rpc debug\.terminals/) { print '{"ok":true,"result":{"terminals":[]}}'; exit 0 }
if ($args =~ /^surface resume show .*--surface (\S+)/) {
  if (open my $b, "<", "$d/binding.$1") { local $/; my $j = <$b>; print '{"resume_binding":' . $j . '}' } else { print '{}' }
  exit 0;
}
if ($args =~ /^surface resume set --surface (\S+) --kind (\S+) --checkpoint (\S+)/) {
  open my $b, ">", "$d/binding.$1"; print $b '{"kind":"' . $2 . '","checkpoint_id":"' . $3 . '"}'; close $b; exit 0;
}
print '{"ok":true,"result":{}}'; exit 0;
"#;

/// An isolated user with recording stub agents, a fake cmux on PATH, the real
/// central server and (once started) a real local host.
struct Env {
    home: tempfile::TempDir,
    stubs: PathBuf,
    work: PathBuf,
    central: Central,
    host: Option<owned_process::Owned>,
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
            // `mcp get` reports the personal Codex registration in
            // `$HOME/codex-mcp.json`, if any. Otherwise the stub records the
            // exact argument vector NUL-delimited in
            // `argv.<pid>`, and only then `pid wrapped-id agent argv`; a Claude stub
            // writes a transcript for the session it was told it is. It says
            // it is ready, appends the bytes its terminal delivers, reports its
            // terminal size on `report-size` and exits 7 on `exit-seven`. It
            // stops on its own once the test writes `stop` (or its directory
            // is gone) and reports `done.<pid>`.
            write_executable(
                &stubs.join(agent),
                &format!(
                    r#"#!/bin/sh
case "$1 $2" in
"mcp get") [ -f "$HOME/codex-mcp.json" ] && exec /bin/cat "$HOME/codex-mcp.json"; exit 1 ;;
esac
printf '%s' "${{CONVERSATIONS_CLI:-}}" > "{stubs}/cli.$$"
printf '%s\0' "$@" > "{stubs}/argv.$$"
echo "$$ ${{CONVERSATIONS_WRAPPED:--}} {agent} $*" >> "{stubs}/runs"
id=""; prev=""
for a in "$@"; do
  case "$prev" in --session-id|--resume) id="$a";; esac
  prev="$a"
done
if [ "{agent}" = claude ] && [ -n "$id" ]; then
  dir="$HOME/.claude/projects/$(pwd -P | /usr/bin/tr './' '--')"
  /bin/mkdir -p "$dir"
  printf '%s\n' '{{"type":"user","uuid":"'"$id"'-u1","sessionId":"'"$id"'","timestamp":"2026-11-02T10:00:00.000Z","message":{{"role":"user","content":"{TRANSCRIPT_MARK} private turn"}}}}' >> "$dir/$id.jsonl"
fi
printf 'stub-agent-ready\r\n'
exec /usr/bin/perl -MIO::Select -MIO::Handle -e '
my ($d) = @ARGV; my $s = IO::Select->new(\*STDIN); STDOUT->autoflush(1);
open my $f, ">>", "$d/stdin" or die $!; $f->autoflush(1);
my $seen = "";
while (-d $d && !-e "$d/stop") {{
  next unless $s->can_read(0.1);
  my $n = sysread(STDIN, my $b, 4096); last unless $n;
  print $f $b; $seen .= $b;
  if ($seen =~ s/report-size//) {{
    my $size = `/bin/stty size </dev/tty`;
    open my $z, ">>", "$d/sizes"; print $z $size; close $z;
    print "size-reported\r\n";
  }}
  if ($seen =~ /exit-seven/) {{
    open my $done, ">", "$d/done.$$"; print $done "exited"; close $done; exit 7;
  }}
}}
if (-d $d) {{ open my $done, ">", "$d/done.$$"; print $done "exited"; }}
' "{stubs}"
"#,
                    stubs = stubs.display()
                ),
            );
        }
        write_executable(&stubs.join("cmux"), FAKE_CMUX);
        let central = Central::start(&root);
        let env = Self {
            home,
            stubs,
            work,
            central,
            host: None,
        };
        // No web UI: parallel tests would contend for its port.
        std::fs::write(
            env.settings(),
            json!({"web": {"enabled": false}}).to_string(),
        )
        .unwrap();
        std::fs::set_permissions(env.settings(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = env.config_dir();
        std::fs::create_dir_all(&config).unwrap();
        let credentials = config.join("credentials.json");
        std::fs::write(
            &credentials,
            json!({"servers": {env.central.url.clone(): env.central.cookie.clone()}}).to_string(),
        )
        .unwrap();
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
        env
    }

    fn root(&self) -> PathBuf {
        self.home.path().canonicalize().unwrap()
    }

    fn settings(&self) -> PathBuf {
        self.root().join("local-host.json")
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

    fn default_runtime(&self) -> PathBuf {
        self.root().join(".conversations-host-local")
    }

    /// The isolated environment, plus `extra` (e.g. a cmux surface).
    fn environment(&self, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let root = self.root();
        let mut env = vec![
            ("HOME".into(), root.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                root.join("config").display().to_string(),
            ),
            (
                "XDG_DATA_HOME".into(),
                root.join("data").display().to_string(),
            ),
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", self.stubs.display()),
            ),
            ("TERM".into(), "xterm-256color".into()),
            (
                "CONVERSATIONS_HOST_SETTINGS_FILE".into(),
                self.settings().display().to_string(),
            ),
            ("FAKE_CMUX_STATE".into(), self.stubs.display().to_string()),
            (
                "CMUX_BUNDLED_CLI_PATH".into(),
                self.stubs.join("cmux").display().to_string(),
            ),
        ];
        env.extend(extra.iter().map(|(k, v)| ((*k).into(), (*v).into())));
        env
    }

    /// The real binary without a terminal, in the isolated user.
    fn command(&self, args: &[&str], extra: &[(&str, &str)]) -> Command {
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .envs(self.environment(extra))
            .current_dir(&self.work)
            .args(["--server", &self.central.url])
            .args(args);
        command
    }

    /// Start a host and wait until it answers on its socket. `args` are the
    /// startup options after `host`.
    fn start_host(&mut self, runtime: &Path, args: &[&str], extra: &[(&str, &str)]) {
        assert!(self.host.is_none());
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root().join("host.log"))
            .unwrap();
        let mut command = self.command(&["host", "--runtime-dir"], extra);
        command
            .arg(runtime)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        self.host = Some(owned_process::Owned::spawn(&mut command));
        let socket = runtime.join("host.sock");
        let deadline = Instant::now() + Duration::from_secs(30);
        while std::os::unix::net::UnixStream::connect(&socket).is_err() {
            assert!(
                Instant::now() < deadline,
                "the host did not start: {}",
                self.host_log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop_host(&mut self) {
        // Dropping the owned host kills its whole process group.
        self.host = None;
    }

    fn host_log(&self) -> String {
        std::fs::read_to_string(self.root().join("host.log")).unwrap_or_default()
    }

    /// Registered sessions of the host in `runtime`, as `host status --json`.
    fn sessions(&self, runtime: &Path) -> Vec<Value> {
        let output = owned_process::run(
            self.command(&["host", "--runtime-dir"], &[])
                .arg(runtime)
                .args(["status", "--json"]),
            b"",
            Duration::from_secs(20),
        )
        .unwrap();
        let status: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        status["sessions"].as_array().cloned().unwrap_or_default()
    }

    fn session(&self, runtime: &Path, id: &str) -> Option<Value> {
        self.sessions(runtime)
            .into_iter()
            .find(|s| s["launch"]["id"] == id)
    }

    fn history(&self, runtime: &Path, id: &str) -> String {
        let output = owned_process::run(
            self.command(&["history", "--runtime-dir"], &[])
                .arg(runtime)
                .args(["get", id, "--json"]),
            b"",
            Duration::from_secs(20),
        )
        .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.stubs.join(name)).unwrap_or_default()
    }

    /// `(pid, wrapped session id, argv)` for every agent the stubs saw start.
    fn runs(&self) -> Vec<(i32, String, String)> {
        self.file("runs")
            .lines()
            .map(|line| {
                let mut parts = line.splitn(3, ' ');
                (
                    parts.next().unwrap().parse().unwrap(),
                    parts.next().unwrap().to_owned(),
                    parts.next().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }

    /// The exact argument vector the agent with `pid` was started with.
    fn argv(&self, pid: i32) -> Vec<String> {
        self.file(&format!("argv.{pid}"))
            .split_terminator('\0')
            .map(str::to_owned)
            .collect()
    }

    /// Pids of every host that took a host lock in the isolated user.
    fn host_pids(&self) -> Vec<i32> {
        let mut pending = vec![self.root()];
        let mut pids = Vec::new();
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    pending.push(path);
                } else if entry.file_name() == "host.lock" {
                    let owner: Value = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| serde_json::from_str(text.trim()).ok())
                        .unwrap_or(Value::Null);
                    pids.extend(owner["pid"].as_i64().map(|pid| pid as i32));
                }
            }
        }
        pids
    }

    /// The fake cmux calls that create, move or reorder workspaces.
    fn relocations(&self) -> Vec<String> {
        self.file("cmux.log")
            .lines()
            .filter(|line| {
                line.starts_with("rpc workspace.create")
                    || line.starts_with("move-surface")
                    || line.starts_with("reorder-workspaces")
            })
            .map(str::to_owned)
            .collect()
    }

    /// Run `args` under a real 40x120 terminal, inside a shell that records
    /// the terminal mode before and after and the command's exit code.
    fn terminal(&self, name: &str, args: &[&str], extra: &[(&str, &str)]) -> Terminal {
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let records = self.root().join(format!("terminal-{name}"));
        std::fs::create_dir_all(&records).unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        for (key, value) in self.environment(extra) {
            command.env(key, value);
        }
        command.env("RECORDS", &records);
        command.cwd(&self.work);
        command.args([
            "-c",
            r#"/bin/stty -g > "$RECORDS/before"; "$@"; echo "$?" > "$RECORDS/code"; /bin/stty -g > "$RECORDS/after.part"; /bin/mv "$RECORDS/after.part" "$RECORDS/after"; exec /bin/sleep 60"#,
            "sh",
            BIN,
            "--server",
            &self.central.url,
        ]);
        command.args(args);
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Terminal {
            output,
            writer: pty.master.take_writer().unwrap(),
            master: pty.master,
            child,
            records,
        }
    }
}

impl Drop for Env {
    /// The owned host's group is killed with it; the stubs stop themselves on
    /// `stop`, on their terminal's hangup, or once their directory is gone.
    /// Every agent and host this test started must be gone afterwards: a
    /// survivor fails an otherwise passing test, and is reported (without a
    /// second panic) when the test is already failing. Their recorded PIDs
    /// may since have been reused, so they are only ever checked, never
    /// signalled.
    fn drop(&mut self) {
        let _ = std::fs::write(self.stubs.join("stop"), b"stop");
        let mut started: Vec<i32> = self.runs().iter().map(|(pid, _, _)| *pid).collect();
        started.extend(self.host_pids());
        self.host = None;
        let alive = |pid: &i32| unsafe { libc::kill(*pid, 0) } == 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while started.iter().any(alive) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let survivors: Vec<i32> = started.into_iter().filter(alive).collect();
        if survivors.is_empty() {
            return;
        }
        let message = format!("test processes outlived their test: {survivors:?}");
        if std::thread::panicking() {
            eprintln!("{message}");
        } else {
            panic!("{message}");
        }
    }
}

/// A real terminal running one command under `/bin/sh`.
struct Terminal {
    output: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    records: PathBuf,
}

impl Terminal {
    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn type_text(&mut self, text: &str) {
        let _ = self
            .writer
            .write_all(text.as_bytes())
            .and_then(|()| self.writer.flush());
    }

    fn record(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.records.join(name))
            .ok()
            .map(|text| text.trim().to_owned())
    }

    /// The terminal mode recorded as `name` (`stty -g`), without macOS's
    /// `PENDIN`: the kernel sets that state bit itself when canonical mode
    /// returns with input still queued; no program asks for it.
    fn mode(&self, name: &str) -> Option<String> {
        let mode = self.record(name)?;
        Some(
            mode.split(':')
                .map(|field| match field.strip_prefix("lflag=") {
                    Some(flags) => {
                        let flags = u64::from_str_radix(flags, 16).unwrap();
                        format!("lflag={:x}", flags & !0x2000_0000)
                    }
                    None => field.to_owned(),
                })
                .collect::<Vec<_>>()
                .join(":"),
        )
    }

    /// The command's exit code once it has returned.
    fn exit_code(&self) -> Option<i32> {
        self.record("after")?;
        self.record("code")?.parse().ok()
    }
}

impl Drop for Terminal {
    /// The shell leads the terminal's process group, which holds the command
    /// under test; it is still unreaped, so the group id cannot be reused.
    fn drop(&mut self) {
        if let Some(pid) = self.child.process_id() {
            unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = self.child.wait();
    }
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn wait(what: &str, mut ready: impl FnMut() -> bool, context: impl Fn() -> String) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}\n{}",
            context()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Every runtime directory and authority database under the isolated user.
fn runtime_traces(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches!(name.as_str(), "authz.db" | "host.sock" | "history.db") {
                found.push(path.clone());
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(path);
            }
        }
    }
    found
}

#[test]
fn without_a_running_host_start_agent_fails_before_anything_starts() {
    let env = Env::new();
    let custom = env.root().join("custom-runtime");
    for (name, args) in [
        ("fresh", vec!["start-agent", "claude"]),
        (
            "resume",
            vec![
                "start-agent",
                "claude",
                "--resume",
                "3a7d1e0c-5b2f-4c8e-9d6a-1f0e2d3c4b5a",
            ],
        ),
        (
            "opencode",
            vec!["start-agent", "opencode", "--resume", "ses_neverAdopted"],
        ),
        (
            "explicit",
            vec![
                "start-agent",
                "codex",
                "--runtime-dir",
                custom.to_str().unwrap(),
            ],
        ),
    ] {
        let terminal = env.terminal(name, &args, &[]);
        wait(
            "start-agent to return",
            || terminal.exit_code().is_some(),
            || terminal.screen(),
        );
        let screen = terminal.screen();
        assert_ne!(terminal.exit_code(), Some(0), "{name}: {screen}");
        assert!(
            screen.contains("start it with `conversations host`"),
            "{name}: {screen}"
        );
        assert_eq!(terminal.mode("before"), terminal.mode("after"), "{name}");
    }
    std::thread::sleep(Duration::from_secs(1));
    assert!(env.runs().is_empty(), "an agent started: {:?}", env.runs());
    assert_eq!(runtime_traces(&env.root()), Vec::<PathBuf>::new());
    assert!(!env.default_runtime().exists() && !custom.exists());
}

#[test]
fn start_agent_runs_in_the_invoking_terminal_as_a_registered_host_session() {
    let mut env = Env::new();
    // Not the default runtime directory: start-agent has to find this host.
    let runtime = env.root().join("user-host");
    env.start_host(&runtime, &["--backend", "normal"], &[]);
    let mut terminal = env.terminal(
        "journey",
        &["start-agent", "claude", "--", "--passthrough", "two words"],
        &[],
    );
    let context = || {
        format!(
            "terminal:\n{}\nhost:\n{}",
            terminal.screen(),
            env.host_log()
        )
    };
    wait("the agent", || !env.runs().is_empty(), context);
    let (pid, wrapped, argv) = env.runs()[0].clone();
    let session = argv
        .strip_prefix("claude --session-id ")
        .unwrap_or_else(|| panic!("{argv}"))
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    uuid::Uuid::parse_str(&session).unwrap();
    assert_eq!(wrapped, session, "the agent is not host-wrapped: {argv}");
    // Arguments after `--` arrive unsplit, after the session and before the
    // wrapper's own readiness settings.
    let exact = env.argv(pid);
    assert_eq!(
        exact[..4],
        [
            "--session-id",
            session.as_str(),
            "--passthrough",
            "two words"
        ],
        "{exact:?}"
    );
    assert_eq!(exact.len(), 6, "{exact:?}");
    assert_eq!(exact[4], "--settings", "{exact:?}");
    // Output reaches the invoking terminal.
    wait(
        "the agent's output",
        || terminal.screen().contains("stub-agent-ready"),
        || terminal.screen(),
    );
    // A canonical registration with the discovered host, with local history.
    wait(
        "the registration",
        || env.session(&runtime, &session).is_some(),
        || env.host_log(),
    );
    let registered = env.session(&runtime, &session).unwrap();
    assert_eq!(registered["launch"]["agent"], "claude");
    assert_eq!(registered["local_terminal"], true, "{registered}");
    wait(
        "the transcript in local history",
        || env.history(&runtime, &session).contains(TRANSCRIPT_MARK),
        || env.host_log(),
    );
    assert!(
        !env.default_runtime().exists(),
        "start-agent used a runtime of its own"
    );
    // Input reaches the agent.
    terminal.type_text("typed-locally\r");
    wait(
        "local input",
        || env.file("stdin").contains("typed-locally"),
        || terminal.screen(),
    );
    // A resized terminal resizes the agent's.
    terminal
        .master
        .resize(PtySize {
            rows: 50,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    terminal.type_text("report-size\r");
    wait(
        "the resized agent terminal",
        || {
            env.file("sizes")
                .lines()
                .any(|line| line.trim() == "50 100")
        },
        || format!("sizes: {:?}\n{}", env.file("sizes"), terminal.screen()),
    );
    // The agent's exit code is the command's, and the terminal is restored.
    terminal.type_text("exit-seven\r");
    wait(
        "start-agent to return",
        || terminal.exit_code().is_some(),
        || terminal.screen(),
    );
    assert_eq!(terminal.exit_code(), Some(7), "{}", terminal.screen());
    assert_eq!(terminal.mode("before"), terminal.mode("after"));
    wait(
        "the agent to exit",
        || unsafe { libc::kill(pid, 0) } != 0,
        || terminal.screen(),
    );
    assert_eq!(env.runs().len(), 1, "{:?}", env.runs());
    // The host keeps running after the agent exits.
    assert!(std::os::unix::net::UnixStream::connect(runtime.join("host.sock")).is_ok());
}

#[test]
fn a_started_agent_reaches_conversations_through_the_conversations_executable() {
    let mut env = Env::new();
    let runtime = env.root().join("user-host");
    env.start_host(&runtime, &["--backend", "normal"], &[]);
    // The Codex stub has no personal registration, so the launch must load a
    // launch-scoped Conversations MCP server.
    let mut terminal = env.terminal("codex", &["start-agent", "codex"], &[]);
    let context = || {
        format!(
            "terminal:\n{}\nhost:\n{}",
            terminal.screen(),
            env.host_log()
        )
    };
    wait("the agent", || !env.runs().is_empty(), context);
    let (pid, _, _) = env.runs()[0].clone();
    let executable = Path::new(BIN).canonicalize().unwrap();
    assert_eq!(executable.file_name().unwrap(), "conversations");
    let argv = env.argv(pid);
    let command = argv
        .iter()
        .find_map(|arg| arg.strip_prefix("mcp_servers.conversations.command="))
        .unwrap_or_else(|| panic!("no launch-scoped MCP command: {argv:?}"));
    let command: String = serde_json::from_str(command).unwrap();
    assert_eq!(
        Path::new(&command).canonicalize().unwrap(),
        executable,
        "{argv:?}"
    );
    // Hooks and agent plugins call back through CONVERSATIONS_CLI.
    let cli = env.file(&format!("cli.{pid}"));
    assert_eq!(
        Path::new(&cli).canonicalize().unwrap(),
        executable,
        "CONVERSATIONS_CLI={cli:?}"
    );
    terminal.type_text("exit-seven\r");
    wait(
        "start-agent to return",
        || terminal.exit_code().is_some(),
        || terminal.screen(),
    );
    wait(
        "the agent to exit",
        || unsafe { libc::kill(pid, 0) } != 0,
        || terminal.screen(),
    );
}

/// A personal Codex or Claude registration is recognized when it names either
/// the `conversations` executable or a still-installed `arrakis-conversations`
/// one, and never when it names another program.
#[test]
fn personal_registrations_of_either_executable_name_are_recognized() {
    let mut env = Env::new();
    let runtime = env.root().join("user-host");
    env.start_host(&runtime, &["--backend", "normal"], &[]);
    let cases = [
        ("/opt/conversations/bin/conversations", true),
        ("/opt/old/bin/arrakis-conversations", true),
        ("/opt/other/bin/other-mcp", false),
    ];
    for (command, recognized) in cases {
        std::fs::write(
            env.root().join("codex-mcp.json"),
            json!({
                "enabled": true,
                "transport": {
                    "type": "stdio",
                    "command": command,
                    "args": ["mcp", "serve", "--delivery", "codex"]
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            env.root().join(".claude.json"),
            json!({"mcpServers": {"conversations": {
                "command": command,
                "args": ["mcp", "serve", "--delivery", "claude"]
            }}})
            .to_string(),
        )
        .unwrap();
        for agent in ["codex", "claude"] {
            let started = env.runs().len();
            let mut terminal = env.terminal(
                &format!("{agent}-{}", started),
                &["start-agent", agent],
                &[],
            );
            wait(
                "the agent",
                || env.runs().len() > started,
                || {
                    format!(
                        "terminal:\n{}\nhost:\n{}",
                        terminal.screen(),
                        env.host_log()
                    )
                },
            );
            let (pid, _, _) = env.runs()[started].clone();
            let argv = env.argv(pid);
            let ours = if agent == "codex" {
                // Only an unrecognized registration gets a launch-scoped one.
                !argv
                    .iter()
                    .any(|arg| arg.starts_with("mcp_servers.conversations.command="))
            } else {
                argv.iter().any(|arg| arg == "server:conversations")
            };
            assert_eq!(ours, recognized, "{agent} with {command}: {argv:?}");
            terminal.type_text("exit-seven\r");
            wait(
                "start-agent to return",
                || terminal.exit_code().is_some(),
                || terminal.screen(),
            );
            wait(
                "the agent to exit",
                || unsafe { libc::kill(pid, 0) } != 0,
                || terminal.screen(),
            );
        }
    }
}

#[test]
fn inside_cmux_start_agent_keeps_the_invoking_surface_through_a_host_restart() {
    let mut env = Env::new();
    let runtime = env.default_runtime();
    let host_surface = [
        ("CMUX_WORKSPACE_ID", "ws-host"),
        ("CMUX_SURFACE_ID", "s-host"),
    ];
    let cmux_bin = env.stubs.join("cmux").display().to_string();
    let host_args = ["--backend", "cmux", "--cmux-bin", cmux_bin.as_str()];
    env.start_host(&runtime, &host_args, &host_surface);
    fn surface<'a>(workspace: &'a str, surface: &'a str) -> [(&'a str, &'a str); 2] {
        [
            ("CMUX_WORKSPACE_ID", workspace),
            ("CMUX_SURFACE_ID", surface),
        ]
    }
    // The host's own surface (a host backgrounded there), another surface of
    // the host's workspace, and two surfaces of one workspace.
    let placements = [
        ("ws-host", "s-host"),
        ("ws-host", "s-side"),
        ("ws-busy", "s-busy-a"),
        ("ws-busy", "s-busy-b"),
    ];
    let mut terminals = Vec::new();
    let mut ids = Vec::new();
    for (n, (workspace, at)) in placements.iter().enumerate() {
        terminals.push(env.terminal(at, &["start-agent", "claude"], &surface(workspace, at)));
        wait(
            "the agent",
            || env.runs().len() > n,
            || format!("{}\n{}", terminals[n].screen(), env.host_log()),
        );
        let (_, wrapped, _) = env.runs()[n].clone();
        wait(
            "the registration",
            || env.session(&runtime, &wrapped).is_some(),
            || format!("{}\n{}", terminals[n].screen(), env.host_log()),
        );
        ids.push(wrapped);
    }
    let placed = |env: &Env| -> Vec<(String, String)> {
        ids.iter()
            .map(|id| {
                let session = env.session(&runtime, id).unwrap_or(Value::Null);
                (
                    session["workspace"].as_str().unwrap_or("-").to_owned(),
                    session["surface"].as_str().unwrap_or("-").to_owned(),
                )
            })
            .collect()
    };
    let expected: Vec<(String, String)> = placements
        .iter()
        .map(|(w, s)| ((*w).to_owned(), (*s).to_owned()))
        .collect();
    assert_eq!(placed(&env), expected);
    assert_eq!(env.relocations(), Vec::<String>::new());
    for id in &ids {
        wait(
            "each session's transcript in local history",
            || env.history(&runtime, id).contains(TRANSCRIPT_MARK),
            || env.host_log(),
        );
    }

    // The same identities, surfaces and history after the host restarts.
    env.stop_host();
    env.start_host(&runtime, &host_args, &host_surface);
    wait(
        "every agent to reconnect",
        || ids.iter().all(|id| env.session(&runtime, id).is_some()),
        || env.host_log(),
    );
    assert_eq!(placed(&env), expected);
    assert_eq!(env.relocations(), Vec::<String>::new());
    assert_eq!(env.runs().len(), placements.len(), "{:?}", env.runs());
    for id in &ids {
        assert!(
            env.history(&runtime, id).contains(TRANSCRIPT_MARK),
            "{id} lost its history across the reconnect"
        );
    }

    // An ordinary wrapper in the occupied workspace is still relocated.
    let claude = env.stubs.join("claude").display().to_string();
    let _ordinary = env.terminal(
        "ordinary",
        &["host", "wrap", "--agent", "claude", "--", claude.as_str()],
        &surface("ws-busy", "s-busy-c"),
    );
    wait(
        "the ordinary wrapper to be relocated",
        || {
            env.relocations()
                .iter()
                .any(|line| line.starts_with("move-surface --surface s-busy-c"))
        },
        || format!("cmux: {}\n{}", env.file("cmux.log"), env.host_log()),
    );
    assert_eq!(placed(&env), expected);
}

#[test]
fn unwinding_start_agent_tests_leave_none_of_their_processes_running() {
    let owned = Arc::new(Mutex::new(Vec::new()));
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut env = Env::new();
        let runtime = env.default_runtime();
        env.start_host(&runtime, &["--backend", "normal"], &[]);
        let terminal = env.terminal("unwind", &["start-agent", "claude"], &[]);
        wait("the agent", || !env.runs().is_empty(), || terminal.screen());
        let mut pids: Vec<i32> = env.runs().iter().map(|(pid, _, _)| *pid).collect();
        pids.extend(terminal.child.process_id().map(|pid| pid as i32));
        let hosts = env.host_pids();
        assert_eq!(hosts.len(), 1, "{hosts:?}");
        pids.extend(hosts);
        *owned.lock().unwrap() = pids;
        panic!("intentional failure with a running agent");
    }));
    assert!(unwound.is_err());
    let owned = owned.lock().unwrap().clone();
    assert!(owned.len() >= 3, "{owned:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while owned.iter().any(|pid| unsafe { libc::kill(*pid, 0) } == 0) {
        assert!(Instant::now() < deadline, "still running: {owned:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn env_cleanup_fails_on_a_survivor_without_signalling_a_process_it_does_not_own() {
    // An unrelated process whose PID the run log names, as after PID reuse.
    let mut unrelated = Command::new("/bin/sleep").arg("60").spawn().unwrap();
    let env = Env::new();
    std::fs::write(
        env.stubs.join("runs"),
        format!("{} - claude reused\n", unrelated.id()),
    )
    .unwrap();
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(env)));
    let untouched = unrelated.try_wait().unwrap().is_none();
    let _ = unrelated.kill();
    let _ = unrelated.wait();
    let message = cleanup
        .err()
        .and_then(|error| error.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert!(message.contains("outlived their test"), "{message:?}");
    assert!(untouched, "cleanup signalled a process it does not own");
}

/// A host from before `start-agent`: it answers `Status` in its old shape and
/// records the type of every request it receives. Dropping it stops its
/// listener and joins its thread, within a bounded wait for a stalled client.
struct PreviousHost {
    received: Arc<Mutex<Vec<String>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn previous_host(runtime: &Path) -> PreviousHost {
    use std::sync::atomic::Ordering;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(runtime)
        .unwrap();
    let listener = std::os::unix::net::UnixListener::bind(runtime.join("host.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (log, stopped) = (received.clone(), stop.clone());
    let thread = std::thread::spawn(move || {
        while !stopped.load(Ordering::SeqCst) {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(_) => return,
            };
            let bounded = stream.set_nonblocking(false).and_then(|()| {
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))
            });
            if bounded.is_err() {
                continue;
            }
            let mut len = [0u8; 4];
            if stream.read_exact(&mut len).is_err() {
                continue;
            }
            let mut body = vec![0; u32::from_be_bytes(len) as usize];
            if stream.read_exact(&mut body).is_err() {
                continue;
            }
            let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let kind = message["type"].as_str().unwrap_or("?").to_owned();
            log.lock().unwrap().push(kind.clone());
            if kind == "status" {
                let reply = json!({"type": "status", "backend": "normal", "sessions": []});
                let bytes = serde_json::to_vec(&reply).unwrap();
                let _ = stream.write_all(&(bytes.len() as u32).to_be_bytes());
                let _ = stream.write_all(&bytes);
            }
        }
    });
    PreviousHost {
        received,
        stop,
        thread: Some(thread),
    }
}

impl Drop for PreviousHost {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn a_previous_host_fixture_releases_its_listener_and_thread_even_when_unwinding() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("previous-host");
    let mut received = None;
    let mut client = None;
    let started = Instant::now();
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let host = previous_host(&runtime);
        received = Some(host.received.clone());
        // A request still half sent when the test fails.
        let mut stream =
            std::os::unix::net::UnixStream::connect(runtime.join("host.sock")).unwrap();
        stream.write_all(&[0, 0]).unwrap();
        client = Some(stream);
        std::thread::sleep(Duration::from_millis(200));
        panic!("intentional failure with a client connected");
    }));
    assert!(unwound.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let received = received.unwrap();
    assert_eq!(
        Arc::strong_count(&received),
        1,
        "the fixture's listener thread outlived it"
    );
    assert!(
        std::os::unix::net::UnixStream::connect(runtime.join("host.sock")).is_err(),
        "the fixture's listener outlived it"
    );
    drop(client);
}

#[test]
fn a_running_host_without_in_place_sessions_is_refused_before_anything_starts() {
    let env = Env::new();
    let runtime = env.root().join("previous-host");
    let previous = previous_host(&runtime);
    let runtime_arg = runtime.display().to_string();
    for (name, args) in [
        ("fresh", vec!["start-agent", "claude"]),
        (
            "resume",
            vec![
                "start-agent",
                "claude",
                "--resume",
                "3a7d1e0c-5b2f-4c8e-9d6a-1f0e2d3c4b5a",
            ],
        ),
        (
            "opencode",
            vec!["start-agent", "opencode", "--resume", "ses_previousHost"],
        ),
    ] {
        let mut full = args.clone();
        full.extend(["--runtime-dir", runtime_arg.as_str()]);
        let output = owned_process::run(&mut env.command(&full, &[]), b"", Duration::from_secs(20))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{name}: {stderr}");
        assert!(
            stderr.contains("restart it with the current `conversations host`"),
            "{name}: {stderr}"
        );
    }
    std::thread::sleep(Duration::from_secs(1));
    assert!(env.runs().is_empty(), "an agent started: {:?}", env.runs());
    // Only the capability question reached the host; nothing was recorded.
    assert_eq!(
        *previous.received.lock().unwrap(),
        ["status", "status", "status"]
    );
    assert_eq!(
        runtime_traces(&env.root()),
        [runtime.join("host.sock")],
        "a resume record or history was written"
    );
}

#[test]
fn start_agent_runs_the_native_executable_the_host_is_configured_to_use() {
    let mut env = Env::new();
    // Each configured executable marks its run, then is the ordinary stub.
    let configured = env.root().join("configured-bin");
    std::fs::create_dir_all(&configured).unwrap();
    let marked = |name: &str| {
        format!(
            "#!/bin/sh\necho \"$$\" >> \"{}/{name}\"\nexec \"{}/claude\" \"$@\"\n",
            env.stubs.display(),
            env.stubs.display()
        )
    };
    write_executable(&configured.join("claude"), &marked("launcher-path"));
    let custom = env.root().join("cmux-custom-claude");
    write_executable(&custom, &marked("cmux-custom"));
    std::fs::write(
        env.settings(),
        json!({"web": {"enabled": false}, "host_env": {"launcher_path": [configured]}}).to_string(),
    )
    .unwrap();
    let runtime = env.default_runtime();
    env.start_host(&runtime, &["--backend", "normal"], &[]);
    // A `claude` is also on the invoking PATH; the configured one wins.
    let _configured = env.terminal("launcher-path", &["start-agent", "claude"], &[]);
    wait(
        "the configured agent",
        || env.runs().len() == 1,
        || env.host_log(),
    );
    assert_eq!(
        env.file("launcher-path").trim(),
        env.runs()[0].0.to_string(),
        "the agent was not the launcher_path executable"
    );
    // A cmux-configured native executable wins over both.
    let custom_path = custom.display().to_string();
    let _custom = env.terminal(
        "cmux-custom",
        &["start-agent", "claude"],
        &[("CMUX_CUSTOM_CLAUDE_PATH", custom_path.as_str())],
    );
    wait(
        "the custom agent",
        || env.runs().len() == 2,
        || env.host_log(),
    );
    assert_eq!(
        env.file("cmux-custom").trim(),
        env.runs()[1].0.to_string(),
        "the agent was not the cmux custom executable"
    );
    assert_eq!(env.file("launcher-path").lines().count(), 1);
}

#[test]
fn resuming_into_a_borrowed_cmux_surface_leaves_its_resume_binding_alone() {
    let mut env = Env::new();
    let runtime = env.default_runtime();
    // A managed session the host restores into cmux: open restore intent.
    let native = "6f1c2d3e-4a5b-4c6d-8e7f-90a1b2c3d4e5";
    std::fs::write(
        env.config_dir().join("open-sessions.json"),
        json!({"servers": {env.central.url.clone(): [{
            "session_id": native,
            "agent": "claude",
            "cwd": env.work.display().to_string(),
            "resume_id": native,
        }]}})
        .to_string(),
    )
    .unwrap();
    // The surface start-agent borrows already resumes something of its own.
    let original = r#"{"kind":"conversations","checkpoint_id":"the-shell-of-s-side"}"#;
    std::fs::write(env.stubs.join("binding.s-side"), original).unwrap();
    let host_surface = [
        ("CMUX_WORKSPACE_ID", "ws-host"),
        ("CMUX_SURFACE_ID", "s-host"),
    ];
    let cmux_bin = env.stubs.join("cmux").display().to_string();
    env.start_host(
        &runtime,
        &["--backend", "cmux", "--cmux-bin", cmux_bin.as_str()],
        &host_surface,
    );
    let terminal = env.terminal(
        "borrowed",
        &["start-agent", "claude", "--resume", native],
        &[
            ("CMUX_WORKSPACE_ID", "ws-host"),
            ("CMUX_SURFACE_ID", "s-side"),
        ],
    );
    wait(
        "the agent",
        || !env.runs().is_empty(),
        || format!("{}\n{}", terminal.screen(), env.host_log()),
    );
    // Non-vacuous: this is the managed record, with its restore intent.
    let (_, wrapped, _) = env.runs()[0].clone();
    assert_eq!(wrapped, native);
    wait(
        "the registration",
        || env.session(&runtime, native).is_some(),
        || env.host_log(),
    );
    // Several title/identity synchronization ticks pass.
    let calls = env.file("cmux.log").lines().count();
    wait(
        "later cmux synchronization",
        || env.file("cmux.log").lines().count() > calls + 6,
        || env.file("cmux.log"),
    );
    std::thread::sleep(Duration::from_secs(5));
    let writes: Vec<String> = env
        .file("cmux.log")
        .lines()
        .filter(|line| line.starts_with("surface resume set --surface s-side"))
        .map(str::to_owned)
        .collect();
    assert_eq!(writes, Vec::<String>::new());
    assert_eq!(env.file("binding.s-side"), original);
    assert!(env.session(&runtime, native).is_some());
}
