//! Local GitHub adapter: the only code that runs `gh` for a remote request.
//!
//! Every entry point takes an already verified, typed target (owner, repo, number
//! and, for a comment, the exact body and thread). Arguments are passed as an
//! argv array, never through a shell, and every value is validated again here so
//! a caller cannot reintroduce an option-looking repo or a path. The GitHub
//! credentials are the ones `gh` already has on this machine; nothing is sent to
//! the server to authenticate.
//!
//! A signed target lets the host READ a pull request; it never decides whether
//! the result is published. PR status and comments leave this machine only when
//! local intent allows it: for a session the local user explicitly shared, only
//! a pull request that a message the server holds links (by full URL, checked
//! against the local publisher's record) is pushed, under the exact central
//! publication snapshot read before `gh` runs; a private host session is not
//! pushed at all; and a session with no canonical local record keeps the
//! legacy revisionless push while the central copy is unmanaged. A withheld cache is reported to the caller with
//! its reason ([`Withheld`]); it is never reported as a completed sync. The
//! central snapshot is a read-side cache fence: it never chooses a `gh` target
//! and is never refreshed after `gh` ran.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, ensure, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use super::authz::envelope::{valid_gh_owner, valid_gh_repo, MAX_PR_BODY_BYTES};
use crate::client::{ApiClient, CentralPrs, PrCacheSource};
use crate::commands::gh;

pub(super) fn validate(owner: &str, repo: &str, number: i64) -> Result<()> {
    ensure!(
        valid_gh_owner(owner) && valid_gh_repo(repo) && (1..=i64::from(i32::MAX)).contains(&number),
        "invalid pull request target"
    );
    Ok(())
}

/// The `gh` arguments that post `body` on `owner/repo#number`: a top-level
/// comment, or a reply in the review thread whose root comment is `in_reply_to`.
/// The body is a single argv element in a `--flag=value` / `field=value` form, so
/// it can never be read as an option or a file reference.
pub fn comment_argv(
    owner: &str,
    repo: &str,
    number: i64,
    body: &str,
    in_reply_to: Option<i64>,
) -> Result<Vec<String>> {
    validate(owner, repo, number)?;
    ensure!(
        !body.trim().is_empty() && body.len() <= MAX_PR_BODY_BYTES && !body.contains('\0'),
        "invalid comment body"
    );
    Ok(match in_reply_to {
        Some(id) => {
            ensure!(id >= 1, "invalid thread");
            vec![
                "api".into(),
                "--method".into(),
                "POST".into(),
                format!("repos/{owner}/{repo}/pulls/{number}/comments/{id}/replies"),
                "-f".into(),
                format!("body={body}"),
            ]
        }
        None => vec![
            "pr".into(),
            "comment".into(),
            number.to_string(),
            format!("--repo={owner}/{repo}"),
            format!("--body={body}"),
        ],
    })
}

