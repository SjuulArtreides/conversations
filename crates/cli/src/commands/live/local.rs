//! The local host a `live` multiplexer's sessions register with.
//!
//! Every mux session runs in a real host wrapper (`host wrap`), so it gets the
//! canonical local session record, local transcript history and the in-harness
//! same-machine handoff from the host, exactly like a cmux or normal-backend
//! terminal. The multiplexer uses the host that already runs for its server;
//! when none does, it starts a normal-backend host for its own lifetime.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// How long a freshly started host has to open its socket.
const START_DEADLINE: Duration = Duration::from_secs(10);
/// How long a host this mux started has to stop after SIGTERM.
const STOP_GRACE: Duration = Duration::from_secs(3);

pub(super) struct LocalHost {
    /// Runtime directory the wrappers register in.
    pub(super) dir: PathBuf,
    /// The host this mux started, stopped when the mux exits.
    owned: Option<Child>,
}

pub(super) fn reachable(dir: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(dir.join("host.sock")).is_ok()
}

impl LocalHost {
    /// The running host for `server`, or a normal-backend host started now.
    /// Sessions still run when no host can be started: their wrappers keep
    /// the terminal local and register once a host appears.
    pub(super) fn ensure(server: &str) -> Self {
        let dir = crate::commands::local_host::agent_link::runtime_for(server);
        if reachable(&dir) {
            return Self { dir, owned: None };
        }
        match start(server, &dir) {
            Ok(owned) => Self {
                dir,
                owned: Some(owned),
            },
            Err(error) => {
                live_error!(
                    "conversations live: no local host for these sessions ({error:#}); \
                     they stay local and register when `host` starts"
                );
                Self { dir, owned: None }
            }
        }
    }
}

fn start(server: &str, dir: &Path) -> Result<Child> {
    crate::commands::local_host::authz::ensure_dir(dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("live-host.log"))
        .context("opening the local host log")?;
    // Same process group as the mux: a terminal hangup that ends the mux ends
    // this host too. It never reads the terminal.
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--server", server, "host", "--runtime-dir"])
        .arg(dir)
        .args(["--backend", "normal"])
        .env_remove("CONVERSATIONS_WRAPPED")
        .env_remove("CONVERSATIONS_WRAPPER_PID")
        .env_remove("CONVERSATIONS_HOST_CONTROL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .context("starting the local host")?;
    let deadline = Instant::now() + START_DEADLINE;
    while !reachable(dir) {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "the local host exited ({status}); see {}",
                dir.join("live-host.log").display()
            );
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            anyhow::bail!("the local host did not open its socket");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    live_info!(
        "conversations live: started local host in {}",
        dir.display()
    );
    Ok(child)
}

fn stop(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: `pid` is our own unreaped child.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for LocalHost {
    fn drop(&mut self) {
        if let Some(child) = self.owned.as_mut() {
            stop(child);
        }
    }
}
