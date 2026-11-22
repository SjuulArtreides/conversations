//! `conversations self-update` — replace the running executable with the
//! latest published release.
//!
//! The artifact contract is the one `scripts/install.sh` consumes and
//! `.github/workflows/release.yml` publishes: for a strict `vX.Y.Z` tag the
//! public release carries `conversations-X.Y.Z-<target>.tar.gz` plus a
//! one-line `.sha256` sidecar naming that archive.
//!
//! The update is fail-closed. The archive is held in memory (bounded) and its
//! digest checked against the sidecar before anything is decompressed; only
//! the regular `conversations` member is extracted, into a temporary file in
//! the executable's own directory; that staged binary must report the expected
//! version through `--version`; only then is it renamed over the running
//! executable in one atomic step. Every earlier failure removes the staged
//! file and leaves the existing binary untouched. Nothing is written outside
//! the executable's directory, no credentials are sent, and no running process
//! (such as a host) is signalled or restarted.

use std::fmt;
use std::fs;
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Public GitHub repository whose releases this command uses.
const REPO_URL: &str = "https://github.com/SjuulArtreides/conversations";
const BIN: &str = "conversations";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_REDIRECTS: usize = 5;
const MAX_SIDECAR_BYTES: u64 = 4 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;
const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, clap::Args)]
pub struct SelfUpdateArgs {
    /// Only report whether a newer release exists; change nothing.
    #[arg(long)]
    pub check: bool,
}

/// A strict `X.Y.Z` release version (no leading zeros, no suffixes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version(u64, u64, u64);

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

