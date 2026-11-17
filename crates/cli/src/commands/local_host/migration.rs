//! Native snapshot copies. The private channel owns admission and reply signing.
use super::{
    authz::{
        self,
        sessions::LocalSession,
        store::{now_secs, open_private, Store},
    },
    history::{History, Source},
    settings::Settings,
};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use conversations_core::{
    migration::{MigrationImport, MigrationManifest, MAX_SNAPSHOT_BYTES, MIGRATION_CHUNK_BYTES},
    private_channel::ReplyBody,
    AgentKind, AgentLauncher,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const TTL: i64 = 3600;
const MAX_TRANSFERS: i64 = 4;

pub(super) fn valid_transfer(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok()
}
pub(super) fn valid_manifest(m: &MigrationManifest) -> bool {
    m.agent != AgentKind::Zsh
        && !m.native_id.is_empty()
        && m.native_id.len() <= 128
        && m.native_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        && m.title.len() <= 1024
        && m.size > 0
        && m.size <= MAX_SNAPSHOT_BYTES as u64
        && m.sha256.len() == 43
        && B64.decode(&m.sha256).is_ok_and(|b| b.len() == 32)
}

fn digest(bytes: &[u8]) -> String {
    B64.encode(ring::digest::digest(&ring::digest::SHA256, bytes))
}

fn open(dir: &Path) -> Result<Connection> {
    let db = open_private(dir, "migration.db")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS exports(kid TEXT NOT NULL,id TEXT NOT NULL,session TEXT NOT NULL,manifest TEXT NOT NULL,data BLOB NOT NULL,created INTEGER NOT NULL,complete INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(kid,id));
        CREATE TABLE IF NOT EXISTS imports(kid TEXT NOT NULL,id TEXT NOT NULL,metadata TEXT NOT NULL,next INTEGER NOT NULL DEFAULT 0,done INTEGER NOT NULL DEFAULT 0,created INTEGER NOT NULL,PRIMARY KEY(kid,id));
        CREATE TABLE IF NOT EXISTS chunks(kid TEXT NOT NULL,id TEXT NOT NULL,offset INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(kid,id,offset));")?;
    db.execute("DELETE FROM exports WHERE created < ?1", [now_secs() - TTL])?;
    db.execute("DELETE FROM imports WHERE created < ?1", [now_secs() - TTL])?;
    db.execute("DELETE FROM chunks WHERE NOT EXISTS(SELECT 1 FROM imports i WHERE i.kid=chunks.kid AND i.id=chunks.id)", [])?;
    Ok(db)
}

struct NativeRoots {
    claude: Option<PathBuf>,
    codex: Option<PathBuf>,
}
impl NativeRoots {
    fn local() -> Self {
        Self {
            claude: conversations_core::claude_projects_dir(),
            codex: conversations_core::codex_sessions_dir(),
        }
    }
    fn root(&self, agent: AgentKind) -> Result<&Path> {
        match agent {
            AgentKind::Claude => self.claude.as_deref(),
            AgentKind::Codex => self.codex.as_deref(),
            _ => None,
        }
        .context("native JSONL storage unavailable")
    }
}

fn bounded_read(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    ensure!(
        size <= MAX_SNAPSHOT_BYTES as u64,
        "session exceeds the 64 MiB migration limit"
    );
    let mut bytes = Vec::new();
    file.take(size).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Snapshot only complete JSONL records. A source writer may append after this read.
fn snapshot_jsonl(roots: &NativeRoots, agent: AgentKind, native: &str) -> Result<Vec<u8>> {
    for file in conversations_core::discover_sessions_for_agent(roots.root(agent)?, agent) {
        if file.session_id != native && !file.session_id.ends_with(&format!("-{native}")) {
            continue;
        }
        let mut bytes = bounded_read(&file.path)?;
        let end = bytes
            .iter()
            .rposition(|b| *b == b'\n')
            .context("native transcript has no complete records")?
            + 1;
        bytes.truncate(end);
        validate_native(agent, native, &bytes)?;
        return Ok(bytes);
    }
    anyhow::bail!("native transcript is unavailable on the source host")
}

fn validate_native(
    agent: AgentKind,
    native: &str,
    data: &[u8],
) -> Result<conversations_core::Session> {
    if agent == AgentKind::OpenCode {
        let value: serde_json::Value = serde_json::from_slice(data)?;
        ensure!(
            value["info"]["id"].as_str() == Some(native),
            "native session identity mismatch"
        );
        return Ok(conversations_core::parse_opencode_export(&value)?);
    }
    ensure!(
        matches!(agent, AgentKind::Claude | AgentKind::Codex),
        "shell sessions cannot be migrated"
    );
    let text = std::str::from_utf8(data)?;
    ensure!(text.ends_with('\n'), "incomplete native transcript");
    let mut bound = false;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let value: serde_json::Value =
            serde_json::from_str(line).context("invalid native transcript record")?;
        let id = if agent == AgentKind::Codex {
            (value["type"] == "session_meta")
                .then(|| value["payload"]["id"].as_str())
                .flatten()
        } else {
            value["sessionId"].as_str()
        };
        if let Some(id) = id {
            ensure!(id == native, "native session identity mismatch");
            bound = true;
        }
    }
    ensure!(bound, "native transcript has no session identity");
    let parsed = conversations_core::parse_session_str(Some(native), text)?;
    ensure!(parsed.agent == agent, "native agent mismatch");
    Ok(parsed)
}

/// Native helpers get fixed arguments, no shell, bounded output and no visible transcript output.
fn opencode(dir: &Path, args: &[&str], cwd: Option<&Path>) -> Result<Vec<u8>> {
    let output = tempfile::NamedTempFile::new_in(dir)?;
    output
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let settings = Settings::load(dir)?;
    let env = super::daemon::LaunchEnv::from_settings(&settings.host_env).resolve_environment();
    let binary = super::opencode_bridge::executable(Path::new("opencode"), &env)
        .or_else(|| {
            dirs::home_dir().and_then(|home| {
                super::opencode_bridge::executable(&home.join(".opencode/bin/opencode"), &env)
            })
        })
        .context("OpenCode is not installed on this host")?;
    let mut command = Command::new(binary);
    command.envs(env);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(output.reopen()?)
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .context("OpenCode is not installed on this host")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "OpenCode session export/import failed");
            return bounded_read(output.path());
        }
        if Instant::now() >= deadline
            || output.as_file().metadata()?.len() > MAX_SNAPSHOT_BYTES as u64
        {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("OpenCode session operation exceeded its time or size limit");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn opencode_db(dir: &Path) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("OPENCODE_DB").filter(|p| !p.is_empty()) {
        return Ok(path.into());
    }
    for args in [&["debug", "paths", "db"][..], &["db", "path"][..]] {
        if let Ok(bytes) = opencode(dir, args, None) {
            let text = String::from_utf8_lossy(&bytes);
            let path = Path::new(text.trim());
            if path.is_absolute() && text.trim().lines().count() == 1 {
                return Ok(path.to_owned());
            }
        }
    }
    anyhow::bail!("OpenCode did not report its database path")
}

