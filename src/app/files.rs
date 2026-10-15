use super::sessions::Pin;
use crate::store::{Store, StoreError};
use crate::time::now_ms;
use parking_lot::Mutex;
use std::collections::HashMap;

pub(crate) struct FileOwners {
    map: Mutex<HashMap<String, String>>,
}

impl FileOwners {
    pub(crate) fn load(store: &Store) -> Result<Self, StoreError> {
        let map = store
            .load_raw("file")?
            .into_iter()
            .filter_map(|(k, v)| Some((k, v["a"].as_str()?.to_owned())))
            .collect();
        Ok(Self { map: Mutex::new(map) })
    }

    pub(crate) fn owner(&self, id: &str) -> Option<String> {
        self.map.lock().get(id).cloned()
    }

    pub(crate) fn bind(&self, store: &Store, id: &str, account: &str) {
        self.map.lock().insert(id.to_owned(), account.to_owned());
        store.put_async(
            "file",
            id,
            &Pin {
                account: account.to_owned(),
                at: now_ms(),
            },
        );
    }
}
