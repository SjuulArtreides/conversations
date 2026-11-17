//! Owner-local adapter for the shared PR rail.
use super::super::{github, pr_metadata};
use super::{authz, *};

#[derive(Deserialize)]
pub(super) struct Cursor {
    #[serde(default)]
    offset: u64,
    sha256: Option<String>,
}
fn known(state: &WebState, id: &str) -> Result<()> {
    anyhow::ensure!(authz::envelope::valid_session_id(id), "Invalid session id.");
    anyhow::ensure!(
        Store::open(&state.dir)?.session(id)?.is_some(),
        "Unknown local session."
    );
    Ok(())
}
pub(super) async fn list(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Query(cursor): Query<Cursor>,
) -> Response {
    match tokio::task::spawn_blocking(move || -> Result<_> {
        known(&state, &id)?;
        let (b64, sha256, next) =
            pr_metadata::chunk(&state.dir, &id, cursor.offset, cursor.sha256.as_deref())?;
        Ok(conversations_core::private_channel::ReplyBody::PrCache { b64, sha256, next })
    })
    .await
    {
        Ok(Ok(body)) => Json(body).into_response(),
        Ok(Err(reason)) => error(StatusCode::CONFLICT, &reason.to_string()),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read local PR cache.",
        ),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Target {
    owner: String,
    repo: String,
    number: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Comment {
    owner: String,
    repo: String,
    number: i64,
    body: String,
    in_reply_to: Option<i64>,
}
fn response(outcome: std::result::Result<Result<String>, tokio::task::JoinError>) -> Response {
    let (status, detail) = match outcome {
        Ok(Ok(detail)) => ("executed", detail),
        Ok(Err(error)) => ("failed", error.to_string()),
        Err(_) => ("failed", "Local PR action failed.".into()),
    };
    Json(json!({"status":status,"detail":detail,"body":null})).into_response()
}
pub(super) async fn sync(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(target): Json<Target>,
) -> Response {
    response(
        tokio::task::spawn_blocking(move || {
            known(&state, &id)?;
            // refresh_local acquires the per-target lock internally.
            github::refresh_local(
                &state.dir,
                &id,
                &target.owner,
                &target.repo,
                target.number,
                true,
            )?;
            Ok("PR status and comments refreshed locally".into())
        })
        .await,
    )
}
pub(super) async fn comment(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Comment>,
) -> Response {
    response(
        tokio::task::spawn_blocking(move || {
            known(&state, &id)?;
            github::post_comment(
                &input.owner,
                &input.repo,
                input.number,
                &input.body,
                input.in_reply_to,
            )?;
            // refresh_local acquires the per-target lock internally.
            let refreshed = github::refresh_local(
                &state.dir,
                &id,
                &input.owner,
                &input.repo,
                input.number,
                true,
            );
            Ok(if refreshed.is_ok() {
                "comment posted; local PR cache refreshed"
            } else {
                "comment posted; local PR cache refresh failed"
            }
            .into())
        })
        .await,
    )
}
pub(super) async fn unlink(
    State(state): State<Arc<WebState>>,
    RoutePath(id): RoutePath<String>,
    Json(target): Json<Target>,
) -> Response {
    response(
        tokio::task::spawn_blocking(move || {
            known(&state, &id)?;
            github::validate(&target.owner, &target.repo, target.number)?;
            pr_metadata::unlink(&state.dir, &id, &target.owner, &target.repo, target.number)?;
            Ok("unlinked".into())
        })
        .await,
    )
}
