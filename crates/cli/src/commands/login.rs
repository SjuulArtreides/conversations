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

    Ok(())
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
