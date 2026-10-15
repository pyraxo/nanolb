use super::audio::read_form;
use super::forward::{Call, Sent, json_response, json_shape};
use super::{Req, bad_request, prelude, require_model};
use crate::app::{App, Key, Provider, Settings};
use crate::server::error::{ApiError, ApiFormat};
use crate::store::Endpoint;
use crate::time::now_ms;
use crate::upstream::codex;
use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Multipart, State};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_IMAGE_MODEL: &str = "gpt-image-2";
const IMAGE_INSTRUCTIONS: &str = "You are an image generator. When asked, you MUST call the image_generation tool exactly once and return only that tool call. Do not produce any additional text output. Mirror the user's request verbatim into the tool's prompt argument.";
const MASK_HINT: &str = "\n\n(The final attached image is a transparent mask: only modify the regions where the mask is non-transparent.)";

const TOOL_FIELDS: [(&str, &str); 6] = [
    ("size", "auto"),
    ("quality", "auto"),
    ("background", "auto"),
    ("output_format", "png"),
    ("moderation", "auto"),
    ("input_fidelity", ""),
];

struct Edit<'a> {
    images: &'a [String],
    mask: Option<&'a str>,
}

fn data_url(data: &[u8], content_type: Option<&str>) -> String {
    format!(
        "data:{};base64,{}",
        content_type.unwrap_or("image/png"),
        base64::engine::general_purpose::STANDARD.encode(data)
    )
}

fn image_model(opts: &Value, key: &Key) -> Result<String, ApiError> {
    let model = opts["model"].as_str().unwrap_or(DEFAULT_IMAGE_MODEL).to_owned();
    if !model.starts_with("gpt-image-") {
        return Err(bad_request(format!("unsupported image model {model}, use a gpt-image-* model")));
    }
    require_model(key, &model)?;
    Ok(model)
}

fn tool_definition(opts: &Value, model: &str, edit: bool) -> Value {
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
    tool
}

fn upstream_body(s: &Settings, opts: &Value, model: &str, edit: &Edit<'_>) -> Result<Bytes, ApiError> {
    let Some(prompt) = opts["prompt"].as_str().filter(|p| !p.is_empty()) else {
        return Err(bad_request("prompt is required"));
    };
    if opts["n"].as_u64().unwrap_or(1) > 1 {
        return Err(bad_request("n > 1 is not supported"));
    }
    if opts["stream"].as_bool().unwrap_or(false) {
        return Err(bad_request("streaming image responses are not supported"));
    }
    let tool = tool_definition(opts, model, !edit.images.is_empty());
    let mut text = prompt.to_owned();
    if edit.mask.is_some() {
        text.push_str(MASK_HINT);
    }
    let mut content = vec![json!({"type": "input_text", "text": text})];
    for url in edit.images.iter().map(String::as_str).chain(edit.mask) {
        content.push(json!({"type": "input_image", "image_url": url}));
    }
    let mut up = json!({
        "model": s.warmup_model, "instructions": IMAGE_INSTRUCTIONS,
        "input": [{"type": "message", "role": "user", "content": content}],
        "tools": [tool], "tool_choice": {"type": "image_generation"},
    });
    codex::prepare_body(&mut up);
    Ok(Bytes::from(up.to_string()))
}

async fn collect(res: reqwest::Response, idle: Duration) -> (Option<Value>, Vec<Value>) {
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
    (done, items)
}

async fn images(app: &Arc<App>, key: &Key, opts: &Value, edit: &Edit<'_>, started: i64) -> Result<Response, ApiError> {
    let model = image_model(opts, key)?;
    let req = Req::new(app, key, Endpoint::Images, &model, started);
    let body = upstream_body(&req.settings, opts, &model, edit)?;
    let call = Call::post(Provider::Codex, "/codex/responses", "text/event-stream");
    let sent = req.send(&call, &json_shape(&body)).await?;
    let Sent { acc, res, guard, ttfb } = sent;
    if res.status().as_u16() >= 400 {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        drop(guard);
        req.record(&acc.id, status, &Value::Null, ttfb);
        return Ok(json_response(status, Bytes::from(text)));
    }
    let idle = Duration::from_secs(req.settings.idle_timeout_secs.max(30));
    let (done, items) = collect(res, idle).await;
    drop(guard);
    let usage = done.as_ref().map_or(Value::Null, |d| d["response"]["usage"].clone());
    let outcome = images_result(done.as_ref(), items);
    req.record(&acc.id, if outcome.is_ok() { 200 } else { 502 }, &usage, ttfb);
    outcome.map(|v| axum::Json(v).into_response())
}

fn images_result(done: Option<&Value>, items: Vec<Value>) -> Result<Value, ApiError> {
    let fail = |msg: &str| ApiError::new(ApiFormat::Chat, 502, msg);
    let Some(done) = done else {
        return Err(fail("upstream stream ended without a response"));
    };
    if done["type"] == "response.failed" {
        return Err(fail(done["response"]["error"]["message"].as_str().unwrap_or("upstream failed")));
    }
    let resp = &done["response"];
    let output = resp["output"].as_array().filter(|o| !o.is_empty()).cloned().unwrap_or(items);
    let calls: Vec<&Value> = output.iter().filter(|i| i["type"] == "image_generation_call").collect();
    if let Some(f) = calls.iter().find(|c| c["status"] == "failed") {
        return Err(fail(f["error"]["message"].as_str().unwrap_or("image generation failed")));
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
        return Err(fail("upstream returned no image data"));
    }
    let mut out = json!({"created": now_ms() / 1000, "data": data});
    if resp["tool_usage"]["image_gen"].is_object() {
        out["usage"] = resp["tool_usage"]["image_gen"].clone();
    }
    Ok(out)
}

pub(super) async fn generations(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let started = now_ms();
    prelude(&key)?;
    let opts: Value = serde_json::from_slice(&body).map_err(|e| bad_request(format!("invalid json: {e}")))?;
    let edit = Edit { images: &[], mask: None };
    images(&app, &key, &opts, &edit, started).await
}

pub(super) async fn edits_json(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let started = now_ms();
    prelude(&key)?;
    let opts: Value = serde_json::from_slice(&body).map_err(|e| bad_request(format!("invalid json: {e}")))?;
    let urls: Vec<String> = opts["images"]
        .as_array()
        .map(|a| a.iter().filter_map(|i| i["image_url"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    if urls.is_empty() {
        return Err(bad_request("at least one images[].image_url data URL is required"));
    }
    let edit = Edit {
        images: &urls,
        mask: opts["mask"]["image_url"].as_str(),
    };
    images(&app, &key, &opts, &edit, started).await
}

pub(super) async fn edits_multipart(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    mp: Multipart,
) -> Result<Response, ApiError> {
    let started = now_ms();
    prelude(&key)?;
    let form = read_form(mp).await?;
    let urls: Vec<String> = form
        .files
        .iter()
        .filter(|f| f.name == "image" || f.name == "image[]")
        .map(|f| data_url(&f.data, f.content_type.as_deref()))
        .collect();
    if urls.is_empty() {
        return Err(bad_request("at least one image is required"));
    }
    if urls.len() > 16 {
        return Err(bad_request("at most 16 images are supported"));
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
    let edit = Edit {
        images: &urls,
        mask: mask.as_deref(),
    };
    images(&app, &key, &Value::Object(opts), &edit, started).await
}