fn export_native(dir: &Path, roots: &NativeRoots, session: &LocalSession) -> Result<Vec<u8>> {
    let native = session
        .resume_id
        .as_deref()
        .context("agent has not reported its native session identity")?;
    if session.agent == AgentKind::OpenCode {
        // V2 uses `session export`; V1 uses `export`.
        let bytes = opencode(dir, &["session", "export", native, "--standalone"], None)
            .or_else(|_| opencode(dir, &["export", native], None))?;
        validate_native(session.agent, native, &bytes)?;
        Ok(bytes)
    } else {
        snapshot_jsonl(roots, session.agent, native)
    }
}

pub(super) fn export(
    dir: &Path,
    kid: &str,
    session: &str,
    transfer: &str,
    offset: u64,
) -> Result<ReplyBody> {
    export_at(dir, &NativeRoots::local(), kid, session, transfer, offset)
}

fn export_at(
    dir: &Path,
    roots: &NativeRoots,
    kid: &str,
    session: &str,
    transfer: &str,
    offset: u64,
) -> Result<ReplyBody> {
    ensure!(valid_transfer(transfer), "invalid transfer id");
    let mut db = open(dir)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT session,manifest FROM exports WHERE kid=?1 AND id=?2",
            params![kid, transfer],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let manifest: MigrationManifest = if let Some((bound, json)) = existing {
        ensure!(bound == session, "transfer belongs to another session");
        serde_json::from_str(&json)?
    } else {
        ensure!(offset == 0, "snapshot expired; restart migration");
        tx.execute("DELETE FROM exports WHERE complete=1", [])?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM exports", [], |r| r.get(0))?;
        ensure!(
            count < MAX_TRANSFERS,
            "too many migration snapshots; retry after they expire"
        );
        let local = Store::open(dir)?
            .session(session)?
            .context("unknown local session")?;
        let bytes = export_native(dir, roots, &local)?;
        let title = History::open(dir)?
            .summary(session)?
            .title
            .unwrap_or_else(|| "Migrated session".into());
        let manifest = MigrationManifest {
            agent: local.agent,
            native_id: local.resume_id.context("missing native identity")?,
            title: title.chars().take(240).collect(),
            size: bytes.len() as u64,
            sha256: digest(&bytes),
        };
        ensure!(valid_manifest(&manifest), "unsupported native snapshot");
        tx.execute(
            "INSERT INTO exports(kid,id,session,manifest,data,created) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                kid,
                transfer,
                session,
                serde_json::to_string(&manifest)?,
                bytes,
                now_secs()
            ],
        )?;
        manifest
    };
    ensure!(offset < manifest.size, "invalid snapshot offset");
    let bytes: Vec<u8> = tx.query_row(
        "SELECT substr(data,?3,?4) FROM exports WHERE kid=?1 AND id=?2",
        params![kid, transfer, offset + 1, MIGRATION_CHUNK_BYTES],
        |r| r.get(0),
    )?;
    let end = offset + bytes.len() as u64;
    if end == manifest.size {
        tx.execute(
            "UPDATE exports SET complete=1 WHERE kid=?1 AND id=?2",
            params![kid, transfer],
        )?;
    }
    tx.commit()?;
    let next = (end < manifest.size).then_some(end);
    Ok(ReplyBody::Migration {
        manifest,
        b64: B64.encode(bytes),
        next,
    })
}

