mod common;

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use common::{Harness, spawn};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;

fn recorder(seen: &Arc<Mutex<Vec<String>>>) -> axum::routing::MethodRouter {
    let log = seen.clone();
    post(move |h: HeaderMap| {
        let log = log.clone();
        async move {
            log.lock().push(h["chatgpt-account-id"].to_str().unwrap_or_default().to_owned());
            Json(json!({"status": "success"}))
        }
    })
}

#[tokio::test]
async fn file_use_routes_to_the_account_that_created_it() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let mock = axum::Router::new()
        .route(
            "/files",
            post(|| async { Json(json!({"file_id": "file_affinity", "upload_url": "http://upload"})) }),
        )
        .route("/files/{id}/uploaded", recorder(&seen));
    let h = Harness::start(&spawn(mock).await, json!({"strategy": "priority"})).await;
    h.codex_account("acct-a", 1, true).await;
    let b = h.codex_account("acct-b", 0, false).await;
    let key = h.key().await;
    let http = reqwest::Client::new();

    let created: Value = http
        .post(format!("{}/backend-api/files", h.url))
        .bearer_auth(&key)
        .json(&json!({"file_name": "a.txt", "file_size": 1, "use_case": "codex"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["file_id"], "file_affinity");

    h.op("account_update", json!({"id": b, "enabled": true})).await;
    let res = http
        .post(format!("{}/backend-api/files/file_affinity/uploaded", h.url))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(*seen.lock(), vec!["acct-a".to_owned()]);
}

#[tokio::test]
async fn forwarder_fails_over_on_429() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let mock = axum::Router::new()
        .route(
            "/files",
            post(|h: HeaderMap| async move {
                if h["chatgpt-account-id"] == "acct-a" {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":600}}"#.to_owned(),
                    );
                }
                (StatusCode::OK, r#"{"file_id":"file_failover"}"#.to_owned())
            }),
        )
        .route("/files/{id}/uploaded", recorder(&seen));
    let h = Harness::start(&spawn(mock).await, json!({"strategy": "priority"})).await;
    h.codex_account("acct-a", 0, true).await;
    h.codex_account("acct-b", 1, true).await;
    let key = h.key().await;
    let http = reqwest::Client::new();

    let res = http
        .post(format!("{}/backend-api/files", h.url))
        .bearer_auth(&key)
        .json(&json!({"file_name": "a.txt", "file_size": 1, "use_case": "codex"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.json::<Value>().await.unwrap()["file_id"], "file_failover");

    http.post(format!("{}/backend-api/files/file_failover/uploaded", h.url))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(*seen.lock(), vec!["acct-b".to_owned()]);
}
