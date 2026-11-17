//! Canonical local session records. The host is the authority on where a
//! session runs and which native agent session it resumes; the central server's
//! copy is never consulted for execution.
//!
//! Producers: a signed start (`record_started`), every wrapper registration
//! (`register_launch`: normal and cmux backends, local `Start`, restore), the
//! native identity probes (`bind_native`, Claude's `--session-id` argv) and the
//! cmux resume sync (`bind_native`), and one-time import of this host's own
//! pre-existing checkpoints (`import_once`). Consumers: signed `session.reopen`, restore, the
//! transcript binding and every grant check.

use anyhow::{ensure, Context, Result};
use conversations_core::{AgentKind, AgentLauncher};
use rusqlite::{params, OptionalExtension};

use super::envelope::valid_session_id;
use super::store::{now_secs, Store};
use crate::open_sessions::OpenSession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSession {
    pub session_id: String,
    pub agent: AgentKind,
    pub cwd: String,
    pub resume_id: Option<String>,
    pub launcher: AgentLauncher,
    pub adk_statusline: bool,
}

fn to_text<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .context("not a string-valued enum")
}

fn from_text<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(
        text.to_owned(),
    ))?)
}

/// Point-lookups for connected IDs: no archive rows are visited or summarized.
pub(super) fn running_sessions(
    conn: &rusqlite::Connection,
    ids: &[String],
    after: Option<&str>,
) -> Result<Vec<super::store::ListedSession>> {
    let cursor = match after {
        Some(cursor) => {
            let (at, id) = cursor.split_once(':').context("invalid session cursor")?;
            Some((at.parse::<i64>()?, id))
        }
        None => None,
    };
    let mut statement =
        conn.prepare("SELECT session_id,agent,cwd,created_at FROM sessions WHERE session_id=?1")?;
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        let row = statement
            .query_row([id], |r| {
                Ok(super::store::ListedSession {
                    session_id: r.get(0)?,
                    agent: r.get(1)?,
                    cwd: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })
            .optional()?;
        if let Some(row) = row {
            if cursor.is_none_or(|(at, id)| {
                row.created_at < at || (row.created_at == at && row.session_id.as_str() < id)
            }) {
                rows.push(row);
            }
        }
    }
    rows.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.session_id.cmp(&a.session_id))
    });
    Ok(rows)
}

