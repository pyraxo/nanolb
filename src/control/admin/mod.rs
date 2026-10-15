mod accounts;
mod args;
mod http;
mod keys;
mod system;

use crate::app::LookupError;
use crate::control::ops::{analytics, credits, plan, sessions};
use crate::store::StoreError;
use crate::time::TimeError;
use crate::upstream::UpstreamError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) use args::{account_arg, str_arg};
pub(crate) use http::http;

use crate::app::App;

#[derive(Debug, thiserror::Error)]
pub(crate) enum AdminError {
    #[error("unknown op `{0}`")]
    UnknownOp(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Lookup(#[from] LookupError),
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Time(#[from] TimeError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl AdminError {
    pub(crate) const fn status(&self) -> StatusCode {
        match self {
            Self::UnknownOp(_) | Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Lookup(e) => e.status(),
            Self::Store(e) if e.is_client() => StatusCode::BAD_REQUEST,
            Self::Invalid(_) | Self::Upstream(_) | Self::Time(_) => StatusCode::BAD_REQUEST,
            Self::Store(_) | Self::Json(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (self.status(), axum::Json(json!({"error": {"message": self.to_string()}}))).into_response()
    }
}

pub(crate) async fn dispatch(app: &Arc<App>, op: &str, a: Value) -> Result<Value, AdminError> {
    match op {
        "status" => accounts::status(app),
        "accounts_list" => Ok(accounts::accounts_list(app)),
        "account_import" => accounts::account_import(app, &a).await,
        "account_add_tokens" => accounts::account_add_tokens(app, &a).await,
        "account_add_openrouter" => accounts::account_add_openrouter(app, &a).await,
        "login_start" => accounts::login_start(app, &a).await,
        "login_poll" => accounts::login_poll(app, &a),
        "account_update" => accounts::account_update(app, &a),
        "account_remove" => accounts::account_remove(app, &a),
        "account_refresh" => accounts::account_refresh(app, &a).await,
        "account_warmup" => accounts::account_warmup(app, &a).await,
        "keys_list" => Ok(keys::keys_list(app)),
        "key_create" => keys::key_create(app, &a),
        "key_update" => keys::key_update(app, &a),
        "key_remove" => keys::key_remove(app, &a),
        "usage" => system::usage(app, &a),
        "requests" => system::requests(app, &a),
        "settings_get" => system::settings_get(app),
        "settings_update" => system::settings_update(app, a),
        "models" => Ok(system::models(app)),
        "sessions_list" => Ok(sessions::sessions_list(app, &a)),
        "session_delete" => sessions::session_delete(app, &a),
        "sessions_purge" => sessions::sessions_purge(app, &a),
        "reset_credits_list" => credits::reset_credits_list(app, &a).await,
        "reset_credits_consume" => credits::reset_credits_consume(app, &a).await,
        "plan" => plan::plan(app, &a),
        "analytics" => analytics::analytics(app, &a),
        _ => Err(AdminError::UnknownOp(op.to_owned())),
    }
}
