use crate::codex;
use crate::state::{Account, App, Key};
use crate::translate::{anthropic, chat};
use crate::{now_ms, sse};
use axum::body::{Body, Bytes};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use eventsource_stream::{Event, Eventsource};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

type Events = Pin<Box<dyn Stream<Item = Result<Event, eventsource_stream::EventStreamError<reqwest::Error>>> + Send>>;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Responses,
    Chat,
    OpenRouter,
    Anthropic,
}

impl Kind {
    fn endpoint(self) -> &'static str {
        match self {
            Kind::Responses => "responses",
            Kind::Chat => "chat",
            Kind::OpenRouter => "openrouter",
            Kind::Anthropic => "anthropic",
        }
    }
}

pub fn error(kind: Kind, status: u16, msg: &str) -> Response {
    let body = match kind {
        Kind::Anthropic => anthropic::error_body(status, msg),
        Kind::OpenRouter => json!({"error": {"code": status, "message": msg}}),
        _ => json!({"error": {"message": msg, "type": if status == 429 { "rate_limit_error" } else { "api_error" }, "code": Value::Null}}),
    };
    (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), axum::Json(body)).into_response()
}

pub fn auth(app: &App, h: &HeaderMap) -> Option<Arc<Key>> {
    let bearer = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let secret = bearer.or_else(|| h.get("x-api-key").and_then(|v| v.to_str().ok()))?;
    app.key_by_secret(secret.trim())
}

fn session_key(h: &HeaderMap, client: &Value, up: &Value) -> String {
    let hdr = ["session_id", "session-id", "thread-id", "conversation_id"]
        .iter()
        .find_map(|n| h.get(*n).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()));
    hdr.map(str::to_string)
        .or_else(|| up["prompt_cache_key"].as_str().map(str::to_string))
        .or_else(|| client["metadata"]["user_id"].as_str().map(str::to_string))
        .unwrap_or_else(|| {
            let mut seed = up["instructions"].to_string();
            if let Some(items) = up["input"].as_array() {
                for it in items.iter().take(2) {
                    seed.push_str(&it.to_string());
                }
            }
            crate::sha256_hex(&seed)[..24].to_string()
        })
}

pub struct Inflight(pub Arc<Account>);

impl Drop for Inflight {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Relaxed);
    }
}

struct Upstream {
    acc: Arc<Account>,
    guard: Inflight,
    first: Event,
    events: Events,
    ttfb: i64,
}

fn is_failure(ev: &Event) -> bool {
    ev.event == "error"
        || ev.event == "response.failed"
        || ev.data.starts_with("{\"type\":\"error\"")
        || ev.data.starts_with("{\"type\":\"response.failed\"")
}

fn upstream_request(app: &App, acc: &Account, provider: &str, h: Option<&HeaderMap>, cache_key: &str) -> reqwest::RequestBuilder {
    if provider == "openrouter" {
        return app
            .http
            .post(format!("{}/responses", app.settings().openrouter_base))
            .bearer_auth(&acc.cfg.read().access_token)
            .header("http-referer", "https://github.com/pyraxo/nanolb")
            .header("x-title", "nanolb");
    }
    let rb = codex::request(app, acc, reqwest::Method::POST, "/codex/responses", h);
    if h.is_some_and(|h| h.contains_key("session_id") || h.contains_key("session-id")) || cache_key.is_empty() {
        return rb;
    }
    rb.header("session_id", cache_key)
}

enum Failure {
    Retry(i64),
    Client,
}

fn classify(status: u16, err: &Value) -> Failure {
    let code = err["code"].as_str().or(err["type"].as_str()).unwrap_or_default();
    let reset = codex::limit_reset_from_error(&json!({ "error": err }));
    match status {
        429 => Failure::Retry(reset.unwrap_or(now_ms() + 60_000)),
        401 => Failure::Retry(now_ms() + 300_000),
        403 => Failure::Retry(0),
        500.. => Failure::Retry(now_ms() + 5_000),
        0 if reset.is_some() => Failure::Retry(reset.unwrap()),
        0 if code.contains("rate_limit")
            || code.contains("usage_limit")
            || code.contains("overloaded")
            || code.contains("server_error") =>
        {
            Failure::Retry(now_ms() + 5_000)
        }
        _ => Failure::Client,
    }
}

