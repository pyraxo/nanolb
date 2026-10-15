use super::account::Provider;
use crate::store::{Store, StoreError};
use crate::time::now_ms;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const MAX_PINS: usize = 50_000;
const PIN_IDLE_MS: i64 = 3_600_000;
const PIN_TTL_MS: i64 = 7 * 86_400_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Pin {
    #[serde(rename = "a")]
    pub(crate) account: String,
    #[serde(rename = "t")]
    pub(crate) at: i64,
}

pub(crate) struct Sessions {
    map: Mutex<HashMap<String, Pin>>,
}

impl Sessions {
    pub(crate) fn key(key_id: &str, provider: Provider, cache: &str) -> String {
        format!("{key_id}\u{1f}{provider}\u{1f}{cache}")
    }

    pub(crate) fn load(store: &Store) -> Result<Self, StoreError> {
        store.prune("sticky", "t", now_ms() - PIN_TTL_MS)?;
        let map = store
            .load_raw("sticky")?
            .into_iter()
            .filter_map(|(k, v)| Some((k, serde_json::from_value(v).ok()?)))
            .collect();
        Ok(Self { map: Mutex::new(map) })
    }

    pub(crate) fn pinned(&self, session: &str) -> Option<String> {
        self.map.lock().get(session).map(|p| p.account.clone())
    }

    pub(crate) fn get(&self, session: &str) -> Option<Pin> {
        self.map.lock().get(session).cloned()
    }

    pub(crate) fn touch(&self, session: &str, now: i64) {
        if let Some(p) = self.map.lock().get_mut(session) {
            p.at = now;
        }
    }

    pub(crate) fn bind(&self, session: String, account: String, now: i64) {
        self.map.lock().insert(session, Pin { account, at: now });
    }

    pub(crate) fn pin(&self, store: &Store, session: &str, account: &str, now: i64) {
        let pin = Pin {
            account: account.to_owned(),
            at: now,
        };
        store.put_async("sticky", session, &pin);
        let mut map = self.map.lock();
        if map.len() > MAX_PINS {
            map.retain(|_, p| now - p.at < PIN_IDLE_MS);
        }
        map.insert(session.to_owned(), pin);
    }

    pub(crate) fn remove(&self, store: &Store, session: &str) -> bool {
        let removed = self.map.lock().remove(session).is_some();
        store.delete_async("sticky", session);
        removed
    }

    pub(crate) fn purge(&self, store: &Store, mut hit: impl FnMut(&Pin) -> bool) -> usize {
        let mut removed = Vec::new();
        self.map.lock().retain(|k, p| {
            let h = hit(p);
            if h {
                removed.push(k.clone());
            }
            !h
        });
        for k in &removed {
            store.delete_async("sticky", k);
        }
        removed.len()
    }

    pub(crate) fn list(&self) -> Vec<(String, Pin)> {
        self.map.lock().iter().map(|(k, p)| (k.clone(), p.clone())).collect()
    }
}