/// Rewrite only native working-directory metadata. Historical messages and tool calls stay intact.
fn relocated(agent: AgentKind, data: &[u8], cwd: &Path) -> Result<Vec<u8>> {
    let directory = cwd.to_string_lossy();
    if agent == AgentKind::OpenCode {
        let mut value: serde_json::Value = serde_json::from_slice(data)?;
        value["info"]["directory"] = directory.as_ref().into();
        // The destination CLI resolves its own project. Never reuse a source snapshot/worktree path.
        if let Some(info) = value["info"].as_object_mut() {
            info.remove("snapshot");
            info.remove("parentID");
        }
        return Ok(serde_json::to_vec(&value)?);
    }
    let mut output = Vec::new();
    for line in std::str::from_utf8(data)?
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let mut value: serde_json::Value = serde_json::from_str(line)?;
        if agent == AgentKind::Claude {
            if value.get("cwd").is_some() {
                value["cwd"] = directory.as_ref().into();
            }
        } else if value["type"] == "session_meta" || value["type"] == "turn_context" {
            value["payload"]["cwd"] = directory.as_ref().into();
        }
        serde_json::to_writer(&mut output, &value)?;
        output.push(b'\n');
    }
    Ok(output)
}

fn install_jsonl(
    roots: &NativeRoots,
    manifest: &MigrationManifest,
    bytes: &[u8],
    cwd: &Path,
) -> Result<()> {
    let root = roots.root(manifest.agent)?;
    // Detect the same native identity anywhere, including a different original project folder.
    for file in conversations_core::discover_sessions_for_agent(root, manifest.agent) {
        if file.session_id == manifest.native_id
            || file
                .session_id
                .ends_with(&format!("-{}", manifest.native_id))
        {
            ensure!(
                bounded_read(&file.path)? == bytes,
                "destination has a different copy of this native session"
            );
            return Ok(());
        }
    }
    let (folder, name) = if manifest.agent == AgentKind::Claude {
        let encoded: String = cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        (root.join(encoded), format!("{}.jsonl", manifest.native_id))
    } else {
        let now = chrono::Utc::now();
        (
            root.join(now.format("%Y/%m/%d").to_string()),
            format!(
                "rollout-{}-{}.jsonl",
                now.format("%Y-%m-%dT%H-%M-%S"),
                manifest.native_id
            ),
        )
    };
    std::fs::create_dir_all(&folder)?;
    let mut file = tempfile::NamedTempFile::new_in(&folder)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(folder.join(name))
        .map_err(|e| e.error)?;
    Ok(())
}