async fn send(
    app: &Arc<App>,
    key: &Key,
    provider: &str,
    h: Option<&HeaderMap>,
    up: &Value,
    session: &str,
) -> Result<Upstream, (u16, String)> {
    let body = Bytes::from(serde_json::to_vec(up).unwrap());
    let cache_key = up["prompt_cache_key"].as_str().unwrap_or_default();
    let first_timeout = std::time::Duration::from_secs(app.settings().first_event_timeout_secs.max(5));
    let mut tried: Vec<String> = vec![];
    let mut refreshed: Vec<String> = vec![];
    let mut last = (503u16, "no available accounts".to_string());
    for _ in 0..app.settings().max_attempts.max(1) {
        let Some(acc) = app.pick(provider, &key.cfg.accounts, &tried, Some(session)) else {
            break;
        };
        if let Err(e) = codex::ensure_fresh(app, &acc, false).await {
            tried.push(acc.id.clone());
            last = (502, e);
            continue;
        }
        acc.inflight.fetch_add(1, Relaxed);
        let guard = Inflight(acc.clone());
        let req = upstream_request(app, &acc, provider, h, cache_key)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.clone());
        let res = match tokio::time::timeout(first_timeout, req.send()).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                tried.push(acc.id.clone());
                last = (502, format!("upstream connect failed: {e}"));
                continue;
            }
            Err(_) => {
                acc.fail(now_ms() + 15_000, "timed out waiting for response headers".into());
                tried.push(acc.id.clone());
                last = (504, "upstream timed out".into());
                continue;
            }
        };
        if provider == "codex" {
            codex::apply_rate_headers(&acc, res.headers());
        }
        let status = res.status().as_u16();
        if status == 401 && !refreshed.contains(&acc.id) {
            refreshed.push(acc.id.clone());
            if codex::ensure_fresh(app, &acc, true).await.is_ok() {
                continue;
            }
        }
        if status >= 400 {
            let text = res.text().await.unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or_default();
            let msg = v["error"]["message"]
                .as_str()
                .or(v["detail"].as_str())
                .map_or_else(|| text.chars().take(500).collect(), str::to_string);
            match classify(status, &v["error"]) {
                Failure::Client => return Err((status, msg)),
                Failure::Retry(until) => {
                    if until > 0 {
                        acc.fail(until, format!("{status}: {msg}"));
                    }
                }
            }
            tracing::warn!(account = %acc.id, status, error = %msg, "upstream error, failing over");
            tried.push(acc.id.clone());
            last = (status, msg);
            continue;
        }
        let mut events: Events = Box::pin(res.bytes_stream().eventsource());
        match tokio::time::timeout(first_timeout, events.next()).await {
            Ok(Some(Ok(ev))) if is_failure(&ev) => {
                let v: Value = serde_json::from_str(&ev.data).unwrap_or_default();
                let err = if v["error"].is_object() {
                    v["error"].clone()
                } else if v["response"]["error"].is_object() {
                    v["response"]["error"].clone()
                } else {
                    v.clone()
                };
                let msg = err["message"].as_str().unwrap_or("upstream failed").to_string();
                match classify(0, &err) {
                    Failure::Client => return Err((400, msg)),
                    Failure::Retry(until) => acc.fail(until, msg.clone()),
                }
                tried.push(acc.id.clone());
                last = (502, msg);
            }
            Ok(Some(Ok(first))) => {
                return Ok(Upstream {
                    acc,
                    guard,
                    first,
                    events,
                    ttfb: now_ms(),
                });
            }
            other => {
                let msg = match other {
                    Err(_) => "timed out waiting for first event".to_string(),
                    Ok(e) => format!("upstream closed before first event: {:?}", e.and_then(Result::err)),
                };
                acc.fail(now_ms() + 10_000, msg.clone());
                tried.push(acc.id.clone());
                last = (502, msg);
            }
        }
    }
    Err(last)
}

enum Tx {
    Pass,
    Chat(chat::Stream),
    Anth(anthropic::Stream),
}

#[derive(Default)]
struct Tap {
    response: Option<Value>,
    items: Vec<Value>,
}

