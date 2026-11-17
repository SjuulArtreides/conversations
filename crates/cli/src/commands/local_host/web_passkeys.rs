//! The owner-cookie equivalent of the restricted private-channel key ceremony.
use super::super::authz::passkey::{self, PasskeyRequest, PasskeyResponse};
use super::*;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
    EnrollStart {
        label: String,
        kind: String,
        grants: Vec<String>,
    },
    Pending,
    Approve {
        cred_id: String,
    },
    Vouch {
        for_host: String,
        challenge: String,
        cred_id: String,
    },
}

pub(super) async fn operate(
    State(state): State<Arc<WebState>>,
    Json(request): Json<Request>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || -> Result<String> {
        let dir = &state.dir;
        if let Request::Vouch {
            for_host,
            challenge,
            cred_id,
        } = request
        {
            return Ok(serde_json::to_string(&passkey::vouch(
                &mut Store::open(dir)?,
                &for_host,
                &challenge,
                &cred_id,
            )?)?);
        }
        let request = match request {
            Request::EnrollStart {
                label,
                kind,
                grants,
            } => PasskeyRequest::EnrollStart {
                label,
                kind,
                grants,
            },
            Request::Pending => PasskeyRequest::Pending,
            Request::Approve { cred_id } => PasskeyRequest::Approve { cred_id },
            Request::Vouch { .. } => unreachable!(),
        };
        let settings = super::super::settings::Settings::load(dir)?;
        match passkey::handle_channel(dir, &settings, request, authz::store::now_secs())? {
            PasskeyResponse::Started { challenge, options } => {
                let descriptor = authz::attest::descriptor(dir)?;
                let link =
                    passkey::enrollment_link(&state.server, &descriptor, &challenge, &options)?;
                Ok(serde_json::to_string(&json!({ "link": link }))?)
            }
            PasskeyResponse::Pending { pending } => Ok(serde_json::to_string(&pending)?),
            PasskeyResponse::Approved { cred_id } => {
                Ok(serde_json::to_string(&json!({ "cred_id": cred_id }))?)
            }
            _ => anyhow::bail!("Unexpected key ceremony response."),
        }
    })
    .await;
    let (status, detail) = match result {
        Ok(Ok(detail)) => ("executed", detail),
        Ok(Err(error)) => ("refused", error.to_string()),
        Err(_) => ("failed", "Could not complete key ceremony.".into()),
    };
    Json(json!({ "status": status, "detail": detail, "body": null })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use webauthn_authenticator_rs::{softpasskey::SoftPasskey, WebauthnAuthenticator};
    use webauthn_rs::prelude::{CreationChallengeResponse, RegisterPublicKeyCredential, Url};

    fn register(dir: &Path, challenge: String, options: Value) -> String {
        let options: CreationChallengeResponse = serde_json::from_value(options).unwrap();
        let response: RegisterPublicKeyCredential = SoftPasskey::new(true)
            .do_registration(Url::parse("http://localhost:8080").unwrap(), options)
            .unwrap();
        let settings = super::super::super::settings::Settings::load(dir).unwrap();
        let (webauthn, _) = passkey::relying_party(&settings).unwrap();
        let PasskeyResponse::Registered { cred_id, .. } = passkey::enroll_finish(
            &mut Store::open(dir).unwrap(),
            &webauthn,
            &challenge,
            &serde_json::to_value(response).unwrap(),
            None,
            authz::store::now_secs(),
        )
        .unwrap() else {
            panic!("registration failed")
        };
        cred_id
    }

    #[test]
    fn local_key_ceremony_requires_owner_cookie_and_approves_only_session_keys() {
        if !super::super::super::test_support::isolated(
            "commands::local_host::web::passkeys::tests::local_key_ceremony_requires_owner_cookie_and_approves_only_session_keys", &[],
        ) { return; }
        std::fs::write(
            super::super::super::settings::Settings::path().unwrap(),
            r#"{"webauthn":{"rp_id":"localhost","origins":["http://localhost:8080"]}}"#,
        )
        .unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let dir = tempfile::tempdir().unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            authz::host_id(dir.path()).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let base = format!("http://127.0.0.1:{port}");
            let state = WebState::for_test(dir.path(), "http://localhost:8080", port, "fixture");
            let server = tokio::spawn(async move { axum::serve(listener, super::super::router(state)).await.unwrap(); });
            let client = reqwest::Client::new();
            let endpoint = format!("{base}/api/local/passkeys");
            let post = |body: Value| client.post(&endpoint).header("Origin", &base)
                .header("Cookie", "ac_local_owner=fixture").json(&body);
            assert_eq!(client.post(&endpoint).header("Origin", &base).json(&json!({"op":"pending"})).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
            assert_eq!(client.post(&endpoint).header("Origin", "https://evil.test").header("Cookie", "ac_local_owner=fixture").json(&json!({"op":"pending"})).send().await.unwrap().status(), StatusCode::FORBIDDEN);
            let session = uuid::Uuid::new_v4().to_string();
            for bad in [json!({"op":"enroll_start","label":"x","kind":"owner","grants":["history.read"]}), json!({"op":"enroll_start","label":"x","kind":"agent","grants":["passkey.manage"]})] {
                let result: Value = post(bad).send().await.unwrap().json().await.unwrap();
                assert_eq!(result["status"], "refused");
            }
            let result: Value = post(json!({"op":"enroll_start","label":"Colleague","kind":"agent","grants":[format!("session.write:{session}"),format!("history.read:{session}")]})).send().await.unwrap().json().await.unwrap();
            assert_eq!(result["status"], "executed", "{result}");
            let detail: Value = serde_json::from_str(result["detail"].as_str().unwrap()).unwrap();
            let link = reqwest::Url::parse(detail["link"].as_str().unwrap()).unwrap();
            assert_eq!(link.origin().ascii_serialization(), "http://localhost:8080");
            let fields = reqwest::Url::parse(&format!("http://x/?{}",link.fragment().unwrap())).unwrap().query_pairs().into_owned().collect::<std::collections::HashMap<_,_>>();
            let options = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&fields["options"]).unwrap()).unwrap();
            let id = register(dir.path(), fields["challenge"].clone(), options);
            let pending: Value = post(json!({"op":"pending"})).send().await.unwrap().json().await.unwrap();
            let offers: Value = serde_json::from_str(pending["detail"].as_str().unwrap()).unwrap();
            assert_eq!(offers[0]["cred_id"], id);
            assert_eq!(Store::open(dir.path()).unwrap().connection().query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get::<_,i64>(0)).unwrap(),0);
            let approved: Value = post(json!({"op":"approve","cred_id":id})).send().await.unwrap().json().await.unwrap();
            assert_eq!(approved["status"], "executed", "{approved}");
            let store = Store::open(dir.path()).unwrap();
            assert_eq!(store.connection().query_row("SELECT COUNT(*) FROM grants WHERE cred_id=?1 AND session_id=?2 AND cap='session.write'", rusqlite::params![id,session], |r| r.get::<_,i64>(0)).unwrap(),1);
            let settings = super::super::super::settings::Settings::load(dir.path()).unwrap();
            let (web, rp) = passkey::relying_party(&settings).unwrap();
            let PasskeyResponse::Started{challenge,options} = passkey::enroll_start(&mut Store::open(dir.path()).unwrap(),&web,&rp,"Local owner","owner", &["passkey.manage".into()],authz::store::now_secs()).unwrap() else {panic!("no start")};
            let owner_id = register(dir.path(), challenge, options);
            let refused: Value = post(json!({"op":"approve","cred_id":owner_id})).send().await.unwrap().json().await.unwrap();
            assert_eq!(refused["status"], "refused");
            assert_eq!(store.connection().query_row("SELECT COUNT(*) FROM pending_credentials WHERE cred_id=?1", [&owner_id], |r| r.get::<_,i64>(0)).unwrap(),1);
            let vouch: Value = post(json!({"op":"vouch","for_host":"A".repeat(43),"challenge":"ab".repeat(16),"cred_id":id})).send().await.unwrap().json().await.unwrap();
            assert_eq!(vouch["status"],"executed");
            let proof: Value = serde_json::from_str(vouch["detail"].as_str().unwrap()).unwrap();
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            let payload = b64.decode(proof["statement"]["p"].as_str().unwrap()).unwrap();
            let mut signed = conversations_core::host_relay::HOST_DOMAIN.to_vec(); signed.extend_from_slice(&payload);
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, b64.decode(proof["host_key"].as_str().unwrap()).unwrap())
                .verify(&signed, &b64.decode(proof["statement"]["s"].as_str().unwrap()).unwrap()).unwrap();
            let refused: Value = post(json!({"op":"vouch","for_host":"bad","challenge":"ab".repeat(16),"cred_id":id})).send().await.unwrap().json().await.unwrap();
            assert_eq!(refused["status"],"refused");
            server.abort();
        });
    }
}
