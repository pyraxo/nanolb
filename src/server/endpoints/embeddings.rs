use super::forward::{Call, JSON, forward, json_response, json_shape};
use super::{Req, bad_request, prelude, require_model};
use crate::app::{App, Provider};
use crate::server::error::{ApiError, ApiFormat};
use crate::store::Endpoint;
use crate::time::now_ms;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) async fn embeddings(State(app): State<Arc<App>>, h: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
    let started = now_ms();
    let key = prelude(&app, &h)?;
    let parsed: Value = serde_json::from_slice(&body).map_err(|e| bad_request(format!("invalid json: {e}")))?;
    let model = parsed["model"].as_str().unwrap_or_default();
    if !model.contains('/') {
        return Err(bad_request(format!(
            "model {model:?} is not supported for embeddings: use an OpenRouter model id such as openai/text-embedding-3-small"
        )));
    }
    require_model(&key, model)?;
    if !app.accounts.any(Provider::OpenRouter) {
        return Err(ApiError::new(
            ApiFormat::Chat,
            503,
            "no openrouter accounts connected to serve embeddings",
        ));
    }
    let req = Req {
        app: &app,
        key: &key,
        endpoint: Endpoint::Embeddings,
        model,
        started,
    };
    let call = Call::post(Provider::OpenRouter, "/embeddings", JSON);
    let sent = forward(&app, &key, &call, &json_shape(&body))
        .await
        .map_err(|e| req.failed(Provider::OpenRouter, e))?;
    let status = sent.res.status().as_u16();
    let bytes = sent.res.bytes().await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let usage = json!({"input_tokens": v["usage"]["prompt_tokens"], "cost": v["usage"]["cost"]});
    req.record(&sent.acc.id, status, &usage, sent.ttfb);
    Ok(json_response(status, bytes))
}
