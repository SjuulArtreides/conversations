//! Owner-local project metadata. Session contents and authority stay in their own stores.
use super::super::history::{validate_title, History};
use super::*;
#[cfg(test)]
use conversations_core::Project;
use rusqlite::{params, OptionalExtension};

use super::super::projects::Projects;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectInput {
    name: String,
    description: Option<String>,
}
impl ProjectInput {
    fn validated(&self) -> Result<(&str, Option<&str>)> {
        let name = validate_title(&self.name)?.context("project name must not be empty")?;
        Ok((
            name,
            self.description
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        ))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Membership {
    project_id: Option<String>,
}
async fn blocking(f: impl FnOnce() -> Result<Response> + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(response)) => response,
        _ => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not access local project metadata.",
        ),
    }
}
pub(super) async fn list(State(state): State<Arc<WebState>>) -> Response {
    blocking(move || {
        Ok(Json(json!({"projects":Projects::open(&state.dir)?.list()?})).into_response())
    })
    .await
}
pub(super) async fn create(
    State(state): State<Arc<WebState>>,
    Json(input): Json<ProjectInput>,
) -> Response {
    if let Err(reason) = input.validated() {
        return error(StatusCode::BAD_REQUEST, &reason.to_string());
    }
    blocking(move || {
        let (name, description) = input.validated()?;
        Ok((
            StatusCode::CREATED,
            Json(Projects::open(&state.dir)?.create(name, description)?),
        )
            .into_response())
    })
    .await
}
pub(super) async fn detail(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    blocking(move || {
        let projects=Projects::open(&state.dir)?;
        let Some(mut project)=projects.get(&id)? else {return Ok(error(StatusCode::NOT_FOUND,"Unknown local project."));};
        let store=Store::open(&state.dir)?;
        let history=History::open(&state.dir)?;
        let mut sessions=Vec::new();
        for id in projects.members(&id)? {
            let Some(session)=store.session(&id)? else {continue;};
            let summary=history.summary(&id)?;
            let title=summary.title.unwrap_or_else(||format!("{} · {}",session.agent.as_str(),Path::new(&session.cwd).file_name().and_then(|name|name.to_str()).unwrap_or("Local session")));
            sessions.push((summary.last_updated,json!({"id":id,"title":title,"git_branch":null,"project_path":session.cwd,"visibility":"private"})));
        }
        sessions.sort_by_key(|row| std::cmp::Reverse(row.0));
        project.session_count=sessions.len();
        let sessions:Vec<_>=sessions.into_iter().map(|(_,session)|session).collect();
        Ok(Json(json!({"project":project,"sessions":sessions})).into_response())
    }).await
}
pub(super) async fn update(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<ProjectInput>,
) -> Response {
    if let Err(reason) = input.validated() {
        return error(StatusCode::BAD_REQUEST, &reason.to_string());
    }
    blocking(move || {
        let projects = Projects::open(&state.dir)?;
        let (name, description) = input.validated()?;
        let changed = projects.conn.execute(
            "UPDATE projects SET name=?2,description=?3,updated_at=?4 WHERE id=?1",
            params![id, name, description, chrono::Utc::now().to_rfc3339()],
        )?;
        if changed == 0 {
            return Ok(error(StatusCode::NOT_FOUND, "Unknown local project."));
        }
        Ok(Json(projects.get(&id)?.context("updated project disappeared")?).into_response())
    })
    .await
}
pub(super) async fn delete(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    blocking(move || {
        let changed = Projects::open(&state.dir)?
            .conn
            .execute("DELETE FROM projects WHERE id=?1", [id])?;
        Ok(if changed == 0 {
            error(StatusCode::NOT_FOUND, "Unknown local project.")
        } else {
            StatusCode::NO_CONTENT.into_response()
        })
    })
    .await
}
pub(super) async fn membership(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if !authz::envelope::valid_session_id(&id) {
        return error(StatusCode::BAD_REQUEST, "Invalid session id.");
    }
    blocking(move || {
        if Store::open(&state.dir)?.session(&id)?.is_none() {
            return Ok(error(StatusCode::NOT_FOUND, "Unknown local session."));
        }
        let project_id: Option<String> = Projects::open(&state.dir)?
            .conn
            .query_row(
                "SELECT project_id FROM project_sessions WHERE session_id=?1",
                [&id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(Json(json!({"project_id":project_id})).into_response())
    })
    .await
}
pub(super) async fn assign(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Membership>,
) -> Response {
    if !authz::envelope::valid_session_id(&id) {
        return error(StatusCode::BAD_REQUEST, "Invalid session id.");
    }
    blocking(move || {
        if Store::open(&state.dir)?.session(&id)?.is_none() {
            return Ok(error(StatusCode::NOT_FOUND, "Unknown local session."));
        }
        let mut projects = Projects::open(&state.dir)?;
        if let Some(project) = &input.project_id {
            if projects.get(project)?.is_none() {
                return Ok(error(StatusCode::BAD_REQUEST, "Unknown local project."));
            }
        }
        projects.assign(&id, input.project_id.as_deref())?;
        Ok(Json(json!({"project_id":input.project_id})).into_response())
    })
    .await
}
/// This local host has no LLM service configured. The shared UI already handles that capability.
pub(super) async fn llm_status() -> Json<Value> {
    Json(json!({"enabled":false,"model":null}))
}

#[cfg(test)]
mod tests {
    use super::super::super::authz::sessions::LocalSession;
    use super::super::super::history::Source;
    use super::*;
    use conversations_core::{AgentKind, AgentLauncher};
    use std::os::unix::fs::PermissionsExt;
    #[tokio::test]
    async fn local_project_crud_and_membership_are_persistent_owner_only_and_never_change_sessions()
    {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let record = LocalSession {
            session_id: session.clone(),
            agent: AgentKind::Claude,
            cwd: "/actual/local/project".into(),
            resume_id: Some(session.clone()),
            launcher: AgentLauncher::Opencode,
            adk_statusline: false,
        };
        store.insert_session(&record, "adopted").unwrap();
        let message=conversations_core::parse_line(&json!({"type":"user","uuid":"turn1","message":{"role":"user","content":"Original private prompt"}})).unwrap();
        History::open(dir.path())
            .unwrap()
            .ingest(
                &session,
                &Source {
                    agent: AgentKind::Claude,
                    native_id: session.clone(),
                    origin: "local".into(),
                },
                std::slice::from_ref(&message),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = super::WebState::for_test(
            dir.path(),
            "https://example.com",
            port,
            "test-project-owner",
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, super::super::router(state))
                .await
                .unwrap();
        });
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        let request = |method, path: &str| {
            client
                .request(method, format!("{base}{path}"))
                .header("Origin", &base)
                .header("Cookie", "ac_local_owner=test-project-owner")
        };
        assert_eq!(
            client
                .get(format!("{base}/api/projects"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .post(format!("{base}/api/projects"))
                .header("Cookie", "ac_local_owner=test-project-owner")
                .header("Origin", "https://evil.example")
                .json(&json!({"name":"Forbidden"}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let mut ids = Vec::new();
        for name in ["First project", "Second project"] {
            let response = request(reqwest::Method::POST, "/api/projects")
                .json(&json!({"name":name,"description":"Local grouping"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 201);
            let project: Project = response.json().await.unwrap();
            assert_eq!(project.owner_email, None);
            ids.push(project.id);
        }
        let member_url = format!("/api/sessions/{session}/project");
        for (target, other) in [(&ids[0], &ids[1]), (&ids[1], &ids[0])] {
            assert_eq!(
                request(reqwest::Method::PUT, &member_url)
                    .json(&json!({"project_id":target}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
            let membership: Value = request(reqwest::Method::GET, &member_url)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(membership["project_id"], *target);
            let detail: Value = request(reqwest::Method::GET, &format!("/api/projects/{target}"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(detail["project"]["session_count"], 1);
            assert_eq!(detail["sessions"][0]["id"], session);
            assert_eq!(detail["sessions"][0]["visibility"], "private");
            assert!(detail["sessions"][0].get("uploaded_by").is_none());
            let other: Value = request(reqwest::Method::GET, &format!("/api/projects/{other}"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(other["project"]["session_count"], 0);
        }
        let unknown = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            request(
                reqwest::Method::PUT,
                &format!("/api/sessions/{unknown}/project")
            )
            .json(&json!({"project_id":ids[0]}))
            .send()
            .await
            .unwrap()
            .status(),
            404
        );
        assert_eq!(
            request(reqwest::Method::PUT, &member_url)
                .json(&json!({"project_id":unknown}))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        let renamed: Project = request(reqwest::Method::PUT, &format!("/api/projects/{}", ids[1]))
            .json(&json!({"name":"Updated project","description":"Updated description"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(renamed.name, "Updated project");
        assert_eq!(renamed.session_count, 1);
        assert_eq!(
            request(
                reqwest::Method::DELETE,
                &format!("/api/projects/{}", ids[1])
            )
            .send()
            .await
            .unwrap()
            .status(),
            204
        );
        assert_eq!(
            request(reqwest::Method::GET, &format!("/api/projects/{}", ids[1]))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        let list: Value = request(reqwest::Method::GET, "/api/projects")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["projects"].as_array().unwrap().len(), 1);
        assert!(Projects::open(dir.path())
            .unwrap()
            .members(&ids[1])
            .unwrap()
            .is_empty());
        assert_eq!(store.session(&session).unwrap(), Some(record));
        assert!(store.session(&unknown).unwrap().is_none());
        assert_eq!(
            History::open(dir.path())
                .unwrap()
                .messages(&session, 10)
                .unwrap(),
            vec![message]
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("projects.db"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        task.abort();
    }
}
