//! Host side of terminal file drops: stages dropped files in a private
//! per-session temp dir and pastes one bracketed path per file (`ESC[200~ path
//! ESC[201~`, never Enter) so the agent attaches them. Nothing is ever submitted,
//! and staged files live until the session host ends (Codex reads them later).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use conversations_core::terminal_files::{
    validate_file_name, TerminalFile, TerminalFileStatus, MAX_FILES, MAX_TOTAL_BYTES,
};
use tempfile::TempDir;

use super::interactive::Shutdown;
use crate::commands::local_host::transfers::{Claim, Permit};

type PtyWriter<W = Box<dyn Write + Send>> = Arc<Mutex<W>>;

#[derive(Clone, Copy)]
pub(crate) enum FileDiagnostics {
    Legacy,
    Quiet,
}

impl FileDiagnostics {
    fn error(self, message: std::fmt::Arguments<'_>) {
        if matches!(self, Self::Legacy) {
            live_error!("{message}");
        }
    }
}

/// Retained staged bytes allowed per session host.
const MAX_SESSION_BYTES: usize = 256 * 1024 * 1024;
/// Staged files retained per session, so empty files cannot exhaust inodes.
const MAX_SESSION_FILES: usize = 4096;
/// Gap between per-file pastes. OpenCode parses pastes asynchronously (it
/// serialises them, so order is kept), Codex synchronously; the gap keeps both
/// from seeing one burst.
const PASTE_GAP: Duration = Duration::from_millis(100);
/// Codex turns any paste over 1000 chars into a placeholder, not an attachment.
const MAX_ENCODED_PATH_CHARS: usize = 900;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Per-session staging area. The temp dir is created on first use and removed
/// when this value drops (session host teardown).
pub(crate) struct FileStage {
    root: Option<TempDir>,
    retained: usize,
    retained_files: usize,
    cap: usize,
    file_cap: usize,
}

impl FileStage {
    pub(crate) fn new() -> Self {
        Self::with_caps(MAX_SESSION_BYTES, MAX_SESSION_FILES)
    }

    fn with_caps(cap: usize, file_cap: usize) -> Self {
        Self {
            root: None,
            retained: 0,
            retained_files: 0,
            cap,
            file_cap,
        }
    }

    /// Validate and decode the whole batch, then write every file. Either all
    /// files are staged (paths returned in order) or none are kept.
    #[cfg(test)]
    fn stage(&mut self, files: &[TerminalFile]) -> Result<Vec<PathBuf>> {
        self.stage_with(files, &|| Ok(()))
    }

    /// [`Self::stage`], asking `still_valid` before each file is written: an
    /// expired transfer stops there and the files already written are removed.
    fn stage_with(
        &mut self,
        files: &[TerminalFile],
        still_valid: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<PathBuf>> {
        if files.is_empty() || files.len() > MAX_FILES {
            bail!("invalid file count {}", files.len());
        }
        if self.retained_files + files.len() > self.file_cap {
            bail!("session staging cap of {} files reached", self.file_cap);
        }
        let engine = base64::engine::general_purpose::STANDARD;
        let mut decoded = Vec::with_capacity(files.len());
        let mut total = 0usize;
        for file in files {
            validate_file_name(&file.name).map_err(|e| anyhow!("rejected file name: {e}"))?;
            if file.name.contains(['\u{2028}', '\u{2029}']) {
                bail!("rejected file name: line separator");
            }
            // Cheap bound before allocating the decoded bytes.
            if file.b64.len() / 4 * 3 > MAX_TOTAL_BYTES + 3 {
                bail!("batch exceeds {MAX_TOTAL_BYTES} bytes");
            }
            let bytes = engine
                .decode(&file.b64)
                .context("file content is not valid base64")?;
            total += bytes.len();
            if total > MAX_TOTAL_BYTES {
                bail!("batch exceeds {MAX_TOTAL_BYTES} bytes");
            }
            decoded.push(bytes);
        }
        if self.retained + total > self.cap {
            bail!("session staging cap of {} bytes reached", self.cap);
        }

        if self.root.is_none() {
            let dir = tempfile::Builder::new()
                .prefix("conv-drop-")
                .tempdir()
                .context("creating private drop dir")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                    .context("restricting drop dir")?;
            }
            self.root = Some(dir);
        }
        let root = self.root.as_ref().expect("root set above").path();
        let transfer_dir = root.join(format!("t-{}", uuid::Uuid::new_v4()));
        create_private_dir(&transfer_dir)?;

        let result = (|| -> Result<Vec<PathBuf>> {
            let mut paths = Vec::with_capacity(files.len());
            for (i, (file, bytes)) in files.iter().zip(&decoded).enumerate() {
                still_valid()?;
                // One dir per file keeps the original basename intact even
                // for duplicate names.
                let dir = transfer_dir.join(i.to_string());
                create_private_dir(&dir)?;
                let path = dir.join(&file.name);
                create_private_file(&path, bytes)?;
                paths.push(path);
            }
            Ok(paths)
        })();
        match result {
            Ok(paths) => {
                self.retained += total;
                self.retained_files += paths.len();
                Ok(paths)
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&transfer_dir);
                Err(e)
            }
        }
    }
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir(path).with_context(|| format!("creating {}", path.display()))
}

