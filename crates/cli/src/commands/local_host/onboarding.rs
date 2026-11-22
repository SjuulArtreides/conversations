//! First-start browser onboarding for an interactive foreground host.
//!
//! When a host starts on a terminal and no browser credential is approved yet,
//! the owner may choose how to approve one: a short-lived passkey setup link,
//! manual approval of a browser public key, or skip. Both approvals reuse the
//! existing local ceremonies (`web::setup_link`/`web::decide_device` and
//! `authz::enroll_from_cli`), so nothing the server holds can grant access:
//! the owner compares a code or fingerprint on this machine and confirms it.
//!
//! The prompt runs on a detached thread so the daemon keeps servicing while it
//! waits. It never runs for wrapped, cmux, headless or non-terminal hosts.
//! Ctrl-C at a prompt stops the host exactly as it would without the prompt.
//!
//! Web UI contract (no web change needed): the setup link opens the existing
//! `/host-link` page, which shows the last 12 characters of the new credential
//! id as its "device verification code"; manual approval takes the key the Host
//! access page copies ("Copy browser public key") and displays the fingerprint
//! derived from it, for the owner to compare with the one the page shows.
use std::io::IsTerminal;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};

use super::authz::{
    self,
    envelope::{key_id, Alg},
    passkey::ENROLL_TTL_SECS,
};
use super::web;

/// The standard owner permissions the Host access page asks for
/// (`approvalCommand` in `HostSetup.tsx`); keep the two lists equal.
const BROWSER_GRANTS: &[&str] = &[
    "history.read",
    "session.start",
    "session.reopen",
    "session.write",
    "session.pr_comment",
    "session.manage_sharing",
    "host.settings_read",
    "host.settings_write",
    "passkey.manage",
];
/// Opt-out for scripted terminals that must never see a prompt.
const OPT_OUT: &str = "CONVERSATIONS_NO_ONBOARDING";
const POLL: Duration = Duration::from_secs(2);

/// Whether this host start may prompt. Only a foreground normal-backend host
/// on a real terminal, never a wrapped, cmux-controlled or scripted one.
fn interactive(
    stdin_tty: bool,
    stderr_tty: bool,
    cmux: bool,
    env: impl Fn(&str) -> Option<String>,
) -> bool {
    stdin_tty
        && stderr_tty
        && !cmux
        && [
            "CONVERSATIONS_WRAPPED",
            "CONVERSATIONS_HOST_CONTROL",
            OPT_OUT,
            "CI",
        ]
        .iter()
        .all(|name| env(name).is_none_or(|value| value.is_empty()))
}

/// Any non-revoked owner credential a browser can use: a passkey or an
/// approved browser (ES256) key. Ed25519 owner keys are CLI/agent keys.
fn has_browser_credential(dir: &Path) -> Result<bool> {
    authz::ensure_dir(dir)?;
    // Opening the store creates the schema; the read uses its own connection.
    authz::store::Store::open(dir)?;
    let conn = authz::store::open_private(dir, "authz.db")?;
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM credentials WHERE kind='owner' AND alg='es256' AND revoked_at IS NULL)",
        [],
        |row| row.get(0),
    )?)
}

/// Restores the terminal if the host stops while a prompt holds it in raw
/// mode with a hidden cursor (the prompt thread is not joined).
pub(super) struct TerminalRestore(libc::termios);

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        // A background process changing terminal attributes would be stopped.
        if foreground() {
            // SAFETY: restores attributes captured from the same descriptor.
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
            eprint!("\x1b[?25h");
        }
    }
}

/// Whether this process owns the terminal. A backgrounded host (`host &`)
/// reading stdin would be stopped by SIGTTIN, halting the daemon too.
fn foreground() -> bool {
    // SAFETY: plain queries about this process and its own descriptor.
    unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) == libc::getpgrp() }
}

/// Offer browser onboarding when this start is interactive and no browser is
/// approved. Returns at once; keep the guard until the host stops.
pub(super) fn offer(dir: &Path, server: &str, cmux: bool) -> Option<TerminalRestore> {
    let env = |name: &str| std::env::var(name).ok();
    if !interactive(
        std::io::stdin().is_terminal() && foreground(),
        std::io::stderr().is_terminal(),
        cmux,
        env,
    ) || has_browser_credential(dir).unwrap_or(true)
    {
        return None;
    }
    // SAFETY: termios is plain data; tcgetattr fully initializes it on success.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 {
        return None;
    }
    let (dir, server) = (dir.to_path_buf(), server.to_owned());
    std::thread::Builder::new()
        .name("browser-onboarding".into())
        .spawn(move || {
            if let Err(error) = run(&dir, &server) {
                if interrupted(&error) {
                    // Same outcome as Ctrl-C without the prompt: the daemon's
                    // signal handler stops the host cleanly.
                    // SAFETY: signalling our own process has no preconditions.
                    unsafe { libc::kill(libc::getpid(), libc::SIGINT) };
                } else {
                    eprintln!("Browser setup stopped: {error:#}. The host keeps running; approve a browser later from Host access.");
                }
            }
        })
        .ok()?;
    Some(TerminalRestore(saved))
}

