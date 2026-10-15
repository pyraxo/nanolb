mod turn;

use super::frames::{error_frame, handshake, pass_headers};
use super::{Link, Upstream, exhausted, header_session, limited_until};
use crate::app::{Account, App, Key, Provider, Sessions, Settings};
use crate::server::error::ApiError;
use crate::store::{Endpoint, Usage, UsageMeta};
use crate::time::{now_ms, ts_iso};
use crate::upstream::{self, UpstreamError, codex};
use axum::extract::ws::{self, WebSocket};
use axum::http::{HeaderMap, HeaderValue};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::{self as tung, Message};

const BETA: &str = "responses_websockets=2026-02-06";

pub(super) enum Ev {
    Client(Option<Result<ws::Message, axum::Error>>),
    Up(Option<Result<Message, tung::Error>>),
    Timeout,
}

enum Step {
    Next(UpstreamError),
    Again,
    Stop(UpstreamError),
}

fn rejected(acc: &Account, status: u16, text: &str) -> Step {
    let (v, message) = upstream::error_body(text);
    let Some(until) = upstream::retry_until(status, &v["error"]) else {
        tracing::warn!(account = %acc.id, status, error = %message, "upstream rejected websocket");
        return Step::Stop(UpstreamError::Status { status, message });
    };
    if until > 0 {
        acc.fail(until, format!("{status}: {message}"));
    }
    tracing::warn!(account = %acc.id, status, error = %message, "websocket handshake failed, failing over");
    Step::Next(UpstreamError::Status { status, message })
}

struct Pending {
    frame: String,
    model: String,
    started: i64,
    ttfb: Option<i64>,
    retries: u32,
    acc: Option<Arc<Account>>,
    deferred: Option<Value>,
}

pub(super) struct Conn {
    app: Arc<App>,
    settings: Arc<Settings>,
    key: Arc<Key>,
    h: HeaderMap,
    cache: Option<String>,
    up: Option<Link>,
    tried: Vec<String>,
    pending: Option<Pending>,
    created: bool,
}

impl Conn {
    pub(super) fn new(app: Arc<App>, key: Arc<Key>, h: HeaderMap) -> Self {
        Self {
            settings: app.settings(),
            app,
            key,
            cache: header_session(&h),
            h,
            up: None,
            tried: vec![],
            pending: None,
            created: false,
        }
    }

    pub(super) async fn preconnect(&mut self) -> Result<HeaderMap, UpstreamError> {
        if self.cache.is_none() {
            return Ok(HeaderMap::new());
        }
        self.connect().await
    }

    fn session(&self) -> Option<String> {
        self.cache.as_ref().map(|c| Sessions::key(&self.key.cfg.id, Provider::Codex, c))
    }

    async fn connect(&mut self) -> Result<HeaderMap, UpstreamError> {
        let s = Arc::clone(&self.settings);
        let session = self.session();
        let mut refreshed: Vec<String> = vec![];
        let mut last = UpstreamError::NoAccount;
        for _ in 0..s.max_attempts.max(1) {
            let Some(acc) = self
                .app
                .pick(&s, Provider::Codex, &self.key.cfg.accounts, &self.tried, session.as_deref())
            else {
                break;
            };
            match self.attempt(&s, &acc, &mut refreshed).await {
                Ok((ws, headers)) => {
                    self.up = Some(Link::new(ws, acc));
                    return Ok(headers);
                }
                Err(Step::Again) => {}
                Err(Step::Next(e)) => {
                    self.tried.push(acc.id.clone());
                    last = e;
                }
                Err(Step::Stop(e)) => return Err(e),
            }
        }
        Err(last)
    }