impl Store {
    /// Local owner listing. Remote callers must use readable_sessions instead.
    pub(crate) fn owner_sessions(
        &self,
        after: Option<&str>,
        limit: u32,
        running: Option<&[String]>,
    ) -> Result<(Vec<super::store::ListedSession>, Option<String>)> {
        let (at, id) = match after {
            Some(cursor) => {
                let (at, id) = cursor.split_once(':').context("invalid session cursor")?;
                (at.parse::<i64>()?, id.to_owned())
            }
            None => (i64::MAX, String::new()),
        };
        let limit = limit.clamp(1, 50) as usize;
        let mut rows = if let Some(ids) = running {
            running_sessions(&self.conn, ids, after)?
        } else {
            let mut stmt = self.conn.prepare("SELECT session_id,agent,cwd,created_at FROM sessions WHERE ?1='' OR created_at<?2 OR (created_at=?2 AND session_id<?1) ORDER BY created_at DESC,session_id DESC LIMIT ?3")?;
            let rows = stmt
                .query_map(params![id, at, (limit + 1) as i64], |r| {
                    Ok(super::store::ListedSession {
                        session_id: r.get(0)?,
                        agent: r.get(1)?,
                        cwd: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let next = if rows.len() > limit {
            rows.truncate(limit);
            rows.last()
                .map(|r| format!("{}:{}", r.created_at, r.session_id))
        } else {
            None
        };
        Ok((rows, next))
    }

    /// Insert a session this host just started. Refuses to replace one.
    pub fn insert_session(&mut self, session: &LocalSession, origin: &str) -> Result<bool> {
        ensure!(valid_session_id(&session.session_id), "invalid session id");
        let now = now_secs();
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO sessions(session_id, agent, cwd, resume_id, launcher, adk_statusline, origin, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                session.session_id,
                to_text(&session.agent)?,
                session.cwd,
                session.resume_id,
                to_text(&session.launcher)?,
                session.adk_statusline,
                origin,
                now
            ],
        )?;
        Ok(changed == 1)
    }

    /// Only explicit local import creates these records; it never changes an existing binding.
    pub fn insert_imported_session(&mut self, session: &LocalSession, mode: &str) -> Result<bool> {
        self.session_transaction(|store| {
            if !store.insert_session(session, "adopted")? {
                return Ok(false);
            }
            store.conn.execute(
                "INSERT INTO remote_imports(session_id,resume_mode) VALUES(?1,?2)",
                params![session.session_id, mode],
            )?;
            Ok(true)
        })
    }

    pub fn import_resume_mode(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT CASE WHEN r.resume_mode = 'continuation' AND s.resume_id IS NOT NULL THEN 'native' ELSE r.resume_mode END FROM remote_imports r JOIN sessions s USING(session_id) WHERE session_id=?1 AND r.resume_mode != 'unsupported'", [id], |r| r.get(0)).optional()?)
    }

    /// Imported read-only records may be read but never launched.
    pub fn can_reopen_import(&self, session_id: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT NOT EXISTS(SELECT 1 FROM remote_imports WHERE session_id=?1 AND resume_mode NOT IN ('native', 'continuation'))", [session_id], |r| r.get(0))?)
    }

    pub fn session(&self, session_id: &str) -> Result<Option<LocalSession>> {
        let row: Option<(String, String, Option<String>, String, bool)> = self
            .conn
            .query_row(
                "SELECT agent, cwd, resume_id, launcher, adk_statusline FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        row.map(|(agent, cwd, resume_id, launcher, adk_statusline)| {
            Ok(LocalSession {
                session_id: session_id.to_owned(),
                agent: from_text(&agent)?,
                cwd,
                resume_id,
                launcher: from_text(&launcher)?,
                adk_statusline,
            })
        })
        .transpose()
    }

    /// The canonical local session bound to `agent`'s native session `native`.
    /// A native id that is itself a recorded session of that agent is its own.
    pub fn session_for_native(&self, agent: AgentKind, native: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT session_id FROM sessions
                  WHERE agent = ?1 AND (resume_id = ?2 OR (session_id = ?2 AND resume_id IS NULL))
                  ORDER BY session_id = ?2 DESC, created_at LIMIT 1",
                params![to_text(&agent)?, native],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Bind the native agent session this host proved for `session_id`, once. The
    /// same value again is a no-op; a different value than the established one is
    /// refused rather than retargeting the session.
    pub fn bind_resume_once(&mut self, session_id: &str, native: &str) -> Result<bool> {
        ensure!(
            !native.trim().is_empty()
                && native.len() <= 256
                && !native.chars().any(char::is_control),
            "invalid native session id"
        );
        let changed = self.conn.execute(
            "UPDATE sessions SET resume_id = ?2, updated_at = ?3 WHERE session_id = ?1 AND resume_id IS NULL",
            params![session_id, native, now_secs()],
        )?;
        let current = self.session(session_id)?.context("unknown local session")?;
        ensure!(
            current.resume_id.as_deref() == Some(native),
            "session {session_id} is already bound to another native session"
        );
        Ok(changed == 1)
    }

    pub fn set_handoff_model(
        &mut self,
        id: &str,
        model: &conversations_core::messaging::HandoffModel,
    ) -> Result<()> {
        let json = serde_json::to_string(model)?;
        self.conn.execute(
            "UPDATE sessions SET handoff_model = ?2 WHERE session_id = ?1",
            params![id, json],
        )?;
        Ok(())
    }

    pub fn handoff_model(
        &self,
        id: &str,
    ) -> Result<Option<conversations_core::messaging::HandoffModel>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT handoff_model FROM sessions WHERE session_id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
    }

    pub fn delete_session(&mut self, session_id: &str, origin: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM sessions WHERE session_id = ?1 AND origin = ?2",
            params![session_id, origin],
        )?;
        Ok(())
    }
}

impl From<&OpenSession> for LocalSession {
    fn from(saved: &OpenSession) -> Self {
        Self {
            session_id: saved.session_id.clone(),
            agent: saved.agent,
            cwd: saved.cwd.clone(),
            resume_id: saved.resume_id.clone(),
            launcher: saved.launcher,
            adk_statusline: saved.adk_statusline,
        }
    }
}

impl Store {
    fn session_transaction<T>(&mut self, action: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match action(self) {
            Ok(value) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn save_restore(&mut self, saved: &OpenSession, server: &str) -> Result<()> {
        self.session_transaction(|store| {
            store.insert_session(&LocalSession::from(saved), "adopted")?;
            if let Some(native) = &saved.resume_id {
                store.bind_resume_once(&saved.session_id, native)?;
            }
            store.open_for_restore(&saved.session_id, server)?;
            Ok(())
        })
    }

    pub fn is_open_for_restore(&self, id: &str, server: &str) -> Result<bool> {
        let server = server.trim_end_matches('/');
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_restore WHERE session_id = ?1 AND server = ?2)",
            params![id, server],
            |r| r.get(0),
        )?)
    }

    /// Open intent is separate from historical session metadata. Removing it
    /// never destroys a native identity needed for an explicit later reopen.
    pub fn open_for_restore(&mut self, id: &str, server: &str) -> Result<bool> {
        let server = server.trim_end_matches('/');
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO local_restore(session_id, server) VALUES (?1, ?2)",
            params![id, server],
        )? == 1)
    }

    pub fn close_for_restore(&mut self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM local_restore WHERE session_id = ?1", [id])?;
        Ok(())
    }

    pub fn restore_sessions(&self, server: &str) -> Result<Vec<LocalSession>> {
        let server = server.trim_end_matches('/');
        let mut query = self
            .conn
            .prepare("SELECT session_id FROM local_restore WHERE server = ?1 ORDER BY rowid")?;
        let ids = query
            .query_map([server], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| self.session(&id)?.context("restore session missing"))
            .collect()
    }
}

