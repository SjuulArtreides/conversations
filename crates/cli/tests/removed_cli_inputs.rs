#![cfg(unix)]
//! Historical `live` and `pr` inputs that were removed must fail at argument
//! parsing with a usage error, before the real binary starts any agent,
//! wrapper or host. None may fall through to an interactive launch.

#[path = "support/owned_process.rs"]
mod owned_process;

use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_arrakis-conversations");

#[test]
fn removed_live_and_pr_inputs_fail_at_parsing_before_launching_anything() {
    let home = tempfile::tempdir().unwrap();
    let stubs = home.path().join("stubs");
    std::fs::create_dir_all(&stubs).unwrap();
    // Any launch would run this stub agent first, and it records that it ran.
    let agent = stubs.join("claude");
    std::fs::write(
        &agent,
        format!(
            "#!/bin/sh\necho \"$$ $*\" >> '{}'\n",
            stubs.join("claude.runs").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let removed: &[&[&str]] = &[
        &["live", "--tail"],
        &["live", "--once"],
        &["live", "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11"],
        &["live", "--latest"],
        &["live", "--project", "work"],
        &["live", "--interactive"],
        &["live", "--complete-on-exit"],
        &["--uploaded-by", "someone@example.com", "list"],
        &["list", "--uploaded-by", "someone@example.com"],
        &["pr", "watch"],
    ];
    for args in removed {
        let mut command = Command::new(BIN);
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy().into_owned();
            if key.starts_with("CONVERSATIONS_") || key.starts_with("CMUX_") {
                command.env_remove(key);
            }
        }
        command
            .args(["--server", "http://127.0.0.1:9"])
            .args(*args)
            .env("HOME", home.path())
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("PATH", format!("{}:/usr/bin:/bin", stubs.display()))
            .current_dir(home.path());
        // A parse error returns at once; anything that started running instead
        // is stopped at the deadline, with whatever it launched.
        let output = owned_process::run(&mut command, b"", Duration::from_secs(20))
            .unwrap_or_else(|error| panic!("{args:?} was accepted: {error}"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(
            stderr.contains("unexpected argument") || stderr.contains("unrecognized subcommand"),
            "{args:?}: {stderr}"
        );
    }
    assert!(
        !stubs.join("claude.runs").exists(),
        "a removed input launched an agent"
    );
}
