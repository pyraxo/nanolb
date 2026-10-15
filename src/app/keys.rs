use super::key::{Counter, Key, KeyCfg, Window};
use crate::id::sha256_hex;
use crate::store::{Store, StoreError};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) struct Keys {
    map: RwLock<HashMap<String, Arc<Key>>>,
}

impl Keys {
    pub(crate) fn load(store: &Store, now: i64) -> Result<Self, StoreError> {
        let mut totals: HashMap<Window, HashMap<String, Counter>> = HashMap::new();
        for w in Window::ALL {
            let start = w.start(now);
            let per_key = store
                .key_totals_since(start)?
                .into_iter()
                .map(|t| {
                    let counter = Counter {
                        start,
                        tokens: t.tokens,
                        cost: t.cost,
                        requests: t.requests,
                    };
                    (t.key_id, counter)
                })
                .collect();
            totals.insert(w, per_key);
        }
        let map = store
            .load::<KeyCfg>("key")?
            .into_iter()
            .map(|cfg| {
                let counters = totals
                    .iter()
                    .map(|(w, m)| {
                        let c = m.get(&cfg.id).copied().unwrap_or_else(|| Counter {
                            start: w.start(now),
                            ..Counter::default()
                        });
                        (*w, c)
                    })
                    .collect();
                (cfg.hash.clone(), Arc::new(Key::new(cfg, counters)))
            })
            .collect();
        Ok(Self { map: RwLock::new(map) })
    }

    pub(crate) fn len(&self) -> usize {
        self.map.read().len()
    }

    pub(crate) fn list(&self) -> Vec<Arc<Key>> {
        self.map.read().values().cloned().collect()
    }

    pub(crate) fn by_secret(&self, secret: &str) -> Option<Arc<Key>> {
        self.map.read().get(&sha256_hex(secret)).cloned()
    }

    pub(crate) fn find(&self, id_or_name: &str) -> Option<Arc<Key>> {
        self.map
            .read()
            .values()
            .find(|k| k.cfg.id == id_or_name || k.cfg.name == id_or_name)
            .cloned()
    }

    pub(crate) fn save(&self, store: &Store, cfg: KeyCfg) -> Result<Arc<Key>, StoreError> {
        store.put("key", &cfg.id, &cfg)?;
        let mut keys = self.map.write();
        let counters = keys.get(&cfg.hash).map(|k| k.counters()).unwrap_or_default();
        let k = Arc::new(Key::new(cfg, counters));
        keys.insert(k.cfg.hash.clone(), k.clone());
        drop(keys);
        Ok(k)
    }

    pub(crate) fn remove(&self, store: &Store, id: &str) -> Result<bool, StoreError> {
        store.delete("key", id)?;
        let mut keys = self.map.write();
        let n = keys.len();
        keys.retain(|_, k| k.cfg.id != id);
        Ok(n != keys.len())
    }
}
