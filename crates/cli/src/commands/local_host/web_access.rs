//! Explicit owner-local publication and session-key management.
use super::super::{publication, settings::Settings};
use super::*;
use authz::envelope::Cap;

fn result(status: &str, detail: impl Into<String>) -> Response {
    Json(json!({"status":status,"detail":detail.into(),"body":null})).into_response()
}
fn known(state: &WebState, id: &str) -> Result<()> {
    anyhow::ensure!(authz::envelope::valid_session_id(id), "Invalid session id.");
    anyhow::ensure!(
        Store::open(&state.dir)?.session(id)?.is_some(),
        "Unknown local session."
    );
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Share {
    audience: publication::Audience,
}
pub(super) async fn status(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    publication_action(state, id, None, false).await
}
pub(super) async fn share(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Share>,
) -> Response {
    publication_action(state, id, Some(input.audience), true).await
}
pub(super) async fn unshare(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    publication_action(state, id, None, true).await
}
async fn publication_action(
    state: Arc<WebState>,
    id: String,
    audience: Option<publication::Audience>,
    change: bool,
) -> Response {
    match tokio::task::spawn_blocking(move || -> Result<String> {
        known(&state, &id)?;
        let status = match (change, audience) {
            (true, Some(audience)) => {
                let settings = Settings::load(&state.dir)?;
                let owner = publication::configured_owner(settings.publication.account.as_deref())?;
                publication::share(&state.dir, &id, audience, owner.as_deref(), &state.server)?
            },
            (true, None) => publication::unshare(&state.dir, &id)?,
            (false, _) => publication::status(&state.dir, &id)?,
        };
        if change { Ok(status.state.as_str().to_owned()) }
        else { Ok(serde_json::to_string(&json!({"state":status.state,"rev":status.rev,"audience":status.audience,"published":status.published,"pending":status.pending}))?) }
    }).await {
        Ok(Ok(detail)) => result("executed", detail),
        Ok(Err(error)) => result("refused", error.to_string()),
        Err(_) => result("failed", "Could not update publication."),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Grants {
    recipient: String,
    caps: Vec<String>,
}
pub(super) async fn add(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Grants>,
) -> Response {
    change_grants(state, id, input, false).await
}
pub(super) async fn remove(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Grants>,
) -> Response {
    change_grants(state, id, input, true).await
}
async fn change_grants(state: Arc<WebState>, id: String, input: Grants, remove: bool) -> Response {
    let dir = state.dir.clone();
    let changed = tokio::task::spawn_blocking(move || -> Result<Option<&'static str>> {
        let Some(caps) = input
            .caps
            .iter()
            .map(|name| Cap::parse(name))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(Some("Unknown capability."));
        };
        Store::open(&dir)?.owner_change_grants(
            &id,
            &input.recipient,
            &caps,
            remove,
            chrono::Utc::now().timestamp(),
        )
    })
    .await;
    match changed {
        Ok(Ok(None)) => {
            if remove {
                let interruptions = match super::super::request(
                    &state.dir,
                    ClientMessage::AuthorityChanged,
                )
                .await
                {
                    Ok(ServerMessage::Interruptions { sessions }) => Ok(sessions),
                    _ => Err(anyhow::anyhow!(
                        "local host did not report interruption outcomes"
                    )),
                };
                result(
                    "executed",
                    super::super::daemon::removal_report(interruptions),
                )
            } else {
                result("executed", "granted")
            }
        }
        Ok(Ok(Some(reason))) => result("refused", reason),
        _ => result("failed", "Could not update key access."),
    }
}
pub(super) async fn grants(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    match tokio::task::spawn_blocking(move || -> Result<Value> {
        known(&state, &id)?;
        let mut status = local_status(&state)?;
        let credentials = status["credentials"]
            .as_array_mut()
            .context("missing credentials")?;
        for credential in credentials.iter_mut() {
            if let Some(grants) = credential["grants"].as_array_mut() {
                grants.retain(|grant| grant["session"] == id || grant["session"] == "*");
            }
        }
        Ok(json!({"credentials":credentials}))
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        _ => error(StatusCode::NOT_FOUND, "Unknown local session."),
    }
}
