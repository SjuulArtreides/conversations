#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Process(Child);

fn wait_for_exit(process: &mut Process) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "competing host stayed alive");
        std::thread::sleep(Duration::from_millis(20));
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn different_runtime_directories_cannot_start_competing_hosts() {
    let home = tempfile::tempdir().unwrap();
    let start = |runtime: &str| {
        Process(
            Command::new(env!("CARGO_BIN_EXE_arrakis-conversations"))
                .env_remove("CONVERSATIONS_WRAPPED")
                .env_remove("CONVERSATIONS_WRAPPER_PID")
                .env_remove("CONVERSATIONS_HOST_CONTROL")
                .env_remove("CONVERSATIONS_HOST_DIR")
                .env_remove("CMUX_WORKSPACE_ID")
                .env_remove("CMUX_SURFACE_ID")
                .env("HOME", home.path())
                .env("XDG_DATA_HOME", home.path().join("data"))
                .env("XDG_CONFIG_HOME", home.path().join("config"))
                .args(["--server", "http://127.0.0.1:9", "host", "--runtime-dir"])
                .arg(home.path().join(runtime))
                .args(["--backend", "normal"])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    };
    let mut first = start("one");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !home.path().join("one/host.sock").exists() {
        assert!(first.0.try_wait().unwrap().is_none(), "first host exited");
        assert!(Instant::now() < deadline, "first host did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(std::os::unix::net::UnixStream::connect(home.path().join("one/host.sock")).is_ok());
    use std::io::Read;
    let mut second = start("two");
    let status = wait_for_exit(&mut second);
    assert!(!status.success());
    let mut error = String::new();
    second
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert!(error.contains("host already running"), "{error}");
    assert!(error.contains(&first.0.id().to_string()), "{error}");
    assert!(first.0.try_wait().unwrap().is_none());
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    let mut replacement = start("two");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !home.path().join("two/host.sock").exists() {
        assert!(
            replacement.0.try_wait().unwrap().is_none(),
            "replacement failed after owner died"
        );
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
}
