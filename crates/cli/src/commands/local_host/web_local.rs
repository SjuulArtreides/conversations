//! Owner-only adapter for the shared SPA's private-channel data shapes.
use super::super::{history::History, protocol::LocalAction, settings::Settings};
use super::*;
use conversations_core::private_channel::{ReplyBody, SearchHit, SessionEntry};

pub(super) async fn marker(
    State(state): State<Arc<WebState>>,
    headers: axum::http::HeaderMap,
) -> Json<Value> {
    Json(json!({"local":true,"authenticated":has_owner_cookie(&state,&headers)}))
}
#[derive(Deserialize)]
pub(super) struct Page {
    #[serde(default)]
    running_only: bool,
    #[serde(default)]
    readiness: bool,
    after: Option<String>,
    limit: Option<u32>,
}
pub(super) async fn list(State(state): State<Arc<WebState>>, Query(page): Query<Page>) -> Response {
    let status = match super::super::request(&state.dir, ClientMessage::Status).await {
        Ok(ServerMessage::Status { sessions, .. }) => sessions
            .into_iter()
            .map(|s| (s.registration.launch.id, s.readiness))
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let running: Vec<_> = status.iter().map(|(id, _)| id.clone()).collect();
    blocking(move || {
        let store = Store::open(&state.dir)?;
        let (rows, next) = store.owner_sessions(
            page.after.as_deref(),
            page.limit.unwrap_or(50),
            page.running_only.then_some(running.as_slice()),
        )?;
        let history = History::open(&state.dir)?;
        let sessions = rows
            .into_iter()
            .map(|row| {
                let summary = history.summary(&row.session_id)?;
                Ok(SessionEntry {
                    readiness: page
                        .readiness
                        .then(|| {
                            status
                                .iter()
                                .find(|(id, _)| id == &row.session_id)
                                .and_then(|(_, readiness)| *readiness)
                        })
                        .flatten(),
                    running: running.contains(&row.session_id),
                    reopenable: Some(store.can_reopen_import(&row.session_id)?),
                    resume_mode: store.import_resume_mode(&row.session_id)?,
                    prs: history.listed_prs(&row.session_id)?,
                    id: row.session_id,
                    agent: row.agent,
                    cwd: row.cwd,
                    title: summary.title,
                    messages: summary.messages,
                    last_activity: summary
                        .last_updated
                        .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
                        .map(|at| at.to_rfc3339()),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ReplyBody::Sessions { sessions, next })
    })
    .await
}
#[derive(Deserialize)]
pub(super) struct HistoryQuery {
    after: Option<u64>,
    limit: Option<u32>,
    digest: Option<bool>,
}
pub(super) async fn history(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    blocking(move || {
        valid(&id)?;
        let (items, next) = History::open(&state.dir)?.page(
            &id,
            q.after.unwrap_or(0),
            q.limit
                .unwrap_or(32)
                .clamp(1, if q.digest.unwrap_or(false) { 256 } else { 32 }),
            40 * 1024,
            q.digest.unwrap_or(false),
        )?;
        Ok(ReplyBody::History { items, next })
    })
    .await
}
#[derive(Deserialize)]
pub(super) struct FragmentQuery {
    seq: u64,
    offset: u64,
}
pub(super) async fn fragment(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Query(q): Query<FragmentQuery>,
) -> Response {
    blocking(move || {
        valid(&id)?;
        let p = History::open(&state.dir)?
            .fragment(&id, q.seq, q.offset)?
            .context("message not found")?;
        Ok(ReplyBody::Fragment {
            seq: q.seq,
            offset: q.offset,
            data: URL_SAFE_NO_PAD.encode(p.data),
            size: p.size,
            sha256: p.sha256,
            next: p.next,
        })
    })
    .await
}
#[derive(Deserialize)]
pub(super) struct SearchQuery {
    query: String,
    limit: Option<u32>,
}
pub(super) async fn search(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Query(q): Query<SearchQuery>,
) -> Response {
    blocking(move || {
        valid(&id)?;
        anyhow::ensure!(q.query.len() <= 256, "search too long");
        let hits = History::open(&state.dir)?
            .search(
                &q.query,
                Some(&id),
                q.limit.unwrap_or(20).clamp(1, 20) as usize,
            )?
            .into_iter()
            .map(|h| SearchHit {
                seq: h.seq,
                uuid: h.uuid,
                snippet: h.snippet,
            })
            .collect();
        Ok(ReplyBody::Search { hits })
    })
    .await
}
pub(super) async fn settings(State(state): State<Arc<WebState>>) -> Response {
    blocking(move || {
        Ok(ReplyBody::Settings {
            launch: serde_json::to_value(Settings::load(&state.dir)?.launch)?,
        })
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LaunchSettings {
    launch: super::super::settings::LaunchSettings,
}
pub(super) async fn save_settings(
    State(state): State<Arc<WebState>>,
    Json(input): Json<LaunchSettings>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        super::super::settings::replace_launch(&state.dir, input.launch)
    })
    .await;
    outcome(match result {
        Ok(r) => r,
        Err(e) => Err(e.into()),
    })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Start {
    session: String,
    agent: conversations_core::AgentKind,
    launcher: Option<conversations_core::AgentLauncher>,
}
pub(super) async fn start(
    State(state): State<Arc<WebState>>,
    Json(input): Json<Start>,
) -> Response {
    action(
        &state,
        LocalAction::Start {
            session: input.session,
            agent: input.agent,
            launcher: input.launcher,
        },
    )
    .await
}
pub(super) async fn reopen(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    action(&state, LocalAction::Reopen { session: id }).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reply {
    text: String,
}
pub(super) async fn reply(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Reply>,
) -> Response {
    action(
        &state,
        LocalAction::Reply {
            session: id,
            text: input.text,
        },
    )
    .await
}
async fn action(state: &WebState, action: LocalAction) -> Response {
    let result =
        match super::super::request(&state.dir, ClientMessage::LocalAction { action }).await {
            Ok(ServerMessage::Started { .. }) => Ok(()),
            Ok(ServerMessage::Error { message }) => Err(anyhow::anyhow!(message)),
            _ => Err(anyhow::anyhow!(
                "The local host did not confirm the action. Check the session before retrying."
            )),
        };
    outcome(result)
}
fn outcome(result: Result<()>) -> Response {
    match result {
        Ok(()) => Json(json!({"status":"executed","detail":null,"body":null})).into_response(),
        Err(e) => {
            Json(json!({"status":"failed","detail":e.to_string(),"body":null})).into_response()
        }
    }
}
fn valid(id: &str) -> Result<()> {
    anyhow::ensure!(
        super::super::authz::envelope::valid_session_id(id),
        "invalid session id"
    );
    Ok(())
}
async fn blocking(f: impl FnOnce() -> Result<ReplyBody> + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(body)) => Json(body).into_response(),
        _ => error(
            StatusCode::BAD_REQUEST,
            "Could not read this local resource.",
        ),
    }
}

pub(super) async fn end(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    action(&state, LocalAction::End { session: id }).await
}
pub(super) async fn preferences(State(state): State<Arc<WebState>>) -> Response {
    match tokio::task::spawn_blocking(move || Settings::load(&state.dir).map(|s| s.ui)).await {
        Ok(Ok(p)) => Json(p).into_response(),
        _ => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read local preferences.",
        ),
    }
}
pub(super) async fn save_preferences(
    State(state): State<Arc<WebState>>,
    Json(input): Json<super::super::settings::LocalPreferences>,
) -> Response {
    match tokio::task::spawn_blocking(move || {
        super::super::settings::replace_preferences(&state.dir, input.clone())?;
        Ok::<_, anyhow::Error>(input)
    })
    .await
    {
        Ok(Ok(p)) => Json(p).into_response(),
        _ => error(StatusCode::BAD_REQUEST, "Could not save local preferences."),
    }
}

#[derive(Deserialize)]
pub(super) struct MigrationQuery {
    transfer: String,
    offset: u64,
}
pub(super) async fn migration_export(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Query(q): Query<MigrationQuery>,
) -> Response {
    blocking(move || {
        super::super::migration::export(&state.dir, "local-owner", &id, &q.transfer, q.offset)
    })
    .await
}
pub(super) async fn migration_import(
    State(state): State<Arc<WebState>>,
    Json(input): Json<conversations_core::migration::MigrationImport>,
) -> Response {
    match tokio::task::spawn_blocking(move || super::super::migration::import(&state.dir, "local-owner", &input)).await {
        Ok(Ok(done)) => Json(json!({"status":"executed","detail":if done {"copied"} else {"receiving"},"body":null})).into_response(),
        _ => error(StatusCode::BAD_REQUEST,"Could not import the session. Check the destination folder, native session conflicts, and host logs."),
    }
}