pub(crate) fn parse_version(text: &str) -> Option<Version> {
    let mut parts = text.split('.');
    let mut next = || {
        let part = parts.next()?;
        let canonical = !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'));
        canonical.then(|| part.parse::<u64>().ok()).flatten()
    };
    let version = Version(next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// Parse a strict `vX.Y.Z` release tag.
pub(crate) fn parse_tag(tag: &str) -> Option<Version> {
    parse_version(tag.strip_prefix('v')?)
}

/// Release target triple of the *running* executable (compile-time), so a
/// Rosetta-translated x86_64 binary updates to the x86_64 build.
pub(crate) fn release_target() -> Option<&'static str> {
    if cfg!(all(
        target_os = "linux",
        target_env = "gnu",
        target_arch = "x86_64"
    )) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(
        target_os = "linux",
        target_env = "gnu",
        target_arch = "aarch64"
    )) {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else {
        None
    }
}

/// Where releases come from. Production uses [`GitHubReleases`]; tests use an
/// in-memory fixture so no public network is touched.
pub(crate) trait ReleaseSource {
    /// The newest published release tag (unvalidated text).
    fn latest_tag(&self) -> Result<String>;
    /// Download one asset of `tag`, failing if it exceeds `limit` bytes.
    fn download(&self, tag: &str, asset: &str, limit: u64) -> Result<Vec<u8>>;
}

/// Anonymous HTTPS client for the fixed public GitHub release.
pub(crate) struct GitHubReleases {
    /// Follows a bounded number of HTTPS-only redirects (asset downloads
    /// redirect to GitHub's object storage).
    client: reqwest::blocking::Client,
    /// Never follows redirects: `/releases/latest` is resolved from its
    /// `Location` header.
    no_redirect: reqwest::blocking::Client,
}

impl GitHubReleases {
    pub(crate) fn new() -> Result<Self> {
        let builder = || {
            reqwest::blocking::Client::builder()
                .https_only(true)
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .user_agent(concat!(
                    "conversations-self-update/",
                    env!("CARGO_PKG_VERSION")
                ))
        };
        let client = builder()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    attempt.error("too many redirects")
                } else if attempt.url().scheme() != "https" {
                    attempt.error("refusing a non-HTTPS redirect")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .context("building the release download client")?;
        let no_redirect = builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building the release lookup client")?;
        Ok(Self {
            client,
            no_redirect,
        })
    }
}

impl ReleaseSource for GitHubReleases {
    fn latest_tag(&self) -> Result<String> {
        let url = format!("{REPO_URL}/releases/latest");
        let response = self
            .no_redirect
            .get(&url)
            .send()
            .with_context(|| format!("failed to reach {url}"))?;
        if !response.status().is_redirection() {
            bail!(
                "{url} returned HTTP {} instead of a release redirect",
                response.status()
            );
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .with_context(|| format!("{url} redirect has no usable Location"))?;
        tag_from_latest_location(location).map(str::to_owned)
    }

    fn download(&self, tag: &str, asset: &str, limit: u64) -> Result<Vec<u8>> {
        let url = format!("{REPO_URL}/releases/download/{tag}/{asset}");
        let response = self
            .client
            .get(&url)
            .send()
            .with_context(|| format!("failed to download {url}"))?;
        if !response.status().is_success() {
            bail!("{url} returned HTTP {}", response.status());
        }
        if response.content_length().is_some_and(|len| len > limit) {
            bail!("{asset} is larger than the {limit}-byte limit");
        }
        read_bounded(response, limit, asset)
    }
}

/// `/releases/latest` redirects to `<repo>/releases/tag/<tag>`; anything else
/// (for example the release list, when nothing is published) is an error.
pub(crate) fn tag_from_latest_location(location: &str) -> Result<&str> {
    let prefix = format!("{REPO_URL}/releases/tag/");
    location
        .strip_prefix(&prefix)
        .filter(|tag| parse_tag(tag).is_some())
        .with_context(|| {
            format!("latest release redirect {location:?} does not name a strict vX.Y.Z tag")
        })
}

fn read_bounded(reader: impl Read, limit: u64, what: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {what}"))?;
    if bytes.len() as u64 > limit {
        bail!("{what} is larger than the {limit}-byte limit");
    }
    Ok(bytes)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    UpToDate { current: Version, latest: Version },
    Available { current: Version, latest: Version },
    Updated { from: Version, to: Version },
}

/// Resolve the latest release and, unless `check_only`, replace `exe` with it.
/// `exe` must be the canonical path of the executable to replace.
pub(crate) fn update(
    source: &dyn ReleaseSource,
    current: Version,
    exe: &Path,
    target: &str,
    check_only: bool,
) -> Result<Outcome> {
    let tag = source.latest_tag()?;
    let latest = parse_tag(&tag)
        .with_context(|| format!("latest release tag {tag:?} is not a strict vX.Y.Z tag"))?;
    if latest <= current {
        return Ok(Outcome::UpToDate { current, latest });
    }
    if check_only {
        return Ok(Outcome::Available { current, latest });
    }

    let dir = exe
        .parent()
        .with_context(|| format!("{} has no parent directory", exe.display()))?;
    let original = fs::File::open(exe).with_context(|| format!("cannot open {}", exe.display()))?;
    original
        .try_lock()
        .context("cannot lock the executable for update; check for another update in progress")?;
    let original_metadata = original.metadata()?;
    let original_mode = original_metadata.permissions().mode();
    // Create the staging file first: an unwritable (managed or read-only)
    // install fails here, before anything is downloaded.
    let mut staged = tempfile::Builder::new()
        .prefix(".conversations-update-")
        .tempfile_in(dir)
        .map_err(|error| not_writable(dir, error))?;

    let archive_name = format!("{BIN}-{latest}-{target}.tar.gz");
    let sidecar_name = format!("{archive_name}.sha256");
    eprintln!("Downloading {archive_name} ({tag})…");
    let sidecar = source.download(&tag, &sidecar_name, MAX_SIDECAR_BYTES)?;
    let archive = source.download(&tag, &archive_name, MAX_ARCHIVE_BYTES)?;
    verify_sha256(&archive, &sidecar, &archive_name)?;
    let binary = extract_member(&archive, BIN)?;

    staged
        .write_all(&binary)
        .context("failed to write the staged binary")?;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode((original_mode & 0o777) | 0o700))
        .context("failed to mark the staged binary executable")?;
    staged
        .as_file()
        .sync_all()
        .context("failed to sync the staged binary")?;
    // Close the write handle: executing a file that is open for writing fails
    // with ETXTBSY on Linux. The path is still removed on drop.
    let staged = staged.into_temp_path();
    verify_staged_version(&staged, latest)?;
    let installed = fs::symlink_metadata(exe)?;
    if (installed.dev(), installed.ino()) != (original_metadata.dev(), original_metadata.ino()) {
        bail!(
            "the installed executable changed during the update; retry with the installed binary"
        );
    }
    staged
        .persist(exe)
        .map_err(|error| not_writable(dir, error.error))?;
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(Outcome::Updated {
        from: current,
        to: latest,
    })
}

fn not_writable(dir: &Path, error: std::io::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "cannot write to {}: {error}. This install is not writable by the current user, \
         which usually means a system-wide or package-manager-managed install; update it the \
         way it was installed. self-update never uses sudo",
        dir.display()
    )
}

