#![cfg(unix)]
//! `conversations login` hands off to host mode: the real binary logs in to
//! the real server router on loopback, in an isolated user.
//!
//! What must hold: every successful login names a pasteable command that
//! starts host mode on the same, explicit server through this exact binary
//! (PATH need not contain it). Without a terminal the login output is only
//! that and nothing is asked. On a terminal it offers to start host mode,
//! defaulting to No; accepting runs the foreground host on the same server
//! with the saved login.

#[path = "support/owned_process.rs"]
#[allow(dead_code)]
mod owned_process;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use conversations_server::{build_router_with_auth, AppState, Config, SqliteRepo};
use portable_pty::{CommandBuilder, PtySize};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_conversations");
const ALICE: &str = "alice@example.com";
const PASSWORD: &str = "test-password-123";
const START_PROMPT: &str = "Start host mode now?";

/// The real server router on loopback with auth on and Alice registered.
struct Central {
    _runtime: tokio::runtime::Runtime,
    url: String,
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
            .json(&json!({"email": ALICE, "password": PASSWORD, "display_name": "Alice"}))
            .send()
            .unwrap();
        assert_eq!(response.status(), 201);
        Self {
            _runtime: runtime,
            url,
        }
    }
}

struct Env {
    root: tempfile::TempDir,
    central: Central,
}

impl Env {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let central = Central::start(root.path());
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        Self { root, central }
    }

    fn runtime(&self) -> PathBuf {
        self.root.path().join("runtime")
    }

    /// A clean user environment whose PATH cannot find `conversations`.
    fn environment(&self) -> Vec<(&'static str, PathBuf)> {
        let home = self.root.path().join("home");
        vec![
            ("XDG_CONFIG_HOME", home.join(".config")),
            ("HOME", home),
            ("PATH", "/usr/bin:/bin".into()),
            ("TERM", "xterm-256color".into()),
            ("CONVERSATIONS_HOST_DIR", self.runtime()),
        ]
    }

    fn login_args(&self) -> [&str; 7] {
        [
            "--server",
            &self.central.url,
            "login",
            "--email",
            ALICE,
            "--password",
            PASSWORD,
        ]
    }

    /// The handoff command `login` must print for this server.
    fn expected_command(&self) -> String {
        let exe = std::fs::canonicalize(BIN).unwrap();
        format!("'{}' --server '{}' host", exe.display(), self.central.url)
    }

    fn terminal(&self) -> Terminal {
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(BIN);
        command.env_clear();
        for (key, value) in self.environment() {
            command.env(key, value);
        }
        command.cwd(self.root.path());
        command.args(self.login_args());
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
            _master: pty.master,
            child,
        }
    }
}

/// `login` under a real terminal; it leads its own session and process group.
struct Terminal {
    output: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn Write + Send>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Terminal {
    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn wait_for(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{}",
                self.screen()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_text(&self, text: &str) {
        self.wait_for(text, |terminal| terminal.screen().contains(text));
    }

    fn press(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn exit_status(&mut self) -> portable_pty::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "login did not exit; screen:\n{}",
                self.screen()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Decline making this server the default, then wait for the host offer.
    fn reach_host_offer(&mut self) {
        self.wait_for_text("as your default server?");
        self.press("n");
        self.wait_for_text(START_PROMPT);
    }
}

impl Drop for Terminal {
    /// While `login` is unreaped its group id cannot be reused, so only a
    /// still-running group is signalled.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            if let Some(pid) = self.child.process_id() {
                unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
            }
            let _ = self.child.wait();
        }
    }
}

#[test]
fn noninteractive_login_prints_a_pasteable_host_command_and_asks_nothing() {
    let env = Env::new();
    let mut command = Command::new(BIN);
    command
        .env_clear()
        .envs(env.environment())
        .args(env.login_args());
    let output = owned_process::run(&mut command, b"", Duration::from_secs(30)).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{output:?}");
    let url = &env.central.url;
    assert_eq!(
        stdout,
        format!(
            "Logged in as Alice on {url}.\nTo start host mode on {url}, run:\n  {}\n",
            env.expected_command()
        )
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(START_PROMPT),
        "{output:?}"
    );
    assert!(!env.runtime().exists(), "no host may start");

    // The printed line runs as-is from a shell whose PATH lacks the binary.
    let line = stdout.lines().last().unwrap().trim();
    let mut pasted = Command::new("/bin/sh");
    pasted
        .env_clear()
        .envs(env.environment())
        .arg("-c")
        .arg(format!("{line} --help"));
    let help = owned_process::run(&mut pasted, b"", Duration::from_secs(30)).unwrap();
    assert!(help.status.success(), "{help:?}");
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("Usage: conversations host"),
        "{help:?}"
    );
}

#[test]
fn interactive_login_offers_host_mode_and_defaults_to_no() {
    let env = Env::new();
    let mut terminal = env.terminal();
    terminal.reach_host_offer();
    assert!(terminal.screen().contains(&env.expected_command()));
    terminal.press("\r");
    assert!(terminal.exit_status().success(), "{}", terminal.screen());
    assert!(!env.runtime().exists(), "declining must not start a host");
}

#[test]
fn accepting_the_offer_runs_the_foreground_host_with_the_saved_login() {
    let env = Env::new();
    let mut terminal = env.terminal();
    terminal.reach_host_offer();
    terminal.press("y");
    let socket = env.runtime().join("host.sock");
    terminal.wait_for("the host socket", |_| socket.exists());
    terminal.wait_for_text(&format!("host → {}", env.central.url));
    terminal.wait_for_text("browser approval prompt");
    let screen = terminal.screen();
    assert!(
        !screen.contains("No localhost login"),
        "the host must use the saved login:\n{screen}"
    );
    // Foreground: login has become the host and keeps running.
    assert!(terminal.child.try_wait().unwrap().is_none(), "{screen}");
}
