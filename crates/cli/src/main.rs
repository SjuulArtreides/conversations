//! `conversations` — discover local Claude Code sessions and upload them
//! to a conversations server.
//!
//! Subcommands include `list`, `upload`, `start-agent`, `host` and `ping`. See
//! `cli.rs` for flags. The HTTP
//! client uses reqwest's blocking API; parsing and discovery come from
//! `conversations-core`. Network/parse failures are kept per-session non-fatal
//! during a batch upload and reported in a summary at the end.

macro_rules! live_error {
    ($($arg:tt)*) => { crate::diagnostics::report(format_args!($($arg)*), true) };
}
macro_rules! live_info {
    ($($arg:tt)*) => { crate::diagnostics::report(format_args!($($arg)*), false) };
}

mod cli;
mod client;
mod commands;
mod config;
mod credentials;
mod diagnostics;
mod discover;
mod open_sessions;
mod opencode_store;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};

fn initialize_tls() {
    // reqwest enables AWS-LC alongside ring; WSS needs an explicit process default.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn main() -> ExitCode {
    initialize_tls();
    let mut args = Cli::parse();
    #[cfg(unix)]
    if let Command::Host(host) = &args.command {
        if let Some(option) = host.startup_conflict() {
            use clap::CommandFactory;
            Cli::command()
                .error(
                    clap::error::ErrorKind::ArgumentConflict,
                    format!("{option} starts bare `host` and cannot be used with a subcommand"),
                )
                .exit();
        }
    }
    // Resolve the effective server once, up front: an explicit `--server` /
    // `CONVERSATIONS_SERVER` wins, then the saved default server, then the
    // built-in fallback. Every subcommand reads `global.server` from here.
    args.global.server = args
        .global
        .server_arg
        .as_deref()
        .map(|s| s.trim_end_matches('/').to_string())
        .or_else(config::default_server)
        .unwrap_or_else(|| cli::DEFAULT_SERVER.to_string());

    let result = match &args.command {
        Command::Mcp(mcp_args) => commands::mcp::run(&args.global, mcp_args),
        Command::List(list_args) => commands::list::run(&args.global, list_args),
        Command::Upload(upload_args) => commands::upload::run(&args.global, upload_args),
        #[cfg(unix)]
        Command::StartAgent(start_args) => {
            commands::local_host::start_agent(&args.global, start_args)
        }
        #[cfg(unix)]
        Command::Host(host_args) => commands::local_host::run(&args.global, host_args),
        #[cfg(unix)]
        Command::Open(open_args) => commands::local_host::open(open_args),
        #[cfg(unix)]
        Command::Credential(credential_args) => commands::local_host::credential(credential_args),
        #[cfg(unix)]
        Command::Machine(machine_args) => commands::local_host::machine(&args.global, machine_args),
        #[cfg(unix)]
        Command::Passkey(passkey_args) => commands::local_host::passkey(&args.global, passkey_args),
        #[cfg(unix)]
        Command::History(history_args) => commands::local_host::history(&args.global, history_args),
        Command::Login(login_args) => commands::login::run(&args.global, login_args),
        Command::Logout => commands::login::logout(&args.global),
        Command::Ping => commands::ping::run(&args.global),
        Command::Pr(pr_args) => commands::pr::run(&args.global, pr_args),
        #[cfg(unix)]
        Command::SelfUpdate(update_args) => commands::self_update::run(update_args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cli_tls_initialization_reaches_wss_handshake_without_panicking() {
        const CHILD: &str = "CONVERSATIONS_TLS_REGRESSION_CHILD";
        // Isolate the process-wide rustls provider from every other test.
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::cli_tls_initialization_reaches_wss_handshake_without_panicking",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "WSS subprocess failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        super::initialize_tls();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            use tokio::io::AsyncWriteExt;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let peer = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                // Reach the real TLS connector, but never accept an invalid TLS peer.
                let _ = stream.write_all(b"not TLS\r\n").await;
                let _ = tokio::io::copy(&mut stream, &mut tokio::io::sink()).await;
            });
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tokio_tungstenite::connect_async(format!("wss://{address}/")),
            )
            .await
            .expect("local WSS handshake must finish");
            peer.await.unwrap();
            assert!(
                matches!(result, Err(tokio_tungstenite::tungstenite::Error::Io(ref error))
                    if error.kind() == std::io::ErrorKind::InvalidData),
                "invalid TLS must be rejected without a provider panic: {result:?}"
            );
        });
    }
}
