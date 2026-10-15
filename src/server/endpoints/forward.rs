use crate::app::{Account, App, Inflight, Key, Provider};
use crate::time::now_ms;
use crate::upstream::{self, Failure, UpstreamError, classify, codex};
use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;

pub(super) const JSON: &str = "application/json";

pub(super) struct Call<'a> {
    pub(super) provider: Provider,
    pub(super) method: reqwest::Method,
    pub(super) path: &'a str,
    pub(super) accept: &'static str,
    pub(super) pinned: Option<&'a str>,
}

impl<'a> Call<'a> {
    pub(super) fn post(provider: Provider, path: &'a str, accept: &'static str) -> Self {
        Self {
            provider,
            method: reqwest::Method::POST,
            path,
            accept,
            pinned: None,
        }
    }
}

pub(super) struct Sent {
    pub(super) acc: Arc<Account>,
    pub(super) res: reqwest::Response,
    pub(super) guard: Inflight,
    pub(super) ttfb: i64,
}

pub(super) type Shape<'a> = &'a (dyn Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Sync);

pub(super) fn json_shape(body: &Bytes) -> impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Sync + '_ {
    move |rb| rb.header(header::CONTENT_TYPE, JSON).body(body.clone())
}

pub(super) async fn forward(app: &Arc<App>, key: &Key, c: &Call<'_>, shape: Shape<'_>) -> Result<Sent, UpstreamError> {
    let first_timeout = Duration::from_secs(app.settings().first_event_timeout_secs.max(5));
    let mut tried: Vec<String> = vec![];
    let mut refreshed: Vec<String> = vec![];
    let mut last = UpstreamError::NoAccount;
    for _ in 0..app.settings().max_attempts.max(1) {
        let acc = match c.pinned {
            Some(id) => {
                if tried.iter().any(|t| t == id) {
                    break;
                }
                let acc = app
                    .accounts
                    .get(id)
                    .filter(|a| key.cfg.accounts.is_empty() || key.cfg.accounts.contains(&a.id));
                if acc.is_none() {
                    last = UpstreamError::FileOwnerGone;
                }
                acc
            }
            None => app.pick(c.provider, &key.cfg.accounts, &tried, None),
        };
        let Some(acc) = acc else { break };
        if let Err(e) = codex::ensure_fresh(app, &acc, false).await {
            tried.push(acc.id.clone());
            last = e;
            continue;
        }
        let guard = acc.acquire();
        let rb = upstream::request(app, &acc, c.method.clone(), c.path, None);
        let mut req = shape(rb).build().map_err(UpstreamError::Build)?;
        req.headers_mut().insert(header::ACCEPT, HeaderValue::from_static(c.accept));
        let res = match tokio::time::timeout(first_timeout, app.http.execute(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                tried.push(acc.id.clone());
                last = UpstreamError::Connect(e);
                continue;
            }
            Err(_) => {
                acc.fail(now_ms() + 15_000, "timed out waiting for response headers".into());
                tried.push(acc.id.clone());
                last = UpstreamError::Timeout;
                continue;
            }
        };
        if c.provider == Provider::Codex {
            codex::apply_rate_headers(&acc, res.headers());
        }
        let status = res.status().as_u16();
        if status == 401 && !refreshed.contains(&acc.id) {
            refreshed.push(acc.id.clone());
            if codex::ensure_fresh(app, &acc, true).await.is_ok() {
                continue;
            }
        }
        if status < 600 && upstream::fails_over(status) {
            let (v, msg) = upstream::error_body(&res.text().await.unwrap_or_default());
            if let Failure::Retry(until) = classify(status, &v["error"])
                && until > 0
            {
                acc.fail(until, format!("{status}: {msg}"));
            }
            tracing::warn!(account = %acc.id, status, error = %msg, "upstream error, failing over");
            tried.push(acc.id.clone());
            last = UpstreamError::Status { status, message: msg };
            continue;
        }
        return Ok(Sent {
            acc,
            res,
            guard,
            ttfb: now_ms(),
        });
    }
    Err(last)
}

pub(super) fn pass(sent: Sent) -> Response {
    let Sent { res, guard, .. } = sent;
    let status = res.status();
    let ct = res.headers().get(header::CONTENT_TYPE).cloned();
    let body = Body::from_stream(res.bytes_stream().map(move |c| {
        let _ = &guard;
        c
    }));
    let mut out = Response::new(body);
    *out.status_mut() = status;
    if let Some(ct) = ct {
        out.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    out
}

pub(super) fn json_response(status: u16, body: Bytes) -> Response {
    let mut r = (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON));
    r
}
