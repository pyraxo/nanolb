use crate::app::Account;
use crate::time::now_ms;
use reqwest::header::HeaderMap;
use serde_json::Value;

#[expect(clippy::cast_possible_truncation, reason = "reset times and window lengths fit in i64")]
fn whole(v: f64) -> i64 {
    v as i64
}

fn secs_to_ms(secs: f64) -> i64 {
    whole(secs * 1000.0)
}

pub(crate) fn apply_rate_headers(a: &Account, h: &HeaderMap) {
    let f = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
    let Some(primary_used) = f("x-codex-primary-used-percent") else {
        return;
    };
    let now = now_ms();
    let reset = |p: &str| {
        f(&format!("x-codex-{p}-reset-at"))
            .map(secs_to_ms)
            .or_else(|| f(&format!("x-codex-{p}-reset-after-seconds")).map(|s| now + secs_to_ms(s)))
            .unwrap_or(0)
    };
    let primary_window_min = whole(f("x-codex-primary-window-minutes").unwrap_or(0.0));
    let primary_reset_at = reset("primary");
    let secondary_window_min = whole(f("x-codex-secondary-window-minutes").unwrap_or(0.0));
    let (secondary_used, secondary_reset_at) = if secondary_window_min > 0 {
        (f("x-codex-secondary-used-percent").unwrap_or(0.0), reset("secondary"))
    } else {
        (0.0, 0)
    };
    let mut r = a.rl.lock();
    r.primary_used = primary_used;
    r.primary_window_min = primary_window_min;
    r.primary_reset_at = primary_reset_at;
    r.secondary_window_min = secondary_window_min;
    r.secondary_used = secondary_used;
    r.secondary_reset_at = secondary_reset_at;
    r.updated_at = now;
}

pub(super) fn apply_window(a: &Account, primary: &Value, secondary: &Value) {
    let now = now_ms();
    let reset = |w: &Value| {
        w["reset_at"]
            .as_f64()
            .map(secs_to_ms)
            .or_else(|| w["reset_after_seconds"].as_f64().map(|s| now + secs_to_ms(s)))
            .unwrap_or(0)
    };
    let mins = |w: &Value| {
        w["window_minutes"]
            .as_i64()
            .or_else(|| w["limit_window_seconds"].as_i64().map(|s| s / 60))
            .unwrap_or(0)
    };
    let window = |w: &Value| {
        w.is_object()
            .then(|| (w["used_percent"].as_f64().unwrap_or(0.0), mins(w), reset(w)))
    };
    let (primary, secondary) = (window(primary), window(secondary));
    let mut r = a.rl.lock();
    if let Some((used, min, reset_at)) = primary {
        r.primary_used = used;
        r.primary_window_min = min;
        r.primary_reset_at = reset_at;
    }
    if let Some((used, min, reset_at)) = secondary {
        r.secondary_used = used;
        r.secondary_window_min = min;
        r.secondary_reset_at = reset_at;
    }
    r.updated_at = now;
}

pub(crate) fn apply_rate_event(a: &Account, ev: &Value) {
    let rl = &ev["rate_limits"];
    apply_window(a, &rl["primary"], &rl["secondary"]);
}
