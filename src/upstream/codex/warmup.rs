use super::auth::ensure_fresh;
use super::limits::apply_rate_headers;
use super::{prepare_body, request};
use crate::app::{Account, App};
use crate::store::{Endpoint, Usage, UsageMeta};
use crate::time::{now_ms, ts_iso};
use chrono::{DateTime, Local, TimeZone};
use futures_util::StreamExt;
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;

const WARMUP_COOLDOWN_MS: i64 = 30_000;

pub(crate) async fn warmup(app: &Arc<App>, a: &Arc<Account>) -> Value {
    let settings = app.settings();
    let (model, prompt) = a
        .cfg
        .read()
        .warmup
        .clone()
        .map_or_else(|| (String::new(), "hi".to_owned()), |w| (w.model, w.prompt));
    let model = if model.is_empty() { settings.warmup_model.clone() } else { model };
    let started = now_ms();
    if let Err(e) = ensure_fresh(app, a, false).await {
        return json!({"ok": false, "error": e.to_string()});
    }
    let mut body = json!({
        "model": model, "instructions": "Reply with OK only.",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": prompt}]}],
        "reasoning": {"effort": "low"},
    });
    prepare_body(&mut body);
    let res = match request(app, a, Method::POST, "/codex/responses", None).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    let status = res.status().as_u16();
    apply_rate_headers(a, res.headers());
    if status >= 400 {
        let err = res.text().await.unwrap_or_default();
        a.fail(now_ms() + WARMUP_COOLDOWN_MS, format!("warmup {status}: {err}"));
        return json!({"ok": false, "status": status, "error": err});
    }
    let usage = completed_usage(res).await;
    tracing::info!(account = %a.id, %model, status, "warmup sent");
    let meta = UsageMeta {
        key_id: "",
        account_id: &a.id,
        model: &model,
        endpoint: Endpoint::Warmup,
        started,
        ttfb: started,
    };
    app.record(None, Usage::new(meta, status, &usage, &app.settings().pricing));
    mark_fired(app, a);
    json!({"ok": true, "status": status, "at": ts_iso(now_ms()), "usage": usage})
}

async fn completed_usage(res: reqwest::Response) -> Value {
    let mut usage = Value::Null;
    let mut events = eventsource_stream::Eventsource::eventsource(res.bytes_stream());
    while let Some(Ok(ev)) = events.next().await {
        if (ev.event == "response.completed" || ev.data.contains("\"response.completed\""))
            && let Ok(v) = serde_json::from_str::<Value>(&ev.data)
        {
            usage = v["response"]["usage"].clone();
        }
    }
    usage
}

pub(super) fn mark_fired(app: &App, a: &Account) {
    if let Some(w) = a.cfg.write().warmup.as_mut() {
        w.last_fired = now_ms();
    }
    if let Err(e) = app.accounts.save(&app.store, a) {
        tracing::error!(account = %a.id, error = %e, "cannot save account");
    }
}

pub(super) fn window_idle(a: &Account) -> bool {
    let r = a.rl.lock();
    let now = now_ms();
    r.updated_at == 0 || r.primary_reset_at <= now || r.secondary_reset_at <= now
}

pub(super) fn warmup_due(cron: &str, last_fired: i64, now: DateTime<Local>) -> bool {
    let Ok(c) = cron.parse::<croner::Cron>() else { return false };
    let since = if last_fired > 0 {
        let Some(t) = Local.timestamp_millis_opt(last_fired).single() else {
            return false;
        };
        t
    } else {
        now - chrono::Duration::minutes(1)
    };
    c.find_next_occurrence(&since, false).is_ok_and(|t| t <= now)
}
