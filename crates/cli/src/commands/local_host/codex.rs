//! Bind an opted-in personal MCP registration without moving resumed Codex threads
//! out of the shared daemon that owns their writer. A session with no personal
//! registration gets a launch-scoped one autoloaded; an explicitly disabled
//! registration stays an opt-out.
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::{self, Message, WebSocket};

use super::protocol::Launch;

pub(super) struct Binding {
    socket: Option<UnixStream>,
    child: Option<std::process::Child>,
    _endpoint_lock: Option<std::fs::File>,
    pub(super) endpoint: Option<PathBuf>,
    reader: Option<JoinHandle<()>>,
}

impl Drop for Binding {
    fn drop(&mut self) {
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(child) = &mut self.child {
            // A cached/reaped child PID may already belong to another process.
            // Signal only while this handle still reports its child running.
            if matches!(child.try_wait(), Ok(None)) {
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_secs(3);
                while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(25));
                }
                if matches!(child.try_wait(), Ok(None)) {
                    let _ = child.kill();
                }
            }
            let _ = child.wait();
        }
        if let Some(path) = &self.endpoint {
            let _ = std::fs::remove_file(path);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Only the registered Conversations server is changed, and only for this native thread.
pub(super) fn prepare(
    launch: &Launch,
    args: &mut Vec<String>,
    origin: &str,
) -> Result<Option<Binding>> {
    let program = &launch.command[0];
    if Path::new(program)
        .file_name()
        .is_none_or(|name| name != "codex")
    {
        return Ok(None);
    }
    let mut options = launch_options(args)?;
    if options.resume_index.is_some() {
        resume_in_directory(args, &launch.cwd)?;
        options = launch_options(args)?;
    }
    if options.passthrough {
        return Ok(None);
    }
    let output = Command::new(program)
        .args(["mcp", "get", "conversations", "--json"])
        .current_dir(&launch.cwd)
        .stderr(Stdio::null())
        .output()?;
    let config: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    let registered = output.status.success() && is_registration(&config);
    // A host-launched session participates in project messaging, so the MCP
    // must be loaded. Autoload a launch-scoped registration when the user has
    // none; an explicitly disabled personal registration stays an opt-out.
    let autoload = !registered && config["enabled"] != false;
    let socket = codex_home()?.join("app-server-control/app-server-control.sock");
    if let Some(index) = options.resume_index {
        if socket.exists() {
            let native = args
                .get(index + 1)
                .context("Codex resume needs a native thread ID")?;
            ensure!(
                uuid::Uuid::parse_str(native).is_ok(),
                "MCP binding requires an explicit Codex resume ID"
            );
            return connect(&socket, native, &launch.id, autoload, origin, &launch.cwd).map(Some);
        }
    }
    if options.profile {
        eprintln!("Conversations automatic delivery is unavailable for this isolated Codex profile; pending messages remain in its inbox. Resume on the shared daemon or use a reachable remote app-server for native queue delivery.");
        args.splice(0..0, registration_overrides(&launch.id, origin, autoload)?);
        return Ok(None);
    }
    // An owned app-server gives fresh threads a reachable native queue without
    // changing the user's shared daemon or its configuration.
    use std::os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    };
    let endpoint = private_endpoint(&launch.id)?;
    let directory = endpoint
        .parent()
        .context("private endpoint has no parent")?;
    std::fs::create_dir_all(directory)?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(endpoint.with_extension("lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another wrapper owns the private Codex endpoint"
    );
    if let Ok(metadata) = std::fs::symlink_metadata(&endpoint) {
        ensure!(
            (metadata.file_type().is_socket() || metadata.file_type().is_symlink())
                && metadata.uid() == unsafe { libc::geteuid() },
            "refusing to remove a private Codex endpoint with unexpected ownership or type"
        );
        // Codex may replace the requested path with an owned alias to its
        // private runtime directory. Validate a present target; remove only
        // the source alias when it is stale, never its destination.
        if let Ok(resolved) = std::fs::canonicalize(&endpoint) {
            let target = std::fs::symlink_metadata(&resolved)?;
            let parent = std::fs::symlink_metadata(
                resolved
                    .parent()
                    .context("private endpoint has no parent")?,
            )?;
            ensure!(
                target.file_type().is_socket()
                    && target.uid() == unsafe { libc::geteuid() }
                    && target.mode() & 0o077 == 0
                    && parent.file_type().is_dir()
                    && parent.uid() == unsafe { libc::geteuid() }
                    && parent.mode() & 0o077 == 0,
                "refusing stale private endpoint cleanup with an unsafe target"
            );
        }
        match UnixStream::connect(&endpoint) {
            Ok(_) => bail!(
                "private Codex endpoint is still live; close its previous wrapper before reopening"
            ),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(&endpoint)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut server_args = vec![
        "app-server".to_string(),
        "--listen".to_string(),
        format!("unix://{}", endpoint.display()),
    ];
    server_args.extend(registration_overrides(&launch.id, origin, autoload)?);
    let mut server = Command::new(program);
    server.env("CONVERSATIONS_SERVER", origin);
    server
        .args(&server_args)
        .current_dir(&launch.cwd)
        .env("CONVERSATIONS_WRAPPED", &launch.id)
        .env("CONVERSATIONS_CODEX_SOCKET", &endpoint)
        .env("CONVERSATIONS_CLI", std::env::current_exe()?)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    // Preserve every accepted spelling of explicit backend configuration.
    server.args(&options.server_overrides);
    let child = server
        .spawn()
        .context("starting private Codex app-server")?;
    let mut guard = Binding {
        socket: None,
        child: Some(child),
        _endpoint_lock: Some(lock),
        endpoint: Some(endpoint.clone()),
        reader: None,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !endpoint.exists() {
        ensure!(
            guard.child.as_mut().unwrap().try_wait()?.is_none(),
            "private Codex app-server exited before startup"
        );
        ensure!(
            Instant::now() < deadline,
            "private Codex app-server startup timed out"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    args.splice(
        0..0,
        ["--remote".into(), format!("unix://{}", endpoint.display())],
    );
    Ok(Some(guard))
}

// Codex replays saved thread_settings_applied events over the JSONL header's cwd.
// Pin the managed folder for both shared-daemon and private-app-server resumes.
fn resume_in_directory(args: &mut Vec<String>, cwd: &Path) -> Result<()> {
    let mut original = std::mem::take(args).into_iter();
    while let Some(arg) = original.next() {
        if arg == "--" {
            args.extend(["--cd".into(), cwd.to_string_lossy().into_owned(), arg]);
            args.extend(original);
            return Ok(());
        }
        if arg == "--cd" || arg == "-C" {
            original
                .next()
                .context("Codex directory option needs a value")?;
        } else if !(arg.starts_with("--cd=") || arg.starts_with("-C") && arg.len() > 2) {
            args.push(arg);
        }
    }
    args.extend(["--cd".into(), cwd.to_string_lossy().into_owned()]);
    Ok(())
}

#[derive(Default)]
struct LaunchOptions {
    passthrough: bool,
    profile: bool,
    resume_index: Option<usize>,
    server_overrides: Vec<String>,
}

fn launch_options(args: &[String]) -> Result<LaunchOptions> {
    let mut options = LaunchOptions::default();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            break;
        }
        match arg.as_str() {
            "--remote" | "--no-daemon" => options.passthrough = true,
            "-p" | "--profile" => {
                options.profile = true;
                index += 1;
            }
            "-c" | "--config" | "--enable" | "--disable" => {
                let value = args
                    .get(index + 1)
                    .context("Codex configuration option needs a value")?;
                options
                    .server_overrides
                    .extend([arg.clone(), value.clone()]);
                index += 1;
            }
            "resume" => options.resume_index = Some(index),
            _ if arg.starts_with("--remote=") => options.passthrough = true,
            _ if arg.starts_with("--profile=") || arg.starts_with("-p") => options.profile = true,
            _ if arg.starts_with("--config=")
                || arg.starts_with("--enable=")
                || arg.starts_with("--disable=")
                || arg.starts_with("-c") =>
            {
                options.server_overrides.push(arg.clone())
            }
            _ => {}
        }
        index += 1;
    }
    Ok(options)
}

/// `-c` overrides binding this session's MCP identity. With `autoload`, also
/// inject a full launch-scoped registration so a session with no personal one
/// still loads the MCP. Explicit user `-c`/`--config` values are appended
/// after these by the caller, so they win on conflict.
fn registration_overrides(session: &str, origin: &str, autoload: bool) -> Result<Vec<String>> {
    let mut overrides = Vec::new();
    if autoload {
        let exe = std::env::current_exe()?;
        let exe = exe.to_str().context("non-UTF-8 CLI executable path")?;
        overrides.extend([
            "-c".to_string(),
            format!(
                "mcp_servers.conversations.command={}",
                serde_json::to_string(exe)?
            ),
            "-c".to_string(),
            format!(
                "mcp_servers.conversations.args={}",
                serde_json::to_string(&json!([
                    "--server",
                    origin,
                    "mcp",
                    "serve",
                    "--delivery",
                    "codex"
                ]))?
            ),
            "-c".to_string(),
            format!(
                "mcp_servers.conversations.env_vars={}",
                serde_json::to_string(&json!([
                    "CONVERSATIONS_WRAPPED",
                    "CONVERSATIONS_SESSION_ID",
                    "CONVERSATIONS_CODEX_SOCKET",
                    "CONVERSATIONS_SERVER"
                ]))?
            ),
        ]);
    }
    overrides.extend([
        "-c".to_string(),
        format!(
            "mcp_servers.conversations.env.CONVERSATIONS_SESSION_ID={}",
            serde_json::to_string(session)?
        ),
    ]);
    Ok(overrides)
}

fn is_registration(config: &Value) -> bool {
    config["enabled"] == true
        && config["transport"]["type"] == "stdio"
        && config["transport"]["command"]
            .as_str()
            .is_some_and(|command| {
                Path::new(command)
                    .file_name()
                    .is_some_and(|name| name == "arrakis-conversations")
            })
        && config["transport"]["args"].as_array().is_some_and(|args| {
            args.windows(2)
                .any(|pair| pair[0] == "mcp" && pair[1] == "serve")
        })
}

fn codex_home() -> Result<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .context("cannot find Codex user configuration")
}

fn rpc(ws: &mut WebSocket<UnixStream>, id: u64, method: &str, params: Value) -> Result<Value> {
    ws.send(Message::Text(
        json!({"id":id,"method":method,"params":params}).to_string(),
    ))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let message = match ws.read() {
            Ok(message) => message,
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => return Err(error.into()),
        };
        if let Message::Text(text) = message {
            let reply: Value = serde_json::from_str(&text)?;
            if reply["id"] == id && reply.get("method").is_none() {
                if let Some(error) = reply.get("error") {
                    bail!("Codex {method}: {error}");
                }
                return Ok(reply["result"].clone());
            }
        }
    }
    bail!("Codex {method} timed out")
}

/// The native root thread a fresh launch created on its private app-server,
/// once Codex has written it to disk. `None` without an endpoint or until
/// exactly one such root is loaded; subagent and ephemeral threads never
/// count. The whole exchange shares one `deadline`; on expiry the connection
/// is dropped, so a stalled server leaves no probe behind.
pub(super) async fn private_root_thread(
    endpoint: &Path,
    deadline: Duration,
) -> Result<Option<String>> {
    if !tokio::fs::try_exists(endpoint).await? {
        return Ok(None);
    }
    tokio::time::timeout(deadline, loaded_root_thread(endpoint))
        .await
        .context("private Codex app-server did not answer in time")?
}

async fn loaded_root_thread(endpoint: &Path) -> Result<Option<String>> {
    let stream = tokio::net::UnixStream::connect(endpoint)
        .await
        .context("connecting to the private Codex app-server")?;
    let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream).await?;
    async_rpc(
        &mut ws,
        1,
        "initialize",
        json!({"clientInfo":{"name":"conversations-host","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),
    )
    .await?;
    ws.send(Message::Text(json!({"method":"initialized"}).to_string()))
        .await?;
    let loaded = async_rpc(&mut ws, 2, "thread/loaded/list", json!({})).await?;
    let mut roots = Vec::new();
    for (request, id) in (3..).zip(
        loaded["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str),
    ) {
        let read = async_rpc(
            &mut ws,
            request,
            "thread/read",
            json!({"threadId":id,"includeTurns":false}),
        )
        .await?;
        let thread = &read["thread"];
        // Review and compaction subagents carry no parent ID; their source does.
        if thread["ephemeral"] == true
            || !thread["parentThreadId"].is_null()
            || thread["source"].get("subAgent").is_some()
            || uuid::Uuid::parse_str(id).is_err()
        {
            continue;
        }
        // Codex reports the rollout path before its first turn writes it.
        let materialized = match thread["path"].as_str() {
            Some(path) => tokio::fs::try_exists(path).await.unwrap_or(false),
            None => false,
        };
        roots.push((id.to_owned(), materialized));
    }
    let _ = ws.close(None).await;
    Ok(match roots.as_slice() {
        [(id, true)] => Some(id.clone()),
        _ => None,
    })
}

async fn async_rpc(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    ws.send(Message::Text(
        json!({"id":id,"method":method,"params":params}).to_string(),
    ))
    .await?;
    while let Some(message) = ws.next().await {
        if let Message::Text(text) = message? {
            let reply: Value = serde_json::from_str(&text)?;
            if reply["id"] == id && reply.get("method").is_none() {
                if let Some(error) = reply.get("error") {
                    bail!("Codex {method}: {error}");
                }
                return Ok(reply["result"].clone());
            }
        }
    }
    bail!("Codex {method}: connection closed")
}

pub(super) fn private_endpoint(session: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(session).is_ok(),
        "invalid wrapper session ID"
    );
    Ok(dirs::home_dir()
        .context("no home directory")?
        .join(".local/state/ac-codex")
        .join(format!("{session}.sock")))
}

fn connect(
    socket: &Path,
    native: &str,
    session: &str,
    autoload: bool,
    origin: &str,
    cwd: &Path,
) -> Result<Binding> {
    let stream = UnixStream::connect(socket).context("connecting to Codex's shared daemon")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let (mut ws, _) = tungstenite::client("ws://localhost/", stream)?;
    rpc(
        &mut ws,
        1,
        "initialize",
        json!({"clientInfo":{"name":"conversations-host","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),
    )?;
    ws.send(Message::Text(json!({"method":"initialized"}).to_string()))?;
    let thread = rpc(
        &mut ws,
        2,
        "thread/read",
        json!({"threadId":native,"includeTurns":false}),
    )?;
    ensure!(
        thread["thread"]["status"]["type"] != "active",
        "Codex thread is still active; finish or interrupt its current turn before restarting"
    );
    let mut config = json!({"mcp_servers.conversations.env.CONVERSATIONS_SESSION_ID":session});
    if autoload {
        let exe = std::env::current_exe().context("no current executable")?;
        config["mcp_servers.conversations.command"] = json!(exe.to_string_lossy());
        config["mcp_servers.conversations.args"] =
            json!(["--server", origin, "mcp", "serve", "--delivery", "codex"]);
        config["mcp_servers.conversations.env_vars"] = json!([
            "CONVERSATIONS_WRAPPED",
            "CONVERSATIONS_SESSION_ID",
            "CONVERSATIONS_CODEX_SOCKET",
            "CONVERSATIONS_SERVER"
        ]);
    }
    rpc(
        &mut ws,
        3,
        "thread/resume",
        json!({"threadId":native,"excludeTurns":true,"config":config,"cwd":cwd}),
    )?;
    let expected = format!("Conversations session {session}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut request = 4;
    loop {
        let status = rpc(
            &mut ws,
            request,
            "mcpServerStatus/list",
            json!({"threadId":native,"serverName":"conversations"}),
        )?;
        request += 1;
        if let Some(server) = status["data"]
            .as_array()
            .and_then(|data| data.iter().find(|server| server["name"] == "conversations"))
        {
            if server["serverInfo"]["title"] == expected {
                break;
            }
            if server["connectionStatus"] == "failed" {
                bail!(
                    "Conversations MCP failed to start; check the personal registration and login"
                );
            }
        }
        ensure!(Instant::now() < deadline, "Codex did not expose the expected Conversations binding. Close other clients for this native thread and check that the MCP executable is current");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Keep the prepared thread subscribed until the wrapped TUI exits. Closing
    // immediately lets the daemon unload it before the TUI can reconnect.
    let socket = ws.get_ref().try_clone()?;
    let reader = std::thread::spawn(move || loop {
        let message = match ws.read() {
            Ok(message) => message,
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => break,
        };
        if let Message::Text(text) = message {
            if let Ok(request) = serde_json::from_str::<Value>(&text) {
                if request.get("method").is_some() && request.get("id").is_some() {
                    let _ = ws.send(Message::Text(json!({"id":request["id"],"error":{"code":-32601,"message":"Use the Codex TUI for interactive requests"}}).to_string()));
                }
            }
        }
    });
    Ok(Binding {
        socket: Some(socket),
        child: None,
        _endpoint_lock: None,
        endpoint: None,
        reader: Some(reader),
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn daemon(listener: UnixListener, active: bool) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            while let Ok(Message::Text(text)) = ws.read() {
                let request: Value = serde_json::from_str(&text).unwrap();
                let id = &request["id"];
                if id.is_null() {
                    continue;
                }
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => json!({}),
                    "thread/read" => {
                        assert_eq!(request["params"]["threadId"], "native-thread");
                        json!({"thread":{"status":{"type":if active {"active"} else {"idle"}}}})
                    }
                    "thread/resume" => {
                        assert!(!active, "an active thread must never be rebound");
                        assert_eq!(
                            request["params"]["cwd"], "/destination",
                            "resume must override the source folder retained by Codex"
                        );
                        assert_eq!(request["params"]["threadId"], "native-thread");
                        assert_eq!(
                            request["params"]["config"]
                                ["mcp_servers.conversations.env.CONVERSATIONS_SESSION_ID"],
                            "wrapper-session"
                        );
                        json!({"thread":{"id":"native-thread"}})
                    }
                    "mcpServerStatus/list" => {
                        json!({"data":[{"name":"conversations","connectionStatus":"connected","serverInfo":{"title":"Conversations session wrapper-session"}}]})
                    }
                    method => panic!("unexpected request {method}"),
                };
                ws.send(Message::Text(json!({"id":id,"result":result}).to_string()))
                    .unwrap();
            }
        })
    }

    #[test]
    fn prepares_native_tui_resume_in_the_destination_folder() {
        let launch = Launch {
            id: "wrapper-session".into(),
            agent: conversations_core::AgentKind::Codex,
            cwd: PathBuf::from("/destination with spaces"),
            command: [
                "/missing/codex",
                "--remote",
                "unix://existing.sock",
                "resume",
                "native-thread",
                "--cd=/source",
            ]
            .map(str::to_owned)
            .to_vec(),
        };
        let original = launch.command.clone();
        let mut args = launch.command[1..].to_vec();
        assert!(prepare(&launch, &mut args, "http://localhost")
            .unwrap()
            .is_none());
        assert_eq!(
            args,
            [
                "--remote",
                "unix://existing.sock",
                "resume",
                "native-thread",
                "--cd",
                "/destination with spaces"
            ]
        );
        assert_eq!(
            launch.command, original,
            "checkpoint arguments must stay unchanged"
        );
    }

    #[test]
    fn managed_resume_folder_replaces_directory_flags_without_changing_prompt() {
        for flags in [
            vec![],
            vec!["-C", "/source"],
            vec!["--cd", "/source"],
            vec!["-C/source"],
            vec!["--cd=/source"],
        ] {
            let mut args: Vec<String> = [
                vec!["--no-alt-screen", "resume", "native-thread"],
                flags,
                vec!["--", "literal --cd /source"],
            ]
            .concat()
            .into_iter()
            .map(str::to_owned)
            .collect();
            resume_in_directory(&mut args, Path::new("/destination with spaces")).unwrap();
            assert_eq!(
                args,
                [
                    "--no-alt-screen",
                    "resume",
                    "native-thread",
                    "--cd",
                    "/destination with spaces",
                    "--",
                    "literal --cd /source"
                ]
            );
            let index = launch_options(&args).unwrap().resume_index.unwrap();
            assert_eq!(args[index + 1], "native-thread");
        }
    }

    #[test]
    fn launch_argument_spellings_preserve_backend_config_and_literal_prompts() {
        for (args, passthrough, profile, expected) in [
            (vec!["--remote=unix://owned.sock"], true, false, vec![]),
            (vec!["--remote", "unix://owned.sock"], true, false, vec![]),
            (vec!["-pwork"], false, true, vec![]),
            (vec!["--profile=work"], false, true, vec![]),
            (vec!["-p", "work"], false, true, vec![]),
            (
                vec![
                    "-ckey=value",
                    "--config=other=value",
                    "--enable=alpha",
                    "--disable=beta",
                ],
                false,
                false,
                vec![
                    "-ckey=value",
                    "--config=other=value",
                    "--enable=alpha",
                    "--disable=beta",
                ],
            ),
            (
                vec![
                    "-c",
                    "key=value",
                    "--config",
                    "other=value",
                    "--enable",
                    "alpha",
                    "--disable",
                    "beta",
                ],
                false,
                false,
                vec![
                    "-c",
                    "key=value",
                    "--config",
                    "other=value",
                    "--enable",
                    "alpha",
                    "--disable",
                    "beta",
                ],
            ),
            (
                vec![
                    "--",
                    "--remote=unix://literal",
                    "-ptext",
                    "--config=literal",
                    "resume",
                ],
                false,
                false,
                vec![],
            ),
            (
                vec!["-c", "literal=--remote", "--", "--enable=literal"],
                false,
                false,
                vec!["-c", "literal=--remote"],
            ),
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            let options = launch_options(&args).unwrap();
            assert_eq!(options.passthrough, passthrough, "{args:?}");
            assert_eq!(options.profile, profile, "{args:?}");
            assert_eq!(options.server_overrides, expected, "{args:?}");
        }
        let launch = Launch {
            id: "unused".into(),
            agent: conversations_core::AgentKind::Codex,
            cwd: PathBuf::from("/"),
            command: vec!["/missing/codex".into()],
        };
        let mut args = vec!["--remote=unix://owned.sock".into()];
        assert!(prepare(&launch, &mut args, "http://localhost")
            .unwrap()
            .is_none());
        assert_eq!(args, vec!["--remote=unix://owned.sock"]);
    }

    #[test]
    fn binds_and_pins_the_correct_native_thread_until_the_wrapper_exits() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server = daemon(UnixListener::bind(&socket).unwrap(), false);
        let binding = connect(
            &socket,
            "native-thread",
            "wrapper-session",
            false,
            "http://localhost",
            Path::new("/destination"),
        )
        .unwrap();
        // A quiet connection must survive the read timeout; this is what keeps
        // the prepared thread alive while the real TUI starts.
        std::thread::sleep(Duration::from_millis(2100));
        assert!(!binding.reader.as_ref().unwrap().is_finished());
        drop(binding);
        server.join().unwrap();
    }

    #[test]
    fn refuses_to_reconfigure_a_thread_with_a_running_turn() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server = daemon(UnixListener::bind(&socket).unwrap(), true);
        let error = connect(
            &socket,
            "native-thread",
            "wrapper-session",
            false,
            "http://localhost",
            Path::new("/destination"),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("still active"));
        server.join().unwrap();
    }

    #[test]
    fn autoloaded_registration_reaches_the_resumed_thread() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let server = {
            let listener = UnixListener::bind(&socket).unwrap();
            std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut ws = tungstenite::accept(stream).unwrap();
                while let Ok(Message::Text(text)) = ws.read() {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let id = &request["id"];
                    if id.is_null() {
                        continue;
                    }
                    let result = match request["method"].as_str().unwrap() {
                        "initialize" => json!({}),
                        "thread/read" => json!({"thread":{"status":{"type":"idle"}}}),
                        "thread/resume" => {
                            let config = &request["params"]["config"];
                            assert_eq!(
                                config["mcp_servers.conversations.args"],
                                json!([
                                    "--server",
                                    "http://localhost",
                                    "mcp",
                                    "serve",
                                    "--delivery",
                                    "codex"
                                ])
                            );
                            assert!(!config["mcp_servers.conversations.command"]
                                .as_str()
                                .unwrap_or("")
                                .is_empty());
                            assert_eq!(
                                config["mcp_servers.conversations.env.CONVERSATIONS_SESSION_ID"],
                                "wrapper-session"
                            );
                            json!({"thread":{"id":"native-thread"}})
                        }
                        "mcpServerStatus/list" => {
                            json!({"data":[{"name":"conversations","connectionStatus":"connected","serverInfo":{"title":"Conversations session wrapper-session"}}]})
                        }
                        method => panic!("unexpected request {method}"),
                    };
                    ws.send(Message::Text(json!({"id":id,"result":result}).to_string()))
                        .unwrap();
                }
            })
        };
        let binding = connect(
            &socket,
            "native-thread",
            "wrapper-session",
            true,
            "http://localhost",
            Path::new("/destination"),
        )
        .unwrap();
        drop(binding);
        server.join().unwrap();
    }

    #[test]
    fn autoload_injects_a_launch_scoped_registration() {
        let overrides =
            registration_overrides("wrapper-session", "https://conv.example", true).unwrap();
        let rendered = overrides.join("\n");
        for expected in [
            "mcp_servers.conversations.command=",
            "mcp_servers.conversations.args=",
            "--server",
            "https://conv.example",
            "--delivery",
            "codex",
            "mcp_servers.conversations.env_vars=",
            "CONVERSATIONS_SESSION_ID",
            "mcp_servers.conversations.env.CONVERSATIONS_SESSION_ID=\"wrapper-session\"",
        ] {
            assert!(rendered.contains(expected), "{expected} in {rendered}");
        }
        // Every override is a `-c key=value` pair consumable by Codex.
        assert_eq!(overrides.len() % 2, 0);
        assert!(overrides.iter().step_by(2).all(|arg| arg == "-c"));
    }

    #[test]
    fn no_autoload_injects_only_the_session_identity() {
        let overrides =
            registration_overrides("wrapper-session", "https://conv.example", false).unwrap();
        assert_eq!(overrides.len(), 2);
        assert_eq!(overrides[0], "-c");
        assert_eq!(
            overrides[1],
            "mcp_servers.conversations.env.CONVERSATIONS_SESSION_ID=\"wrapper-session\""
        );
    }

    /// Speaks the installed Codex app-server shapes (checked against
    /// codex-cli 0.164 `thread/loaded/list` and `thread/read`).
    pub(in crate::commands::local_host) fn app_server(
        listener: UnixListener,
        threads: Vec<Value>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            while let Ok(message) = ws.read() {
                let Message::Text(text) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let id = &request["id"];
                if id.is_null() {
                    continue;
                }
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => json!({"codexHome":"/unused","platformFamily":"unix"}),
                    "thread/loaded/list" => {
                        json!({"data": threads.iter().map(|t| t["id"].clone()).collect::<Vec<_>>(), "nextCursor": null})
                    }
                    "thread/read" => {
                        let thread = threads
                            .iter()
                            .find(|t| t["id"] == request["params"]["threadId"])
                            .unwrap();
                        json!({ "thread": thread })
                    }
                    method => panic!("unexpected request {method}"),
                };
                if ws
                    .send(Message::Text(json!({"id":id,"result":result}).to_string()))
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    pub(in crate::commands::local_host) fn thread(
        id: &str,
        path: Option<&Path>,
        source: Value,
        parent: Option<&str>,
        ephemeral: bool,
    ) -> Value {
        json!({"id": id, "sessionId": id, "path": path, "source": source, "parentThreadId": parent, "forkedFromId": null, "ephemeral": ephemeral, "status": {"type": "idle"}})
    }

    #[tokio::test]
    async fn learns_only_an_unambiguous_materialized_root_thread() {
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout.jsonl");
        std::fs::write(&rollout, "").unwrap();
        let root = "01a168c9-b038-70a3-9f0f-4d3aebae2d94";
        let spawned = thread(
            "01a168ca-0000-7000-8000-000000000001",
            Some(&rollout),
            json!({"subAgent": {"thread_spawn": {"parent_thread_id": root, "depth": 1}}}),
            Some(root),
            false,
        );
        // A review subagent has no parent ID; only its source marks it.
        let review = thread(
            "01a168ca-0000-7000-8000-000000000004",
            Some(&rollout),
            json!({"subAgent": "review"}),
            None,
            false,
        );
        let ephemeral = thread(
            "01a168ca-0000-7000-8000-000000000002",
            None,
            json!("vscode"),
            None,
            true,
        );
        let other_root = thread(
            "01a168ca-0000-7000-8000-000000000003",
            Some(&rollout),
            json!("cli"),
            None,
            false,
        );
        let root_thread = |path: Option<&Path>| thread(root, path, json!("vscode"), None, false);
        for (threads, expected) in [
            (
                vec![
                    root_thread(Some(&rollout)),
                    spawned.clone(),
                    review.clone(),
                    ephemeral,
                ],
                Some(root),
            ),
            // Codex has not written the rollout yet, so a resume would fail.
            (
                vec![root_thread(Some(&dir.path().join("missing.jsonl")))],
                None,
            ),
            (vec![root_thread(None)], None),
            (vec![spawned.clone(), review.clone()], None),
            (vec![], None),
            // Never guess between two roots (for example after `/new`).
            (vec![root_thread(Some(&rollout)), other_root.clone()], None),
        ] {
            let socket = dir.path().join("private.sock");
            let _ = std::fs::remove_file(&socket);
            let server = app_server(UnixListener::bind(&socket).unwrap(), threads.clone());
            assert_eq!(
                private_root_thread(&socket, Duration::from_secs(5))
                    .await
                    .unwrap()
                    .as_deref(),
                expected,
                "{threads:?}"
            );
            server.join().unwrap();
        }
        assert_eq!(
            private_root_thread(&dir.path().join("absent.sock"), Duration::from_secs(5))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_stalled_app_server_is_abandoned_at_one_shared_deadline() {
        for trickle in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("private.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut ws = tungstenite::accept(stream).unwrap();
                ws.get_ref()
                    .set_read_timeout(Some(Duration::from_millis(20)))
                    .unwrap();
                // Answer initialize, then stall the listing: silently, or with
                // a stream of unrelated notifications that never match its ID.
                loop {
                    match ws.read() {
                        Ok(Message::Text(text)) => {
                            let request: Value = serde_json::from_str(&text).unwrap();
                            if request["method"] == "initialize" {
                                ws.send(Message::Text(
                                    json!({"id":request["id"],"result":{}}).to_string(),
                                ))
                                .unwrap();
                            }
                        }
                        Ok(_) => {}
                        Err(tungstenite::Error::Io(error))
                            if error.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            if trickle
                                && ws
                                    .send(Message::Text(
                                        json!({"method":"thread/status/changed","params":{}})
                                            .to_string(),
                                    ))
                                    .is_err()
                            {
                                return;
                            }
                        }
                        // The probe closed its connection.
                        Err(_) => return,
                    }
                }
            });
            let started = std::time::Instant::now();
            let error = private_root_thread(&socket, Duration::from_millis(300))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("did not answer in time"),
                "{error:#}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{:?}",
                started.elapsed()
            );
            // Joining proves the abandoned probe released its connection.
            tokio::time::timeout(
                Duration::from_secs(2),
                tokio::task::spawn_blocking(move || server.join().unwrap()),
            )
            .await
            .expect("the abandoned probe kept its connection open")
            .unwrap();
        }
    }

    /// Run with `cargo test -- --ignored real_codex` where `codex` is installed.
    /// Holds the fakes above to the installed app-server protocol.
    #[tokio::test]
    #[ignore = "needs an installed codex CLI"]
    async fn real_codex_app_server_matches_the_probe_contract() {
        let home = tempfile::tempdir().unwrap();
        let socket = home.path().join("s.sock");
        let mut server = tokio::process::Command::new("codex")
            .args(["app-server", "--listen"])
            .arg(format!("unix://{}", socket.display()))
            .env("CODEX_HOME", home.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !socket.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "codex app-server did not start"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream)
            .await
            .unwrap();
        async_rpc(&mut ws, 1, "initialize", json!({"clientInfo":{"name":"probe-contract","version":"0"},"capabilities":{"experimentalApi":true}}))
            .await
            .unwrap();
        ws.send(Message::Text(json!({"method":"initialized"}).to_string()))
            .await
            .unwrap();
        let started = async_rpc(&mut ws, 2, "thread/start", json!({"cwd": home.path()}))
            .await
            .unwrap();
        let id = started["thread"]["id"].as_str().unwrap().to_owned();
        let loaded = async_rpc(&mut ws, 3, "thread/loaded/list", json!({}))
            .await
            .unwrap();
        assert_eq!(loaded["data"], json!([id]));
        let read = async_rpc(
            &mut ws,
            4,
            "thread/read",
            json!({"threadId": id, "includeTurns": false}),
        )
        .await
        .unwrap();
        let thread = &read["thread"];
        assert!(thread["source"].is_string(), "{thread}");
        assert!(thread["parentThreadId"].is_null(), "{thread}");
        assert_eq!(thread["ephemeral"], false);
        // The path is reported before the first turn writes the rollout.
        let path = thread["path"].as_str().unwrap();
        assert!(!Path::new(path).exists());
        assert_eq!(
            private_root_thread(&socket, Duration::from_secs(5))
                .await
                .unwrap(),
            None,
            "an unwritten root must not be persisted"
        );
        server.kill().await.unwrap();
    }
}

/// Read a fresh native tree and its running background terminals. Unsupported
/// app-servers return an error, never a guessed completion state.
pub(super) async fn readiness(
    session: &str,
    native: &str,
) -> Result<conversations_core::private_channel::AgentReadiness> {
    let private = private_endpoint(session)?;
    let endpoint = if tokio::fs::try_exists(&private).await? {
        private
    } else {
        codex_home()?.join("app-server-control/app-server-control.sock")
    };
    let stream = tokio::net::UnixStream::connect(endpoint).await?;
    let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream).await?;
    let result = readiness_snapshot(&mut ws, native).await;
    let _ = ws.close(None).await;
    result
}