fn install(dir: &Path, roots: &NativeRoots, input: &MigrationImport, bytes: &[u8]) -> Result<()> {
    let m = &input.manifest;
    let cwd = Path::new(&input.cwd);
    ensure!(
        cwd.is_absolute() && cwd.is_dir(),
        "choose an existing absolute destination folder"
    );
    let cwd = cwd.canonicalize()?;
    let mut parsed = validate_native(m.agent, &m.native_id, bytes)?;
    let mut store = Store::open(dir)?;
    if let Some(existing) = store.session_for_native(m.agent, &m.native_id)? {
        ensure!(
            existing == input.destination,
            "native session already exists on this host"
        );
    }
    if let Some(existing) = store.session(&input.destination)? {
        ensure!(
            existing.agent == m.agent
                && existing.resume_id.as_deref() == Some(&m.native_id)
                && Path::new(&existing.cwd) == cwd,
            "destination session already exists"
        );
    }
    let bytes = relocated(m.agent, bytes, &cwd)?;
    if m.agent == AgentKind::OpenCode {
        // Refuse a native collision before invoking a CLI importer that may upsert.
        let db = opencode_db(dir)?;
        let existing = if db.is_file() {
            crate::opencode_store::discover(&db)?
                .into_iter()
                .find(|r| r.session_id == m.native_id)
        } else {
            None
        };
        ensure!(
            existing.is_none(),
            "OpenCode session already exists on the destination"
        );
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        let path = file.path().to_str().context("invalid import path")?;
        let modern = opencode(dir, &["session", "--help"], None).is_ok_and(|help| {
            let help = String::from_utf8_lossy(&help);
            help.contains("export") && help.contains("import")
        });
        let directory = cwd.to_str().context("invalid directory")?;
        let args = if modern {
            vec![
                "session",
                "import",
                path,
                "--directory",
                directory,
                "--standalone",
            ]
        } else {
            vec!["import", path, "--directory", directory]
        };
        // Import is an effect: never retry it under another command spelling after failure.
        opencode(dir, &args, Some(&cwd))?;
        // Do not launch an unrelated session if an importer assigned a different native identity.
        let found = crate::opencode_store::discover(&db)?
            .into_iter()
            .find(|r| r.session_id == m.native_id)
            .context("OpenCode did not preserve the imported native identity")?;
        parsed = crate::opencode_store::parse_session(&found)?;
    } else {
        install_jsonl(roots, m, &bytes, &cwd)?;
    }
    // A native resume honors the chosen project folder. ADK attaches to its own checkout.
    let launcher = AgentLauncher::default();
    let local = LocalSession {
        session_id: input.destination.clone(),
        agent: m.agent,
        cwd: cwd.to_string_lossy().into_owned(),
        resume_id: Some(m.native_id.clone()),
        launcher,
        adk_statusline: false,
    };
    // Publish the local binding only after the complete native copy exists.
    if !store.insert_imported_session(&local, "native")? {
        ensure!(
            store.session(&input.destination)?.as_ref() == Some(&local),
            "destination identity changed during import"
        );
    }
    let mut history = History::open(dir)?;
    history.import_metadata(&input.destination, &m.title, Some(&local.cwd))?;
    history.import_messages(
        &input.destination,
        &Source {
            agent: m.agent,
            native_id: m.native_id.clone(),
            origin: "private-host-migration".into(),
        },
        &parsed.messages,
    )?;
    Ok(())
}

