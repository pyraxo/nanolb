use super::account::{Account, AccountCfg, Provider};
use crate::store::{Store, StoreError};
use crate::time::now_ms;
use axum::http::StatusCode;
use parking_lot::{RwLock, RwLockReadGuard};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

#[derive(Debug, thiserror::Error)]
pub(crate) enum LookupError {
    #[error("account {0} not found (use an id, email or label)")]
    NotFound(String),
    #[error("account {query} is ambiguous, use one of the ids: {}", ids.join(", "))]
    Ambiguous { query: String, ids: Vec<String> },
}

impl LookupError {
    pub(crate) const fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Ambiguous { .. } => StatusCode::BAD_REQUEST,
        }
    }
}

const RATE_HEADERS: [[&str; 3]; 2] = [
    [
        "x-codex-primary-used-percent",
        "x-codex-primary-window-minutes",
        "x-codex-primary-reset-at",
    ],
    [
        "x-codex-secondary-used-percent",
        "x-codex-secondary-window-minutes",
        "x-codex-secondary-reset-at",
    ],
];

fn plan_weights(plan: &str) -> [f64; 2] {
    match plan {
        "free" => [0.0, 0.15],
        "pro" | "enterprise" => [6.67, 6.67],
        "prolite" => [5.0, 5.0],
        _ => [1.0, 1.0],
    }
}

fn permitted(allowed: &[String], id: &String) -> bool {
    allowed.is_empty() || allowed.contains(id)
}

pub(crate) struct Accounts {
    list: RwLock<Vec<Arc<Account>>>,
    rr: AtomicUsize,
}

impl Accounts {
    pub(crate) fn load(store: &Store) -> Result<Self, StoreError> {
        let list: Vec<Arc<Account>> = store.load::<AccountCfg>("account")?.into_iter().map(Account::new).collect();
        let states: HashMap<String, Value> = store.load_raw("account_state")?.into_iter().collect();
        for a in &list {
            if let Some(st) = states.get(&a.id) {
                if let Ok(rl) = serde_json::from_value(st["rl"].clone()) {
                    *a.rl.lock() = rl;
                }
                a.cooldown_until.store(st["cooldown_until"].as_i64().unwrap_or(0), Relaxed);
                if let (Some(t), Some(m)) = (st["last_error_at"].as_i64(), st["last_error"].as_str()) {
                    *a.last_error.lock() = Some((t, m.to_owned()));
                }
            }
        }
        Ok(Self {
            list: RwLock::new(list),
            rr: AtomicUsize::new(0),
        })
    }

