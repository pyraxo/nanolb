use super::{call_binding, internal, with_query};
use crate::app::{App, Provider};
use crate::server::error::ApiError;
use crate::server::ws::frames::{PASS_HEADERS, retry_until};
use crate::server::ws::{authed, exhausted};
use crate::store::{Endpoint, Usage, UsageMeta};
use crate::time::now_ms;
use crate::upstream::{UpstreamError, codex};
use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

fn call_id_from_location(loc: &str) -> Option<&str> {
    let path = loc.split(['?', '#']).next()?;
    path.rsplit('/')
        .find(|s| !s.is_empty() && s.starts_with("rtc_"))
        .or_else(|| path.rsplit('/').find(|s| !s.is_empty()))
}
fn relayed(status: reqwest::StatusCode, headers: &HeaderMap, bytes: Bytes) -> Response {
    let mut out = Response::new(Body::from(bytes));
    *out.status_mut() = status;
    for (k, v) in headers {
        if [header::CONTENT_TYPE, header::LOCATION].contains(k) || PASS_HEADERS.contains(&k.as_str()) {
            out.headers_mut().append(k.clone(), v.clone());
        }
    }
    out
}

async fn calls(app: Arc<App>, h: HeaderMap, target: String, body: Bytes) -> Result<Response, ApiError> {
    let started = now_ms();
    let key = authed(&app, &h)?;
    let s = app.settings();
    let timeout = Duration::from_secs(s.first_event_timeout_secs.max(5));
    let mut tried: Vec<String> = vec![];
    let mut last = UpstreamError::NoAccount;
    for _ in 0..s.max_attempts.max(1) {
        let Some(acc) = app.pick(Provider::Codex, &key.cfg.accounts, &tried, None) else {
            break;
        };
        tried.push(acc.id.clone());
        if let Err(e) = codex::ensure_fresh(&app, &acc, false).await {
            last = e;
            continue;
        }
        let mut rb = codex::request(&app, &acc, reqwest::Method::POST, "/codex/realtime/calls", Some(&h))
            .header(
                header::ACCEPT,
                h.get(header::ACCEPT).cloned().unwrap_or(HeaderValue::from_static("*/*")),
            )
            .body(body.clone());
        if let Some(ct) = h.get(header::CONTENT_TYPE) {
            rb = rb.header(header::CONTENT_TYPE, ct);
        }
        let mut req = rb.build().map_err(|e| internal(&e))?;
        *req.url_mut() = reqwest::Url::parse(&target).map_err(|e| internal(&e))?;
        let res = match tokio::time::timeout(timeout, app.http.execute(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                last = UpstreamError::Connect(e);
                continue;
            }
            Err(_) => {
                acc.fail(now_ms() + 15_000, "realtime call timed out".into());
                last = UpstreamError::Timeout;
                continue;
            }
        };
        codex::apply_rate_headers(&acc, res.headers());
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res.bytes().await.unwrap_or_default();
        if status.as_u16() >= 400 {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            if let Some(until) = retry_until(status.as_u16(), &v["error"]) {
                let message = v["error"]["message"].as_str().unwrap_or("upstream error").to_owned();
                if until > 0 {
                    acc.fail(until, format!("{}: {message}", status.as_u16()));
                }
                last = UpstreamError::Status {
                    status: status.as_u16(),
                    message,
                };
                continue;
            }
        }
        if status.is_success()
            && let Some(id) = headers
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(call_id_from_location)
        {
            app.sessions.bind(call_binding(&key, id), acc.id.clone(), now_ms());
        }
        let meta = UsageMeta {
            key_id: &key.cfg.id,
            account_id: &acc.id,
            model: "realtime",
            endpoint: Endpoint::RealtimeCall,
            started,
            ttfb: now_ms(),
        };
        app.record(Some(&key), Usage::new(meta, status.as_u16(), &Value::Null, &s.pricing));
        return Ok(relayed(status, &headers, bytes));
    }
    Err(exhausted(&app, &key, last))
}
pub(in crate::server::ws) async fn backend_calls(
    State(app): State<Arc<App>>,
    h: HeaderMap,
    RawQuery(q): RawQuery,
    body: Bytes,
) -> Result<Response, ApiError> {
    let target = with_query(format!("{}/codex/realtime/calls", app.settings().upstream_base), q.as_deref());
    calls(app, h, target, body).await
}

pub(in crate::server::ws) async fn v1_calls(
    State(app): State<Arc<App>>,
    h: HeaderMap,
    RawQuery(q): RawQuery,
    body: Bytes,
) -> Result<Response, ApiError> {
    let target = with_query(format!("{}/realtime/calls", app.settings().realtime_base), q.as_deref());
    calls(app, h, target, body).await
}
