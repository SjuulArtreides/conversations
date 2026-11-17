//! Explicit local import; remote data never supplies launch commands or grants.
use super::{
    authz::{sessions::LocalSession, store::Store},
    history::{History, Source},
    settings::Settings,
};
use anyhow::{ensure, Context, Result};
use conversations_core::{AgentKind, AgentLauncher, Message};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path};

#[derive(Deserialize)]
struct RemoteSession {
    id: String,
    agent: AgentKind,
    title: String,
    owner_email: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    agent_session_id: Option<String>,
    messages: Vec<Message>,
}

#[derive(Default, Serialize)]
struct Report {
    owned: usize,
    imported: usize,
    resumable: usize,
    continuation: usize,
    read_only: usize,
    unowned: usize,
    failed: usize,
}

fn import_one(
    dir: &Path,
    owner: &str,
    remote: &RemoteSession,
    native: Option<&str>,
    settings: &Settings,
) -> Result<bool> {
    ensure!(
        remote.owner_email.as_deref() == Some(owner),
        "session is not owned by the authenticated user"
    );
    ensure!(
        super::authz::envelope::valid_session_id(&remote.id),
        "invalid remote session id"
    );
    let remote_cwd = remote
        .cwd
        .as_deref()
        .map(Path::new)
        .filter(|p| p.is_absolute() && p.is_dir());
    let cwd = match remote_cwd {
        Some(p) => p.canonicalize()?,
        None => std::env::current_dir()?.canonicalize()?,
    };
    let supported = remote.agent != AgentKind::Zsh;
    let launcher = if settings.launch.default_launcher.supports(remote.agent) {
        settings.launch.default_launcher
    } else {
        AgentLauncher::Opencode
    };
    let record = LocalSession {
        session_id: remote.id.clone(),
        agent: remote.agent,
        cwd: cwd.to_string_lossy().into_owned(),
        resume_id: native.map(str::to_owned),
        launcher,
        adk_statusline: launcher == AgentLauncher::Adk && settings.launch.adk_statusline,
    };
    let mut store = Store::open(dir)?;
    store.insert_imported_session(
        &record,
        if !supported {
            "unsupported"
        } else if native.is_some() {
            "native"
        } else {
            "continuation"
        },
    )?;
    let mut history = History::open(dir)?;
    history.import_metadata(&remote.id, &remote.title, remote.cwd.as_deref())?;
    history.import_messages(
        &remote.id,
        &Source {
            agent: remote.agent,
            native_id: remote
                .agent_session_id
                .clone()
                .unwrap_or_else(|| remote.id.clone()),
            origin: "explicit-remote-import".into(),
        },
        &remote.messages,
    )?;
    store.can_reopen_import(&remote.id)
}

/// Build one local index. The server's asserted native id is only a lookup key.
fn native_index() -> HashSet<(String, String)> {
    let mut ids = HashSet::new();
    for (agent, root) in [
        (AgentKind::Claude, conversations_core::claude_projects_dir()),
        (AgentKind::Codex, conversations_core::codex_sessions_dir()),
    ] {
        if let Some(root) = root {
            for file in conversations_core::discover_sessions_for_agent(&root, agent) {
                if let Some(summary) = conversations_core::quick_summary(&file.path) {
                    ids.insert((agent.as_str().to_owned(), summary.id));
                }
            }
        }
    }
    if let Ok(path) = crate::opencode_store::resolve_db_path(None) {
        if let Ok(files) = crate::opencode_store::discover(&path) {
            ids.extend(files.into_iter().map(|f| ("opencode".into(), f.session_id)));
        }
    }
    ids
}

fn matched_native<'a>(
    remote: &'a RemoteSession,
    index: &HashSet<(String, String)>,
) -> Option<&'a str> {
    let id = remote.agent_session_id.as_deref().unwrap_or(&remote.id);
    index
        .contains(&(remote.agent.as_str().to_owned(), id.to_owned()))
        .then_some(id)
}

