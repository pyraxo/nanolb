use crate::app::{Account, App, Inflight, Key, Provider, Sessions, canonical_model};
use crate::server::auth::api_key;
use crate::server::error::{ApiError, ApiFormat};
use crate::sse;
use crate::store::{Usage, UsageMeta};
use crate::time::now_ms;
use crate::translate::{anthropic, chat};
use crate::upstream::{self, Failure, UpstreamError, classify, codex, limit_reset_from_error};
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

type Events = Pin<Box<dyn Stream<Item = Result<Event, eventsource_stream::EventStreamError<reqwest::Error>>> + Send>>;

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
            crate::id::sha256_hex(&seed)[..24].to_string()
        })
}

struct Upstream {
    acc: Arc<Account>,
    guard: Inflight,
    first: Event,
    events: Events,
    ttfb: i64,
    headers: HeaderMap,
}

const TURN_STATE: &str = "x-codex-turn-state";
const PASS_HEADERS: &[&str] = &["x-request-id", "openai-model", "x-reasoning-included", "x-models-etag"];

fn passthrough_headers(h: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in h {
        let n = k.as_str();
        if PASS_HEADERS.contains(&n)
            || (n.starts_with("x-codex-") && !n.starts_with("x-codex-primary-") && !n.starts_with("x-codex-secondary-"))
        {
            out.append(k.clone(), v.clone());
        }
    }
    out
}

fn is_failure(ev: &Event) -> bool {
    ev.event == "error"
        || ev.event == "response.failed"
        || ev.data.starts_with("{\"type\":\"error\"")
        || ev.data.starts_with("{\"type\":\"response.failed\"")
}

fn upstream_request(app: &App, acc: &Account, provider: Provider, h: Option<&HeaderMap>, cache_key: &str) -> reqwest::RequestBuilder {
    if provider == Provider::OpenRouter {
        return upstream::request(app, acc, reqwest::Method::POST, "/responses", None);
    }
    let rb = upstream::request(app, acc, reqwest::Method::POST, "/codex/responses", h);
    if h.is_some_and(|h| h.contains_key("session_id") || h.contains_key("session-id")) || cache_key.is_empty() {
        return rb;
    }
    rb.header("session_id", cache_key)
}

