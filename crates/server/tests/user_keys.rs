//! Account-bound key publication and directory discovery over HTTP.
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use conversations_server::{build_router, AppState, Config, Repository, SqliteRepo};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use std::sync::Arc;

#[tokio::test]
async fn publication_requires_possession_owner_and_fresh_challenge() {
    let repo = SqliteRepo::connect("sqlite::memory:").await.unwrap();
    repo.upsert_user("alice@example.com", Some("Alice"))
        .await
        .unwrap();
    repo.upsert_user("bob@example.com", Some("Bob"))
        .await
        .unwrap();
    let pool = repo.pool().clone();
    let state = AppState::new(
        Arc::new(repo),
        Arc::new(Config {
            auth_enabled: false,
            ..Config::default()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task =
        tokio::spawn(async move { axum::serve(listener, build_router(state)).await.unwrap() });
    let http = reqwest::Client::new();
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let pair =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let public = STANDARD.encode(pair.public_key().as_ref());
    let begin = || {
        http.post(format!("{base}/api/user-keys/challenge"))
            .header("x-demo-user", "alice@example.com")
            .json(&json!({"public_key":public,"label":"Browser"}))
    };
    let publish = |owner: &str, proof: &Value| {
        http.post(format!("{base}/api/user-keys"))
            .header("x-demo-user", owner)
            .json(proof)
    };
    let challenge: Value = begin()
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let bytes = format!(
        "conversations-browser-key/v1\n{}",
        challenge["payload"].as_str().unwrap()
    );
    let sig = URL_SAFE_NO_PAD.encode(pair.sign(&rng, bytes.as_bytes()).unwrap().as_ref());
    let proof = json!({"challenge":challenge["challenge"],"signature":sig});
    assert_eq!(
        http.get(format!("{base}/api/user-keys"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        publish("bob@example.com", &proof)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let tampered =
        json!({"challenge":challenge["challenge"],"signature":URL_SAFE_NO_PAD.encode([0u8;64])});
    assert_eq!(
        publish("alice@example.com", &tampered)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let other_pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let other =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, other_pkcs8.as_ref(), &rng)
            .unwrap();
    let wrong_key = json!({"challenge":challenge["challenge"],"signature":URL_SAFE_NO_PAD.encode(other.sign(&rng, bytes.as_bytes()).unwrap().as_ref())});
    assert_eq!(
        publish("alice@example.com", &wrong_key)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    for changed in [
        bytes.replace("publish_browser_key", "another_purpose"),
        bytes.replace("alice@example.com", "bob@example.com"),
    ] {
        let changed_proof = json!({"challenge":challenge["challenge"],"signature":URL_SAFE_NO_PAD.encode(pair.sign(&rng, changed.as_bytes()).unwrap().as_ref())});
        assert_eq!(
            publish("alice@example.com", &changed_proof)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    let (first, duplicate) = tokio::join!(
        publish("alice@example.com", &proof).send(),
        publish("alice@example.com", &proof).send()
    );
    let mut statuses = [
        first.unwrap().status().as_u16(),
        duplicate.unwrap().status().as_u16(),
    ];
    statuses.sort();
    assert_eq!(statuses, [200, 400]);
    assert_eq!(
        publish("alice@example.com", &proof)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let directory: Value = http
        .get(format!("{base}/api/user-keys?q=Alice"))
        .header("x-demo-user", "bob@example.com")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(directory["users"].as_array().unwrap().len(), 1);
    assert_eq!(directory["users"][0]["keys"][0]["public_key"], public);
    assert_eq!(directory["users"][0]["keys"][0]["alg"], "es256");
    let expired: Value = begin().send().await.unwrap().json().await.unwrap();
    sqlx::query("UPDATE browser_key_challenges SET expires_at = 0")
        .execute(&pool)
        .await
        .unwrap();
    let bytes = format!(
        "conversations-browser-key/v1\n{}",
        expired["payload"].as_str().unwrap()
    );
    let signature = URL_SAFE_NO_PAD.encode(pair.sign(&rng, bytes.as_bytes()).unwrap().as_ref());
    assert_eq!(
        publish(
            "alice@example.com",
            &json!({"challenge":expired["challenge"],"signature":signature})
        )
        .send()
        .await
        .unwrap()
        .status(),
        400
    );
    task.abort();
}