fn on_event(tx: &mut Tx, ev: &Event, acc: &Account, tap: &mut Tap, collect: bool, out: &mut Vec<Bytes>) {
    let interesting = matches!(
        ev.event.as_str(),
        "response.completed" | "response.failed" | "response.incomplete" | "codex.rate_limits" | "error" | "message"
    ) || (collect && ev.event == "response.output_item.done");
    let parsed: Option<Value> = if interesting || !matches!(tx, Tx::Pass) {
        serde_json::from_str(&ev.data).ok()
    } else {
        None
    };
    if let Some(v) = &parsed {
        match v["type"].as_str().unwrap_or_default() {
            "response.completed" | "response.incomplete" | "response.failed" => {
                tap.response = Some(v["response"].clone());
                if v["type"] == "response.failed" {
                    let err = &v["response"]["error"];
                    if let Some(reset) = codex::limit_reset_from_error(&json!({"error": err})) {
                        acc.fail(reset, err["message"].as_str().unwrap_or("rate limited").into());
                    }
                }
            }
            "codex.rate_limits" => codex::apply_rate_event(acc, v),
            "response.output_item.done" if collect => tap.items.push(v["item"].clone()),
            _ => {}
        }
    }
    if collect {
        return;
    }
    match tx {
        Tx::Pass => out.push(sse::frame((ev.event != "message").then_some(ev.event.as_str()), &ev.data)),
        Tx::Chat(s) => {
            if let Some(v) = &parsed {
                s.event(v, out)
            }
        }
        Tx::Anth(s) => {
            if let Some(v) = &parsed {
                s.event(v, out)
            }
        }
    }
}

struct Job {
    app: Arc<App>,
    key: Arc<Key>,
    kind: Kind,
    model: String,
    started: i64,
}

impl Job {
    fn record(&self, acc: &Account, resp: Option<&Value>, ttfb: i64) {
        let status = match resp.and_then(|r| r["status"].as_str()) {
            Some("completed") => 200,
            Some("incomplete") => 206,
            Some(_) => 502,
            None => 499,
        };
        let empty = Value::Null;
        let r = resp.unwrap_or(&empty);
        let model = r["model"].as_str().unwrap_or(&self.model);
        let row = codex::usage_row(
            &self.key.cfg.id,
            &acc.id,
            model,
            self.kind.endpoint(),
            status,
            &r["usage"],
            self.started,
            ttfb,
        );
        self.app.record(Some(&self.key), row);
    }
}

async fn run(job: Job, up: Upstream, mut tx: Tx, stream: bool, opts_json: impl FnOnce(&Value) -> Value + Send + 'static) -> Response {
    let Upstream {
        acc,
        guard,
        first,
        mut events,
        ttfb,
    } = up;
    if !stream {
        let mut tap = Tap::default();
        let mut sink = vec![];
        on_event(&mut tx, &first, &acc, &mut tap, true, &mut sink);
        let idle = std::time::Duration::from_secs(job.app.settings().idle_timeout_secs.max(30));
        while let Ok(Some(Ok(ev))) = tokio::time::timeout(idle, events.next()).await {
            on_event(&mut tx, &ev, &acc, &mut tap, true, &mut sink);
        }
        drop(guard);
        job.record(&acc, tap.response.as_ref(), ttfb);
        let Some(mut resp) = tap.response else {
            return error(job.kind, 502, "upstream stream ended without a response");
        };
        if resp["output"].as_array().is_none_or(|o| o.is_empty()) && !tap.items.is_empty() {
            resp["output"] = Value::Array(tap.items);
        }
        if resp["status"] == "failed" {
            return error(job.kind, 502, resp["error"]["message"].as_str().unwrap_or("upstream failed"));
        }
        return axum::Json(opts_json(&resp)).into_response();
    }
    let idle = std::time::Duration::from_secs(job.app.settings().idle_timeout_secs.max(30));
    let (sender, rx) = tokio::sync::mpsc::channel::<Bytes>(128);
    tokio::spawn(async move {
        let _guard = guard;
        let mut tap = Tap::default();
        let mut out = Vec::with_capacity(8);
        on_event(&mut tx, &first, &acc, &mut tap, false, &mut out);
        let mut alive = true;
        'outer: loop {
            for b in out.drain(..) {
                if sender.send(b).await.is_err() {
                    alive = false;
                    break 'outer;
                }
            }
            match tokio::time::timeout(idle, events.next()).await {
                Ok(Some(Ok(ev))) => on_event(&mut tx, &ev, &acc, &mut tap, false, &mut out),
                _ => break,
            }
        }
        if alive {
            match &mut tx {
                Tx::Chat(s) => s.finish(&mut out),
                Tx::Anth(s) => s.finish(&mut out),
                Tx::Pass => {}
            }
            for b in out.drain(..) {
                let _ = sender.send(b).await;
            }
        }
        job.record(&acc, tap.response.as_ref(), ttfb);
    });
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|b| (Ok::<_, Infallible>(b), rx))
    }));
    let mut res = Response::new(body);
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    res
}