pub fn run(dir: &Path, server: &str, selected: Option<&str>, json: bool) -> Result<()> {
    super::authz::ensure_dir(dir)?;
    let cookie = crate::credentials::cookie_for(server)
        .context("log in to the source server before importing")?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let report = rt.block_on(async {
        let get = |path: String| client.get(format!("{}{path}", server.trim_end_matches('/'))).header(reqwest::header::COOKIE, &cookie);
        let me: serde_json::Value = get("/api/me".into()).send().await?.error_for_status()?.json().await?;
        let owner = me["email"].as_str().filter(|v| !v.is_empty()).context("source server did not return an authenticated identity")?;
        let list: serde_json::Value = get("/api/sessions".into()).send().await?.error_for_status()?.json().await?;
        let summaries = list["sessions"].as_array().context("invalid session list")?;
        let native = native_index();
        let settings = Settings::load(dir)?;
        let mut report = Report::default();
        for summary in summaries {
            let Some(id) = summary["id"].as_str() else { report.failed += 1; continue; };
            if selected.is_some_and(|wanted| wanted != id) { continue; }
            if summary["owner_email"].as_str() != Some(owner) { report.unowned += 1; continue; }
            report.owned += 1;
            if !super::authz::envelope::valid_session_id(id) { report.failed += 1; continue; }
            let result: Result<(bool, Option<String>)> = async {
                let remote: RemoteSession = get(format!("/api/sessions/{id}")).send().await?.error_for_status()?.json().await?;
                ensure!(remote.id == id, "detail identity mismatch");
                let matched = matched_native(&remote, &native);
                Ok((import_one(dir, owner, &remote, matched, &settings)?, Store::open(dir)?.import_resume_mode(id)?))
            }.await;
            match result {
                Ok((reopenable, mode)) => { report.imported += 1; if mode.as_deref() == Some("continuation") { report.continuation += 1; } else if reopenable { report.resumable += 1; } else { report.read_only += 1; } },
                Err(error) => { if selected.is_some() { return Err(error.context("importing selected session")); } report.failed += 1; eprintln!("Could not import session {id}; rerun with --session {id} after checking source access."); }
            }
        }
        ensure!(selected.is_none() || report.owned > 0, "selected session was not found among owned sessions");
        Ok::<_, anyhow::Error>(report)
    })?;
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!("Imported {} of {} owned sessions: {} resumable, {} available for local continuation, {} read-only; {} unowned skipped, {} failed.", report.imported, report.owned, report.resumable, report.continuation, report.read_only, report.unowned, report.failed);
    }
    ensure!(
        report.failed == 0,
        "some sessions failed to import; successfully imported history is retained"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn remote_native_assertion_requires_local_evidence_for_the_same_agent() {
        let mut remote: RemoteSession = serde_json::from_value(serde_json::json!({
            "id":"wrapper", "agent":"codex", "agent_session_id":"native", "title":"A name",
            "owner_email":"me@example.com", "cwd":"/workspace", "messages":[]
        }))
        .unwrap();
        let index = HashSet::from([("codex".into(), "native".into())]);
        assert_eq!(matched_native(&remote, &index), Some("native"));
        remote.agent = AgentKind::Claude;
        assert_eq!(matched_native(&remote, &index), None);
        remote.agent = AgentKind::Codex;
        remote.agent_session_id = None;
        assert_eq!(matched_native(&remote, &index), None);
        remote.id = "native".into();
        assert_eq!(matched_native(&remote, &index), Some("native"));
    }

    #[test]
    fn import_schema_upgrade_keeps_existing_identity_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let host = store.host_id().unwrap();
        let local = LocalSession {
            session_id: uuid::Uuid::new_v4().to_string(),
            agent: AgentKind::Codex,
            cwd: "/canonical".into(),
            resume_id: Some("native".into()),
            launcher: AgentLauncher::Opencode,
            adk_statusline: false,
        };
        store.insert_session(&local, "signed_start").unwrap();
        store
            .open_for_restore(&local.session_id, "https://server")
            .unwrap();
        drop(store);
        let conn = rusqlite::Connection::open(dir.path().join("authz.db")).unwrap();
        conn.execute_batch("DROP TABLE remote_imports; PRAGMA user_version=11;")
            .unwrap();
        drop(conn);
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(store.host_id().unwrap(), host);
        assert_eq!(
            store.restore_sessions("https://server").unwrap(),
            vec![local.clone()]
        );
        assert!(!store
            .insert_imported_session(&local, "continuation")
            .unwrap());
        assert_eq!(store.import_resume_mode(&local.session_id).unwrap(), None);
        assert!(store.can_reopen_import(&local.session_id).unwrap());
        let history = History::open(dir.path()).unwrap();
        drop(history);
        let conn = rusqlite::Connection::open(dir.path().join("history.db")).unwrap();
        conn.execute_batch("DROP TABLE imported_sessions; PRAGMA user_version=1;")
            .unwrap();
        drop(conn);
        History::open(dir.path())
            .unwrap()
            .import_metadata(&local.session_id, "Preserved name", None)
            .unwrap();
        assert_eq!(
            History::open(dir.path())
                .unwrap()
                .summary(&local.session_id)
                .unwrap()
                .title
                .as_deref(),
            Some("Preserved name")
        );
    }

    #[test]
    fn import_preserves_identity_history_and_closed_intent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut remote: RemoteSession = serde_json::from_value(serde_json::json!({
            "id": id, "agent":"claude", "title":"My saved name", "owner_email":"me@example.com",
            "cwd":dir.path(), "messages": []
        }))
        .unwrap();
        remote.messages = vec![conversations_core::parse_line(&serde_json::json!({
            "type":"user", "uuid":"u1", "message":{"role":"user","content":"original"}
        }))
        .unwrap()];
        let settings = Settings::default();
        assert!(import_one(dir.path(), "other@example.com", &remote, None, &settings).is_err());
        assert!(Store::open(dir.path())
            .unwrap()
            .session(&id)
            .unwrap()
            .is_none());
        assert!(import_one(dir.path(), "me@example.com", &remote, None, &settings).unwrap());
        assert_eq!(
            History::open(dir.path())
                .unwrap()
                .summary(&id)
                .unwrap()
                .title
                .as_deref(),
            Some("My saved name")
        );
        assert_eq!(
            Store::open(dir.path())
                .unwrap()
                .import_resume_mode(&id)
                .unwrap()
                .as_deref(),
            Some("continuation")
        );
        let mut updated = remote.messages.clone();
        updated[0].text = "newer local turn".into();
        History::open(dir.path())
            .unwrap()
            .ingest(
                &id,
                &Source {
                    agent: AgentKind::Claude,
                    native_id: id.clone(),
                    origin: "local".into(),
                },
                &updated,
            )
            .unwrap();
        import_one(dir.path(), "me@example.com", &remote, Some(&id), &settings).unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(store.restore_sessions("https://server").unwrap().is_empty());
        assert!(
            store.session(&id).unwrap().unwrap().resume_id.is_none(),
            "existing continuation binding is preserved"
        );
        assert_eq!(
            History::open(dir.path())
                .unwrap()
                .messages(&id, 10)
                .unwrap(),
            updated
        );
        let other = uuid::Uuid::new_v4().to_string();
        remote.id = other.clone();
        assert!(import_one(
            dir.path(),
            "me@example.com",
            &remote,
            Some("native-proven"),
            &settings
        )
        .unwrap());
        let local = store.session(&other).unwrap().unwrap();
        assert_eq!(local.resume_id.as_deref(), Some("native-proven"));
        assert!(
            matches!(super::super::authz::dispatch::reopen_command(&settings, &local).unwrap(), crate::commands::live::host::ControlCommand::Reopen { resume_id: Some(id), .. } if id == "native-proven")
        );
        remote.agent = AgentKind::Codex;
        remote.cwd = Some("/missing/path".into());
        import_one(dir.path(), "me@example.com", &remote, None, &settings).unwrap();
        assert_eq!(
            store.session(&other).unwrap().unwrap(),
            local,
            "never overwrite canonical binding"
        );
    }
}