fn create_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)?;
    f.flush()?;
    Ok(())
}

/// Encode a path so OpenCode (strips outer quotes, then unescapes every `\x`)
/// and Codex (`shlex`, single token) both recover it. Both `trim()` the paste
/// first, so a trailing space cannot be a bare `\ `: such paths are wrapped in
/// double quotes instead, escaping only `"`, `\`, `$` and the backtick, which
/// both parsers undo identically. All other paths are backslash-escaped.
fn encode_path(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        bail!("path is not absolute");
    }
    let s = path.to_str().context("path is not UTF-8")?;
    if s.chars()
        .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
    {
        bail!("path contains a control character");
    }
    let trimmable_end = s
        .chars()
        .next_back()
        .is_some_and(|c| c.is_whitespace() || c == '\u{feff}');
    let mut out = String::with_capacity(s.len() + 8);
    if trimmable_end {
        out.push('"');
        for c in s.chars() {
            if matches!(c, '"' | '\\' | '$' | '`') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        for c in s.chars() {
            let plain = c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '/' | '.' | '_' | '-' | '+' | '@' | ',' | ':' | '=' | '%' | '~'
                )
                || (!c.is_ascii() && !c.is_whitespace());
            if !plain {
                out.push('\\');
            }
            out.push(c);
        }
    }
    if out.chars().count() > MAX_ENCODED_PATH_CHARS {
        bail!("encoded path is too long");
    }
    Ok(out)
}

fn paste_bytes(path: &Path) -> Result<Vec<u8>> {
    let enc = encode_path(path)?;
    let mut out = Vec::with_capacity(enc.len() + 12);
    out.extend_from_slice(PASTE_START);
    out.extend_from_slice(enc.as_bytes());
    out.extend_from_slice(PASTE_END);
    Ok(out)
}

#[derive(Clone)]
// Local elapsed clocks only: wall time covers suspend on platforms whose
// monotonic clock pauses. Backwards wall steps fail closed. A paused VM where
// neither clock advances cannot be detected locally.
pub(crate) struct Deadline {
    started: tokio::time::Instant,
    wall_started: std::time::SystemTime,
    ttl: Duration,
    /// The signed authority the paste must still hold when it is written.
    permit: Permit,
}

impl Deadline {
    fn start() -> Self {
        Self {
            started: tokio::time::Instant::now(),
            wall_started: std::time::SystemTime::now(),
            ttl: Duration::ZERO,
            permit: Permit::default(),
        }
    }

    /// The trusted deadline: what the host's signed authorization had left when
    /// the wrapper claimed it. Everything later may only shorten it.
    pub(crate) fn from_claim(claim: Claim) -> Self {
        Self {
            ttl: claim.left,
            permit: claim.permit,
            ..Self::start()
        }
    }

    fn elapsed_expired(ttl: Duration, monotonic: Duration, wall: Option<Duration>) -> bool {
        monotonic >= ttl || wall.is_none_or(|elapsed| elapsed >= ttl)
    }

    fn expired(&self) -> bool {
        Self::elapsed_expired(
            self.ttl,
            self.started.elapsed(),
            self.wall_started.elapsed().ok(),
        )
    }
}

fn still_deliverable(deadline: &Deadline, shutdown: &Shutdown) -> Result<()> {
    if deadline.expired() {
        bail!("transfer expired before delivery completed");
    }
    if shutdown.is_set() {
        bail!("session is shutting down");
    }
    if let Some(reason) = deadline.permit.denial() {
        bail!("transfer authority no longer holds: {reason}");
    }
    Ok(())
}

