//! One control-channel owner per local user and server, independent of backend
//! or runtime directory. The kernel lock is authoritative; PID metadata is only
//! diagnostic, so stale files and PID reuse cannot block a replacement host.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use base64::Engine;

pub(crate) struct HostOwner {
    _lock: File,
}

impl Drop for HostOwner {
    fn drop(&mut self) {
        // A concurrently spawned child can briefly inherit this descriptor before
        // exec closes it. Explicitly unlock so that child cannot prolong ownership.
        // SAFETY: the descriptor remains valid until this guard finishes dropping.
        unsafe { libc::flock(self._lock.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn root() -> Result<std::path::PathBuf> {
    Ok(dirs::data_local_dir()
        .context("no local data directory")?
        .join("arrakis-conversations/hosts/v1"))
}

fn owner_dir(root: &Path, server: &str) -> Result<std::path::PathBuf> {
    // Keep components short for long server URLs, without truncating keys.
    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(server.trim_end_matches('/'));
    let mut dir = root.to_path_buf();
    for component in key.as_bytes().chunks(48) {
        dir.push(std::str::from_utf8(component)?);
    }
    Ok(dir)
}

impl HostOwner {
    pub(crate) fn acquire(server: &str, runtime: Option<&Path>) -> Result<Self> {
        Self::acquire_in(&root()?, server, runtime)
    }

    /// The runtime directory of the host that currently owns `server`, if one
    /// is running. The kernel lock decides whether it runs; the directory is
    /// the one that host recorded when it took the lock.
    pub(crate) fn running_runtime(server: &str) -> Option<std::path::PathBuf> {
        Self::running_runtime_in(&root().ok()?, server)
    }

    fn running_runtime_in(root: &Path, server: &str) -> Option<std::path::PathBuf> {
        let path = owner_dir(root, server).ok()?.join("host.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok()?;
        // SAFETY: the descriptor stays open for both calls.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return None;
        }
        let mut owner = String::new();
        (&mut file).take(4096).read_to_string(&mut owner).ok()?;
        let owner: serde_json::Value = serde_json::from_str(owner.trim()).ok()?;
        owner["runtime_dir"].as_str().map(std::path::PathBuf::from)
    }

    fn acquire_in(root: &Path, server: &str, runtime: Option<&Path>) -> Result<Self> {
        let dir = owner_dir(root, server)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let meta = std::fs::symlink_metadata(&dir)?;
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        ensure!(
            meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0,
            "host ownership directory must be owner-only: {}",
            dir.display()
        );
        let path = dir.join("host.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.uid() == uid && meta.mode() & 0o077 == 0,
            "host ownership file must be owner-only: {}",
            path.display()
        );
        // SAFETY: file remains open for the lifetime of this guard. Never unlink
        // the lock: doing so would allow two owners to lock different inodes.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error).context("locking host ownership file");
            }
            let mut owner = String::new();
            let _ = (&mut file).take(4096).read_to_string(&mut owner);
            bail!("host already running for {server}: {} (ownership file: {}). Stop that host before starting another.", owner.trim(), path.display());
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        serde_json::to_writer(
            &mut file,
            &serde_json::json!({
                "pid": std::process::id(), "server": server,
                "runtime_dir": runtime
            }),
        )?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(Self { _lock: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_excludes_other_runtimes_and_allows_other_servers() {
        let root = tempfile::tempdir().unwrap();
        let owner = HostOwner::acquire_in(root.path(), "https://one.test/", Some(Path::new("one")))
            .unwrap();
        let error = HostOwner::acquire_in(root.path(), "https://one.test", Some(Path::new("two")))
            .err()
            .unwrap();
        assert!(error.to_string().contains(&std::process::id().to_string()));
        let _other = HostOwner::acquire_in(root.path(), "https://two.test", None).unwrap();
        drop(owner);
        let _replacement = HostOwner::acquire_in(root.path(), "https://one.test", None).unwrap();
    }

    #[test]
    fn running_runtime_names_only_a_live_owner() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            HostOwner::running_runtime_in(root.path(), "https://one.test"),
            None
        );
        let owner =
            HostOwner::acquire_in(root.path(), "https://one.test/", Some(Path::new("/rt/one")))
                .unwrap();
        assert_eq!(
            HostOwner::running_runtime_in(root.path(), "https://one.test"),
            Some(std::path::PathBuf::from("/rt/one"))
        );
        assert_eq!(
            HostOwner::running_runtime_in(root.path(), "https://two.test"),
            None
        );
        drop(owner);
        assert_eq!(
            HostOwner::running_runtime_in(root.path(), "https://one.test"),
            None
        );
    }
}