async fn readiness_snapshot(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
    native: &str,
) -> Result<conversations_core::private_channel::AgentReadiness> {
    use conversations_core::private_channel::AgentReadiness;
    async_rpc(ws, 1, "initialize", json!({"clientInfo":{"name":"conversations-readiness","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
    ws.send(Message::Text(json!({"method":"initialized"}).to_string()))
        .await?;
    let root = async_rpc(
        ws,
        2,
        "thread/read",
        json!({"threadId":native,"includeTurns":false}),
    )
    .await?;
    ensure!(
        root["thread"]["id"] == native,
        "native thread identity mismatch"
    );
    if root["thread"]["status"]["type"] == "active" {
        let flags = root["thread"]["status"]["activeFlags"]
            .as_array()
            .context("missing native active flags")?;
        if flags.iter().any(|flag| {
            matches!(
                flag.as_str(),
                Some("waitingOnApproval" | "waitingOnUserInput")
            )
        }) {
            return Ok(AgentReadiness::NeedsInput);
        }
    }
    let mut request = 3;
    let mut cursor = Value::Null;
    let mut threads = std::collections::HashMap::new();
    loop {
        let page = async_rpc(
            ws,
            request,
            "thread/loaded/list",
            json!({"cursor":cursor,"limit":100}),
        )
        .await?;
        request += 1;
        for id in page["data"].as_array().context("invalid loaded threads")? {
            let id = id.as_str().context("invalid thread ID")?;
            let read = async_rpc(
                ws,
                request,
                "thread/read",
                json!({"threadId":id,"includeTurns":false}),
            )
            .await?;
            request += 1;
            ensure!(
                read["thread"]["id"] == id,
                "native thread identity mismatch"
            );
            threads.insert(id.to_owned(), read["thread"].clone());
        }
        cursor = page["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
        ensure!(request < 1000, "too many native threads");
    }
    ensure!(threads.contains_key(native), "native thread is not loaded");
    let mut family = std::collections::BTreeSet::from([native.to_owned()]);
    loop {
        let before = family.len();
        for (id, thread) in &threads {
            if thread["parentThreadId"]
                .as_str()
                .or_else(|| {
                    thread["source"]["subAgent"]["thread_spawn"]["parent_thread_id"].as_str()
                })
                .is_some_and(|parent| family.contains(parent))
            {
                family.insert(id.clone());
            }
        }
        if family.len() == before {
            break;
        }
    }
    let mut working = false;
    let mut background = false;
    for id in &family {
        let status = &threads[id]["status"];
        match status["type"].as_str() {
            Some("active") => {
                let flags = status["activeFlags"]
                    .as_array()
                    .context("missing native active flags")?;
                if flags.iter().any(|flag| {
                    matches!(
                        flag.as_str(),
                        Some("waitingOnApproval" | "waitingOnUserInput")
                    )
                }) {
                    return Ok(AgentReadiness::NeedsInput);
                }
                if id == native {
                    working = true;
                } else {
                    background = true;
                }
            }
            Some("idle") => {}
            _ => return Ok(AgentReadiness::Unknown),
        }
    }
    if working {
        return Ok(AgentReadiness::Working);
    }
    if background {
        return Ok(AgentReadiness::Background);
    }
    for id in &family {
        let page = async_rpc(
            ws,
            request,
            "thread/backgroundTerminals/list",
            json!({"threadId":id,"limit":1}),
        )
        .await?;
        request += 1;
        if !page["data"]
            .as_array()
            .context("invalid native background terminals")?
            .is_empty()
        {
            background = true;
        }
    }
    Ok(if working {
        AgentReadiness::Working
    } else if background {
        AgentReadiness::Background
    } else {
        AgentReadiness::Ready
    })
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    use conversations_core::private_channel::AgentReadiness;

    #[tokio::test]
    async fn native_snapshot_checks_descendants_and_background_terminals_before_ready() {
        for (root_status, child_status, terminals, supported, expected) in [
            (
                json!({"type":"active","activeFlags":[]}),
                json!({"type":"active","activeFlags":["waitingOnApproval"]}),
                false,
                false,
                AgentReadiness::NeedsInput,
            ),
            (
                json!({"type":"idle"}),
                json!({"type":"active","activeFlags":[]}),
                false,
                true,
                AgentReadiness::Background,
            ),
            (
                json!({"type":"idle"}),
                json!({"type":"idle"}),
                true,
                true,
                AgentReadiness::Background,
            ),
            (
                json!({"type":"idle"}),
                json!({"type":"idle"}),
                false,
                true,
                AgentReadiness::Ready,
            ),
            (
                json!({"type":"active","activeFlags":["waitingOnApproval"]}),
                json!({"type":"idle"}),
                false,
                false,
                AgentReadiness::NeedsInput,
            ),
            (
                json!({"type":"active","activeFlags":[]}),
                json!({"type":"idle"}),
                false,
                false,
                AgentReadiness::Working,
            ),
            (
                json!({"type":"idle"}),
                json!({"type":"idle"}),
                false,
                false,
                AgentReadiness::Unknown,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("native.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut ws = tungstenite::accept(stream).unwrap();
                while let Ok(Message::Text(text)) = ws.read() {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    if request["id"].is_null() {
                        continue;
                    }
                    let method = request["method"].as_str().unwrap();
                    let id = request["params"]["threadId"].as_str().unwrap_or("");
                    let result = match method {
                        "initialize" => json!({}),
                        "thread/loaded/list" if request["params"]["cursor"].is_null() => {
                            json!({"data":["root"],"nextCursor":"second"})
                        }
                        "thread/loaded/list" => json!({"data":["child"],"nextCursor":null}),
                        "thread/read" => {
                            json!({"thread":{"id":id,"parentThreadId":if id == "child" {Some("root")} else {None},"status":if id == "root" {&root_status} else {&child_status}}})
                        }
                        "thread/backgroundTerminals/list" if supported => {
                            json!({"data":if terminals {json!([{"itemId":"background"}])} else {json!([])},"nextCursor":null})
                        }
                        "thread/backgroundTerminals/list" => {
                            ws.send(Message::Text(json!({"id":request["id"],"error":{"code":-32601,"message":"unsupported"}}).to_string())).unwrap();
                            continue;
                        }
                        _ => panic!("unexpected native method {method}"),
                    };
                    ws.send(Message::Text(
                        json!({"id":request["id"],"result":result}).to_string(),
                    ))
                    .unwrap();
                }
            });
            let stream = tokio::net::UnixStream::connect(path).await.unwrap();
            let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream)
                .await
                .unwrap();
            assert_eq!(
                readiness_snapshot(&mut ws, "root")
                    .await
                    .unwrap_or_default(),
                expected
            );
            drop(ws);
            server.join().unwrap();
        }
    }
}
