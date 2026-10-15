mod conn;
mod frames;
mod realtime;

use crate::app::{Account, App, Inflight, Key, Provider};
use crate::server::error::{ApiError, ApiFormat};
use crate::time::now_ms;
use crate::upstream::UpstreamError;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Router};
use conn::Conn;
use std::sync::Arc;

type Upstream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/backend-api/codex/realtime/calls", post(realtime::backend_calls))
        .route("/v1/realtime/calls", post(realtime::v1_calls))
        .route("/v1/realtime", get(realtime::v1_realtime))
        .route("/v1/live/{call_id}", get(realtime::v1_live))
        .route("/backend-api/codex/{call_id}", get(realtime::v1_live))
}

struct Link {
    ws: Upstream,
    acc: Arc<Account>,
    _guard: Inflight,
}

impl Link {
    fn new(ws: Upstream, acc: Arc<Account>) -> Self {
        Self {
            ws,
            _guard: acc.acquire(),
            acc,
        }
    }
}

fn header_session(h: &HeaderMap) -> Option<String> {
    ["session_id", "session-id", "thread-id", "conversation_id"]
        .iter()
        .find_map(|n| h.get(*n).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()))
        .map(str::to_owned)
}

fn authed(key: &Key) -> Result<(), ApiError> {
    key.check(now_ms()).map_err(|e| ApiError::from((ApiFormat::Responses, e)))
}

fn limited_until(app: &App, key: &Key, e: &UpstreamError) -> Option<i64> {
    (e.status() == 503)
        .then(|| app.accounts.next_available(Provider::Codex, &key.cfg.accounts))
        .flatten()
}

fn exhausted(app: &App, key: &Key, e: UpstreamError) -> ApiError {
    match limited_until(app, key, &e) {
        Some(until) => ApiError::limited(ApiFormat::Responses, Provider::Codex, until),
        None => ApiError::from((ApiFormat::Responses, e)),
    }
}

pub(crate) async fn upgrade(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    h: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    authed(&key)?;
    let mut conn = Conn::new(app, key, h);
    let extra = conn.preconnect().await.map_err(|e| {
        tracing::warn!(status = e.status(), error = %e, "websocket handshake failed");
        conn.fail_response(e)
    })?;
    let mut res = ws.on_upgrade(move |socket| conn.run(socket));
    for (k, v) in &extra {
        res.headers_mut().append(k, v.clone());
    }
    Ok(res)
}
