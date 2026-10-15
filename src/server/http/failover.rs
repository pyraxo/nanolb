use super::relay::Job;
use crate::app::{Account, Inflight, Provider};
use crate::time::now_ms;
use crate::upstream::{self, Failure, UpstreamError, classify, codex};
use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, header};
use eventsource_stream::{Event, EventStreamError, Eventsource};
use futures_util::{Stream, StreamExt};
use reqwest::{Method, RequestBuilder, Response};
use serde_json::Value;
use std::cell::OnceCell;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

pub(super) type Events = Pin<Box<dyn Stream<Item = Result<Event, EventStreamError<reqwest::Error>>> + Send>>;

const TURN_STATE: &str = "x-codex-turn-state";
const PASS_HEADERS: [&str; 4] = ["x-request-id", "openai-model", "x-reasoning-included", "x-models-etag"];

pub(super) struct Upstream {
    pub(super) acc: Arc<Account>,
    pub(super) guard: Inflight,
    pub(super) first: Event,
    pub(super) events: Events,
    pub(super) ttfb: i64,
    pub(super) headers: HeaderMap,
}

enum Step {
    Next(UpstreamError),
    Again,
    Stop(UpstreamError),
}

struct Attempt<'a> {
    job: &'a Job,
    provider: Provider,
    body: Bytes,
    compressed: bool,
    cache_key: &'a str,
    first_timeout: Duration,
}

