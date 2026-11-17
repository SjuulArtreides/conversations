use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

type Environment = HashMap<OsString, OsString>;
const FALLBACK_DIAGNOSTIC: &str = "CONVERSATIONS_OPENCODE_BRIDGE: Automatic naming is unavailable. Check the OpenCode version, inherited terminal config and host runtime permissions, then reopen the session.";
const HANDOFF_ENV: &str = crate::commands::live::interactive::OPENCODE_HANDOFF_ENV;
const HANDOFF_CLAIM: &str = "handoff.claim";
const HANDOFF_RESULT: &str = "handoff-result.json";
const HANDOFF_NEEDS_V2: &str =
    "Choosing a model needs OpenCode 2. Update OpenCode on this computer, then retry with a new key.";

fn write_handoff_result(dir: &Path, id: &str, result: &Value) -> Result<()> {
    uuid::Uuid::parse_str(id)?;
    private_dir(&dir.join("opencode"))?;
    let launch_dir = dir.join("opencode").join(id);
    private_dir(&launch_dir)?;
    write_private(&launch_dir.join(HANDOFF_RESULT), &result.to_string())
}

/// Take the one-shot handoff claim for the host, so a plugin that loads later
/// does nothing. False means the plugin already holds it and may still prompt.
pub(super) fn claim_handoff_for_host(dir: &Path, id: &str) -> Result<bool> {
    uuid::Uuid::parse_str(id)?;
    private_dir(&dir.join("opencode"))?;
    let launch_dir = dir.join("opencode").join(id);
    private_dir(&launch_dir)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(launch_dir.join(HANDOFF_CLAIM))
    {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// A model handoff the plugin could not complete. `session_exists` means the child
/// terminal already holds a session, so it is alive and keeps its checkpoint.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PluginFailure {
    pub message: String,
    pub session_exists: bool,
}

/// The plugin's verdict on a model handoff: `None` until it has written one.
/// Reading consumes it, so a later launch with the same ID never sees it.
pub(super) fn take_handoff_result(
    dir: &Path,
    id: &str,
) -> Result<Option<std::result::Result<(), PluginFailure>>> {
    uuid::Uuid::parse_str(id)?;
    let file = dir.join("opencode").join(id).join(HANDOFF_RESULT);
    let mut bytes = Vec::new();
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&file)
    {
        Ok(handle) => handle.take(4 * 1024 + 1).read_to_end(&mut bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let _ = std::fs::remove_file(&file);
    ensure!(
        bytes.len() <= 4 * 1024,
        "OpenCode handoff result is too large"
    );
    let value: Value = serde_json::from_slice(&bytes).context("invalid OpenCode handoff result")?;
    let session_exists = value
        .get("sessionID")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    Ok(Some(match value.get("error").and_then(Value::as_str) {
        Some(error) => Err(PluginFailure {
            message: error.chars().take(500).collect(),
            session_exists,
        }),
        None if value.get("ok") == Some(&Value::Bool(true)) => Ok(()),
        None => Err(PluginFailure {
            message: "OpenCode reported an unreadable handoff result".into(),
            session_exists,
        }),
    }))
}

#[derive(Debug, Deserialize)]
pub(super) struct Report {
    #[serde(rename = "launchID")]
    launch_id: String,
    #[serde(rename = "nativeID")]
    pub native_id: String,
    pub title: Option<String>,
}

pub(super) fn readiness(
    dir: &Path,
    id: &str,
    pid: Option<u32>,
) -> Result<conversations_core::private_channel::AgentReadiness> {
    let bound = report(dir, id, None)?.context("native identity unavailable")?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("opencode").join(id).join("readiness.json"))?;
    let observed: Value = serde_json::from_reader(file.take(16 * 1024))?;
    ensure!(
        observed["nativeID"] == bound.native_id
            && observed["wrapperPID"].as_u64() == pid.map(u64::from)
            && pid.is_some(),
        "lifecycle identity mismatch"
    );
    let now = chrono::Utc::now().timestamp_millis();
    let at = observed["observedAt"]
        .as_i64()
        .context("missing lifecycle observation time")?;
    ensure!(
        (0..5000).contains(&(now - at)),
        "lifecycle evidence expired"
    );
    Ok(serde_json::from_value(observed["readiness"].clone())?)
}

pub(super) fn report(dir: &Path, id: &str, known: Option<&str>) -> Result<Option<Report>> {
    uuid::Uuid::parse_str(id)?;
    let file = dir.join("opencode").join(id).join("report.json");
    let mut bytes = Vec::new();
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(file)
    {
        Ok(file) => file.take(16 * 1024 + 1).read_to_end(&mut bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        bytes.len() <= 16 * 1024,
        "OpenCode identity report is too large"
    );
    let report: Report =
        serde_json::from_slice(&bytes).context("invalid OpenCode identity report")?;
    if report.launch_id != id
        || report.native_id.trim().is_empty()
        || report.native_id.len() > 256
        || known.is_some_and(|known| known != report.native_id)
    {
        return Ok(None);
    }
    Ok(Some(report))
}

// Runs inside the wrapper's child, after the login shell established PATH.
pub(super) fn command(dir: &Path, id: &str, argv: Vec<String>) -> Result<Vec<String>> {
    let (_, words) = split_environment(&argv);
    if !supported_command(words) {
        return Ok(argv);
    }
    uuid::Uuid::parse_str(id)?;
    let mut wrapped = vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "host".into(),
        "--runtime-dir".into(),
        dir.to_string_lossy().into_owned(),
        "opencode-bridge".into(),
        "--launch-id".into(),
        id.into(),
        "--".into(),
    ];
    wrapped.extend(argv);
    Ok(wrapped)
}

pub(super) fn exec(dir: &Path, id: &str, argv: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let (overrides, words) = split_environment(argv);
    ensure!(!words.is_empty(), "missing OpenCode command");
    let mut environment: Environment = std::env::vars_os().collect();
    environment.extend(overrides);
    let home = environment
        .get(std::ffi::OsStr::new("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default();
    let (mut child, diagnostic) = prepare_or_baseline(dir, id, words, &environment, &home);
    if let Some(diagnostic) = diagnostic {
        eprintln!("{diagnostic}");
    }
    Err(child.exec()).context("starting OpenCode; check that the launcher is executable")
}

fn prepare_or_baseline(
    dir: &Path,
    id: &str,
    words: &[String],
    environment: &Environment,
    home: &Path,
) -> (Command, Option<&'static str>) {
    let mut environment = environment.clone();
    // Only the plugin may act on the payload. It still reaches OpenCode's environment
    // inside the plugin options; the one-shot claim stops any nested OpenCode from acting.
    let handoff = environment.remove(std::ffi::OsStr::new(HANDOFF_ENV));
    // This runs after the terminal's login shell, including for raw Start/Wrap
    // and for baseline launches when optional identity integration fails.
    let policy_valid = match crate::commands::live::interactive::managed_opencode_cli_config(
        environment
            .get(std::ffi::OsStr::new("OPENCODE_CLI_CONFIG_CONTENT"))
            .cloned(),
    ) {
        Ok(config) => {
            environment.insert("OPENCODE_CLI_CONFIG_CONTENT".into(), config.into());
            true
        }
        Err(_) => false,
    };
    let prepared = (|| -> Result<Command> {
        uuid::Uuid::parse_str(id)?;
        match std::fs::remove_file(dir.join("opencode").join(id).join("report.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let _ = std::fs::remove_file(dir.join("opencode").join(id).join("readiness.json"));
        ensure!(policy_valid, "invalid managed CLI configuration");
        let handoff = handoff
            .as_ref()
            .map(|payload| -> Result<Value> {
                Ok(serde_json::from_str(
                    payload.to_str().context("handoff payload is not UTF-8")?,
                )?)
            })
            .transpose()?;
        ensure!(
            !home.as_os_str().is_empty()
                && environment
                    .get(std::ffi::OsStr::new("HOME"))
                    .is_some_and(|home| !home.is_empty()),
            "HOME is missing"
        );
        prepare(dir, id, words, &environment, home, handoff)
    })();
    match prepared {
        Ok(child) => (child, None),
        Err(error) => {
            if handoff.is_some() {
                // The plugin will not run, so report now instead of leaving the
                // host waiting for a prompt that never comes.
                let message = if error.to_string() == HANDOFF_NEEDS_V2 {
                    HANDOFF_NEEDS_V2
                } else {
                    "OpenCode could not load the Conversations plugin, so the requested model could not be applied. Check the OpenCode version and the host runtime directory permissions, then retry with a new key."
                };
                let _ = write_handoff_result(dir, id, &json!({"error": message}));
            }
            // Keep the user's original configuration and launch usable on failure.
            // Parser diagnostics can contain config values, so never print them.
            let mut child = Command::new(&words[0]);
            child.args(&words[1..]).env_clear().envs(&environment);
            let diagnostic = if error.to_string() == "managed resume with --fork is unsupported" {
                "CONVERSATIONS_OPENCODE_BRIDGE: Managed resume with --fork is unsupported. Start another conversation in Conversations."
            } else {
                FALLBACK_DIAGNOSTIC
            };
            (child, Some(diagnostic))
        }
    }
}

fn split_environment(argv: &[String]) -> (Environment, &[String]) {
    let mut environment = Environment::new();
    let mut offset = 0;
    if argv
        .first()
        .is_some_and(|word| word == "/usr/bin/env" || word == "env")
    {
        offset = 1;
        while let Some((key, value)) = argv.get(offset).and_then(|word| word.split_once('=')) {
            environment.insert(key.into(), value.into());
            offset += 1;
        }
    }
    (environment, &argv[offset..])
}

fn supported_command(words: &[String]) -> bool {
    match words
        .first()
        .and_then(|word| Path::new(word).file_name())
        .and_then(|s| s.to_str())
    {
        Some("opencode") => true,
        Some("adk") => words.get(1).is_some_and(|word| word == "code"),
        _ => false,
    }
}

fn session_target(words: &[String], major: u32) -> Result<Option<String>> {
    let adk = words
        .first()
        .and_then(|word| Path::new(word).file_name())
        .is_some_and(|name| name == "adk");
    // adk consumes its forwarding separator; a later separator belongs to the
    // native CLI. Direct OpenCode's first separator is already native.
    let start = if adk {
        words
            .iter()
            .position(|word| word == "--")
            .map_or(2, |index| index + 1)
    } else {
        1
    };
    let mut target = None;
    let mut fork = None;
    let boolean_literal = |value: &str| match value {
        "true" => Some(true),
        "false" => Some(false),
        "yes" | "on" | "1" | "y" if major == 2 => Some(true),
        "no" | "off" | "0" | "n" if major == 2 => Some(false),
        _ => None,
    };
    let mut index = start;
    while let Some(word) = words.get(index) {
        index += 1;
        if word == "--" {
            break;
        }
        // Only inspect identity options, not the contents of native value args.
        if matches!(
            word.as_str(),
            "--prompt"
                | "--model"
                | "-m"
                | "--agent"
                | "--server"
                | "--log-level"
                | "--port"
                | "--hostname"
                | "--mdns-domain"
                | "--cors"
                | "--replay-limit"
        ) {
            if words.get(index).is_some_and(|value| value == "--") {
                break;
            }
            index += usize::from(words.get(index).is_some());
            continue;
        }
        let assignment = if word == "--no-fork" {
            Some(false)
        } else if word == "--fork" {
            Some(
                match words.get(index).and_then(|value| boolean_literal(value)) {
                    Some(value) => {
                        index += 1;
                        value
                    }
                    None => true,
                },
            )
        } else if let Some(value) = word.strip_prefix("--fork=") {
            Some(if major == 1 {
                value == "true"
            } else {
                boolean_literal(value).context("invalid fork boolean")?
            })
        } else {
            None
        };
        if let Some(value) = assignment {
            // V1.18 yargs overwrites booleans; V2.0.27 Effect Param.parseFlag
            // reads the first occurrence. Neither uses string truthiness.
            if major == 1 || fork.is_none() {
                fork = Some(value);
            }
            continue;
        }
        let value = if word == "--session" || word == "-s" {
            Some(
                words
                    .get(index)
                    .context("session flag requires a value")?
                    .as_str(),
            )
        } else {
            word.strip_prefix("--session=")
                .or_else(|| word.strip_prefix("-s="))
        };
        if let Some(value) = value {
            ensure!(
                !value.is_empty() && !value.starts_with('-'),
                "session flag requires a value"
            );
            ensure!(
                target.as_deref().is_none_or(|existing| existing == value),
                "conflicting session flags"
            );
            target = Some(value.to_owned());
            if word == "--session" || word == "-s" {
                index += 1;
            }
        }
    }
    ensure!(
        target.is_none() || fork != Some(true),
        "managed resume with --fork is unsupported"
    );
    Ok(target)
}

pub(super) fn cleanup_after_exit(dir: &Path, id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)?;
    let launch = dir.join("opencode").join(id);
    for (directory, files) in [
        (launch.clone(), &["tui.json"][..]),
        (
            launch.join("plugin"),
            &[
                "common.js",
                "readiness.js",
                "entry.js",
                "tui.js",
                "package.json",
            ][..],
        ),
        (launch.join("bin"), &["opencode"][..]),
    ] {
        let metadata = match std::fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "bridge cleanup requires an owner-only directory"
        );
        for file in files {
            match std::fs::remove_file(directory.join(file)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if directory != launch {
            // Never sweep unknown files, reports or another launch's state.
            match std::fs::remove_dir(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

/// Resolve an executable using the supplied environment, returning an absolute
/// path so later shell initialization cannot select a different program.
pub(super) fn executable(name: &Path, environment: &Environment) -> Option<PathBuf> {
    let executable = |path: &Path| {
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.components().count() > 1 {
        return executable(name)
            .then(|| std::path::absolute(name).ok())
            .flatten();
    }
    std::env::split_paths(environment.get(std::ffi::OsStr::new("PATH"))?)
        .map(|dir| dir.join(name))
        .find(|path| executable(path))
        .and_then(|path| std::path::absolute(path).ok())
}

fn private_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(error)
            }
        })?;
    let metadata = std::fs::symlink_metadata(dir)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "host bridge directory must be owner-only"
    );
    Ok(())
}

fn write_private(file: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let temporary = file.with_extension(uuid::Uuid::new_v4().to_string());
    let result = (|| -> Result<()> {
        let mut handle = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        handle.write_all(contents.as_bytes())?;
        std::fs::rename(&temporary, file)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

fn prepare(
    dir: &Path,
    id: &str,
    words: &[String],
    environment: &Environment,
    home: &Path,
    handoff: Option<Value>,
) -> Result<Command> {
    uuid::Uuid::parse_str(id)?;
    ensure!(supported_command(words), "unsupported OpenCode launcher");
    let adk = Path::new(&words[0])
        .file_name()
        .is_some_and(|name| name == "adk");
    let binary = if adk {
        executable(Path::new("opencode"), environment)
            .or_else(|| executable(&home.join(".opencode/bin/opencode"), environment))
    } else {
        executable(Path::new(&words[0]), environment)
    }
    .context("OpenCode is not installed")?;
    let major = probe_version(&binary, environment)?;
    let target = session_target(words, major)?;
    private_dir(&dir.join("opencode"))?;
    let launch_dir = dir.join("opencode").join(id);
    private_dir(&launch_dir)?;
    ensure!(handoff.is_none() || major == 2, HANDOFF_NEEDS_V2);
    let mut options = json!({
        "launchID": id,
        "reportPath": launch_dir.join("report.json"),
        "readinessPath": launch_dir.join("readiness.json"),
        "wrapperPID": environment.get(std::ffi::OsStr::new("CONVERSATIONS_WRAPPER_PID")).and_then(|pid| pid.to_str()).and_then(|pid| pid.parse::<u32>().ok()),
        "knownID": target,
    });
    if let Some(mut handoff) = handoff {
        for stale in [HANDOFF_CLAIM, HANDOFF_RESULT] {
            match std::fs::remove_file(launch_dir.join(stale)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        handoff["claimPath"] = json!(launch_dir.join(HANDOFF_CLAIM));
        handoff["resultPath"] = json!(launch_dir.join(HANDOFF_RESULT));
        options["handoff"] = handoff;
    }
    let mut env = environment.clone();
    let plugin = launch_dir.join("plugin");
    private_dir(&plugin)?;
    write_private(
        &plugin.join("common.js"),
        include_str!("opencode_bridge/common.js"),
    )?;
    write_private(
        &plugin.join("readiness.js"),
        include_str!("opencode_bridge/readiness.js"),
    )?;
    write_private(
        &plugin.join(if major == 1 { "entry.js" } else { "tui.js" }),
        if major == 1 {
            include_str!("opencode_bridge/v1.js")
        } else {
            include_str!("opencode_bridge/v2.js")
        },
    )?;
    write_private(
        &plugin.join("package.json"),
        &json!({"name":"conversations-identity","type":"module","exports":{"./tui":if major == 1 { "./entry.js" } else { "./tui.js" }}}).to_string(),
    )?;
    if major == 1 {
        let mut config = if let Some(source) = env.get(std::ffi::OsStr::new("OPENCODE_TUI_CONFIG"))
        {
            let source = std::path::absolute(source)?;
            relocate_v1(
                &std::fs::read_to_string(&source)?,
                source.parent().context("missing config parent")?,
                home,
                &env,
            )?
        } else {
            json!({})
        };
        let plugins = config
            .as_object_mut()
            .context("TUI config must be an object")?
            .entry("plugin")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("TUI plugin must be an array")?;
        plugins.push(json!([plugin, options]));
        let config_file = launch_dir.join("tui.json");
        write_private(&config_file, &config.to_string())?;
        env.insert("OPENCODE_TUI_CONFIG".into(), config_file.into_os_string());
    } else {
        let global = env
            .get(std::ffi::OsStr::new("XDG_CONFIG_HOME"))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("opencode/cli.json");
        let inherited = env
            .get(std::ffi::OsStr::new("OPENCODE_CLI_CONFIG_CONTENT"))
            .map(|s| s.to_str().context("inline CLI config must be UTF-8"))
            .transpose()?;
        let global_text = match std::fs::read_to_string(&global) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".into(),
            Err(error) => return Err(error.into()),
        };
        env.insert(
            "OPENCODE_CLI_CONFIG_CONTENT".into(),
            v2_config(
                inherited,
                &global_text,
                global.parent().unwrap(),
                &plugin,
                options,
            )?
            .into(),
        );
    }
    let mut child = if adk {
        // adk uses shutil.which before its ~/.opencode fallback. Pin that lookup
        // to the executable whose major we just inspected, even off PATH.
        let bin = launch_dir.join("bin");
        private_dir(&bin)?;
        let pinned = bin.join("opencode");
        let _ = std::fs::remove_file(&pinned);
        std::os::unix::fs::symlink(&binary, &pinned)?;
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            env.get(std::ffi::OsStr::new("PATH"))
                .unwrap_or(&OsString::new()),
        ));
        env.insert("PATH".into(), std::env::join_paths(paths)?);
        Command::new(executable(Path::new(&words[0]), environment).context("adk is not installed")?)
    } else {
        Command::new(binary)
    };
    child.args(&words[1..]).env_clear().envs(env);
    Ok(child)
}

fn probe_version(binary: &Path, environment: &Environment) -> Result<u32> {
    probe_version_with_deadline(binary, environment, std::time::Duration::from_secs(5))
}

fn probe_version_with_deadline(
    binary: &Path,
    environment: &Environment,
    timeout: std::time::Duration,
) -> Result<u32> {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    let mut child = Command::new(binary)
        .arg("--version")
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("cannot read OpenCode version")?;
    let result = (|| -> Result<u32> {
        let mut stdout = child.stdout.take().context("missing probe stdout")?;
        let mut stderr = child.stderr.take().context("missing probe stderr")?;
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // SAFETY: these descriptors belong to the child pipes retained above.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            ensure!(
                flags >= 0
                    && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
                "cannot bound version probe pipes"
            );
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let out_done = read_probe(&mut stdout, &mut out, 128)?;
            let err_done = read_probe(&mut stderr, &mut err, 2048)?;
            if let Some(status) = child.try_wait()? {
                if out_done && err_done {
                    ensure!(status.success(), "version probe failed");
                    return version_major(std::str::from_utf8(&out)?);
                }
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "version probe timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        // Only this probe's private process group, never an OpenCode service/TUI.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn read_probe(reader: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> Result<bool> {
    let mut buffer = [0; 256];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                ensure!(bytes.len() <= limit, "version probe output exceeded limit");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn parse(text: &str) -> Result<Value> {
    jsonc_parser::parse_to_serde_value(text, &Default::default())?
        .context("terminal config must contain an object")
}

fn version_major(output: &str) -> Result<u32> {
    ensure!(output.len() <= 64, "invalid OpenCode version output");
    let version = output.trim();
    let version = version.strip_prefix("opencode ").unwrap_or(version);
    let version = version.strip_prefix('v').unwrap_or(version);
    let parts: Vec<_> = version.split('.').collect();
    ensure!(
        parts.len() == 3
            && parts.iter().all(|part| {
                !part.is_empty()
                    && part.bytes().all(|byte| byte.is_ascii_digit())
                    && (part.len() == 1 || !part.starts_with('0'))
            }),
        "invalid OpenCode version output"
    );
    let numbers = parts
        .into_iter()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let major = numbers[0];
    ensure!(
        major == 1 || major == 2,
        "unsupported OpenCode major version"
    );
    ensure!(
        major != 1 || numbers[1] >= 18,
        "OpenCode V1 terminal bridge requires 1.18 or later"
    );
    Ok(major)
}

fn tokens(
    text: &str,
    prefix: &str,
    mut replace: impl FnMut(&str) -> Result<String>,
) -> Result<String> {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(prefix) {
        let Some(end) = rest[start + prefix.len()..].find('}') else {
            break;
        };
        let end = start + prefix.len() + end;
        result.push_str(&rest[..start]);
        result.push_str(&replace(&rest[start + prefix.len()..end])?);
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

fn source_path(value: &str, base: &Path, home: &Path) -> PathBuf {
    let path = if let Some(value) = value.strip_prefix("~/") {
        home.join(value)
    } else if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        base.join(value)
    };
    lexical_path(&path)
}

fn lexical_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            _ => result.push(component.as_os_str()),
        }
    }
    result
}

fn relocate_v1(text: &str, base: &Path, home: &Path, env: &Environment) -> Result<Value> {
    // Match the native order: one environment pass, then one file pass. Leave
    // generic file values to the native loader, rebasing only their source.
    let expanded = tokens(text, "{env:", |key| {
        Ok(env
            .get(std::ffi::OsStr::new(key))
            .map(|s| s.to_str().context("config environment value must be UTF-8"))
            .transpose()?
            .unwrap_or("")
            .to_owned())
    })?;
    ensure!(
        !expanded.contains("{env:"),
        "nested environment substitutions are unsupported"
    );
    let mut config = parse(&expanded)?;
    fn rebase(value: &mut Value, base: &Path, home: &Path) -> Result<()> {
        match value {
            Value::String(text) => {
                *text = tokens(text, "{file:", |value| {
                    Ok(format!(
                        "{{file:{}}}",
                        source_path(value, base, home).display()
                    ))
                })?
            }
            Value::Array(items) => {
                for item in items {
                    rebase(item, base, home)?;
                }
            }
            Value::Object(items) => {
                ensure!(
                    !items.keys().any(|key| key.contains("{file:")),
                    "file substitutions in config keys are unsupported"
                );
                for item in items.values_mut() {
                    rebase(item, base, home)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    rebase(&mut config, base, home)?;
    let object = config
        .as_object_mut()
        .context("TUI config must be an object")?;
    if let Some(Value::Object(mut nested)) = object.remove("tui") {
        nested.extend(object.clone());
        *object = nested;
    }
    if let Some(plugins) = object.get_mut("plugin") {
        for entry in plugins
            .as_array_mut()
            .context("TUI plugin must be an array")?
        {
            let spec = if entry.is_array() {
                &mut entry[0]
            } else {
                entry
            };
            let text = spec.as_str().context("plugin spec must be a string")?;
            let expanded = expand_files(text)?;
            *spec = Value::String(plugin_path(&expanded, base));
        }
    }
    if let Some(sounds) = object
        .get_mut("attention")
        .and_then(|v| v.get_mut("sounds"))
    {
        for value in sounds
            .as_object_mut()
            .context("sounds must be an object")?
            .values_mut()
        {
            let text = expand_files(value.as_str().context("sound path must be a string")?)?;
            let path = if text.starts_with("file://") {
                reqwest::Url::parse(&text)?
                    .to_file_path()
                    .map_err(|_| anyhow::anyhow!("invalid sound file URL"))?
            } else {
                PathBuf::from(&text)
            };
            *value = Value::String(if text.is_empty() {
                text
            } else if path.is_absolute() {
                path.to_string_lossy().into_owned()
            } else {
                lexical_path(&base.join(path))
                    .to_string_lossy()
                    .into_owned()
            });
        }
    }
    Ok(config)
}

fn expand_files(text: &str) -> Result<String> {
    let expanded = tokens(text, "{file:", |path| {
        Ok(std::fs::read_to_string(path)
            .unwrap_or_default()
            .trim()
            .to_owned())
    })?;
    ensure!(
        !expanded.contains("{env:") && !expanded.contains("{file:"),
        "nested path substitutions are unsupported"
    );
    Ok(expanded)
}

fn plugin_path(text: &str, base: &Path) -> String {
    if text.starts_with('.') {
        lexical_path(&base.join(text))
            .to_string_lossy()
            .into_owned()
    } else {
        text.to_owned()
    }
}

fn v2_config(
    inherited: Option<&str>,
    global: &str,
    base: &Path,
    plugin: &Path,
    options: Value,
) -> Result<String> {
    let mut config: Value = serde_json::from_str(inherited.unwrap_or("{}"))?;
    let object = config
        .as_object_mut()
        .context("inline CLI config must be an object")?;
    if !object.contains_key("plugins") {
        let mut global: Value = serde_json::from_str(global)?;
        if let Some(plugins) = global.get_mut("plugins") {
            // Moved global path declarations retain their original source base.
            for entry in plugins
                .as_array_mut()
                .context("global plugins must be an array")?
            {
                let spec = if entry.is_object() {
                    &mut entry["package"]
                } else {
                    entry
                };
                if let Some(text) = spec.as_str() {
                    *spec = Value::String(plugin_path(text, base));
                }
            }
            object.insert("plugins".into(), plugins.clone());
        }
    }
    let plugins = object
        .entry("plugins")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("plugins must be an array")?;
    plugins.push(json!({"package": plugin, "options": options}));
    Ok(config.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_equals_and_short_flags_preserve_authority_and_reject_missing_values() {
        for prefix in [vec!["opencode"], vec!["adk", "code", "--"]] {
            for (suffix, expected) in [
                (vec!["--session=saved", "--fork=false"], Some("saved")),
                (vec!["--session=saved", "--fork", "false"], Some("saved")),
                (
                    vec!["--session=saved", "--fork", "--no-fork"],
                    Some("saved"),
                ),
                (
                    vec!["--session=saved", "--fork=true", "--fork=false"],
                    Some("saved"),
                ),
                (vec!["--", "--session=literal"], None),
                (vec!["--prompt", "--session=literal"], None),
                (vec!["--prompt", "--", "--session=literal"], None),
                (vec!["--model", "--session=literal"], None),
                (vec!["--prompt", "--fork", "--session=saved"], Some("saved")),
                (vec!["--session=saved", "--", "--fork"], Some("saved")),
            ] {
                let words: Vec<_> = prefix
                    .iter()
                    .chain(&suffix)
                    .map(|word| (*word).to_owned())
                    .collect();
                assert_eq!(
                    session_target(&words, 1).unwrap(),
                    expected.map(str::to_owned),
                    "{words:?}"
                );
            }
            for suffix in [
                vec!["--session=saved", "--fork=true"],
                vec!["--session=saved", "--no-fork", "--fork"],
                vec!["--session=saved", "--fork", "true"],
            ] {
                let words: Vec<_> = prefix
                    .iter()
                    .chain(&suffix)
                    .map(|word| (*word).to_owned())
                    .collect();
                assert_eq!(
                    session_target(&words, 1).unwrap_err().to_string(),
                    "managed resume with --fork is unsupported"
                );
            }
        }
        assert_eq!(
            session_target(
                &[
                    "opencode".into(),
                    "--prompt".into(),
                    "Handoff brief contains --session=literal, not a session flag".into()
                ],
                1
            )
            .unwrap(),
            None
        );
        for args in [
            vec!["opencode", "--session=saved"],
            vec!["opencode", "--session", "saved"],
            vec!["opencode", "-s", "saved"],
            vec!["adk", "code", "--", "-s=saved"],
        ] {
            assert_eq!(
                session_target(&args.into_iter().map(str::to_owned).collect::<Vec<_>>(), 1)
                    .unwrap()
                    .as_deref(),
                Some("saved")
            );
        }
        for args in [
            vec!["opencode", "--session"],
            vec!["opencode", "--session="],
            vec!["opencode", "--session", "--auto"],
            vec!["opencode", "--session=saved", "--fork"],
        ] {
            assert!(
                session_target(&args.into_iter().map(str::to_owned).collect::<Vec<_>>(), 1)
                    .is_err()
            );
        }
    }

    #[test]
    fn v2_fork_boolean_uses_effect_literals_and_first_occurrence() {
        for (flags, forks) in [
            (vec!["--fork=false", "--fork=true"], false),
            (vec!["--fork=true", "--fork=false"], true),
            (vec!["--no-fork", "--fork"], false),
            (vec!["--fork", "--no-fork"], true),
            (vec!["--fork=yes"], true),
            (vec!["--fork", "on"], true),
            (vec!["--fork=off"], false),
        ] {
            let words: Vec<_> = ["opencode", "mini", "--session=saved"]
                .into_iter()
                .chain(flags)
                .map(str::to_owned)
                .collect();
            let result = session_target(&words, 2);
            if forks {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "managed resume with --fork is unsupported",
                    "{words:?}"
                );
            } else {
                assert_eq!(result.unwrap().as_deref(), Some("saved"), "{words:?}");
            }
        }
    }

    #[test]
    fn confirmed_exit_cleanup_removes_only_generated_assets_and_keeps_report() {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        private_dir(&root.path().join("opencode")).unwrap();
        let launch = root.path().join("opencode").join(&id);
        private_dir(&launch).unwrap();
        private_dir(&launch.join("plugin")).unwrap();
        private_dir(&launch.join("bin")).unwrap();
        for file in [
            "plugin/common.js",
            "plugin/entry.js",
            "plugin/tui.js",
            "plugin/package.json",
            "tui.json",
            "bin/opencode",
            "report.json",
            "unrelated",
        ] {
            write_private(&launch.join(file), "private fixture").unwrap();
        }
        cleanup_after_exit(root.path(), &id).unwrap();
        assert!(!launch.join("plugin").exists());
        assert!(!launch.join("bin").exists());
        assert!(!launch.join("tui.json").exists());
        assert_eq!(
            std::fs::read_to_string(launch.join("report.json")).unwrap(),
            "private fixture"
        );
        assert!(launch.join("unrelated").exists());
        cleanup_after_exit(root.path(), &id).unwrap();
    }

    #[test]
    fn version_probe_deadline_includes_descendants_holding_pipes_after_exit() {
        for script in [
            "/bin/sleep 3 &\nprintf 'opencode v2.0.27\\n'\nexit 0",
            "/bin/sleep 3\nprintf 'opencode v2.0.27\\n'",
        ] {
            let root = tempfile::tempdir().unwrap();
            let binary = root.path().join("opencode");
            // The shell exits successfully, but a descendant retains both pipes.
            // It self-terminates after 3s so even the expected-red test is bounded.
            std::fs::write(&binary, format!("#!/bin/sh\nif [ \"$1\" != --version ]; then printf baseline; exit 0; fi\n{script}\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let start = std::time::Instant::now();
            let env = Environment::from([("HOME".into(), root.path().as_os_str().into())]);
            assert!(probe_version_with_deadline(
                &binary,
                &env,
                std::time::Duration::from_millis(200)
            )
            .unwrap_err()
            .to_string()
            .contains("timed out"));
            assert!(start.elapsed() >= std::time::Duration::from_millis(150));
            assert!(start.elapsed() < std::time::Duration::from_millis(2600));
            assert_eq!(Command::new(&binary).output().unwrap().stdout, b"baseline");
        }
    }

    #[test]
    fn fallback_clears_stale_reports_and_applies_child_tabs_policy_without_home() {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let launch = root.path().join("opencode").join(&id);
        std::fs::create_dir_all(&launch).unwrap();
        std::fs::write(
            launch.join("report.json"),
            json!({"launchID":id,"nativeID":"stale","title":"Wrong subject"}).to_string(),
        )
        .unwrap();
        let words = vec!["opencode".into()];
        let env = Environment::from([(
            "OPENCODE_CLI_CONFIG_CONTENT".into(),
            r#"{"tabs":{"mode":"on"},"mouse":false,"plugins":["shell-plugin"]}"#.into(),
        )]);
        let (child, diagnostic) = prepare_or_baseline(root.path(), &id, &words, &env, root.path());
        assert!(diagnostic.is_some());
        assert!(
            report(root.path(), &id, None).unwrap().is_none(),
            "a failed new launch must not inherit an old report"
        );
        let config: Value = serde_json::from_str(
            child
                .get_envs()
                .find(|(key, _)| *key == "OPENCODE_CLI_CONFIG_CONTENT")
                .unwrap()
                .1
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(config["tabs"]["mode"], "off");
        assert_eq!(config["mouse"], false);
        assert_eq!(config["plugins"][0], "shell-plugin");
        let malformed =
            Environment::from([("OPENCODE_CLI_CONFIG_CONTENT".into(), "{broken".into())]);
        let (child, _) = prepare_or_baseline(root.path(), &id, &words, &malformed, root.path());
        assert_eq!(
            child
                .get_envs()
                .find(|(key, _)| *key == "OPENCODE_CLI_CONFIG_CONTENT")
                .unwrap()
                .1
                .unwrap(),
            "{broken"
        );
    }

    #[test]
    fn version_probe_stops_excess_output_before_the_probe_can_finish() {
        for redirect in ["", ">&2"] {
            let root = tempfile::tempdir().unwrap();
            let binary = root.path().join("opencode");
            let marker = root.path().join("completed");
            std::fs::write(&binary, format!("#!/bin/sh\nif [ \"$1\" != --version ]; then printf baseline; exit 0; fi\ni=0; while [ $i -lt 4096 ]; do printf x {redirect}; i=$((i+1)); done\n/bin/sleep 3\necho finished > '{}'\n", marker.display())).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let env = Environment::from([("HOME".into(), root.path().as_os_str().into())]);
            let start = std::time::Instant::now();
            assert!(probe_version(&binary, &env)
                .unwrap_err()
                .to_string()
                .contains("exceeded limit"));
            assert!(
                start.elapsed() < std::time::Duration::from_millis(2600),
                "probe output must be bounded before collecting it"
            );
            assert!(!marker.exists());
            assert_eq!(Command::new(&binary).output().unwrap().stdout, b"baseline");
        }
    }

    #[test]
    fn relocated_v1_sounds_preserve_native_literal_tilde_and_home_file_references() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("config");
        let home = root.path().join("user");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join("sound-ref.txt"), "../from-ref.wav\n").unwrap();
        let absolute = root.path().join("absolute.wav");
        let url_path = root.path().join("space in name.wav");
        let config = json!({"attention":{"sounds":{
            "done":"~/sounds/done.wav",
            "error":"./sounds/error.wav",
            "question":"../question.wav",
            "default":absolute,
            "permission":reqwest::Url::from_file_path(&url_path).unwrap().as_str(),
            "subagent_done":"{file:~/sound-ref.txt}",
        }}});
        let relocated =
            relocate_v1(&config.to_string(), &base, &home, &Environment::new()).unwrap();
        let sounds = &relocated["attention"]["sounds"];
        assert_eq!(
            sounds["done"],
            base.join("~/sounds/done.wav").to_str().unwrap()
        );
        assert_eq!(
            sounds["error"],
            base.join("sounds/error.wav").to_str().unwrap()
        );
        assert_eq!(
            sounds["question"],
            root.path().join("question.wav").to_str().unwrap()
        );
        assert_eq!(sounds["default"], absolute.to_str().unwrap());
        assert_eq!(sounds["permission"], url_path.to_str().unwrap());
        assert_eq!(
            sounds["subagent_done"],
            root.path().join("from-ref.wav").to_str().unwrap()
        );
    }

    #[test]
    fn version_output_rejects_unknown_or_malformed_values() {
        for output in [
            "",
            "2",
            "2.0",
            "2.0.27.1",
            "2.x.27",
            "2.0.27 extra",
            "vv2.0.27",
            "opencode v3.0.0",
            "1.17.9",
            "02.0.27",
            "2.0.4294967296",
            "opencode v2.0.27\nprivate-config",
        ] {
            assert!(
                version_major(output).is_err(),
                "accepted malformed or unsupported version"
            );
        }
        assert!(version_major(&"2".repeat(1000)).is_err());
    }

    #[test]
    fn unsupported_version_retains_baseline_with_bounded_redacted_diagnostic() {
        for output in [
            "opencode v3.0.0 PRIVATE_OUTPUT_DO_NOT_LOG",
            "opencode v2.invalid.27",
            "opencode v2.0",
        ] {
            let root = tempfile::tempdir().unwrap();
            let binary = root.path().join("opencode");
            std::fs::write(&binary, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{output}'; exit 0; fi\nexit 93\n")).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let words = vec![
                binary.to_string_lossy().into_owned(),
                "--session".into(),
                "saved-native".into(),
            ];
            let env = Environment::from([
                ("PRIVATE_CONFIG".into(), "PRIVATE_CONFIG_DO_NOT_LOG".into()),
                ("HOME".into(), root.path().as_os_str().into()),
            ]);
            let (child, diagnostic) = prepare_or_baseline(
                root.path(),
                &uuid::Uuid::new_v4().to_string(),
                &words,
                &env,
                root.path(),
            );
            assert_eq!(child.get_program(), binary.as_os_str());
            assert_eq!(
                child
                    .get_args()
                    .map(|arg| arg.to_str().unwrap())
                    .collect::<Vec<_>>(),
                words[1..]
            );
            assert_eq!(
                child
                    .get_envs()
                    .find(|(key, _)| *key == "PRIVATE_CONFIG")
                    .unwrap()
                    .1
                    .unwrap(),
                "PRIVATE_CONFIG_DO_NOT_LOG"
            );
            let diagnostic =
                diagnostic.expect("fallback must explain that automatic naming is unavailable");
            assert!(diagnostic.starts_with("CONVERSATIONS_OPENCODE_BRIDGE:"));
            assert!(diagnostic.len() <= 256);
            assert!(!diagnostic.contains('\n'));
            assert!(!diagnostic.contains("PRIVATE_") && !diagnostic.contains(output));
            assert!(diagnostic.contains("Check the OpenCode version"));
            assert!(diagnostic.contains("inherited terminal config"));
        }
    }

    #[test]
    fn model_handoff_reaches_only_the_plugin_and_v1_fails_at_once() {
        for (version, adk) in [("2.1.3", false), ("2.1.3", true), ("1.18.34", false)] {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("home");
            let tools = root.path().join("tools");
            std::fs::create_dir_all(&tools).unwrap();
            let binary = tools.join("opencode");
            std::fs::write(
                &binary,
                format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo {version}; exit 0; fi\n"),
            )
            .unwrap();
            std::fs::write(tools.join("adk"), "#!/bin/sh\nexec opencode \"$@\"\n").unwrap();
            for tool in ["opencode", "adk"] {
                std::fs::set_permissions(tools.join(tool), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            let id = uuid::Uuid::new_v4().to_string();
            let launch = root.path().join("opencode").join(&id);
            private_dir(&root.path().join("opencode")).unwrap();
            private_dir(&launch).unwrap();
            std::fs::write(launch.join(HANDOFF_RESULT), r#"{"ok":true}"#).unwrap();
            let payload =
                json!({"model":"anthropic/claude-haiku-4-5","variant":"high","prompt":"Brief"});
            let env = Environment::from([
                ("HOME".into(), home.as_os_str().into()),
                ("PATH".into(), tools.as_os_str().into()),
                ("XDG_CONFIG_HOME".into(), "".into()),
                (HANDOFF_ENV.into(), payload.to_string().into()),
            ]);
            let words: Vec<String> = if adk {
                vec!["adk".into(), "code".into()]
            } else {
                vec![binary.to_string_lossy().into_owned()]
            };
            let (child, diagnostic) = prepare_or_baseline(root.path(), &id, &words, &env, &home);
            assert!(
                child.get_envs().all(|(key, _)| key != HANDOFF_ENV),
                "OpenCode and adk must never see the handoff payload"
            );
            if version.starts_with('1') {
                assert!(diagnostic.is_some());
                assert_eq!(
                    take_handoff_result(root.path(), &id).unwrap(),
                    Some(Err(PluginFailure {
                        message: HANDOFF_NEEDS_V2.into(),
                        session_exists: false
                    }))
                );
                assert_eq!(
                    take_handoff_result(root.path(), &id).unwrap(),
                    None,
                    "reading consumes it"
                );
                continue;
            }
            assert!(diagnostic.is_none());
            assert_eq!(
                take_handoff_result(root.path(), &id).unwrap(),
                None,
                "a stale verdict must not confirm a new launch"
            );
            let config: Value = serde_json::from_str(
                child
                    .get_envs()
                    .find(|(key, _)| *key == "OPENCODE_CLI_CONFIG_CONTENT")
                    .unwrap()
                    .1
                    .unwrap()
                    .to_str()
                    .unwrap(),
            )
            .unwrap();
            let handoff =
                &config["plugins"].as_array().unwrap().last().unwrap()["options"]["handoff"];
            assert_eq!(handoff["model"], "anthropic/claude-haiku-4-5");
            assert_eq!(handoff["variant"], "high");
            assert_eq!(handoff["prompt"], "Brief");
            assert_eq!(handoff["claimPath"], json!(launch.join(HANDOFF_CLAIM)));
            assert_eq!(handoff["resultPath"], json!(launch.join(HANDOFF_RESULT)));
        }
    }

    #[test]
    fn actual_child_binary_selects_v1_v2_for_direct_and_adk_without_starting_sessions() {
        for (version, major) in [
            ("1.18.34", 1),
            ("2.0.27", 2),
            ("v2.0.27", 2),
            ("opencode v2.0.27", 2),
        ] {
            for adk in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let home = root.path().join("home");
                let bin = home.join(".opencode/bin");
                std::fs::create_dir_all(&bin).unwrap();
                let binary = bin.join("opencode");
                std::fs::write(&binary, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo {version}; exit 0; fi\nprintf '%s' \"$OPENCODE_CLI_CONFIG_CONTENT\"\n")).unwrap();
                std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
                let tools = root.path().join("tools");
                std::fs::create_dir(&tools).unwrap();
                // Match adk's real PATH-first, ~/.opencode fallback resolution,
                // including when optional bridge setup leaves PATH unchanged.
                std::fs::write(tools.join("adk"), "#!/bin/sh\nif command -v opencode >/dev/null 2>&1; then exec opencode \"$@\"; fi\nexec \"$HOME/.opencode/bin/opencode\" \"$@\"\n").unwrap();
                std::fs::set_permissions(tools.join("adk"), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                let config_dir = home.join(".config/opencode");
                std::fs::create_dir_all(&config_dir).unwrap();
                std::fs::write(config_dir.join("cli.json"), r#"{"tabs":{"mode":"on"},"plugins":[{"package":"team","options":{"compact":true}},"-opencode.notifications"]}"#).unwrap();
                let env = Environment::from([
                    ("HOME".into(), home.as_os_str().into()),
                    ("PATH".into(), tools.as_os_str().into()),
                    (
                        "OPENCODE_CLI_CONFIG_CONTENT".into(),
                        r#"{"tabs":{"mode":"on"},"mouse":false}"#.into(),
                    ),
                ]);
                let words = if adk {
                    vec![
                        "adk".into(),
                        "code".into(),
                        "--".into(),
                        "--session".into(),
                        "saved-native".into(),
                    ]
                } else {
                    vec![
                        binary.to_string_lossy().into_owned(),
                        "--session".into(),
                        "saved-native".into(),
                    ]
                };
                let id = uuid::Uuid::new_v4().to_string();
                let mut env = env;
                env.insert("XDG_CONFIG_HOME".into(), "".into());
                let (mut child, diagnostic) =
                    prepare_or_baseline(root.path(), &id, &words, &env, &home);
                assert!(diagnostic.is_none());
                let environment: Environment = child
                    .get_envs()
                    .filter_map(|(key, value)| value.map(|value| (key.into(), value.into())))
                    .collect();
                let launch = root.path().join("opencode").join(&id);
                if major == 2 {
                    // v2.0.27 resolves local directories as <directory>/tui,
                    // not through package.json exports (native QA verified).
                    assert_eq!(
                        std::fs::read_to_string(launch.join("plugin/tui.js")).unwrap(),
                        include_str!("opencode_bridge/v2.js")
                    );
                }
                assert_eq!(
                    std::fs::metadata(&launch).unwrap().permissions().mode() & 0o777,
                    0o700
                );
                assert_eq!(
                    std::fs::metadata(launch.join(if major == 1 {
                        "plugin/entry.js"
                    } else {
                        "plugin/tui.js"
                    }))
                    .unwrap()
                    .permissions()
                    .mode()
                        & 0o777,
                    0o600
                );
                let (config, options) = if major == 1 {
                    let file = environment
                        .get(std::ffi::OsStr::new("OPENCODE_TUI_CONFIG"))
                        .unwrap();
                    assert!(Path::new(file).starts_with(&launch));
                    let config: Value =
                        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
                    let options = config["plugin"][0][1].clone();
                    (config, options)
                } else {
                    let config: Value = serde_json::from_str(
                        environment
                            .get(std::ffi::OsStr::new("OPENCODE_CLI_CONFIG_CONTENT"))
                            .unwrap()
                            .to_str()
                            .unwrap(),
                    )
                    .unwrap();
                    assert_eq!(config["tabs"]["mode"], "off");
                    assert_eq!(config["mouse"], false);
                    assert_eq!(config["plugins"][0]["options"]["compact"], true);
                    assert_eq!(config["plugins"][1], "-opencode.notifications");
                    let options = config["plugins"][2]["options"].clone();
                    (config, options)
                };
                assert!(config.is_object());
                assert_eq!(options["knownID"], "saved-native");
                assert_eq!(options["launchID"], id);
                assert_eq!(
                    options["reportPath"],
                    launch.join("report.json").to_str().unwrap()
                );
                if adk {
                    let resolved = executable(Path::new("opencode"), &environment).unwrap();
                    assert_eq!(
                        std::fs::canonicalize(resolved).unwrap(),
                        std::fs::canonicalize(&binary).unwrap()
                    );
                    assert_eq!(Path::new(child.get_program()), tools.join("adk"));
                } else {
                    assert_eq!(Path::new(child.get_program()), binary);
                }
                assert_eq!(
                    child
                        .get_args()
                        .map(|v| v.to_str().unwrap())
                        .collect::<Vec<_>>(),
                    words[1..]
                );
                let output = child.output().unwrap();
                assert!(output.status.success());
                let delivered: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(delivered["tabs"]["mode"], "off");
                assert_eq!(delivered["mouse"], false);
                for (native_args, known, enabled_fork) in [
                    (
                        vec!["--session=saved-native", "--fork=false"],
                        Some("saved-native"),
                        false,
                    ),
                    (vec!["--", "--session=literal"], None, false),
                    (vec!["--prompt", "--session=literal"], None, false),
                    (vec!["--session=saved-native", "--fork=true"], None, true),
                ] {
                    let mut invocation = words[..if adk { 2 } else { 1 }].to_vec();
                    if adk {
                        invocation.push("--".into());
                    }
                    invocation.extend(native_args.into_iter().map(str::to_owned));
                    let (mut child, diagnostic) =
                        prepare_or_baseline(root.path(), &id, &invocation, &env, &home);
                    assert_eq!(
                        child
                            .get_args()
                            .map(|arg| arg.to_str().unwrap())
                            .collect::<Vec<_>>(),
                        invocation[1..]
                    );
                    if enabled_fork {
                        assert!(diagnostic
                            .unwrap()
                            .contains("Managed resume with --fork is unsupported"));
                    } else {
                        assert!(diagnostic.is_none(), "{invocation:?}");
                        let child_env = child
                            .get_envs()
                            .filter_map(|(key, value)| value.map(|value| (key, value)))
                            .collect::<HashMap<_, _>>();
                        let options: Value = if major == 1 {
                            let config: Value = serde_json::from_slice(
                                &std::fs::read(
                                    child_env[std::ffi::OsStr::new("OPENCODE_TUI_CONFIG")],
                                )
                                .unwrap(),
                            )
                            .unwrap();
                            config["plugin"][0][1].clone()
                        } else {
                            let config: Value = serde_json::from_str(
                                child_env[std::ffi::OsStr::new("OPENCODE_CLI_CONFIG_CONTENT")]
                                    .to_str()
                                    .unwrap(),
                            )
                            .unwrap();
                            config["plugins"][2]["options"].clone()
                        };
                        assert_eq!(options["knownID"], json!(known));
                    }
                    let output = child.output().unwrap();
                    assert!(
                        output.status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(config["tabs"]["mode"], "off");
                    assert_eq!(config["mouse"], false);
                }
                if major == 2 {
                    // No inline list: snapshot global plugins while overriding global tabs.
                    env.remove(std::ffi::OsStr::new("OPENCODE_CLI_CONFIG_CONTENT"));
                    let (mut child, diagnostic) =
                        prepare_or_baseline(root.path(), &id, &words, &env, &home);
                    assert!(diagnostic.is_none());
                    let output = child.output().unwrap();
                    let delivered: Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(delivered["tabs"]["mode"], "off");
                    assert_eq!(delivered["plugins"][0]["options"]["compact"], true);
                    env.insert("OPENCODE_CLI_CONFIG_CONTENT".into(), r#"{"tabs":{"mode":"on"},"plugins":["terminal-shell-plugin"],"mouse":false}"#.into());
                    let (mut child, diagnostic) =
                        prepare_or_baseline(root.path(), &id, &words, &env, &home);
                    assert!(diagnostic.is_none());
                    let output = child.output().unwrap();
                    let delivered: Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(delivered["plugins"][0], "terminal-shell-plugin");
                    assert_eq!(delivered["plugins"].as_array().unwrap().len(), 2);
                    assert_eq!(delivered["tabs"]["mode"], "off");
                    assert_eq!(delivered["mouse"], false);
                    for malformed in [
                        "{broken",
                        "{\"tabs\":\"invalid\"}",
                        "{\"mouse\":false // JSONC is not inline JSON\n}",
                    ] {
                        env.insert("OPENCODE_CLI_CONFIG_CONTENT".into(), malformed.into());
                        let (mut child, diagnostic) =
                            prepare_or_baseline(root.path(), &id, &words, &env, &home);
                        assert!(
                            diagnostic.is_some(),
                            "invalid inline config must be left to native validation"
                        );
                        let output = child.output().unwrap();
                        assert!(
                            output.status.success(),
                            "version={version}, adk={adk}: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                        assert_eq!(output.stdout, malformed.as_bytes());
                    }
                }
            }
        }
    }

    #[test]
    fn wrapper_boundary_keeps_explicit_cmux_environment_and_opaque_commands() {
        let id = uuid::Uuid::new_v4().to_string();
        let words = vec![
            "/usr/bin/env".into(),
            r#"OPENCODE_CLI_CONFIG_CONTENT={"tabs":{"mode":"off"},"plugins":["-team.*"]}"#.into(),
            "opencode".into(),
            "--auto".into(),
        ];
        let command = command(Path::new("/private/runtime"), &id, words.clone()).unwrap();
        let boundary = command.iter().position(|word| word == "--").unwrap();
        assert_eq!(command[boundary + 1..], words);
        let (env, words) = split_environment(&command[boundary + 1..]);
        let config: Value = serde_json::from_str(
            env.get(std::ffi::OsStr::new("OPENCODE_CLI_CONFIG_CONTENT"))
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(config["plugins"][0], "-team.*");
        assert_eq!(config["tabs"]["mode"], "off");
        assert_eq!(words, ["opencode", "--auto"]);
        let opaque = vec!["zsh".into(), "-lc".into(), "opencode".into()];
        assert_eq!(
            super::command(Path::new("/private/runtime"), &id, opaque.clone()).unwrap(),
            opaque
        );
    }

    #[test]
    fn relocated_readonly_v1_config_preserves_paths_and_single_pass_substitutions() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("readonly");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("plugin.txt"), " ./plugins/team.js\n").unwrap();
        let original = r#"{
            // JSONC and nested legacy terminal settings are supported.
            "tui": {"theme":"dark"},
            "plugin": [["{file:plugin.txt}", {"enabled":false,"label":"{env:LABEL}","data":"{file:notes.txt}"}], "team-package"],
            "attention":{"sounds":{"done":"{env:SOUND}"}},
            "other":"{file:notes.txt}",
        }"#;
        std::fs::write(source.join("tui.jsonc"), original).unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o500)).unwrap();
        let env = Environment::from([
            ("LABEL".into(), "My label".into()),
            ("SOUND".into(), "./sounds/done.wav".into()),
        ]);
        let config = relocate_v1(original, &source, root.path(), &env).unwrap();
        assert_eq!(
            config["plugin"][0][0],
            source.join("plugins/team.js").to_str().unwrap()
        );
        assert_eq!(config["plugin"][0][1]["enabled"], false);
        assert_eq!(config["plugin"][0][1]["label"], "My label");
        assert_eq!(
            config["other"],
            format!("{{file:{}}}", source.join("notes.txt").display())
        );
        assert_eq!(
            config["attention"]["sounds"]["done"],
            source.join("sounds/done.wav").to_str().unwrap()
        );
        assert_eq!(config["theme"], "dark");
        assert_eq!(
            std::fs::read_to_string(source.join("tui.jsonc")).unwrap(),
            original
        );
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn nested_substitutions_fail_instead_of_expanding_twice() {
        let root = tempfile::tempdir().unwrap();
        let env = Environment::from([("PLUGIN".into(), "{env:OTHER}".into())]);
        assert!(relocate_v1(
            r#"{"plugin":["{env:PLUGIN}"]}"#,
            root.path(),
            root.path(),
            &env
        )
        .is_err());
        std::fs::write(root.path().join("ref"), "{file:another}").unwrap();
        assert!(relocate_v1(
            r#"{"plugin":["{file:ref}"]}"#,
            root.path(),
            root.path(),
            &Environment::new()
        )
        .is_err());
    }

    #[test]
    fn v2_preserves_effective_plugin_options_disables_and_inline_preferences() {
        let base = Path::new("/global/opencode");
        let global = r#"{"plugins":["-opencode.notifications",{"package":"./team","options":{"compact":true}}],"theme":{"mode":"dark"}}"#;
        let config: Value = serde_json::from_str(
            &v2_config(
                Some(r#"{"tabs":{"mode":"off"},"mouse":false}"#),
                global,
                base,
                Path::new("/bridge"),
                json!({}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(config["plugins"][0], "-opencode.notifications");
        assert_eq!(config["plugins"][1]["package"], "/global/opencode/team");
        assert_eq!(config["plugins"][1]["options"]["compact"], true);
        assert_eq!(config["tabs"]["mode"], "off");
        assert_eq!(config["mouse"], false);
        let explicit: Value = serde_json::from_str(
            &v2_config(
                Some(r#"{"plugins":["-team.*"]}"#),
                global,
                base,
                Path::new("/bridge"),
                json!({}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(explicit["plugins"].as_array().unwrap().len(), 2);
        assert_eq!(explicit["plugins"][0], "-team.*");
    }

    #[test]
    fn exact_launch_reports_do_not_exchange_identity_or_override_known_resume() {
        let root = tempfile::tempdir().unwrap();
        let a = uuid::Uuid::new_v4().to_string();
        let b = uuid::Uuid::new_v4().to_string();
        for (id, native, title) in [(&a, "native-a", "First"), (&b, "native-b", "Second")] {
            std::fs::create_dir_all(root.path().join("opencode").join(id)).unwrap();
            std::fs::write(
                root.path().join("opencode").join(id).join("report.json"),
                json!({"launchID":id,"nativeID":native,"title":title}).to_string(),
            )
            .unwrap();
        }
        assert_eq!(
            report(root.path(), &a, None)
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("First")
        );
        assert_eq!(
            report(root.path(), &b, None).unwrap().unwrap().native_id,
            "native-b"
        );
        assert!(report(root.path(), &a, Some("already-saved"))
            .unwrap()
            .is_none());
        std::fs::copy(
            root.path().join("opencode").join(&b).join("report.json"),
            root.path().join("opencode").join(&a).join("report.json"),
        )
        .unwrap();
        assert!(report(root.path(), &a, None).unwrap().is_none());
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    use conversations_core::private_channel::AgentReadiness;

    #[test]
    fn readiness_expires_and_cannot_cross_native_or_wrapper_identity() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let launch = dir.path().join("opencode").join(&id);
        std::fs::create_dir_all(&launch).unwrap();
        std::fs::write(
            launch.join("report.json"),
            json!({"launchID":id,"nativeID":"native","title":null}).to_string(),
        )
        .unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        for (native, pid, at, valid) in [
            ("native", 42, now, true),
            ("other", 42, now, false),
            ("native", 43, now, false),
            ("native", 42, now - 10_000, false),
        ] {
            std::fs::write(
                launch.join("readiness.json"),
                json!({"nativeID":native,"wrapperPID":pid,"observedAt":at,"readiness":"ready"})
                    .to_string(),
            )
            .unwrap();
            let observed = readiness(dir.path(), &id, Some(42));
            if valid {
                assert_eq!(observed.unwrap(), AgentReadiness::Ready);
            } else {
                assert!(observed.is_err());
            }
        }
    }
}