    pub(crate) fn read(&self) -> RwLockReadGuard<'_, Vec<Arc<Account>>> {
        self.list.read()
    }

    pub(crate) fn snapshot(&self) -> Vec<Arc<Account>> {
        self.list.read().clone()
    }

    pub(crate) fn len(&self) -> usize {
        self.list.read().len()
    }

    pub(crate) fn next_rr(&self) -> usize {
        self.rr.fetch_add(1, Relaxed)
    }

    pub(crate) fn any(&self, provider: Provider) -> bool {
        self.list.read().iter().any(|a| a.provider() == provider)
    }

    pub(crate) fn get(&self, id: &str) -> Option<Arc<Account>> {
        self.list.read().iter().find(|a| a.id == id).cloned()
    }

    pub(crate) fn resolve(&self, query: &str) -> Result<Arc<Account>, LookupError> {
        let accs = self.list.read();
        if let Some(a) = accs.iter().find(|a| a.id == query) {
            return Ok(a.clone());
        }
        let hits: Vec<Arc<Account>> = accs
            .iter()
            .filter(|a| {
                let c = a.cfg.read();
                c.email.eq_ignore_ascii_case(query) || c.label.eq_ignore_ascii_case(query)
            })
            .cloned()
            .collect();
        drop(accs);
        match hits.as_slice() {
            [a] => Ok(a.clone()),
            [] => Err(LookupError::NotFound(query.to_owned())),
            many => Err(LookupError::Ambiguous {
                query: query.to_owned(),
                ids: many.iter().map(|a| a.id.clone()).collect(),
            }),
        }
    }

    pub(crate) fn save(&self, store: &Store, a: &Account) -> Result<(), StoreError> {
        if !self.list.read().iter().any(|x| std::ptr::eq(Arc::as_ptr(x), a)) {
            return Ok(());
        }
        let cfg = a.cfg.read().clone();
        store.put("account", &cfg.id, &cfg)
    }

    pub(crate) fn upsert(&self, store: &Store, cfg: AccountCfg) -> Result<Arc<Account>, StoreError> {
        store.put("account", &cfg.id, &cfg)?;
        let mut accs = self.list.write();
        if let Some(a) = accs.iter().find(|a| a.id == cfg.id).cloned() {
            drop(accs);
            *a.cfg.write() = cfg;
            return Ok(a);
        }
        let a = Account::new(cfg);
        accs.push(a.clone());
        drop(accs);
        Ok(a)
    }

    pub(crate) fn add(&self, store: &Store, mut cfg: AccountCfg) -> Result<Arc<Account>, StoreError> {
        if let Some(old) = self.get(&cfg.id) {
            let o = old.cfg.read();
            cfg.enabled = o.enabled;
            cfg.priority = o.priority;
            cfg.warmup.clone_from(&o.warmup);
            cfg.created_at = o.created_at;
        }
        let a = self.upsert(store, cfg)?;
        a.clear_cooldown();
        *a.last_error.lock() = None;
        Ok(a)
    }

    pub(crate) fn remove(&self, store: &Store, id: &str) -> Result<bool, StoreError> {
        store.delete("account", id)?;
        let mut accs = self.list.write();
        let n = accs.len();
        accs.retain(|a| a.id != id);
        Ok(n != accs.len())
    }

    pub(crate) fn persist_runtime(&self, store: &Store) {
        for a in self.list.read().iter() {
            let err = a.last_error.lock().clone();
            store.put_async(
                "account_state",
                &a.id,
                &json!({
                    "rl": *a.rl.lock(), "cooldown_until": a.cooldown_until.load(Relaxed),
                    "last_error_at": err.as_ref().map(|e| e.0), "last_error": err.map(|e| e.1),
                }),
            );
        }
    }

    pub(crate) fn pool_rate_headers(&self, allowed: &[String]) -> Vec<(&'static str, String)> {
        let now = now_ms();
        let mut agg = [(0.0f64, 0.0f64, i64::MAX, 0i64); 2];
        for a in self.list.read().iter() {
            let weights = {
                let c = a.cfg.read();
                if c.kind.provider() != Provider::Codex || !c.enabled || !permitted(allowed, &a.id) {
                    continue;
                }
                plan_weights(&c.plan)
            };
            let r = *a.rl.lock();
            if r.updated_at == 0 {
                continue;
            }
            let mut wins = [
                (r.primary_used, r.primary_window_min, r.primary_reset_at),
                (r.secondary_used, r.secondary_window_min, r.secondary_reset_at),
            ];
            if wins[1].1 <= 0 && wins[0].1 >= 10_080 {
                wins = [(0.0, 0, 0), wins[0]];
            }
            for ((used, mins, reset), (g, w)) in wins.into_iter().zip(agg.iter_mut().zip(weights)) {
                if mins <= 0 {
                    continue;
                }
                g.3 = g.3.max(mins);
                g.0 += w;
                if reset > now {
                    g.1 += w * used;
                    g.2 = g.2.min(reset);
                }
            }
        }
        let mut out = vec![];
        for ((cap, used, reset, mins), names) in agg.into_iter().zip(RATE_HEADERS) {
            if mins == 0 || cap <= 0.0 {
                continue;
            }
            out.push((names[0], format!("{}", (used / cap * 100.0).round() / 100.0)));
            out.push((names[1], mins.to_string()));
            if reset != i64::MAX {
                out.push((names[2], (reset / 1000).to_string()));
            }
        }
        out
    }

    pub(crate) fn next_available(&self, provider: Provider, allowed: &[String]) -> Option<i64> {
        let now = now_ms();
        let mut earliest: Option<i64> = None;
        for a in self.list.read().iter() {
            {
                let c = a.cfg.read();
                if c.kind.provider() != provider || !c.enabled || !permitted(allowed, &a.id) {
                    continue;
                }
            }
            let r = *a.rl.lock();
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
}
