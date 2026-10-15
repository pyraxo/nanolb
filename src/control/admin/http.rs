use super::{AdminError, dispatch};
use crate::app::App;
use crate::server::auth::{admin_denied, is_admin};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

fn query_args(q: HashMap<String, String>) -> Value {
    Value::Object(
        q.into_iter()
            .map(|(k, v)| (k, serde_json::from_str(&v).unwrap_or(Value::String(v))))
            .collect(),
    )
}

pub(crate) async fn http(
    State(app): State<Arc<App>>,
    Path(op): Path<String>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if !is_admin(&app, &h) {
        return admin_denied();
    }
    let args = if body.is_empty() {
        query_args(q)
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return AdminError::Invalid(format!("invalid json: {e}")).into_response(),
        }
    };
    tracing::info!(%op, "admin op");
    match dispatch(&app, &op, args).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}
