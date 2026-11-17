//! Owner-cookie transport for the shared read-only communication rail.
use super::*;
use conversations_core::private_channel::ReplyBody;

#[derive(Deserialize)]
pub(super) struct Page {
    after: Option<String>,
    limit: Option<u32>,
}

pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Query(page): Query<Page>,
) -> Response {
    let running = match super::super::request(&state.dir, ClientMessage::Status).await {
        Ok(ServerMessage::Status { sessions, .. }) => Some(
            sessions
                .into_iter()
                .map(|s| s.registration.launch.id)
                .collect::<Vec<_>>(),
        ),
        _ => None,
    };
    match tokio::task::spawn_blocking(move || -> Result<ReplyBody> {
        let store = Store::open(&state.dir)?;
        anyhow::ensure!(authz::envelope::valid_session_id(&id), "Invalid session id");
        let page = super::super::communication::list(
            &state.dir,
            &id,
            page.after.as_deref(),
            page.limit.unwrap_or(32),
            |session| Ok(store.session(session)?.is_some()),
            running.as_deref(),
        )?;
        Ok(ReplyBody::Communications {
            peers: page.peers,
            exchanges: page.exchanges,
            next: page.next,
        })
    })
    .await
    {
        Ok(Ok(body)) => Json(body).into_response(),
        _ => error(
            StatusCode::BAD_REQUEST,
            "Could not read local communications.",
        ),
    }
}
