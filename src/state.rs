use crate::now_ms;
use crate::store::{Store, Usage};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering::Relaxed};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AccountCfg {
    pub id: String,
    #[serde(default = "codex")]
    pub kind: String,
    pub email: String,
    pub label: String,
    pub plan: String,
    pub chatgpt_account_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub expires_at: i64,
    pub last_refresh: i64,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub warmup: Option<Warmup>,
    pub created_at: i64,
}

fn codex() -> String {
    "codex".into()
}
fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Warmup {
    pub cron: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "warm_prompt")]
    pub prompt: String,
    #[serde(default)]
    pub last_fired: i64,
}

fn warm_prompt() -> String {
    "hi".into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RateLimit {
    pub primary_used: f64,
    pub primary_window_min: i64,
    pub primary_reset_at: i64,
    pub secondary_used: f64,
    pub secondary_window_min: i64,
    pub secondary_reset_at: i64,
    pub updated_at: i64,
}

impl RateLimit {
    fn weekly_reset(&self, now: i64) -> i64 {
        let reset = if self.secondary_window_min >= self.primary_window_min {
            self.secondary_reset_at
        } else {
            self.primary_reset_at
        };
        if reset > now { reset } else { i64::MAX }
    }

    fn used(&self, now: i64) -> f64 {
        let p = if self.primary_reset_at > now { self.primary_used } else { 0.0 };
        let s = if self.secondary_reset_at > now { self.secondary_used } else { 0.0 };
        p.max(s)
    }
}

pub struct Account {
    pub cfg: RwLock<AccountCfg>,
    pub id: String,
    pub rl: Mutex<RateLimit>,
    pub cooldown_until: AtomicI64,
    pub inflight: AtomicU32,
    pub last_error: Mutex<Option<(i64, String)>>,
    pub refresh_lock: tokio::sync::Mutex<()>,
}

impl Account {
    pub fn new(cfg: AccountCfg) -> Arc<Self> {
        Arc::new(Self {
            id: cfg.id.clone(),
            cfg: RwLock::new(cfg),
            rl: Mutex::new(RateLimit::default()),
            cooldown_until: AtomicI64::new(0),
            inflight: AtomicU32::new(0),
            last_error: Mutex::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn available(&self, now: i64) -> bool {
        self.cfg.read().enabled && self.cooldown_until.load(Relaxed) <= now && self.rl.lock().used(now) < 100.0
    }

    pub fn fail(&self, until: i64, msg: String) {
        tracing::warn!(account = %self.id, until = %crate::ts_iso(until), error = %msg, "account cooling down");
        self.cooldown_until.store(until, Relaxed);
        *self.last_error.lock() = Some((now_ms(), msg));
    }

    pub fn view(&self) -> Value {
        let c = self.cfg.read();
        let now = now_ms();
        let cd = self.cooldown_until.load(Relaxed);
        serde_json::json!({
            "id": c.id, "kind": c.kind, "email": c.email, "label": c.label, "plan": c.plan,
            "enabled": c.enabled, "priority": c.priority, "available": self.available(now),
            "cooldown_until": (cd > now).then(|| crate::ts_iso(cd)),
            "inflight": self.inflight.load(Relaxed),
            "token_expires_at": crate::ts_iso(c.expires_at), "last_refresh": crate::ts_iso(c.last_refresh),
            "rate_limit": rl_view(&self.rl.lock()),
            "last_error": self.last_error.lock().as_ref().map(|(t, m)| serde_json::json!({"at": crate::ts_iso(*t), "message": m})),
            "warmup": c.warmup, "created_at": crate::ts_iso(c.created_at),
        })
    }
}

fn rl_view(r: &RateLimit) -> Value {
    let iso = |t: i64| (t > 0).then(|| crate::ts_iso(t));
    serde_json::json!({
        "primary": {"used_percent": r.primary_used, "window_minutes": r.primary_window_min, "resets_at": iso(r.primary_reset_at)},
        "secondary": {"used_percent": r.secondary_used, "window_minutes": r.secondary_window_min, "resets_at": iso(r.secondary_reset_at)},
        "updated_at": iso(r.updated_at),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limit {
    pub window: Window,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    #[serde(default)]
    pub max_requests: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Window {
    Day,
    Week,
    Month,
    Total,
}

impl Window {
    pub fn start(self, now_ms: i64) -> i64 {
        use chrono::{Datelike, Local, TimeZone};
        let now = Local.timestamp_millis_opt(now_ms).unwrap();
        let d = now.date_naive();
        let day = match self {
            Window::Day => d,
            Window::Week => d - chrono::Days::new(d.weekday().num_days_from_monday() as u64),
            Window::Month => d.with_day(1).unwrap(),
            Window::Total => return 0,
        };
        Local
            .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .map_or(0, |t| t.timestamp_millis())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyCfg {
    pub id: String,
    pub name: String,
    pub hash: String,
    pub prefix: String,
    #[serde(default)]
    pub accounts: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub limits: Vec<Limit>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub expires_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Default, Clone, Copy, Serialize)]
pub struct Counter {
    pub start: i64,
    pub tokens: u64,
    pub cost: f64,
    pub requests: u64,
}

pub struct Key {
    pub cfg: KeyCfg,
    pub counters: Mutex<HashMap<Window, Counter>>,
}

impl Key {
    pub fn check(&self, now: i64) -> Result<(), String> {
        if !self.cfg.enabled {
            return Err("api key disabled".into());
        }
        if self.cfg.expires_at.is_some_and(|e| e <= now) {
            return Err("api key expired".into());
        }
        let mut cs = self.counters.lock();
        for l in &self.cfg.limits {
            let c = roll(&mut cs, l.window, now);
            let hit = l.max_tokens.is_some_and(|m| c.tokens >= m)
                || l.max_cost_usd.is_some_and(|m| c.cost >= m)
                || l.max_requests.is_some_and(|m| c.requests >= m);
            if hit {
                return Err(format!("api key {:?} limit reached", l.window).to_lowercase());
            }
        }
        Ok(())
    }

    pub fn add(&self, now: i64, tokens: u64, cost: f64) {
        let mut cs = self.counters.lock();
        for w in [Window::Day, Window::Week, Window::Month, Window::Total] {
            let c = roll(&mut cs, w, now);
            c.tokens += tokens;
            c.cost += cost;
            c.requests += 1;
        }
    }

    pub fn allows_model(&self, model: &str) -> bool {
        self.cfg.models.is_empty() || self.cfg.models.iter().any(|m| m == model)
    }

    pub fn view(&self) -> Value {
        let c = &self.cfg;
        let now = now_ms();
        let usage: HashMap<_, _> = {
            let mut cs = self.counters.lock();
            [Window::Day, Window::Week, Window::Month, Window::Total]
                .into_iter()
                .map(|w| (format!("{w:?}").to_lowercase(), *roll(&mut cs, w, now)))
                .collect()
        };
        serde_json::json!({
            "id": c.id, "name": c.name, "prefix": c.prefix, "accounts": c.accounts, "models": c.models,
            "limits": c.limits, "enabled": c.enabled, "expires_at": c.expires_at.map(crate::ts_iso),
            "created_at": crate::ts_iso(c.created_at), "usage": usage,
        })
    }
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub strategy: Strategy,
    pub sticky: bool,
    pub max_attempts: u32,
    pub models: Vec<String>,
    pub pricing: HashMap<String, [f64; 3]>,
    pub upstream_base: String,
    pub codex_version: String,
    pub usage_poll_secs: u64,
    pub warmup_model: String,
    pub openrouter_base: String,
    pub openrouter_models: Vec<String>,
    pub first_event_timeout_secs: u64,
    pub idle_timeout_secs: u64,
    pub claude_base: String,
    pub claude_cli_version: String,
    pub claude_models: Vec<String>,
    pub claude_warmup_model: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Preserve,
    Burn,
    RoundRobin,
    Priority,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            strategy: Strategy::Preserve,
            sticky: true,
            max_attempts: 4,
            models: crate::pricing::DEFAULT_MODELS.iter().map(|s| s.to_string()).collect(),
            pricing: crate::pricing::defaults(),
            upstream_base: "https://chatgpt.com/backend-api".into(),
            codex_version: "0.160.0".into(),
            usage_poll_secs: 60,
            warmup_model: "gpt-6-luna".into(),
            claude_base: "https://api.anthropic.com".into(),
            claude_cli_version: "2.1.300".into(),
            claude_models: ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]
                .into_iter()
                .map(String::from)
                .collect(),
            claude_warmup_model: "claude-haiku-5-5".into(),
            first_event_timeout_secs: 90,
            idle_timeout_secs: 600,
            openrouter_base: "https://openrouter.ai/api/v1".into(),
            openrouter_models: [
                "z-ai/glm-5.3-flash",
                "deepseek/deepseek-v4.1-flash",
                "anthropic/claude-opus-5.5",
                "anthropic/claude-sonnet-5.5",
                "anthropic/claude-haiku-5.5",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }
}

pub struct App {
    pub store: Store,
    pub accounts: RwLock<Vec<Arc<Account>>>,
    pub keys: RwLock<HashMap<String, Arc<Key>>>,
    pub settings: RwLock<Arc<Settings>>,
    pub sticky: Mutex<HashMap<String, (String, i64)>>,
    pub rr: AtomicUsize,
    pub logins: Mutex<HashMap<String, Value>>,
    pub live_models: RwLock<Vec<(String, u64)>>,
    pub http: reqwest::Client,
    pub admin_token: String,
    pub started_at: i64,
}

impl App {
    pub fn new(store: Store, admin_token: String) -> Arc<Self> {
        let accounts: Vec<Arc<Account>> = store.load::<AccountCfg>("account").into_iter().map(Account::new).collect();
        let states: HashMap<String, Value> = store.load_raw("account_state").into_iter().collect();
        for a in &accounts {
            if let Some(st) = states.get(&a.id) {
                if let Ok(rl) = serde_json::from_value(st["rl"].clone()) {
                    *a.rl.lock() = rl;
                }
                a.cooldown_until.store(st["cooldown_until"].as_i64().unwrap_or(0), Relaxed);
                if let (Some(t), Some(m)) = (st["last_error_at"].as_i64(), st["last_error"].as_str()) {
                    *a.last_error.lock() = Some((t, m.to_string()));
                }
            }
        }
        store.prune("sticky", "t", now_ms() - 7 * 86_400_000);
        let sticky: HashMap<String, (String, i64)> = store
            .load_raw("sticky")
            .into_iter()
            .filter_map(|(k, v)| Some((k, (v["a"].as_str()?.to_string(), v["t"].as_i64()?))))
            .collect();
        let settings = store.load::<Settings>("settings").pop().unwrap_or_default();
        let now = now_ms();
        let totals: HashMap<Window, HashMap<String, (u64, f64, u64)>> = [Window::Day, Window::Week, Window::Month, Window::Total]
            .into_iter()
            .map(|w| {
                (
                    w,
                    store
                        .key_totals_since(w.start(now))
                        .into_iter()
                        .map(|(k, t, c, n)| (k, (t, c, n)))
                        .collect(),
                )
            })
            .collect();
        let keys = store
            .load::<KeyCfg>("key")
            .into_iter()
            .map(|cfg| {
                let counters = totals
                    .iter()
                    .map(|(w, m)| {
                        let (tokens, cost, requests) = m.get(&cfg.id).copied().unwrap_or_default();
                        (
                            *w,
                            Counter {
                                start: w.start(now),
                                tokens,
                                cost,
                                requests,
                            },
                        )
                    })
                    .collect();
                (
                    cfg.hash.clone(),
                    Arc::new(Key {
                        cfg,
                        counters: Mutex::new(counters),
                    }),
                )
            })
            .collect();
        let http = reqwest::Client::builder()
            .tcp_nodelay(true)
            .pool_idle_timeout(std::time::Duration::from_secs(300))
            .pool_max_idle_per_host(64)
            .http2_adaptive_window(true)
            .connect_timeout(std::time::Duration::from_secs(10))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        Arc::new(Self {
            store,
            accounts: RwLock::new(accounts),
            keys: RwLock::new(keys),
            settings: RwLock::new(Arc::new(settings)),
            sticky: Mutex::new(sticky),
            rr: AtomicUsize::new(0),
            logins: Mutex::new(HashMap::new()),
            live_models: RwLock::new(vec![]),
            http,
            admin_token,
            started_at: now,
        })
    }

    pub fn model_list(&self) -> Vec<(String, u64)> {
        let s = self.settings();
        let live = self.live_models.read();
        let mut out = if live.is_empty() {
            s.models.iter().map(|m| (m.clone(), 272_000)).collect()
        } else {
            live.clone()
        };
        if self.accounts.read().iter().any(|a| a.cfg.read().kind == "openrouter") {
            out.extend(s.openrouter_models.iter().map(|m| (m.clone(), 1_000_000)));
        }
        if self.accounts.read().iter().any(|a| a.cfg.read().kind == "claude") {
            out.extend(s.claude_models.iter().map(|m| (m.clone(), 1_000_000)));
        }
        out
    }

    pub fn canonical_model(model: &str) -> &str {
        model.strip_prefix("openai/").unwrap_or(model)
    }

    pub fn provider_for(model: &str) -> &'static str {
        if model.contains('/') {
            "openrouter"
        } else if model.starts_with("claude") {
            "claude"
        } else {
            "codex"
        }
    }

    pub fn settings(&self) -> Arc<Settings> {
        self.settings.read().clone()
    }

    pub fn save_settings(&self, s: Settings) {
        self.store.put("settings", "settings", &s);
        *self.settings.write() = Arc::new(s);
    }

    pub fn account(&self, id: &str) -> Option<Arc<Account>> {
        self.accounts
            .read()
            .iter()
            .find(|a| a.id == id || a.cfg.read().email == id)
            .cloned()
    }

    pub fn save_account(&self, a: &Account) {
        if !self.accounts.read().iter().any(|x| std::ptr::eq(Arc::as_ptr(x), a)) {
            return;
        }
        let cfg = a.cfg.read().clone();
        self.store.put("account", &cfg.id, &cfg);
    }

    pub fn upsert_account(&self, cfg: AccountCfg) -> Arc<Account> {
        self.store.put("account", &cfg.id, &cfg);
        let mut accs = self.accounts.write();
        if let Some(a) = accs.iter().find(|a| a.id == cfg.id) {
            *a.cfg.write() = cfg;
            return a.clone();
        }
        let a = Account::new(cfg);
        accs.push(a.clone());
        a
    }

    pub fn remove_account(&self, id: &str) -> bool {
        self.store.delete("account", id);
        let mut accs = self.accounts.write();
        let n = accs.len();
        accs.retain(|a| a.id != id);
        n != accs.len()
    }

    pub fn key_by_secret(&self, secret: &str) -> Option<Arc<Key>> {
        self.keys.read().get(&crate::sha256_hex(secret)).cloned()
    }

    pub fn save_key(&self, cfg: KeyCfg) -> Arc<Key> {
        self.store.put("key", &cfg.id, &cfg);
        let mut keys = self.keys.write();
        let counters = keys.get(&cfg.hash).map(|k| k.counters.lock().clone()).unwrap_or_default();
        let k = Arc::new(Key {
            cfg,
            counters: Mutex::new(counters),
        });
        keys.insert(k.cfg.hash.clone(), k.clone());
        k
    }

    pub fn find_key(&self, id_or_name: &str) -> Option<Arc<Key>> {
        self.keys
            .read()
            .values()
            .find(|k| k.cfg.id == id_or_name || k.cfg.name == id_or_name)
            .cloned()
    }

    pub fn remove_key(&self, id: &str) -> bool {
        self.store.delete("key", id);
        let mut keys = self.keys.write();
        let n = keys.len();
        keys.retain(|_, k| k.cfg.id != id);
        n != keys.len()
    }

    pub fn pick(&self, kind: &str, allowed: &[String], exclude: &[String], session: Option<&str>) -> Option<Arc<Account>> {
        let now = now_ms();
        let s = self.settings();
        let accs = self.accounts.read();
        let ok = |a: &&Arc<Account>| {
            a.cfg.read().kind == kind && (allowed.is_empty() || allowed.contains(&a.id)) && !exclude.contains(&a.id) && a.available(now)
        };
        if s.sticky
            && let Some(sess) = session
        {
            let pinned = self.sticky.lock().get(sess).map(|(id, _)| id.clone());
            if let Some(a) = pinned.and_then(|id| accs.iter().find(|a| a.id == id).filter(ok).cloned()) {
                if let Some(e) = self.sticky.lock().get_mut(sess) {
                    e.1 = now;
                }
                return Some(a);
            }
        }
        let cands: Vec<&Arc<Account>> = accs.iter().filter(ok).collect();
        let chosen = match s.strategy {
            Strategy::RoundRobin => cands.get(self.rr.fetch_add(1, Relaxed) % cands.len().max(1)).copied(),
            Strategy::Priority => cands
                .iter()
                .copied()
                .min_by_key(|a| (a.cfg.read().priority, a.inflight.load(Relaxed))),
            Strategy::Preserve => cands.iter().copied().min_by(|a, b| {
                let (ua, ub) = (a.rl.lock().used(now), b.rl.lock().used(now));
                ua.total_cmp(&ub).then(a.inflight.load(Relaxed).cmp(&b.inflight.load(Relaxed)))
            }),
            Strategy::Burn => cands.iter().copied().min_by_key(|a| {
                let r = a.rl.lock();
                (r.weekly_reset(now), -(r.used(now) as i64), a.cfg.read().priority)
            }),
        }?
        .clone();
        if s.sticky
            && let Some(sess) = session
        {
            let mut st = self.sticky.lock();
            if st.len() > 50_000 {
                st.retain(|_, (_, t)| now - *t < 3_600_000);
            }
            st.insert(sess.to_string(), (chosen.id.clone(), now));
            self.store.put_async("sticky", sess, &serde_json::json!({"a": chosen.id, "t": now}));
        }
        Some(chosen)
    }

    pub fn next_available(&self, kind: &str, allowed: &[String]) -> Option<i64> {
        let now = now_ms();
        let mut earliest: Option<i64> = None;
        for a in self.accounts.read().iter() {
            let c = a.cfg.read();
            if c.kind != kind || !c.enabled || !(allowed.is_empty() || allowed.contains(&a.id)) {
                continue;
            }
            let r = a.rl.lock();
            let mut t = a.cooldown_until.load(Relaxed);
            for (used, reset) in [(r.primary_used, r.primary_reset_at), (r.secondary_used, r.secondary_reset_at)] {
                if used >= 100.0 && reset > now {
                    t = t.max(reset);
                }
            }
            if t <= now {
                return None;
            }
            earliest = Some(earliest.map_or(t, |e| e.min(t)));
        }
        earliest
    }

    pub fn persist_runtime(&self) {
        for a in self.accounts.read().iter() {
            let err = a.last_error.lock().clone();
            self.store.put_async(
                "account_state",
                &a.id,
                &serde_json::json!({
                    "rl": *a.rl.lock(), "cooldown_until": a.cooldown_until.load(Relaxed),
                    "last_error_at": err.as_ref().map(|e| e.0), "last_error": err.map(|e| e.1),
                }),
            );
        }
    }

    pub fn record(&self, key: Option<&Key>, u: Usage) {
        tracing::info!(
            endpoint = u.endpoint, key = %u.key_id, account = %u.account_id, model = %u.model, status = u.status,
            input = u.input, cached = u.cached, output = u.output, reasoning = u.reasoning, cost_usd = u.cost,
            latency_ms = u.latency_ms, ttfb_ms = u.ttfb_ms, "request"
        );
        if let Some(k) = key {
            k.add(u.ts, u.input + u.output, u.cost);
        }
        self.store.record(u);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(strategy: Strategy, accs: &[(&str, f64, i64)]) -> Arc<App> {
        let dir = std::env::temp_dir().join(format!("nanolb-test-{}-{}", std::process::id(), crate::random_hex(4)));
        let app = App::new(Store::open(&dir).unwrap(), String::new());
        app.save_settings(Settings {
            strategy,
            ..Default::default()
        });
        let far = now_ms() + 86_400_000;
        for (id, used, reset_in) in accs {
            let a = app.upsert_account(AccountCfg {
                id: id.to_string(),
                kind: "codex".into(),
                enabled: true,
                ..Default::default()
            });
            *a.rl.lock() = RateLimit {
                secondary_used: *used,
                secondary_window_min: 10_080,
                secondary_reset_at: far + reset_in,
                ..Default::default()
            };
        }
        app
    }

    #[test]
    fn preserve_picks_least_used_and_skips_exhausted() {
        let app = app_with(Strategy::Preserve, &[("a", 60.0, 0), ("b", 10.0, 0), ("c", 100.0, 0)]);
        assert_eq!(app.pick("codex", &[], &[], None).unwrap().id, "b");
        assert_eq!(app.pick("codex", &[], &["b".into()], None).unwrap().id, "a");
        assert!(app.pick("codex", &[], &["a".into(), "b".into()], None).is_none());
    }

    #[test]
    fn burn_picks_soonest_weekly_reset() {
        let app = app_with(Strategy::Burn, &[("a", 10.0, 5_000), ("b", 90.0, 9_000), ("c", 50.0, 1_000)]);
        assert_eq!(app.pick("codex", &[], &[], None).unwrap().id, "c");
    }

    #[test]
    fn next_available_is_earliest_reset_only_when_every_account_is_limited() {
        let app = app_with(Strategy::Preserve, &[("a", 100.0, 5_000), ("b", 100.0, 1_000)]);
        let b_reset = app.account("b").unwrap().rl.lock().secondary_reset_at;
        assert_eq!(app.next_available("codex", &[]), Some(b_reset));
        app.account("a").unwrap().rl.lock().secondary_used = 40.0;
        assert_eq!(app.next_available("codex", &[]), None);
    }

    #[test]
    fn key_blocks_after_daily_cost_limit() {
        let k = Key {
            cfg: KeyCfg {
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
            counters: Mutex::new(HashMap::new()),
        };
        let now = now_ms();
        k.add(now, 10, 0.6);
        assert!(k.check(now).is_ok());
        k.add(now, 10, 0.6);
        assert!(k.check(now).is_err());
    }
}
