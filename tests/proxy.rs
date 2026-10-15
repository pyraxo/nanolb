mod common;

use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use common::{Harness, spawn};
use futures_util::StreamExt;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const SSE_OK: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"model\":\"gpt-5.4\",\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"total_tokens\":12}}}\n\n";

async fn two_accounts(upstream: axum::Router, settings: serde_json::Value) -> (Harness, String) {
    let h = Harness::start(&spawn(upstream).await, settings).await;
    h.codex_account("acct-a", 0, true).await;
    h.codex_account("acct-b", 1, true).await;
    let key = h.key().await;
    (h, key)
}

#[tokio::test]
async fn rate_limited_account_fails_over_and_cache_session_sticks_to_survivor() {
    let hits_b = Arc::new(AtomicUsize::new(0));
    let hits = hits_b.clone();
    let upstream = axum::Router::new().route(
        "/codex/responses",
        post(move |h: HeaderMap| {
            let hits = hits.clone();
            async move {
                if h["chatgpt-account-id"] == "acct-a" {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":600}}"#.to_owned(),
                    );
                }
                if h.get("session_id").is_none_or(|v| v != "conv-1") {
                    return (StatusCode::BAD_REQUEST, "missing cache session".to_owned());
                }
                hits.fetch_add(1, Ordering::SeqCst);
                (StatusCode::OK, SSE_OK.to_owned())
            }
        }),
    );
    let (h, key) = two_accounts(upstream, json!({})).await;
    let http = reqwest::Client::new();
    let body = json!({"model": "gpt-5.4", "input": "hi", "stream": true, "prompt_cache_key": "conv-1"});

    for _ in 0..2 {
        let res = http
            .post(format!("{}/v1/responses", h.url))
            .bearer_auth(&key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(res.text().await.unwrap().contains("\"delta\":\"hello\""));
    }

    assert_eq!(hits_b.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn stalled_stream_gets_keepalive_comments() {
    let upstream = axum::Router::new().route(
        "/codex/responses",
        post(|| async {
            let chunks: Vec<String> = SSE_OK.split_inclusive("\n\n").map(str::to_owned).collect();
            axum::body::Body::from_stream(
                futures_util::stream::iter(chunks.into_iter().enumerate()).then(|(i, c)| async move {
                    if i == 1 {
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    }
                    Ok::<_, std::convert::Infallible>(c)
                }),
            )
        }),
    );
    let (h, key) = two_accounts(upstream, json!({"keepalive_secs": 1})).await;
    let body = json!({"model": "gpt-5.4", "input": "hi", "stream": true});
    let res = reqwest::Client::new()
        .post(format!("{}/v1/responses", h.url))
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .unwrap();
    let text = res.text().await.unwrap();
    let ka = text.find(": keepalive\n\n").expect("keepalive comment");
    assert!(ka < text.find("\"delta\":\"hello\"").unwrap());
}
