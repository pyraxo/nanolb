use crate::app::App;
use crate::id::sha256_hex;
use axum::http::HeaderMap;
use serde_json::Value;

const SESSION_HEADERS: [&str; 4] = ["session_id", "session-id", "thread-id", "conversation_id"];

pub(super) fn session_key(h: &HeaderMap, user_id: Option<&str>, up: &Value) -> String {
    SESSION_HEADERS
        .iter()
        .find_map(|n| h.get(*n).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()))
        .or_else(|| up["prompt_cache_key"].as_str())
        .or(user_id)
        .map_or_else(|| content_hash(up), str::to_owned)
}

fn content_hash(up: &Value) -> String {
    let mut seed = up["instructions"].to_string();
    if let Some(items) = up["input"].as_array() {
        for it in items.iter().take(2) {
            seed.push_str(&it.to_string());
        }
    }
    let mut hash = sha256_hex(&seed);
    hash.truncate(24);
    hash
}

pub(super) fn file_owner(app: &App, up: &Value) -> Option<String> {
    up["input"]
        .as_array()?
        .iter()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .find_map(|part| part["file_id"].as_str())
        .and_then(|id| app.files.owner(id))
}
