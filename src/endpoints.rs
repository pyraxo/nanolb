use crate::proxy::{self, Inflight, Kind};
use crate::state::{Account, App, Key, Window};
use crate::{codex, now_ms};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

const TRANSCRIBE_MODEL: &str = "gpt-4o-transcribe";
const DEFAULT_IMAGE_MODEL: &str = "gpt-image-2";
const IMAGE_INSTRUCTIONS: &str = "You are an image generator. When asked, you MUST call the image_generation tool exactly once and return only that tool call. Do not produce any additional text output. Mirror the user's request verbatim into the tool's prompt argument.";
const MASK_HINT: &str = "\n\n(The final attached image is a transparent mask: only modify the regions where the mask is non-transparent.)";

static FILES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/backend-api/wham/usage", get(pool_usage))
        .route("/api/codex/usage", get(pool_usage))
        .route("/v1/usage", get(key_usage))
        .route("/backend-api/files", post(files_create))
        .route("/backend-api/files/{id}/uploaded", post(files_finalize))
        .route("/backend-api/transcribe", post(transcribe_backend))
        .route("/v1/audio/transcriptions", post(transcribe_v1))
        .route("/v1/images/generations", post(images_generations))
        .route("/backend-api/codex/images/generations", post(images_generations))
        .route("/v1/images/edits", post(images_edits_multipart))
        .route("/backend-api/codex/images/edits", post(images_edits_json))
        .route("/v1/embeddings", post(embeddings))
}

pub fn file_account(app: &App, id: &str) -> Option<String> {
    FILES
        .lock()
        .get_or_insert_with(|| {
            app.store
                .load_raw("file")
                .into_iter()
                .filter_map(|(k, v)| Some((k, v["a"].as_str()?.to_string())))
                .collect()
        })
        .get(id)
        .cloned()
}

fn bind_file(app: &App, id: &str, account: &str) {
    file_account(app, id);
    if let Some(m) = FILES.lock().as_mut() {
        m.insert(id.to_string(), account.to_string());
    }
    app.store.put_async("file", id, &json!({"a": account, "t": now_ms()}));
}

fn prelude(app: &App, h: &HeaderMap) -> Result<Arc<Key>, Box<Response>> {
    let key = proxy::auth(app, h).ok_or_else(|| Box::new(proxy::error(Kind::Chat, 401, "invalid or missing api key")))?;
    key.check(now_ms()).map_err(|e| Box::new(proxy::error(Kind::Chat, 429, &e)))?;
    Ok(key)
}

#[allow(clippy::too_many_arguments)]
fn record(app: &App, key: &Key, account: &str, model: &str, endpoint: &'static str, status: u16, usage: &Value, started: i64, ttfb: i64) {
    app.record(
        Some(key),
        codex::usage_row(&key.cfg.id, account, model, endpoint, status, usage, started, ttfb),
    );
}

struct Call<'a> {
    provider: &'static str,
    method: reqwest::Method,
    path: &'a str,
    accept: &'static str,
    pinned: Option<&'a str>,
}

struct Sent {
    acc: Arc<Account>,
    res: reqwest::Response,
    guard: Inflight,
    ttfb: i64,
}

type Shape<'a> = &'a (dyn Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Sync);

