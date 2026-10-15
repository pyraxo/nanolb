use crate::app::{App, Key, Provider};
use crate::server::error::{ApiError, ApiFormat};
use crate::time::ts_iso;
use crate::translate::anthropic;
use crate::upstream::{UpstreamError, codex};
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) async fn count_tokens(body: Bytes) -> Result<Json<Value>, ApiError> {
    let req: Value = serde_json::from_slice(&body).map_err(|e| ApiError::new(ApiFormat::Anthropic, 400, e.to_string()))?;
    Ok(Json(anthropic::count_tokens(&req)))
}

pub(crate) async fn models(State(app): State<Arc<App>>, h: HeaderMap) -> Json<Value> {
    let anth = h.contains_key("anthropic-version");
    let list = app.model_list();
    if anth {
        let created_at = ts_iso(app.started_at);
        let data: Vec<Value> = list
            .into_iter()
            .map(|(m, _)| json!({"type": "model", "id": m, "display_name": m, "created_at": created_at}))
            .collect();
        let (first_id, last_id) = (data.first().map(|d| d["id"].clone()), data.last().map(|d| d["id"].clone()));
        return Json(json!({"first_id": first_id, "last_id": last_id, "has_more": false, "data": data}));
    }
    let created = app.started_at / 1000;
    let data: Vec<Value> = list
        .into_iter()
        .map(|(m, ctx)| json!({"id": m, "object": "model", "created": created, "owned_by": "openai", "context_length": ctx}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}

pub(crate) async fn openrouter_models(State(app): State<Arc<App>>) -> Json<Value> {
    let settings = app.settings();
    let per_tok = |m: &str, i: usize| {
        settings
            .pricing
            .rates(m)
            .map_or_else(|| "0".to_owned(), |r| format!("{}", <[f64; 3]>::from(r)[i] / 1e6))
    };
    let created = app.started_at / 1000;
    let data: Vec<Value> = app
        .model_list()
        .into_iter()
        .map(|(m, ctx)| {
            let m = m.as_str();
            let id = if m.contains('/') { m.to_owned() } else { format!("openai/{m}") };
            json!({
                "id": id, "canonical_slug": id, "name": id,
                "created": created, "description": "", "context_length": ctx,
                "architecture": {"modality": "text+image->text", "input_modalities": ["text", "image"], "output_modalities": ["text"], "tokenizer": "GPT", "instruct_type": null},
                "pricing": {"prompt": per_tok(m, 0), "completion": per_tok(m, 2), "input_cache_read": per_tok(m, 1), "request": "0", "image": "0", "web_search": "0", "internal_reasoning": "0"},
                "top_provider": {"context_length": ctx, "max_completion_tokens": 128_000, "is_moderated": false},
                "per_request_limits": null,
                "supported_parameters": ["tools", "tool_choice", "reasoning", "include_reasoning", "response_format", "structured_outputs", "max_tokens"],
            })
        })
        .collect();
    Json(json!({"data": data}))
}

pub(crate) async fn openrouter_key(Extension(key): Extension<Arc<Key>>) -> Result<Json<Value>, ApiError> {
    let v = key.view();
    let u = &v["usage"];
    let limit = key.cfg.limits.iter().find_map(|l| l.max_cost_usd);
    let total = u["total"]["cost"].as_f64().unwrap_or(0.0);
    Ok(Json(json!({"data": {
        "label": key.cfg.prefix, "limit": limit, "limit_remaining": limit.map(|l| (l - total).max(0.0)), "limit_reset": null,
        "usage": total, "usage_daily": u["day"]["cost"], "usage_weekly": u["week"]["cost"], "usage_monthly": u["month"]["cost"],
        "byok_usage": 0, "is_free_tier": false, "is_management_key": false, "is_provisioning_key": false,
        "expires_at": v["expires_at"], "rate_limit": {"requests": -1, "interval": "10s"},
    }})))
}

pub(crate) async fn codex_models(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    h: HeaderMap,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiError> {
    let format = ApiFormat::Responses;
    let s = app.settings();
    let acc = app
        .pick(&s, Provider::Codex, &key.cfg.accounts, &[], None)
        .ok_or_else(|| ApiError::from((format, UpstreamError::NoAccount)))?;
    codex::ensure_fresh(&app, &acc, false)
        .await
        .map_err(|e| ApiError::new(format, 502, e.to_string()))?;
    let query = q.unwrap_or_else(|| format!("client_version={}", s.codex_version));
    let res = codex::request(&app, &s, &acc, reqwest::Method::GET, &format!("/codex/models?{query}"), Some(&h))
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| ApiError::new(format, 502, e.to_string()))?;
    let status = res.status();
    let content_type = res.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = res.bytes().await.unwrap_or_default();
    let mut out = (status, bytes).into_response();
    if let Some(ct) = content_type {
        out.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    Ok(out)
}
