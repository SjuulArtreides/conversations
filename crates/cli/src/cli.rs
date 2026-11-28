//! clap command-line interface definition for `conversations`.

use clap::{Args, Parser, Subcommand};
use conversations_core::AgentKind;

/// Version string shown by `--version`: the crate version plus the short git
/// SHA captured at build time (see `build.rs`), e.g. `0.2.0 (78d747e)`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_SHA"), ")");

/// Built-in fallback server, used when neither a flag/env nor a saved default
/// (see `config::default_server`) is present.
pub const DEFAULT_SERVER: &str = "http://localhost:8080";

/// Upload local Claude Code session histories to a conversations server.
#[derive(Debug, Parser)]
#[command(name = "conversations", version = VERSION, about)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[command(subcommand)]
    pub command: Command,
}

/// Options shared by every subcommand. Each is also settable via an env var so
/// the tool is easy to drive from CI or a self-hosted shell profile.
#[derive(Debug, Args, Clone)]
pub struct GlobalOpts {
    /// Base URL of the conversations server. When omitted, falls back to the
    /// saved default server (set during `login`, stored in
    /// `config.json`), then to `http://localhost:8080`.
    #[arg(
        long = "server",
        value_name = "URL",
        global = true,
        env = "CONVERSATIONS_SERVER"
    )]
    pub server_arg: Option<String>,

    /// Effective server base URL, resolved in `main` from `server_arg`, the
    /// saved default, or the built-in fallback. Not a CLI flag — every
    /// subcommand reads this rather than `server_arg`.
    #[arg(skip)]
    pub server: String,

    /// Override the Claude projects directory (default: `~/.claude/projects`).
    #[arg(long, global = true, env = "CLAUDE_PROJECTS_DIR")]
    pub projects_dir: Option<std::path::PathBuf>,

    /// Override the Codex sessions directory (default: `~/.codex/sessions`).
    #[arg(long, global = true, env = "CODEX_SESSIONS_DIR")]
    pub codex_sessions_dir: Option<std::path::PathBuf>,

    /// Override the OpenCode database (default: `opencode debug paths db`).
    #[arg(long, global = true, env = "OPENCODE_DB")]
    pub opencode_db: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Serve session messaging tools over MCP stdio.
    Mcp(McpArgs),
    /// List discovered local sessions.
    List(ListArgs),
    /// Upload one or more sessions to the server.
    Upload(UploadArgs),
    /// Start a managed agent (claude, codex or opencode) in this terminal,
    /// registered with the running local host. Arguments after `--` are
    /// passed to the agent.
    #[cfg(unix)]
    StartAgent(crate::commands::local_host::StartAgentArgs),
    /// Run the local host in the foreground, with its loopback management UI;
    /// subcommands inspect and configure it and never start one (Unix only).
    #[cfg(unix)]
    Host(crate::commands::local_host::HostArgs),
    /// Open the running host's management UI with a fresh one-time owner link.
    #[cfg(unix)]
    Open(crate::commands::local_host::OpenArgs),
    /// Locally approved credentials that may sign remote operations for this host.
    #[cfg(unix)]
    Credential(crate::commands::local_host::CredentialArgs),
    /// Pair your own machines: a paired machine has full control of this one.
    #[cfg(unix)]
    Machine(crate::commands::local_host::MachineArgs),
    /// Passkey enrollment and browser delegation, performed by the running host
    /// over its owner-only socket.
    #[cfg(unix)]
    Passkey(crate::commands::local_host::PasskeyArgs),
    /// Read local transcript history or import owned server histories.
    #[cfg(unix)]
    History(crate::commands::local_host::HistoryArgs),
    /// Authenticate with the server (email + password) and save the session.
    Login(LoginArgs),
    /// Forget the saved session for the server.
    Logout,
    /// Probe the server's /healthz endpoint.
    Ping,
    /// Manage GitHub PR status for a session (fetched via the local `gh` CLI).
    Pr(PrArgs),
    /// Replace this executable with the latest published release after
    /// verifying its checksum and version. Running hosts are not restarted.
    #[cfg(unix)]
    SelfUpdate(crate::commands::self_update::SelfUpdateArgs),
}