async fn forward(app: &Arc<App>, key: &Key, c: &Call<'_>, shape: Shape<'_>) -> Result<Sent, (u16, String)> {
    let first_timeout = Duration::from_secs(app.settings().first_event_timeout_secs.max(5));
    let mut tried: Vec<String> = vec![];
    let mut refreshed: Vec<String> = vec![];
    let mut last = (503u16, "no available accounts".to_string());
    for _ in 0..app.settings().max_attempts.max(1) {
        let acc = match c.pinned {
            Some(id) => {
                if tried.iter().any(|t| t == id) {
                    break;
                }
                let acc = app
                    .account(id)
                    .filter(|a| key.cfg.accounts.is_empty() || key.cfg.accounts.contains(&a.id));
                if acc.is_none() {
                    last = (404, "the account that owns this file is not available".into());
                }
                acc
            }
            None => app.pick(c.provider, &key.cfg.accounts, &tried, None),
        };
        let Some(acc) = acc else { break };
        if let Err(e) = codex::ensure_fresh(app, &acc, false).await {
            tried.push(acc.id.clone());
            last = (502, e);
            continue;
        }
        acc.inflight.fetch_add(1, Relaxed);
        let guard = Inflight(acc.clone());
        let rb = if c.provider == "openrouter" {
            app.http
                .request(c.method.clone(), format!("{}{}", app.settings().openrouter_base, c.path))
                .bearer_auth(&acc.cfg.read().access_token)
                .header("http-referer", "https://github.com/pyraxo/nanolb")
                .header("x-title", "nanolb")
        } else {
            codex::request(app, &acc, c.method.clone(), c.path, None)
        };
        let mut req = match shape(rb).build() {
            Ok(r) => r,
            Err(e) => return Err((500, format!("build request: {e}"))),
        };
        req.headers_mut().insert(header::ACCEPT, HeaderValue::from_static(c.accept));
        let res = match tokio::time::timeout(first_timeout, app.http.execute(req)).await {
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
        if c.provider == "codex" {
            codex::apply_rate_headers(&acc, res.headers());
        }
        let status = res.status().as_u16();
        if status == 401 && !refreshed.contains(&acc.id) {
            refreshed.push(acc.id.clone());
            if codex::ensure_fresh(app, &acc, true).await.is_ok() {
                continue;
            }
        }
        if matches!(status, 401 | 403 | 429 | 500..=599) {
            let text = res.text().await.unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or_default();
            let msg = v["error"]["message"]
                .as_str()
                .or(v["detail"].as_str())
                .map_or_else(|| text.chars().take(500).collect(), str::to_string);
            let until = match status {
                429 => codex::limit_reset_from_error(&v).unwrap_or(now_ms() + 60_000),
                401 => now_ms() + 300_000,
                403 => 0,
                _ => now_ms() + 5_000,
            };
            if until > 0 {
                acc.fail(until, format!("{status}: {msg}"));
            }
            tracing::warn!(account = %acc.id, status, error = %msg, "upstream error, failing over");
            tried.push(acc.id.clone());
            last = (status, msg);
            continue;
        }
        return Ok(Sent {
            acc,
            res,
            guard,
            ttfb: now_ms(),
        });
    }
    Err(last)
}

#[allow(clippy::too_many_arguments)]
fn failed(
    app: &App,
    key: &Key,
    provider: &str,
    endpoint: &'static str,
    model: &str,
    started: i64,
    (status, msg): (u16, String),
) -> Response {
    tracing::warn!(endpoint, status, error = %msg, "request failed");
    record(app, key, "", model, endpoint, status, &Value::Null, started, now_ms());
    if status == 503
        && let Some(until) = app.next_available(provider, &key.cfg.accounts)
    {
        return proxy::limited(Kind::Chat, provider, until);
    }
    proxy::error(Kind::Chat, status, &msg)
}

fn pass(sent: Sent) -> Response {
    let Sent { res, guard, .. } = sent;
    let status = StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let ct = res.headers().get(header::CONTENT_TYPE).cloned();
    let body = Body::from_stream(res.bytes_stream().map(move |c| {
        let _ = &guard;
        c
    }));
    let mut out = Response::new(body);
    *out.status_mut() = status;
    if let Some(ct) = ct {
        out.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_bytes(ct.as_bytes()).unwrap());
    }
    out
}

fn json_response(status: u16, body: Bytes) -> Response {
    let mut r = (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), body).into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