fn interrupted(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::Interrupted)
            || cause.downcast_ref::<dialoguer::Error>().is_some_and(|e| {
                matches!(e, dialoguer::Error::IO(io) if io.kind() == std::io::ErrorKind::Interrupted)
            })
    })
}

fn run(dir: &Path, server: &str) -> Result<()> {
    let theme = ColorfulTheme::default();
    let link = link_available(dir, server);
    loop {
        let mut items = Vec::new();
        if link {
            items.push("Open a short-lived setup link in the browser (passkey)");
        }
        items.push("Approve a browser's public key here (compare its fingerprint)");
        items.push("Skip for now");
        eprintln!();
        let choice = Select::with_theme(&theme)
            .with_prompt(
                "No browser is approved for this host yet. How do you want to approve one?",
            )
            .items(&items)
            .default(items.len() - 1)
            .interact()?;
        let result = match items[choice] {
            "Skip for now" => {
                eprintln!("Skipped. The host keeps running; approve a browser later from Host access at {}/settings.", server.trim_end_matches('/'));
                return Ok(());
            }
            item if item.starts_with("Open") => setup_link_flow(dir, server, &theme),
            _ => browser_key_flow(dir, server, &theme),
        };
        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if interrupted(&error) => return Err(error),
            Err(error) => eprintln!("{error:#}"),
        }
    }
}

/// The passkey setup link works with an HTTPS server, configuring the host's
/// relying party for it on first use, or with an already configured origin.
fn link_available(dir: &Path, server: &str) -> bool {
    let Ok(settings) = super::settings::Settings::load(dir) else {
        return false;
    };
    let Ok(url) = reqwest::Url::parse(server) else {
        return false;
    };
    let origin = url.origin().ascii_serialization();
    settings.webauthn.origins.contains(&origin)
        || (url.scheme() == "https"
            && settings.webauthn.rp_id.is_none()
            && settings.webauthn.origins.is_empty())
}

/// The code `/host-link` shows next to a freshly registered device.
fn device_code(cred_id: &str) -> &str {
    &cred_id[cred_id.len().saturating_sub(12)..]
}

/// `Ok(true)` once a browser is approved; `Ok(false)` to return to the menu.
fn setup_link_flow(dir: &Path, server: &str, theme: &ColorfulTheme) -> Result<bool> {
    let settings = super::settings::Settings::load(dir)?;
    if settings.webauthn.rp_id.is_none() && settings.webauthn.origins.is_empty() {
        super::settings::configure_device_passkeys(dir, server)?;
    }
    let before: Vec<String> = web::pending_devices(dir)?
        .into_iter()
        .map(|p| p.cred_id)
        .collect();
    let link = web::setup_link(dir, server)?;
    eprintln!(
        "\nOpen this link in the browser you want to use, signed in to {server}. It expires in {} minutes:\n  {link}\n\
         Waiting for the browser to register a passkey. The host keeps running; press Ctrl-C to stop it.",
        ENROLL_TTL_SECS / 60
    );
    let deadline = Instant::now() + Duration::from_secs(ENROLL_TTL_SECS as u64 + 30);
    let id = loop {
        if let Some(found) = web::pending_devices(dir)?
            .into_iter()
            .find(|p| !before.contains(&p.cred_id))
        {
            break found.cred_id;
        }
        if Instant::now() >= deadline {
            eprintln!("The setup link expired before a browser registered.");
            return Ok(false);
        }
        std::thread::sleep(POLL);
    };
    eprintln!(
        "\nA browser registered a passkey. Device verification code: {}\n  Full device identifier: {id}",
        device_code(&id)
    );
    let approve = Confirm::with_theme(theme)
        .with_prompt("Does the browser show exactly this verification code? Approve it as an owner of this host")
        .default(false)
        .interact()?;
    web::decide_device(dir, &id, approve)?;
    if approve {
        eprintln!("Approved. Continue in the browser.");
    } else {
        eprintln!("Rejected that registration. Nothing was approved.");
    }
    Ok(approve)
}

fn browser_key_flow(dir: &Path, server: &str, theme: &ColorfulTheme) -> Result<bool> {
    eprintln!(
        "\nIn the browser, open Host access at {}/settings, paste this public host descriptor and click Connect machine:\n  {}\n\
         Then click \"Copy browser public key\". The page also shows this browser's fingerprint.",
        server.trim_end_matches('/'),
        authz::attest::descriptor(dir)?
    );
    let pasted: String = Input::with_theme(theme)
        .with_prompt("Paste the browser public key (empty to go back)")
        .allow_empty(true)
        .interact_text()?;
    if pasted.trim().is_empty() {
        return Ok(false);
    }
    let (public_key, fingerprint) = parse_browser_key(&pasted)?;
    eprintln!("\nBrowser key fingerprint: {fingerprint}");
    let approve = Confirm::with_theme(theme)
        .with_prompt(
            "Does the browser show exactly this fingerprint? Approve it as an owner of this host",
        )
        .default(false)
        .interact()?;
    if !approve {
        eprintln!("Not approved.");
        return Ok(false);
    }
    let grants: Vec<String> = BROWSER_GRANTS.iter().map(|g| (*g).to_owned()).collect();
    let (kid, _) =
        authz::enroll_from_cli(dir, "owner", "es256", &public_key, BROWSER_LABEL, &grants)?;
    eprintln!(
        "Approved browser {kid}. Return to Host access and choose Check browser approval if shown."
    );
    Ok(true)
}

