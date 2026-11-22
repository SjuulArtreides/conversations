#![cfg(unix)]
//! Historical `live`, `pr`, `host start` and nested `host` inputs, and the
//! deprecated multiplexer options on `start-agent`, that were removed must
//! fail at argument parsing with a usage error, before the real binary starts
//! any agent, wrapper, host or web server. None may fall through to an
//! interactive launch.

#[path = "support/owned_process.rs"]
mod owned_process;

use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_conversations");

/// The built executable presents itself as `conversations`, with exactly the
/// current top-level commands and none of the removed ones.
#[test]
fn help_shows_the_conversations_command_and_its_current_commands() {
    let output = Command::new(BIN)
        .arg("--help")
        .env_remove("CONVERSATIONS_SERVER")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{help}");
    assert!(
        help.contains("\nUsage: conversations [OPTIONS] <COMMAND>\n"),
        "{help}"
    );
    let commands: Vec<&str> = help
        .split("\nCommands:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .unwrap_or_else(|| panic!("{help}"))
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(
        commands,
        [
            "mcp",
            "list",
            "upload",
            "start-agent",
            "host",
            "open",
            "credential",
            "passkey",
            "history",
            "login",
            "logout",
            "ping",
            "pr",
            "self-update",
            "help"
        ],
        "{help}"
    );
    assert!(!help.contains("arrakis-conversations"), "{help}");
}

#[test]
fn removed_inputs_fail_at_parsing_before_launching_anything() {
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
    // A browser opened by any input would be recorded here, never shown.
    let browser = stubs.join("browser");
    std::fs::write(
        &browser,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\n",
            stubs.join("browser.runs").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o700)).unwrap();
    let settings = home.path().join("local-host.json");
    std::fs::write(&settings, r#"{"web": {"enabled": false}}"#).unwrap();
    let removed: &[&[&str]] = &[
        &["live", "--tail"],
        &["live", "--once"],
        &["live", "3f1d2c9e-6f2a-4a53-9a0c-0c6d6f1d9b11"],
        &["live", "--latest"],
        &["live", "--project", "work"],
        &["live", "--interactive"],
        &["live", "--complete-on-exit"],
        &["live"],
        &["live", "claude"],
        &["live", "--host"],
        &["live", "codex", "--resume", "native-codex-thread"],
        &["host", "start"],
        &["host", "start", "--agent", "claude"],
        &["start-agent", "--agent", "claude"],
        &["start-agent", "claude", "--host"],
        &["start-agent", "claude", "--tail"],
        &["start-agent", "claude", "--no-resume"],
        &["start-agent", "claude", "--mirror-size", "40x120"],
        &["--uploaded-by", "someone@example.com", "list"],
        &["list", "--uploaded-by", "someone@example.com"],
        &["pr", "watch"],
        &["host", "run"],
        &["host", "run", "--backend", "normal"],
        &["host", "web"],
        &["host", "web", "--port", "1"],
        &["host", "credential", "identity"],
        &["host", "passkey", "pending"],
        &["host", "history", "search", "x"],
        &["open", "--port", "1"],
    ];
    // A shell is not an agent to start.
    let invalid: &[&[&str]] = &[&["start-agent", "zsh"], &["start-agent", "bash"]];
    // Startup options belong to bare `host` only.
    let conflicting: &[&[&str]] = &[
        &["host", "--backend", "normal", "status"],
        &["host", "--cookie-file", "cookie", "configure"],
    ];
    for (args, usage) in removed
        .iter()
        .map(|args| {
            (
                args,
                &["unexpected argument", "unrecognized subcommand"][..],
            )
        })
        .chain(invalid.iter().map(|args| (args, &["invalid value"][..])))
        .chain(
            conflicting
                .iter()
                .map(|args| (args, &["cannot be used with"][..])),
        )
    {
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
            .env("CONVERSATIONS_BROWSER", &browser)
            .env("CONVERSATIONS_HOST_SETTINGS_FILE", &settings)
            .current_dir(home.path());
        // A parse error returns at once; anything that started running instead
        // is stopped at the deadline, with whatever it launched.
        let output = owned_process::run(&mut command, b"", Duration::from_secs(20))
            .unwrap_or_else(|error| panic!("{args:?} was accepted: {error}"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(
            usage.iter().any(|usage| stderr.contains(usage)),
            "{args:?}: {stderr}"
        );
    }
    assert!(
        !stubs.join("claude.runs").exists(),
        "a removed input launched an agent"
    );
    assert!(
        !stubs.join("browser.runs").exists(),
        "a removed input opened a browser"
    );
    assert!(
        !home
            .path()
            .join(".conversations-host-local/host.sock")
            .exists(),
        "a removed input started a host"
    );
}
