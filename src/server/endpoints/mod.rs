mod audio;
mod embeddings;
mod files;
mod forward;
mod images;
mod usage;

use crate::app::{App, Key, Provider, Settings};
use crate::server::error::{ApiError, ApiFormat};
use crate::store::{Endpoint, Usage, UsageMeta};
use crate::time::now_ms;
use crate::upstream::UpstreamError;
use axum::Router;
use axum::routing::{get, post};
use forward::{Call, Sent, Shape, forward};
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/backend-api/wham/usage", get(usage::pool_usage))
        .route("/api/codex/usage", get(usage::pool_usage))
        .route("/v1/usage", get(usage::key_usage))
        .route("/backend-api/files", post(files::create))
        .route("/backend-api/files/{id}/uploaded", post(files::finalize))
        .route("/backend-api/transcribe", post(audio::transcribe_backend))
        .route("/v1/audio/transcriptions", post(audio::transcribe_v1))
        .route("/v1/images/generations", post(images::generations))
        .route("/backend-api/codex/images/generations", post(images::generations))
        .route("/v1/images/edits", post(images::edits_multipart))
        .route("/backend-api/codex/images/edits", post(images::edits_json))
        .route("/v1/embeddings", post(embeddings::embeddings))
}

fn prelude(key: &Key) -> Result<(), ApiError> {
    key.check(now_ms()).map_err(|e| ApiError::from((ApiFormat::Chat, e)))
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError::new(ApiFormat::Chat, 400, msg)
}

fn require_model(key: &Key, model: &str) -> Result<(), ApiError> {
    if key.allows_model(model) {
        Ok(())
    } else {
        Err(ApiError::new(
            ApiFormat::Chat,
            403,
            format!("model {model} not allowed for this key"),
        ))
    }
}

struct Req<'a> {
    app: &'a Arc<App>,
    settings: Arc<Settings>,
    key: &'a Key,
    endpoint: Endpoint,
    model: &'a str,
    started: i64,
}

impl<'a> Req<'a> {
    fn new(app: &'a Arc<App>, key: &'a Key, endpoint: Endpoint, model: &'a str, started: i64) -> Self {
        Self {
            app,
            settings: app.settings(),
            key,
            endpoint,
            model,
            started,
        }
    }

    async fn send(&self, call: &Call<'_>, shape: Shape<'_>) -> Result<Sent, ApiError> {
        forward(self.app, &self.settings, self.key, call, shape)
            .await
            .map_err(|e| self.failed(call.provider, e))
    }

    fn record(&self, account: &str, status: u16, usage: &Value, ttfb: i64) {
        let meta = UsageMeta {
            key_id: &self.key.cfg.id,
            account_id: account,
            model: self.model,
            endpoint: self.endpoint,
            started: self.started,
            ttfb,
        };
        self.app
            .record(Some(self.key), Usage::new(meta, status, usage, &self.settings.pricing));
    }

    fn failed(&self, provider: Provider, e: UpstreamError) -> ApiError {
        let status = e.status();
        tracing::warn!(endpoint = self.endpoint.as_str(), status, error = %e, "request failed");
        self.record("", status, &Value::Null, now_ms());
        if status == 503
            && let Some(until) = self.app.accounts.next_available(provider, &self.key.cfg.accounts)
        {
            return ApiError::limited(ApiFormat::Chat, provider, until);
        }
        ApiError::from((ApiFormat::Chat, e))
    }
}
