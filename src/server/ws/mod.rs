use crate::app::{Account, App, Inflight, Key, Provider, Sessions, canonical_model};
use crate::server::auth::api_key;
use crate::server::error::{ApiError, ApiFormat};
use crate::store::{Endpoint, Usage, UsageMeta};
use crate::time::now_ms;
use crate::upstream::{self, codex};
use axum::Router;
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{self as tung, Message};

type Upstream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const BETA: &str = "responses_websockets=2026-02-06";
const PASS_HEADERS: &[&str] = &["openai-model", "x-reasoning-included", "x-models-etag", "x-request-id"];

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/backend-api/codex/realtime/calls", post(backend_calls))
        .route("/v1/realtime/calls", post(v1_calls))
        .route("/v1/realtime", get(v1_realtime))
        .route("/v1/live/{call_id}", get(v1_live))
        .route("/backend-api/codex/{call_id}", get(v1_live))
}

struct Fail(u16, String);

struct Link {
    ws: Upstream,
    acc: Arc<Account>,
    _guard: Inflight,
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

struct Conn {
    app: Arc<App>,
    key: Arc<Key>,
    h: HeaderMap,
    cache: Option<String>,
    up: Option<Link>,
    tried: Vec<String>,
    pending: Option<Pending>,
    created: bool,
}

enum Ev {
    Client(Option<Result<ws::Message, axum::Error>>),
    Up(Option<Result<Message, tung::Error>>),
    Timeout,
}

fn header_session(h: &HeaderMap) -> Option<String> {
    ["session_id", "session-id", "thread-id", "conversation_id"]
        .iter()
        .find_map(|n| h.get(*n).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()))
        .map(str::to_string)
}

fn ws_url(http: &str) -> String {
    if let Some(r) = http.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = http.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        http.to_string()
    }
}

fn handshake(app: &App, acc: &Account, client: &HeaderMap, url: Option<&str>) -> Result<tung::handshake::client::Request, String> {
    let built = codex::request(app, acc, reqwest::Method::GET, "/codex/responses", Some(client))
        .build()
        .map_err(|e| e.to_string())?;
    let url = url.map_or_else(|| ws_url(built.url().as_str()), str::to_string);
    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    let hs = req.headers_mut();
    for (k, v) in built.headers() {
        if k != header::ACCEPT {
            hs.append(k.clone(), v.clone());
        }
    }
    Ok(req)
}

fn pass_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in src {
        if PASS_HEADERS.contains(&k.as_str()) || k.as_str().starts_with("x-codex-") {
            out.append(k.clone(), v.clone());
        }
    }
    out
}

fn retry_until(status: u16, err: &Value) -> Option<i64> {
    let code = err["code"].as_str().or(err["type"].as_str()).unwrap_or_default();
    let reset = upstream::limit_reset_from_error(&json!({ "error": err }));
    match status {
        429 => Some(reset.unwrap_or(now_ms() + 60_000)),
        401 => Some(now_ms() + 300_000),
        403 => Some(0),
        500.. => Some(now_ms() + 5_000),
        _ if reset.is_some() => reset,
        _ if code.contains("rate_limit")
            || code.contains("usage_limit")
            || code.contains("overloaded")
            || code.contains("server_error") =>
        {
            Some(now_ms() + 5_000)
        }
        _ => None,
    }
}

fn event_type(t: &str) -> &str {
    t.find("\"type\":\"")
        .map(|i| &t[i + 8..])
        .and_then(|r| r.find('"').map(|j| &r[..j]))
        .unwrap_or_default()
}

fn error_frame(status: u16, typ: &str, msg: &str, retry_after: Option<i64>) -> String {
    let mut v = json!({"type": "error", "status": status, "error": {"type": typ, "message": msg}});
    if let Some(s) = retry_after {
        v["headers"] = json!({"retry-after": s.to_string()});
        v["error"]["resets_at"] = json!((now_ms() / 1000) + s);
    }
    v.to_string()
}

