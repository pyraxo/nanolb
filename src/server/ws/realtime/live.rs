use super::owner;
use crate::app::{Account, App, Key, Provider, Settings};
use crate::server::error::{ApiError, ApiFormat};
use crate::server::ws::conn::Ev;
use crate::server::ws::frames::{event_type, handshake, to_client, to_upstream};
use crate::server::ws::{Link, Upstream, authed};
use crate::store::{Endpoint, Usage, UsageMeta};
use crate::time::now_ms;
use crate::upstream::{self, UpstreamError, codex};
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::{self as tung, Message};

pub(super) struct Live {
    pub(super) app: Arc<App>,
    pub(super) settings: Arc<Settings>,
    pub(super) key: Arc<Key>,
    pub(super) h: HeaderMap,
    pub(super) url: String,
}

enum Step {
    Next(UpstreamError),
    Stop(ApiError),
}

fn rejected(acc: &Account, resp: &tung::http::Response<Option<Vec<u8>>>) -> Step {
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
        None => return Step::Stop(ApiError::new(ApiFormat::Responses, status, message)),
    }
    Step::Next(UpstreamError::Status { status, message })
}

impl Live {
    pub(super) async fn run(self, ws: WebSocketUpgrade, call_id: Option<String>, model: String) -> Result<Response, ApiError> {
        authed(&self.key)?;
        let owner = owner(&self.app, &self.key, call_id.as_deref())?;
        let mut tried: Vec<String> = vec![];
        let mut last = UpstreamError::NoAccount;
        for _ in 0..self.settings.max_attempts.max(1) {
            let acc = match &owner {
                Some(a) if tried.is_empty() => a.clone(),
                Some(_) => break,
                None => match self.app.pick(&self.settings, Provider::Codex, &self.key.cfg.accounts, &tried, None) {
                    Some(a) => a,
                    None => break,
                },
            };
            tried.push(acc.id.clone());
            match self.open(&acc).await {
                Ok((up, proto)) => {
                    let ws = match proto {
                        Some(p) => ws.protocols([p]),
                        None => ws,
                    };
                    let link = Link::new(up, acc);
                    let Self { app, key, .. } = self;
                    return Ok(ws.on_upgrade(move |client| relay_live(app, key, link, client, model)));
                }
                Err(Step::Next(e)) => last = e,
                Err(Step::Stop(e)) => return Err(e),
            }
        }
        Err(ApiError::from((ApiFormat::Responses, last)))
    }

    async fn open(&self, acc: &Arc<Account>) -> Result<(Upstream, Option<String>), Step> {
        codex::ensure_fresh(&self.app, acc, false).await.map_err(Step::Next)?;
        let mut req = handshake(&self.app, &self.settings, acc, &self.h, Some(&self.url))
            .map_err(|e| Step::Stop(ApiError::from((ApiFormat::Responses, e))))?;
        if let Some(p) = self.h.get(header::SEC_WEBSOCKET_PROTOCOL) {
            req.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, p.clone());
        }
        let timeout = Duration::from_secs(self.settings.first_event_timeout_secs.max(5));
        match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(req)).await {
            Ok(Ok((up, resp))) => {
                let proto = resp
                    .headers()
                    .get(header::SEC_WEBSOCKET_PROTOCOL)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                Ok((up, proto))
            }
            Ok(Err(tung::Error::Http(resp))) => Err(rejected(acc, &resp)),
            Ok(Err(e)) => {
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                Err(Step::Next(UpstreamError::WsConnect(e)))
            }
            Err(_) => {
                acc.fail(now_ms() + 15_000, "realtime handshake timed out".into());
                Err(Step::Next(UpstreamError::Timeout))
            }
        }
    }
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
