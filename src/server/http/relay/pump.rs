use super::{Job, Tap, Tx};
use crate::app::Account;
use crate::server::error::ApiFormat;
use crate::server::http::failover::{Body as Upbody, Upstream};
use crate::sse::Splitter;
use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use eventsource_stream::Event;
use futures_util::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

const KEEPALIVE: &[u8] = b": keepalive\n\n";

struct Pump {
    tx: Tx,
    tap: Tap,
    acc: Arc<Account>,
    body: Upbody,
    sse: Splitter,
    sender: mpsc::Sender<Bytes>,
    out: Vec<Bytes>,
}

impl Pump {
    fn on(&mut self, ev: &Event) {
        let parsed = self.tap.observe(ev, &self.acc, false, false);
        self.tx.emit(ev, parsed.as_ref(), &mut self.out);
    }

    fn drain(&mut self) {
        while let Some(ev) = self.sse.pop() {
            self.on(&ev);
        }
        self.sse.released(&mut self.out);
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
        self.sse.take_dispatched();
        let mut last = Instant::now();
        let tick = tokio::time::sleep_until(last + keepalive);
        tokio::pin!(tick);
        loop {
            self.drain();
            if !self.flush().await {
                return false;
            }
            loop {
                tokio::select! {
                    biased;
                    chunk = self.body.next() => {
                        let Some(Ok(chunk)) = chunk else { return true };
                        self.sse.feed(chunk);
                        if self.sse.take_dispatched() {
                            last = Instant::now();
                        }
                        break;
                    }
                    () = &mut tick => {
                        let now = Instant::now();
                        if now >= last + idle {
                            return true;
                        }
                        if now < last + keepalive {
                            tick.as_mut().reset(last + keepalive);
                            continue;
                        }
                        if self.sender.send(Bytes::from_static(KEEPALIVE)).await.is_err() {
                            return false;
                        }
                        tick.as_mut().reset(now + keepalive);
                    }
                }
            }
        }
    }
}

pub(super) fn relay(job: Job, up: Upstream, tx: Tx) -> Response {
    let Upstream {
        acc,
        guard,
        first,
        body,
        sse,
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
        body,
        sse,
        sender,
        out: Vec::with_capacity(8),
    };
    tokio::spawn(async move {
        let _guard = guard;
        if pump.pump(&first, idle, keepalive).await {
            pump.sse.rest(&mut pump.out);
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
