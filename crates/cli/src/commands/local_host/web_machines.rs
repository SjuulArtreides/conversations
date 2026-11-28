//! Local-owner machine pairing. The owner compares a machine's fingerprint
//! here; the server only lists which machines the account attached.
use super::*;
use crate::commands::local_host::machines;

fn net_parts(state: &WebState) -> (reqwest::Client, Option<String>) {
    (
        reqwest::Client::new(),
        crate::credentials::cookie_for(&state.server),
    )
}

/// `GET /api/machines`: this machine's fingerprint and the account's machines.
pub(super) async fn list(State(state): State<Arc<WebState>>) -> Response {
    let (http, cookie) = net_parts(&state);
    let net = machines::Net {
        http: &http,
        server: &state.server,
        cookie: cookie.as_deref(),
    };
    let own = {
        let dir = state.dir.clone();
        tokio::task::spawn_blocking(move || authz::host_id(&dir)).await
    };
    let Ok(Ok(own)) = own else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read this machine's identity.",
        );
    };
    let this = json!({
        "host_id": own,
        "fingerprint": machines::fingerprint(&own),
        "label": machines::machine_name(),
    });
    match machines::account_machines(&net, &state.dir).await {
        Ok(listed) => Json(json!({ "this": this, "machines": listed })).into_response(),
        Err(_) => Json(json!({
            "this": this,
            "machines": [],
            "error": "Could not load your account's machines. Check your saved server login.",
        }))
        .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pair {
    host_id: String,
    compared: bool,
}

/// `POST /api/machines/pair`: pair a listed machine after the owner compared
/// its fingerprint.
pub(super) async fn pair(State(state): State<Arc<WebState>>, Json(input): Json<Pair>) -> Response {
    if !input.compared {
        return error(
            StatusCode::BAD_REQUEST,
            "Compare the fingerprint with the one that machine shows before pairing it.",
        );
    }
    let (http, cookie) = net_parts(&state);
    let net = machines::Net {
        http: &http,
        server: &state.server,
        cookie: cookie.as_deref(),
    };
    match machines::pair_listed(&net, &state.dir, &input.host_id).await {
        Ok(outcome) => Json(json!(outcome)).into_response(),
        Err(err) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Unpair {
    host_id: String,
}

/// `POST /api/machines/unpair`: unpair here, tell the other machine, and
/// interrupt work it started here.
pub(super) async fn unpair(
    State(state): State<Arc<WebState>>,
    Json(input): Json<Unpair>,
) -> Response {
    let (http, cookie) = net_parts(&state);
    let net = machines::Net {
        http: &http,
        server: &state.server,
        cookie: cookie.as_deref(),
    };
    match machines::unpair(&net, &state.dir, &input.host_id).await {
        Ok(unpaired) => {
            let _ = super::super::request(&state.dir, ClientMessage::AuthorityChanged).await;
            Json(json!({
                "label": unpaired.label,
                "other": match unpaired.other {
                    Ok(status) => json!({ "told": true, "status": status }),
                    Err(_) => json!({ "told": false }),
                },
            }))
            .into_response()
        }
        Err(err) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
    }
}