async fn send(
    app: &Arc<App>,
    key: &Key,
    provider: Provider,
    h: Option<&HeaderMap>,
    up: &Value,
    session: &str,
) -> Result<Upstream, UpstreamError> {
    let raw = serde_json::to_vec(up).unwrap();
    let (body, compressed) = match provider == Provider::Codex && app.settings().compress_upstream {
        true => match zstd::stream::encode_all(&raw[..], 3) {
            Ok(c) => (Bytes::from(c), true),
            Err(_) => (Bytes::from(raw), false),
        },
        false => (Bytes::from(raw), false),
    };
    let cache_key = up["prompt_cache_key"].as_str().unwrap_or_default();
    let first_timeout = std::time::Duration::from_secs(app.settings().first_event_timeout_secs.max(5));
    let mut tried: Vec<String> = vec![];
    let mut refreshed: Vec<String> = vec![];
    let mut last = UpstreamError::NoAccount;
    let mut first_acc: Option<String> = None;
    let stripped = std::cell::OnceCell::new();
    for _ in 0..app.settings().max_attempts.max(1) {
        let Some(acc) = app.pick(provider, &key.cfg.accounts, &tried, Some(session)) else {
            break;
        };
        let first_id = first_acc.get_or_insert_with(|| acc.id.clone());
        let h = match h {
            Some(ch) if *first_id != acc.id && ch.contains_key(TURN_STATE) => Some(stripped.get_or_init(|| {
                let mut c = ch.clone();
                c.remove(TURN_STATE);
                c
            })),
            other => other,
        };
        if let Err(e) = codex::ensure_fresh(app, &acc, false).await {
            tried.push(acc.id.clone());
            last = e;
            continue;
        }
        let guard = acc.acquire();
        let mut req = upstream_request(app, &acc, provider, h, cache_key).header(header::CONTENT_TYPE, "application/json");
        if compressed {
            req = req.header(header::CONTENT_ENCODING, "zstd");
        }
        let req = req.body(body.clone());
        let sent_at = now_ms();
        let res = match tokio::time::timeout(first_timeout, req.send()).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                acc.observe(None);
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                tried.push(acc.id.clone());
                last = UpstreamError::Connect(e);
                continue;
            }
            Err(_) => {
                acc.observe(None);
                acc.fail(now_ms() + 15_000, "timed out waiting for response headers".into());
                tried.push(acc.id.clone());
                last = UpstreamError::Timeout;
                continue;
            }
        };
        if provider == Provider::Codex {
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
                Failure::Client => {
                    tracing::warn!(account = %acc.id, status, error = %msg, "upstream rejected request");
                    return Err(UpstreamError::Status { status, message: msg });
                }
                Failure::Retry(until) => {
                    acc.observe(None);
                    if until > 0 {
                        acc.fail(until, format!("{status}: {msg}"));
                    }
                }
            }
            tracing::warn!(account = %acc.id, status, error = %msg, "upstream error, failing over");
            tried.push(acc.id.clone());
            last = UpstreamError::Status { status, message: msg };
            continue;
        }
        let mut headers = passthrough_headers(res.headers());
        if provider == Provider::Codex {
            for (k, v) in app.accounts.pool_rate_headers(&key.cfg.accounts) {
                if let Ok(v) = HeaderValue::from_str(&v) {
                    headers.insert(k, v);
                }
            }
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
                    Failure::Client => return Err(UpstreamError::Status { status: 400, message: msg }),
                    Failure::Retry(until) => {
                        acc.observe(None);
                        acc.fail(until, msg.clone())
                    }
                }
                tried.push(acc.id.clone());
                last = UpstreamError::FirstEvent(msg);
            }
            Ok(Some(Ok(first))) => {
                let ttfb = now_ms();
                acc.observe(Some(ttfb - sent_at));
                return Ok(Upstream {
                    acc,
                    guard,
                    first,
                    events,
                    ttfb,
                    headers,
                });
            }
            other => {
                let msg = match other {
                    Err(_) => "timed out waiting for first event".to_string(),
                    Ok(e) => format!("upstream closed before first event: {:?}", e.and_then(Result::err)),
                };
                acc.observe(None);
                acc.fail(now_ms() + 10_000, msg.clone());
                tried.push(acc.id.clone());
                last = UpstreamError::FirstEvent(msg);
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
                    if let Some(reset) = limit_reset_from_error(&json!({"error": err})) {
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
    kind: ApiFormat,
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
        let meta = UsageMeta {
            key_id: &self.key.cfg.id,
            account_id: &acc.id,
            model,
            endpoint: self.kind.endpoint(),
            started: self.started,
            ttfb,
        };
        let row = Usage::new(meta, status, &r["usage"], &self.app.settings().pricing);
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
        headers,
    } = up;
    let pass = job.kind == ApiFormat::Responses;
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
            return ApiError::new(job.kind, 502, "upstream stream ended without a response").into_response();
        };
        if resp["output"].as_array().is_none_or(|o| o.is_empty()) && !tap.items.is_empty() {
            resp["output"] = Value::Array(tap.items);
        }
        if resp["status"] == "failed" {
            return ApiError::new(job.kind, 502, resp["error"]["message"].as_str().unwrap_or("upstream failed")).into_response();
        }
        let mut res = axum::Json(opts_json(&resp)).into_response();
        if pass {
            res.headers_mut().extend(headers);
        }
        return res;
    }
    let idle = std::time::Duration::from_secs(job.app.settings().idle_timeout_secs.max(30));
    let keepalive = match job.app.settings().keepalive_secs {
        0 => idle,
        k => std::time::Duration::from_secs(k).min(idle),
    };
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
            let deadline = tokio::time::Instant::now() + idle;
            loop {
                match tokio::time::timeout(keepalive, events.next()).await {
                    Ok(Some(Ok(ev))) => {
                        on_event(&mut tx, &ev, &acc, &mut tap, false, &mut out);
                        break;
                    }
                    Err(_) if tokio::time::Instant::now() < deadline => {
                        if sender.send(Bytes::from_static(b": keepalive\n\n")).await.is_err() {
                            alive = false;
                            break 'outer;
                        }
                    }
                    _ => break 'outer,
                }
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
    if pass {
        h.extend(headers);
    }
    res
}