/// Paste on a blocking worker, holding the writer only per paste. Check expiry,
/// shutdown and future cancellation inside that worker, including after locking:
/// a queued worker must never start an expired or cancelled paste. A PTY
/// write that has already started may still finish after the server's timeout;
/// that is the accepted "uncertain" outcome (the user inspects the prompt).
/// Stops (failed) at expiry, shutdown or the first writer error; partial
/// delivery is never retried.
async fn deliver<W: Write + Send + 'static>(
    paths: &[PathBuf],
    writer: &PtyWriter<W>,
    deadline: Deadline,
    gap: Duration,
    shutdown: &Arc<Shutdown>,
) -> Result<()> {
    let pastes = paths
        .iter()
        .map(|p| paste_bytes(p))
        .collect::<Result<Vec<_>>>()?;
    struct CancelOnDrop(Arc<AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(cancelled.clone());
    let (writer, shutdown) = (writer.clone(), shutdown.clone());
    tokio::task::spawn_blocking(move || {
        let check = || -> Result<()> {
            if cancelled.load(Ordering::SeqCst) {
                bail!("file delivery cancelled");
            }
            still_deliverable(&deadline, &shutdown)
        };
        for (i, bytes) in pastes.iter().enumerate() {
            loop {
                check()?;
                let acquired = match writer.try_lock() {
                    Ok(mut w) => {
                        check()?;
                        w.write_all(bytes).context("writing paste to pty")?;
                        w.flush().context("flushing paste to pty")?;
                        true
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => bail!("pty writer poisoned"),
                    Err(std::sync::TryLockError::WouldBlock) => false,
                };
                if acquired {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if i + 1 < pastes.len() {
                std::thread::sleep(gap);
            }
        }
        Ok(())
    })
    .await
    .context("file delivery worker failed")?
}

/// Handle one `files` event end to end and return the outcome to acknowledge.
async fn handle_files<W: Write + Send + 'static>(
    stage: &Arc<Mutex<FileStage>>,
    writer: &PtyWriter<W>,
    files: Vec<TerminalFile>,
    deadline: Deadline,
    gap: Duration,
    shutdown: &Arc<Shutdown>,
    diagnostics: FileDiagnostics,
) -> TerminalFileStatus {
    if still_deliverable(&deadline, shutdown).is_err() {
        diagnostics.error(format_args!("live: file drop expired before staging"));
        return TerminalFileStatus::Failed;
    }
    let staged = {
        let stage = Arc::clone(stage);
        let shutdown = shutdown.clone();
        let deadline = deadline.clone();
        tokio::task::spawn_blocking(move || {
            let mut stage = stage.lock().map_err(|_| anyhow!("stage poisoned"))?;
            still_deliverable(&deadline, &shutdown)?;
            stage.stage_with(&files, &|| still_deliverable(&deadline, &shutdown))
        })
        .await
    };
    let paths = match staged {
        Ok(Ok(paths)) => paths,
        Ok(Err(e)) => {
            diagnostics.error(format_args!("live: file drop staging failed: {e:#}"));
            return TerminalFileStatus::Failed;
        }
        Err(e) => {
            diagnostics.error(format_args!("live: file drop staging task failed: {e}"));
            return TerminalFileStatus::Failed;
        }
    };
    match deliver(&paths, writer, deadline, gap, shutdown).await {
        Ok(()) => TerminalFileStatus::Sent,
        Err(e) => {
            diagnostics.error(format_args!("live: file drop delivery failed: {e:#}"));
            TerminalFileStatus::Failed
        }
    }
}

/// Stage and paste the files of one claimed transfer whose bytes came from the
/// local host (received on the encrypted private channel), exactly like a file
/// drop: the claim's deadline and signed authority are re-checked before staging
/// and under the writer lock.
pub(crate) async fn apply_files<W: Write + Send + 'static>(
    stage: &Arc<Mutex<FileStage>>,
    writer: &PtyWriter<W>,
    files: Vec<TerminalFile>,
    claim: Claim,
    shutdown: &Arc<Shutdown>,
) -> TerminalFileStatus {
    handle_files(
        stage,
        writer,
        files,
        Deadline::from_claim(claim),
        PASTE_GAP,
        shutdown,
        FileDiagnostics::Quiet,
    )
    .await
}

/// File extension for the image types a signed paste may carry. Anything else is
/// refused rather than staged under a misleading name.
fn image_extension(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// Apply one authorized image: stage it in this session's private directory under
/// a generated name and paste its path (bracketed, never Enter), exactly like a
/// file drop. The machine clipboard is never used: it is shared by every session
/// on the machine, and an agent reads it later, so it cannot bind the image to
/// the session the signed operation named.
pub(crate) async fn stage_and_paste_image<W: Write + Send + 'static>(
    stage: &Arc<Mutex<FileStage>>,
    writer: &PtyWriter<W>,
    bytes: &[u8],
    mime: &str,
    permit: Deadline,
    shutdown: &Arc<Shutdown>,
) -> TerminalFileStatus {
    let Some(ext) = image_extension(mime) else {
        live_error!("live: unsupported image type {mime}; nothing staged");
        return TerminalFileStatus::Failed;
    };
    let file = TerminalFile {
        name: format!("pasted-image-{}.{ext}", uuid::Uuid::new_v4().simple()),
        b64: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    handle_files(
        stage,
        writer,
        vec![file],
        permit,
        Duration::ZERO,
        shutdown,
        FileDiagnostics::Legacy,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn file(name: &str, content: &[u8]) -> TerminalFile {
        TerminalFile {
            name: name.into(),
            b64: base64::engine::general_purpose::STANDARD.encode(content),
        }
    }

    #[derive(Clone, Default)]
    struct Recorder {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        fail_after: Option<usize>,
        count: Arc<AtomicUsize>,
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            if self.fail_after.is_some_and(|limit| n >= limit) {
                return Err(std::io::Error::other("closed"));
            }
            self.writes.lock().unwrap().push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn writer(rec: &Recorder) -> PtyWriter {
        Arc::new(Mutex::new(Box::new(rec.clone())))
    }

    fn deadline_after(ttl: Duration) -> Deadline {
        Deadline {
            ttl,
            ..Deadline::start()
        }
    }

    fn far_future() -> Deadline {
        deadline_after(Duration::from_secs(60))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_file_write_does_not_starve_end_on_current_thread() {
        #[derive(Default)]
        struct Gate {
            entered: bool,
            end_processed: bool,
            release: bool,
            writes: usize,
            flushed: bool,
        }
        struct BlockedWriter {
            gate: Arc<(Mutex<Gate>, std::sync::Condvar)>,
            started: Option<tokio::sync::oneshot::Sender<()>>,
        }
        impl Write for BlockedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let (mutex, changed) = &*self.gate;
                let mut gate = mutex.lock().unwrap();
                gate.entered = true;
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                }
                changed.notify_all();
                while !gate.release {
                    gate = changed.wait(gate).unwrap();
                }
                gate.writes += 1;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.gate.0.lock().unwrap().flushed = true;
                Ok(())
            }
        }
        let gate = Arc::new((Mutex::new(Gate::default()), std::sync::Condvar::new()));
        let watchdog = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                let (mutex, changed) = &*gate;
                let (state, _) = changed
                    .wait_timeout_while(mutex.lock().unwrap(), Duration::from_secs(5), |state| {
                        !state.entered
                    })
                    .unwrap();
                let (mut state, _) = changed
                    .wait_timeout_while(state, Duration::from_secs(1), |state| !state.end_processed)
                    .unwrap();
                let progressed = state.end_processed;
                state.release = true;
                changed.notify_all();
                progressed
            })
        };
        let (started, entered) = tokio::sync::oneshot::channel();
        let writer = Arc::new(Mutex::new(BlockedWriter {
            gate: gate.clone(),
            started: Some(started),
        }));
        let shutdown = Shutdown::new();
        let delivery = {
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                deliver(
                    &[PathBuf::from("/first.bin"), PathBuf::from("/second.bin")],
                    &writer,
                    far_future(),
                    Duration::ZERO,
                    &shutdown,
                )
                .await
            })
        };
        entered.await.unwrap();
        let pending = !delivery.is_finished();
        // This is the same executor that must process wrapper End and signals.
        shutdown.trigger();
        gate.0.lock().unwrap().end_processed = true;
        gate.1.notify_all();
        let outcome = delivery.await.unwrap();
        assert!(
            watchdog.join().unwrap(),
            "blocked file write starved End on the current-thread executor"
        );
        assert!(
            pending,
            "delivery completed before the actual write and flush"
        );
        assert!(outcome.is_err());
        let state = gate.0.lock().unwrap();
        assert_eq!(state.writes, 1, "shutdown must prevent the second paste");
        assert!(state.flushed);
    }

    #[test]
    fn queued_pastes_recheck_expiry_shutdown_and_future_cancellation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            for reason in ["expiry", "shutdown", "abort"] {
                let (release, held) = std::sync::mpsc::channel();
                let (entered, ready) = tokio::sync::oneshot::channel();
                let occupying = tokio::task::spawn_blocking(move || {
                    entered.send(()).unwrap();
                    held.recv().unwrap();
                });
                ready.await.unwrap();
                let rec = Recorder::default();
                let writer = writer(&rec);
                let shutdown = Shutdown::new();
                let deadline = deadline_after(if reason == "expiry" {
                    Duration::from_millis(30)
                } else {
                    Duration::from_secs(20)
                });
                let mut delivery = Some({
                    let (writer, shutdown) = (writer.clone(), shutdown.clone());
                    tokio::spawn(async move {
                        deliver(
                            &[PathBuf::from("/queued.bin")],
                            &writer,
                            deadline,
                            Duration::ZERO,
                            &shutdown,
                        )
                        .await
                    })
                });
                tokio::task::yield_now().await;
                assert!(!delivery.as_ref().unwrap().is_finished());
                if reason == "expiry" {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                } else if reason == "shutdown" {
                    shutdown.trigger();
                } else {
                    let task = delivery.take().unwrap();
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled());
                    assert!(
                        Arc::strong_count(&writer) > 1,
                        "the blocking job must already be queued"
                    );
                }
                assert!(rec.writes.lock().unwrap().is_empty());
                release.send(()).unwrap();
                occupying.await.unwrap();
                if let Some(task) = delivery {
                    assert!(task.await.unwrap().is_err());
                }
                wait_for(|| Arc::strong_count(&writer) == 1).await;
                assert!(
                    rec.writes.lock().unwrap().is_empty(),
                    "{reason} allowed a late queued paste"
                );
            }
        });
    }

    #[test]
    fn expiry_between_files_stops_staging_and_cleans_up() {
        let mut stage = FileStage::new();
        let files = vec![
            file("a.txt", b"1"),
            file("b.txt", b"2"),
            file("c.txt", b"3"),
        ];
        let written = std::cell::Cell::new(0);
        let result = stage.stage_with(&files, &|| {
            written.set(written.get() + 1);
            if written.get() > 1 {
                bail!("transfer expired")
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(written.get(), 2, "the remaining files were never written");
        let root = stage.root.as_ref().unwrap().path();
        assert_eq!(
            std::fs::read_dir(root).unwrap().count(),
            0,
            "partial batch removed"
        );
        assert_eq!((stage.retained, stage.retained_files), (0, 0));
    }
    #[test]
    fn deadline_fails_closed_for_suspend_clock_steps_and_monotonic_expiry() {
        let seconds = Duration::from_secs;
        let expired = |mono: u64, wall: Option<u64>| {
            Deadline::elapsed_expired(seconds(20), seconds(mono), wall.map(seconds))
        };
        assert!(!expired(1, Some(1)));
        assert!(expired(1, Some(25)), "suspend must not allow a stale paste");
        assert!(expired(1, None), "a backwards local clock fails closed");
        assert!(expired(20, Some(1)));
    }

    #[tokio::test]
    async fn suspended_deadline_never_stages_or_pastes() {
        let rec = Recorder::default();
        let stage = Arc::new(Mutex::new(FileStage::new()));
        let mut deadline = deadline_after(Duration::from_secs(20));
        deadline.wall_started = std::time::SystemTime::now() - Duration::from_secs(25);
        let status = handle_files(
            &stage,
            &writer(&rec),
            vec![file("image.png", b"data")],
            deadline,
            PASTE_GAP,
            &Shutdown::new(),
            FileDiagnostics::Legacy,
        )
        .await;
        assert_eq!(status, TerminalFileStatus::Failed);
        assert!(stage.lock().unwrap().root.is_none());
        assert!(rec.writes.lock().unwrap().is_empty());
    }

    #[test]
    fn stages_byte_identical_including_duplicates_and_unicode() {
        let mut stage = FileStage::new();
        let files = vec![
            file("a b.png", &[0, 1, 2, 255]),
            file("a b.png", b"second"),
            file("日本語 'q' \"d\" (1).txt", b"unicode"),
            file("empty", b""),
        ];
        let paths = stage.stage(&files).unwrap();
        assert_eq!(paths.len(), 4);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), vec![0, 1, 2, 255]);
        assert_eq!(std::fs::read(&paths[1]).unwrap(), b"second");
        assert_ne!(paths[0], paths[1]);
        for (p, f) in paths.iter().zip(&files) {
            assert!(p.is_absolute());
            assert_eq!(p.file_name().unwrap().to_str().unwrap(), f.name);
        }
        assert_eq!(std::fs::read(&paths[2]).unwrap(), b"unicode");
        assert_eq!(std::fs::read(&paths[3]).unwrap(), b"");
    }

    #[cfg(unix)]
    #[test]
    fn staging_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let mut stage = FileStage::new();
        let paths = stage.stage(&[file("a.png", b"x")]).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&paths[0]), 0o600);
        assert_eq!(mode(paths[0].parent().unwrap()), 0o700);
        assert_eq!(mode(stage.root.as_ref().unwrap().path()), 0o700);
    }

    #[test]
    fn rejects_unsafe_names_and_bad_batches_without_staging() {
        for bad in [
            "../x",
            "a/b",
            "a\\b",
            "..",
            ".",
            "",
            "a\u{1b}[201~b",
            "a\nb",
            "a\u{2028}b",
        ] {
            let mut stage = FileStage::new();
            assert!(stage.stage(&[file(bad, b"x")]).is_err(), "{bad:?}");
            assert!(stage.root.is_none(), "nothing staged for {bad:?}");
        }
        let mut stage = FileStage::new();
        assert!(stage.stage(&[]).is_err());
        let many: Vec<_> = (0..=MAX_FILES)
            .map(|i| file(&format!("f{i}"), b"x"))
            .collect();
        assert!(stage.stage(&many).is_err());
        let bad_b64 = TerminalFile {
            name: "a".into(),
            b64: "!!!".into(),
        };
        assert!(stage.stage(&[file("ok", b"x"), bad_b64]).is_err());
        assert!(stage.root.is_none(), "invalid batch writes nothing");
    }

    #[test]
    fn enforces_batch_and_session_limits_without_deleting_pending() {
        let mut stage = FileStage::new();
        let big = vec![0u8; MAX_TOTAL_BYTES];
        let over = vec![0u8; MAX_TOTAL_BYTES / 2 + 1];
        assert!(stage.stage(&[file("a", &over), file("b", &over)]).is_err());

        let mut stage = FileStage::with_caps(10, 100);
        let first = stage.stage(&[file("a.png", b"123456")]).unwrap();
        assert!(stage.stage(&[file("b.png", b"123456")]).is_err());
        assert_eq!(std::fs::read(&first[0]).unwrap(), b"123456");
        // Limit counts retained bytes: a fitting batch still works.
        assert!(stage.stage(&[file("c.png", b"1234")]).is_ok());
        drop(big);
    }

    #[cfg(unix)]
    #[test]
    fn io_failure_is_reported_and_not_counted() {
        use std::os::unix::fs::PermissionsExt;
        let mut stage = FileStage::with_caps(100, 100);
        let ok = stage.stage(&[file("a", b"x")]).unwrap();
        let root = stage.root.as_ref().unwrap().path().to_path_buf();
        let set = |m| std::fs::set_permissions(&root, std::fs::Permissions::from_mode(m)).unwrap();
        set(0o500);
        let failed = stage.stage(&[file("x", b"1")]);
        set(0o700);
        assert!(failed.is_err());
        assert_eq!(stage.retained, 1);
        assert!(ok[0].exists());
    }

    #[test]
    fn temp_dir_lives_with_stage_and_goes_away_on_drop() {
        let mut stage = FileStage::new();
        let paths = stage.stage(&[file("a.png", b"x")]).unwrap();
        let root = stage.root.as_ref().unwrap().path().to_path_buf();
        assert!(paths[0].exists());
        stage.stage(&[file("b.png", b"y")]).unwrap();
        assert!(paths[0].exists(), "earlier attachment survives later drops");
        drop(stage);
        assert!(!root.exists());
    }

    #[test]
    fn path_encoding_round_trips_through_shell_unescape() {
        // Decoder mirroring both CLIs: drop a backslash, keep the next char.
        fn unescape(s: &str) -> String {
            let mut out = String::new();
            let mut it = s.chars();
            while let Some(c) = it.next() {
                if c == '\\' {
                    out.push(it.next().unwrap());
                } else {
                    out.push(c);
                }
            }
            out
        }
        for name in [
            "plain.png",
            "a b.png",
            "it's \"q\" $x `y` (1) [2] {3} & ; | < > * ? ! # \\ .png",
            "日本語 é.png",
            "tab\u{a0}nbsp.png",
        ] {
            let p = PathBuf::from("/tmp/conv-drop-x/t-1/0").join(name);
            let enc = encode_path(&p).unwrap();
            assert_eq!(unescape(&enc), p.to_str().unwrap(), "{name}");
            assert!(!enc.contains(['\n', '\r', '\u{1b}']));
        }
        assert!(encode_path(Path::new("rel/a.png")).is_err());
        assert!(encode_path(Path::new("/tmp/a\u{1b}b")).is_err());
    }

    #[test]
    fn trailing_whitespace_names_survive_the_parsers_trim() {
        // Codex: trim, strip one quote pair, shlex (double quotes unescape
        // only `"`, `\\`, `$`, backtick). OpenCode: trim, strip outer quotes,
        // unescape every `\x`. Both must yield the exact path.
        fn shlex_dq(s: &str) -> String {
            let inner = s.strip_prefix('"').unwrap().strip_suffix('"').unwrap();
            let mut out = String::new();
            let mut it = inner.chars().peekable();
            while let Some(c) = it.next() {
                match (c, it.peek().copied()) {
                    ('\\', Some(n @ ('"' | '\\' | '$' | '`'))) => {
                        out.push(n);
                        it.next();
                    }
                    _ => out.push(c),
                }
            }
            out
        }
        fn opencode(s: &str) -> String {
            let inner = s.trim_matches(|c| c == '\'' || c == '"');
            let mut out = String::new();
            let mut it = inner.chars();
            while let Some(c) = it.next() {
                out.push(if c == '\\' { it.next().unwrap() } else { c });
            }
            out
        }
        for name in [
            "trailing.png ",
            "wide\u{3000}.png\u{3000}",
            "nb\u{a0}",
            "bom\u{feff}",
            "q \"$x\" `y` it's.png ",
            "a\\b.png ",
        ] {
            let p = PathBuf::from("/tmp/conv-drop-x/t-1/0").join(name);
            let enc = encode_path(&p).unwrap();
            assert_eq!(enc.trim(), enc, "{name:?} must not be trimmed");
            assert_eq!(shlex_dq(&enc), p.to_str().unwrap(), "codex {name:?}");
            assert_eq!(opencode(&enc), p.to_str().unwrap(), "opencode {name:?}");
        }
    }

    #[test]
    fn paste_bytes_are_bracketed_with_no_enter() {
        let b = paste_bytes(Path::new("/tmp/a b.png")).unwrap();
        assert_eq!(b, b"\x1b[200~/tmp/a\\ b.png\x1b[201~");
        assert!(!b.contains(&b'\r') && !b.contains(&b'\n'));
    }

    #[tokio::test]
    async fn delivers_one_paste_per_file_in_order_after_full_staging() {
        let rec = Recorder::default();
        let stage = Arc::new(Mutex::new(FileStage::new()));
        let sd = Shutdown::new();
        let files = vec![
            file("a.png", b"1"),
            file("b c.png", b"2"),
            file("a.png", b"3"),
        ];
        let status = handle_files(
            &stage,
            &writer(&rec),
            files,
            far_future(),
            Duration::from_millis(1),
            &sd,
            FileDiagnostics::Legacy,
        )
        .await;
        assert_eq!(status, TerminalFileStatus::Sent);
        let writes = rec.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 3, "one write per file");
        let root = stage
            .lock()
            .unwrap()
            .root
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();
        let mut seen = Vec::new();
        for (w, content) in writes.iter().zip([b"1", b"2", b"3"]) {
            let s = std::str::from_utf8(w).unwrap();
            let inner = s
                .strip_prefix("\x1b[200~")
                .unwrap()
                .strip_suffix("\x1b[201~")
                .unwrap();
            assert!(!inner.contains(['\r', '\n', '\u{1b}']));
            let path = inner.replace("\\ ", " ");
            assert!(path.starts_with(root.to_str().unwrap()));
            assert_eq!(
                std::fs::read(&path).unwrap(),
                content,
                "staged before paste"
            );
            seen.push(path);
        }
        seen.dedup();
        assert_eq!(seen.len(), 3, "duplicate names get distinct paths");
        assert!(!writes.iter().flatten().any(|b| *b == b'\r'));
    }

    #[tokio::test]
    async fn invalid_batch_writes_no_input_and_fails() {
        let rec = Recorder::default();
        let stage = Arc::new(Mutex::new(FileStage::new()));
        let sd = Shutdown::new();
        let files = vec![file("ok.png", b"1"), file("../evil", b"2")];
        let status = handle_files(
            &stage,
            &writer(&rec),
            files,
            far_future(),
            PASTE_GAP,
            &sd,
            FileDiagnostics::Legacy,
        )
        .await;
        assert_eq!(status, TerminalFileStatus::Failed);
        assert!(rec.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn expired_frame_writes_nothing() {
        let rec = Recorder::default();
        let stage = Arc::new(Mutex::new(FileStage::new()));
        let sd = Shutdown::new();
        let status = handle_files(
            &stage,
            &writer(&rec),
            vec![file("a.png", b"1")],
            deadline_after(Duration::ZERO),
            PASTE_GAP,
            &sd,
            FileDiagnostics::Legacy,
        )
        .await;
        assert_eq!(status, TerminalFileStatus::Failed);
        assert!(rec.writes.lock().unwrap().is_empty());
        assert!(stage.lock().unwrap().root.is_none());
    }

    #[tokio::test]
    async fn expiry_mid_delivery_stops_remaining_pastes() {
        let rec = Recorder::default();
        let stage = Arc::new(Mutex::new(FileStage::new()));
        let sd = Shutdown::new();
        let files = vec![
            file("a.png", b"1"),
            file("b.png", b"2"),
            file("c.png", b"3"),
        ];
        let status = handle_files(
            &stage,
            &writer(&rec),
            files,
            deadline_after(Duration::from_millis(150)),
            Duration::from_millis(200),
            &sd,
            FileDiagnostics::Legacy,
        )
        .await;
        assert_eq!(status, TerminalFileStatus::Failed);
        assert_eq!(
            rec.writes.lock().unwrap().len(),
            1,
            "no retry or late pastes"
        );
    }

    #[tokio::test]
    async fn writer_held_past_expiry_gets_no_paste() {
        let rec = Recorder::default();
        let w = writer(&rec);
        let stage = Arc::new(Mutex::new(FileStage::new()));
        let sd = Shutdown::new();
        let holder = Arc::clone(&w);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let hold = std::thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            locked_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
        });
        locked_rx.recv().unwrap();
        let status = handle_files(
            &stage,
            &w,
            vec![file("a.png", b"1")],
            deadline_after(Duration::from_millis(150)),
            PASTE_GAP,
            &sd,
            FileDiagnostics::Legacy,
        )
        .await;
        hold.join().unwrap();
        assert_eq!(status, TerminalFileStatus::Failed);
        assert!(
            rec.writes.lock().unwrap().is_empty(),
            "expired paste must not be written once the lock frees"
        );
    }

    #[tokio::test]
    async fn writer_failure_acks_failed_and_stops() {
        let rec = Recorder {
            fail_after: Some(1),
            ..Recorder::default()
        };
        let stage = Arc::new(Mutex::new(FileStage::new()));
        let sd = Shutdown::new();
        let files = vec![
            file("a.png", b"1"),
            file("b.png", b"2"),
            file("c.png", b"3"),
        ];
        let status = handle_files(
            &stage,
            &writer(&rec),
            files,
            far_future(),
            Duration::from_millis(1),
            &sd,
            FileDiagnostics::Legacy,
        )
        .await;
        assert_eq!(status, TerminalFileStatus::Failed);
        assert_eq!(rec.writes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn writer_lock_is_free_during_the_gap() {
        let rec = Recorder::default();
        let w = writer(&rec);
        let stage = Arc::new(Mutex::new(FileStage::new()));
        let sd = Shutdown::new();
        let files = vec![file("a.png", b"1"), file("b.png", b"2")];
        let w2 = Arc::clone(&w);
        let fut = handle_files(
            &stage,
            &w,
            files,
            far_future(),
            Duration::from_millis(300),
            &sd,
            FileDiagnostics::Legacy,
        );
        let probe = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            w2.try_lock().is_ok()
        };
        let (status, free) = tokio::join!(fut, probe);
        assert_eq!(status, TerminalFileStatus::Sent);
        assert!(free, "writer mutex not held across the sleep");
    }
    async fn wait_for(cond: impl Fn() -> bool) {
        for _ in 0..100 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("condition not met in time");
    }
    #[test]
    fn file_count_cap_rejects_zero_byte_floods_and_keeps_earlier_files() {
        let mut stage = FileStage::with_caps(MAX_SESSION_BYTES, 3);
        let first = stage.stage(&[file("a", b""), file("b", b"")]).unwrap();
        assert!(stage.stage(&[file("c", b""), file("d", b"")]).is_err());
        assert!(first.iter().all(|p| p.exists()), "pending attachments kept");
        let third = stage.stage(&[file("e", b"")]).unwrap();
        assert!(third[0].exists());
        assert!(stage.stage(&[file("f", b"")]).is_err());
    }

    /// Emits the production paste sequences for `scripts/probe-terminal-files.py`:
    /// `PROBE_OUT=<dir> cargo test -p conversations-cli emit_probe -- --ignored`.
    #[test]
    #[ignore]
    fn emit_probe_sequences() {
        let out = PathBuf::from(std::env::var("PROBE_OUT").expect("PROBE_OUT"));
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        // Distinct trailing bytes keep each image unique (OpenCode de-dupes
        // identical image payloads) while the header stays a valid PNG.
        let engine = base64::engine::general_purpose::STANDARD;
        let png = |i: usize, name: &str| {
            let mut bytes = engine.decode(PNG).unwrap();
            bytes.extend_from_slice(format!("probe-{i}").as_bytes());
            file(name, &bytes)
        };
        let files = vec![
            png(0, "a b.png"),
            png(1, "a b.png"),
            png(2, "it's \"q\" (1) $x `y` & ;.png"),
            png(3, "日本語 é.png"),
            png(4, "plain.png"),
            // Edge group (Codex only): names ending in whitespace that the
            // native parsers would trim from an unquoted paste.
            png(5, "trailing.png "),
            png(6, "wide\u{3000}.png\u{3000}"),
            png(7, "q \"$x\" `y` it's.png "),
        ];
        let mut stage = FileStage::new();
        let paths = stage.stage(&files).unwrap();
        let hex = |p: &PathBuf| -> String {
            paste_bytes(p)
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let pastes: Vec<String> = paths[..5].iter().map(hex).collect();
        let edge_pastes: Vec<String> = paths[5..].iter().map(hex).collect();
        let root = stage.root.take().unwrap().keep();
        // Ownership marker: only this emitter writes it, so the probe never
        // deletes a real session's staging dir that merely looks similar.
        let token = uuid::Uuid::new_v4().to_string();
        std::fs::write(root.join(".probe-owner"), &token).unwrap();
        let json = serde_json::json!({
            "root": root,
            "token": token,
            "gap_ms": PASTE_GAP.as_millis() as u64,
            "pastes": pastes,
            "edge_pastes": edge_pastes,
            "names": files.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        });
        std::fs::write(out.join("sequences.json"), json.to_string()).unwrap();
    }
}