pub(super) fn import(dir: &Path, kid: &str, input: &MigrationImport) -> Result<bool> {
    import_at(dir, &NativeRoots::local(), kid, input)
}

fn import_at(dir: &Path, roots: &NativeRoots, kid: &str, input: &MigrationImport) -> Result<bool> {
    ensure!(
        valid_transfer(&input.transfer)
            && authz::envelope::valid_session_id(&input.destination)
            && valid_manifest(&input.manifest),
        "invalid migration manifest"
    );
    ensure!(
        input.b64.len() <= MIGRATION_CHUNK_BYTES.div_ceil(3) * 4,
        "migration chunk too large"
    );
    let bytes = B64.decode(&input.b64)?;
    ensure!(
        !bytes.is_empty()
            && bytes.len() <= MIGRATION_CHUNK_BYTES
            && input.offset <= input.manifest.size
            && bytes.len() as u64 <= input.manifest.size - input.offset,
        "invalid migration chunk"
    );
    ensure!(
        Path::new(&input.cwd).is_absolute() && Path::new(&input.cwd).is_dir(),
        "choose an existing absolute destination folder"
    );
    let metadata = serde_json::to_string(&(&input.destination, &input.cwd, &input.manifest))?;
    let mut db = open(dir)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let existing: Option<(String, u64, bool)> = tx
        .query_row(
            "SELECT metadata,next,done FROM imports WHERE kid=?1 AND id=?2",
            params![kid, input.transfer],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let next = if let Some((bound, next, done)) = existing {
        ensure!(bound == metadata, "transfer metadata changed");
        if done {
            return Ok(true);
        }
        next
    } else {
        ensure!(input.offset == 0, "transfer expired; restart migration");
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM imports WHERE done=0", [], |r| {
            r.get(0)
        })?;
        ensure!(
            count < MAX_TRANSFERS,
            "too many incoming migrations; retry after they expire"
        );
        ensure!(
            Store::open(dir)?.session(&input.destination)?.is_none(),
            "destination session already exists"
        );
        tx.execute(
            "INSERT INTO imports(kid,id,metadata,created) VALUES(?1,?2,?3,?4)",
            params![kid, input.transfer, metadata, now_secs()],
        )?;
        0
    };
    if input.offset < next {
        let original: Option<Vec<u8>> = tx
            .query_row(
                "SELECT data FROM chunks WHERE kid=?1 AND id=?2 AND offset=?3",
                params![kid, input.transfer, input.offset],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            original.as_deref() == Some(bytes.as_slice()),
            "conflicting migration retry"
        );
    } else {
        ensure!(
            input.offset == next,
            "migration chunks arrived out of order"
        );
        tx.execute(
            "INSERT INTO chunks(kid,id,offset,data) VALUES(?1,?2,?3,?4)",
            params![kid, input.transfer, input.offset, bytes],
        )?;
        tx.execute(
            "UPDATE imports SET next=?3 WHERE kid=?1 AND id=?2",
            params![kid, input.transfer, input.offset + bytes.len() as u64],
        )?;
    }
    let next: u64 = tx.query_row(
        "SELECT next FROM imports WHERE kid=?1 AND id=?2",
        params![kid, input.transfer],
        |r| r.get(0),
    )?;
    if next != input.manifest.size {
        tx.commit()?;
        return Ok(false);
    }
    let mut data = Vec::with_capacity(next as usize);
    {
        let mut stmt =
            tx.prepare("SELECT data FROM chunks WHERE kid=?1 AND id=?2 ORDER BY offset")?;
        for chunk in stmt.query_map(params![kid, input.transfer], |r| r.get::<_, Vec<u8>>(0))? {
            data.extend(chunk?);
        }
    }
    ensure!(
        digest(&data) == input.manifest.sha256,
        "migration checksum mismatch"
    );
    install(dir, roots, input, &data)?;
    tx.execute(
        "UPDATE imports SET done=1 WHERE kid=?1 AND id=?2",
        params![kid, input.transfer],
    )?;
    tx.execute(
        "DELETE FROM chunks WHERE kid=?1 AND id=?2",
        params![kid, input.transfer],
    )?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    fn roots(home: &Path) -> NativeRoots {
        NativeRoots {
            claude: Some(home.join("claude")),
            codex: Some(home.join("codex")),
        }
    }
    fn fixture(home: &Path, agent: AgentKind, native: &str) -> PathBuf {
        let root = if agent == AgentKind::Claude {
            home.join("claude/project")
        } else {
            home.join("codex/2026/11/10")
        };
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("{native}.jsonl"));
        let header = if agent == AgentKind::Claude {
            serde_json::json!({"type":"user","sessionId":native,"uuid":"original","cwd":"/source","message":{"role":"user","content":"original context ".repeat(2000)}})
        } else {
            serde_json::json!({"type":"session_meta","payload":{"id":native,"timestamp":"2026-11-10T12:00:00Z","cwd":"/source","source":"cli","cli_version":"0.167.0","model_provider":"openai"}})
        };
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "{header}").unwrap();
        if agent == AgentKind::Codex {
            writeln!(file,"{}",serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"original context ".repeat(2000)}]}})).unwrap();
        }
        // A live writer has not finished this record. It must not corrupt the copy.
        write!(file, "{{\"unfinished\":").unwrap();
        path
    }

    #[test]
    fn native_copy_lifecycle_keeps_source_and_resumes_destination_after_restart() {
        for agent in [AgentKind::Claude, AgentKind::Codex] {
            let source = private();
            let target = private();
            let native = uuid::Uuid::new_v4().to_string();
            let source_id = uuid::Uuid::new_v4().to_string();
            let destination = uuid::Uuid::new_v4().to_string();
            let transfer = uuid::Uuid::new_v4().to_string();
            let path = fixture(source.path(), agent, &native);
            let original = std::fs::read(&path).unwrap();
            Store::open(source.path())
                .unwrap()
                .insert_session(
                    &LocalSession {
                        session_id: source_id.clone(),
                        agent,
                        cwd: "/source".into(),
                        resume_id: Some(native.clone()),
                        launcher: AgentLauncher::default(),
                        adk_statusline: false,
                    },
                    "signed_start",
                )
                .unwrap();
            History::open(source.path())
                .unwrap()
                .import_metadata(&source_id, "Kept title", None)
                .unwrap();
            let mut offset = 0;
            let mut latest = None;
            loop {
                let ReplyBody::Migration {
                    manifest,
                    b64,
                    next,
                } = export_at(
                    source.path(),
                    &roots(source.path()),
                    "reader",
                    &source_id,
                    &transfer,
                    offset,
                )
                .unwrap()
                else {
                    panic!()
                };
                let input = MigrationImport {
                    transfer: transfer.clone(),
                    destination: destination.clone(),
                    cwd: target.path().to_string_lossy().into_owned(),
                    manifest,
                    offset,
                    b64,
                };
                let copied =
                    import_at(target.path(), &roots(target.path()), "owner", &input).unwrap();
                assert_eq!(copied, next.is_none());
                if next.is_some() {
                    assert!(
                        Store::open(target.path())
                            .unwrap()
                            .session(&destination)
                            .unwrap()
                            .is_none(),
                        "partial copy must not be resumable"
                    );
                    // The same chunk can be retried safely, but not under another key.
                    assert!(
                        !import_at(target.path(), &roots(target.path()), "owner", &input).unwrap()
                    );
                }
                if offset == 0 {
                    std::fs::write(&path, format!("{}\n", String::from_utf8_lossy(&original)))
                        .unwrap();
                }
                latest.replace(input);
                match next {
                    Some(next) => offset = next,
                    None => break,
                }
            }
            let input = latest.unwrap();
            assert!(
                import_at(target.path(), &roots(target.path()), "owner", &input).unwrap(),
                "completed retry returns its receipt"
            );
            let saved = Store::open(target.path())
                .unwrap()
                .session(&destination)
                .unwrap()
                .unwrap();
            assert_eq!(saved.resume_id.as_deref(), Some(native.as_str()));
            assert_eq!(saved.cwd, target.path().to_string_lossy());
            let command = authz::dispatch::reopen_command(&Settings::default(), &saved).unwrap();
            assert!(
                matches!(command,crate::commands::live::host::ControlCommand::Reopen { resume_id: Some(id), .. } if id==native)
            );
            let messages = History::open(target.path())
                .unwrap()
                .messages(&destination, 100)
                .unwrap();
            assert!(!messages.is_empty());
            assert_eq!(
                History::open(target.path())
                    .unwrap()
                    .summary(&destination)
                    .unwrap()
                    .title
                    .as_deref(),
                Some("Kept title")
            );
            let installed = snapshot_jsonl(&roots(target.path()), agent, &native).unwrap();
            assert!(String::from_utf8_lossy(&installed).contains(target.path().to_str().unwrap()));
            assert!(String::from_utf8_lossy(&installed).contains("original context"));
            assert!(Store::open(source.path())
                .unwrap()
                .session(&source_id)
                .unwrap()
                .is_some());
            assert!(path.exists());
        }
    }

    #[test]
    fn incomplete_corrupt_and_conflicting_copies_never_replace_native_history() {
        let source = private();
        let target = private();
        let native = uuid::Uuid::new_v4().to_string();
        let path = fixture(source.path(), AgentKind::Claude, &native);
        let bytes = snapshot_jsonl(&roots(source.path()), AgentKind::Claude, &native).unwrap();
        let manifest = MigrationManifest {
            agent: AgentKind::Claude,
            native_id: native.clone(),
            title: "Test".into(),
            size: bytes.len() as u64,
            sha256: digest(&bytes),
        };
        let mut input = MigrationImport {
            transfer: uuid::Uuid::new_v4().to_string(),
            destination: uuid::Uuid::new_v4().to_string(),
            cwd: target.path().to_string_lossy().into_owned(),
            manifest,
            offset: 1,
            b64: B64.encode(&bytes[..10]),
        };
        assert!(import_at(target.path(), &roots(target.path()), "owner", &input).is_err());
        input.offset = 0;
        input.manifest.size = 10;
        assert!(
            import_at(target.path(), &roots(target.path()), "owner", &input).is_err(),
            "checksum mismatch cannot commit"
        );
        assert!(Store::open(target.path())
            .unwrap()
            .session(&input.destination)
            .unwrap()
            .is_none());
        input.manifest.size = bytes.len() as u64;
        input.b64 = B64.encode(&bytes[..MIGRATION_CHUNK_BYTES]);
        assert!(!import_at(target.path(), &roots(target.path()), "owner", &input).unwrap());
        input.b64 = B64.encode(vec![b'x'; MIGRATION_CHUNK_BYTES]);
        assert!(
            import_at(target.path(), &roots(target.path()), "owner", &input).is_err(),
            "same offset cannot carry different bytes"
        );
        assert!(Store::open(target.path())
            .unwrap()
            .session(&input.destination)
            .unwrap()
            .is_none());
        let target_roots = roots(target.path());
        let cwd = target.path().canonicalize().unwrap();
        let relocated = relocated(AgentKind::Claude, &bytes, &cwd).unwrap();
        install_jsonl(&target_roots, &input.manifest, &relocated, &cwd).unwrap();
        let before = snapshot_jsonl(&target_roots, AgentKind::Claude, &native).unwrap();
        let changed = String::from_utf8(relocated)
            .unwrap()
            .replace("original context", "different context");
        assert!(install_jsonl(&target_roots, &input.manifest, changed.as_bytes(), &cwd).is_err());
        assert_eq!(
            snapshot_jsonl(&target_roots, AgentKind::Claude, &native).unwrap(),
            before
        );
        assert!(path.exists());
    }
}
