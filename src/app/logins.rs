use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum LoginState {
    Pending,
    Done { account: Value },
    Error { error: String },
}

#[derive(Default)]
pub(crate) struct Logins {
    map: Mutex<HashMap<String, LoginState>>,
}

impl Logins {
    pub(crate) fn set(&self, id: String, state: LoginState) {
        self.map.lock().insert(id, state);
    }

    pub(crate) fn get(&self, id: &str) -> Option<LoginState> {
        self.map.lock().get(id).cloned()
    }
}
