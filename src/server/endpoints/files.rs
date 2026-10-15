use super::forward::{Call, JSON, forward, json_response, json_shape};
use super::{Req, prelude};
use crate::app::{App, Key, Provider};
use crate::server::error::ApiError;
use crate::store::Endpoint;
use crate::time::now_ms;
use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

pub(super) async fn create(State(app): State<Arc<App>>, Extension(key): Extension<Arc<Key>>, body: Bytes) -> Result<Response, ApiError> {
    let started = now_ms();
    prelude(&key)?;
    let req = Req {
        app: &app,
        key: &key,
        endpoint: Endpoint::Files,
        model: "files-create",
        started,
    };
    let call = Call::post(Provider::Codex, "/files", JSON);
    let sent = forward(&app, &key, &call, &json_shape(&body))
        .await
        .map_err(|e| req.failed(Provider::Codex, e))?;
    let status = sent.res.status().as_u16();
    let bytes = sent.res.bytes().await.unwrap_or_default();
    if status < 300
        && let Some(id) = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["file_id"].as_str().map(str::to_owned))
    {
        app.files.bind(&app.store, &id, &sent.acc.id);
    }
    req.record(&sent.acc.id, status, &Value::Null, sent.ttfb);
    Ok(json_response(status, bytes))
}

pub(super) async fn finalize(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let started = now_ms();
    prelude(&key)?;
    let req = Req {
        app: &app,
        key: &key,
        endpoint: Endpoint::Files,
        model: "files-finalize",
        started,
    };
    let owner = app.files.owner(&id);
    let path = format!("/files/{id}/uploaded");
    let call = Call {
        pinned: owner.as_deref(),
        ..Call::post(Provider::Codex, &path, JSON)
    };
    let body = Bytes::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let sent = forward(&app, &key, &call, &json_shape(&body))
            .await
            .map_err(|e| req.failed(Provider::Codex, e))?;
        let status = sent.res.status().as_u16();
        let bytes = sent.res.bytes().await.unwrap_or_default();
        let retry = serde_json::from_slice::<Value>(&bytes).is_ok_and(|v| v["status"] == "retry");
        if !retry || tokio::time::Instant::now() >= deadline {
            req.record(&sent.acc.id, status, &Value::Null, sent.ttfb);
            return Ok(json_response(status, bytes));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
