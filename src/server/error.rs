use crate::app::Provider;
use crate::store::Endpoint;
use crate::time::{now_ms, ts_iso};
use crate::translate::anthropic;
use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApiFormat {
    Responses,
    Chat,
    OpenRouter,
    Anthropic,
}

impl ApiFormat {
    pub(crate) const fn endpoint(self) -> Endpoint {
        match self {
            Self::Responses => Endpoint::Responses,
            Self::Chat => Endpoint::Chat,
            Self::OpenRouter => Endpoint::OpenRouter,
            Self::Anthropic => Endpoint::Anthropic,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct ApiError {
    format: ApiFormat,
    status: StatusCode,
    message: String,
    retry_after: Option<i64>,
}

impl ApiError {
    pub(crate) fn new(format: ApiFormat, status: u16, message: impl Into<String>) -> Self {
        Self {
            format,
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            message: message.into(),
            retry_after: None,
        }
    }

    pub(crate) fn unauthorized(format: ApiFormat) -> Self {
        Self::new(format, 401, "invalid or missing api key")
    }

    pub(crate) fn limited(format: ApiFormat, provider: Provider, until: i64) -> Self {
        Self {
            retry_after: Some(((until - now_ms()) / 1000).max(1)),
            ..Self::new(
                format,
                429,
                format!("all {provider} accounts are rate limited until {}", ts_iso(until)),
            )
        }
    }

    pub(crate) const fn status(&self) -> StatusCode {
        self.status
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = self.status.as_u16();
        let body = match self.format {
            ApiFormat::Anthropic => anthropic::error_body(code, &self.message),
            ApiFormat::OpenRouter => json!({"error": {"code": code, "message": self.message}}),
            ApiFormat::Responses | ApiFormat::Chat => json!({"error": {
                "message": self.message,
                "type": if code == 429 { "rate_limit_error" } else { "api_error" },
                "code": Value::Null,
            }}),
        };
        let mut res = (self.status, Json(body)).into_response();
        if let Some(secs) = self.retry_after {
            res.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        res
    }
}