fn window_value(vals: &[(f64, i64, i64)], now: i64) -> Value {
    let known: Vec<_> = vals.iter().filter(|v| v.1 > 0).collect();
    if known.is_empty() {
        return Value::Null;
    }
    let used = known.iter().map(|v| if v.2 > now { v.0 } else { 0.0 }).sum::<f64>() / known.len() as f64;
    let reset = known.iter().map(|v| v.2).filter(|r| *r > now).min().unwrap_or(0);
    json!({
        "used_percent": used,
        "limit_window_seconds": known.iter().map(|v| v.1).max().unwrap_or(0) * 60,
        "reset_after_seconds": if reset > 0 { (reset - now) / 1000 } else { 0 },
        "reset_at": reset / 1000,
    })
}

fn pool(app: &App, key: &Key) -> (Value, Value, bool, String) {
    let now = now_ms();
    let accs: Vec<Arc<Account>> = app
        .accounts
        .read()
        .iter()
        .filter(|a| {
            let c = a.cfg.read();
            c.kind == "codex" && c.enabled && (key.cfg.accounts.is_empty() || key.cfg.accounts.contains(&a.id))
        })
        .cloned()
        .collect();
    let (mut p, mut s) = (vec![], vec![]);
    for a in &accs {
        let r = a.rl.lock();
        p.push((r.primary_used, r.primary_window_min, r.primary_reset_at));
        s.push((r.secondary_used, r.secondary_window_min, r.secondary_reset_at));
    }
    let plan = accs.first().map_or("plus".to_string(), |a| a.cfg.read().plan.clone());
    (
        window_value(&p, now),
        window_value(&s, now),
        accs.iter().any(|a| a.available(now)),
        plan,
    )
}

async fn pool_usage(State(app): State<Arc<App>>, h: HeaderMap) -> Response {
    let key = match prelude(&app, &h) {
        Ok(k) => k,
        Err(r) => return *r,
    };
    let (primary, secondary, allowed, plan) = pool(&app, &key);
    axum::Json(json!({
        "plan_type": plan,
        "rate_limit": {
            "allowed": allowed, "limit_reached": !allowed,
            "primary_window": primary, "secondary_window": secondary,
        },
        "credits": null,
        "additional_rate_limits": [],
    }))
    .into_response()
}

fn key_limits(key: &Key) -> Vec<Value> {
    let usage = key.view()["usage"].clone();
    let mut out = vec![];
    for l in &key.cfg.limits {
        let c = &usage[format!("{:?}", l.window).to_lowercase()];
        let start = c["start"].as_i64().unwrap_or(0);
        let reset = match l.window {
            Window::Day => Some(start + 86_400_000),
            Window::Week => Some(start + 7 * 86_400_000),
            Window::Month => Some(Window::Month.start(start + 32 * 86_400_000)),
            Window::Total => None,
        };
        let dims = [
            ("total_tokens", l.max_tokens, c["tokens"].as_u64()),
            (
                "cost_usd",
                l.max_cost_usd.map(|m| (m * 1e6) as u64),
                c["cost"].as_f64().map(|x| (x * 1e6) as u64),
            ),
            ("requests", l.max_requests, c["requests"].as_u64()),
        ];
        for (ty, max, cur) in dims {
            let Some(max) = max else { continue };
            let cur = cur.unwrap_or(0).min(max);
            out.push(json!({
                "limit_type": ty, "limit_window": format!("{:?}", l.window).to_lowercase(),
                "max_value": max, "current_value": cur, "remaining_value": max - cur,
                "model_filter": null, "reset_at": reset.map(crate::ts_iso), "source": "api_key_limit",
            }));
        }
    }
    out
}

async fn key_usage(State(app): State<Arc<App>>, h: HeaderMap) -> Response {
    let key = match prelude(&app, &h) {
        Ok(k) => k,
        Err(r) => return *r,
    };
    let total = key.view()["usage"]["total"].clone();
    let cached = app.store.usage_summary(0, "key");
    let cached = cached
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["group"] == key.cfg.id.as_str()))
        .and_then(|r| r["cached_tokens"].as_i64())
        .unwrap_or(0);
    let (primary, secondary, _, _) = pool(&app, &key);
    axum::Json(json!({
        "request_count": total["requests"], "total_tokens": total["tokens"],
        "cached_input_tokens": cached, "total_cost_usd": total["cost"],
        "limits": key_limits(&key), "upstream_limits": [],
        "account_pool_usage": {"primary": primary["used_percent"], "secondary": secondary["used_percent"]},
    }))
    .into_response()
}

