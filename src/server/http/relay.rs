use super::failover::{Events, Upstream};
use crate::app::{Account, App, Key, Provider, Settings};
use crate::server::error::{ApiError, ApiFormat};
use crate::sse;
use crate::store::{Usage, UsageMeta};
use crate::time::{now_ms, ts_iso};
use crate::translate::{anthropic, chat};
use crate::upstream::{UpstreamError, codex, limit_reset_from_error};
use axum::Json;
use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use eventsource_stream::Event;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

const KEEPALIVE: &[u8] = b": keepalive\n\n";

pub(super) enum Reply {
    Responses,
    Chat(chat::Opts),
    Anthropic(anthropic::Opts),
}

enum Tx {
    Pass,
    Chat(chat::Stream),
    Anth(anthropic::Stream),
}

impl From<Reply> for Tx {
    fn from(reply: Reply) -> Self {
        match reply {
            Reply::Responses => Self::Pass,
            Reply::Chat(opts) => Self::Chat(chat::Stream::new(opts)),
            Reply::Anthropic(opts) => Self::Anth(anthropic::Stream::new(opts)),
        }
    }
}

impl Tx {
    fn emit(&mut self, ev: &Event, parsed: Option<&Value>, out: &mut Vec<Bytes>) {
        match (self, parsed) {
            (Self::Pass, _) => out.push(sse::frame((ev.event != "message").then_some(ev.event.as_str()), &ev.data)),
            (Self::Chat(s), Some(v)) => s.event(v, out),
            (Self::Anth(s), Some(v)) => s.event(v, out),
            _ => {}
        }
    }

    fn finish(&mut self, out: &mut Vec<Bytes>) {
        match self {
            Self::Chat(s) => s.finish(out),
            Self::Anth(s) => s.finish(out),
            Self::Pass => {}
        }
    }
}

#[derive(Default)]
struct Tap {
    response: Option<Value>,
    items: Vec<Value>,
}

impl Tap {
    fn observe(&mut self, ev: &Event, acc: &Account, collect: bool, parse_all: bool) -> Option<Value> {
        let interesting = matches!(
            ev.event.as_str(),
            "response.completed" | "response.failed" | "response.incomplete" | "codex.rate_limits" | "error" | "message"
        ) || (collect && ev.event == "response.output_item.done");
        if !interesting && !parse_all {
            return None;
        }
        let v: Value = serde_json::from_str(&ev.data).ok()?;
        match v["type"].as_str().unwrap_or_default() {
            "response.failed" => {
                let err = &v["response"]["error"];
                if let Some(reset) = limit_reset_from_error(&json!({"error": err})) {
                    acc.fail(reset, err["message"].as_str().unwrap_or("rate limited").into());
                }
                self.response = Some(v["response"].clone());
            }
            "response.completed" | "response.incomplete" => self.response = Some(v["response"].clone()),
            "codex.rate_limits" => codex::apply_rate_event(acc, &v),
            "response.output_item.done" if collect => self.items.push(v["item"].clone()),
            _ => {}
        }
        Some(v)
    }
}

pub(super) struct Job {
    pub(super) app: Arc<App>,
    pub(super) settings: Arc<Settings>,
    pub(super) key: Arc<Key>,
    pub(super) format: ApiFormat,
    pub(super) model: String,
    pub(super) started: i64,
}

impl Job {
    fn idle(&self) -> Duration {
        Duration::from_secs(self.settings.idle_timeout_secs.max(30))
    }

    fn record(&self, account_id: &str, model: &str, status: u16, usage: &Value, ttfb: i64) {
        let meta = UsageMeta {
            key_id: &self.key.cfg.id,
            account_id,
            model,
            endpoint: self.format.endpoint(),
            started: self.started,
            ttfb,
        };
        self.app
            .record(Some(&self.key), Usage::new(meta, status, usage, &self.settings.pricing));
    }

    fn finished(&self, acc: &Account, resp: Option<&Value>, ttfb: i64) {
        let status = match resp.and_then(|r| r["status"].as_str()) {
            Some("completed") => 200,
            Some("incomplete") => 206,
            Some(_) => 502,
            None => 499,
        };
        let r = resp.unwrap_or(&Value::Null);
        self.record(&acc.id, r["model"].as_str().unwrap_or(&self.model), status, &r["usage"], ttfb);
    }

