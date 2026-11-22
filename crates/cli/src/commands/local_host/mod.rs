//! The local daemon (`host`), the commands that manage it, and `start-agent`,
//! which runs an agent in the current terminal under it.
mod access_metadata;
pub(crate) mod agent_link;
pub(crate) mod authz;
mod cmux;
mod codex;
mod communication;
mod continuation;
mod daemon;
pub(crate) mod github;
pub(crate) mod history;
mod import;
mod installer;
mod linear_metadata;
mod migration;
mod onboarding;
mod opencode_bridge;
mod order;
#[cfg(test)]
mod pr_cache_tests;
mod pr_metadata;
pub(crate) mod private_peer;
mod projects;
pub(crate) mod protocol;
pub(crate) mod publication;
mod readiness;
pub(crate) mod settings;
mod subject;
#[cfg(test)]
mod test_support;
pub(crate) mod transfers;
mod web;
mod wrapper;
pub(crate) use wrapper::peer_input;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::builder::TypedValueParser as _;
use clap::{Args, Subcommand, ValueEnum};
use conversations_core::{AgentKind, AgentLauncher};
use serde::{Deserialize, Serialize};

use crate::cli::GlobalOpts;
use protocol::{read_message, write_message, ClientMessage, ServerMessage};

fn configured_directory(path: &Path, option: &str) -> Result<PathBuf> {
    // Keep stable profile symlinks in the saved recipe. Resolve their current
    // target when the daemon starts, rather than freezing today's tool version.
    let directory = std::path::absolute(path)
        .with_context(|| format!("resolving {option} {}", path.display()))?;
    anyhow::ensure!(
        directory.is_dir(),
        "{option} must be a directory: {}",
        directory.display()
    );
    Ok(directory)
}

/// The private runtime directory a command acts on.
#[derive(Debug, Args, Clone, Default)]
pub struct RuntimeDirArgs {
    /// Private runtime directory shared by this host and its wrappers.
    #[arg(long, global = true, env = "CONVERSATIONS_HOST_DIR")]
    pub runtime_dir: Option<PathBuf>,
}

impl RuntimeDirArgs {
    fn resolve(&self) -> Result<PathBuf> {
        let dir = self.runtime_dir.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".conversations-host-local")
        });
        Ok(std::path::absolute(dir)?)
    }
}

/// Run the local host in the foreground, or manage it with a subcommand.
#[derive(Debug, Args)]
pub struct HostArgs {
    #[command(flatten)]
    pub runtime: RuntimeDirArgs,
    #[command(flatten)]
    pub startup: HostStartup,
    #[command(subcommand)]
    pub command: Option<HostCommand>,
}

impl HostArgs {
    /// Startup options select how bare `host` runs; with a subcommand they
    /// would be silently ignored.
    pub fn startup_conflict(&self) -> Option<&'static str> {
        let used = [
            self.startup.backend.is_some().then_some("--backend"),
            self.startup.cmux_bin.is_some().then_some("--cmux-bin"),
            self.startup
                .cookie_file
                .is_some()
                .then_some("--cookie-file"),
        ];
        self.command
            .as_ref()
            .and_then(|_| used.into_iter().flatten().next())
    }
}

/// Startup options of the foreground host.
#[derive(Debug, Args, Default)]
pub struct HostStartup {
    /// Terminal backend [default: auto: cmux inside a reachable cmux
    /// workspace, otherwise normal].
    #[arg(long, value_enum)]
    pub backend: Option<Backend>,
    /// Override cmux CLI discovery (useful for isolated compatibility tests).
    #[arg(long)]
    pub cmux_bin: Option<PathBuf>,
    /// Read the localhost session cookie from an owner-only file, without changing saved login defaults.
    #[arg(long)]
    pub cookie_file: Option<PathBuf>,
}

/// Top-level `start-agent`.
#[derive(Debug, Args)]
pub struct StartAgentArgs {
    #[command(flatten)]
    pub runtime: RuntimeDirArgs,
    /// Agent to start.
    #[arg(value_parser = clap::builder::PossibleValuesParser::new(["claude", "codex", "opencode"])
        .map(|agent| agent.parse::<AgentKind>().expect("a listed agent")))]
    pub agent: AgentKind,
    /// Resume this native session id (a Claude or Codex session id, or an
    /// OpenCode `ses_…` id) under its one canonical local session.
    #[arg(long, value_name = "ID")]
    pub resume: Option<String>,
    /// Working directory [default: the current directory].
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// How to invoke the agent: `opencode` runs the agent's own binary, `adk`
    /// runs OpenCode through the Arrakis dev harness (`adk code`), which needs
    /// `ARRAKIS_HOME` and opens the monorepo root. `adk` starts OpenCode only.
    #[arg(long, default_value_t = AgentLauncher::Opencode, env = "CONVERSATIONS_LAUNCHER")]
    pub launcher: AgentLauncher,
    /// Let an `adk code` session draw its tmux status strip.
    #[arg(long)]
    pub adk_statusline: bool,
    /// Arguments after `--` are passed to the agent verbatim.
    #[arg(last = true, value_name = "AGENT_ARG")]
    pub args: Vec<String>,
}

