//! `conversations login` / `logout` — authenticate against a server
//! with email + password and persist the session cookie so `upload`/`ping`
//! work when the server has auth enabled.

use std::io::IsTerminal;

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password};

use crate::cli::{GlobalOpts, LoginArgs};
use crate::client::ApiClient;
use crate::{config, credentials};

pub fn run(global: &GlobalOpts, args: &LoginArgs) -> Result<()> {
    // Logging in must target an explicit server — either a `--server` /
    // `CONVERSATIONS_SERVER` value or an already-saved default. Don't silently
    // authenticate against the built-in `http://localhost:8080` fallback.
    if global.server_arg.is_none() && config::default_server().is_none() {
        bail!(
            "no server specified — pass --server <URL> or set CONVERSATIONS_SERVER before logging in"
        );
    }

    let client = ApiClient::new(&global.server)?;
    let theme = ColorfulTheme::default();

    let email = match &args.email {
        Some(e) => e.clone(),
        None => Input::<String>::with_theme(&theme)
            .with_prompt("Email")
            .interact_text()
            .context("failed to read email")?,
    };

    let password = match &args.password {
        Some(p) => p.clone(),
        None => {
            let mut prompt = Password::with_theme(&theme).with_prompt("Password");
            // Confirm on registration so a typo'd new password can't lock you out.
            if args.register {
                prompt = prompt.with_confirmation("Confirm password", "passwords do not match");
            }
            prompt.interact().context("failed to read password")?
        }
    };

    let result = client.authenticate(args.register, &email, &password)?;
    credentials::store_cookie(&global.server, &result.cookie)?;

    let who = result.display_name.unwrap_or(result.email);
    let verb = if args.register {
        "Registered and logged in"
    } else {
        "Logged in"
    };
    println!("{verb} as {who} on {}.", global.server);

    // Offer to remember this server as the default so future commands don't
    // need `--server`. Skip when it's already the default, or when there's no
    // TTY to prompt on (e.g. a fully scripted `--email`/`--password` login).
    let already_default = config::default_server().as_deref() == Some(global.server.as_str());
    if !already_default && std::io::stdin().is_terminal() {
        let make_default = Confirm::with_theme(&theme)
            .with_prompt(format!("Set {} as your default server?", global.server))
            .default(true)
            .interact()
            .context("failed to read confirmation")?;
        if make_default {
            config::set_default_server(&global.server)?;
            println!("Default server set to {}.", global.server);
        }
    }

    #[cfg(unix)]
    host_handoff(global, &theme)?;

    Ok(())
}

/// After a successful login, name the exact command that starts host mode on
/// this server and, on a terminal, offer to start it now (opt-in).
#[cfg(unix)]
fn host_handoff(global: &GlobalOpts, theme: &ColorfulTheme) -> Result<()> {
    let exe = std::env::current_exe().ok();
    println!(
        "To start host mode on {}, run:\n  {}",
        global.server,
        host_command(exe.as_deref(), &global.server)
    );
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Ok(());
    }
    let start = Confirm::with_theme(theme)
        .with_prompt("Start host mode now?")
        .default(false)
        .interact()
        .context("failed to read confirmation")?;
    if !start {
        return Ok(());
    }
    // Parse exactly the printed command, so `host` takes the same environment
    // defaults (e.g. CONVERSATIONS_HOST_DIR) it would when pasted.
    let cli = <crate::cli::Cli as clap::Parser>::try_parse_from([
        "conversations",
        "--server",
        global.server.as_str(),
        "host",
    ])?;
    let crate::cli::Command::Host(host) = cli.command else {
        unreachable!("`host` parses as the host command");
    };
    crate::commands::local_host::run(global, &host)
}

/// A POSIX-shell-safe command that runs this binary (PATH may lack it) as a
/// foreground host on `server`.
#[cfg(unix)]
fn host_command(exe: Option<&std::path::Path>, server: &str) -> String {
    let quote = |word: &str| format!("'{}'", word.replace('\'', "'\\''"));
    let program = exe.map_or("conversations".into(), |exe| quote(&exe.to_string_lossy()));
    format!("{program} --server {} host", quote(server))
}

/// `logout` — drop the saved cookie for this server.
pub fn logout(global: &GlobalOpts) -> Result<()> {
    if credentials::clear_cookie(&global.server)? {
        println!("Logged out of {}.", global.server);
    } else {
        println!("No saved session for {}.", global.server);
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Pasted into a POSIX shell, the command runs this exact executable with
    /// this exact server, whatever characters either contains.
    #[test]
    fn host_command_is_shell_safe_and_explicit() {
        let exe = std::path::Path::new("/opt/my tools/it's/$HOME/conversations");
        let server = "https://conv.example/a b?x='y'&z=`id`";
        let line = host_command(Some(exe), server);
        let argv = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\0' {line}"))
            .output()
            .unwrap();
        assert!(argv.status.success());
        let argv: Vec<_> = argv
            .stdout
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect();
        assert_eq!(argv, [exe.to_str().unwrap(), "--server", server, "host"]);
    }
}
