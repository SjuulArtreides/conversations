//! Owner-cookie adapters for the shared Linear reference card.
use super::super::history::History;
use super::*;
#[derive(Deserialize)]
pub(super) struct Page {
    after: Option<String>,
    limit: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Unlink {
    url: String,
}
fn known(dir: &Path, id: &str) -> Result<()> {
    anyhow::ensure!(authz::envelope::valid_session_id(id), "Invalid session id");
    anyhow::ensure!(
        Store::open(dir)?.session(id)?.is_some(),
        "Unknown local session"
    );
    Ok(())
}
pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Query(page): Query<Page>,
) -> Response {
    match tokio::task::spawn_blocking(move || -> Result<Value> {
        known(&state.dir, &id)?;
        let (docs, next) = History::open(&state.dir)?.linear_page(
            &id,
            page.after.as_deref(),
            page.limit.unwrap_or(32),
        )?;
        Ok(json!({"kind":"linear","docs":docs,"next":next}))
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        _ => error(
            StatusCode::BAD_REQUEST,
            "Could not read local Linear references.",
        ),
    }
}
pub(super) async fn unlink(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Unlink>,
) -> Response {
    match tokio::task::spawn_blocking(move || -> Result<bool> {
        known(&state.dir, &id)?;
        History::open(&state.dir)?.unlink_linear(&id, &input.url)
    })
    .await
    {
        Ok(Ok(true)) => {
            Json(json!({"status":"executed","detail":"unlinked","body":null})).into_response()
        }
        Ok(Ok(false)) => {
            Json(json!({"status":"refused","detail":"Unknown Linear reference.","body":null}))
                .into_response()
        }
        _ => error(
            StatusCode::BAD_REQUEST,
            "Could not unlink local Linear reference.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::local_host::{authz::sessions::LocalSession, history::Source};
    use conversations_core::{AgentKind, AgentLauncher};
    use std::os::unix::fs::PermissionsExt;
    #[tokio::test]
    async fn owner_linear_routes_share_history_and_enforce_cookie_and_origin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        Store::open(dir.path())
            .unwrap()
            .insert_session(
                &LocalSession {
                    session_id: session.clone(),
                    agent: AgentKind::Claude,
                    cwd: dir.path().to_string_lossy().into_owned(),
                    resume_id: Some(session.clone()),
                    launcher: AgentLauncher::Opencode,
                    adk_statusline: false,
                },
                "adopted",
            )
            .unwrap();
        let message=conversations_core::parse_line(&json!({"type":"user","uuid":"one","message":{"role":"user","content":"https://linear.app/ws/issue/ENG-1"}})).unwrap();
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
        let state =
            super::WebState::for_test(dir.path(), "https://example.com", port, "linear-owner");
        let task = tokio::spawn(async move {
            axum::serve(listener, super::super::router(state))
                .await
                .unwrap();
        });
        let base = format!("http://127.0.0.1:{port}");
        let url = format!("{base}/api/local/sessions/{session}/linear");
        let client = reqwest::Client::new();
        assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
        let data: Value = client
            .get(&url)
            .header("Cookie", "ac_local_owner=linear-owner")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(data["docs"][0]["identifier"], "ENG-1");
        assert_eq!(
            client
                .delete(&url)
                .header("Cookie", "ac_local_owner=linear-owner")
                .header("Origin", "https://evil.example")
                .json(&json!({"url":data["docs"][0]["url"]}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let removed: Value = client
            .delete(&url)
            .header("Cookie", "ac_local_owner=linear-owner")
            .header("Origin", &base)
            .json(&json!({"url":data["docs"][0]["url"]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(removed["status"], "executed");
        let data: Value = client
            .get(&url)
            .header("Cookie", "ac_local_owner=linear-owner")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(data["docs"], json!([]));
        assert_eq!(
            History::open(dir.path())
                .unwrap()
                .messages(&session, 10)
                .unwrap(),
            vec![message]
        );
        task.abort();
    }
}