/// The sidecar must be exactly one `<64-hex>  <archive>` line (an optional
/// `*` binary-mode marker before the name), matching the archive's digest.
pub(crate) fn verify_sha256(archive: &[u8], sidecar: &[u8], archive_name: &str) -> Result<()> {
    let text = std::str::from_utf8(sidecar).context("checksum sidecar is not UTF-8")?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    if line.contains(['\n', '\r']) {
        bail!("checksum sidecar must contain exactly one line");
    }
    let fields: Vec<&str> = line.split_whitespace().collect();
    let &[expected, name] = fields.as_slice() else {
        bail!("checksum sidecar line must have exactly two fields");
    };
    let name = name.strip_prefix('*').unwrap_or(name);
    if name != archive_name {
        bail!("checksum sidecar names {name:?}, expected {archive_name:?}");
    }
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("checksum sidecar digest is not a 64-character hex value");
    }
    let actual: String = ring::digest::digest(&ring::digest::SHA256, archive)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !expected.eq_ignore_ascii_case(&actual) {
        bail!("checksum verification failed for {archive_name}; refusing to extract");
    }
    Ok(())
}

/// Extract the single regular-file member called `wanted` (optionally `./`
/// prefixed) from a gzip-compressed tar. Other members are skipped; a
/// non-regular or duplicate `wanted` member is an error.
pub(crate) fn extract_member(archive: &[u8], wanted: &str) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(archive).take(MAX_UNPACKED_BYTES);
    let mut archive = tar::Archive::new(decoder);
    let mut found = None;
    for entry in archive.entries().context("cannot read release archive")? {
        let entry = entry.context("invalid release archive member")?;
        let name = entry.path_bytes();
        let mut name = name.as_ref();
        while let Some(rest) = name.strip_prefix(b"./") {
            name = rest;
        }
        if name != wanted.as_bytes() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            bail!("archive member {wanted:?} is not a regular file");
        }
        if found.is_some() {
            bail!("archive contains more than one {wanted:?} member");
        }
        found = Some(read_bounded(entry, MAX_UNPACKED_BYTES, wanted)?);
    }
    found.with_context(|| format!("archive does not contain {wanted:?}"))
}

/// Run the staged binary's `--version` and require `conversations <expected>`.
fn verify_staged_version(staged: &Path, expected: Version) -> Result<()> {
    let dir = staged.parent().context("staged executable has no parent")?;
    let mut stdout = tempfile::tempfile_in(dir).context("cannot stage version check output")?;
    let mut attempts = 0;
    let mut child = loop {
        let spawned = Command::new(staged)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(stdout.try_clone()?)
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            // A concurrently forked process can briefly inherit the write
            // handle we just closed; retry instead of failing the update.
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            other => break other.context("staged binary failed to start")?,
        }
    };
    let deadline = Instant::now() + VERSION_CHECK_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("staged binary did not answer --version in time");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        bail!("staged binary failed to run --version ({status})");
    }
    stdout.rewind()?;
    let bytes = read_bounded(stdout, 4096, "staged version output")?;
    let stdout = std::str::from_utf8(&bytes).context("staged version output is not UTF-8")?;
    let mut words = stdout.split_whitespace();
    let reported = (words.next(), words.next());
    if reported != (Some(BIN), Some(expected.to_string().as_str())) {
        bail!("staged binary did not report the expected version {BIN} {expected}");
    }
    Ok(())
}