/// Run `gh` with `argv`. No shell, no stdin; failure text stays in the host log.
fn run_gh(argv: &[String]) -> Result<()> {
    let output = Command::new("gh")
        .args(argv)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!("`gh` not found on PATH")
            } else {
                anyhow::anyhow!("failed to invoke `gh`: {error}")
            }
        })?;
    if !output.status.success() {
        bail!(
            "`gh {}` failed: {}",
            argv.first().map_or("", String::as_str),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn post_comment(
    owner: &str,
    repo: &str,
    number: i64,
    body: &str,
    in_reply_to: Option<i64>,
) -> Result<()> {
    run_gh(&comment_argv(owner, repo, number, body, in_reply_to)?)
}

/// A separate lock inode per runtime and target, shared by daemon, web and CLI.
/// Closing the descriptor releases the flock, including during unwinding.
pub(super) struct PrSyncLock(#[allow(dead_code)] std::fs::File);
impl PrSyncLock {
    pub(super) fn acquire(
        dir: &Path,
        session: &str,
        owner: &str,
        repo: &str,
        number: i64,
        wait: bool,
    ) -> Result<Option<Self>> {
        use std::os::unix::{
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawFd,
        };
        validate(owner, repo, number)?;
        let uid = unsafe { libc::geteuid() };
        let metadata = std::fs::symlink_metadata(dir)?;
        ensure!(
            metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
            "PR runtime must be an owner-only directory"
        );
        let key = serde_json::json!([session, owner, repo, number]);
        let hash = super::pr_metadata::pr_fingerprint_hash(&key);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir.join(format!("pr-sync-{hash}.lock")))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == uid
                && metadata.mode() & 0o077 == 0
                && metadata.nlink() == 1,
            "PR lock must be an owner-only regular file"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // SAFETY: the descriptor is owned here and remains open in the guard.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(Self(file)));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error.into());
            }
            if !wait {
                return Ok(None);
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "PR refresh is busy; try again shortly"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

fn manual_lock(
    dir: &Path,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<PrSyncLock> {
    PrSyncLock::acquire(dir, session, owner, repo, number, true)?
        .ok_or_else(|| anyhow::anyhow!("PR refresh is busy"))
}

fn target_authorized(
    dir: &Path,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<bool> {
    Ok(super::authz::pr_poll_candidates(dir, session, true)?
        .iter()
        .any(|(o, r, n, _)| o == owner && r == repo && *n == number))
}

fn finish_manual(dir: &Path, session: &str, owner: &str, repo: &str, number: i64) -> Result<()> {
    let fingerprint = super::pr_metadata::local_fingerprint(dir, session, owner, repo, number)?;
    super::authz::apply_pr_poll_outcome(
        dir,
        session,
        owner,
        repo,
        number,
        fingerprint.as_deref(),
        true,
    )
}

/// What local intent allows for a session's PR caches. Only local state is
/// read; nothing the server says can widen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    /// Explicitly shared by the local user, at this local share revision, to
    /// the server this client talks to.
    Shared { rev: u64 },
    /// A host session that is not (or no longer) shared: nothing leaves.
    Private,
    /// No canonical local record: an unmanaged legacy central session.
    Legacy,
}

/// The local publisher's row for `session`: `(phase, rev, server)`. Read-only;
/// absent until the publisher has ever handled the session.
fn local_publication(dir: &Path, session: &str) -> Result<Option<(String, u64, String)>> {
    let path = dir.join("history.db");
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'publications')",
        [],
        |r| r.get(0),
    )?;
    if !present {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT phase, rev, server FROM publications WHERE session_id = ?1",
            params![session],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)?, r.get(2)?)),
        )
        .optional()?
        .map(|(phase, rev, server)| (phase, u64::try_from(rev).unwrap_or(0), server)))
}

fn intent(dir: &Path, client: &ApiClient, session: &str) -> Result<Intent> {
    if let Some((phase, rev, server)) = local_publication(dir, session)? {
        let bound = server.trim_end_matches('/') == client.base();
        return Ok(if phase == "publishing" && bound {
            Intent::Shared { rev }
        } else {
            Intent::Private
        });
    }
    Ok(if super::authz::lookup(dir, session)?.is_some() {
        Intent::Private
    } else {
        Intent::Legacy
    })
}

/// Whether `text` links `owner/repo#number` by a full GitHub PR URL. Only the
/// exact URL form counts; a bare `PR #N` (which the server may resolve against
/// the session's repository) is not proof here, so such a link fails closed.
fn links_pr(text: &str, owner: &str, repo: &str, number: i64) -> bool {
    ["https://github.com/", "http://github.com/"]
        .iter()
        .any(|marker| {
            text.match_indices(marker).any(|(at, _)| {
                let mut parts = text[at + marker.len()..].splitn(4, '/');
                let (Some(o), Some(r), Some("pull"), Some(rest)) =
                    (parts.next(), parts.next(), parts.next(), parts.next())
                else {
                    return false;
                };
                let digits = rest
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(rest.len());
                o.eq_ignore_ascii_case(owner)
                    && r.eq_ignore_ascii_case(repo)
                    && rest[..digits].parse() == Ok(number)
            })
        })
}

