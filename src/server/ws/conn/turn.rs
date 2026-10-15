use super::{Conn, Pending};
use crate::app::{Provider, canonical_model};
use crate::id::sha256_hex;
use crate::server::ws::frames::{close_to_client, error_message, event_type, status_of, to_client, to_upstream};
use crate::time::now_ms;
use crate::upstream::{self, codex};
use axum::extract::ws::{self, WebSocket};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{self as tung, Message};

fn cache_seed(v: &Value) -> String {
    let mut seed = v["instructions"].to_string();
    for it in v["input"].as_array().into_iter().flatten().take(2) {
        seed.push_str(&it.to_string());
    }
    sha256_hex(&seed)[..24].to_owned()
}

impl Conn {
    async fn on_create(&mut self, mut v: Value, client: &mut WebSocket) -> bool {
        let started = now_ms();
        if let Err(e) = self.key.check(started) {
            return client.send(error_message(429, "rate_limit_exceeded", &e.to_string())).await.is_ok();
        }
        let requested = v["model"].as_str().unwrap_or_default().to_owned();
        let model = canonical_model(&requested).to_owned();
        if !self.key.allows_model(&requested) && !self.key.allows_model(&model) {
            let m = format!("model {requested} not allowed for this key");
            return client.send(error_message(403, "invalid_request_error", &m)).await.is_ok();
        }
        if Provider::for_model(&model) != Provider::Codex {
            let m = format!("model {model} is not served over websocket; use POST /v1/responses");
            return client.send(error_message(400, "invalid_request_error", &m)).await.is_ok();
        }
        v["model"] = Value::String(model.clone());
        v["store"] = Value::Bool(false);
        if self.cache.is_none() {
            self.cache = Some(v["prompt_cache_key"].as_str().map_or_else(|| cache_seed(&v), str::to_owned));
        }
        if v.get("prompt_cache_key").is_none() {
            v["prompt_cache_key"] = Value::String(self.cache.clone().unwrap_or_default());
        }
        self.settings = self.app.settings();
        let warm = self.settings.prewarm
            && !self.created
            && v["generate"] != Value::Bool(false)
            && v["previous_response_id"].as_str().is_none_or(str::is_empty);
        self.created = true;
        let (frame, deferred) = if warm {
            let mut w = v.clone();
            w["generate"] = Value::Bool(false);
            (w.to_string(), Some(v))
        } else {
            (v.to_string(), None)
        };
        self.start(frame, model, started, deferred, client).await
    }

    async fn start(&mut self, frame: String, model: String, started: i64, deferred: Option<Value>, client: &mut WebSocket) -> bool {
        self.tried.clear();
        self.pending = Some(Pending {
            frame,
            model,
            started,
            ttfb: None,
            retries: 0,
            acc: None,
            deferred,
        });
        self.dispatch(client).await
    }

    async fn dispatch(&mut self, client: &mut WebSocket) -> bool {
        loop {
            if self.up.is_none()
                && let Err(e) = self.connect().await
            {
                tracing::warn!(status = e.status(), error = %e, "websocket request failed");
                let model = self.pending.take().map(|p| p.model).unwrap_or_default();
                self.record_unsent(&model, e.status());
                return client.send(ws::Message::Text(self.fail_frame(&e).into())).await.is_ok();
            }
            let acc = self.up.as_ref().map(|l| l.acc.clone());
            let Some(frame) = self.pending.as_mut().map(|p| {
                p.acc = acc;
                p.frame.clone()
            }) else {
                return true;
            };
            if self.send_up(Message::Text(frame.into())).await {
                return true;
            }
            if !self.drop_upstream(now_ms() + 15_000, "upstream websocket send failed") {
                self.record(502, &Value::Null);
                return client
                    .send(error_message(502, "api_error", "upstream websocket closed"))
                    .await
                    .is_ok();
            }
        }
    }

    pub(super) async fn on_client(&mut self, m: ws::Message, client: &mut WebSocket) -> bool {
        if let ws::Message::Text(t) = &m
            && event_type(t) == "response.create"
            && let Ok(v) = serde_json::from_str::<Value>(t)
            && v["type"] == "response.create"
        {
            return self.on_create(v, client).await;
        }
        let close = matches!(m, ws::Message::Close(_));
        if let Some(u) = to_upstream(m)
            && self.up.is_some()
        {
            self.send_up(u).await;
        }
        !close
    }