/// Import the old host checkpoints exactly once per server, atomically with
/// their open intent. Keep JSON untouched for the independent legacy live host.
pub fn import_once(dir: &std::path::Path, server: &str) -> Result<usize> {
    import_from(dir, server, || crate::open_sessions::try_load_for(server))
}

fn import_from(
    dir: &std::path::Path,
    server: &str,
    load: impl FnOnce() -> Result<Vec<OpenSession>>,
) -> Result<usize> {
    let server = server.trim_end_matches('/');
    let mut store = Store::open(dir)?;
    let imported = |store: &Store| -> Result<bool> {
        Ok(store.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM local_restore_imports WHERE server = ?1)",
            [server],
            |r| r.get(0),
        )?)
    };
    if imported(&store)? {
        return Ok(0);
    }
    store.session_transaction(|store| {
        if imported(store)? {
            return Ok(0);
        }
        let mut count = 0;
        for saved in load()? {
            if !valid_session_id(&saved.session_id) {
                continue;
            }
            store.insert_session(&LocalSession::from(&saved), "adopted")?;
            count += usize::from(store.open_for_restore(&saved.session_id, server)?);
        }
        store.conn.execute(
            "INSERT INTO local_restore_imports(server) VALUES (?1)",
            [server],
        )?;
        Ok(count)
    })
}

/// Restore uses the same current argument policy as signed reopen. OpenSession
/// is only a launch DTO here; this path never writes a second copy to JSON.
pub fn restore(dir: &std::path::Path, server: &str) -> Result<Vec<OpenSession>> {
    let settings = super::super::settings::Settings::load(dir)?;
    restore_with_settings(dir, server, &settings)
}

