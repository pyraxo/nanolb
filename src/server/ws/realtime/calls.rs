use super::{call_binding, internal};
use crate::app::{Account, App, Key, Provider, Settings};
use crate::server::error::ApiError;
use crate::server::ws::frames::PASS_HEADERS;
use crate::server::ws::{authed, exhausted};
use crate::store::{Endpoint, Usage, UsageMeta};
use crate::time::now_ms;
use crate::upstream::{self, UpstreamError, codex};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

pub(super) struct Calls {
    pub(super) app: Arc<App>,
    pub(super) settings: Arc<Settings>,
    pub(super) key: Arc<Key>,
    pub(super) h: HeaderMap,
    pub(super) target: String,
    pub(super) body: Bytes,
}

enum Step {
    Next(UpstreamError),
    Stop(ApiError),
}

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

fn rejected(acc: &Account, status: u16, bytes: &[u8]) -> Option<UpstreamError> {
    if status < 400 {
        return None;
    }
    let v: Value = serde_json::from_slice(bytes).unwrap_or_default();
    let until = upstream::retry_until(status, &v["error"])?;
    let message = v["error"]["message"].as_str().unwrap_or("upstream error").to_owned();
    if until > 0 {
        acc.fail(until, format!("{status}: {message}"));
    }
    Some(UpstreamError::Status { status, message })
}

impl Calls {
    pub(super) async fn run(self) -> Result<Response, ApiError> {
        let started = now_ms();
        authed(&self.key)?;
        let mut tried: Vec<String> = vec![];
        let mut last = UpstreamError::NoAccount;
        for _ in 0..self.settings.max_attempts.max(1) {
            let Some(acc) = self.app.pick(&self.settings, Provider::Codex, &self.key.cfg.accounts, &tried, None) else {
                break;
            };
            tried.push(acc.id.clone());
            let res = match self.send(&acc).await {
                Ok(r) => r,
                Err(Step::Next(e)) => {
                    last = e;
                    continue;
                }
                Err(Step::Stop(e)) => return Err(e),
            };
            codex::apply_rate_headers(&acc, res.headers());
            let status = res.status();
            let headers = res.headers().clone();
            let bytes = res.bytes().await.unwrap_or_default();
            if let Some(e) = rejected(&acc, status.as_u16(), &bytes) {
                last = e;
                continue;
            }
            self.record(&acc, status, &headers, started);
            return Ok(relayed(status, &headers, bytes));
        }
        Err(exhausted(&self.app, &self.key, last))
    }

    async fn send(&self, acc: &Arc<Account>) -> Result<reqwest::Response, Step> {
        codex::ensure_fresh(&self.app, acc, false).await.map_err(Step::Next)?;
        let h = &self.h;
        let mut rb = codex::request(
            &self.app,
            &self.settings,
            acc,
            reqwest::Method::POST,
            "/codex/realtime/calls",
            Some(h),
        )
        .header(
            header::ACCEPT,
            h.get(header::ACCEPT).cloned().unwrap_or(HeaderValue::from_static("*/*")),
        )
        .body(self.body.clone());
        if let Some(ct) = h.get(header::CONTENT_TYPE) {
            rb = rb.header(header::CONTENT_TYPE, ct);
        }
        let mut req = rb.build().map_err(|e| Step::Stop(internal(&e)))?;
        *req.url_mut() = reqwest::Url::parse(&self.target).map_err(|e| Step::Stop(internal(&e)))?;
        let timeout = Duration::from_secs(self.settings.first_event_timeout_secs.max(5));
        match tokio::time::timeout(timeout, self.app.http.execute(req)).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => {
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                Err(Step::Next(UpstreamError::Connect(e)))
            }
            Err(_) => {
                acc.fail(now_ms() + 15_000, "realtime call timed out".into());
                Err(Step::Next(UpstreamError::Timeout))
            }
        }
    }

    fn record(&self, acc: &Account, status: reqwest::StatusCode, headers: &HeaderMap, started: i64) {
        if status.is_success()
            && let Some(id) = headers
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(call_id_from_location)
        {
            self.app.sessions.bind(call_binding(&self.key, id), acc.id.clone(), now_ms());
        }
        let meta = UsageMeta {
            key_id: &self.key.cfg.id,
            account_id: &acc.id,
            model: "realtime",
            endpoint: Endpoint::RealtimeCall,
            started,
            ttfb: now_ms(),
        };
        self.app.record(
            Some(&self.key),
            Usage::new(meta, status.as_u16(), &Value::Null, &self.settings.pricing),
        );
    }
}
