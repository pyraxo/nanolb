use crate::app::{Account, App, Inflight, Key, Provider, Settings};
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

enum Step {
    Next(UpstreamError),
    Again,
    Stop(UpstreamError),
}

struct Forward<'a> {
    app: &'a Arc<App>,
    s: &'a Settings,
    c: &'a Call<'a>,
    shape: Shape<'a>,
    first_timeout: Duration,
}

pub(super) async fn forward(app: &Arc<App>, s: &Settings, key: &Key, c: &Call<'_>, shape: Shape<'_>) -> Result<Sent, UpstreamError> {
    let f = Forward {
        app,
        s,
        c,
        shape,
        first_timeout: Duration::from_secs(s.first_event_timeout_secs.max(5)),
    };
    let mut tried: Vec<String> = vec![];
    let mut refreshed: Vec<String> = vec![];
    let mut last = UpstreamError::NoAccount;
    for _ in 0..s.max_attempts.max(1) {
        let acc = match c.pinned {
            Some(id) if tried.iter().any(|t| t == id) => break,
            Some(id) => {
                let acc = app
                    .accounts
                    .get(id)
                    .filter(|a| key.cfg.accounts.is_empty() || key.cfg.accounts.contains(&a.id));
                if acc.is_none() {
                    last = UpstreamError::FileOwnerGone;
                }
                acc
            }
            None => app.pick(s, c.provider, &key.cfg.accounts, &tried, None),
        };
        let Some(acc) = acc else { break };
        match f.attempt(&acc, &mut refreshed).await {
            Ok(sent) => return Ok(sent),
            Err(Step::Again) => {}
            Err(Step::Next(e)) => {
                tried.push(acc.id.clone());
                last = e;
            }
            Err(Step::Stop(e)) => return Err(e),
        }
    }
    Err(last)
}

impl Forward<'_> {
    async fn attempt(&self, acc: &Arc<Account>, refreshed: &mut Vec<String>) -> Result<Sent, Step> {
        codex::ensure_fresh(self.app, acc, false).await.map_err(Step::Next)?;
        let guard = acc.acquire();
        let rb = upstream::request(self.app, self.s, acc, self.c.method.clone(), self.c.path, None);
        let mut req = (self.shape)(rb).build().map_err(|e| Step::Stop(UpstreamError::Build(e)))?;
        req.headers_mut().insert(header::ACCEPT, HeaderValue::from_static(self.c.accept));
        let res = match tokio::time::timeout(self.first_timeout, self.app.http.execute(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(cool(acc, format!("connect: {e}"), UpstreamError::Connect(e))),
            Err(_) => return Err(cool(acc, "timed out waiting for response headers".into(), UpstreamError::Timeout)),
        };
        if self.c.provider == Provider::Codex {
            codex::apply_rate_headers(acc, res.headers());
        }
        let status = res.status().as_u16();
        if status == 401 && !refreshed.contains(&acc.id) {
            refreshed.push(acc.id.clone());
            if codex::ensure_fresh(self.app, acc, true).await.is_ok() {
                return Err(Step::Again);
            }
        }
        if status < 600 && upstream::fails_over(status) {
            return Err(rejected(acc, status, res).await);
        }
        Ok(Sent {
            acc: Arc::clone(acc),
            res,
            guard,
            ttfb: now_ms(),
        })
    }
}

fn cool(acc: &Account, reason: String, e: UpstreamError) -> Step {
    acc.fail(now_ms() + 15_000, reason);
    Step::Next(e)
}

async fn rejected(acc: &Account, status: u16, res: reqwest::Response) -> Step {
    let (v, message) = upstream::error_body(&res.text().await.unwrap_or_default());
    if let Failure::Retry(until) = classify(status, &v["error"])
        && until > 0
    {
        acc.fail(until, format!("{status}: {message}"));
    }
    tracing::warn!(account = %acc.id, status, error = %message, "upstream error, failing over");
    Step::Next(UpstreamError::Status { status, message })
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
