mod common;

use axum::extract::ws::{Message as CMsg, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use common::{Harness, spawn};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const BETA: &str = "responses_websockets=2026-02-06";

const EVENTS: [&str; 3] = [
    r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
    r#"{"type":"response.output_text.delta","delta":"hello"}"#,
    r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","model":"gpt-5.4","output":[],"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}"#,
];

fn event_type(t: &str) -> String {
    serde_json::from_str::<Value>(t).unwrap_or_default()["type"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn mock_upstream(hits_b: Arc<AtomicUsize>) -> axum::Router {
    axum::Router::new().route(
        "/codex/responses",
        get(move |h: HeaderMap, ws: WebSocketUpgrade| {
            let hits_b = hits_b.clone();
            async move {
                if h["chatgpt-account-id"] == "acct-a" {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":600}}"#,
                    )
                        .into_response();
                }
                if h.get("openai-beta").is_none_or(|v| v != BETA) || h.get("session_id").is_none_or(|v| v != "conv-1") {
                    return (StatusCode::BAD_REQUEST, "bad handshake").into_response();
                }
                ws.on_upgrade(move |mut sock| async move {
                    while let Some(Ok(CMsg::Text(t))) = sock.recv().await {
                        if event_type(&t) != "response.create" {
                            continue;
                        }
                        hits_b.fetch_add(1, Ordering::SeqCst);
                        for e in EVENTS {
                            let _ = sock.send(CMsg::Text(e.into())).await;
                        }
                    }
                })
            }
        }),
    )
}

#[tokio::test]
async fn websocket_relays_events_after_failing_over_a_rate_limited_handshake() {
    let hits_b = Arc::new(AtomicUsize::new(0));
    let h = Harness::start(&spawn(mock_upstream(hits_b.clone())).await, json!({})).await;
    let a = h.codex_account("acct-a", 0, true).await;
    h.codex_account("acct-b", 1, true).await;
    let key = h.key().await;

    let mut req = format!("{}/v1/responses", h.url.replacen("http://", "ws://", 1))
        .into_client_request()
        .unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {key}").parse().unwrap());
    req.headers_mut().insert("session_id", "conv-1".parse().unwrap());
    let (mut sock, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let create = json!({"type": "response.create", "model": "gpt-5.4", "input": [], "stream": true});
    sock.send(Message::Text(create.to_string().into())).await.unwrap();
    let mut got = vec![];
    while let Some(Ok(Message::Text(t))) = sock.next().await {
        let done = event_type(&t) == "response.completed";
        got.push(t.to_string());
        if done {
            break;
        }
    }

    assert_eq!(got, EVENTS.map(String::from));
    assert_eq!(hits_b.load(Ordering::SeqCst), 1);
    let accounts = h.op("accounts_list", json!({})).await;
    let acc_a = accounts.as_array().unwrap().iter().find(|x| x["id"] == a.as_str()).unwrap();
    assert_eq!(acc_a["available"], false);
}
