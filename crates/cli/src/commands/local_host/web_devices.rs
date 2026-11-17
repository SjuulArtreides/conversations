//! Local-owner device enrollment; the host remains the authority.
use super::*;
use crate::commands::local_host::authz::passkey::{self, PasskeyResponse};

const LABEL: &str = "Conversations phone";
const GRANTS: &[&str] = &[
    "history.read",
    "session.write",
    "session.start",
    "session.reopen",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeviceId {
    cred_id: String,
}

fn failure(err: anyhow::Error) -> Response {
    error(StatusCode::BAD_REQUEST, &err.to_string())
}

pub(super) async fn configure(State(state): State<Arc<WebState>>) -> Response {
    let dir = state.dir.clone();
    let server = state.server.clone();
    match tokio::task::spawn_blocking(move || {
        super::super::settings::configure_device_passkeys(&dir, &server)
    })
    .await
    {
        Ok(Ok(())) => Json(json!({"ok":true})).into_response(),
        Ok(Err(err)) => failure(err),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not configure passkeys.",
        ),
    }
}

pub(super) async fn start(State(state): State<Arc<WebState>>) -> Response {
    let dir = state.dir.clone();
    let server = state.server.clone();
    match tokio::task::spawn_blocking(move || -> Result<String> {
        let settings = super::super::settings::Settings::load(&dir)?;
        let (webauthn, rp_id) = passkey::relying_party(&settings)?;
        let origin = reqwest::Url::parse(&server)?.origin().ascii_serialization();
        anyhow::ensure!(settings.webauthn.origins.iter().any(|allowed| allowed == &origin), "This server is not in the host's configured passkey origins. Use the configured server on your laptop.");
        let mut store = Store::open(&dir)?;
        let grants: Vec<String> = GRANTS.iter().map(|s| (*s).to_owned()).collect();
        let PasskeyResponse::Started { challenge, options } = passkey::enroll_start(
            &mut store,
            &webauthn,
            &rp_id,
            LABEL,
            "owner",
            &grants,
            authz::store::now_secs(),
        )?
        else {
            anyhow::bail!("unexpected enrollment response")
        };
        let descriptor = authz::attest::descriptor(&dir)?;
        passkey::enrollment_link(&server, &descriptor, &challenge, &options)
    })
    .await
    {
        Ok(Ok(link)) => Json(json!({"link":link})).into_response(),
        Ok(Err(err)) => failure(err),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not start device setup.",
        ),
    }
}

pub(super) async fn pending(State(state): State<Arc<WebState>>) -> Response {
    let dir = state.dir.clone();
    match tokio::task::spawn_blocking(move || -> Result<Vec<passkey::PendingCredential>> {
        let PasskeyResponse::Pending { pending } = passkey::pending(&Store::open(&dir)?)? else {
            anyhow::bail!("unexpected pending response")
        };
        Ok(pending
            .into_iter()
            .filter(|p| p.label == LABEL && p.kind == "owner" && p.grants == GRANTS)
            .collect())
    })
    .await
    {
        Ok(Ok(devices)) => Json(json!({"devices":devices})).into_response(),
        Ok(Err(err)) => failure(err),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not load pending devices.",
        ),
    }
}