fn restore_with_settings(
    dir: &std::path::Path,
    server: &str,
    settings: &super::super::settings::Settings,
) -> Result<Vec<OpenSession>> {
    let store = Store::open(dir)?;
    store
        .restore_sessions(server)?
        .into_iter()
        .map(|s| {
            let mut model_args = store
                .handoff_model(&s.session_id)?
                .map(|m| crate::commands::live::interactive::model_args(s.agent, &m))
                .unwrap_or_default();
            let mut args = settings.launch.agent_args(s.agent)?;
            args.append(&mut model_args);
            Ok(OpenSession {
                agent_args: if s.agent == AgentKind::Zsh {
                    vec![]
                } else {
                    args
                },
                session_id: s.session_id,
                agent: s.agent,
                cwd: s.cwd,
                resume_id: s.resume_id,
                launcher: s.launcher,
                adk_statusline: s.adk_statusline,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(agent: AgentKind) -> OpenSession {
        OpenSession {
            session_id: uuid::Uuid::new_v4().to_string(),
            agent,
            cwd: "/canonical".into(),
            resume_id: Some("native-id".into()),
            agent_args: vec!["obsolete-flag".into()],
            launcher: AgentLauncher::Adk,
            adk_statusline: true,
        }
    }

    #[test]
    fn import_keeps_canonical_identity_and_never_reopens_closed_history() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let canonical = saved(AgentKind::Codex);
        let history = saved(AgentKind::Claude);
        let mut store = Store::open(dir.path()).unwrap();
        store
            .insert_session(&LocalSession::from(&canonical), "signed_start")
            .unwrap();
        store
            .insert_session(&LocalSession::from(&history), "adopted")
            .unwrap();
        let mut obsolete = canonical.clone();
        obsolete.cwd = "/obsolete".into();
        obsolete.resume_id = Some("wrong-native".into());
        assert!(import_from(dir.path(), "https://one", || anyhow::bail!(
            "unreadable legacy JSON"
        ))
        .is_err());
        assert_eq!(
            import_from(dir.path(), "https://one", || Ok(vec![obsolete.clone()])).unwrap(),
            1
        );
        assert_eq!(
            store.restore_sessions("https://one").unwrap(),
            vec![LocalSession::from(&canonical)]
        );
        store.close_for_restore(&canonical.session_id).unwrap();
        assert_eq!(
            import_from(dir.path(), "https://one/", || panic!(
                "must not read stale JSON again"
            ))
            .unwrap(),
            0
        );
        assert!(store.restore_sessions("https://one").unwrap().is_empty());
        assert!(store.session(&canonical.session_id).unwrap().is_some());
        assert!(store.session(&history.session_id).unwrap().is_some());
        assert!(store.restore_sessions("https://other").unwrap().is_empty());
    }

    #[test]
    fn restore_uses_current_arguments_but_preserves_harness_native_id_and_shell_intent() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let codex = saved(AgentKind::Codex);
        let shell = saved(AgentKind::Zsh);
        assert_eq!(
            import_from(dir.path(), "http://local", || Ok(vec![
                codex.clone(),
                shell.clone()
            ]))
            .unwrap(),
            2
        );
        let model = conversations_core::messaging::HandoffModel {
            model: "gpt-5".into(),
            variant: Some("high".into()),
        };
        Store::open(dir.path())
            .unwrap()
            .set_handoff_model(&codex.session_id, &model)
            .unwrap();
        assert_eq!(
            Store::open(dir.path())
                .unwrap()
                .handoff_model(&codex.session_id)
                .unwrap(),
            Some(model)
        );
        let mut settings = super::super::super::settings::Settings::default();
        settings.launch.codex_sandbox_mode = Some("read-only".into());
        let restored = restore_with_settings(dir.path(), "http://local", &settings).unwrap();
        assert_eq!(
            restored[0].agent_args,
            [
                "--sandbox",
                "read-only",
                "--model",
                "gpt-5",
                "-c",
                "model_reasoning_effort=\"high\""
            ]
        );
        assert_eq!(restored[0].resume_id, codex.resume_id);
        assert_eq!(restored[0].launcher, codex.launcher);
        assert!(restored[0].adk_statusline);
        assert_eq!(restored[1].session_id, shell.session_id);
        assert!(
            restored[1].agent_args.is_empty(),
            "shell commands are never replayed"
        );
    }
}
