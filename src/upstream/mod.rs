pub(crate) mod codex;
pub(crate) mod openrouter;

use crate::app::{Account, App, Provider};
use crate::store::StoreError;
use crate::time::now_ms;
use axum::http::HeaderMap;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite;

#[derive(Debug, thiserror::Error)]
pub(crate) enum UpstreamError {
    #[error("no available accounts")]
    NoAccount,
    #[error("upstream connect failed: {0}")]
    Connect(reqwest::Error),
    #[error("upstream timed out")]
    Timeout,
    #[error("{message}")]
    Status { status: u16, message: String },
    #[error("{0}")]
    FirstEvent(String),
    #[error("upstream websocket connect failed: {0}")]
    WsConnect(tungstenite::Error),
    #[error("the account that owns this file is not available")]
    FileOwnerGone,
    #[error("build request: {0}")]
    Build(reqwest::Error),
    #[error("{0}")]
    Handshake(reqwest::Error),
    #[error("{0}")]
    HandshakeUrl(tungstenite::Error),
    #[error("refresh: {0}")]
    RefreshSend(reqwest::Error),
    #[error("token refresh failed ({status}): {body}")]
    Refresh { status: StatusCode, body: Value },
    #[error("token exchange failed ({status}): {body}")]
    Exchange { status: StatusCode, body: Value },
    #[error("usage fetch: {0}")]
    UsageFetch(StatusCode),
    #[error("device code login is not enabled for this ChatGPT workspace; use `nanolb login --browser` or import auth.json")]
    DeviceLoginDisabled,
    #[error("device token poll failed: {0}")]
    DevicePoll(StatusCode),
    #[error("device login expired")]
    DeviceExpired,
    #[error("wham/{path} {status}: {body}")]
    Wham { path: String, status: StatusCode, body: String },
    #[error("wham/{path}: invalid json: {source}")]
    WhamJson { path: String, source: serde_json::Error },
    #[error("openrouter rejected key: {0}")]
    KeyRejected(StatusCode),
    #[error("{0}")]
    Decode(String),
    #[error("{0}")]
    Encode(#[from] serde_json::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}

impl UpstreamError {
    pub(crate) const fn status(&self) -> u16 {
        match self {
            Self::NoAccount => 503,
            Self::Timeout => 504,
            Self::FileOwnerGone => 404,
            Self::Build(_) | Self::Handshake(_) | Self::HandshakeUrl(_) => 500,
            Self::Status { status, .. } => *status,
            _ => 502,
        }
    }
}

pub(crate) enum Failure {
    Retry(i64),
    Client,
}

pub(crate) fn limit_reset_from_error(body: &Value) -> Option<i64> {
    let e = &body["error"];
    e["resets_at"]
        .as_i64()
        .map(|s| s * 1000)
        .or_else(|| e["resets_in_seconds"].as_i64().map(|s| now_ms() + s * 1000))
}

pub(crate) fn classify(status: u16, err: &Value) -> Failure {
    let code = err["code"].as_str().or_else(|| err["type"].as_str()).unwrap_or_default();
    let reset = limit_reset_from_error(&json!({ "error": err }));
    match (status, reset) {
        (429, r) => Failure::Retry(r.unwrap_or_else(|| now_ms() + 60_000)),
        (401, _) => Failure::Retry(now_ms() + 300_000),
        (403, _) => Failure::Retry(0),
        (500.., _) => Failure::Retry(now_ms() + 5_000),
        (0, Some(r)) => Failure::Retry(r),
        (0, None)
            if code.contains("rate_limit")
                || code.contains("usage_limit")
                || code.contains("overloaded")
                || code.contains("server_error") =>
        {
            Failure::Retry(now_ms() + 5_000)
        }
        _ => Failure::Client,
    }
}

pub(crate) fn request(
    app: &App,
    acc: &Account,
    method: reqwest::Method,
    path: &str,
    client: Option<&HeaderMap>,
) -> reqwest::RequestBuilder {
    match acc.provider() {
        Provider::OpenRouter => openrouter::request(app, acc, method, path),
        Provider::Codex | Provider::Other => codex::request(app, acc, method, path, client),
    }
}
