mod catalog;
mod failover;
mod relay;
mod session;

pub(crate) use catalog::{codex_models, count_tokens, models, openrouter_key, openrouter_models};

use crate::app::{App, Key, Provider, Sessions, canonical_model};
use crate::server::error::{ApiError, ApiFormat};
use crate::time::now_ms;
use crate::translate::{anthropic, chat};
use crate::upstream::codex;
use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use relay::{Job, Reply};
use serde_json::{Value, json};
use session::{file_owner, session_key};
use std::sync::Arc;

pub(crate) async fn responses(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    h: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    handle(app, key, ApiFormat::Responses, h, body).await
}

pub(crate) async fn chat_completions(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    h: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    handle(app, key, ApiFormat::Chat, h, body).await
}

pub(crate) async fn openrouter(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    h: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    handle(app, key, ApiFormat::OpenRouter, h, body).await
}

pub(crate) async fn messages(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    h: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    handle(app, key, ApiFormat::Anthropic, h, body).await
}

fn translate(req: Value, format: ApiFormat, requested: &str) -> Result<(Value, Reply), ApiError> {
    let invalid = |e| ApiError::from((format, e));
    Ok(match format {
        ApiFormat::Responses => (req, Reply::Responses),
        ApiFormat::Chat | ApiFormat::OpenRouter => {
            let (up, opts) = chat::to_responses(&req, format == ApiFormat::OpenRouter).map_err(invalid)?;
            (up, Reply::Chat(opts))
        }
        ApiFormat::Anthropic => {
            let (up, opts) = anthropic::to_responses(&req, canonical_model(requested)).map_err(invalid)?;
            (up, Reply::Anthropic(opts))
        }
    })
}

fn prepare(app: &App, key: &Key, provider: Provider, h: &HeaderMap, user_id: Option<&str>, up: &mut Value) -> String {
    if provider == Provider::Codex {
        codex::prepare_body(up);
    } else {
        up["stream"] = Value::Bool(true);
        up["usage"] = json!({"include": true});
    }
    let cache = session_key(h, user_id, up);
    let session = Sessions::key(&key.cfg.id, provider, &cache);
    if provider == Provider::Codex && up.get("prompt_cache_key").is_none() {
        up["prompt_cache_key"] = Value::String(cache);
    }
    if provider == Provider::Codex
        && let Some(acc) = file_owner(app, up)
    {
        app.sessions.bind(session.clone(), acc, now_ms());
    }
    session
}

async fn handle(app: Arc<App>, key: Arc<Key>, format: ApiFormat, h: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
    let started = now_ms();
    key.check(started).map_err(|e| ApiError::from((format, e)))?;
    let req: Value = serde_json::from_slice(&body).map_err(|e| ApiError::new(format, 400, format!("invalid json: {e}")))?;
    let requested = req["model"].as_str().unwrap_or_default().to_owned();
    let stream = req["stream"].as_bool().unwrap_or(false);
    let user_id = req["metadata"]["user_id"].as_str().map(str::to_owned);
    let (mut up, mut reply) = translate(req, format, &requested)?;
    if !up.is_object() {
        return Err(ApiError::new(format, 400, "invalid json: expected an object"));
    }
    let model = canonical_model(up["model"].as_str().unwrap_or(&requested)).to_owned();
    if !key.allows_model(&requested) && !key.allows_model(&model) {
        return Err(ApiError::new(format, 403, format!("model {requested} not allowed for this key")));
    }
    let settings = app.settings();
    if let Reply::Chat(opts) = &mut reply {
        opts.rates = settings.pricing.rates(&model);
    }
    up["model"] = Value::String(model.clone());
    let provider = Provider::for_model(&model);
    if !app.accounts.any(provider) {
        return Err(ApiError::new(
            format,
            503,
            format!("no {provider} accounts connected to serve model {model}"),
        ));
    }
    let session = prepare(&app, &key, provider, &h, user_id.as_deref(), &mut up);
    let job = Job {
        app,
        settings,
        key,
        format,
        model,
        started,
        stream,
    };
    let client_headers = (format == ApiFormat::Responses).then_some(&h);
    let upstream = failover::send(&job, provider, client_headers, &up, &session)
        .await
        .map_err(|e| job.failed(provider, e))?;
    relay::run(job, upstream, reply).await
}
