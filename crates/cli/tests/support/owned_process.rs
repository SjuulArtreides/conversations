//! A process a test spawned and owns, as the leader of its own process group.
//! On success, timeout or unwind the whole group is killed and the leader
//! reaped, so a forked descendant cannot outlive the test. Nothing outside
//! that group is ever signalled: the leader stays unreaped until its group has
//! been killed, so its PID, and with it the group id, cannot be reused.

use std::io::{Read, Seek, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

pub struct Owned {
    child: Child,
    reaped: bool,
}

impl Owned {
    pub fn spawn(command: &mut Command) -> Self {
        Self {
            child: command.process_group(0).spawn().unwrap(),
            reaped: false,
        }
    }

    /// Whether the leader has exited, without reaping it.
    pub fn exited(&self) -> bool {
        // SAFETY: a zeroed `siginfo_t` is a valid out-parameter; `WNOWAIT`
        // leaves the child waitable, and `WNOHANG` zeroes `si_signo` when it
        // has not exited yet.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let polled = libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            polled != 0 || info.si_signo != 0
        }
    }

    fn kill_group(&self) {
        // SAFETY: the unreaped leader still holds this group id.
        unsafe { libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL) };
    }

    /// Wait for the leader within `timeout`, then end anything left in its
    /// group and reap it. On timeout the group is killed when `self` drops.
    pub fn finish(mut self, timeout: Duration) -> Result<ExitStatus, String> {
        let deadline = Instant::now() + timeout;
        while !self.exited() {
            if Instant::now() >= deadline {
                return Err(format!("did not finish within {timeout:?}"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.kill_group();
        self.reaped = true;
        self.child.wait().map_err(|error| error.to_string())
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill_group();
            let _ = self.child.wait();
        }
    }
}

/// Run `command` with `input` on stdin, bounded by `timeout`, collecting its
/// output through files so a full pipe can never block it.
pub fn run(command: &mut Command, input: &[u8], timeout: Duration) -> Result<Output, String> {
    let (mut stdout, mut stderr) = (tempfile::tempfile().unwrap(), tempfile::tempfile().unwrap());
    let mut owned = Owned::spawn(
        command
            .stdin(Stdio::piped())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap()),
    );
    // Closing stdin at the end of this statement delivers end of input.
    let _ = owned.child.stdin.take().unwrap().write_all(input);
    let status = owned.finish(timeout)?;
    let read = |file: &mut std::fs::File| {
        let mut bytes = Vec::new();
        file.rewind().unwrap();
        file.read_to_end(&mut bytes).unwrap();
        bytes
    };
    Ok(Output {
        status,
        stdout: read(&mut stdout),
        stderr: read(&mut stderr),
    })
}