#[derive(Debug, Args)]
pub struct PrArgs {
    #[command(subcommand)]
    pub command: PrCommand,
}

#[derive(Debug, Subcommand)]
pub enum PrCommand {
    /// Re-read the pull requests this machine recorded for a session (from
    /// signed web actions) via `gh` and push the snapshots to the server.
    Sync(PrSyncArgs),
}

#[derive(Debug, Args)]
pub struct PrSyncArgs {
    /// Session id whose linked PRs to sync. Required for v1.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Email to authenticate with (prompted if omitted).
    #[arg(long)]
    pub email: Option<String>,

    /// Password (prompted, hidden, if omitted). Prefer the prompt — a password
    /// passed on the command line can leak into shell history.
    #[arg(long)]
    pub password: Option<String>,

    /// Create the account first (`POST /auth/register`) instead of logging in.
    #[arg(long)]
    pub register: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Agent transcript store to list.
    #[arg(long, default_value_t = AgentKind::Claude, env = "CONVERSATIONS_AGENT")]
    pub agent: AgentKind,

    /// Only show sessions whose decoded project path contains this substring.
    #[arg(long)]
    pub project: Option<String>,

    /// Limit the number of rows shown (newest first).
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Args)]
pub struct UploadArgs {
    /// Agent transcript store to upload.
    #[arg(long, default_value_t = AgentKind::Claude, env = "CONVERSATIONS_AGENT")]
    pub agent: AgentKind,

    /// Session id(s) to upload, given positionally. Equivalent to repeating
    /// `--session`; the two are merged.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,

    /// Upload every discovered session (optionally narrowed by `--project`).
    #[arg(long)]
    pub all: bool,

    /// Upload a specific session id (repeatable).
    #[arg(long = "session", value_name = "ID")]
    pub sessions: Vec<String>,

    /// Only consider sessions whose decoded project path contains this substring.
    #[arg(long)]
    pub project: Option<String>,

