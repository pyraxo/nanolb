mod calls;

use super::conn::Ev;
use super::frames::{event_type, handshake, to_client, to_upstream, ws_url};
use super::{Link, authed};
use crate::app::{Account, App, Key, Provider};
use crate::server::error::{ApiError, ApiFormat};
use crate::store::{Endpoint, Usage, UsageMeta};
use crate::time::now_ms;
use crate::upstream::{self, UpstreamError, codex};
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, header};
use axum::response::Response;
pub(super) use calls::{backend_calls, v1_calls};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::{self as tung, Message};

fn internal(e: &impl Display) -> ApiError {
    ApiError::new(ApiFormat::Responses, 500, e.to_string())
}

fn call_binding(key: &Key, call_id: &str) -> String {
    format!("rt\u{1f}{}\u{1f}{call_id}", key.cfg.id)
}
fn with_query(url: String, q: Option<&str>) -> String {
    match q.filter(|q| !q.is_empty()) {
        Some(q) => format!("{url}?{q}"),
        None => url,
    }
}
fn owner(app: &App, key: &Key, call_id: Option<&str>) -> Result<Option<Arc<Account>>, ApiError> {
    call_id
        .map(|c| {
            app.sessions
                .pinned(&call_binding(key, c))
                .and_then(|id| app.accounts.get(&id))
                .ok_or_else(|| ApiError::new(ApiFormat::Responses, 404, "realtime call binding not found or expired"))
        })
        .transpose()
}

async fn live(
    app: Arc<App>,
    h: HeaderMap,
    ws: WebSocketUpgrade,
    call_id: Option<String>,
    target: String,
    model: String,
) -> Result<Response, ApiError> {
    let key = authed(&app, &h)?;
    let owner = owner(&app, &key, call_id.as_deref())?;
    let s = app.settings();
    let url = ws_url(&target);
    let timeout = Duration::from_secs(s.first_event_timeout_secs.max(5));
    let mut tried: Vec<String> = vec![];
    let mut last = UpstreamError::NoAccount;
    for _ in 0..s.max_attempts.max(1) {
        let acc = match &owner {
            Some(a) if tried.is_empty() => a.clone(),
            Some(_) => break,
            None => match app.pick(Provider::Codex, &key.cfg.accounts, &tried, None) {
                Some(a) => a,
                None => break,
            },
        };
        tried.push(acc.id.clone());
        if let Err(e) = codex::ensure_fresh(&app, &acc, false).await {
            last = e;
            continue;
        }
        let mut req = handshake(&app, &acc, &h, Some(&url)).map_err(|e| ApiError::from((ApiFormat::Responses, e)))?;
        if let Some(p) = h.get(header::SEC_WEBSOCKET_PROTOCOL) {
            req.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, p.clone());
        }
        match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(req)).await {
            Ok(Ok((up, resp))) => {
                let proto = resp
                    .headers()
                    .get(header::SEC_WEBSOCKET_PROTOCOL)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let ws = match proto {
                    Some(p) => ws.protocols([p]),
                    None => ws,
                };
                let link = Link::new(up, acc);
                return Ok(ws.on_upgrade(move |client| relay_live(app, key, link, client, model)));
            }
            Ok(Err(tung::Error::Http(resp))) => {
                let status = resp.status().as_u16();
                let v: Value = resp
                    .body()
                    .as_deref()
                    .and_then(|b| serde_json::from_slice(b).ok())
                    .unwrap_or_default();
                let message = v["error"]["message"]
                    .as_str()
                    .unwrap_or("upstream rejected realtime websocket")
                    .to_owned();
                match upstream::retry_until(status, &v["error"]) {
                    Some(until) if until > 0 => acc.fail(until, format!("{status}: {message}")),
                    Some(_) => {}
                    None => return Err(ApiError::new(ApiFormat::Responses, status, message)),
                }
                last = UpstreamError::Status { status, message };
            }
            Ok(Err(e)) => {
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                last = UpstreamError::WsConnect(e);
            }
            Err(_) => {
                acc.fail(now_ms() + 15_000, "realtime handshake timed out".into());
                last = UpstreamError::Timeout;
            }
        }
    }
    Err(ApiError::from((ApiFormat::Responses, last)))
}

fn record_done(app: &App, key: &Key, acc: &Account, model: &str, started: i64, t: &str) {
    let v: Value = serde_json::from_str(t).unwrap_or_default();
    let u = &v["response"]["usage"];
    let usage = json!({
        "input_tokens": u["input_tokens"], "output_tokens": u["output_tokens"],
        "input_tokens_details": {"cached_tokens": u["input_token_details"]["cached_tokens"]},
    });
    let meta = UsageMeta {
        key_id: &key.cfg.id,
        account_id: &acc.id,
        model,
        endpoint: Endpoint::Realtime,
        started,
        ttfb: started,
    };
    app.record(Some(key), Usage::new(meta, 200, &usage, &app.settings().pricing));
}

async fn relay_live(app: Arc<App>, key: Arc<Key>, mut up: Link, mut client: WebSocket, mut model: String) {
    let started = now_ms();
    loop {
        let ev = tokio::select! {
            m = client.recv() => Ev::Client(m),
            m = up.ws.next() => Ev::Up(m),
        };
        match ev {
            Ev::Client(Some(Ok(m))) => {
                let close = matches!(m, ws::Message::Close(_));
                if let Some(u) = to_upstream(m)
                    && up.ws.send(u).await.is_err()
                {
                    break;
                }
                if close {
                    break;
                }
            }
            Ev::Up(Some(Ok(m))) => {
                if let Message::Text(t) = &m {
                    match event_type(t) {
                        "session.created" | "session.updated" => {
                            if let Some(m) = serde_json::from_str::<Value>(t)
                                .ok()
                                .and_then(|v| v["session"]["model"].as_str().map(str::to_owned))
                            {
                                model = m;
                            }
                        }
                        "response.done" => record_done(&app, &key, &up.acc, &model, started, t),
                        _ => {}
                    }
                }
                let close = matches!(m, Message::Close(_));
                if let Some(c) = to_client(m)
                    && client.send(c).await.is_err()
                {
                    break;
                }
                if close {
                    break;
                }
            }
            Ev::Up(Some(Err(_)) | None) => {
                let _ = client.send(ws::Message::Close(None)).await;
                break;
            }
            Ev::Client(_) => break,
            Ev::Timeout => {}
        }
    }
    let _ = up.ws.close(None).await;
}
pub(super) async fn v1_realtime(
    State(app): State<Arc<App>>,
    h: HeaderMap,
    Query(q): Query<Vec<(String, String)>>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let (call_id, model) = (get("call_id"), get("model").unwrap_or_else(|| "realtime".into()));
    let mut url = reqwest::Url::parse(&format!("{}/realtime", app.settings().realtime_base)).map_err(|e| internal(&e))?;
    url.query_pairs_mut().extend_pairs(q.iter());
    live(app, h, ws, call_id, url.to_string(), model).await
}

pub(super) async fn v1_live(
    State(app): State<Arc<App>>,
    h: HeaderMap,
    Path(call_id): Path<String>,
    RawQuery(q): RawQuery,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if matches!(call_id.as_str(), "." | "..") || !call_id.chars().all(|c| c.is_ascii_alphanumeric() || "._~-".contains(c)) {
        return Err(ApiError::new(ApiFormat::Responses, 404, "invalid realtime call id"));
    }
    let target = with_query(format!("{}/live/{call_id}", app.settings().realtime_base), q.as_deref());
    live(app, h, ws, Some(call_id), target, "realtime".into()).await
}