    pub(super) fn failed(&self, provider: Provider, e: UpstreamError) -> ApiError {
        let endpoint = self.format.endpoint().as_str();
        if e.status() == 503
            && let Some(until) = self.app.accounts.next_available(provider, &self.key.cfg.accounts)
        {
            tracing::warn!(endpoint, model = %self.model, until = %ts_iso(until), "all accounts rate limited");
            return ApiError::limited(self.format, provider, until);
        }
        let status = e.status();
        tracing::warn!(endpoint, model = %self.model, status, error = %e, "request failed");
        self.record("", &self.model, status, &Value::Null, now_ms());
        ApiError::from((self.format, e))
    }
}

pub(super) async fn run(job: Job, up: Upstream, reply: Reply, stream: bool) -> Result<Response, ApiError> {
    if stream {
        Ok(relay(job, up, reply.into()))
    } else {
        collect(job, up, reply).await
    }
}

async fn collect(job: Job, up: Upstream, reply: Reply) -> Result<Response, ApiError> {
    let Upstream {
        acc,
        guard,
        first,
        mut events,
        ttfb,
        headers,
    } = up;
    let parse_all = !matches!(reply, Reply::Responses);
    let mut tap = Tap::default();
    tap.observe(&first, &acc, true, parse_all);
    let idle = job.idle();
    while let Ok(Some(Ok(ev))) = timeout(idle, events.next()).await {
        tap.observe(&ev, &acc, true, parse_all);
    }
    drop(guard);
    job.finished(&acc, tap.response.as_ref(), ttfb);
    let Some(mut resp) = tap.response.filter(Value::is_object) else {
        return Err(ApiError::new(job.format, 502, "upstream stream ended without a response"));
    };
    if resp["output"].as_array().is_none_or(Vec::is_empty) && !tap.items.is_empty() {
        resp["output"] = Value::Array(tap.items);
    }
    if resp["status"] == "failed" {
        return Err(ApiError::new(
            job.format,
            502,
            resp["error"]["message"].as_str().unwrap_or("upstream failed"),
        ));
    }
    let body = match reply {
        Reply::Responses => resp,
        Reply::Chat(opts) => chat::from_response(&resp, &opts),
        Reply::Anthropic(opts) => anthropic::from_response(&resp, &opts),
    };
    let mut res = Json(body).into_response();
    if job.format == ApiFormat::Responses {
        res.headers_mut().extend(headers);
    }
    Ok(res)
}

struct Pump {
    tx: Tx,
    tap: Tap,
    acc: Arc<Account>,
    events: Events,
    sender: mpsc::Sender<Bytes>,
    out: Vec<Bytes>,
}

impl Pump {
    fn on(&mut self, ev: &Event) {
        let parsed = self.tap.observe(ev, &self.acc, false, !matches!(self.tx, Tx::Pass));
        self.tx.emit(ev, parsed.as_ref(), &mut self.out);
    }

    async fn flush(&mut self) -> bool {
        for b in self.out.drain(..) {
            if self.sender.send(b).await.is_err() {
                return false;
            }
        }
        true
    }

    async fn pump(&mut self, first: &Event, idle: Duration, keepalive: Duration) -> bool {
        self.on(first);
        loop {
            if !self.flush().await {
                return false;
            }
            let deadline = Instant::now() + idle;
            loop {
                match timeout(keepalive, self.events.next()).await {
                    Ok(Some(Ok(ev))) => {
                        self.on(&ev);
                        break;
                    }
                    Err(_) if Instant::now() < deadline => {
                        if self.sender.send(Bytes::from_static(KEEPALIVE)).await.is_err() {
                            return false;
                        }
                    }
                    _ => return true,
                }
            }
        }
    }
}

fn relay(job: Job, up: Upstream, tx: Tx) -> Response {
    let Upstream {
        acc,
        guard,
        first,
        events,
        ttfb,
        headers,
    } = up;
    let idle = job.idle();
    let keepalive = match job.settings.keepalive_secs {
        0 => idle,
        k => Duration::from_secs(k).min(idle),
    };
    let pass = job.format == ApiFormat::Responses;
    let (sender, rx) = mpsc::channel::<Bytes>(128);
    let mut pump = Pump {
        tx,
        tap: Tap::default(),
        acc,
        events,
        sender,
        out: Vec::with_capacity(8),
    };
    tokio::spawn(async move {
        let _guard = guard;
        if pump.pump(&first, idle, keepalive).await {
            pump.tx.finish(&mut pump.out);
            pump.flush().await;
        }
        job.finished(&pump.acc, pump.tap.response.as_ref(), ttfb);
    });
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|b| (Ok::<_, Infallible>(b), rx))
    }));
    let mut res = Response::new(body);
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    if pass {
        h.extend(headers);
    }
    res
}