fn to_client(m: Message) -> Option<ws::Message> {
    Some(match m {
        Message::Text(t) => ws::Message::Text(ws::Utf8Bytes::try_from(Bytes::from(t)).ok()?),
        Message::Binary(b) => ws::Message::Binary(b),
        Message::Close(f) => ws::Message::Close(f.map(|f| ws::CloseFrame {
            code: f.code.into(),
            reason: ws::Utf8Bytes::try_from(Bytes::from(f.reason)).unwrap_or_default(),
        })),
        _ => return None,
    })
}

fn to_upstream(m: ws::Message) -> Option<Message> {
    Some(match m {
        ws::Message::Text(t) => Message::Text(tung::Utf8Bytes::try_from(Bytes::from(t)).ok()?),
        ws::Message::Binary(b) => Message::Binary(b),
        ws::Message::Close(f) => Message::Close(f.map(|f| CloseFrame {
            code: f.code.into(),
            reason: tung::Utf8Bytes::try_from(Bytes::from(f.reason)).unwrap_or_default(),
        })),
        _ => return None,
    })
}

impl Conn {
    fn session(&self) -> Option<String> {
        self.cache.as_ref().map(|c| Sessions::key(&self.key.cfg.id, Provider::Codex, c))
    }

    async fn connect(&mut self) -> Result<HeaderMap, Fail> {
        let s = self.app.settings();
        let session = self.session();
        let timeout = Duration::from_secs(s.first_event_timeout_secs.max(5));
        let mut refreshed: Vec<String> = vec![];
        let mut last = Fail(503, "no available accounts".into());
        for _ in 0..s.max_attempts.max(1) {
            let Some(acc) = self
                .app
                .pick(Provider::Codex, &self.key.cfg.accounts, &self.tried, session.as_deref())
            else {
                break;
            };
            if let Err(e) = codex::ensure_fresh(&self.app, &acc, false).await {
                self.tried.push(acc.id.clone());
                last = Fail(502, e.to_string());
                continue;
            }
            let mut req = match handshake(&self.app, &acc, &self.h, None) {
                Ok(r) => r,
                Err(e) => return Err(Fail(500, e)),
            };
            let hs = req.headers_mut();
            hs.insert("openai-beta", HeaderValue::from_static(BETA));
            if !hs.contains_key("session_id")
                && !hs.contains_key("session-id")
                && let Some(v) = self.cache.as_deref().and_then(|c| HeaderValue::from_str(c).ok())
            {
                hs.insert("session_id", v);
            }
            match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(req)).await {
                Ok(Ok((ws, resp))) => {
                    codex::apply_rate_headers(&acc, resp.headers());
                    self.up = Some(Link {
                        ws,
                        _guard: acc.acquire(),
                        acc,
                    });
                    return Ok(pass_headers(resp.headers()));
                }
                Ok(Err(tung::Error::Http(resp))) => {
                    let status = resp.status().as_u16();
                    codex::apply_rate_headers(&acc, resp.headers());
                    if status == 401 && !refreshed.contains(&acc.id) {
                        refreshed.push(acc.id.clone());
                        if codex::ensure_fresh(&self.app, &acc, true).await.is_ok() {
                            continue;
                        }
                    }
                    let text = resp.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default().to_string();
                    let v: Value = serde_json::from_str(&text).unwrap_or_default();
                    let msg = v["error"]["message"]
                        .as_str()
                        .or(v["detail"].as_str())
                        .map_or_else(|| text.chars().take(500).collect(), str::to_string);
                    let Some(until) = retry_until(status, &v["error"]) else {
                        tracing::warn!(account = %acc.id, status, error = %msg, "upstream rejected websocket");
                        return Err(Fail(status, msg));
                    };
                    if until > 0 {
                        acc.fail(until, format!("{status}: {msg}"));
                    }
                    tracing::warn!(account = %acc.id, status, error = %msg, "websocket handshake failed, failing over");
                    self.tried.push(acc.id.clone());
                    last = Fail(status, msg);
                }
                Ok(Err(e)) => {
                    acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                    self.tried.push(acc.id.clone());
                    last = Fail(502, format!("upstream websocket connect failed: {e}"));
                }
                Err(_) => {
                    acc.fail(now_ms() + 15_000, "websocket handshake timed out".into());
                    self.tried.push(acc.id.clone());
                    last = Fail(504, "upstream timed out".into());
                }
            }
        }
        Err(last)
    }

    fn limited_until(&self, f: &Fail) -> Option<i64> {
        (f.0 == 503)
            .then(|| self.app.accounts.next_available(Provider::Codex, &self.key.cfg.accounts))
            .flatten()
    }

    fn fail_frame(&self, f: &Fail) -> String {
        match self.limited_until(f) {
            Some(until) => error_frame(
                429,
                "usage_limit_reached",
                &format!("all codex accounts are rate limited until {}", crate::time::ts_iso(until)),
                Some(((until - now_ms()) / 1000).max(1)),
            ),
            None => error_frame(f.0, "api_error", &f.1, None),
        }
    }

    fn fail_response(&self, f: &Fail) -> Response {
        match self.limited_until(f) {
            Some(until) => ApiError::limited(ApiFormat::Responses, Provider::Codex, until).into_response(),
            None => ApiError::new(ApiFormat::Responses, f.0, &f.1).into_response(),
        }
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
        let row = Usage::new(meta, status, &resp["usage"], &self.app.settings().pricing);
        self.app.record(Some(&self.key), row);
    }

    async fn send_up(&mut self, m: Message) -> bool {
        match self.up.as_mut() {
            Some(l) => l.ws.send(m).await.is_ok(),
            None => false,
        }
    }

    async fn on_create(&mut self, mut v: Value, client: &mut WebSocket) -> bool {
        let started = now_ms();
        let reject = |s: u16, t: &str, m: &str| ws::Message::Text(error_frame(s, t, m, None).into());
        if let Err(e) = self.key.check(started) {
            return client.send(reject(429, "rate_limit_exceeded", &e.to_string())).await.is_ok();
        }
        let requested = v["model"].as_str().unwrap_or_default().to_string();
        let model = canonical_model(&requested).to_string();
        if !self.key.allows_model(&requested) && !self.key.allows_model(&model) {
            let m = format!("model {requested} not allowed for this key");
            return client.send(reject(403, "invalid_request_error", &m)).await.is_ok();
        }
        if Provider::for_model(&model) != Provider::Codex {
            let m = format!("model {model} is not served over websocket; use POST /v1/responses");
            return client.send(reject(400, "invalid_request_error", &m)).await.is_ok();
        }
        v["model"] = Value::String(model.clone());
        v["store"] = Value::Bool(false);
        if self.cache.is_none() {
            self.cache = Some(v["prompt_cache_key"].as_str().map_or_else(
                || {
                    let mut seed = v["instructions"].to_string();
                    for it in v["input"].as_array().into_iter().flatten().take(2) {
                        seed.push_str(&it.to_string());
                    }
                    crate::id::sha256_hex(&seed)[..24].to_string()
                },
                str::to_string,
            ));
        }
        if v.get("prompt_cache_key").is_none() {
            v["prompt_cache_key"] = Value::String(self.cache.clone().unwrap_or_default());
        }
        let warm = self.app.settings().prewarm
            && !self.created
            && v["generate"] != Value::Bool(false)
            && v["previous_response_id"].as_str().is_none_or(str::is_empty);
        self.created = true;
        let (frame, deferred) = if warm {
            let mut w = v.clone();
            w["generate"] = Value::Bool(false);
            (w.to_string(), Some(v))
        } else {
            (v.to_string(), None)
        };
        self.start(frame, model, started, deferred, client).await
    }

    async fn start(&mut self, frame: String, model: String, started: i64, deferred: Option<Value>, client: &mut WebSocket) -> bool {
        self.tried.clear();
        self.pending = Some(Pending {
            frame,
            model,
            started,
            ttfb: None,
            retries: 0,
            acc: None,
            deferred,
        });
        self.dispatch(client).await
    }

    async fn dispatch(&mut self, client: &mut WebSocket) -> bool {
        loop {
            if self.up.is_none()
                && let Err(f) = self.connect().await
            {
                tracing::warn!(status = f.0, error = %f.1, "websocket request failed");
                let model = self.pending.take().map(|p| p.model).unwrap_or_default();
                let now = now_ms();
                let meta = UsageMeta {
                    key_id: &self.key.cfg.id,
                    account_id: "",
                    model: &model,
                    endpoint: Endpoint::ResponsesWs,
                    started: now,
                    ttfb: now,
                };
                self.app
                    .record(Some(&self.key), Usage::new(meta, f.0, &Value::Null, &self.app.settings().pricing));
                return client.send(ws::Message::Text(self.fail_frame(&f).into())).await.is_ok();
            }
            let acc = self.up.as_ref().map(|l| l.acc.clone());
            let Some(frame) = self.pending.as_mut().map(|p| {
                p.acc = acc;
                p.frame.clone()
            }) else {
                return true;
            };
            if self.send_up(Message::Text(frame.into())).await {
                return true;
            }
            if !self.drop_upstream(now_ms() + 15_000, "upstream websocket send failed") {
                self.record(502, &Value::Null);
                return client
                    .send(ws::Message::Text(
                        error_frame(502, "api_error", "upstream websocket closed", None).into(),
                    ))
                    .await
                    .is_ok();
            }
        }
    }

    fn drop_upstream(&mut self, until: i64, msg: &str) -> bool {
        if let Some(l) = self.up.take() {
            if until > 0 {
                l.acc.fail(until, msg.into());
            }
            self.tried.push(l.acc.id.clone());
        }
        let max = self.app.settings().max_attempts.max(1);
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

    async fn on_client(&mut self, m: ws::Message, client: &mut WebSocket) -> bool {
        if let ws::Message::Text(t) = &m
            && event_type(t) == "response.create"
            && let Ok(v) = serde_json::from_str::<Value>(t)
            && v["type"] == "response.create"
        {
            return self.on_create(v, client).await;
        }
        let close = matches!(m, ws::Message::Close(_));
        if let Some(u) = to_upstream(m)
            && self.up.is_some()
        {
            self.send_up(u).await;
        }
        !close
    }

    async fn on_upstream(&mut self, m: Option<Result<Message, tung::Error>>, client: &mut WebSocket) -> bool {
        let m = match m {
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => return true,
            Some(Ok(Message::Close(f))) => Err(Some(f)),
            Some(Ok(m)) => Ok(m),
            Some(Err(_)) | None => Err(None),
        };
        let m = match m {
            Ok(m) => m,
            Err(close) => {
                let Some(started) = self.pending.as_ref().map(|p| p.ttfb.is_some()) else {
                    self.up = None;
                    return true;
                };
                if started {
                    self.record(502, &Value::Null);
                    let _ = client.send(to_client(Message::Close(close.flatten())).unwrap()).await;
                    return false;
                }
                if self.drop_upstream(now_ms() + 10_000, "upstream websocket closed before first event") {
                    tracing::warn!("upstream websocket closed before first event, failing over");
                    return self.dispatch(client).await;
                }
                self.record(502, &Value::Null);
                let f = error_frame(502, "api_error", "upstream websocket closed before first event", None);
                return client.send(ws::Message::Text(f.into())).await.is_ok();
            }
        };
        let mut warm_done = None;
        let warming = self.pending.as_ref().is_some_and(|p| p.deferred.is_some());
        if let Message::Text(t) = &m {
            let kind = event_type(t);
            if self.pending.as_ref().is_some_and(|p| p.ttfb.is_none()) {
                if kind == "error" {
                    let v: Value = serde_json::from_str(t).unwrap_or_default();
                    let status = v["status"].as_u64().or(v["status_code"].as_u64()).unwrap_or(0) as u16;
                    if let Some(until) = retry_until(status, &v["error"]) {
                        let msg = v["error"]["message"].as_str().unwrap_or("upstream error").to_string();
                        if self
                            .pending
                            .as_ref()
                            .is_some_and(|p| p.retries + 1 < self.app.settings().max_attempts.max(1))
                        {
                            self.drop_upstream(until, &msg);
                            tracing::warn!(status, error = %msg, "upstream websocket error before first event, failing over");
                            return self.dispatch(client).await;
                        }
                        self.cool(until, &msg);
                    }
                } else if let Some(p) = self.pending.as_mut() {
                    p.ttfb = Some(now_ms());
                    self.tried.clear();
                }
            }
            match kind {
                "codex.rate_limits" => {
                    if let (Ok(v), Some(l)) = (serde_json::from_str::<Value>(t), self.up.as_ref()) {
                        codex::apply_rate_event(&l.acc, &v);
                    }
                }
                "response.completed" | "response.incomplete" | "response.failed" => {
                    let v: Value = serde_json::from_str(t).unwrap_or_default();
                    let resp = &v["response"];
                    let status = match resp["status"].as_str() {
                        Some("completed") => 200,
                        Some("incomplete") => 206,
                        _ => 502,
                    };
                    if kind == "response.failed"
                        && let Some(reset) = upstream::limit_reset_from_error(&json!({"error": resp["error"]}))
                    {
                        self.cool(reset, resp["error"]["message"].as_str().unwrap_or("rate limited"));
                    }
                    if let Some(mut real) = self.pending.as_mut().and_then(|p| p.deferred.take()) {
                        if let (200, Some(id)) = (status, resp["id"].as_str()) {
                            real["previous_response_id"] = Value::String(id.to_string());
                            real["input"] = json!([]);
                        }
                        warm_done = Some(real);
                    }
                    self.record(status, resp);
                }
                "error" => {
                    let v: Value = serde_json::from_str(t).unwrap_or_default();
                    let status = v["status"].as_u64().unwrap_or(502) as u16;
                    warm_done = self.pending.as_mut().and_then(|p| p.deferred.take());
                    self.record(status, &Value::Null);
                }
                _ => {}
            }
        }
        if let Some(real) = warm_done {
            let model = real["model"].as_str().unwrap_or_default().to_string();
            return self.start(real.to_string(), model, now_ms(), None, client).await;
        }
        if warming {
            return true;
        }
        match to_client(m) {
            Some(cm) => client.send(cm).await.is_ok(),
            None => true,
        }
    }

    async fn run(mut self, mut client: WebSocket) {
        let s = self.app.settings();
        let first = Duration::from_secs(s.first_event_timeout_secs.max(5));
        let idle = Duration::from_secs(s.idle_timeout_secs.max(30));
        loop {
            let wait = self.pending.as_ref().map(|p| if p.ttfb.is_none() { first } else { idle });
            let up = &mut self.up;
            let ev = tokio::select! {
                m = client.recv() => Ev::Client(m),
                m = async { match up { Some(l) => l.ws.next().await, None => std::future::pending().await } } => Ev::Up(m),
                _ = async { match wait { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => Ev::Timeout,
            };
            let alive = match ev {
                Ev::Client(Some(Ok(m))) => self.on_client(m, &mut client).await,
                Ev::Client(_) => false,
                Ev::Up(m) => self.on_upstream(m, &mut client).await,
                Ev::Timeout => {
                    if self.drop_upstream(now_ms() + 10_000, "timed out waiting for first event") {
                        self.dispatch(&mut client).await
                    } else {
                        self.record(504, &Value::Null);
                        let _ = client
                            .send(ws::Message::Text(error_frame(504, "api_error", "upstream timed out", None).into()))
                            .await;
                        false
                    }
                }
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

pub async fn upgrade(State(app): State<Arc<App>>, h: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let Some(key) = api_key(&app, &h) else {
        return ApiError::new(ApiFormat::Responses, 401, "invalid or missing api key").into_response();
    };
    if let Err(e) = key.check(now_ms()) {
        return ApiError::new(ApiFormat::Responses, 429, e.to_string()).into_response();
    }
    let mut conn = Conn {
        app,
        key,
        cache: header_session(&h),
        h,
        up: None,
        tried: vec![],
        pending: None,
        created: false,
    };
    let mut extra = HeaderMap::new();
    if conn.cache.is_some() {
        match conn.connect().await {
            Ok(hs) => extra = hs,
            Err(f) => {
                tracing::warn!(status = f.0, error = %f.1, "websocket handshake failed");
                return conn.fail_response(&f);
            }
        }
    }
    let mut res = ws.on_upgrade(move |socket| conn.run(socket));
    for (k, v) in extra.iter() {
        res.headers_mut().append(HeaderName::clone(k), v.clone());
    }
    res
}

fn call_binding(key: &Key, call_id: &str) -> String {
    format!("rt\u{1f}{}\u{1f}{call_id}", key.cfg.id)
}

fn call_id_from_location(loc: &str) -> Option<&str> {
    let path = loc.split(['?', '#']).next()?;
    path.rsplit('/')
        .find(|s| !s.is_empty() && s.starts_with("rtc_"))
        .or_else(|| path.rsplit('/').find(|s| !s.is_empty()))
}

fn with_query(url: String, q: Option<&str>) -> String {
    match q.filter(|q| !q.is_empty()) {
        Some(q) => format!("{url}?{q}"),
        None => url,
    }
}

fn authed(app: &App, h: &HeaderMap) -> Result<Arc<Key>, Fail> {
    let key = api_key(app, h).ok_or_else(|| Fail(401, "invalid or missing api key".into()))?;
    key.check(now_ms()).map_err(|e| Fail(429, e.to_string()))?;
    Ok(key)
}

async fn calls(app: Arc<App>, h: HeaderMap, target: String, body: Bytes) -> Response {
    let started = now_ms();
    let key = match authed(&app, &h) {
        Ok(k) => k,
        Err(f) => return ApiError::new(ApiFormat::Responses, f.0, &f.1).into_response(),
    };
    let s = app.settings();
    let mut tried: Vec<String> = vec![];
    let mut last = Fail(503, "no available accounts".into());
    for _ in 0..s.max_attempts.max(1) {
        let Some(acc) = app.pick(Provider::Codex, &key.cfg.accounts, &tried, None) else {
            break;
        };
        tried.push(acc.id.clone());
        if let Err(e) = codex::ensure_fresh(&app, &acc, false).await {
            last = Fail(502, e.to_string());
            continue;
        }
        let mut rb = codex::request(&app, &acc, reqwest::Method::POST, "/codex/realtime/calls", Some(&h))
            .header(
                header::ACCEPT,
                h.get(header::ACCEPT).cloned().unwrap_or(HeaderValue::from_static("*/*")),
            )
            .body(body.clone());
        if let Some(ct) = h.get(header::CONTENT_TYPE) {
            rb = rb.header(header::CONTENT_TYPE, ct);
        }
        let mut req = match rb.build() {
            Ok(r) => r,
            Err(e) => return ApiError::new(ApiFormat::Responses, 500, &e.to_string()).into_response(),
        };
        match reqwest::Url::parse(&target) {
            Ok(u) => *req.url_mut() = u,
            Err(e) => return ApiError::new(ApiFormat::Responses, 500, &e.to_string()).into_response(),
        }
        let res = match tokio::time::timeout(Duration::from_secs(s.first_event_timeout_secs.max(5)), app.http.execute(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                last = Fail(502, format!("upstream connect failed: {e}"));
                continue;
            }
            Err(_) => {
                acc.fail(now_ms() + 15_000, "realtime call timed out".into());
                last = Fail(504, "upstream timed out".into());
                continue;
            }
        };
        codex::apply_rate_headers(&acc, res.headers());
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        let bytes = res.bytes().await.unwrap_or_default();
        if status >= 400 {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            if let Some(until) = retry_until(status, &v["error"]) {
                let msg = v["error"]["message"].as_str().unwrap_or("upstream error").to_string();
                if until > 0 {
                    acc.fail(until, format!("{status}: {msg}"));
                }
                last = Fail(status, msg);
                continue;
            }
        }
        if (200..300).contains(&status)
            && let Some(id) = headers
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(call_id_from_location)
        {
            app.sessions.bind(call_binding(&key, id), acc.id.clone(), now_ms());
        }
        let meta = UsageMeta {
            key_id: &key.cfg.id,
            account_id: &acc.id,
            model: "realtime",
            endpoint: Endpoint::RealtimeCall,
            started,
            ttfb: now_ms(),
        };
        app.record(Some(&key), Usage::new(meta, status, &Value::Null, &s.pricing));
        let mut out = Response::new(axum::body::Body::from(bytes));
        *out.status_mut() = axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
        for (k, v) in headers.iter() {
            if [header::CONTENT_TYPE, header::LOCATION].contains(k) || PASS_HEADERS.contains(&k.as_str()) {
                out.headers_mut().append(k.clone(), v.clone());
            }
        }
        return out;
    }
    match (last.0 == 503)
        .then(|| app.accounts.next_available(Provider::Codex, &key.cfg.accounts))
        .flatten()
    {
        Some(until) => ApiError::limited(ApiFormat::Responses, Provider::Codex, until).into_response(),
        None => ApiError::new(ApiFormat::Responses, last.0, &last.1).into_response(),
    }
}

async fn live(app: Arc<App>, h: HeaderMap, ws: WebSocketUpgrade, call_id: Option<String>, target: String, model: String) -> Response {
    let key = match authed(&app, &h) {
        Ok(k) => k,
        Err(f) => return ApiError::new(ApiFormat::Responses, f.0, &f.1).into_response(),
    };
    let owner = match &call_id {
        Some(c) => {
            let bound = app.sessions.pinned(&call_binding(&key, c));
            match bound.and_then(|id| app.accounts.get(&id)) {
                Some(a) => Some(a),
                None => return ApiError::new(ApiFormat::Responses, 404, "realtime call binding not found or expired").into_response(),
            }
        }
        None => None,
    };
    let s = app.settings();
    let mut tried: Vec<String> = vec![];
    let mut last = Fail(503, "no available accounts".into());
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
            last = Fail(502, e.to_string());
            continue;
        }
        let mut req = match handshake(&app, &acc, &h, Some(&ws_url(&target))) {
            Ok(r) => r,
            Err(e) => return ApiError::new(ApiFormat::Responses, 500, &e).into_response(),
        };
        if let Some(p) = h.get(header::SEC_WEBSOCKET_PROTOCOL) {
            req.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, p.clone());
        }
        match tokio::time::timeout(
            Duration::from_secs(s.first_event_timeout_secs.max(5)),
            tokio_tungstenite::connect_async(req),
        )
        .await
        {
            Ok(Ok((up, resp))) => {
                let proto = resp
                    .headers()
                    .get(header::SEC_WEBSOCKET_PROTOCOL)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let ws = match proto {
                    Some(p) => ws.protocols([p]),
                    None => ws,
                };
                let link = Link {
                    ws: up,
                    _guard: acc.acquire(),
                    acc,
                };
                return ws.on_upgrade(move |client| relay_live(app, key, link, client, model));
            }
            Ok(Err(tung::Error::Http(resp))) => {
                let status = resp.status().as_u16();
                let v: Value = resp
                    .body()
                    .as_deref()
                    .and_then(|b| serde_json::from_slice(b).ok())
                    .unwrap_or_default();
                let msg = v["error"]["message"]
                    .as_str()
                    .unwrap_or("upstream rejected realtime websocket")
                    .to_string();
                match retry_until(status, &v["error"]) {
                    Some(until) if until > 0 => acc.fail(until, format!("{status}: {msg}")),
                    Some(_) => {}
                    None => return ApiError::new(ApiFormat::Responses, status, &msg).into_response(),
                }
                last = Fail(status, msg);
            }
            Ok(Err(e)) => {
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                last = Fail(502, format!("upstream websocket connect failed: {e}"));
            }
            Err(_) => {
                acc.fail(now_ms() + 15_000, "realtime handshake timed out".into());
                last = Fail(504, "upstream timed out".into());
            }
        }
    }
    ApiError::new(ApiFormat::Responses, last.0, &last.1).into_response()
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
                                .and_then(|v| v["session"]["model"].as_str().map(str::to_string))
                            {
                                model = m;
                            }
                        }
                        "response.done" => {
                            let v: Value = serde_json::from_str(t).unwrap_or_default();
                            let u = &v["response"]["usage"];
                            let usage = json!({
                                "input_tokens": u["input_tokens"], "output_tokens": u["output_tokens"],
                                "input_tokens_details": {"cached_tokens": u["input_token_details"]["cached_tokens"]},
                            });
                            let meta = UsageMeta {
                                key_id: &key.cfg.id,
                                account_id: &up.acc.id,
                                model: &model,
                                endpoint: Endpoint::Realtime,
                                started,
                                ttfb: started,
                            };
                            let row = Usage::new(meta, 200, &usage, &app.settings().pricing);
                            app.record(Some(&key), row);
                        }
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

async fn backend_calls(State(app): State<Arc<App>>, h: HeaderMap, RawQuery(q): RawQuery, body: Bytes) -> Response {
    let target = with_query(format!("{}/codex/realtime/calls", app.settings().upstream_base), q.as_deref());
    calls(app, h, target, body).await
}

async fn v1_calls(State(app): State<Arc<App>>, h: HeaderMap, RawQuery(q): RawQuery, body: Bytes) -> Response {
    let target = with_query(format!("{}/realtime/calls", app.settings().realtime_base), q.as_deref());
    calls(app, h, target, body).await
}

async fn v1_realtime(State(app): State<Arc<App>>, h: HeaderMap, Query(q): Query<Vec<(String, String)>>, ws: WebSocketUpgrade) -> Response {
    let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let (call_id, model) = (get("call_id"), get("model").unwrap_or_else(|| "realtime".into()));
    let mut url = reqwest::Url::parse(&format!("{}/realtime", app.settings().realtime_base)).expect("realtime_base url");
    url.query_pairs_mut().extend_pairs(q.iter());
    live(app, h, ws, call_id, url.to_string(), model).await
}

async fn v1_live(
    State(app): State<Arc<App>>,
    h: HeaderMap,
    Path(call_id): Path<String>,
    RawQuery(q): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    if matches!(call_id.as_str(), "." | "..") || !call_id.chars().all(|c| c.is_ascii_alphanumeric() || "._~-".contains(c)) {
        return ApiError::new(ApiFormat::Responses, 404, "invalid realtime call id").into_response();
    }
    let target = with_query(format!("{}/live/{call_id}", app.settings().realtime_base), q.as_deref());
    live(app, h, ws, Some(call_id), target, "realtime".into()).await
}
