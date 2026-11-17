//! Personal OpenCode TUI adapter installation. Shared server config is untouched.

use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const PLUGIN: &str = include_str!("../../assets/opencode-delivery/tui.js");

pub fn install() -> Result<()> {
    let root = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => dirs::home_dir()
            .context("Could not locate your home directory")?
            .join(".config"),
    };
    let path = install_at(&root)?;
    println!("Installed {}", path.display());
    println!(
        "Requires OpenCode 2.0.27 or newer. Reopen your wrapped OpenCode or adk code terminal to load it."
    );
    Ok(())
}

fn install_at(root: &Path) -> Result<PathBuf> {
    let directory = root.join("opencode/plugins/arrakis-conversations");
    fs::create_dir_all(&directory).context("Create personal OpenCode plugin directory")?;
    let path = directory.join("tui.js");
    let temporary = directory.join(format!(".tui-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .context("Create OpenCode plugin file")?;
        file.write_all(PLUGIN.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &path).context("Install OpenCode plugin atomically")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_install_preserves_server_config_and_replaces_plugin_atomically() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("opencode")).unwrap();
        let config = root.path().join("opencode/opencode.json");
        fs::write(&config, "personal config").unwrap();
        let installed = install_at(root.path()).unwrap();
        fs::write(&installed, "older plugin").unwrap();
        assert_eq!(install_at(root.path()).unwrap(), installed);
        assert_eq!(fs::read_to_string(installed).unwrap(), PLUGIN);
        assert_eq!(fs::read_to_string(config).unwrap(), "personal config");
        assert_eq!(
            fs::read_dir(root.path().join("opencode/plugins/arrakis-conversations"))
                .unwrap()
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn plugin_is_owner_readable_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let installed = install_at(root.path()).unwrap();
        assert_eq!(
            fs::metadata(installed).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