pub(super) async fn send(
    job: &Job,
    provider: Provider,
    h: Option<&HeaderMap>,
    up: &Value,
    session: &str,
) -> Result<Upstream, UpstreamError> {
    let raw = serde_json::to_vec(up).map_err(|e| UpstreamError::Decode(e.to_string()))?;
    let compress = provider == Provider::Codex && job.settings.compress_upstream;
    let (body, compressed) = match compress.then(|| zstd::encode_all(raw.as_slice(), 3)) {
        Some(Ok(c)) => (Bytes::from(c), true),
        _ => (Bytes::from(raw), false),
    };
    let attempt = Attempt {
        job,
        provider,
        body,
        compressed,
        cache_key: up["prompt_cache_key"].as_str().unwrap_or_default(),
        first_timeout: Duration::from_secs(job.settings.first_event_timeout_secs.max(5)),
    };
    let mut tried: Vec<String> = Vec::new();
    let mut refreshed: Vec<String> = Vec::new();
    let mut last = UpstreamError::NoAccount;
    let mut first_acc: Option<String> = None;
    let stripped = OnceCell::new();
    for _ in 0..job.settings.max_attempts.max(1) {
        let Some(acc) = job.app.pick(provider, &job.key.cfg.accounts, &tried, Some(session)) else {
            break;
        };
        let first_id = first_acc.get_or_insert_with(|| acc.id.clone());
        let h = match h {
            Some(ch) if *first_id != acc.id && ch.contains_key(TURN_STATE) => Some(stripped.get_or_init(|| without_turn_state(ch))),
            other => other,
        };
        match attempt.run(&acc, h, &mut refreshed).await {
            Ok(up) => return Ok(up),
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

fn without_turn_state(h: &HeaderMap) -> HeaderMap {
    let mut h = h.clone();
    h.remove(TURN_STATE);
    h
}

impl Attempt<'_> {
    async fn run(&self, acc: &Arc<Account>, h: Option<&HeaderMap>, refreshed: &mut Vec<String>) -> Result<Upstream, Step> {
        codex::ensure_fresh(&self.job.app, acc, false).await.map_err(Step::Next)?;
        let guard = acc.acquire();
        let req = self.request(acc, h);
        let sent_at = now_ms();
        let res = match tokio::time::timeout(self.first_timeout, req.send()).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(cool(acc, 15_000, format!("connect: {e}"), UpstreamError::Connect(e))),
            Err(_) => {
                let reason = "timed out waiting for response headers".to_owned();
                return Err(cool(acc, 15_000, reason, UpstreamError::Timeout));
            }
        };
        if self.provider == Provider::Codex {
            codex::apply_rate_headers(acc, res.headers());
        }
        let status = res.status().as_u16();
        if status == 401 && !refreshed.contains(&acc.id) {
            refreshed.push(acc.id.clone());
            if codex::ensure_fresh(&self.job.app, acc, true).await.is_ok() {
                return Err(Step::Again);
            }
        }
        if status >= 400 {
            return Err(rejected(acc, status, res).await);
        }
        let headers = self.headers(res.headers());
        let mut events: Events = Box::pin(res.bytes_stream().eventsource());
        match tokio::time::timeout(self.first_timeout, events.next()).await {
            Ok(Some(Ok(ev))) if is_failure(&ev) => Err(failed_first(acc, &ev)),
            Ok(Some(Ok(first))) => {
                let ttfb = now_ms();
                acc.observe(Some(ttfb - sent_at));
                Ok(Upstream {
                    acc: Arc::clone(acc),
                    guard,
                    first,
                    events,
                    ttfb,
                    headers,
                })
            }
            other => {
                let msg = match other {
                    Err(_) => "timed out waiting for first event".to_owned(),
                    Ok(e) => format!("upstream closed before first event: {:?}", e.and_then(Result::err)),
                };
                Err(cool(acc, 10_000, msg.clone(), UpstreamError::FirstEvent(msg)))
            }
        }
    }

    fn request(&self, acc: &Account, h: Option<&HeaderMap>) -> RequestBuilder {
        let mut rb = if self.provider == Provider::OpenRouter {
            upstream::request(&self.job.app, acc, Method::POST, "/responses", None)
        } else {
            let rb = upstream::request(&self.job.app, acc, Method::POST, "/codex/responses", h);
            let has_session = h.is_some_and(|h| h.contains_key("session_id") || h.contains_key("session-id"));
            if has_session || self.cache_key.is_empty() {
                rb
            } else {
                rb.header("session_id", self.cache_key)
            }
        };
        rb = rb.header(header::CONTENT_TYPE, "application/json");
        if self.compressed {
            rb = rb.header(header::CONTENT_ENCODING, "zstd");
        }
        rb.body(self.body.clone())
    }

    fn headers(&self, upstream: &HeaderMap) -> HeaderMap {
        let mut headers = passthrough_headers(upstream);
        if self.provider == Provider::Codex {
            for (k, v) in self.job.app.accounts.pool_rate_headers(&self.job.key.cfg.accounts) {
                if let Ok(v) = HeaderValue::from_str(&v) {
                    headers.insert(k, v);
                }
            }
        }
        headers
    }
}

fn cool(acc: &Account, ms: i64, reason: String, e: UpstreamError) -> Step {
    acc.observe(None);
    acc.fail(now_ms() + ms, reason);
    Step::Next(e)
}

async fn rejected(acc: &Account, status: u16, res: Response) -> Step {
    let text = res.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or_default();
    let message = v["error"]["message"]
        .as_str()
        .or_else(|| v["detail"].as_str())
        .map_or_else(|| text.chars().take(500).collect(), str::to_owned);
    match classify(status, &v["error"]) {
        Failure::Client => {
            tracing::warn!(account = %acc.id, status, error = %message, "upstream rejected request");
            return Step::Stop(UpstreamError::Status { status, message });
        }
        Failure::Retry(until) => {
            acc.observe(None);
            if until > 0 {
                acc.fail(until, format!("{status}: {message}"));
            }
        }
    }
    tracing::warn!(account = %acc.id, status, error = %message, "upstream error, failing over");
    Step::Next(UpstreamError::Status { status, message })
}

fn failed_first(acc: &Account, ev: &Event) -> Step {
    let v: Value = serde_json::from_str(&ev.data).unwrap_or_default();
    let err = if v["error"].is_object() {
        &v["error"]
    } else if v["response"]["error"].is_object() {
        &v["response"]["error"]
    } else {
        &v
    };
    let msg = err["message"].as_str().unwrap_or("upstream failed").to_owned();
    match classify(0, err) {
        Failure::Client => Step::Stop(UpstreamError::Status { status: 400, message: msg }),
        Failure::Retry(until) => {
            acc.observe(None);
            acc.fail(until, msg.clone());
            Step::Next(UpstreamError::FirstEvent(msg))
        }
    }
}

fn passthrough_headers(h: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in h {
        let n = k.as_str();
        if PASS_HEADERS.contains(&n)
            || (n.starts_with("x-codex-") && !n.starts_with("x-codex-primary-") && !n.starts_with("x-codex-secondary-"))
        {
            out.append(k.clone(), v.clone());
        }
    }
    out
}

fn is_failure(ev: &Event) -> bool {
    ev.event == "error"
        || ev.event == "response.failed"
        || ev.data.starts_with("{\"type\":\"error\"")
        || ev.data.starts_with("{\"type\":\"response.failed\"")
}
