use crate::app::{App, Provider, Warmup};
use crate::control::admin::{AdminError, account_arg, str_arg};
use crate::time::{now_ms, parse_since, ts_iso};
use crate::upstream::codex;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn sessions_list(app: &App, a: &Value) -> Value {
    let key = a["key"].as_str();
    let resolved = a["account"]
        .as_str()
        .map(|q| app.accounts.resolve(q).map_or_else(|_| q.to_string(), |acc| acc.id.clone()));
    let account = resolved.as_deref();
    let limit = a["limit"].as_u64().unwrap_or(100) as usize;
    let mut rows: Vec<(String, String, i64)> = app
        .sessions
        .list()
        .into_iter()
        .filter(|(k, p)| key.is_none_or(|q| k.contains(q)) && account.is_none_or(|q| p.account == q))
        .map(|(k, p)| (k, p.account, p.at))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    json!(
        rows.into_iter()
            .take(limit)
            .map(|(k, acc, t)| json!({"key": k, "account": acc, "last_used": ts_iso(t)}))
            .collect::<Vec<_>>()
    )
}

pub(crate) fn session_delete(app: &App, a: &Value) -> Result<Value, AdminError> {
    let key = str_arg(a, "key")?;
    let removed = app.sessions.remove(&app.store, key);
    Ok(json!({"removed": removed}))
}

pub(crate) fn sessions_purge(app: &App, a: &Value) -> Result<Value, AdminError> {
    let all = a["all"].as_bool().unwrap_or(false);
    let resolved = match a["account"].as_str() {
        Some(q) => Some(app.accounts.resolve(q)?.id.clone()),
        None => None,
    };
    let account = resolved.as_deref();
    let cutoff = match a.get("older_than").filter(|v| !v.is_null()) {
        Some(v) => Some(parse_since(v)?),
        None => None,
    };
    if !all && account.is_none() && cutoff.is_none() {
        return Err(AdminError::Invalid("sessions_purge needs all, account or older_than".into()));
    }
    let removed = app.sessions.purge(&app.store, |p| {
        all || account.is_some_and(|q| p.account == q) || cutoff.is_some_and(|c| p.at < c)
    });
    Ok(json!({"removed": removed}))
}

fn codex_account(app: &App, a: &Value) -> Result<Arc<crate::app::Account>, AdminError> {
    let acc = account_arg(app, a)?;
    if acc.provider() != Provider::Codex {
        return Err(AdminError::Invalid(format!("account {} is not a codex account", acc.id)));
    }
    Ok(acc)
}

pub(crate) async fn reset_credits_list(app: &Arc<App>, a: &Value) -> Result<Value, AdminError> {
    let acc = codex_account(app, a)?;
    codex::ensure_fresh(app, &acc, false).await?;
    Ok(codex::wham(app, &acc, reqwest::Method::GET, "rate-limit-reset-credits", None).await?)
}