const BROWSER_LABEL: &str = "Browser (approved in terminal)";

/// A pasted browser public key: standard base64 of a raw ES256 (uncompressed
/// P-256) point. Returns the normalized base64 and the fingerprint derived
/// here, which the owner compares with the one the browser shows.
fn parse_browser_key(pasted: &str) -> Result<(String, String)> {
    let compact: String = pasted.split_whitespace().collect();
    let public = STANDARD
        .decode(&compact)
        .context("that is not a base64 browser public key")?;
    ensure!(
        Alg::Es256.valid_public_key(&public),
        "that is not a valid ES256 browser public key"
    );
    Ok((STANDARD.encode(&public), key_id(&public)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn prompts_only_on_a_plain_interactive_terminal() {
        assert!(interactive(true, true, false, env(&[])));
        assert!(interactive(true, true, false, env(&[("CI", "")])));
        assert!(!interactive(false, true, false, env(&[])), "piped stdin");
        assert!(
            !interactive(true, false, false, env(&[])),
            "captured stderr"
        );
        assert!(!interactive(true, true, true, env(&[])), "cmux backend");
        for name in [
            "CONVERSATIONS_WRAPPED",
            "CONVERSATIONS_HOST_CONTROL",
            OPT_OUT,
            "CI",
        ] {
            let vars: &'static [(&'static str, &'static str)] =
                Box::leak(vec![(name, "1")].into_boxed_slice());
            assert!(!interactive(true, true, false, env(vars)), "{name}");
        }
    }

    #[test]
    fn only_a_live_owner_browser_credential_counts_as_approved() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("runtime");
        assert!(!has_browser_credential(&dir).unwrap());
        let cli_key = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).unwrap();
        let cli_key = STANDARD.encode(ring::signature::KeyPair::public_key(&cli_key).as_ref());
        authz::enroll_from_cli(
            &dir,
            "owner",
            "ed25519",
            &cli_key,
            "CLI",
            &["session.start".into()],
        )
        .unwrap();
        assert!(
            !has_browser_credential(&dir).unwrap(),
            "Ed25519 keys are not browsers"
        );
        let (public, _) = browser_key();
        let grants: Vec<String> = BROWSER_GRANTS.iter().map(|g| (*g).to_owned()).collect();
        let (kid, _) = authz::enroll_from_cli(
            &dir,
            "owner",
            "es256",
            &STANDARD.encode(&public),
            "Laptop",
            &grants,
        )
        .unwrap();
        assert!(has_browser_credential(&dir).unwrap());
        authz::store::Store::open(&dir)
            .unwrap()
            .connection()
            .execute(
                "UPDATE credentials SET revoked_at=1 WHERE cred_id=?1",
                [&kid],
            )
            .unwrap();
        assert!(!has_browser_credential(&dir).unwrap(), "revoked");
    }

    fn browser_key() -> (Vec<u8>, String) {
        use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let public = pair.public_key().as_ref().to_vec();
        let kid = key_id(&public);
        (public, kid)
    }

    #[test]
    fn pasted_browser_key_is_validated_and_fingerprinted() {
        let (public, kid) = browser_key();
        let encoded = STANDARD.encode(&public);
        // Terminal wrapping and surrounding whitespace are tolerated.
        let wrapped = format!("  {}\n{}  ", &encoded[..20], &encoded[20..]);
        assert_eq!(parse_browser_key(&wrapped).unwrap(), (encoded, kid));
        assert!(parse_browser_key("not base64!").is_err());
        // Valid base64 but not a P-256 point (an Ed25519-sized key).
        assert!(parse_browser_key(&STANDARD.encode([7u8; 32])).is_err());
        let mut wrong_format = public.clone();
        wrong_format[0] = 0x02;
        assert!(parse_browser_key(&STANDARD.encode(&wrong_format)).is_err());
    }

    #[test]
    fn device_code_is_the_suffix_host_link_displays() {
        let id = "A".repeat(31) + "BCDEFGHIJKLM";
        assert_eq!(device_code(&id), "BCDEFGHIJKLM");
        assert_eq!(device_code("short"), "short");
    }

    #[test]
    fn interrupted_prompts_are_recognised() {
        let error = anyhow::Error::from(dialoguer::Error::IO(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "read interrupted",
        )));
        assert!(interrupted(&error));
        assert!(!interrupted(&anyhow::anyhow!("server refused")));
    }

    #[test]
    fn standard_browser_grants_are_enrollable() {
        assert!(BROWSER_GRANTS
            .iter()
            .all(|g| web::is_enrollable_owner_cap(g)));
    }
}