/// Top-level `credential`.
#[derive(Debug, Args)]
pub struct CredentialArgs {
    #[command(flatten)]
    pub runtime: RuntimeDirArgs,
    #[command(subcommand)]
    pub command: CredentialCommand,
}

/// Top-level `passkey`.
#[derive(Debug, Args)]
pub struct PasskeyArgs {
    #[command(flatten)]
    pub runtime: RuntimeDirArgs,
    #[command(subcommand)]
    pub command: PasskeyCommand,
}

/// Top-level `history`.
#[derive(Debug, Args)]
pub struct HistoryArgs {
    #[command(flatten)]
    pub runtime: RuntimeDirArgs,
    #[command(subcommand)]
    pub command: HistoryCommand,
}

/// Top-level `open`.
#[derive(Debug, Args)]
pub struct OpenArgs {
    #[command(flatten)]
    pub runtime: RuntimeDirArgs,
    /// Revoke every remembered browser and active local terminal connection, then open setup.
    #[arg(long)]
    pub reset_browser_access: bool,
}

#[derive(Debug, Subcommand)]
pub enum HostCommand {
    /// Internal native lifecycle callback. Emits no agent-visible output.
    #[command(hide = true)]
    AgentHook {
        #[arg(long)]
        launch_id: uuid::Uuid,
        #[arg(long)]
        wrapper_pid: u32,
    },
    /// Install or remove the optional cmux terminal startup command.
    InstallCmux {
        /// Ghostty configuration file (normally discovered automatically).
        #[arg(long)]
        config_file: Option<PathBuf>,
        /// Enable wrapping when installing; otherwise preserve the preference.
        #[arg(long, conflicts_with = "uninstall")]
        enable: bool,
        /// Remove only the Conversations startup command block.
        #[arg(long)]
        uninstall: bool,
    },
    /// Show or update this local host's persistent startup settings.
    Configure {
        /// Automatically wrap new cmux shells through the installed startup launcher.
        #[arg(long, action = clap::ArgAction::Set)]
        auto_wrap: Option<bool>,
        /// The account shared history is published as (must be the account this host logs in with).
        #[arg(long)]
        publish_account: Option<String>,
        /// The Arrakis DevKit checkout the daemon should use as `ARRAKIS_HOME` for
        /// `adk code` launches when no ambient `ARRAKIS_HOME` is set. Pass an
        /// empty string to clear a previously configured value.
        #[arg(long, value_name = "PATH")]
        arrakis_home: Option<String>,
        /// Directories prepended to the daemon's launcher lookup `PATH`
        /// (colon-separated, standard `PATH` format). Used only to resolve
        /// `adk`, `codex`, `claude`, and `opencode`. Pass an empty string to
        /// clear previously configured entries.
        #[arg(long, value_name = "DIR:DIR:...")]
        launcher_path: Option<String>,
        /// Serve the owner-only loopback web UI when this host starts.
        #[arg(long, action = clap::ArgAction::Set)]
        web: Option<bool>,
        /// Loopback port for the web UI.
        #[arg(long, value_name = "PORT")]
        web_port: Option<u16>,
    },
    /// Terminal startup launcher: wrap when configured, otherwise execute the normal shell.
    #[command(hide = true)]
    Shell {
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Wrap a terminal command; defaults to the login shell. Does not contact the server.
    Wrap {
        #[arg(long)]
        session_id: Option<uuid::Uuid>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long, default_value_t = AgentKind::Zsh)]
        agent: AgentKind,
        /// No local stdin/stdout bridge (normal backend's browser-only sessions).
        #[arg(long)]
        headless: bool,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// List connected terminal wrappers and their cmux workspaces.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Prepare terminal-local OpenCode identity integration immediately before exec.
    #[command(hide = true)]
    OpencodeBridge {
        #[arg(long)]
        launch_id: uuid::Uuid,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum CredentialCommand {
    /// Approve a public key on THIS machine. Nothing a server or account holds
    /// can add a credential; this command is the approval.
    Add {
        /// `owner` (may start sessions) or `agent` (never may).
        #[arg(long)]
        kind: String,
        /// `ed25519` (raw 32-byte key) or `es256` (uncompressed P-256 point).
        #[arg(long, default_value = "ed25519")]
        alg: String,
        /// Standard-base64 public key.
        #[arg(long)]
        public_key: String,
        #[arg(long)]
        label: String,
        /// Repeatable: `session.start`, or `session.write:<session-uuid>`.
        #[arg(long = "grant", required = true)]
        grants: Vec<String>,
    },
    /// Permanently revoke a locally approved credential: its grants, the
    /// browser delegations it parents and its undelivered agent messages stop
    /// at once, including work already queued. A running host is then asked to
    /// interrupt the current interaction of affected sessions (never ending
    /// them); when it is not running the revocation still holds.
    Revoke {
        /// The credential id printed by `credential add` / `passkey approve`.
        /// Ids are base64url, so one may start with `-` or `--`.
        #[arg(allow_hyphen_values = true)]
        credential_id: String,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print this host's audience id (`aud` in signed operations).
    Identity,
    /// Print the pairing descriptor (public key) to paste into the browser.
    Descriptor,
    /// Print THIS machine's agent signing key. The recipient approves it once with
    /// `credential add --kind agent --public-key <this>` and grants it
    /// `session.write:<session>` per session it may message.
    AgentKey,
    /// Pin a recipient session for agent messages: its host descriptor (from
    /// `credential descriptor` on that machine) and session id. Messages are
    /// signed for exactly that host; discovery never chooses a destination.
    PairPeer {
        /// The recipient host descriptor (`chd1.…`).
        #[arg(long = "host")]
        host: String,
        #[arg(long)]
        session: String,
        #[arg(long)]
        label: String,
    },
    /// Forget a pinned recipient.
    UnpairPeer {
        #[arg(long)]
        session: String,
    },
    /// List pinned recipients.
    Peers,
}

#[derive(Debug, Subcommand)]
pub enum PasskeyCommand {
    /// Begin registering a passkey; prints the WebAuthn creation options.
    EnrollStart {
        #[arg(long)]
        label: String,
        /// `owner` or `agent`.
        #[arg(long, default_value = "owner")]
        kind: String,
        /// Repeatable grants the credential may hold, e.g. `session.start`.
        #[arg(long = "grant", required = true)]
        grants: Vec<String>,
    },
    /// Complete a registration with the authenticator response (JSON file).
    EnrollFinish {
        #[arg(long)]
        challenge: String,
        #[arg(long)]
        response_file: PathBuf,
    },
    /// List verified registrations waiting for local approval.
    Pending,
    /// Approve a pending registration as a permanent credential.
    Approve { cred_id: String },
    /// Begin a 24-hour browser delegation; prints the WebAuthn request options.
    DelegateStart {
        /// Standard-base64 uncompressed P-256 point of the browser key.
        #[arg(long)]
        browser_pub: String,
        /// 32 lowercase hex characters chosen by the browser.
        #[arg(long)]
        dlg: String,
        #[arg(long = "cap", required = true)]
        caps: Vec<String>,
    },
    /// Complete a delegation with the passkey assertion (JSON file).
    DelegateFinish {
        #[arg(long)]
        challenge: String,
        #[arg(long)]
        response_file: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    /// Download owned server histories to this host. Never starts agents or grants access.
    Import {
        /// Import just this Conversations session id; omit for all owned sessions.
        #[arg(long)]
        session: Option<String>,
        /// Print aggregate import counts as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print one session's stored messages in order.
    Get {
        /// The Conversations session id.
        session: String,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// One JSON message per line (full content blocks).
        #[arg(long)]
        json: bool,
    },
    /// Full-text search of locally stored messages; all words must match.
    Search {
        query: String,
        /// Restrict the search to one session.
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Publish one session's local history (past and future) to the server,
    /// readable by the chosen audience. Grants no access to this host.
    #[command(group(clap::ArgGroup::new("audience").required(true).args(["organization", "email"])))]
    Share {
        session: String,
        /// Everyone in the organization, including future members.
        #[arg(long)]
        organization: bool,
        /// Only these people (repeat for several).
        #[arg(long)]
        email: Vec<String>,
    },
    /// Stop publishing one session and delete its server copy. Local history
    /// and the running agent are not touched.
    Unshare { session: String },
    /// Show one session's publication state.
    Status {
        session: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Pick cmux when launched inside a reachable cmux workspace, otherwise normal.
    Auto,
    Normal,
    Cmux,
}

/// Bare `host` runs the daemon in the foreground; a subcommand manages it
/// and never starts one.
pub fn run(global: &GlobalOpts, args: &HostArgs) -> Result<()> {
    let dir = args.runtime.resolve()?;
    let Some(command) = &args.command else {
        return run_daemon(global, &dir, &args.startup);
    };
    if let HostCommand::OpencodeBridge { launch_id, command } = command {
        return opencode_bridge::exec(&dir, &launch_id.to_string(), command);
    }
    if let HostCommand::AgentHook {
        launch_id,
        wrapper_pid,
    } = command
    {
        // A reporting failure must never block an agent turn or approve a tool.
        let _ = readiness::claude_hook(
            &dir,
            &launch_id.to_string(),
            *wrapper_pid,
            std::io::stdin().lock(),
        );
        return Ok(());
    }
    match command {
        HostCommand::InstallCmux {
            config_file,
            enable,
            uninstall,
        } => {
            installer::run(&dir, config_file.as_deref(), *enable, *uninstall)?;
            return Ok(());
        }
        HostCommand::Configure {
            auto_wrap,
            publish_account,
            arrakis_home,
            launcher_path,
            web,
            web_port,
        } => {
            let mut settings = settings::Settings::load(&dir)?;
            if let Some(enabled) = auto_wrap {
                settings.cmux.auto_wrap = *enabled;
            }
            if let Some(account) = publish_account {
                let account = account.trim().to_lowercase();
                anyhow::ensure!(
                    publication::valid_email(&account),
                    "not an email address: {account}"
                );
                settings.publication.account = Some(account);
            }
            if let Some(path) = arrakis_home {
                settings.host_env.arrakis_home = if path.is_empty() {
                    None
                } else {
                    Some(configured_directory(Path::new(path), "--arrakis-home")?)
                };
            }
            if let Some(raw) = launcher_path {
                settings.host_env.launcher_path = if raw.trim().is_empty() {
                    Vec::new()
                } else {
                    std::env::split_paths(raw.as_str())
                        .map(|entry| configured_directory(&entry, "--launcher-path entry"))
                        .collect::<Result<_>>()?
                };
            }
            if let Some(enabled) = web {
                settings.web.enabled = *enabled;
            }
            if let Some(port) = web_port {
                settings.web.port = *port;
            }
            if auto_wrap.is_some()
                || publish_account.is_some()
                || arrakis_home.is_some()
                || launcher_path.is_some()
                || web.is_some()
                || web_port.is_some()
            {
                settings.save()?;
            }
            let host_env = &settings.host_env;
            println!(
                "Automatic cmux wrapping: {}\n\
                 Publication account: {}\n\
                 ARRAKIS_HOME (startup recipe): {}\n\
                 Launcher PATH prefix: {}\n\
                 Local web UI (starts with the host): {}\n\
                 Setting: {}",
                if settings.cmux.auto_wrap { "on" } else { "off" },
                if settings.publication.account.is_some() {
                    "configured"
                } else {
                    "none (only a server with authentication off accepts this)"
                },
                host_env
                    .arrakis_home
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "none (ambient env only)".into()),
                if host_env.launcher_path.is_empty() {
                    "none (ambient PATH only)".into()
                } else {
                    host_env
                        .launcher_path
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(":")
                },
                if settings.web.enabled {
                    format!("on, http://localhost:{}/", settings.web.port)
                } else {
                    "off".into()
                },
                settings::Settings::path()?.display()
            );
            if web.is_some() || web_port.is_some() {
                println!("Web UI changes apply when the host next starts.");
            }
            return Ok(());
        }
        HostCommand::Shell { command } => {
            // Host identity follows its surface when moved between workspaces.
            let cmux_context = std::env::var_os("CMUX_WORKSPACE_ID").is_some()
                && std::env::var_os("CONVERSATIONS_WRAPPED").is_none();
            let host_control = cmux_context
                && (std::env::var("CONVERSATIONS_HOST_CONTROL").as_deref() == Ok("1")
                    || std::env::var("CMUX_SURFACE_ID")
                        .ok()
                        .is_some_and(|surface| {
                            tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .ok()
                                .and_then(|runtime| {
                                    runtime
                                        .block_on(async {
                                            cmux::Cmux::connect(None)
                                                .await?
                                                .resume_binding(&surface)
                                                .await
                                        })
                                        .ok()
                                        .flatten()
                                })
                                .is_some_and(|binding| {
                                    binding["kind"] == "conversations"
                                        && binding["checkpoint_id"] == cmux::host_checkpoint(&dir)
                                })
                        }));
            if host_control
                && std::os::unix::net::UnixStream::connect(dir.join("host.sock")).is_err()
            {
                let result = run_daemon(
                    global,
                    &dir,
                    &HostStartup {
                        backend: Some(Backend::Cmux),
                        ..HostStartup::default()
                    },
                );
                // Another restored host may have won the runtime lock after our
                // socket check. Keep this pane usable in that case.
                if result.is_ok()
                    || std::os::unix::net::UnixStream::connect(dir.join("host.sock")).is_err()
                {
                    return result;
                }
            }
            if host_control {
                // This pane lost the host election and is now an ordinary shell.
                // Do not leave a marker that would make bootstrap wake a shell
                // forever instead of starting a daemon.
                if let Ok(surface) = std::env::var("CMUX_SURFACE_ID") {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    if let Err(error) = runtime.block_on(async {
                        cmux::Cmux::connect(None)
                            .await?
                            .clear_resume_binding(&surface)
                            .await
                    }) {
                        eprintln!("Cannot clear inactive host binding: {error:#}");
                    }
                }
            }
            let enabled = match settings::Settings::load(&dir) {
                Ok(settings) => settings.cmux.auto_wrap,
                Err(error) => {
                    eprintln!("Conversations auto-wrap disabled: {error:#}");
                    false
                }
            };
            let command = default_command(AgentKind::Zsh, command);
            if enabled
                && std::env::var_os("CMUX_WORKSPACE_ID").is_some()
                && std::env::var_os("CONVERSATIONS_WRAPPED").is_none()
            {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                // Identity belongs to one surface, never a workspace-wide
                // environment variable inherited by new tabs and splits.
                let saved = if let Ok(surface) = std::env::var("CMUX_SURFACE_ID") {
                    runtime
                        .block_on(async {
                            cmux::Cmux::connect(None)
                                .await?
                                .saved_session_for_surface(&dir, &global.server, &surface)
                                .await
                        })
                        .unwrap_or_else(|error| {
                            eprintln!("Conversations restore lookup: {error:#}");
                            None
                        })
                } else {
                    None
                };
                let capture =
                    saved.is_some() || std::env::var("CONVERSATIONS_CAPTURE").as_deref() != Ok("0");
                if capture {
                    if let Err(error) = bootstrap_host(&dir, &global.server) {
                        eprintln!("Conversations host startup: {error:#}");
                    }
                }
                if let Some(saved) = saved {
                    let restored = (|| -> Result<()> {
                        // No running daemon here (we're the terminal's own
                        // bootstrap). Read the persisted startup recipe so an
                        // `adk code` resume finds the Arrakis DevKit checkout the
                        // same way the daemon will.
                        let launch_env = daemon::LaunchEnv::from_settings(
                            &settings::Settings::load(&dir)?.host_env,
                        );
                        let mut launch = daemon::launch_from_saved_session(
                            saved,
                            &std::env::current_dir()?,
                            &launch_env,
                        )?;
                        if launch.agent == AgentKind::OpenCode {
                            launch.command =
                                opencode_bridge::command(&dir, &launch.id, launch.command)?;
                        }
                        launch.command = cmux::login_command(&launch.command);
                        print!("\x1b[2J\x1b[H");
                        runtime.block_on(wrapper::run(&dir, launch, false, &global.server))?;
                        Ok(())
                    })();
                    match restored {
                        Ok(()) => return Ok(()),
                        Err(error) => eprintln!("Cannot restore conversation; keeping checkpoint and starting a shell: {error:#}"),
                    }
                }
                if capture {
                    if let Err(error) = run(
                        global,
                        &HostArgs {
                            runtime: RuntimeDirArgs {
                                runtime_dir: Some(dir),
                            },
                            startup: HostStartup::default(),
                            command: Some(HostCommand::Wrap {
                                session_id: None,
                                cwd: None,
                                agent: AgentKind::Zsh,
                                headless: false,
                                command: command.clone(),
                            }),
                        },
                    ) {
                        eprintln!(
                            "Conversations auto-wrap failed; starting ordinary shell: {error:#}"
                        );
                    } else {
                        return Ok(());
                    }
                }
            }
            use std::os::unix::process::CommandExt;
            let error = std::process::Command::new(&command[0])
                .args(&command[1..])
                .exec();
            return Err(error).context("starting ordinary shell");
        }
        _ => {}
    }
    block_on(async {
        match command {
            HostCommand::InstallCmux { .. }
            | HostCommand::Configure { .. }
            | HostCommand::Shell { .. }
            | HostCommand::OpencodeBridge { .. }
            | HostCommand::AgentHook { .. } => unreachable!(),
            HostCommand::Wrap {
                session_id,
                cwd,
                agent,
                headless,
                command,
            } => {
                let id = session_id.unwrap_or_else(uuid::Uuid::new_v4).to_string();
                let command = default_command(*agent, command);
                let command = if *agent == AgentKind::OpenCode {
                    opencode_bridge::command(&dir, &id, command)?
                } else {
                    command
                };
                let code = wrapper::run(
                    &dir,
                    protocol::Launch {
                        id,
                        agent: *agent,
                        cwd: cwd
                            .as_ref()
                            .map(std::path::absolute)
                            .transpose()?
                            .unwrap_or(std::env::current_dir()?),
                        command,
                    },
                    *headless,
                    &global.server,
                )
                .await?;
                std::process::exit(i32::try_from(code).unwrap_or(1));
            }
            HostCommand::Status { json } => match request(&dir, ClientMessage::Status).await? {
                ServerMessage::Status {
                    backend, sessions, ..
                } => {
                    if *json {
                        println!(
                            "{}",
                            serde_json::json!({"backend":backend,"sessions":sessions})
                        );
                    } else {
                        println!("{backend:?} host: {} terminal(s)", sessions.len());
                        for session in sessions {
                            println!(
                                "{}  {}  {}  {}{}",
                                session.registration.launch.id,
                                session.registration.launch.agent,
                                session.registration.workspace.as_deref().unwrap_or("-"),
                                session.registration.launch.cwd.display(),
                                if session.registration.effect_gate {
                                    ""
                                } else {
                                    "  (upgrade required: no remote input)"
                                }
                            );
                        }
                    }
                    Ok(())
                }
                ServerMessage::Error { message } => bail!("{message}"),
                _ => bail!("unexpected host response"),
            },
        }
    })
}

fn run_daemon(global: &GlobalOpts, dir: &Path, startup: &HostStartup) -> Result<()> {
    validate_server(&global.server)?;
    block_on(daemon::run(
        global,
        dir,
        startup.backend.unwrap_or(Backend::Auto),
        startup.cmux_bin.as_deref(),
        startup.cookie_file.as_deref(),
    ))
}

fn block_on(work: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(work);
    // GitHub reads already in flight on blocking threads must not delay a
    // host-only restart or keep disconnected wrappers waiting indefinitely.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

/// `credential`: approval state lives in the runtime directory and works
/// whether or not a host is running.
pub fn credential(args: &CredentialArgs) -> Result<()> {
    let dir = args.runtime.resolve()?;
    match &args.command {
        CredentialCommand::Add {
            kind,
            alg,
            public_key,
            label,
            grants,
        } => {
            let (credential, host_id) =
                authz::enroll_from_cli(&dir, kind, alg, public_key, label, grants)?;
            println!("Approved credential {credential}\nHost audience (aud): {host_id}");
        }
        CredentialCommand::Revoke {
            credential_id,
            json,
        } => revoke_credential(&dir, credential_id, *json)?,
        CredentialCommand::Identity => println!("{}", authz::host_id(&dir)?),
        CredentialCommand::Descriptor => println!("{}", authz::attest::descriptor(&dir)?),
        CredentialCommand::AgentKey => {
            let (public, kid) = agent_link::agent_public(&dir)?;
            println!("Agent public key (standard base64): {public}\nKey id: {kid}");
        }
        CredentialCommand::PairPeer {
            host,
            session,
            label,
        } => {
            let id = agent_link::pair_peer(&dir, host, session, label)?;
            println!("Paired session {session} on host {id}");
        }
        CredentialCommand::UnpairPeer { session } => {
            authz::ensure_dir(&dir)?;
            let removed = authz::store::Store::open(&dir)?.target_remove(session)?;
            println!("{}", if removed { "Unpaired" } else { "Not paired" });
        }
        CredentialCommand::Peers => {
            authz::ensure_dir(&dir)?;
            for (session, host, label) in authz::store::Store::open(&dir)?.targets()? {
                println!("{session}  host {host}  {label}");
            }
        }
    }
    Ok(())
}

/// `history`: local history reads and publication state in the runtime
/// directory; `import` also contacts the server.
pub fn history(global: &GlobalOpts, args: &HistoryArgs) -> Result<()> {
    let dir = args.runtime.resolve()?;
    match &args.command {
        HistoryCommand::Import { session, json } => {
            import::run(&dir, &global.server, session.as_deref(), *json)?
        }
        HistoryCommand::Get {
            session,
            limit,
            json,
        } => history::run_get(&dir, session, *limit, *json)?,
        HistoryCommand::Search {
            query,
            session,
            limit,
        } => history::run_search(&dir, query, session.as_deref(), *limit)?,
        HistoryCommand::Share {
            session,
            organization,
            email,
        } => {
            let audience = if *organization {
                publication::Audience::Organization {}
            } else {
                publication::Audience::Restricted {
                    emails: email.clone(),
                    write_pending: Vec::new(),
                }
            };
            let settings = settings::Settings::load(&dir)?;
            let owner = publication::configured_owner(settings.publication.account.as_deref())?;
            let status =
                publication::share(&dir, session, audience, owner.as_deref(), &global.server)?;
            println!("{}", publication::render(&status));
            println!("The running host publishes it; check with `history status {session}`.");
        }
        HistoryCommand::Unshare { session } => {
            let status = publication::unshare(&dir, session)?;
            println!("{}", publication::render(&status));
        }
        HistoryCommand::Status { session, json } => {
            let status = publication::status(&dir, session)?;
            if *json {
                println!("{}", serde_json::to_string(&status)?);
            } else {
                println!("{}", publication::render(&status));
            }
        }
    }
    Ok(())
}

/// `passkey`: performed by the running host over its owner-only socket.
pub fn passkey(global: &GlobalOpts, args: &PasskeyArgs) -> Result<()> {
    let dir = args.runtime.resolve()?;
    let command = &args.command;
    block_on(async {
        let read = |path: &Path| -> Result<serde_json::Value> {
            serde_json::from_slice(&std::fs::read(path)?).context("response file is not JSON")
        };
        let request = match command {
            PasskeyCommand::EnrollStart {
                label,
                kind,
                grants,
            } => authz::passkey::PasskeyRequest::EnrollStart {
                label: label.clone(),
                kind: kind.clone(),
                grants: grants.clone(),
            },
            PasskeyCommand::EnrollFinish {
                challenge,
                response_file,
            } => authz::passkey::PasskeyRequest::EnrollFinish {
                challenge: challenge.clone(),
                response: read(response_file)?,
                // The local CLI path never carries a cross-host vouch;
                // that is the browser ceremony's proof to present.
                vouch: None,
            },
            PasskeyCommand::Pending => authz::passkey::PasskeyRequest::Pending,
            PasskeyCommand::Approve { cred_id } => authz::passkey::PasskeyRequest::Approve {
                cred_id: cred_id.clone(),
            },
            PasskeyCommand::DelegateStart {
                browser_pub,
                dlg,
                caps,
            } => authz::passkey::PasskeyRequest::DelegateStart {
                browser_pub: browser_pub.clone(),
                dlg: dlg.clone(),
                caps: caps.clone(),
                nonce: uuid::Uuid::new_v4().simple().to_string(),
            },
            PasskeyCommand::DelegateFinish {
                challenge,
                response_file,
            } => authz::passkey::PasskeyRequest::DelegateFinish {
                challenge: challenge.clone(),
                response: read(response_file)?,
            },
        };
        match self::request(&dir, ClientMessage::Passkey { request }).await? {
            ServerMessage::Passkey {
                response: Some(response),
                ..
            } => {
                println!("{}", serde_json::to_string_pretty(&response)?);
                if let (
                    PasskeyCommand::EnrollStart { .. },
                    authz::passkey::PasskeyResponse::Started { challenge, options },
                ) = (command, &response)
                {
                    println!(
                            "\nOpen this link in the browser where you want the passkey. Everything after the\n# stays in that browser and is never sent to the server:\n\n  {}\n\nThen approve the registration here: `passkey pending`, `passkey approve <id>`.",
                            authz::passkey::enrollment_link(
                                &global.server,
                                &authz::attest::descriptor(&dir)?,
                                challenge,
                                options,
                            )?
                        );
                }
                Ok(())
            }
            ServerMessage::Passkey {
                error: Some(error), ..
            } => bail!("{error}"),
            other => bail!("unexpected host reply: {other:?}"),
        }
    })
}

/// `start-agent`: run a managed agent in this terminal, registered with the
/// running local host. It never starts a host and never opens, moves or
/// reorders a workspace; the agent's exit code is the command's.
pub fn start_agent(global: &GlobalOpts, args: &StartAgentArgs) -> Result<()> {
    use crate::commands::live::host::ControlCommand;
    if !args.launcher.supports(args.agent) {
        bail!(
            "`--launcher {}` launches OpenCode only; got `{}`",
            args.launcher,
            args.agent
        );
    }
    let dir = match &args.runtime.runtime_dir {
        Some(dir) => std::path::absolute(dir)?,
        None => agent_link::runtime_for(&global.server),
    };
    let cwd = match &args.cwd {
        Some(cwd) => std::path::absolute(cwd)?,
        None => std::env::current_dir()?,
    };
    block_on(async {
        // Nothing starts and nothing is recorded unless the host answers.
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            request(&dir, ClientMessage::Status),
        )
        .await
        .with_context(|| {
            format!(
                "the local host in {} did not answer; start it with `conversations host`",
                dir.display()
            )
        })??;
        match status {
            ServerMessage::Status {
                in_place_sessions: true,
                ..
            } => {}
            // An older host would move or detach this terminal's session.
            ServerMessage::Status { .. } => bail!(
                "the local host in {} is too old for `start-agent`; restart it with the current \
                 `conversations host`",
                dir.display()
            ),
            ServerMessage::Error { message } => bail!("{message}"),
            _ => bail!("unexpected host response"),
        }
        let settings = settings::Settings::load(&dir)?;
        let env = daemon::LaunchEnv::from_settings(&settings.host_env);
        let mut agent_args = settings.launch.agent_args(args.agent)?;
        agent_args.extend(args.args.iter().cloned());
        let command = match &args.resume {
            None => ControlCommand::StartNew {
                cwd: None,
                agent: args.agent,
                agent_args,
                launcher: args.launcher,
                adk_statusline: args.adk_statusline,
            },
            Some(native) => {
                let launch_cwd = daemon::local_launcher_cwd(args.launcher, &cwd, &env)?
                    .to_string_lossy()
                    .into_owned();
                ControlCommand::Reopen {
                    session_id: authz::adopt_native(&dir, args.agent, native, Some(&launch_cwd))?,
                    cwd: launch_cwd,
                    agent: args.agent,
                    resume_id: Some(native.clone()),
                    agent_args,
                    launcher: args.launcher,
                    adk_statusline: args.adk_statusline,
                }
            }
        };
        let mut launch = daemon::launch_from_control(command, &cwd, &env)?;
        daemon::pin_native_executable(&mut launch, &env)?;
        if args.agent == AgentKind::OpenCode {
            launch.command = opencode_bridge::command(&dir, &launch.id, launch.command)?;
        }
        let code = wrapper::run_in_place(&dir, launch, &global.server).await?;
        std::process::exit(i32::try_from(code).unwrap_or(1));
    })
}

/// `open`: ask the running host for a fresh one-time owner link to the UI it
/// serves and hand it to the browser. Never serves the UI itself.
pub fn open(args: &OpenArgs) -> Result<()> {
    let dir = args.runtime.resolve()?;
    let command = if args.reset_browser_access {
        ClientMessage::WebBootstrapReset
    } else {
        ClientMessage::WebBootstrap
    };
    block_on(async {
        match request(&dir, command).await? {
            ServerMessage::WebBootstrap {
                url: Some(bootstrap),
            } => {
                // The token stays in the fragment, which only the browser sees.
                let public = bootstrap.split('#').next().unwrap_or(&bootstrap);
                println!("Local host management: {public}");
                web::open_local_management(&bootstrap)
            }
            ServerMessage::WebBootstrap { url: None } => bail!(
                "the running host is not serving its local web UI: it is turned off, or its port \
                 was unavailable when the host started (see the host's output). Turn it on with \
                 `conversations host configure --web true` (and `--web-port <PORT>` for a \
                 free port), then restart `conversations host`."
            ),
            ServerMessage::Error { message } => bail!("{message}"),
            _ => bail!("unexpected host response"),
        }
    })
}

fn bootstrap_host(dir: &std::path::Path, server: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::time::{Duration, Instant};

    if std::os::unix::net::UnixStream::connect(dir.join("host.sock")).is_ok() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(dir)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("host-bootstrap.lock"))?;
    // Concurrent cmux startup shells share one host creation attempt.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(());
    }
    if std::os::unix::net::UnixStream::connect(dir.join("host.sock")).is_ok() {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        cmux::Cmux::connect(None)
            .await?
            .start_host(dir, server)
            .await
    })?;
    // Allow both auto-detection probes in addition to host startup.
    let deadline = Instant::now() + Duration::from_secs(10) + cmux::RPC_TIMEOUT.saturating_mul(2);
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(dir.join("host.sock")).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("cmux created a host workspace, but its local socket did not start")
}

pub(super) fn default_command(agent: AgentKind, command: &[String]) -> Vec<String> {
    if !command.is_empty() {
        return command.to_vec();
    }
    if agent == AgentKind::Zsh {
        vec![
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
            "-l".into(),
        ]
    } else {
        vec![agent.as_str().into()]
    }
}

/// `credential revoke`: commit the revocation locally first (it holds
/// whether or not a host is running), then ask a running host to interrupt the
/// affected interactions and report exactly what it did.
fn revoke_credential(dir: &Path, credential_id: &str, json: bool) -> Result<()> {
    let (revoked, interrupted) = revoke_and_interrupt(dir, credential_id)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "credential": credential_id,
                "already_revoked": revoked.already,
                "grants": revoked.grants,
                "delegations": revoked.delegations,
                "messages": revoked.messages,
                "interruption": match &interrupted {
                    Some(sessions) => serde_json::json!({"available": true, "sessions": sessions}),
                    None => serde_json::json!({"available": false, "sessions": []}),
                },
            })
        );
        return Ok(());
    }
    if revoked.already {
        println!("Credential {credential_id} was already revoked");
    } else {
        println!(
            "Revoked credential {credential_id}: {} grant(s), {} delegation(s), {} queued message(s)",
            revoked.grants, revoked.delegations, revoked.messages
        );
    }
    match interrupted {
        None => println!(
            "Interruption unavailable: the local host is not running. Queued work stays refused; an interaction already running was not interrupted."
        ),
        Some(sessions) if sessions.is_empty() => {
            println!("No running interaction used this credential")
        }
        Some(sessions) => {
            for interruption in sessions {
                let outcome = protocol::CancelOutcome::describe(interruption.outcome);
                println!("Session {}: {outcome}", interruption.session);
            }
        }
    }
    Ok(())
}