pub(crate) async fn reset_credits_consume(app: &Arc<App>, a: &Value) -> Result<Value, AdminError> {
    let acc = codex_account(app, a)?;
    codex::ensure_fresh(app, &acc, false).await?;
    let credit_id = match a["credit_id"].as_str() {
        Some(c) => c.to_string(),
        None => {
            let list = codex::wham(app, &acc, reqwest::Method::GET, "rate-limit-reset-credits", None).await?;
            list["credits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["status"].as_str() == Some("available"))
                .min_by_key(|c| c["expires_at"].as_str().unwrap_or("9999").to_string())
                .and_then(|c| c["id"].as_str().map(str::to_string))
                .ok_or_else(|| AdminError::Invalid(format!("account {} has no available reset credit", acc.id)))?
        }
    };
    let body = json!({"credit_id": credit_id, "redeem_request_id": crate::id::uuid_v4()});
    let mut res = codex::wham(app, &acc, reqwest::Method::POST, "rate-limit-reset-credits/consume", Some(body)).await?;
    let _ = codex::fetch_usage(app, &acc).await;
    res["account"] = acc.view();
    Ok(res)
}

pub(crate) struct Projection {
    pub(crate) exhausts_at: Option<i64>,
    pub(crate) before_reset: bool,
}

pub(crate) fn project_exhaustion(
    used_percent: f64,
    now: i64,
    reset_at: i64,
    window_ms: i64,
    window_tokens: u64,
    recent_tokens: u64,
    lookback_ms: i64,
) -> Projection {
    if used_percent >= 100.0 {
        return Projection {
            exhausts_at: Some(now),
            before_reset: true,
        };
    }
    let elapsed = (window_ms - (reset_at - now)).max(1) as f64;
    let rate = if window_tokens > 0 && used_percent > 0.0 {
        let tokens_per_pct = window_tokens as f64 / used_percent;
        recent_tokens as f64 / lookback_ms.max(1) as f64 / tokens_per_pct
    } else {
        used_percent / elapsed
    };
    if rate <= 0.0 {
        return Projection {
            exhausts_at: None,
            before_reset: false,
        };
    }
    let eta = now + ((100.0 - used_percent) / rate) as i64;
    Projection {
        exhausts_at: Some(eta),
        before_reset: eta < reset_at,
    }
}

fn parse_hhmm(v: &str) -> Result<u32, AdminError> {
    let bad = || AdminError::Invalid("work_start must be HH:MM".into());
    let (h, m) = v.split_once(':').ok_or_else(bad)?;
    let (h, m): (u32, u32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

pub(crate) fn plan(app: &App, a: &Value) -> Result<Value, AdminError> {
    let now = now_ms();
    let lookback = (now - parse_since(a.get("lookback").filter(|v| !v.is_null()).unwrap_or(&json!("6h")))?).max(60_000);
    let start = parse_hhmm(a["work_start"].as_str().unwrap_or("09:00"))?;
    let apply = a["apply"].as_bool().unwrap_or(false);
    let mut accounts: Vec<_> = app
        .accounts
        .read()
        .iter()
        .filter(|x| x.provider() == Provider::Codex)
        .cloned()
        .collect();
    accounts.sort_by_key(|x| (x.cfg.read().priority, x.id.clone()));
    let rls: Vec<_> = accounts.iter().map(|x| x.rl.lock().clone()).collect();
    let plannable = rls.iter().filter(|r| (1..=1440).contains(&r.primary_window_min)).count().max(1) as i64;
    let mut k = 0i64;
    let mut out = Vec::new();
    for (acc, r) in accounts.iter().zip(&rls) {
        let mut windows = Vec::new();
        for (name, used, win_min, reset_at) in [
            ("primary", r.primary_used, r.primary_window_min, r.primary_reset_at),
            ("secondary", r.secondary_used, r.secondary_window_min, r.secondary_reset_at),
        ] {
            if win_min <= 0 || reset_at <= now {
                continue;
            }
            let window_ms = win_min * 60_000;
            let window_start = reset_at - window_ms;
            let window_tokens = app.store.account_tokens_since(&acc.id, window_start);
            let lb = lookback.min(now - window_start).max(60_000);
            let recent = app.store.account_tokens_since(&acc.id, now - lb);
            let p = project_exhaustion(used, now, reset_at, window_ms, window_tokens, recent, lb);
            windows.push(json!({
                "window": name, "window_minutes": win_min, "used_percent": used, "resets_at": ts_iso(reset_at),
                "exhausts_at": p.exhausts_at.map(ts_iso), "exhausts_before_reset": p.before_reset,
                "window_tokens": window_tokens, "recent_tokens": recent,
            }));
        }
        let recommended = if (1..=1440).contains(&r.primary_window_min) {
            let at = (start as i64 + k * r.primary_window_min / plannable) % 1440;
            k += 1;
            Some(json!({
                "cron": format!("{} {} * * *", at % 60, at / 60),
                "fires_at_local": format!("{:02}:{:02}", at / 60, at % 60),
                "window_minutes": r.primary_window_min,
            }))
        } else {
            None
        };
        let mut applied = false;
        if apply && let Some(rec) = &recommended {
            let mut c = acc.cfg.write();
            let (model, prompt) = c.warmup.as_ref().map(|w| (w.model.clone(), w.prompt.clone())).unwrap_or_default();
            c.warmup = Some(Warmup {
                cron: rec["cron"].as_str().unwrap().into(),
                model,
                prompt: if prompt.is_empty() { "hi".into() } else { prompt },
                last_fired: now,
            });
            drop(c);
            app.accounts.save(&app.store, acc)?;
            applied = true;
        }
        out.push(json!({
            "account": acc.id, "label": acc.cfg.read().label, "windows": windows,
            "current_warmup": acc.cfg.read().warmup.as_ref().map(|w| w.cron.clone()),
            "recommended_warmup": recommended, "applied": applied,
        }));
    }
    Ok(json!({"generated_at": ts_iso(now), "work_start": format!("{:02}:{:02}", start / 60, start % 60), "accounts": out}))
}

pub(crate) fn analytics(app: &App, a: &Value) -> Result<Value, AdminError> {
    let since = parse_since(a.get("since").filter(|v| !v.is_null()).unwrap_or(&json!("7d")))?;
    let view = a["view"].as_str().unwrap_or("all");
    let by = a["by"].as_str().unwrap_or("account");
    let bucket = a["bucket"].as_str().unwrap_or("day");
    let limit = a["limit"].as_i64().unwrap_or(10).clamp(1, 1000);
    if view != "all" {
        return Ok(app.store.analytics(view, by, bucket, since, limit)?);
    }
    let mut o = serde_json::Map::new();
    for v in ["series", "latency", "errors", "cache", "top", "utilization"] {
        o.insert(v.into(), app.store.analytics(v, by, bucket, since, limit)?);
    }
    Ok(Value::Object(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhaustion_follows_recent_rate() {
        let now = 1_000_000_000;
        let min = 60_000;
        let p = project_exhaustion(50.0, now, now + 100 * min, 300 * min, 1000, 1200, 60 * min);
        assert!((p.exhausts_at.unwrap() - (now + 50 * min)).abs() <= 1);
        assert!(p.before_reset);
        let slow = project_exhaustion(50.0, now, now + 100 * min, 300 * min, 1000, 300, 60 * min);
        assert!((slow.exhausts_at.unwrap() - (now + 200 * min)).abs() <= 1);
        assert!(!slow.before_reset);
        assert!(
            project_exhaustion(0.0, now, now + 100 * min, 300 * min, 0, 0, 60 * min)
                .exhausts_at
                .is_none()
        );
    }
}
