//! clap command-line interface definition for `arrakis-conversations`.

use clap::{Args, Parser, Subcommand};
use conversations_core::{AgentKind, AgentLauncher};

/// Version string shown by `--version`: the crate version plus the short git
/// SHA captured at build time (see `build.rs`), e.g. `0.2.0 (78d747e)`.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_SHA"), ")");

/// Built-in fallback server, used when neither a flag/env nor a saved default
/// (see `config::default_server`) is present.
pub const DEFAULT_SERVER: &str = "http://localhost:8080";

/// Upload local Claude Code session histories to a conversations server.
#[derive(Debug, Parser)]
#[command(name = "arrakis-conversations", version = VERSION, about)]
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
    /// Start a live session: spawns an interactive `claude` under a PTY and
    /// mirrors it to the web. Use `--resume <id>` to continue an existing
    /// session, or `--host` to resume checkpointed local sessions.
    Live(LiveArgs),
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
pub struct LiveArgs {
    /// Agent to launch.
    #[arg(long, default_value_t = AgentKind::Claude, env = "CONVERSATIONS_AGENT")]
    pub agent: AgentKind,

    /// How to invoke the agent: `opencode` runs the agent's own binary, `adk`
    /// runs OpenCode through the Arrakis dev harness (`adk code`), which needs
    /// `ARRAKIS_HOME` and opens the monorepo root. Only valid with
    /// `--agent opencode`.
    #[arg(long, default_value_t = AgentLauncher::Opencode, env = "CONVERSATIONS_LAUNCHER")]
    pub launcher: AgentLauncher,

    /// Let an `adk code` session draw its tmux status strip. Off by default: the
    /// strip is what the web mirror would show instead of the OpenCode TUI.
    #[arg(long)]
    pub adk_statusline: bool,

    /// Stay resident without starting a new local session of your own: offer to
    /// resume the checkpointed sessions, and start more locally with Ctrl-a c.
    /// Web start, re-open, input and end need the signed local host
    /// (`arrakis-conversations host`).
    #[arg(long)]
    pub host: bool,

    /// Skip the `--host` "resume previous session(s)?" startup prompt and clear
    /// any saved open-session set instead. By default `--host` checkpoints its
    /// open sessions and offers to resume them on the next start; pass this to
    /// start clean (and discard the saved set). No effect without `--host`.
    #[arg(long, requires = "host")]
    pub no_resume: bool,

    /// Render web mirrors of sessions you are NOT locally focused on at this
    /// fixed terminal grid ("COLSxROWS", e.g. `120x40`) instead of your host
    /// terminal's size. The session you're focused on always tracks your real
    /// terminal (so your local view never garbles); background/hidden sessions —
    /// and any you switch away from — render at this larger grid for web viewers.
    /// Unset: every session uses the host terminal size (current behavior).
    #[arg(long, value_name = "COLSxROWS", value_parser = parse_mirror_size)]
    pub mirror_size: Option<(u16, u16)>,

    /// Resume an existing session as a live interactive session: spawn
    /// `claude --resume <ID>` (continuing that session's id and JSONL) instead
    /// of minting a fresh one. Run it from the session's project directory so
    /// claude finds the conversation.
    #[arg(long, value_name = "ID", conflicts_with = "host")]
    pub resume: Option<String>,

    /// Extra arguments passed through to the agent CLI.
    /// Everything after a `--` separator is forwarded verbatim.
    #[arg(last = true, value_name = "AGENT_ARG")]
    pub claude_args: Vec<String>,
}

/// Parse a `--mirror-size` value of the form `COLSxROWS` (e.g. `120x40`) into
/// `(cols, rows)`. Both must be positive; capped at 1000×1000 so a typo can't
/// open an absurd PTY. The separator is a literal lowercase `x`.
fn parse_mirror_size(s: &str) -> Result<(u16, u16), String> {
    const MAX_DIM: u16 = 1000;
    let (cols, rows) = s
        .split_once('x')
        .ok_or_else(|| format!("expected COLSxROWS (e.g. 120x40), got `{s}`"))?;
    let cols: u16 = cols
        .trim()
        .parse()
        .map_err(|_| format!("invalid column count in `{s}`"))?;
    let rows: u16 = rows
        .trim()
        .parse()
        .map_err(|_| format!("invalid row count in `{s}`"))?;
    if cols == 0 || rows == 0 {
        return Err(format!("columns and rows must both be > 0 (got `{s}`)"));
    }
    if cols > MAX_DIM || rows > MAX_DIM {
        return Err(format!(
            "columns and rows must each be <= {MAX_DIM} (got `{s}`)"
        ));
    }
    Ok((cols, rows))
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
    use super::{parse_mirror_size, Cli, Command};
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("arrakis-conversations").chain(args.iter().copied()))
            .unwrap_or_else(|error| panic!("{args:?}: {error}"))
    }

    #[test]
    fn live_launch_resume_and_host_forms_still_parse() {
        let Command::Live(live) = parse(&["live"]).command else {
            panic!("not live")
        };
        assert!(!live.host && live.resume.is_none() && live.claude_args.is_empty());
        let Command::Live(live) = parse(&["live", "--resume", "abc", "--", "--model", "x"]).command
        else {
            panic!("not live")
        };
        assert_eq!(live.resume.as_deref(), Some("abc"));
        assert_eq!(live.claude_args, ["--model", "x"]);
        let Command::Live(live) = parse(&["live", "--host", "--no-resume"]).command else {
            panic!("not live")
        };
        assert!(live.host && live.no_resume);
        assert!(Cli::try_parse_from([
            "arrakis-conversations",
            "live",
            "--host",
            "--resume",
            "abc"
        ])
        .is_err());
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

    #[test]
    fn parses_valid_cols_x_rows() {
        assert_eq!(parse_mirror_size("120x40"), Ok((120, 40)));
        assert_eq!(parse_mirror_size(" 80 x 24 "), Ok((80, 24)));
    }

    #[test]
    fn rejects_missing_separator_or_zero_or_oversized() {
        assert!(parse_mirror_size("120").is_err());
        assert!(parse_mirror_size("120,40").is_err());
        assert!(parse_mirror_size("0x40").is_err());
        assert!(parse_mirror_size("120x0").is_err());
        assert!(parse_mirror_size("1001x40").is_err());
        assert!(parse_mirror_size("abcxdef").is_err());
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