async fn handle(app: Arc<App>, kind: Kind, h: HeaderMap, body: Bytes) -> Response {
    let started = now_ms();
    let Some(key) = auth(&app, &h) else {
        return error(kind, 401, "invalid or missing api key");
    };
    if let Err(e) = key.check(started) {
        return error(kind, 429, &e);
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error(kind, 400, &format!("invalid json: {e}")),
    };
    let requested = req["model"].as_str().unwrap_or_default().to_string();
    let stream = req["stream"].as_bool().unwrap_or(false);
    if App::provider_for(&requested) == "claude" {
        if kind != Kind::Anthropic {
            return error(
                kind,
                400,
                &format!("{requested} is served on POST /v1/messages (Anthropic Messages format)"),
            );
        }
        if !key.allows_model(&requested) {
            return error(kind, 403, &format!("model {requested} not allowed for this key"));
        }
        if !app.accounts.read().iter().any(|a| a.cfg.read().kind == "claude") {
            return error(kind, 503, &format!("no claude accounts connected to serve model {requested}"));
        }
        return crate::claude::handle(app, key, h, req, started).await;
    }
    let (mut up, tx) = match kind {
        Kind::Responses => (req.clone(), Tx::Pass),
        Kind::Chat | Kind::OpenRouter => match chat::to_responses(&req, kind == Kind::OpenRouter) {
            Ok((up, opts)) => (up, Tx::Chat(chat::Stream::new(opts))),
            Err(e) => return error(kind, 400, &e),
        },
        Kind::Anthropic => match anthropic::to_responses(&req, App::canonical_model(&requested)) {
            Ok((up, opts)) => (up, Tx::Anth(anthropic::Stream::new(opts))),
            Err(e) => return error(kind, 400, &e),
        },
    };
    let model = App::canonical_model(up["model"].as_str().unwrap_or(&requested)).to_string();
    if !key.allows_model(&requested) && !key.allows_model(&model) {
        return error(kind, 403, &format!("model {requested} not allowed for this key"));
    }
    up["model"] = Value::String(model.clone());
    let provider = App::provider_for(&model);
    if !app.accounts.read().iter().any(|a| a.cfg.read().kind == provider) {
        return error(kind, 503, &format!("no {provider} accounts connected to serve model {model}"));
    }
    if provider == "codex" {
        codex::prepare_body(&mut up);
    } else {
        up["stream"] = Value::Bool(true);
        up["usage"] = json!({"include": true});
    }
    let cache = session_key(&h, &req, &up);
    if provider == "codex" && up.get("prompt_cache_key").is_none() {
        up["prompt_cache_key"] = Value::String(cache.clone());
    }
    let session = format!("{}\u{1f}{provider}\u{1f}{cache}", key.cfg.id);
    let upstream = match send(&app, &key, provider, (kind == Kind::Responses).then_some(&h), &up, &session).await {
        Ok(u) => u,
        Err((status, msg)) => {
            tracing::warn!(endpoint = kind.endpoint(), %model, status, error = %msg, "request failed");
            app.record(
                Some(&key),
                codex::usage_row(&key.cfg.id, "", &model, kind.endpoint(), status, &Value::Null, started, now_ms()),
            );
            return error(kind, status, &msg);
        }
    };
    let job = Job {
        app,
        key,
        kind,
        model: model.clone(),
        started,
    };
    let echo = requested.clone();
    match kind {
        Kind::Responses => run(job, upstream, tx, stream, |r| r.clone()).await,
        Kind::Chat | Kind::OpenRouter => {
            let or = kind == Kind::OpenRouter;
            let opts = chat::to_responses(&req, or).map(|(_, o)| o).unwrap();
            run(job, upstream, tx, stream, move |r| chat::from_response(r, &opts)).await
        }
        Kind::Anthropic => {
            let opts = anthropic::to_responses(&req, &model).map(|(_, mut o)| {
                o.model = echo;
                o
            });
            let opts = opts.unwrap();
            run(job, upstream, tx, stream, move |r| anthropic::from_response(r, &opts)).await
        }
    }
}