    async fn on_closed(&mut self, close: Option<CloseFrame>, client: &mut WebSocket) -> bool {
        let Some(started) = self.pending.as_ref().map(|p| p.ttfb.is_some()) else {
            self.up = None;
            return true;
        };
        if started {
            self.record(502, &Value::Null);
            let _ = client.send(close_to_client(close)).await;
            return false;
        }
        if self.drop_upstream(now_ms() + 10_000, "upstream websocket closed before first event") {
            tracing::warn!("upstream websocket closed before first event, failing over");
            return self.dispatch(client).await;
        }
        self.record(502, &Value::Null);
        client
            .send(error_message(502, "api_error", "upstream websocket closed before first event"))
            .await
            .is_ok()
    }

    fn fails_over_before_first_event(&mut self, kind: &str, t: &str) -> bool {
        if kind != "error" {
            if let Some(p) = self.pending.as_mut() {
                p.ttfb = Some(now_ms());
                self.tried.clear();
            }
            return false;
        }
        let v: Value = serde_json::from_str(t).unwrap_or_default();
        let status = status_of(v["status"].as_u64().or_else(|| v["status_code"].as_u64()), 0);
        let Some(until) = upstream::retry_until(status, &v["error"]) else {
            return false;
        };
        let msg = v["error"]["message"].as_str().unwrap_or("upstream error").to_owned();
        let max = self.settings.max_attempts.max(1);
        if self.pending.as_ref().is_some_and(|p| p.retries + 1 < max) {
            self.drop_upstream(until, &msg);
            tracing::warn!(status, error = %msg, "upstream websocket error before first event, failing over");
            return true;
        }
        self.cool(until, &msg);
        false
    }

    fn on_event(&mut self, kind: &str, t: &str) -> Option<Value> {
        match kind {
            "codex.rate_limits" => {
                if let (Ok(v), Some(l)) = (serde_json::from_str::<Value>(t), self.up.as_ref()) {
                    codex::apply_rate_event(&l.acc, &v);
                }
                None
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let v: Value = serde_json::from_str(t).unwrap_or_default();
                let resp = &v["response"];
                let status = match resp["status"].as_str() {
                    Some("completed") => 200,
                    Some("incomplete") => 206,
                    _ => 502,
                };
                if kind == "response.failed"
                    && let Some(reset) = upstream::limit_reset(&resp["error"])
                {
                    self.cool(reset, resp["error"]["message"].as_str().unwrap_or("rate limited"));
                }
                let warm_done = self.pending.as_mut().and_then(|p| p.deferred.take()).map(|mut real| {
                    if let (200, Some(id)) = (status, resp["id"].as_str()) {
                        real["previous_response_id"] = Value::String(id.to_owned());
                        real["input"] = json!([]);
                    }
                    real
                });
                self.record(status, resp);
                warm_done
            }
            "error" => {
                let v: Value = serde_json::from_str(t).unwrap_or_default();
                let warm_done = self.pending.as_mut().and_then(|p| p.deferred.take());
                self.record(status_of(v["status"].as_u64(), 502), &Value::Null);
                warm_done
            }
            _ => None,
        }
    }

    pub(super) async fn on_upstream(&mut self, m: Option<Result<Message, tung::Error>>, client: &mut WebSocket) -> bool {
        let m = match m {
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => return true,
            Some(Ok(Message::Close(f))) => return self.on_closed(f, client).await,
            Some(Ok(m)) => m,
            Some(Err(_)) | None => return self.on_closed(None, client).await,
        };
        let warming = self.pending.as_ref().is_some_and(|p| p.deferred.is_some());
        if let Message::Text(t) = &m {
            let kind = event_type(t);
            if self.pending.as_ref().is_some_and(|p| p.ttfb.is_none()) && self.fails_over_before_first_event(kind, t) {
                return self.dispatch(client).await;
            }
            if let Some(real) = self.on_event(kind, t) {
                let model = real["model"].as_str().unwrap_or_default().to_owned();
                return self.start(real.to_string(), model, now_ms(), None, client).await;
            }
        }
        if warming {
            return true;
        }
        match to_client(m) {
            Some(cm) => client.send(cm).await.is_ok(),
            None => true,
        }
    }

    pub(super) async fn on_timeout(&mut self, client: &mut WebSocket) -> bool {
        if self.drop_upstream(now_ms() + 10_000, "timed out waiting for first event") {
            return self.dispatch(client).await;
        }
        self.record(504, &Value::Null);
        let _ = client.send(error_message(504, "api_error", "upstream timed out")).await;
        false
    }
}