async fn decide(state: Arc<WebState>, id: String, approve: bool) -> Response {
    if id.len() != 43
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return error(StatusCode::BAD_REQUEST, "Invalid device identifier.");
    }
    let dir = state.dir.clone();
    match tokio::task::spawn_blocking(move || -> Result<()> {
        let mut store = Store::open(&dir)?;
        let PasskeyResponse::Pending { pending } = passkey::pending(&store)? else {
            anyhow::bail!("unexpected pending response")
        };
        anyhow::ensure!(
            pending.iter().any(|p| p.cred_id == id
                && p.label == LABEL
                && p.kind == "owner"
                && p.grants == GRANTS),
            "No matching pending device."
        );
        if approve {
            passkey::approve(&mut store, &id, authz::store::now_secs())?;
        } else {
            let conn = authz::store::open_private(&dir, "authz.db")?;
            let changed = conn.execute(
                "DELETE FROM pending_credentials WHERE cred_id=?1 AND label=?2 AND kind='owner'",
                rusqlite::params![id, LABEL],
            )?;
            anyhow::ensure!(changed == 1, "No matching pending device.");
        }
        Ok(())
    })
    .await
    {
        Ok(Ok(())) => Json(json!({"ok":true})).into_response(),
        Ok(Err(err)) => failure(err),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not update device.",
        ),
    }
}
pub(super) async fn approve(
    State(state): State<Arc<WebState>>,
    Json(body): Json<DeviceId>,
) -> Response {
    decide(state, body.cred_id, true).await
}
pub(super) async fn reject(
    State(state): State<Arc<WebState>>,
    Json(body): Json<DeviceId>,
) -> Response {
    decide(state, body.cred_id, false).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn device_management_requires_local_owner_and_explicit_approval() {
        const CHILD: &str = "CONVERSATIONS_DEVICE_UI_TEST";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "commands::local_host::web::devices::tests::device_management_requires_local_owner_and_explicit_approval", "--nocapture"])
                .env(CHILD, "1").env("HOME", home.path())
                .env("CONVERSATIONS_HOST_SETTINGS_FILE", home.path().join("settings.json"))
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            authz::host_id(dir.path()).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let base = format!("http://127.0.0.1:{port}");
            let state = super::WebState::for_test(dir.path(), "https://example.test", port, "fixture");
            let server = tokio::spawn(async move { axum::serve(listener, super::super::router(state)).await.unwrap(); });
            let client = reqwest::Client::new();
            for endpoint in ["configure", "start", "approve", "reject"] {
                let request = || client.post(format!("{base}/api/devices/{endpoint}")).json(&json!({"cred_id":"bad"}));
                assert_eq!(request().header("Origin", &base).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
                assert_eq!(request().header("Cookie", "ac_local_owner=fixture").header("Origin", "https://evil.test").send().await.unwrap().status(), StatusCode::FORBIDDEN);
            }
            let post = |endpoint: &str| client.post(format!("{base}/api/devices/{endpoint}")).header("Origin", &base).header("Cookie", "ac_local_owner=fixture");
            assert_eq!(post("configure").send().await.unwrap().status(), StatusCode::OK);
            assert_eq!(post("configure").send().await.unwrap().status(), StatusCode::BAD_REQUEST);
            let response = post("start").send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let result: Value = response.json().await.unwrap();
            assert!(result["link"].as_str().unwrap().starts_with("https://example.test/host-link#desc="));
            let id = "A".repeat(43);
            let store = Store::open(dir.path()).unwrap();
            store.connection().execute("INSERT INTO pending_credentials(cred_id,kind,label,grants,rp_id,passkey,created_at) VALUES(?1,'owner',?2,?3,'example.test','{}',1)", rusqlite::params![id, LABEL, GRANTS.join(",")]).unwrap();
            let pending: Value = client.get(format!("{base}/api/devices/pending")).header("Cookie", "ac_local_owner=fixture").send().await.unwrap().json().await.unwrap();
            assert_eq!(pending["devices"][0]["cred_id"], id);
            assert_eq!(store.connection().query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get::<_,i64>(0)).unwrap(), 0);
            assert_eq!(post("approve").json(&json!({"cred_id":id})).send().await.unwrap().status(), StatusCode::OK);
            assert_eq!(store.connection().query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get::<_,i64>(0)).unwrap(), 1);
            assert_eq!(post("approve").json(&json!({"cred_id":id})).send().await.unwrap().status(), StatusCode::BAD_REQUEST);
            assert_eq!(post("reject").json(&json!({"cred_id":"../bad"})).send().await.unwrap().status(), StatusCode::BAD_REQUEST);
            let rejected = "B".repeat(43);
            store.connection().execute("INSERT INTO pending_credentials(cred_id,kind,label,grants,rp_id,passkey,created_at) VALUES(?1,'owner',?2,?3,'example.test','{}',1)", rusqlite::params![rejected, LABEL, GRANTS.join(",")]).unwrap();
            assert_eq!(post("reject").json(&json!({"cred_id":rejected})).send().await.unwrap().status(), StatusCode::OK);
            assert_eq!(post("approve").json(&json!({"cred_id":rejected})).send().await.unwrap().status(), StatusCode::BAD_REQUEST);
            server.abort();
        });
    }
}