/// Whether the local publisher recorded that the server holds, at its current
/// local version, a message of `session` that links this pull request. Local
/// state only: a central PR list is never taken as proof of publication.
fn published_locally(
    dir: &Path,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<bool> {
    let path = dir.join("history.db");
    if !path.exists() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'
           AND name IN ('messages', 'publication_sent')",
        [],
        |r| r.get(0),
    )?;
    if present < 2 {
        return Ok(false);
    }
    let mut statement = conn.prepare(
        "SELECT m.message_json FROM messages m JOIN publication_sent p
           ON p.session_id = m.session_id AND p.uuid = m.uuid AND p.sha256 = m.sha256
         WHERE m.session_id = ?1",
    )?;
    let mut rows = statement.query(params![session])?;
    while let Some(row) = rows.next()? {
        if links_pr(&row.get::<_, String>(0)?, owner, repo, number) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Why a PR cache stayed on this host. Each reason is reported to the caller
/// (signed answer, CLI) instead of claiming that a sync completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withheld {
    /// A private host session: there is no PR cache to fetch or upload.
    Private,
    /// Shared locally, but the central copy is not at this host's share
    /// revision (or reports no publication at all).
    CentralNotAtShare,
    /// No local record, and the central session is managed by a publisher.
    CentralManaged,
    /// The local user unshared the session while `gh` was reading.
    UnsharedDuringRead,
    /// Shared, but no message the server holds links this pull request, so
    /// sharing the session did not publish it.
    NotPublished,
}

impl Withheld {
    /// The explanation for a signed `pr.sync` answer (at most 160 characters,
    /// nothing host-local). Every variant says the target stays recorded.
    pub fn sync_detail(self) -> &'static str {
        match self {
            Withheld::Private => {
                "PR recorded on the host; this session is private, so its PR cache is unavailable: \
                 not fetched or uploaded"
            }
            Withheld::CentralNotAtShare => {
                "PR recorded; the central copy is not at this host's share revision, so the PR \
                 cache was not fetched or uploaded"
            }
            Withheld::CentralManaged => {
                "PR recorded; the central session is managed but this host does not share it, so \
                 nothing was fetched or uploaded"
            }
            Withheld::UnsharedDuringRead => {
                "PR recorded; the session was unshared while the PR was read, so nothing was \
                 uploaded"
            }
            Withheld::NotPublished => {
                "PR recorded; it is not linked in this session's published history, so its PR \
                 cache was not fetched or uploaded"
            }
        }
    }

    /// The explanation after a signed `pr.comment` that WAS posted.
    pub fn comment_detail(self) -> &'static str {
        match self {
            Withheld::Private => {
                "comment posted; this session is private, so its PR cache was not refreshed or \
                 uploaded"
            }
            Withheld::NotPublished => {
                "comment posted; the PR is not linked in this session's published history, so \
                 its cache was not refreshed or uploaded"
            }
            _ => "comment posted; the PR cache was not refreshed or uploaded for this session",
        }
    }
}

/// What happened to one PR cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrCache {
    Pushed,
    CachedLocally,
    Withheld(Withheld),
}

/// The source this pull request's caches may be pushed under, decided BEFORE
/// `gh` runs. `Err(reason)`: nothing may leave this machine, so `gh` is not run.
fn cache_source(
    dir: &Path,
    client: &ApiClient,
    session: &str,
    (owner, repo, number): (&str, &str, i64),
) -> Result<std::result::Result<PrCacheSource, Withheld>> {
    Ok(match intent(dir, client, session)? {
        Intent::Private => Err(Withheld::Private),
        Intent::Shared { rev } => {
            if !published_locally(dir, session, owner, repo, number)? {
                eprintln!(
                    "PR cache for {session} {owner}/{repo}#{number} not published: no message \
                     the server holds links it"
                );
                return Ok(Err(Withheld::NotPublished));
            }
            match client.central_prs(session)? {
                CentralPrs {
                    publication: Some(snapshot),
                    ..
                } if snapshot.rev != rev => {
                    eprintln!(
                        "PR cache for {session} not published: shared locally at revision \
                         {rev}, central copy at {snapshot:?}"
                    );
                    Err(Withheld::CentralNotAtShare)
                }
                central @ CentralPrs {
                    publication: Some(snapshot),
                    ..
                } => {
                    // The central list can only narrow: a PR it does not link
                    // would be refused, so it is not read for publication.
                    if central.lists(owner, repo, number) {
                        Ok(PrCacheSource::Published(snapshot))
                    } else {
                        eprintln!(
                            "PR cache for {session} {owner}/{repo}#{number} not published: the \
                             central copy does not link it"
                        );
                        Err(Withheld::NotPublished)
                    }
                }
                CentralPrs {
                    publication: None, ..
                } => {
                    eprintln!(
                        "PR cache for {session} not published: shared locally at revision \
                         {rev}, central copy unmanaged"
                    );
                    Err(Withheld::CentralNotAtShare)
                }
            }
        }
        Intent::Legacy => match client.central_prs(session)?.publication {
            None => Ok(PrCacheSource::Unmanaged),
            Some(_) => {
                eprintln!(
                    "PR cache for {session} not published: the central session is managed \
                     but this host does not share it"
                );
                Err(Withheld::CentralManaged)
            }
        },
    })
}

