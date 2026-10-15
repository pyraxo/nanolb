use super::prelude;
use crate::app::{Account, App, Key, Provider, Window};
use crate::server::error::ApiError;
use crate::time::{now_ms, ts_iso};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::sync::Arc;

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "saturating conversion of micro-dollars"
)]
fn micros(x: f64) -> u64 {
    (x * 1e6) as u64
}

fn window_value(vals: &[(f64, i64, i64)], now: i64) -> Value {
    let known: Vec<_> = vals.iter().filter(|v| v.1 > 0).collect();
    if known.is_empty() {
        return Value::Null;
    }
    let used = known.iter().map(|v| if v.2 > now { v.0 } else { 0.0 }).sum::<f64>() / known.len() as f64;
    let reset = known.iter().map(|v| v.2).filter(|r| *r > now).min().unwrap_or(0);
    json!({
        "used_percent": used,
        "limit_window_seconds": known.iter().map(|v| v.1).max().unwrap_or(0) * 60,
        "reset_after_seconds": if reset > 0 { (reset - now) / 1000 } else { 0 },
        "reset_at": reset / 1000,
    })
}

fn pool(app: &App, key: &Key) -> (Value, Value, bool, String) {
    let now = now_ms();
    let accs: Vec<Arc<Account>> = app
        .accounts
        .read()
        .iter()
        .filter(|a| {
            let c = a.cfg.read();
            c.provider == Provider::Codex && c.enabled && (key.cfg.accounts.is_empty() || key.cfg.accounts.contains(&a.id))
        })
        .cloned()
        .collect();
    let (mut p, mut s) = (vec![], vec![]);
    for a in &accs {
        let r = a.rl.lock();
        p.push((r.primary_used, r.primary_window_min, r.primary_reset_at));
        s.push((r.secondary_used, r.secondary_window_min, r.secondary_reset_at));
    }
    let plan = accs.first().map_or_else(|| "plus".to_owned(), |a| a.cfg.read().plan.clone());
    (
        window_value(&p, now),
        window_value(&s, now),
        accs.iter().any(|a| a.available(now)),
        plan,
    )
}

pub(super) async fn pool_usage(State(app): State<Arc<App>>, h: HeaderMap) -> Result<Response, ApiError> {
    let key = prelude(&app, &h)?;
    let (primary, secondary, allowed, plan) = pool(&app, &key);
    Ok(axum::Json(json!({
        "plan_type": plan,
        "rate_limit": {
            "allowed": allowed, "limit_reached": !allowed,
            "primary_window": primary, "secondary_window": secondary,
        },
        "credits": null,
        "additional_rate_limits": [],
    }))
    .into_response())
}

fn key_limits(key: &Key) -> Vec<Value> {
    let usage = key.view()["usage"].clone();
    let mut out = vec![];
    for l in &key.cfg.limits {
        let name = format!("{:?}", l.window).to_lowercase();
        let c = &usage[&name];
        let start = c["start"].as_i64().unwrap_or(0);
        let reset = match l.window {
            Window::Day => Some(start + 86_400_000),
            Window::Week => Some(start + 7 * 86_400_000),
            Window::Month => Some(Window::Month.start(start + 32 * 86_400_000)),
            Window::Total => None,
        };
        let dims = [
            ("total_tokens", l.max_tokens, c["tokens"].as_u64()),
            ("cost_usd", l.max_cost_usd.map(micros), c["cost"].as_f64().map(micros)),
            ("requests", l.max_requests, c["requests"].as_u64()),
        ];
        for (ty, max, cur) in dims {
            let Some(max) = max else { continue };
            let cur = cur.unwrap_or(0).min(max);
            out.push(json!({
                "limit_type": ty, "limit_window": name,
                "max_value": max, "current_value": cur, "remaining_value": max - cur,
                "model_filter": null, "reset_at": reset.map(ts_iso), "source": "api_key_limit",
            }));
        }
    }
    out
}

pub(super) async fn key_usage(State(app): State<Arc<App>>, h: HeaderMap) -> Result<Response, ApiError> {
    let key = prelude(&app, &h)?;
    let total = key.view()["usage"]["total"].clone();
    let cached = app.store.usage_summary(0, "key").unwrap_or_default();
    let cached = cached
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["group"] == key.cfg.id.as_str()))
        .and_then(|r| r["cached_tokens"].as_i64())
        .unwrap_or(0);
    let (primary, secondary, _, _) = pool(&app, &key);
    Ok(axum::Json(json!({
        "request_count": total["requests"], "total_tokens": total["tokens"],
        "cached_input_tokens": cached, "total_cost_usd": total["cost"],
        "limits": key_limits(&key), "upstream_limits": [],
        "account_pool_usage": {"primary": primary["used_percent"], "secondary": secondary["used_percent"]},
    }))
    .into_response())
}