pub async fn require_key(State(app): State<Arc<App>>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if auth(&app, req.headers()).is_none() {
        let p = req.uri().path();
        let kind = if p.starts_with("/v1/messages") || req.headers().contains_key("anthropic-version") {
            Kind::Anthropic
        } else if p.starts_with("/api/") {
            Kind::OpenRouter
        } else {
            Kind::Chat
        };
        return error(kind, 401, "invalid or missing api key");
    }
    next.run(req).await
}

pub async fn responses(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    handle(app, Kind::Responses, h, body).await
}

pub async fn chat_completions(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    handle(app, Kind::Chat, h, body).await
}

pub async fn openrouter(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    handle(app, Kind::OpenRouter, h, body).await
}

pub async fn messages(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    handle(app, Kind::Anthropic, h, body).await
}

pub async fn count_tokens(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    if auth(&app, &h).is_none() {
        return error(Kind::Anthropic, 401, "invalid or missing api key");
    }
    match serde_json::from_slice::<Value>(&body) {
        Ok(v) => axum::Json(anthropic::count_tokens(&v)).into_response(),
        Err(e) => error(Kind::Anthropic, 400, &e.to_string()),
    }
}

pub async fn models(State(app): State<Arc<App>>, h: HeaderMap) -> Response {
    let anth = h.contains_key("anthropic-version");
    let kind = if anth { Kind::Anthropic } else { Kind::Chat };
    if auth(&app, &h).is_none() {
        return error(kind, 401, "invalid or missing api key");
    }
    let created = app.started_at / 1000;
    let body = if anth {
        let data: Vec<Value> = app
            .model_list()
            .into_iter()
            .filter(|(m, _)| App::provider_for(m) == "claude")
            .map(|(m, _)| json!({"type": "model", "id": m, "display_name": m, "created_at": crate::ts_iso(app.started_at)}))
            .collect();
        json!({"first_id": data.first().map(|d| d["id"].clone()), "last_id": data.last().map(|d| d["id"].clone()), "has_more": false, "data": data})
    } else {
        json!({"object": "list", "data": app.model_list().into_iter().filter(|(m, _)| App::provider_for(m) != "claude").map(|(m, ctx)| json!({"id": m, "object": "model", "created": created, "owned_by": "openai", "context_length": ctx})).collect::<Vec<_>>()})
    };
    axum::Json(body).into_response()
}

pub async fn openrouter_models(State(app): State<Arc<App>>) -> Response {
    let per_tok = |m: &str, i: usize| crate::pricing::rates(m).map_or("0".to_string(), |r| format!("{}", r[i] / 1e6));
    let data: Vec<Value> = app
        .model_list()
        .into_iter()
        .filter(|(m, _)| App::provider_for(m) != "claude")
        .map(|(m, ctx)| {
            let m = m.as_str();
            let id = if m.contains('/') { m.to_string() } else { format!("openai/{m}") };
            json!({
                "id": id, "canonical_slug": id, "name": id,
                "created": app.started_at / 1000, "description": "", "context_length": ctx,
                "architecture": {"modality": "text+image->text", "input_modalities": ["text", "image"], "output_modalities": ["text"], "tokenizer": "GPT", "instruct_type": null},
                "pricing": {"prompt": per_tok(m, 0), "completion": per_tok(m, 2), "input_cache_read": per_tok(m, 1), "request": "0", "image": "0", "web_search": "0", "internal_reasoning": "0"},
                "top_provider": {"context_length": ctx, "max_completion_tokens": 128000, "is_moderated": false},
                "per_request_limits": null,
                "supported_parameters": ["tools", "tool_choice", "reasoning", "include_reasoning", "response_format", "structured_outputs", "max_tokens"],
            })
        })
        .collect();
    axum::Json(json!({"data": data})).into_response()
}

pub async fn openrouter_key(State(app): State<Arc<App>>, h: HeaderMap) -> Response {
    let Some(key) = auth(&app, &h) else {
        return error(Kind::OpenRouter, 401, "invalid or missing api key");
    };
    let v = key.view();
    let u = &v["usage"];
    let limit = key.cfg.limits.iter().find_map(|l| l.max_cost_usd);
    let total = u["total"]["cost"].as_f64().unwrap_or(0.0);
    axum::Json(json!({"data": {
        "label": key.cfg.prefix, "limit": limit, "limit_remaining": limit.map(|l| (l - total).max(0.0)), "limit_reset": null,
        "usage": total, "usage_daily": u["day"]["cost"], "usage_weekly": u["week"]["cost"], "usage_monthly": u["month"]["cost"],
        "byok_usage": 0, "is_free_tier": false, "is_management_key": false, "is_provisioning_key": false,
        "expires_at": v["expires_at"], "rate_limit": {"requests": -1, "interval": "10s"},
    }}))
    .into_response()
}