    async fn attempt(&self, s: &Settings, acc: &Arc<Account>, refreshed: &mut Vec<String>) -> Result<(Upstream, HeaderMap), Step> {
        codex::ensure_fresh(&self.app, acc, false).await.map_err(Step::Next)?;
        let req = self.handshake(s, acc).map_err(Step::Stop)?;
        let timeout = Duration::from_secs(s.first_event_timeout_secs.max(5));
        match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(req)).await {
            Ok(Ok((ws, resp))) => {
                codex::apply_rate_headers(acc, resp.headers());
                Ok((ws, pass_headers(resp.headers())))
            }
            Ok(Err(tung::Error::Http(resp))) => {
                let status = resp.status().as_u16();
                codex::apply_rate_headers(acc, resp.headers());
                if status == 401 && !refreshed.contains(&acc.id) {
                    refreshed.push(acc.id.clone());
                    if codex::ensure_fresh(&self.app, acc, true).await.is_ok() {
                        return Err(Step::Again);
                    }
                }
                let text = resp.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default();
                Err(rejected(acc, status, &text))
            }
            Ok(Err(e)) => {
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                Err(Step::Next(UpstreamError::WsConnect(e)))
            }
            Err(_) => {
                acc.fail(now_ms() + 15_000, "websocket handshake timed out".into());
                Err(Step::Next(UpstreamError::Timeout))
            }
        }
    }

    fn handshake(&self, s: &Settings, acc: &Account) -> Result<Request, UpstreamError> {
        let mut req = handshake(&self.app, s, acc, &self.h, None)?;
        let hs = req.headers_mut();
        hs.insert("openai-beta", HeaderValue::from_static(BETA));
        if !hs.contains_key("session_id")
            && !hs.contains_key("session-id")
            && let Some(v) = self.cache.as_deref().and_then(|c| HeaderValue::from_str(c).ok())
        {
            hs.insert("session_id", v);
        }
        Ok(req)
    }

    fn fail_frame(&self, e: &UpstreamError) -> String {
        match limited_until(&self.app, &self.key, e) {
            Some(until) => error_frame(
                429,
                "usage_limit_reached",
                &format!("all codex accounts are rate limited until {}", ts_iso(until)),
                Some(((until - now_ms()) / 1000).max(1)),
            ),
            None => error_frame(e.status(), "api_error", &e.to_string(), None),
        }
    }

    pub(super) fn fail_response(&self, e: UpstreamError) -> ApiError {
        exhausted(&self.app, &self.key, e)
    }

    fn record(&mut self, status: u16, resp: &Value) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let model = resp["model"].as_str().unwrap_or(&p.model);
        let meta = UsageMeta {
            key_id: &self.key.cfg.id,
            account_id: p.acc.as_ref().map_or("", |a| &a.id),
            model,
            endpoint: Endpoint::ResponsesWs,
            started: p.started,
            ttfb: p.ttfb.unwrap_or(p.started),
        };
        let row = Usage::new(meta, status, &resp["usage"], &self.settings.pricing);
        self.app.record(Some(&self.key), row);
    }

    fn record_unsent(&self, model: &str, status: u16) {
        let now = now_ms();
        let meta = UsageMeta {
            key_id: &self.key.cfg.id,
            account_id: "",
            model,
            endpoint: Endpoint::ResponsesWs,
            started: now,
            ttfb: now,
        };
        let row = Usage::new(meta, status, &Value::Null, &self.settings.pricing);
        self.app.record(Some(&self.key), row);
    }

    async fn send_up(&mut self, m: Message) -> bool {
        match self.up.as_mut() {
            Some(l) => l.ws.send(m).await.is_ok(),
            None => false,
        }
    }

    fn drop_upstream(&mut self, until: i64, msg: &str) -> bool {
        if let Some(l) = self.up.take() {
            if until > 0 {
                l.acc.fail(until, msg.into());
            }
            self.tried.push(l.acc.id.clone());
        }
        let max = self.settings.max_attempts.max(1);
        match self.pending.as_mut() {
            Some(p) if p.ttfb.is_none() && p.retries + 1 < max => {
                p.retries += 1;
                true
            }
            _ => false,
        }
    }

    fn cool(&self, until: i64, msg: &str) {
        if until > 0
            && let Some(l) = self.up.as_ref()
        {
            l.acc.fail(until, msg.into());
        }
    }

    pub(super) async fn run(mut self, mut client: WebSocket) {
        let first = Duration::from_secs(self.settings.first_event_timeout_secs.max(5));
        let idle = Duration::from_secs(self.settings.idle_timeout_secs.max(30));
        loop {
            let wait = self.pending.as_ref().map(|p| if p.ttfb.is_none() { first } else { idle });
            let up = &mut self.up;
            let ev = tokio::select! {
                m = client.recv() => Ev::Client(m),
                m = async { match up { Some(l) => l.ws.next().await, None => std::future::pending().await } } => Ev::Up(m),
                () = async { match wait { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => Ev::Timeout,
            };
            let alive = match ev {
                Ev::Client(Some(Ok(m))) => self.on_client(m, &mut client).await,
                Ev::Client(_) => false,
                Ev::Up(m) => self.on_upstream(m, &mut client).await,
                Ev::Timeout => self.on_timeout(&mut client).await,
            };
            if !alive {
                break;
            }
        }
        if self.pending.is_some() {
            self.record(499, &Value::Null);
        }
        if let Some(mut l) = self.up.take() {
            let _ = l.ws.close(None).await;
        }
    }
}
