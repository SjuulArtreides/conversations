//! Owner-cookie metadata mutations for the shared session UI.
use super::super::history::{validate_title, History};
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Rename {
    title: String,
}

pub(super) async fn rename(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Rename>,
) -> Response {
    if !authz::envelope::valid_session_id(&id) {
        return error(StatusCode::BAD_REQUEST, "Invalid session id.");
    }
    if let Err(reason) = validate_title(&input.title) {
        return error(StatusCode::BAD_REQUEST, &reason.to_string());
    }
    let dir_check = state.dir.clone();
    let id_check = id.clone();
    match tokio::task::spawn_blocking(move || -> Result<bool> {
        Ok(Store::open(&dir_check)?.session(&id_check)?.is_some())
    })
    .await
    {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return error(StatusCode::NOT_FOUND, "Unknown local session."),
        _ => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not rename this local session.",
            )
        }
    }
    // Route through the running daemon so the DB commit AND the cmux sidebar
    // push share one path with the signed `session.rename` op: same effective
    // title, same shared-workspace / stale-surface invariants. Without a
    // daemon there is no cmux to mirror into, so a DB-only commit is the
    // truthful fallback — the owner-cookie boundary is unchanged either way.
    if super::super::host_sock_present(&state.dir).await {
        match super::super::request(
            &state.dir,
            ClientMessage::OwnerRename {
                session: id.clone(),
                title: input.title.clone(),
            },
        )
        .await
        {
            Ok(ServerMessage::OwnerRenameApplied { title }) => {
                return Json(json!({"title":title})).into_response();
            }
            Ok(ServerMessage::Error { message }) => {
                return error(StatusCode::INTERNAL_SERVER_ERROR, &message);
            }
            Ok(_) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Could not rename this local session.",
                );
            }
            // The request may have committed before the connection failed.
            // A fallback write could overwrite a newer rename; let the caller retry.
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Could not confirm rename; please retry.",
                )
            }
        }
    }
    match tokio::task::spawn_blocking(move || -> Result<String> {
        History::open(&state.dir)?.rename(&id, &input.title)
    })
    .await
    {
        Ok(Ok(title)) => Json(json!({"title":title})).into_response(),
        _ => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not rename this local session.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::local_host::authz::sessions::LocalSession;
    use conversations_core::{AgentKind, AgentLauncher};
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn owner_rename_requires_cookie_origin_and_known_session_and_preserves_authority() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let record = LocalSession {
            session_id: id.clone(),
            agent: AgentKind::Claude,
            cwd: dir.path().to_string_lossy().into_owned(),
            resume_id: Some(id.clone()),
            launcher: AgentLauncher::Opencode,
            adk_statusline: false,
        };
        store.insert_session(&record, "adopted").unwrap();
        let reader = store
            .enroll(
                authz::store::Kind::Owner,
                Alg::Ed25519,
                &[7; 32],
                "Existing reader",
                &[authz::store::Grant::parse("history.read").unwrap()],
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state =
            super::WebState::for_test(dir.path(), "https://example.com", port, "test-owner-cookie");
        let task = tokio::spawn(async move {
            axum::serve(listener, super::super::router(state))
                .await
                .unwrap();
        });
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        let url = format!("{base}/api/local/sessions/{id}/rename");
        let request = || client.post(&url).json(&json!({"title":"Renamed locally"}));
        assert_eq!(
            request()
                .header("Origin", &base)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            request()
                .header("Cookie", "ac_local_owner=test-owner-cookie")
                .header("Origin", "https://evil.example")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let response = request()
            .header("Cookie", "ac_local_owner=test-owner-cookie")
            .header("Origin", &base)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.json::<Value>().await.unwrap()["title"],
            "Renamed locally"
        );
        // A connection can fail after the daemon has received a rename. Do
        // not replay an ambiguous old write directly into the database.
        let socket = tokio::net::UnixListener::bind(dir.path().join("host.sock")).unwrap();
        let fixture_dir = dir.path().to_owned();
        let fixture_id = id.clone();
        let disconnect = tokio::spawn(async move {
            let (mut stream, _) = socket.accept().await.unwrap();
            let _: ClientMessage = super::super::super::protocol::read_message(&mut stream)
                .await
                .unwrap();
            // Simulate a newer rename committed while the first reply was lost.
            History::open(&fixture_dir)
                .unwrap()
                .rename(&fixture_id, "Newer title")
                .unwrap();
        });
        let response = client
            .post(&url)
            .header("Cookie", "ac_local_owner=test-owner-cookie")
            .header("Origin", &base)
            .json(&json!({"title":"Ambiguous old rename"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        disconnect.await.unwrap();
        assert_eq!(
            History::open(dir.path())
                .unwrap()
                .summary(&id)
                .unwrap()
                .title
                .as_deref(),
            Some("Newer title")
        );

        let unknown = uuid::Uuid::new_v4().to_string();
        let response = client
            .post(format!("{base}/api/local/sessions/{unknown}/rename"))
            .header("Cookie", "ac_local_owner=test-owner-cookie")
            .header("Origin", &base)
            .json(&json!({"title":"Unknown"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        assert!(store.session(&unknown).unwrap().is_none());
        assert_eq!(store.session(&id).unwrap(), Some(record));
        assert!(store
            .restore_sessions("https://example.com")
            .unwrap()
            .is_empty());
        let conn = rusqlite::Connection::open(dir.path().join("authz.db")).unwrap();
        let grants: Vec<(String, String, Option<i64>)> = conn
            .prepare("SELECT cap,session_id,revoked_at FROM grants WHERE cred_id=?1")
            .unwrap()
            .query_map([reader], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(grants, vec![("history.read".into(), "*".into(), None)]);
        assert!(History::open(dir.path())
            .unwrap()
            .messages(&id, 10)
            .unwrap()
            .is_empty());
        task.abort();
    }
}