pub fn run(args: &SelfUpdateArgs) -> Result<()> {
    let target =
        release_target().context("self-update supports only Linux and macOS on x86_64 or ARM64")?;
    let current = parse_version(env!("CARGO_PKG_VERSION"))
        .context("this build's version is not a strict X.Y.Z release version")?;
    // Replace the real file, not a symlink pointing at it.
    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .context("cannot locate the running conversations executable")?;
    let source = GitHubReleases::new()?;
    let outcome = update(&source, current, &exe, target, args.check)
        .with_context(|| format!("self-update failed; {} was left unchanged", exe.display()))?;
    match outcome {
        Outcome::UpToDate { current, latest } if latest == current => {
            println!("{BIN} {current} is up to date.");
        }
        Outcome::UpToDate { current, latest } => {
            println!("{BIN} {current} is newer than release v{latest}; not downgrading.");
        }
        Outcome::Available { current, latest } => {
            println!("Update available: {current} -> {latest}. Run `{BIN} self-update`.");
        }
        Outcome::Updated { from, to } => {
            println!("Updated {} from {from} to {to}.", exe.display());
            println!(
                "Already-running hosts and agents keep running {from}; restart them to use {to}."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::PathBuf;

    const TARGET: &str = "x86_64-unknown-linux-gnu";
    const OLD: &[u8] = b"old executable";

    fn v(text: &str) -> Version {
        parse_version(text).unwrap()
    }

    /// In-memory release fixture; records every asset requested.
    struct FakeRelease {
        tag: String,
        assets: HashMap<String, Vec<u8>>,
        requested: RefCell<Vec<String>>,
    }

    impl ReleaseSource for FakeRelease {
        fn latest_tag(&self) -> Result<String> {
            Ok(self.tag.clone())
        }
        fn download(&self, tag: &str, asset: &str, limit: u64) -> Result<Vec<u8>> {
            assert_eq!(tag, self.tag);
            self.requested.borrow_mut().push(asset.to_owned());
            let bytes = self
                .assets
                .get(asset)
                .with_context(|| format!("404 {asset}"))?;
            read_bounded(&bytes[..], limit, asset)
        }
    }

    fn tar_entry(name: &str, typeflag: u8, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000755\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = typeflag;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        let mut entry = header.to_vec();
        entry.extend_from_slice(data);
        entry.resize(512 + data.len().div_ceil(512) * 512, 0);
        entry
    }

    fn tar_gz(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut tar: Vec<u8> = entries.concat();
        tar.extend_from_slice(&[0; 1024]);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn fake_binary(prints: &str, exit: u8) -> Vec<u8> {
        format!("#!/bin/sh\necho '{prints}'\nexit {exit}\n").into_bytes()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        ring::digest::digest(&ring::digest::SHA256, bytes)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A release whose archive holds `entries`, with a correct sidecar.
    fn release(tag: &str, entries: &[Vec<u8>]) -> FakeRelease {
        let archive_name = format!("conversations-{}-{TARGET}.tar.gz", &tag[1..]);
        let archive = tar_gz(entries);
        let sidecar = format!("{}  {archive_name}\n", sha256_hex(&archive));
        FakeRelease {
            tag: tag.to_owned(),
            assets: HashMap::from([
                (format!("{archive_name}.sha256"), sidecar.into_bytes()),
                (archive_name, archive),
            ]),
            requested: RefCell::default(),
        }
    }

    fn good_release() -> FakeRelease {
        release(
            "v0.70.0",
            &[tar_entry(
                "conversations",
                b'0',
                &fake_binary("conversations 0.70.0 (abc1234)", 0),
            )],
        )
    }

    /// An install directory holding a stand-in for the running executable.
    fn install() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("conversations");
        fs::write(&exe, OLD).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, exe)
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The failure must leave the old binary byte-for-byte and no staged file.
    fn assert_untouched(dir: &Path, exe: &Path) {
        assert_eq!(fs::read(exe).unwrap(), OLD);
        assert_eq!(entries(dir), ["conversations"]);
    }

    fn run_update(source: &FakeRelease, exe: &Path) -> Result<Outcome> {
        update(source, v("0.69.2"), exe, TARGET, false)
    }

    #[test]
    fn a_second_update_keeps_the_installed_binary_and_skips_asset_downloads() {
        let (dir, exe) = install();
        let lock = fs::File::open(&exe).unwrap();
        lock.try_lock().unwrap();
        let release = good_release();
        let error = run_update(&release, &exe).unwrap_err();
        assert!(error.to_string().contains("another update in progress"));
        assert!(release.requested.borrow().is_empty());
        assert_untouched(dir.path(), &exe);
    }

    #[test]
    fn updates_from_a_verified_release_and_leaves_no_staging_files() {
        let (dir, exe) = install();
        let source = good_release();
        let outcome = run_update(&source, &exe).unwrap();
        assert_eq!(
            outcome,
            Outcome::Updated {
                from: v("0.69.2"),
                to: v("0.70.0")
            }
        );
        assert_eq!(
            fs::read(&exe).unwrap(),
            fake_binary("conversations 0.70.0 (abc1234)", 0)
        );
        assert_eq!(
            fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(entries(dir.path()), ["conversations"]);
        assert_eq!(
            *source.requested.borrow(),
            [
                format!("conversations-0.70.0-{TARGET}.tar.gz.sha256"),
                format!("conversations-0.70.0-{TARGET}.tar.gz"),
            ]
        );
    }

    #[test]
    fn extracts_only_the_conversations_member_among_others() {
        let (dir, exe) = install();
        let new = fake_binary("conversations 0.70.0 (abc1234)", 0);
        let source = release(
            "v0.70.0",
            &[
                tar_entry("._conversations", b'0', b"appledouble"),
                tar_entry("README", b'0', b"readme"),
                tar_entry("./conversations", b'0', &new),
            ],
        );
        run_update(&source, &exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), new);
        assert_eq!(entries(dir.path()), ["conversations"]);
    }

    #[test]
    fn bad_digest_keeps_the_old_executable() {
        let (dir, exe) = install();
        let mut source = good_release();
        let sidecar = format!("{}  conversations-0.70.0-{TARGET}.tar.gz\n", "0".repeat(64));
        source.assets.insert(
            format!("conversations-0.70.0-{TARGET}.tar.gz.sha256"),
            sidecar.into_bytes(),
        );
        let error = run_update(&source, &exe).unwrap_err();
        assert!(
            format!("{error:#}").contains("checksum verification failed"),
            "{error:#}"
        );
        assert_untouched(dir.path(), &exe);
    }

    #[test]
    fn malformed_sidecars_are_rejected() {
        let archive = b"archive bytes";
        let name = "conversations-0.70.0-x.tar.gz";
        let digest = sha256_hex(archive);
        verify_sha256(archive, format!("{digest}  {name}\n").as_bytes(), name).unwrap();
        verify_sha256(
            archive,
            format!("{}  *{name}", digest.to_uppercase()).as_bytes(),
            name,
        )
        .unwrap();
        for sidecar in [
            format!("{digest}  {name}\n{digest}  {name}\n"),
            format!("{digest}  {name}\r\n"),
            format!("{digest}  other.tar.gz\n"),
            format!("{digest}\n"),
            format!("{digest}  {name} extra\n"),
            format!("{}  {name}\n", &digest[..63]),
            format!("{}g  {name}\n", &digest[..63]),
            String::new(),
        ] {
            assert!(
                verify_sha256(archive, sidecar.as_bytes(), name).is_err(),
                "{sidecar:?}"
            );
        }
    }

    #[test]
    fn failed_staged_verification_keeps_the_old_executable() {
        for (binary, why) in [
            (
                fake_binary("conversations 0.69.9 (abc1234)", 0),
                "staged binary did not report the expected version",
            ),
            (
                fake_binary("conversations 0.70.0 (abc1234)", 1),
                "failed to run --version",
            ),
            (
                fake_binary("something-else 0.70.0", 0),
                "staged binary did not report the expected version",
            ),
            // Not executable as a program; depending on the libc this fails
            // to spawn or runs through the shell and exits non-zero.
            (b"\x7fELF not really".to_vec(), "staged binary"),
        ] {
            let (dir, exe) = install();
            let source = release("v0.70.0", &[tar_entry("conversations", b'0', &binary)]);
            let error = run_update(&source, &exe).unwrap_err();
            assert!(format!("{error:#}").contains(why), "{why}: {error:#}");
            assert_untouched(dir.path(), &exe);
        }
    }

    #[test]
    fn archives_without_a_regular_conversations_member_keep_the_old_executable() {
        for entries_ in [
            vec![tar_entry("bin/conversations", b'0', b"x")],
            vec![tar_entry("conversations", b'2', b"")],
            vec![
                tar_entry("conversations", b'0', b"a"),
                tar_entry("conversations", b'0', b"b"),
            ],
        ] {
            let (dir, exe) = install();
            assert!(run_update(&release("v0.70.0", &entries_), &exe).is_err());
            assert_untouched(dir.path(), &exe);
        }
        let (dir, exe) = install();
        let mut source = good_release();
        let archive = format!("conversations-0.70.0-{TARGET}.tar.gz");
        let garbage = b"not gzip".to_vec();
        let sidecar = format!("{}  {archive}\n", sha256_hex(&garbage));
        source.assets.insert(archive.clone(), garbage);
        source
            .assets
            .insert(format!("{archive}.sha256"), sidecar.into_bytes());
        assert!(run_update(&source, &exe).is_err());
        assert_untouched(dir.path(), &exe);
    }

    #[test]
    fn pax_path_overrides_the_header_name() {
        let record = b"24 path=./conversations\n";
        assert_eq!(record.len(), 24);
        let archive = tar_gz(&[
            tar_entry("PaxHeader/x", b'x', record),
            tar_entry("renamed-by-pax", b'0', b"binary"),
        ]);
        assert_eq!(
            extract_member(&archive, "conversations").unwrap(),
            b"binary"
        );
    }

    #[test]
    fn equal_or_older_releases_and_check_mode_download_nothing() {
        let stale = [
            ("v0.69.2", "0.69.2"),
            ("v0.69.1", "0.69.2"),
            ("v0.70.0", "0.70.1"),
        ];
        for (tag, current) in stale {
            let (dir, exe) = install();
            let mut source = good_release();
            source.tag = tag.to_owned();
            let outcome = update(&source, v(current), &exe, TARGET, false).unwrap();
            assert!(matches!(outcome, Outcome::UpToDate { .. }), "{tag}");
            assert!(source.requested.borrow().is_empty());
            assert_untouched(dir.path(), &exe);
        }
        let (dir, exe) = install();
        let source = good_release();
        let outcome = update(&source, v("0.69.2"), &exe, TARGET, true).unwrap();
        assert_eq!(
            outcome,
            Outcome::Available {
                current: v("0.69.2"),
                latest: v("0.70.0")
            }
        );
        assert!(source.requested.borrow().is_empty());
        assert_untouched(dir.path(), &exe);
    }

    #[test]
    fn a_non_strict_latest_tag_is_rejected_before_downloading() {
        for tag in [
            "0.70.0",
            "v0.70",
            "v0.70.0-rc1",
            "v0.070.0",
            "v0.70.0 ",
            "latest",
        ] {
            let (dir, exe) = install();
            let mut source = good_release();
            source.tag = tag.to_owned();
            assert!(run_update(&source, &exe).is_err(), "{tag}");
            assert!(source.requested.borrow().is_empty());
            assert_untouched(dir.path(), &exe);
        }
    }

    #[test]
    fn unwritable_install_directory_fails_before_downloading() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores directory permissions
        }
        let (dir, exe) = install();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let source = good_release();
        let error = run_update(&source, &exe).unwrap_err();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            format!("{error:#}").contains("never uses sudo"),
            "{error:#}"
        );
        assert!(source.requested.borrow().is_empty());
        assert_untouched(dir.path(), &exe);
    }

    #[test]
    fn oversized_downloads_are_rejected() {
        let (dir, exe) = install();
        let mut source = good_release();
        let sidecar = format!("conversations-0.70.0-{TARGET}.tar.gz.sha256");
        source
            .assets
            .insert(sidecar, vec![b'a'; MAX_SIDECAR_BYTES as usize + 1]);
        let error = run_update(&source, &exe).unwrap_err();
        assert!(format!("{error:#}").contains("limit"), "{error:#}");
        assert_untouched(dir.path(), &exe);
        assert_eq!(read_bounded(&b"12345"[..], 5, "x").unwrap(), b"12345");
        assert!(read_bounded(&b"123456"[..], 5, "x").is_err());
    }

    #[test]
    fn strict_versions_and_latest_redirects() {
        assert_eq!(parse_tag("v1.20.3"), Some(Version(1, 20, 3)));
        assert_eq!(parse_tag("v0.0.0"), Some(Version(0, 0, 0)));
        let invalid = [
            "1.2.3",
            "v1.2",
            "v1.2.3.4",
            "v1.02.3",
            "v1.2.3-rc1",
            "v+1.2.3",
            "V1.2.3",
        ];
        for bad in invalid {
            assert_eq!(parse_tag(bad), None, "{bad}");
        }
        assert!(v("0.70.0") > v("0.69.12") && v("1.0.0") > v("0.99.99"));
        assert!(parse_version(env!("CARGO_PKG_VERSION")).is_some());
        let ok = format!("{REPO_URL}/releases/tag/v0.70.0");
        assert_eq!(tag_from_latest_location(&ok).unwrap(), "v0.70.0");
        for bad in [
            format!("{REPO_URL}/releases"),
            format!("{REPO_URL}/releases/tag/v0.70"),
            "https://github.com/someone-else/conversations/releases/tag/v0.70.0".to_owned(),
            "http://github.com/SjuulArtreides/conversations/releases/tag/v0.70.0".to_owned(),
            "/SjuulArtreides/conversations/releases/tag/v0.70.0".to_owned(),
        ] {
            assert!(tag_from_latest_location(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_test_host_has_a_release_target() {
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(release_target().is_some());
        }
    }
}