async fn handle(app: Arc<App>, kind: ApiFormat, h: HeaderMap, body: Bytes) -> Response {
    let started = now_ms();
    let Some(key) = api_key(&app, &h) else {
        return ApiError::new(kind, 401, "invalid or missing api key").into_response();
    };
    if let Err(e) = key.check(started) {
        return ApiError::from((kind, e)).into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return ApiError::new(kind, 400, &format!("invalid json: {e}")).into_response(),
    };
    let requested = req["model"].as_str().unwrap_or_default().to_string();
    let stream = req["stream"].as_bool().unwrap_or(false);
    let (mut up, chat_opts, anth_opts) = match kind {
        ApiFormat::Responses => (req.clone(), None, None),
        ApiFormat::Chat | ApiFormat::OpenRouter => match chat::to_responses(&req, kind == ApiFormat::OpenRouter) {
            Ok((up, opts)) => (up, Some(opts), None),
            Err(e) => return ApiError::from((kind, e)).into_response(),
        },
        ApiFormat::Anthropic => match anthropic::to_responses(&req, canonical_model(&requested)) {
            Ok((up, opts)) => (up, None, Some(opts)),
            Err(e) => return ApiError::from((kind, e)).into_response(),
        },
    };
    let model = canonical_model(up["model"].as_str().unwrap_or(&requested)).to_string();
    if !key.allows_model(&requested) && !key.allows_model(&model) {
        return ApiError::new(kind, 403, &format!("model {requested} not allowed for this key")).into_response();
    }
    let rates = app.settings().pricing.rates(&model);
    let tx = match (chat_opts, anth_opts) {
        (Some(mut opts), _) => {
            opts.rates = rates;
            Tx::Chat(chat::Stream::new(opts))
        }
        (None, Some(opts)) => Tx::Anth(anthropic::Stream::new(opts)),
        (None, None) => Tx::Pass,
    };
    up["model"] = Value::String(model.clone());
    let provider = Provider::for_model(&model);
    if !app.accounts.any(provider) {
        return ApiError::new(kind, 503, &format!("no {provider} accounts connected to serve model {model}")).into_response();
    }
    if provider == Provider::Codex {
        codex::prepare_body(&mut up);
    } else {
        up["stream"] = Value::Bool(true);
        up["usage"] = json!({"include": true});
    }
    let cache = session_key(&h, &req, &up);
    if provider == Provider::Codex && up.get("prompt_cache_key").is_none() {
        up["prompt_cache_key"] = Value::String(cache.clone());
    }
    let session = Sessions::key(&key.cfg.id, provider, &cache);
    if provider == Provider::Codex
        && let Some(acc) = file_owner(&app, &up)
    {
        app.sessions.bind(session.clone(), acc, now_ms());
    }
    let upstream = match send(&app, &key, provider, (kind == ApiFormat::Responses).then_some(&h), &up, &session).await {
        Ok(u) => u,
        Err(e)
            if e.status() == 503
                && let Some(until) = app.accounts.next_available(provider, &key.cfg.accounts) =>
        {
            tracing::warn!(endpoint = kind.endpoint().as_str(), %model, until = %crate::time::ts_iso(until), "all accounts rate limited");
            return ApiError::limited(kind, provider, until).into_response();
        }
        Err(e) => {
            let status = e.status();
            tracing::warn!(endpoint = kind.endpoint().as_str(), %model, status, error = %e, "request failed");
            let meta = UsageMeta {
                key_id: &key.cfg.id,
                account_id: "",
                model: &model,
                endpoint: kind.endpoint(),
                started,
                ttfb: now_ms(),
            };
            app.record(Some(&key), Usage::new(meta, status, &Value::Null, &app.settings().pricing));
            return ApiError::from((kind, e)).into_response();
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
        ApiFormat::Responses => run(job, upstream, tx, stream, |r| r.clone()).await,
        ApiFormat::Chat | ApiFormat::OpenRouter => {
            let or = kind == ApiFormat::OpenRouter;
            let mut opts = chat::to_responses(&req, or).map(|(_, o)| o).unwrap();
            opts.rates = rates;
            run(job, upstream, tx, stream, move |r| chat::from_response(r, &opts)).await
        }
        ApiFormat::Anthropic => {
            let opts = anthropic::to_responses(&req, &model).map(|(_, mut o)| {
                o.model = echo;
                o
            });
            let opts = opts.unwrap();
            run(job, upstream, tx, stream, move |r| anthropic::from_response(r, &opts)).await
        }
    }
}

fn file_owner(app: &App, up: &Value) -> Option<String> {
    up["input"]
        .as_array()?
        .iter()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .find_map(|part| part["file_id"].as_str())
        .and_then(|id| app.files.owner(id))
}

pub async fn responses(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    handle(app, ApiFormat::Responses, h, body).await
}

pub async fn chat_completions(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    handle(app, ApiFormat::Chat, h, body).await
}

pub async fn openrouter(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    handle(app, ApiFormat::OpenRouter, h, body).await
}

pub async fn messages(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    handle(app, ApiFormat::Anthropic, h, body).await
}

pub async fn count_tokens(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    if api_key(&app, &h).is_none() {
        return ApiError::new(ApiFormat::Anthropic, 401, "invalid or missing api key").into_response();
    }
    match serde_json::from_slice::<Value>(&body) {
        Ok(v) => axum::Json(anthropic::count_tokens(&v)).into_response(),
        Err(e) => ApiError::new(ApiFormat::Anthropic, 400, &e.to_string()).into_response(),
    }
}

pub async fn models(State(app): State<Arc<App>>, h: HeaderMap) -> Response {
    let anth = h.contains_key("anthropic-version");
    let kind = if anth { ApiFormat::Anthropic } else { ApiFormat::Chat };
    if api_key(&app, &h).is_none() {
        return ApiError::new(kind, 401, "invalid or missing api key").into_response();
    }
    let created = app.started_at / 1000;
    let body = if anth {
        let data: Vec<Value> = app
            .model_list()
            .into_iter()
            .map(|(m, _)| json!({"type": "model", "id": m, "display_name": m, "created_at": crate::time::ts_iso(app.started_at)}))
            .collect();
        json!({"first_id": data.first().map(|d| d["id"].clone()), "last_id": data.last().map(|d| d["id"].clone()), "has_more": false, "data": data})
    } else {
        json!({"object": "list", "data": app.model_list().into_iter().map(|(m, ctx)| json!({"id": m, "object": "model", "created": created, "owned_by": "openai", "context_length": ctx})).collect::<Vec<_>>()})
    };
    axum::Json(body).into_response()
}

pub async fn openrouter_models(State(app): State<Arc<App>>) -> Response {
    let settings = app.settings();
    let per_tok = |m: &str, i: usize| {
        settings
            .pricing
            .rates(m)
            .map_or_else(|| "0".to_owned(), |r| format!("{}", <[f64; 3]>::from(r)[i] / 1e6))
    };
    let data: Vec<Value> = app
        .model_list()
        .into_iter()
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
    let Some(key) = api_key(&app, &h) else {
        return ApiError::new(ApiFormat::OpenRouter, 401, "invalid or missing api key").into_response();
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
    let Some(key) = api_key(&app, &h) else {
        return ApiError::new(ApiFormat::Responses, 401, "invalid or missing api key").into_response();
    };
    let Some(acc) = app.pick(Provider::Codex, &key.cfg.accounts, &[], None) else {
        return ApiError::new(ApiFormat::Responses, 503, "no available accounts").into_response();
    };
    if let Err(e) = codex::ensure_fresh(&app, &acc, false).await {
        return ApiError::new(ApiFormat::Responses, 502, e.to_string()).into_response();
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
        Err(e) => ApiError::new(ApiFormat::Responses, 502, &e.to_string()).into_response(),
    }
}
