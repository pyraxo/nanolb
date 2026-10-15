use crate::state::App;
use axum::Router;
use std::sync::Arc;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
}
