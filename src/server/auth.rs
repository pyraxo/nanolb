use crate::state::{App, Key};
use crate::id::sha256_hex;
use crate::server::error::{ApiError, ApiFormat};
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

pub(crate) fn api_key(app: &App, h: &HeaderMap) -> Option<Arc<Key>> {
    let secret = bearer(h).or_else(|| h.get("x-api-key").and_then(|v| v.to_str().ok()))?;
    app.key_by_secret(secret.trim())
}

pub(crate) fn is_admin(app: &App, h: &HeaderMap) -> bool {
    bearer(h).is_some_and(|t| !app.admin_token.is_empty() && sha256_hex(t) == sha256_hex(&app.admin_token))
}

pub(crate) fn admin_denied() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "invalid admin token"}}))).into_response()
}

pub(crate) async fn require_key(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if api_key(&app, req.headers()).is_none() {
        let p = req.uri().path();
        let format = if p.starts_with("/v1/messages") || req.headers().contains_key("anthropic-version") {
            ApiFormat::Anthropic
        } else if p.starts_with("/api/") {
            ApiFormat::OpenRouter
        } else {
            ApiFormat::Chat
        };
        return ApiError::unauthorized(format).into_response();
    }
    next.run(req).await
}

pub(crate) async fn admin_guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if !is_admin(&app, req.headers()) {
        return admin_denied();
    }
    next.run(req).await
}