/// Right before a push: local intent still allows exactly `source` for this
/// pull request. The snapshot itself is never refreshed; a newer central state
/// is the server's refusal to make. This narrows, but does not close, the
/// window in which a local unshare races a push already being sent (see the
/// checkpoint).
fn still_allowed(
    dir: &Path,
    client: &ApiClient,
    session: &str,
    (owner, repo, number): (&str, &str, i64),
    source: PrCacheSource,
) -> Result<bool> {
    Ok(match (intent(dir, client, session)?, source) {
        (Intent::Shared { rev }, PrCacheSource::Published(snapshot)) => {
            rev == snapshot.rev && published_locally(dir, session, owner, repo, number)?
        }
        (Intent::Legacy, PrCacheSource::Unmanaged) => true,
        _ => false,
    })
}

/// Re-read one pull request (status and comments) with the local `gh` and push
/// both snapshots under `session`, if local intent allows publishing it. A
/// failed status read or a refused push (e.g. a stale snapshot) is an error and
/// is not retried; a comment read degrades to whatever could be gathered, as it
/// always has. Thin wrapper over the cached variant so pre-existing tests keep
/// their shape; production callers go through [`sync_recorded`] /
/// [`sync_pr_explicit`] which choose caching per intent.
#[cfg(test)]
pub fn sync_pr(
    dir: &Path,
    client: &ApiClient,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<PrCache> {
    sync_pr_cached(dir, client, session, owner, repo, number, false)
}
#[allow(clippy::too_many_arguments)]
fn sync_pr_cached(
    dir: &Path,
    client: &ApiClient,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
    cache: bool,
) -> Result<PrCache> {
    validate(owner, repo, number)?;
    let target = (owner, repo, number);
    let source = match cache_source(dir, client, session, target)? {
        Ok(source) => source,
        Err(reason) => return Ok(PrCache::Withheld(reason)),
    };
    let status = gh::fetch_pr_status_for(owner, repo, number)?;
    if !still_allowed(dir, client, session, target, source)?
        || (cache && !target_authorized(dir, session, owner, repo, number)?)
    {
        return Ok(PrCache::Withheld(Withheld::UnsharedDuringRead));
    }
    client.push_pr_status(session, source, owner, repo, number, &status)?;
    let comments = gh::fetch_pr_comments(owner, repo, number)?;
    if !still_allowed(dir, client, session, target, source)?
        || (cache && !target_authorized(dir, session, owner, repo, number)?)
    {
        return Ok(PrCache::Withheld(Withheld::UnsharedDuringRead));
    }
    if cache {
        super::pr_metadata::save(
            dir,
            session,
            owner,
            repo,
            number,
            Some(&status),
            &comments,
            false,
        )?;
    }
    client.push_pr_comments(session, source, owner, repo, number, &comments)?;
    Ok(PrCache::Pushed)
}

/// Explicit user-requested refresh, separate from background polling.
/// Acquires the per-target lock before the gh-touching path so a concurrent
/// daemon background pass cannot call `gh` for the same target at the same time.
pub fn sync_pr_explicit(
    dir: &Path,
    client: &ApiClient,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<PrCache> {
    validate(owner, repo, number)?;
    if intent(dir, client, session)? != Intent::Private {
        // Non-private: acquire the lock before sync_pr_cached (which calls gh).
        // Lock is released on return; refresh_local acquires its own lock for private.
        let _lock = manual_lock(dir, session, owner, repo, number)?;
        ensure!(
            target_authorized(dir, session, owner, repo, number)?,
            "PR refresh authority was revoked"
        );
        let result = sync_pr_cached(dir, client, session, owner, repo, number, true)?;
        if result == PrCache::Pushed {
            finish_manual(dir, session, owner, repo, number)?;
        }
        return Ok(result);
    }
    // Private: refresh_local acquires the per-target lock internally.
    refresh_local(dir, session, owner, repo, number, false)
}
/// Actual local owner, not a manufactured signed authority. Never uploads.
/// Holds the per-target lock around the `gh` calls so a simultaneous
/// background poll pass cannot call `gh` for the same target at once.
pub fn refresh_local(
    dir: &Path,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
    owner_recorded: bool,
) -> Result<PrCache> {
    validate(owner, repo, number)?;
    let _lock = manual_lock(dir, session, owner, repo, number)?;
    if owner_recorded {
        super::authz::record_local_pr_target(dir, session, owner, repo, number)?;
    }
    ensure!(
        target_authorized(dir, session, owner, repo, number)?,
        "PR refresh authority was revoked"
    );
    let (status, comments, _) = local_fetch_pr(owner, repo, number)?;
    ensure!(
        target_authorized(dir, session, owner, repo, number)?,
        "PR refresh authority was revoked"
    );
    super::pr_metadata::save(
        dir,
        session,
        owner,
        repo,
        number,
        Some(&status),
        &comments,
        owner_recorded,
    )?;
    finish_manual(dir, session, owner, repo, number)?;
    Ok(PrCache::CachedLocally)
}

pub fn resync_comments_explicit(
    dir: &Path,
    client: &ApiClient,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<PrCache> {
    if intent(dir, client, session)? == Intent::Private {
        refresh_local(dir, session, owner, repo, number, false)
    } else {
        resync_comments(dir, client, session, owner, repo, number)
    }
}

/// Only the comments side, after a comment was posted.
/// Acquires the per-target lock before the `gh` call to prevent a concurrent
/// background poll from calling `gh` for the same target simultaneously.
pub fn resync_comments(
    dir: &Path,
    client: &ApiClient,
    session: &str,
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<PrCache> {
    validate(owner, repo, number)?;
    let target = (owner, repo, number);
    let source = match cache_source(dir, client, session, target)? {
        Ok(source) => source,
        Err(reason) => return Ok(PrCache::Withheld(reason)),
    };
    let _lock = manual_lock(dir, session, owner, repo, number)?;
    let comments = gh::fetch_pr_comments(owner, repo, number)?;
    if !still_allowed(dir, client, session, target, source)? {
        return Ok(PrCache::Withheld(Withheld::UnsharedDuringRead));
    }
    super::pr_metadata::save(dir, session, owner, repo, number, None, &comments, false)?;
    client.push_pr_comments(session, source, owner, repo, number, &comments)?;
    Ok(PrCache::Pushed)
}

/// The result of re-reading the recorded pull requests of one session.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recorded {
    /// Active targets attempted by this pass, excluding unlinked or withdrawn work.
    pub targets: usize,
    /// Pushed under an admissible source.
    pub pushed: usize,
    /// Refreshed into this host's own `pr-cache.db` without a central push
    /// (private session's intent, by design — see [`sync_recorded`]).
    pub cached_locally: usize,
    /// Kept on this host, with the reason (one entry per target).
    pub withheld: Vec<Withheld>,
    /// Failed (gh read, transport, or a refused push such as a stale snapshot).
    pub failed: usize,
}

/// Re-read recorded pull requests of `session`. Targets come ONLY from this
/// host's own table. The background poll (`explicit=false`) only reads
/// targets whose backoff schedule says they are due; an explicit user
/// command (`explicit=true`, e.g. `conversations pr sync`) reads
/// every recorded target now — the manual bypass that also resets each
/// polled target's schedule to the floor via [`apply_pr_poll_outcome`]
/// even when metadata is unchanged.
///
/// A private session reads into the host's local pr-cache; it NEVER uploads
/// a PR cache to the central server (the shared/legacy branch below is the
/// only one that calls [`sync_pr_cached`]). Each successful read updates
/// the per-target backoff schedule: unchanged metadata doubles the delay up
/// to the ceiling, a change resets it to the floor; a transient failure
/// doubles without touching the last-seen fingerprint, so the next good
/// read can still detect "actually changed since last success".
pub fn sync_recorded(dir: &Path, client: &ApiClient, session: &str) -> Result<Recorded> {
    sync_recorded_inner(dir, client, session, false)
}

/// Explicit user-initiated variant: every recorded target of `session` is
/// polled now, regardless of its backoff schedule. Used by the CLI's
/// `pr sync` command so the user does not sit behind the background delay
/// when they explicitly asked for fresh data.
pub fn sync_recorded_explicit(dir: &Path, client: &ApiClient, session: &str) -> Result<Recorded> {
    sync_recorded_inner(dir, client, session, true)
}

fn sync_recorded_inner(
    dir: &Path,
    client: &ApiClient,
    session: &str,
    explicit: bool,
) -> Result<Recorded> {
    // Only locally ingested full URLs can enlarge the automatic target set.
    if !explicit
        && super::authz::store::Store::open(dir)?
            .session(session)?
            .is_some()
    {
        let history = super::history::History::open(dir)?;
        for url in history.prs(session)? {
            let Some((owner, repo, number)) =
                conversations_core::terminal_prs::parse_canonical(&url)
            else {
                continue;
            };
            if super::pr_metadata::is_unlinked(dir, session, &owner, &repo, number)? {
                continue;
            }
            // Preserve existing rows, including revoked signed provenance.
            if let Err(error) = super::authz::store::Store::open(dir)?.discover_pr_target(
                session,
                &owner,
                &repo,
                number,
                super::authz::store::now_secs(),
            ) {
                eprintln!("PR discovery {owner}/{repo}#{number}: {error:#}");
            }
        }
    }
    let targets = super::authz::pr_poll_candidates(dir, session, explicit)?;
    let mut recorded = Recorded::default();
    for (owner, repo, number, _) in targets {
        let Some(_lock) = PrSyncLock::acquire(dir, session, &owner, &repo, number, explicit)?
        else {
            continue;
        };
        // A manual refresh may have completed since this pass listed targets.
        let current = super::authz::pr_poll_candidates(dir, session, explicit)?;
        let Some((_, _, _, last_fingerprint)) = current
            .into_iter()
            .find(|(o, r, n, _)| o == &owner && r == &repo && *n == number)
        else {
            continue;
        };
        if super::pr_metadata::is_unlinked(dir, session, &owner, &repo, number)? {
            continue;
        }
        recorded.targets += 1;
        let session_intent = intent(dir, client, session)?;
        let result: Result<Option<(PrCache, Option<String>)>> = {
            if session_intent == Intent::Private {
                // Local-only refresh: gh → pr-cache.db; nothing leaves.
                match local_fetch_pr(&owner, &repo, number) {
                    Ok((status, comments, fingerprint)) => {
                        // Recheck authority after the network read.
                        // If the grant was revoked or the PR was unlinked while
                        // `gh` ran, abandon the write to prevent resurrection.
                        let revoked_or_unlinked =
                            super::pr_metadata::is_unlinked(dir, session, &owner, &repo, number)?
                                || !super::authz::pr_poll_candidates(dir, session, true)?
                                    .iter()
                                    .any(|(o, r, n, _)| o == &owner && r == &repo && *n == number);
                        if revoked_or_unlinked {
                            Ok(None)
                        } else {
                            super::pr_metadata::save(
                                dir,
                                session,
                                &owner,
                                &repo,
                                number,
                                Some(&status),
                                &comments,
                                false,
                            )?;
                            Ok(Some((PrCache::CachedLocally, Some(fingerprint))))
                        }
                    }
                    Err(error) => Err(error),
                }
            } else {
                // Shared or legacy: push under the admissible source and also
                // cache locally so the host's own pr-cache.db stays current.
                match sync_pr_cached(dir, client, session, &owner, &repo, number, true) {
                    Ok(cache) => {
                        let fingerprint = super::pr_metadata::local_fingerprint(
                            dir, session, &owner, &repo, number,
                        )?;
                        Ok(Some((cache, fingerprint)))
                    }
                    Err(error) => Err(error),
                }
            }
        };

        match result {
            // Authority or unlink state changed during the read: no cache write,
            // failure, private-sharing explanation, or scheduling mutation.
            Ok(None) => recorded.targets -= 1,
            Ok(Some((PrCache::Pushed, fingerprint))) => {
                recorded.pushed += 1;
                // Manual refresh always resets the next poll to the floor.
                let changed = fingerprint.as_deref() != last_fingerprint.as_deref() || explicit;
                super::authz::apply_pr_poll_outcome(
                    dir,
                    session,
                    &owner,
                    &repo,
                    number,
                    fingerprint.as_deref(),
                    changed,
                )?;
            }
            Ok(Some((PrCache::CachedLocally, fingerprint))) => {
                recorded.cached_locally += 1;
                let changed = fingerprint.as_deref() != last_fingerprint.as_deref() || explicit;
                super::authz::apply_pr_poll_outcome(
                    dir,
                    session,
                    &owner,
                    &repo,
                    number,
                    fingerprint.as_deref(),
                    changed,
                )?;
            }
            Ok(Some((PrCache::Withheld(reason), _))) => {
                recorded.withheld.push(reason);
                // Treat a central-side "not published yet" / "not at share
                // revision" withhold as a quiet reason to back off locally
                // too: there's nothing we can do about it and polling at
                // the floor just burns `gh` rate limit. The last-seen
                // fingerprint is preserved (pass None).
                super::authz::apply_pr_poll_outcome(
                    dir, session, &owner, &repo, number, None, false,
                )?;
            }
            Err(error) => {
                eprintln!("PR sync {owner}/{repo}#{number} failed: {error:#}");
                recorded.failed += 1;
                super::authz::apply_pr_poll_outcome(
                    dir, session, &owner, &repo, number, None, false,
                )?;
            }
        }
    }
    Ok(recorded)
}

/// Fetch PR status and comments from `gh` and compute the fingerprint.
/// Does NOT save to pr-cache; callers own the write (and perform any
/// authority recheck between fetch and save).
fn local_fetch_pr(
    owner: &str,
    repo: &str,
    number: i64,
) -> Result<(gh::PrStatus, Vec<gh::PrComment>, String)> {
    let status = gh::fetch_pr_status_for(owner, repo, number)?;
    let comments = gh::fetch_pr_comments(owner, repo, number)?;
    let fingerprint = fingerprint_of(&status, &comments);
    Ok((status, comments, fingerprint))
}

/// SHA-256 over the status fields and sorted comments, in the canonical
/// shape shared with [`super::pr_metadata::local_fingerprint`]. Both sides
/// call [`super::pr_metadata::pr_fingerprint_hash`] on the same JSON shape,
/// so a re-read of unchanged data produces an identical fingerprint.
fn fingerprint_of(status: &gh::PrStatus, comments: &[gh::PrComment]) -> String {
    super::pr_metadata::metadata_fingerprint(
        serde_json::json!({
            "state": status.state, "merged": status.merged, "draft": status.draft,
            "title": status.title, "review_decision": status.review_decision,
        }),
        serde_json::to_value(comments).expect("PR comments serialize"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_arguments_are_one_element_each_and_cannot_become_options() {
        let argv = comment_argv("octo", "repo.x", 7, "-rf --body=evil\n$(touch x)", None).unwrap();
        assert_eq!(argv[..3], ["pr", "comment", "7"]);
        assert_eq!(argv[3], "--repo=octo/repo.x");
        assert_eq!(argv[4], "--body=-rf --body=evil\n$(touch x)");
        assert_eq!(argv.len(), 5);
        let reply = comment_argv("octo", "r", 7, "hi", Some(99)).unwrap();
        assert_eq!(reply[3], "repos/octo/r/pulls/7/comments/99/replies");
        assert_eq!(reply[5], "body=hi");
    }

    #[test]
    fn hostile_targets_never_reach_gh() {
        for (owner, repo, number) in [
            ("--help", "r", 1),
            ("o", "--web", 1),
            ("o/x", "r", 1),
            ("o", "r/../x", 1),
            ("o", "..", 1),
            ("o", "r", 0),
            ("o", "r", -1),
            ("", "r", 1),
            ("o", "r r", 1),
            ("o-", "r", 1),
        ] {
            assert!(
                comment_argv(owner, repo, number, "x", None).is_err(),
                "{owner}/{repo}#{number}"
            );
        }
        assert!(comment_argv("o", "r", 1, "  ", None).is_err());
        assert!(comment_argv("o", "r", 1, "x", Some(0)).is_err());
    }
}
