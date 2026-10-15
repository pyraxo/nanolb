mod pump;

use super::failover::Upstream;
use crate::app::{Account, App, Key, Provider, Settings};
use crate::server::error::{ApiError, ApiFormat};
use crate::store::{Usage, UsageMeta};
use crate::time::{now_ms, ts_iso};
use crate::translate::{anthropic, chat};
use crate::upstream::{UpstreamError, codex, limit_reset};
use axum::Json;
use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use eventsource_stream::Event;
use futures_util::StreamExt;
use pump::relay;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

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
            (Self::Pass, _) => {}
            (Self::Chat(s), Some(v)) => s.event(v, out),
            (Self::Chat(s), None) => s.event_data(&ev.data, out),
            (Self::Anth(s), Some(v)) => s.event(v, out),
            (Self::Anth(s), None) => s.event_data(&ev.data, out),
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

pub(super) fn tapped(event: &str) -> bool {
    matches!(
        event,
        "response.completed" | "response.failed" | "response.incomplete" | "codex.rate_limits" | "error" | "message"
    )
}

#[derive(Default)]
struct Tap {
    response: Option<Value>,
    items: Vec<Value>,
}

impl Tap {
    fn observe(&mut self, ev: &Event, acc: &Account, collect: bool, parse_all: bool) -> Option<Value> {
        let interesting = tapped(&ev.event) || (collect && ev.event == "response.output_item.done");
        if !interesting && !parse_all {
            return None;
        }
        let v: Value = serde_json::from_str(&ev.data).ok()?;
        match v["type"].as_str().unwrap_or_default() {
            "response.failed" => {
                let err = &v["response"]["error"];
                if let Some(reset) = limit_reset(err) {
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
    pub(super) stream: bool,
}

impl Job {
    pub(super) fn pass(&self) -> bool {
        self.stream && self.format == ApiFormat::Responses
    }

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

pub(super) async fn run(job: Job, up: Upstream, reply: Reply) -> Result<Response, ApiError> {
    if job.stream {
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
        mut body,
        mut sse,
        ttfb,
        headers,
    } = up;
    let parse_all = !matches!(reply, Reply::Responses);
    let mut tap = Tap::default();
    tap.observe(&first, &acc, true, parse_all);
    let idle = job.idle();
    sse.take_dispatched();
    let mut deadline = Instant::now() + idle;
    loop {
        while let Some(ev) = sse.pop() {
            tap.observe(&ev, &acc, true, parse_all);
        }
        let Ok(Some(Ok(chunk))) = timeout_at(deadline, body.next()).await else {
            break;
        };
        sse.feed(chunk);
        if sse.take_dispatched() {
            deadline = Instant::now() + idle;
        }
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