async fn files_create(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    let started = now_ms();
    let key = match prelude(&app, &h) {
        Ok(k) => k,
        Err(r) => return *r,
    };
    let call = Call {
        provider: "codex",
        method: reqwest::Method::POST,
        path: "/files",
        accept: "application/json",
        pinned: None,
    };
    let sent = match forward(&app, &key, &call, &|rb| {
        rb.header(header::CONTENT_TYPE, "application/json").body(body.clone())
    })
    .await
    {
        Ok(s) => s,
        Err(e) => return failed(&app, &key, "codex", "files", "files-create", started, e),
    };
    let status = sent.res.status().as_u16();
    let bytes = sent.res.bytes().await.unwrap_or_default();
    if status < 300
        && let Some(id) = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["file_id"].as_str().map(str::to_string))
    {
        bind_file(&app, &id, &sent.acc.id);
    }
    record(
        &app,
        &key,
        &sent.acc.id,
        "files-create",
        "files",
        status,
        &Value::Null,
        started,
        sent.ttfb,
    );
    json_response(status, bytes)
}

async fn files_finalize(State(app): State<Arc<App>>, h: HeaderMap, Path(id): Path<String>) -> Response {
    let started = now_ms();
    let key = match prelude(&app, &h) {
        Ok(k) => k,
        Err(r) => return *r,
    };
    let owner = file_account(&app, &id);
    let path = format!("/files/{id}/uploaded");
    let call = Call {
        provider: "codex",
        method: reqwest::Method::POST,
        path: &path,
        accept: "application/json",
        pinned: owner.as_deref(),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let sent = match forward(&app, &key, &call, &|rb| {
            rb.header(header::CONTENT_TYPE, "application/json").body(Bytes::new())
        })
        .await
        {
            Ok(s) => s,
            Err(e) => return failed(&app, &key, "codex", "files", "files-finalize", started, e),
        };
        let status = sent.res.status().as_u16();
        let bytes = sent.res.bytes().await.unwrap_or_default();
        let retry = serde_json::from_slice::<Value>(&bytes).is_ok_and(|v| v["status"] == "retry");
        if !retry || tokio::time::Instant::now() >= deadline {
            record(
                &app,
                &key,
                &sent.acc.id,
                "files-finalize",
                "files",
                status,
                &Value::Null,
                started,
                sent.ttfb,
            );
            return json_response(status, bytes);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

struct Upload {
    name: String,
    file_name: String,
    content_type: Option<String>,
    data: Bytes,
}

#[derive(Default)]
struct Fields {
    text: HashMap<String, String>,
    files: Vec<Upload>,
}

async fn read_form(mut mp: Multipart) -> Result<Fields, String> {
    let mut out = Fields::default();
    while let Some(f) = mp.next_field().await.map_err(|e| e.to_string())? {
        let name = f.name().unwrap_or_default().to_string();
        let file_name = f.file_name().map(str::to_string);
        let content_type = f.content_type().map(str::to_string);
        let data = f.bytes().await.map_err(|e| e.to_string())?;
        match file_name {
            Some(file_name) => out.files.push(Upload {
                name,
                file_name,
                content_type,
                data,
            }),
            None => {
                out.text.insert(name, String::from_utf8_lossy(&data).into_owned());
            }
        }
    }
    Ok(out)
}

fn part(data: &Bytes, file_name: &str, content_type: &str) -> Part {
    let make = || Part::stream_with_length(reqwest::Body::from(data.clone()), data.len() as u64).file_name(file_name.to_string());
    make().mime_str(content_type).unwrap_or_else(|_| make())
}

fn bad_request(msg: &str) -> Response {
    proxy::error(Kind::Chat, 400, msg)
}

async fn transcribe(app: Arc<App>, h: HeaderMap, mp: Multipart, v1: bool) -> Response {
    let started = now_ms();
    let key = match prelude(&app, &h) {
        Ok(k) => k,
        Err(r) => return *r,
    };
    let form = match read_form(mp).await {
        Ok(f) => f,
        Err(e) => return bad_request(&format!("invalid multipart body: {e}")),
    };
    if v1 {
        match form.text.get("model").map(String::as_str) {
            None => return bad_request("model is required"),
            Some(m) if m != TRANSCRIBE_MODEL => {
                return bad_request(&format!("model {m} is not supported, use {TRANSCRIBE_MODEL}"));
            }
            Some(m) if !key.allows_model(m) => {
                return proxy::error(Kind::Chat, 403, &format!("model {m} not allowed for this key"));
            }
            _ => {}
        }
    }
    let Some(file) = form.files.iter().find(|f| f.name == "file") else {
        return bad_request("file is required");
    };
    let prompt = form.text.get("prompt");
    let content_type = file.content_type.clone().unwrap_or_else(|| "application/octet-stream".into());
    let call = Call {
        provider: "codex",
        method: reqwest::Method::POST,
        path: "/transcribe",
        accept: "application/json",
        pinned: None,
    };
    let shape = |rb: reqwest::RequestBuilder| {
        let mut f = Form::new().part("file", part(&file.data, &file.file_name, &content_type));
        if let Some(p) = prompt {
            f = f.text("prompt", p.clone());
        }
        rb.multipart(f)
    };
    let sent = match forward(&app, &key, &call, &shape).await {
        Ok(s) => s,
        Err(e) => return failed(&app, &key, "codex", "transcribe", TRANSCRIBE_MODEL, started, e),
    };
    record(
        &app,
        &key,
        &sent.acc.id,
        TRANSCRIBE_MODEL,
        "transcribe",
        sent.res.status().as_u16(),
        &Value::Null,
        started,
        sent.ttfb,
    );
    pass(sent)
}

async fn transcribe_backend(State(app): State<Arc<App>>, h: HeaderMap, mp: Multipart) -> Response {
    transcribe(app, h, mp, false).await
}

async fn transcribe_v1(State(app): State<Arc<App>>, h: HeaderMap, mp: Multipart) -> Response {
    transcribe(app, h, mp, true).await
}

const TOOL_FIELDS: [(&str, &str); 6] = [
    ("size", "auto"),
    ("quality", "auto"),
    ("background", "auto"),
    ("output_format", "png"),
    ("moderation", "auto"),
    ("input_fidelity", ""),
];

fn data_url(data: &[u8], content_type: Option<&str>) -> String {
    format!(
        "data:{};base64,{}",
        content_type.unwrap_or("image/png"),
        base64::engine::general_purpose::STANDARD.encode(data)
    )
}

async fn images(app: Arc<App>, key: Arc<Key>, opts: Value, images: Vec<String>, mask: Option<String>, started: i64) -> Response {
    let model = opts["model"].as_str().unwrap_or(DEFAULT_IMAGE_MODEL).to_string();
    if !model.starts_with("gpt-image-") {
        return bad_request(&format!("unsupported image model {model}, use a gpt-image-* model"));
    }
    if !key.allows_model(&model) {
        return proxy::error(Kind::Chat, 403, &format!("model {model} not allowed for this key"));
    }
    let Some(prompt) = opts["prompt"].as_str().filter(|p| !p.is_empty()) else {
        return bad_request("prompt is required");
    };
    if opts["n"].as_u64().unwrap_or(1) > 1 {
        return bad_request("n > 1 is not supported");
    }
    if opts["stream"].as_bool().unwrap_or(false) {
        return bad_request("streaming image responses are not supported");
    }
    let edit = !images.is_empty();
    let mut tool = json!({
        "type": "image_generation", "model": model,
        "output_compression": opts["output_compression"].as_u64().unwrap_or(100),
    });
    for (f, default) in TOOL_FIELDS {
        match opts[f].as_str().unwrap_or(default) {
            "" => {}
            v => tool[f] = Value::from(v),
        }
    }
    if edit {
        tool["action"] = "edit".into();
    }
    let mut text = prompt.to_string();
    if mask.is_some() {
        text.push_str(MASK_HINT);
    }
    let mut content = vec![json!({"type": "input_text", "text": text})];
    for url in images.iter().chain(mask.iter()) {
        content.push(json!({"type": "input_image", "image_url": url}));
    }
    let mut up = json!({
        "model": app.settings().warmup_model, "instructions": IMAGE_INSTRUCTIONS,
        "input": [{"type": "message", "role": "user", "content": content}],
        "tools": [tool], "tool_choice": {"type": "image_generation"},
    });
    codex::prepare_body(&mut up);
    let body = Bytes::from(serde_json::to_vec(&up).unwrap());
    let call = Call {
        provider: "codex",
        method: reqwest::Method::POST,
        path: "/codex/responses",
        accept: "text/event-stream",
        pinned: None,
    };
    let sent = match forward(&app, &key, &call, &|rb| {
        rb.header(header::CONTENT_TYPE, "application/json").body(body.clone())
    })
    .await
    {
        Ok(s) => s,
        Err(e) => return failed(&app, &key, "codex", "images", &model, started, e),
    };
    let Sent { acc, res, guard, ttfb } = sent;
    if res.status().as_u16() >= 400 {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        drop(guard);
        record(&app, &key, &acc.id, &model, "images", status, &Value::Null, started, ttfb);
        return json_response(status, Bytes::from(text));
    }
    let idle = Duration::from_secs(app.settings().idle_timeout_secs.max(30));
    let mut events = res.bytes_stream().eventsource();
    let mut done: Option<Value> = None;
    let mut items: Vec<Value> = vec![];
    while let Ok(Some(Ok(ev))) = tokio::time::timeout(idle, events.next()).await {
        let v: Value = serde_json::from_str(&ev.data).unwrap_or_default();
        match v["type"].as_str().unwrap_or_default() {
            "response.output_item.done" => items.push(v["item"].clone()),
            "response.completed" | "response.failed" | "response.incomplete" => {
                done = Some(v);
                break;
            }
            "error" => {
                done = Some(json!({"type": "response.failed", "response": {"error": v["error"].clone()}}));
                break;
            }
            _ => {}
        }
    }
    drop(guard);
    let usage = done.as_ref().map_or(Value::Null, |d| d["response"]["usage"].clone());
    let outcome = images_result(done.as_ref(), items);
    let status = match &outcome {
        Ok(_) => 200,
        Err(_) => 502,
    };
    record(&app, &key, &acc.id, &model, "images", status, &usage, started, ttfb);
    match outcome {
        Ok(v) => axum::Json(v).into_response(),
        Err(msg) => proxy::error(Kind::Chat, 502, &msg),
    }
}

fn images_result(done: Option<&Value>, items: Vec<Value>) -> Result<Value, String> {
    let Some(done) = done else {
        return Err("upstream stream ended without a response".into());
    };
    if done["type"] == "response.failed" {
        return Err(done["response"]["error"]["message"].as_str().unwrap_or("upstream failed").into());
    }
    let resp = &done["response"];
    let output = resp["output"].as_array().filter(|o| !o.is_empty()).cloned().unwrap_or(items);
    let calls: Vec<&Value> = output.iter().filter(|i| i["type"] == "image_generation_call").collect();
    if let Some(f) = calls.iter().find(|c| c["status"] == "failed") {
        return Err(f["error"]["message"].as_str().unwrap_or("image generation failed").into());
    }
    let data: Vec<Value> = calls
        .iter()
        .filter_map(|c| {
            let b64 = c["result"].as_str().filter(|r| !r.is_empty())?;
            let mut d = json!({"b64_json": b64});
            if let Some(p) = c["revised_prompt"].as_str().filter(|p| !p.is_empty()) {
                d["revised_prompt"] = p.into();
            }
            Some(d)
        })
        .collect();
    if data.is_empty() {
        return Err("upstream returned no image data".into());
    }
    let mut out = json!({"created": now_ms() / 1000, "data": data});
    if resp["tool_usage"]["image_gen"].is_object() {
        out["usage"] = resp["tool_usage"]["image_gen"].clone();
    }
    Ok(out)
}

async fn images_generations(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    let started = now_ms();
    let key = match prelude(&app, &h) {
        Ok(k) => k,
        Err(r) => return *r,
    };
    match serde_json::from_slice::<Value>(&body) {
        Ok(opts) => images(app, key, opts, vec![], None, started).await,
        Err(e) => bad_request(&format!("invalid json: {e}")),
    }
}

async fn images_edits_json(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    let started = now_ms();
    let key = match prelude(&app, &h) {
        Ok(k) => k,
        Err(r) => return *r,
    };
    let opts: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return bad_request(&format!("invalid json: {e}")),
    };
    let urls: Vec<String> = opts["images"]
        .as_array()
        .map(|a| a.iter().filter_map(|i| i["image_url"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if urls.is_empty() {
        return bad_request("at least one images[].image_url data URL is required");
    }
    let mask = opts["mask"]["image_url"].as_str().map(str::to_string);
    images(app, key, opts, urls, mask, started).await
}

async fn images_edits_multipart(State(app): State<Arc<App>>, h: HeaderMap, mp: Multipart) -> Response {
    let started = now_ms();
    let key = match prelude(&app, &h) {
        Ok(k) => k,
        Err(r) => return *r,
    };
    let form = match read_form(mp).await {
        Ok(f) => f,
        Err(e) => return bad_request(&format!("invalid multipart body: {e}")),
    };
    let urls: Vec<String> = form
        .files
        .iter()
        .filter(|f| f.name == "image" || f.name == "image[]")
        .map(|f| data_url(&f.data, f.content_type.as_deref()))
        .collect();
    if urls.is_empty() {
        return bad_request("at least one image is required");
    }
    if urls.len() > 16 {
        return bad_request("at most 16 images are supported");
    }
    let mask = form
        .files
        .iter()
        .find(|f| f.name == "mask")
        .map(|f| data_url(&f.data, f.content_type.as_deref()));
    let mut opts = serde_json::Map::new();
    for (k, v) in &form.text {
        let val = match k.as_str() {
            "n" | "output_compression" => v.parse::<u64>().map_or(Value::Null, Value::from),
            "stream" => Value::Bool(v == "true"),
            _ => Value::from(v.as_str()),
        };
        opts.insert(k.clone(), val);
    }
    images(app, key, Value::Object(opts), urls, mask, started).await
}

async fn embeddings(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Response {
    let started = now_ms();
    let key = match prelude(&app, &h) {
        Ok(k) => k,
        Err(r) => return *r,
    };
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return bad_request(&format!("invalid json: {e}")),
    };
    let model = req["model"].as_str().unwrap_or_default().to_string();
    if !model.contains('/') {
        return bad_request(&format!(
            "model {model:?} is not supported for embeddings: use an OpenRouter model id such as openai/text-embedding-3-small"
        ));
    }
    if !key.allows_model(&model) {
        return proxy::error(Kind::Chat, 403, &format!("model {model} not allowed for this key"));
    }
    if !app.accounts.read().iter().any(|a| a.cfg.read().kind == "openrouter") {
        return proxy::error(Kind::Chat, 503, "no openrouter accounts connected to serve embeddings");
    }
    let call = Call {
        provider: "openrouter",
        method: reqwest::Method::POST,
        path: "/embeddings",
        accept: "application/json",
        pinned: None,
    };
    let sent = match forward(&app, &key, &call, &|rb| {
        rb.header(header::CONTENT_TYPE, "application/json").body(body.clone())
    })
    .await
    {
        Ok(s) => s,
        Err(e) => return failed(&app, &key, "openrouter", "embeddings", &model, started, e),
    };
    let status = sent.res.status().as_u16();
    let bytes = sent.res.bytes().await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let usage = json!({"input_tokens": v["usage"]["prompt_tokens"], "cost": v["usage"]["cost"]});
    record(&app, &key, &sent.acc.id, &model, "embeddings", status, &usage, started, sent.ttfb);
    json_response(status, bytes)
}

#[cfg(test)]
mod tests {
    use crate::state::{AccountCfg, App, KeyCfg, Settings, Strategy};
    use crate::store::Store;
    use axum::Json;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use parking_lot::Mutex;
    use serde_json::{Value, json};
    use std::sync::Arc;

    async fn serve(router: axum::Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn setup(base: String, accounts: &[(&str, i64, bool)]) -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("nanolb-endpoints-{}", crate::random_hex(4)));
        let app = App::new(Store::open(&dir).unwrap(), "admin".into());
        app.save_settings(Settings {
            upstream_base: base,
            strategy: Strategy::Priority,
            ..Default::default()
        });
        for (id, priority, enabled) in accounts {
            upsert(&app, id, *priority, *enabled);
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
        app
    }

    fn upsert(app: &App, id: &str, priority: i64, enabled: bool) {
        app.upsert_account(AccountCfg {
            id: id.into(),
            kind: "codex".into(),
            enabled,
            priority,
            chatgpt_account_id: format!("acct-{id}"),
            expires_at: crate::now_ms() + 86_400_000,
            last_refresh: crate::now_ms(),
            ..Default::default()
        });
    }

    #[tokio::test]
    async fn file_use_routes_to_the_account_that_created_it() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        let mock = axum::Router::new()
            .route(
                "/files",
                post(|| async { Json(json!({"file_id": "file_affinity", "upload_url": "http://upload"})) }),
            )
            .route(
                "/files/{id}/uploaded",
                post(move |h: HeaderMap| {
                    let log = log.clone();
                    async move {
                        log.lock().push(h["chatgpt-account-id"].to_str().unwrap().to_string());
                        Json(json!({"status": "success"}))
                    }
                }),
            );
        let app = setup(serve(mock).await, &[("a", 1, true), ("b", 0, false)]);
        let url = serve(crate::router(app.clone())).await;
        let http = reqwest::Client::new();

        let created: Value = http
            .post(format!("{url}/backend-api/files"))
            .bearer_auth("nlb-test")
            .json(&json!({"file_name": "a.txt", "file_size": 1, "use_case": "codex"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(created["file_id"], "file_affinity");

        upsert(&app, "b", 0, true);
        let res = http
            .post(format!("{url}/backend-api/files/file_affinity/uploaded"))
            .bearer_auth("nlb-test")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(*seen.lock(), vec!["acct-a".to_string()]);
    }

    #[tokio::test]
    async fn forwarder_fails_over_on_429() {
        let mock = axum::Router::new().route(
            "/files",
            post(|h: HeaderMap| async move {
                if h["chatgpt-account-id"] == "acct-a" {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":600}}"#.to_string(),
                    );
                }
                (StatusCode::OK, r#"{"file_id":"file_failover"}"#.to_string())
            }),
        );
        let app = setup(serve(mock).await, &[("a", 0, true), ("b", 1, true)]);
        let url = serve(crate::router(app.clone())).await;
        let res = reqwest::Client::new()
            .post(format!("{url}/backend-api/files"))
            .bearer_auth("nlb-test")
            .json(&json!({"file_name": "a.txt", "file_size": 1, "use_case": "codex"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.json::<Value>().await.unwrap()["file_id"], "file_failover");
        assert_eq!(super::file_account(&app, "file_failover").as_deref(), Some("b"));
    }
}
