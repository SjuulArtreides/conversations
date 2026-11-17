//! Shared process isolation for tests that exercise local host state.

use std::os::unix::fs::PermissionsExt;

const CHILD: &str = "CONVERSATIONS_ISOLATED_TEST";

/// Run exactly one test with private state and only explicit tool stubs.
/// Returns true only in its child, where the caller runs the test body.
pub(super) fn isolated(name: &str, extra_tools: &[(&str, &str)]) -> bool {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    let home = tempfile::tempdir().unwrap();
    // Test daemons must not race for the machine-global default web port: the
    // web UI lifecycle proof lives in `tests/host_lifecycle.rs` with its own
    // ports. A child can still overwrite this file for its own web settings.
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"web":{"enabled":false}}"#,
    )
    .unwrap();
    let tools = home.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    for (tool, script) in [
        ("claude", "#!/bin/sh\nexit 0\n"),
        ("codex", "#!/bin/sh\nexit 0\n"),
        ("opencode", "#!/bin/sh\nexit 0\n"),
        ("ps", "#!/bin/sh\nexit 0\n"),
    ]
    .into_iter()
    .chain(extra_tools.iter().copied())
    {
        let path = tools.join(tool);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.env_clear();
    // Explicit test-only inputs, never personal agent/configuration variables.
    for key in [
        "CONVERSATIONS_TEST_NODE",
        "REGEN_HOST_FIXTURES",
        "STREAM_COST_OUT",
        "RUST_BACKTRACE",
    ] {
        if let Some(value) = std::env::var_os(key) {
            child.env(key, value);
        }
    }
    let output = child
        .args([
            "--exact",
            name,
            "--nocapture",
            "--include-ignored",
            "--test-threads=1",
        ])
        .env(CHILD, name)
        .env("PATH", &tools)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TMPDIR", home.path())
        .env(
            "CONVERSATIONS_HOST_SETTINGS_FILE",
            home.path().join("settings.json"),
        )
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child did not pass {name}:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[test]
#[should_panic(expected = "child did not pass")]
fn a_missing_child_test_cannot_report_success() {
    isolated("this_test_does_not_exist", &[]);
}
