mod auth;
mod device;
mod limits;
mod poller;
mod warmup;
mod wham;

pub(crate) use auth::{account_from_auth_json, account_from_tokens, ensure_fresh};
pub(crate) use device::device_login_start;
pub(crate) use limits::{apply_rate_event, apply_rate_headers};
pub(crate) use poller::spawn_background;
pub(crate) use warmup::warmup;
pub(crate) use wham::{fetch_usage, wham};

use crate::app::{Account, App, Settings};
use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

pub(crate) const ISSUER: &str = "https://auth.openai.com";
pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

const FORWARD_HEADERS: &[&str] = &[
    "session_id",
    "session-id",
    "thread-id",
    "conversation_id",
    "x-client-request-id",
    "x-openai-subagent",
    "openai-beta",
];

const STRIP_FIELDS: &[&str] = &[
    "max_output_tokens",
    "max_tokens",
    "metadata",
    "prompt_cache_retention",
    "safety_identifier",
    "temperature",
    "top_p",
    "truncation",
    "user",
    "prompt_cache_options",
    "previous_response_id",
    "stream_options",
];

pub(crate) fn prepare_body(body: &mut Value) {
    if let Some(o) = body.as_object_mut() {
        for f in STRIP_FIELDS {
            o.remove(*f);
        }
        o.insert("store".into(), Value::Bool(false));
        o.insert("stream".into(), Value::Bool(true));
        if !o.contains_key("instructions") && !has_system_message(o) {
            o.insert("instructions".into(), Value::String(String::new()));
        }
        if let Some(Value::String(s)) = o.get_mut("input") {
            let text = std::mem::take(s);
            o.insert(
                "input".into(),
                json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}]),
            );
        }
    }
}

fn has_system_message(o: &Map<String, Value>) -> bool {
    o.get("input")
        .and_then(Value::as_array)
        .is_some_and(|i| i.iter().any(|m| m["role"] == "developer" || m["role"] == "system"))
}

pub(crate) fn request(
    app: &App,
    s: &Settings,
    a: &Account,
    method: reqwest::Method,
    path: &str,
    client: Option<&HeaderMap>,
) -> reqwest::RequestBuilder {
    let version = app.codex_version(s);
    let c = a.cfg.read();
    let mut rb = app
        .http
        .request(method, format!("{}{path}", s.upstream_base))
        .bearer_auth(&c.access_token)
        .header("chatgpt-account-id", &c.chatgpt_account_id)
        .header("accept", "text/event-stream")
        .header("originator", "codex_cli_rs")
        .header("version", version.as_str());
    let mut ua = None;
    if let Some(h) = client {
        for (k, v) in h {
            let n = k.as_str();
            if FORWARD_HEADERS.contains(&n) || n.starts_with("x-codex-") {
                rb = rb.header(k, v);
            }
        }
        ua = h
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .filter(|u| u.starts_with("codex_"))
            .map(str::to_owned);
    }
    rb.header(
        "user-agent",
        ua.unwrap_or_else(|| format!("codex_cli_rs/{version} (Mac OS 26.5.0; arm64) nanolb")),
    )
}
