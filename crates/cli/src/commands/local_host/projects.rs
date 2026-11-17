//! Persistent local project metadata shared by owner HTTP and signed private operations.
use super::authz::store::open_private;
use anyhow::{Context, Result};
use conversations_core::Project;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;

pub(super) struct Projects {
    pub(super) conn: Connection,
}
impl Projects {
    pub(super) fn open(dir: &Path) -> Result<Self> {
        let mut conn = open_private(dir, "projects.db")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        anyhow::ensure!(version <= 1, "project database was written by a newer host");
        if version == 0 {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
            anyhow::ensure!(version <= 1, "project database was written by a newer host");
            if version == 0 {
                tx.execute_batch("CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT NOT NULL,description TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
                    CREATE TABLE project_sessions(session_id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE);
                    CREATE INDEX project_members ON project_sessions(project_id,session_id);")?;
                tx.pragma_update(None, "user_version", 1)?;
            }
            tx.commit()?;
        }
        Ok(Self { conn })
    }
    pub(super) fn get(&self, id: &str) -> Result<Option<Project>> {
        Ok(self.conn.query_row("SELECT id,name,description,created_at,updated_at,(SELECT COUNT(*) FROM project_sessions WHERE project_id=projects.id) FROM projects WHERE id=?1", [id], project_row).optional()?)
    }
    pub(super) fn list(&self) -> Result<Vec<Project>> {
        Ok(self.conn.prepare("SELECT id,name,description,created_at,updated_at,(SELECT COUNT(*) FROM project_sessions WHERE project_id=projects.id) FROM projects ORDER BY updated_at DESC,id")?.query_map([],project_row)?.collect::<rusqlite::Result<_>>()?)
    }
    pub(super) fn create(&self, name: &str, description: Option<&str>) -> Result<Project> {
        let name =
            super::history::validate_title(name)?.context("project name must not be empty")?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute("INSERT INTO projects(id,name,description,created_at,updated_at) VALUES(?1,?2,?3,?4,?4)",params![id,name,description,now])?;
        self.get(&id)?.context("created project disappeared")
    }
    pub(super) fn members(&self, id: &str) -> Result<Vec<String>> {
        Ok(self
            .conn
            .prepare(
                "SELECT session_id FROM project_sessions WHERE project_id=?1 ORDER BY session_id",
            )?
            .query_map([id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub(super) fn membership(&self, session: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT project_id FROM project_sessions WHERE session_id=?1",
                [session],
                |row| row.get(0),
            )
            .optional()?)
    }
    pub(super) fn assign(&mut self, session: &str, project: Option<&str>) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // FK validation and assignment share a transaction; a deleted project cannot be revived.
        if let Some(project) = project {
            tx.execute("INSERT INTO project_sessions(session_id,project_id) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET project_id=excluded.project_id", params![session,project])?;
        } else {
            tx.execute(
                "DELETE FROM project_sessions WHERE session_id=?1",
                [session],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}
fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let time = |column| -> rusqlite::Result<chrono::DateTime<chrono::Utc>> {
        row.get::<_, String>(column)?.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    };
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        owner_email: None,
        session_count: row.get::<_, i64>(5)? as usize,
        created_at: time(3)?,
        updated_at: time(4)?,
    })
}

/// The daemon's metadata effect, with the same current-authority recheck as rename.
pub(super) fn execute(dir: &Path, op: &super::authz::envelope::Verified) -> Result<String> {
    use super::authz::{dispatch, envelope::Operation};
    let mut projects = Projects::open(dir)?;
    dispatch::begin(dir, op)?;
    match &op.op {
        Operation::ProjectCreate { name, description } => Ok(serde_json::to_string(
            &projects.create(name, description.as_deref())?,
        )?),
        Operation::SessionProjectSet {
            session,
            project_id,
        } => {
            projects.assign(session, project_id.as_deref())?;
            Ok(serde_json::json!({"project_id":project_id}).to_string())
        }
        _ => anyhow::bail!("operation is not a project mutation"),
    }
}