    /// Parse and report what would be uploaded without POSTing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Do not attach repo/branch metadata discovered from the transcript.
    #[arg(long)]
    pub no_repos: bool,
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("conversations").chain(args.iter().copied()))
            .unwrap_or_else(|error| panic!("{args:?}: {error}"))
    }

    #[cfg(unix)]
    #[test]
    fn start_agent_takes_an_agent_and_passes_arguments_after_the_separator() {
        use conversations_core::{AgentKind, AgentLauncher};
        let Command::StartAgent(start) = parse(&["start-agent", "claude"]).command else {
            panic!("not start-agent")
        };
        assert_eq!(start.agent, AgentKind::Claude);
        assert!(start.resume.is_none() && start.cwd.is_none() && start.args.is_empty());
        let Command::StartAgent(start) = parse(&[
            "start-agent",
            "opencode",
            "--resume",
            "ses_abc",
            "--cwd",
            "/w",
            "--launcher",
            "adk",
            "--adk-statusline",
            "--runtime-dir",
            "/r",
            "--",
            "--model",
            "x",
            "--resume",
        ])
        .command
        else {
            panic!("not start-agent")
        };
        assert_eq!(start.agent, AgentKind::OpenCode);
        assert_eq!(start.resume.as_deref(), Some("ses_abc"));
        assert_eq!(start.cwd.as_deref(), Some("/w".as_ref()));
        assert_eq!(start.launcher, AgentLauncher::Adk);
        assert!(start.adk_statusline);
        assert_eq!(start.runtime.runtime_dir.as_deref(), Some("/r".as_ref()));
        assert_eq!(start.args, ["--model", "x", "--resume"]);
        let Command::StartAgent(start) = parse(&["start-agent", "codex"]).command else {
            panic!("not start-agent")
        };
        assert_eq!(start.agent, AgentKind::Codex);
        for args in [
            &["start-agent"][..],
            &["start-agent", "zsh"],
            &["start-agent", "--agent", "claude"],
            &["start-agent", "claude", "--host"],
            &["start-agent", "claude", "--mirror-size", "120x40"],
            &["live"],
            &["host", "start"],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("conversations").chain(args.iter().copied()))
                    .is_err(),
                "{args:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn bare_host_runs_and_management_groups_select_the_runtime_at_top_level() {
        let Command::Host(host) =
            parse(&["host", "--backend", "normal", "--cookie-file", "c"]).command
        else {
            panic!("not host")
        };
        assert!(host.command.is_none() && host.startup_conflict().is_none());
        let Command::Host(host) = parse(&["host", "--runtime-dir", "/r", "status"]).command else {
            panic!("not host")
        };
        assert!(host.command.is_some() && host.startup_conflict().is_none());
        assert_eq!(host.runtime.runtime_dir.as_deref(), Some("/r".as_ref()));
        let Command::Host(host) = parse(&["host", "--backend", "normal", "status"]).command else {
            panic!("not host")
        };
        assert_eq!(host.startup_conflict(), Some("--backend"));
        for args in [
            &["credential", "--runtime-dir", "/r", "identity"][..],
            &["credential", "identity", "--runtime-dir", "/r"],
            &["passkey", "--runtime-dir", "/r", "pending"],
            &["history", "search", "words", "--runtime-dir", "/r"],
            &["open", "--runtime-dir", "/r", "--reset-browser-access"],
        ] {
            let runtime = match parse(args).command {
                Command::Credential(group) => group.runtime,
                Command::Passkey(group) => group.runtime,
                Command::History(group) => group.runtime,
                Command::Open(open) => {
                    assert!(open.reset_browser_access);
                    open.runtime
                }
                other => panic!("{args:?}: {other:?}"),
            };
            assert_eq!(
                runtime.runtime_dir.as_deref(),
                Some("/r".as_ref()),
                "{args:?}"
            );
        }
    }

    #[test]
    fn transcript_store_overrides_stay_global_for_list_and_upload() {
        for args in [
            &[
                "--projects-dir",
                "/p",
                "--codex-sessions-dir",
                "/c",
                "--opencode-db",
                "/o",
                "list",
            ][..],
            &[
                "list",
                "--projects-dir",
                "/p",
                "--codex-sessions-dir",
                "/c",
                "--opencode-db",
                "/o",
            ],
            &[
                "--projects-dir",
                "/p",
                "--codex-sessions-dir",
                "/c",
                "--opencode-db",
                "/o",
                "upload",
                "--all",
            ],
            &[
                "upload",
                "--all",
                "--projects-dir",
                "/p",
                "--codex-sessions-dir",
                "/c",
                "--opencode-db",
                "/o",
            ],
        ] {
            let cli = parse(args);
            assert!(matches!(cli.command, Command::List(_) | Command::Upload(_)));
            assert_eq!(
                cli.global.projects_dir.as_deref(),
                Some("/p".as_ref()),
                "{args:?}"
            );
            assert_eq!(
                cli.global.codex_sessions_dir.as_deref(),
                Some("/c".as_ref()),
                "{args:?}"
            );
            assert_eq!(
                cli.global.opencode_db.as_deref(),
                Some("/o".as_ref()),
                "{args:?}"
            );
        }
    }
}

#[derive(Debug, Args)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommand,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum McpDelivery {
    Codex,
    Claude,
    Conversations,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum McpSessionContext {
    #[value(name = "opencode")]
    OpenCode,
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Install personal automatic delivery support for OpenCode 2.
    Install {
        #[arg(long)]
        agent: conversations_core::AgentKind,
    },
    #[command(hide = true)]
    Bridge {
        #[arg(long, env = "CONVERSATIONS_SESSION_ID")]
        session: Option<String>,
        #[arg(long)]
        native_session: String,
        #[arg(long)]
        cookie_file: Option<std::path::PathBuf>,
    },
    /// Serve messaging and handoff tools. Stdout is reserved for MCP JSON-RPC.
    Serve {
        /// Resolve each tool call from OpenCode native session metadata. For shared servers.
        #[arg(long, value_enum, conflicts_with_all = ["session", "delivery"])]
        session_context: Option<McpSessionContext>,
        #[arg(long, value_enum)]
        delivery: Option<McpDelivery>,
        /// Registered Conversations session you own. Its project defines the discovery scope.
        #[arg(long)]
        session: Option<String>,
        /// Read a session cookie from an owner-only file instead of the saved login.
        #[arg(long)]
        cookie_file: Option<std::path::PathBuf>,
    },
}
