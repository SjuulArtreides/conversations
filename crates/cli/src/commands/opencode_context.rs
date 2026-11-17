//! Native tool-call identity for OpenCode's shared, location-scoped MCP process.
use super::messaging::MessagingClient;
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection};
use std::fs;

fn native_id(native: &str) -> Result<()> {
    ensure!(
        native.starts_with("ses_")
            && native.len() > 4
            && native.len() <= 256
            && native
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "missing or invalid OpenCode native session context"
    );
    Ok(())
}

fn registry() -> Result<Connection> {
    let directory = dirs::data_dir()
        .context("Could not locate personal application data")?
        .join("arrakis-conversations/opencode-context");
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(directory.join("bindings.sqlite"))?;
        fs::set_permissions(
            directory.join("bindings.sqlite"),
            fs::Permissions::from_mode(0o600),
        )?;
    }
    let db = Connection::open(directory.join("bindings.sqlite"))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS bindings (
        origin TEXT NOT NULL, native TEXT NOT NULL, source TEXT NOT NULL,
        PRIMARY KEY(origin,native))",
    )?;
    Ok(db)
}

/// Called only by the bridge after its explicit native binding succeeds, while
/// holding that source's delivery lock. No directory/newest-session inference.
pub(crate) async fn register_native(client: &MessagingClient, native: &str) -> Result<()> {
    native_id(native)?;
    ensure!(
        client.native_session_id().await? == native,
        "OpenCode source native identity changed"
    );
    uuid::Uuid::parse_str(client.session_id()).context("invalid Conversations source identity")?;
    let mut db = registry()?;
    let transaction = db.transaction()?;
    transaction.execute(
        "INSERT OR IGNORE INTO bindings(origin,native,source) VALUES (?1,?2,?3)",
        params![client.server(), native, client.session_id()],
    )?;
    let source: String = transaction.query_row(
        "SELECT source FROM bindings WHERE origin=?1 AND native=?2",
        params![client.server(), native],
        |row| row.get(0),
    )?;
    ensure!(
        source == client.session_id(),
        "OpenCode native session already belongs to another Conversations session; reopen its original wrapped terminal"
    );
    transaction.commit()?;
    Ok(())
}

pub(crate) async fn resolve(
    server: &str,
    cookie: Option<&str>,
    native: &str,
) -> Result<MessagingClient> {
    native_id(native)?;
    let canonical = MessagingClient::new(server, "context-validation")?;
    let source: String = registry()?.query_row("SELECT source FROM bindings WHERE origin=?1 AND native=?2",
        params![canonical.server(), native], |row| row.get(0))
        .context("No explicit Conversations binding for this OpenCode session; open it in its wrapped terminal first")?;
    let client = match cookie {
        Some(cookie) => MessagingClient::with_cookie(server, &source, Some(cookie.to_owned()))?,
        None => MessagingClient::new(server, &source)?,
    };
    ensure!(
        client.native_session_id().await? == native,
        "Conversations source now belongs to a different native session"
    );
    // Metadata can be visible through a read share. Each tool's existing server
    // operation, not this lookup, enforces ownership and communication permissions.
    Ok(client)
}
