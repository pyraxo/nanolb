use crate::time::{now_ms, ts_iso};
use chrono::{Datelike, Days, Local, NaiveTime, TimeZone};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Limit {
    pub(crate) window: Window,
    #[serde(default)]
    pub(crate) max_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) max_cost_usd: Option<f64>,
    #[serde(default)]
    pub(crate) max_requests: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Window {
    Day,
    Week,
    Month,
    Total,
}

impl Window {
    pub(crate) const ALL: [Self; 4] = [Self::Day, Self::Week, Self::Month, Self::Total];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Total => "total",
        }
    }

    pub(crate) fn start(self, now_ms: i64) -> i64 {
        let Some(now) = Local.timestamp_millis_opt(now_ms).earliest() else {
            return 0;
        };
        let d = now.date_naive();
        let day = match self {
            Self::Day => d,
            Self::Week => d - Days::new(u64::from(d.weekday().num_days_from_monday())),
            Self::Month => d.with_day(1).unwrap_or(d),
            Self::Total => return 0,
        };
        Local
            .from_local_datetime(&day.and_time(NaiveTime::MIN))
            .earliest()
            .map_or(0, |t| t.timestamp_millis())
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct KeyCfg {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) hash: String,
    pub(crate) prefix: String,
    #[serde(default)]
    pub(crate) accounts: Vec<String>,
    #[serde(default)]
    pub(crate) models: Vec<String>,
    #[serde(default)]
    pub(crate) limits: Vec<Limit>,
    #[serde(default = "yes")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) expires_at: Option<i64>,
    pub(crate) created_at: i64,
}

const fn yes() -> bool {
    true
}

#[derive(Default, Clone, Copy, Serialize)]
pub(crate) struct Counter {
    pub(crate) start: i64,
    pub(crate) tokens: u64,
    pub(crate) cost: f64,
    pub(crate) requests: u64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum KeyRejected {
    #[error("api key disabled")]
    Disabled,
    #[error("api key expired")]
    Expired,
    #[error("api key {0} limit reached")]
    Limit(Window),
}

pub(crate) struct Key {
    pub(crate) cfg: KeyCfg,
    counters: Mutex<HashMap<Window, Counter>>,
}

fn roll(cs: &mut HashMap<Window, Counter>, w: Window, now: i64) -> &mut Counter {
    let start = w.start(now);
    let c = cs.entry(w).or_default();
    if c.start != start {
        *c = Counter {
            start,
            ..Default::default()
        };
    }
    c
}

impl Key {
    pub(crate) fn new(cfg: KeyCfg, counters: HashMap<Window, Counter>) -> Self {
        Self {
            cfg,
            counters: Mutex::new(counters),
        }
    }

    pub(crate) fn counters(&self) -> HashMap<Window, Counter> {
        self.counters.lock().clone()
    }

    pub(crate) fn check(&self, now: i64) -> Result<(), KeyRejected> {
        if !self.cfg.enabled {
            return Err(KeyRejected::Disabled);
        }
        if self.cfg.expires_at.is_some_and(|e| e <= now) {
            return Err(KeyRejected::Expired);
        }
        let mut cs = self.counters.lock();
        for l in &self.cfg.limits {
            let c = roll(&mut cs, l.window, now);
            let hit = l.max_tokens.is_some_and(|m| c.tokens >= m)
                || l.max_cost_usd.is_some_and(|m| c.cost >= m)
                || l.max_requests.is_some_and(|m| c.requests >= m);
            if hit {
                return Err(KeyRejected::Limit(l.window));
            }
        }
        Ok(())
    }

    pub(crate) fn add(&self, now: i64, tokens: u64, cost: f64) {
        let mut cs = self.counters.lock();
        for w in Window::ALL {
            let c = roll(&mut cs, w, now);
            c.tokens += tokens;
            c.cost += cost;
            c.requests += 1;
        }
    }

    pub(crate) fn allows_model(&self, model: &str) -> bool {
        self.cfg.models.is_empty() || self.cfg.models.iter().any(|m| m == model)
    }

    pub(crate) fn view(&self) -> Value {
        let c = &self.cfg;
        let now = now_ms();
        let usage: HashMap<_, _> = {
            let mut cs = self.counters.lock();
            Window::ALL.into_iter().map(|w| (w.as_str(), *roll(&mut cs, w, now))).collect()
        };
        json!({
            "id": c.id, "name": c.name, "prefix": c.prefix, "accounts": c.accounts, "models": c.models,
            "limits": c.limits, "enabled": c.enabled, "expires_at": c.expires_at.map(ts_iso),
            "created_at": ts_iso(c.created_at), "usage": usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Key, KeyCfg, Limit, Window};
    use crate::time::now_ms;
    use std::collections::HashMap;

    #[test]
    fn key_blocks_after_daily_cost_limit() {
        let k = Key::new(
            KeyCfg {
                id: "k".into(),
                name: "k".into(),
                hash: String::new(),
                prefix: String::new(),
                accounts: vec![],
                models: vec![],
                limits: vec![Limit {
                    window: Window::Day,
                    max_tokens: None,
                    max_cost_usd: Some(1.0),
                    max_requests: None,
                }],
                enabled: true,
                expires_at: None,
                created_at: 0,
            },
            HashMap::new(),
        );
        let now = now_ms();
        k.add(now, 10, 0.6);
        assert!(k.check(now).is_ok());
        k.add(now, 10, 0.6);
        assert!(k.check(now).is_err());
    }
}
