use crate::state::App;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
}

pub async fn upgrade(State(_app): State<Arc<App>>) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}
