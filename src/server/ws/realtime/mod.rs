mod calls;
mod live;

use super::frames::ws_url;
use crate::app::{Account, App, Key};
use crate::server::error::{ApiError, ApiFormat};
use axum::Extension;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use calls::Calls;
use live::Live;
use std::fmt::Display;
use std::sync::Arc;

fn internal(e: &impl Display) -> ApiError {
    ApiError::new(ApiFormat::Responses, 500, e.to_string())
}

fn call_binding(key: &Key, call_id: &str) -> String {
    format!("rt\u{1f}{}\u{1f}{call_id}", key.cfg.id)
}
fn with_query(url: String, q: Option<&str>) -> String {
    match q.filter(|q| !q.is_empty()) {
        Some(q) => format!("{url}?{q}"),
        None => url,
    }
}
fn owner(app: &App, key: &Key, call_id: Option<&str>) -> Result<Option<Arc<Account>>, ApiError> {
    call_id
        .map(|c| {
            app.sessions
                .pinned(&call_binding(key, c))
                .and_then(|id| app.accounts.get(&id))
                .ok_or_else(|| ApiError::new(ApiFormat::Responses, 404, "realtime call binding not found or expired"))
        })
        .transpose()
}

pub(super) async fn v1_realtime(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    h: HeaderMap,
    Query(q): Query<Vec<(String, String)>>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let (call_id, model) = (get("call_id"), get("model").unwrap_or_else(|| "realtime".into()));
    let settings = app.settings();
    let mut url = reqwest::Url::parse(&format!("{}/realtime", settings.realtime_base)).map_err(|e| internal(&e))?;
    url.query_pairs_mut().extend_pairs(q.iter());
    let url = ws_url(url.as_str());
    Live {
        app,
        settings,
        key,
        h,
        url,
    }
    .run(ws, call_id, model)
    .await
}

pub(super) async fn v1_live(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    h: HeaderMap,
    Path(call_id): Path<String>,
    RawQuery(q): RawQuery,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if matches!(call_id.as_str(), "." | "..") || !call_id.chars().all(|c| c.is_ascii_alphanumeric() || "._~-".contains(c)) {
        return Err(ApiError::new(ApiFormat::Responses, 404, "invalid realtime call id"));
    }
    let settings = app.settings();
    let url = ws_url(&with_query(format!("{}/live/{call_id}", settings.realtime_base), q.as_deref()));
    Live {
        app,
        settings,
        key,
        h,
        url,
    }
    .run(ws, Some(call_id), "realtime".into())
    .await
}

pub(super) async fn backend_calls(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    h: HeaderMap,
    RawQuery(q): RawQuery,
    body: Bytes,
) -> Result<Response, ApiError> {
    let settings = app.settings();
    let target = with_query(format!("{}/codex/realtime/calls", settings.upstream_base), q.as_deref());
    Calls {
        app,
        settings,
        key,
        h,
        target,
        body,
    }
    .run()
    .await
}

pub(super) async fn v1_calls(
    State(app): State<Arc<App>>,
    Extension(key): Extension<Arc<Key>>,
    h: HeaderMap,
    RawQuery(q): RawQuery,
    body: Bytes,
) -> Result<Response, ApiError> {
    let settings = app.settings();
    let target = with_query(format!("{}/realtime/calls", settings.realtime_base), q.as_deref());
    Calls {
        app,
        settings,
        key,
        h,
        target,
        body,
    }
    .run()
    .await
}
