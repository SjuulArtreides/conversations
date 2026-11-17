use super::*;
use conversations_core::attention::AttentionRequest;

pub(super) async fn request(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<AttentionRequest>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        Store::open(&state.dir)?.owner_attention(&id, &input, authz::store::now_secs())
    })
    .await;
    mutation_result(result, "Could not save attention request.")
}

pub(super) async fn recipients(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    match tokio::task::spawn_blocking(move || -> Result<_> {
        let store = Store::open(&state.dir)?;
        anyhow::ensure!(store.session(&id)?.is_some(), "unknown local session");
        store.attention_recipients(&id, &conversations_core::attention::Subject::LocalOwner)
    })
    .await
    {
        Ok(Ok(recipients)) => {
            Json(json!({"kind":"attention_recipients","recipients":recipients})).into_response()
        }
        _ => error(
            StatusCode::BAD_REQUEST,
            "Could not list eligible recipients.",
        ),
    }
}
#[derive(Deserialize)]
pub(super) struct Page {
    after: Option<i64>,
    limit: Option<u32>,
}
pub(super) async fn list(State(state): State<Arc<WebState>>, Query(page): Query<Page>) -> Response {
    match tokio::task::spawn_blocking(move || -> Result<_> {
        let (mut notifications, next) = Store::open(&state.dir)?.human_notifications(
            &conversations_core::attention::Subject::LocalOwner,
            page.after,
            page.limit.unwrap_or(20),
            |_| Ok(true),
        )?;
        let history = super::super::history::History::open(&state.dir)?;
        for notification in &mut notifications {
            if let Some(title) = history.summary(&notification.session_id)?.title {
                notification.session_title = title.chars().take(200).collect();
            }
        }
        Ok((notifications, next))
    })
    .await
    {
        Ok(Ok((notifications, next))) => {
            Json(json!({"kind":"notifications","notifications":notifications,"next":next}))
                .into_response()
        }
        _ => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read notifications.",
        ),
    }
}
pub(super) async fn read(
    State(state): State<Arc<WebState>>,
    RoutePath((session, id)): RoutePath<(String, String)>,
) -> Response {
    mark(state, session, id, false).await
}
pub(super) async fn resolve(
    State(state): State<Arc<WebState>>,
    RoutePath((session, id)): RoutePath<(String, String)>,
) -> Response {
    mark(state, session, id, true).await
}
async fn mark(state: Arc<WebState>, session: String, id: String, resolve: bool) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        Store::open(&state.dir)?.owner_notification_mark(
            &session,
            &id,
            resolve,
            authz::store::now_secs(),
        )
    })
    .await;
    mutation_result(result, "Could not update notification.")
}

fn mutation_result(
    result: std::result::Result<Result<Option<&'static str>>, tokio::task::JoinError>,
    failure: &str,
) -> Response {
    match result {
        Ok(Ok(reason)) => Json(json!({
            "status": if reason.is_some() { "refused" } else { "executed" },
            "detail": reason,
            "body": null
        }))
        .into_response(),
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, failure),
    }
}
