//! Boots the real `conversations-server` binary to prove the auth posture that
//! ships when an operator sets nothing: auth is on, anonymous API access is
//! refused, `AUTH_ENABLED=false` still opts out, and a config with no usable
//! login method refuses to start.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const AUTH_ENV: &[&str] = &[
    "AUTH_ENABLED",
    "PASSWORD_AUTH_ENABLED",
    "INITIAL_ADMIN_EMAILS",
    "GOOGLE_CLIENT_ID",
    "GOOGLE_CLIENT_SECRET",
    "OIDC_REDIRECT_URL",
    "ALLOWED_HOSTED_DOMAIN",
];

fn server(dir: &std::path::Path, env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_conversations-server"));
    for key in AUTH_ENV {
        cmd.env_remove(key);
    }
    cmd.env("BIND_ADDR", "127.0.0.1:0")
        .env(
            "DATABASE_URL",
            format!("sqlite://{}/server.db?mode=rwc", dir.display()),
        )
        .env("STATIC_DIR", dir.join("missing-static"))
        .env("SHUTDOWN_DRAIN_SECS", "0")
        .env("LIVE_REAP_INTERVAL_SECS", "0")
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start the server and return it with the port parsed from its listen log.
fn start(dir: &std::path::Path, env: &[(&str, &str)]) -> (Running, u16) {
    let mut child = server(dir, env).spawn().expect("spawn server");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("conversations-server listening") {
                let port = line
                    .rsplit("127.0.0.1:")
                    .next()
                    .and_then(|rest| {
                        rest.chars()
                            .take_while(char::is_ascii_digit)
                            .collect::<String>()
                            .parse::<u16>()
                            .ok()
                    })
                    .expect("listen log carries the bound port");
                let _ = tx.send(port);
            }
        }
    });
    let port = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("server did not report a listening address");
    (Running(child), port)
}

fn get_status(port: u16, path: &str) -> u16 {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("malformed response: {response}"))
}

#[test]
fn unconfigured_server_requires_auth() {
    let dir = tempfile_dir("default");
    let (_server, port) = start(&dir, &[]);
    assert_eq!(get_status(port, "/api/sessions"), 401);
    assert_eq!(get_status(port, "/healthz"), 200);
}

#[test]
fn auth_enabled_false_opts_out_explicitly() {
    let dir = tempfile_dir("opt-out");
    let (_server, port) = start(&dir, &[("AUTH_ENABLED", "false")]);
    assert_eq!(get_status(port, "/api/sessions"), 200);
}

#[test]
fn server_without_a_login_method_refuses_to_start() {
    let dir = tempfile_dir("no-method");
    let mut server = Running(
        server(&dir, &[("PASSWORD_AUTH_ENABLED", "false")])
            .spawn()
            .expect("spawn server"),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server started instead of refusing the config"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(!status.success(), "server must not start");
    let mut stderr = String::new();
    server
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains("no login method is configured"),
        "unclear startup error: {stderr}"
    );
}

fn tempfile_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "conversations-default-auth-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