/// The revocation, then the running host's interruptions (`None`: no host
/// answered, so nothing could be interrupted now).
fn revoke_and_interrupt(
    dir: &Path,
    credential_id: &str,
) -> Result<(
    authz::store::Revocation,
    Option<Vec<protocol::Interruption>>,
)> {
    authz::ensure_dir(dir)?;
    let revoked = authz::store::Store::open(dir)?
        .revoke_credential(credential_id, authz::store::now_secs())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let interrupted = runtime.block_on(async {
        match request(dir, ClientMessage::AuthorityChanged).await {
            Ok(ServerMessage::Interruptions { sessions }) => Some(sessions),
            _ => None,
        }
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    Ok((revoked, interrupted))
}

/// Cheap, race-tolerant presence check for the daemon socket. A `true` here
/// means `request` is worth attempting; the daemon can still exit between
/// the check and the connect, so callers must handle a connect failure.
pub(crate) async fn host_sock_present(dir: &Path) -> bool {
    tokio::fs::metadata(dir.join("host.sock")).await.is_ok()
}

pub(crate) async fn request(dir: &Path, message: ClientMessage) -> Result<ServerMessage> {
    let mut socket = tokio::net::UnixStream::connect(dir.join("host.sock"))
        .await
        .with_context(|| {
            format!(
                "no local host is answering in {}; start it with `conversations host`",
                dir.display()
            )
        })?;
    write_message(&mut socket, &message).await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        read_message(&mut socket),
    )
    .await?
}

/// Permit HTTPS servers and local HTTP development without sending cookies over remote HTTP.
fn validate_server(server: &str) -> Result<()> {
    let url = reqwest::Url::parse(server)?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("host requires an HTTPS server or an HTTP loopback origin");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_directory_rejects_empty_launcher_segment() {
        configured_directory(Path::new(""), "--launcher-path entry")
            .expect_err("an empty PATH segment must not authorize the current directory");
    }

    #[test]
    fn server_origin_requires_tls_off_loopback() {
        for url in [
            "http://localhost.example.com",
            "http://localhost@evil.test",
            "http://127.0.0.1/api",
            "http://192.168.1.2",
            "https://conversations.example.com/api",
            "https://user:pass@conversations.example.com",
        ] {
            assert!(validate_server(url).is_err(), "{url:?}");
        }
        for url in [
            "https://conversations.example.com",
            "http://127.0.0.1:8097",
            "http://localhost:8097",
            "http://[::1]:8097",
        ] {
            validate_server(url).unwrap();
        }
    }
}
