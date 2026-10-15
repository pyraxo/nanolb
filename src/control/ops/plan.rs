use crate::app::account::RateLimit;
use crate::app::{Account, App, Provider, Warmup};
use crate::control::admin::AdminError;
use crate::time::{now_ms, parse_since, ts_iso};
use serde_json::{Value, json};
use std::sync::Arc;

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
    #[expect(clippy::cast_possible_truncation, reason = "the offset in milliseconds fits in i64")]
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

struct Recommendation {
    cron: String,
    at: i64,
    window_minutes: i64,
}

impl Recommendation {
    fn view(&self) -> Value {
        json!({
            "cron": self.cron,
            "fires_at_local": format!("{:02}:{:02}", self.at / 60, self.at % 60),
            "window_minutes": self.window_minutes,
        })
    }
}

fn is_plannable(r: &RateLimit) -> bool {
    (1..=1440).contains(&r.primary_window_min)
}

fn window_views(app: &App, acc: &Account, r: &RateLimit, now: i64, lookback: i64) -> Vec<Value> {
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
    windows
}

fn apply_warmup(app: &App, acc: &Account, rec: &Recommendation, now: i64) -> Result<(), AdminError> {
    {
        let mut c = acc.cfg.write();
        let (model, prompt) = c.warmup.as_ref().map(|w| (w.model.clone(), w.prompt.clone())).unwrap_or_default();
        c.warmup = Some(Warmup {
            cron: rec.cron.clone(),
            model,
            prompt: if prompt.is_empty() { "hi".into() } else { prompt },
            last_fired: now,
        });
    }
    app.accounts.save(&app.store, acc)?;
    Ok(())
}

fn codex_accounts(app: &App) -> Vec<Arc<Account>> {
    let mut accounts: Vec<_> = app
        .accounts
        .read()
        .iter()
        .filter(|x| x.provider() == Provider::Codex)
        .cloned()
        .collect();
    accounts.sort_by_key(|x| (x.cfg.read().priority, x.id.clone()));
    accounts
}

pub(crate) fn plan(app: &App, a: &Value) -> Result<Value, AdminError> {
    let now = now_ms();
    let default_lookback = json!("6h");
    let lookback = (now - parse_since(a.get("lookback").filter(|v| !v.is_null()).unwrap_or(&default_lookback))?).max(60_000);
    let start = parse_hhmm(a["work_start"].as_str().unwrap_or("09:00"))?;
    let apply = a["apply"].as_bool().unwrap_or(false);
    let accounts = codex_accounts(app);
    let rls: Vec<RateLimit> = accounts.iter().map(|x| *x.rl.lock()).collect();
    let plannable = i64::try_from(rls.iter().filter(|r| is_plannable(r)).count().max(1)).unwrap_or(1);
    let mut k = 0i64;
    let mut out = Vec::new();
    for (acc, r) in accounts.iter().zip(&rls) {
        let windows = window_views(app, acc, r, now, lookback);
        let recommended = is_plannable(r).then(|| {
            let at = (i64::from(start) + k * r.primary_window_min / plannable) % 1440;
            k += 1;
            Recommendation {
                cron: format!("{} {} * * *", at % 60, at / 60),
                at,
                window_minutes: r.primary_window_min,
            }
        });
        let mut applied = false;
        if apply && let Some(rec) = &recommended {
            apply_warmup(app, acc, rec, now)?;
            applied = true;
        }
        out.push(json!({
            "account": acc.id, "label": acc.cfg.read().label, "windows": windows,
            "current_warmup": acc.cfg.read().warmup.as_ref().map(|w| w.cron.clone()),
            "recommended_warmup": recommended.as_ref().map(Recommendation::view), "applied": applied,
        }));
    }
    Ok(json!({"generated_at": ts_iso(now), "work_start": format!("{:02}:{:02}", start / 60, start % 60), "accounts": out}))
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
