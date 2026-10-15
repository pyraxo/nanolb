use crate::time::{now_ms, ts_iso};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicI64, AtomicU32};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Provider {
    #[default]
    Codex,
    OpenRouter,
    #[serde(other)]
    Other,
}

impl Provider {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenRouter => "openrouter",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub(crate) struct AccountCfg {
    pub(crate) id: String,
    #[serde(rename = "kind", default)]
    pub(crate) provider: Provider,
    pub(crate) email: String,
    pub(crate) label: String,
    pub(crate) plan: String,
    pub(crate) chatgpt_account_id: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) id_token: String,
    pub(crate) expires_at: i64,
    pub(crate) last_refresh: i64,
    #[serde(default = "yes")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) priority: i64,
    #[serde(default)]
    pub(crate) warmup: Option<Warmup>,
    pub(crate) created_at: i64,
}

const fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Warmup {
    pub(crate) cron: String,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default = "warm_prompt")]
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) last_fired: i64,
}

fn warm_prompt() -> String {
    "hi".into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct RateLimit {
    pub(crate) primary_used: f64,
    pub(crate) primary_window_min: i64,
    pub(crate) primary_reset_at: i64,
    pub(crate) secondary_used: f64,
    pub(crate) secondary_window_min: i64,
    pub(crate) secondary_reset_at: i64,
    pub(crate) updated_at: i64,
}

impl RateLimit {
    pub(crate) const fn weekly_reset(&self, now: i64) -> i64 {
        let reset = if self.secondary_window_min >= self.primary_window_min {
            self.secondary_reset_at
        } else {
            self.primary_reset_at
        };
        if reset > now { reset } else { i64::MAX }
    }

    pub(crate) fn used(&self, now: i64) -> f64 {
        let p = if self.primary_reset_at > now { self.primary_used } else { 0.0 };
        let s = if self.secondary_reset_at > now { self.secondary_used } else { 0.0 };
        p.max(s)
    }

    fn view(&self) -> Value {
        let iso = |t: i64| (t > 0).then(|| ts_iso(t));
        json!({
            "primary": {"used_percent": self.primary_used, "window_minutes": self.primary_window_min, "resets_at": iso(self.primary_reset_at)},
            "secondary": {"used_percent": self.secondary_used, "window_minutes": self.secondary_window_min, "resets_at": iso(self.secondary_reset_at)},
            "updated_at": iso(self.updated_at),
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Health {
    ttfb_ms: f64,
    err: f64,
    at: i64,
}

impl Health {
    const ALPHA: f64 = 0.3;

    pub(crate) fn error_rate(&self, now: i64) -> f64 {
        self.err * 0.5f64.powf((now - self.at).max(0) as f64 / 300_000.0)
    }

    pub(crate) fn score(&self, now: i64) -> f64 {
        (self.ttfb_ms + 1_000.0) * (1.0 + 4.0 * self.error_rate(now))
    }
}

pub(crate) struct Account {
    pub(crate) cfg: RwLock<AccountCfg>,
    pub(crate) id: String,
    pub(crate) rl: Mutex<RateLimit>,
    pub(crate) cooldown_until: AtomicI64,
    pub(crate) inflight: AtomicU32,
    pub(crate) last_error: Mutex<Option<(i64, String)>>,
    pub(crate) refresh_lock: tokio::sync::Mutex<()>,
    pub(crate) health: Mutex<Health>,
}

impl Account {
    pub(crate) fn new(cfg: AccountCfg) -> Arc<Self> {
        Arc::new(Self {
            id: cfg.id.clone(),
            cfg: RwLock::new(cfg),
            rl: Mutex::new(RateLimit::default()),
            cooldown_until: AtomicI64::new(0),
            inflight: AtomicU32::new(0),
            last_error: Mutex::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
            health: Mutex::new(Health::default()),
        })
    }

    pub(crate) fn provider(&self) -> Provider {
        self.cfg.read().provider
    }

    pub(crate) fn acquire(self: &Arc<Self>) -> Inflight {
        self.inflight.fetch_add(1, Relaxed);
        Inflight(self.clone())
    }

    pub(crate) fn observe(&self, ttfb_ms: Option<i64>) {
        let now = now_ms();
        let mut h = self.health.lock();
        let err = h.error_rate(now) * (1.0 - Health::ALPHA);
        match ttfb_ms {
            Some(t) if h.ttfb_ms == 0.0 => h.ttfb_ms = t as f64,
            Some(t) => h.ttfb_ms = Health::ALPHA * t as f64 + (1.0 - Health::ALPHA) * h.ttfb_ms,
            None => {}
        }
        h.err = if ttfb_ms.is_some() { err } else { err + Health::ALPHA };
        h.at = now;
    }

    pub(crate) fn available(&self, now: i64) -> bool {
        self.cfg.read().enabled && self.cooldown_until.load(Relaxed) <= now && self.rl.lock().used(now) < 100.0
    }

    pub(crate) fn fail(&self, until: i64, msg: String) {
        tracing::warn!(account = %self.id, until = %ts_iso(until), error = %msg, "account cooling down");
        self.cooldown_until.store(until, Relaxed);
        *self.last_error.lock() = Some((now_ms(), msg));
    }

    pub(crate) fn clear_cooldown(&self) {
        self.cooldown_until.store(0, Relaxed);
    }

    pub(crate) fn view(&self) -> Value {
        let c = self.cfg.read();
        let now = now_ms();
        let cd = self.cooldown_until.load(Relaxed);
        let hl = *self.health.lock();
        json!({
            "id": c.id, "kind": c.provider, "email": c.email, "label": c.label, "plan": c.plan,
            "enabled": c.enabled, "priority": c.priority, "available": self.available(now),
            "cooldown_until": (cd > now).then(|| ts_iso(cd)),
            "inflight": self.inflight.load(Relaxed),
            "token_expires_at": ts_iso(c.expires_at), "last_refresh": ts_iso(c.last_refresh),
            "rate_limit": self.rl.lock().view(),
            "latency": {
                "ttfb_ewma_ms": hl.ttfb_ms.round(),
                "error_rate": (hl.error_rate(now) * 1000.0).round() / 1000.0,
            },
            "last_error": self.last_error.lock().as_ref().map(|(t, m)| json!({"at": ts_iso(*t), "message": m})),
            "warmup": c.warmup, "created_at": ts_iso(c.created_at),
        })
    }
}

pub(crate) struct Inflight(Arc<Account>);

impl Drop for Inflight {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Relaxed);
    }
}