pub async fn codex_models(State(app): State<Arc<App>>, h: HeaderMap, RawQuery(q): RawQuery) -> Response {
    let Some(key) = auth(&app, &h) else {
        return error(Kind::Responses, 401, "invalid or missing api key");
    };
    let Some(acc) = app.pick("codex", &key.cfg.accounts, &[], None) else {
        return error(Kind::Responses, 503, "no available accounts");
    };
    if let Err(e) = codex::ensure_fresh(&app, &acc, false).await {
        return error(Kind::Responses, 502, &e);
    }
    let path = format!(
        "/codex/models?{}",
        q.unwrap_or_else(|| format!("client_version={}", app.settings().codex_version))
    );
    match codex::request(&app, &acc, reqwest::Method::GET, &path, Some(&h))
        .header(header::ACCEPT, "application/json")
        .send()
        .await
    {
        Ok(r) => {
            let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let ct = r.headers().get(header::CONTENT_TYPE).cloned();
            let bytes = r.bytes().await.unwrap_or_default();
            let mut res = (status, bytes).into_response();
            if let Some(ct) = ct {
                res.headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_bytes(ct.as_bytes()).unwrap());
            }
            res
        }
        Err(e) => error(Kind::Responses, 502, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use crate::state::{AccountCfg, App, KeyCfg, Settings};
    use crate::store::Store;
    use axum::routing::post;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SSE_OK: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"model\":\"gpt-5.4\",\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"total_tokens\":12}}}\n\n";

    async fn mock_upstream(hits_b: Arc<AtomicUsize>) -> String {
        let app = axum::Router::new().route(
            "/codex/responses",
            post(move |h: axum::http::HeaderMap| {
                let hits_b = hits_b.clone();
                async move {
                    if h["chatgpt-account-id"] == "acct-a" {
                        return (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":600}}"#.to_string(),
                        );
                    }
                    if h.get("session_id").is_none_or(|v| v != "conv-1") {
                        return (axum::http::StatusCode::BAD_REQUEST, "missing cache session".to_string());
                    }
                    hits_b.fetch_add(1, Ordering::SeqCst);
                    (axum::http::StatusCode::OK, SSE_OK.to_string())
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn rate_limited_account_fails_over_and_cache_session_sticks_to_survivor() {
        let hits_b = Arc::new(AtomicUsize::new(0));
        let base = mock_upstream(hits_b.clone()).await;
        let dir = std::env::temp_dir().join(format!("nanolb-proxy-{}", crate::random_hex(4)));
        let app = App::new(Store::open(&dir).unwrap(), "admin".into());
        app.save_settings(Settings {
            upstream_base: base,
            ..Default::default()
        });
        let far = crate::now_ms() + 86_400_000;
        for (id, priority) in [("a", 0), ("b", 1)] {
            app.upsert_account(AccountCfg {
                id: id.into(),
                kind: "codex".into(),
                enabled: true,
                priority,
                chatgpt_account_id: format!("acct-{id}"),
                expires_at: far,
                last_refresh: crate::now_ms(),
                ..Default::default()
            });
        }
        app.save_key(KeyCfg {
            id: "k".into(),
            name: "k".into(),
            hash: crate::sha256_hex("nlb-test"),
            prefix: "nlb-test".into(),
            accounts: vec![],
            models: vec![],
            limits: vec![],
            enabled: true,
            expires_at: None,
            created_at: 0,
        });
        let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", srv.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(srv, crate::router(app)).await.unwrap() });
        let http = reqwest::Client::new();
        let body = serde_json::json!({"model": "gpt-5.4", "input": "hi", "stream": true, "prompt_cache_key": "conv-1"});

        for _ in 0..2 {
            let res = http.post(&url).bearer_auth("nlb-test").json(&body).send().await.unwrap();
            assert_eq!(res.status(), 200);
            assert!(res.text().await.unwrap().contains("\"delta\":\"hello\""));
        }

        assert_eq!(hits_b.load(Ordering::SeqCst), 2);
    }
}
