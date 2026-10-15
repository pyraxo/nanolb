use super::UpstreamError;
use crate::app::{Account, AccountCfg, App, Provider, Settings};
use crate::id::sha256_hex;
use crate::time::now_ms;
use serde_json::Value;

pub(crate) fn request(app: &App, s: &Settings, acc: &Account, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
    app.http
        .request(method, format!("{}{path}", s.openrouter_base))
        .bearer_auth(&acc.cfg.read().access_token)
        .header("http-referer", "https://github.com/pyraxo/nanolb")
        .header("x-title", "nanolb")
}

pub(crate) async fn verify_key(app: &App, key: &str) -> Result<Value, UpstreamError> {
    let res = app
        .http
        .get(format!("{}/key", app.settings().openrouter_base))
        .bearer_auth(key)
        .send()
        .await?;
    if !res.status().is_success() {
        return Err(UpstreamError::KeyRejected(res.status()));
    }
    Ok(res.json().await.unwrap_or_default())
}

pub(crate) fn account_cfg(key: &str, label: Option<&str>, info: &Value) -> AccountCfg {
    let label = label.or_else(|| info["data"]["label"].as_str()).unwrap_or("openrouter").to_owned();
    AccountCfg {
        id: sha256_hex(key)[..12].to_owned(),
        kind: Provider::OpenRouter.into(),
        label,
        plan: "openrouter".into(),
        access_token: key.into(),
        expires_at: i64::MAX / 2,
        last_refresh: now_ms(),
        enabled: true,
        created_at: now_ms(),
        ..Default::default()
    }
}
