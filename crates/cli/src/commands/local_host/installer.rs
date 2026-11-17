//! Optional, reversible Ghostty startup command. This is setup, not runtime code.
use std::fs;
use std::io::IsTerminal;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};

use super::settings::Settings;

const BEGIN: &str = "# BEGIN Conversations localhost startup\n";
const END: &str = "# END Conversations localhost startup\n";

pub fn run(runtime: &Path, selected: Option<&Path>, enable: bool, uninstall: bool) -> Result<()> {
    run_with_confirmation(runtime, selected, enable, uninstall, |link, target| {
        ensure!(std::io::stdin().is_terminal(),
            "Ghostty config is a symlink: {} -> {}; run interactively to approve editing its target",
            link.display(), target.display());
        dialoguer::Confirm::new()
            .with_prompt(format!(
                "Follow Ghostty config symlink {} and edit {}?",
                link.display(),
                target.display()
            ))
            .default(false)
            .interact()
            .context("reading symlink confirmation")
    })
}

fn run_with_confirmation(
    runtime: &Path,
    selected: Option<&Path>,
    enable: bool,
    uninstall: bool,
    confirm: impl FnOnce(&Path, &Path) -> Result<bool>,
) -> Result<()> {
    let selected_config = selected
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config);
    let config = if selected_config.is_symlink() {
        let target = fs::canonicalize(&selected_config).with_context(|| {
            format!(
                "resolving Ghostty config symlink {}",
                selected_config.display()
            )
        })?;
        ensure!(
            confirm(&selected_config, &target)?,
            "Ghostty config left unchanged: symlink target edit declined"
        );
        target
    } else {
        selected_config.clone()
    };
    let original = match fs::read_to_string(&config) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", config.display())),
    };
    let clean = remove_owned_block(&original)?;
    let updated = if uninstall {
        clean
    } else {
        for line in clean.lines() {
            let key = line.trim_start().split_once('=').map(|(key, _)| key.trim());
            ensure!(
                !matches!(key, Some("command" | "config-file")),
                "Ghostty already has a command or config-file include; {} was left unchanged",
                config.display()
            );
        }
        let binary = std::env::current_exe()?;
        let parent = selected_config
            .parent()
            .context("Ghostty config has no parent directory")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let launcher_dir = parent.join("conversations");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&launcher_dir)?;
        let launcher = launcher_dir.join("cmux-shell.sh");
        ensure!(
            !launcher.is_symlink(),
            "startup launcher is a symlink: {}",
            launcher.display()
        );
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let script = format!(
            "#!/bin/sh\nruntime={}\nruntime=\"${{CONVERSATIONS_HOST_DIR:-$runtime}}\"\nshell={}\nbinary={}\nif [ -n \"${{CMUX_WORKSPACE_ID:-}}\" ] && [ -x \"$binary\" ] && \"$binary\" host --help >/dev/null 2>&1; then\n  exec \"$binary\" host --runtime-dir \"$runtime\" shell -- \"$shell\" -l\nfi\nexec \"$shell\" -l\n",
            quote(&runtime.to_string_lossy()), quote(&shell), quote(&binary.to_string_lossy()));
        atomic_write(&launcher, script.as_bytes(), 0o700)?;
        format!(
            "{}{}{}command = shell:{}\n{}",
            clean,
            if clean.is_empty() || clean.ends_with('\n') {
                ""
            } else {
                "\n"
            },
            BEGIN,
            quote(&launcher.to_string_lossy()),
            END
        )
    };
    if original != updated {
        if config.exists() {
            let backup = config.with_file_name(format!(
                "{}.conversations-{}.bak",
                config.file_name().unwrap_or_default().to_string_lossy(),
                uuid::Uuid::new_v4()
            ));
            fs::copy(&config, &backup)?;
            println!("Backup: {}", backup.display());
        }
        let mode = config
            .metadata()
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        atomic_write(&config, updated.as_bytes(), mode)?;
    }
    if enable {
        let mut settings = Settings::load(runtime)?;
        settings.cmux.auto_wrap = true;
        settings.save()?;
    }
    println!(
        "{} cmux startup in {}",
        if uninstall { "Removed" } else { "Installed" },
        config.display()
    );
    if !uninstall {
        let enabled = Settings::load(runtime)?.cmux.auto_wrap;
        println!("Automatic wrapping: {}", if enabled { "on" } else { "off" });
    }
    println!(
        "Run `cmux reload-config` to apply to new terminals. Existing terminals continue running."
    );
    Ok(())
}

fn default_config() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let primary = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("ghostty/config");
    #[cfg(target_os = "macos")]
    if !primary.exists() {
        let app_support = home.join("Library/Application Support/com.mitchellh.ghostty/config");
        if app_support.exists() {
            return app_support;
        }
    }
    primary
}

fn remove_owned_block(original: &str) -> Result<String> {
    match (original.find(BEGIN), original.find(END)) {
        (None, None) => Ok(original.to_owned()),
        (Some(start), Some(end)) if end > start => {
            let end = end + END.len();
            ensure!(
                original[end..].find(BEGIN).is_none(),
                "duplicate Conversations startup blocks"
            );
            Ok(format!("{}{}", &original[..start], &original[end..]))
        }
        _ => bail!("incomplete Conversations startup block; configuration left unchanged"),
    }
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temporary = path.with_file_name(format!(".conversations-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok::<(), anyhow::Error>(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn symlink_install_and_uninstall_require_consent_and_preserve_link() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        let ghostty = dir.path().join("ghostty");
        fs::create_dir(&dotfiles).unwrap();
        fs::create_dir(&ghostty).unwrap();
        let target = dotfiles.join("config");
        let link = ghostty.join("config");
        fs::write(&target, "font-size = 14\n").unwrap();
        symlink("../dotfiles/config", &link).unwrap();
        let runtime = dir.path().join("runtime");
        let declined = run_with_confirmation(&runtime, Some(&link), true, false, |_, _| Ok(false));
        assert!(declined.is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "font-size = 14\n");
        assert!(!ghostty.join("conversations").exists());
        assert!(!runtime.exists());
        run_with_confirmation(&runtime, Some(&link), false, false, |from, to| {
            assert_eq!(from, link);
            assert_eq!(to, fs::canonicalize(&target).unwrap());
            Ok(true)
        })
        .unwrap();
        assert!(link.is_symlink());
        assert_eq!(
            fs::read_link(&link).unwrap(),
            PathBuf::from("../dotfiles/config")
        );
        let installed = fs::read_to_string(&target).unwrap();
        assert!(installed.starts_with("font-size = 14\n"));
        assert!(installed.contains(BEGIN));
        assert!(ghostty.join("conversations/cmux-shell.sh").is_file());
        assert!(!dotfiles.join("conversations").exists());
        assert!(fs::read_dir(&dotfiles).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".bak")));
        assert!(
            run_with_confirmation(&runtime, Some(&link), false, true, |_, _| Ok(false)).is_err()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), installed);
        run_with_confirmation(&runtime, Some(&link), false, true, |_, _| Ok(true)).unwrap();
        assert!(link.is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "font-size = 14\n");
    }
}
