use super::AdminError;
use super::args::{invalid, str_arg};
use crate::app::{App, Key, KeyCfg, Limit};
use crate::time::{now_ms, parse_time};
use serde_json::{Value, json};
use std::sync::Arc;

fn apply_key_fields(app: &App, cfg: &mut KeyCfg, a: &Value) -> Result<(), AdminError> {
    if let Some(n) = a["name"].as_str() {
        cfg.name = n.into();
    }
    if !a["accounts"].is_null() {
        let names: Vec<String> = serde_json::from_value(a["accounts"].clone()).map_err(invalid("accounts"))?;
        cfg.accounts = names
            .iter()
            .map(|n| app.accounts.resolve(n).map(|acc| acc.id.clone()))
            .collect::<Result<_, _>>()?;
    }
    if !a["models"].is_null() {
        cfg.models = serde_json::from_value(a["models"].clone()).map_err(invalid("models"))?;
    }
    if !a["limits"].is_null() {
        cfg.limits = serde_json::from_value::<Vec<Limit>>(a["limits"].clone()).map_err(invalid("limits"))?;
    }
    if let Some(e) = a["enabled"].as_bool() {
        cfg.enabled = e;
    }
    if a.get("expires_at").is_some() {
        cfg.expires_at = parse_time(&a["expires_at"])?;
    }
    Ok(())
}

fn find_key(app: &App, a: &Value) -> Result<Arc<Key>, AdminError> {
    let id = str_arg(a, "id")?;
    app.keys.find(id).ok_or_else(|| AdminError::NotFound(format!("key {id} not found")))
}

pub(super) fn keys_list(app: &App) -> Value {
    json!(app.keys.list().iter().map(|k| k.view()).collect::<Vec<_>>())
}

pub(super) fn key_create(app: &App, a: &Value) -> Result<Value, AdminError> {
    let secret = format!("nlb-{}", crate::id::random_hex(24));
    let mut cfg = KeyCfg {
        id: crate::id::random_hex(6),
        name: str_arg(a, "name")?.into(),
        hash: crate::id::sha256_hex(&secret),
        prefix: secret[..12].into(),
        accounts: vec![],
        models: vec![],
        limits: vec![],
        enabled: true,
        expires_at: None,
        created_at: now_ms(),
    };
    apply_key_fields(app, &mut cfg, a)?;
    let mut v = app.keys.save(&app.store, cfg)?.view();
    v["key"] = Value::String(secret);
    Ok(v)
}

pub(super) fn key_update(app: &App, a: &Value) -> Result<Value, AdminError> {
    let mut cfg = find_key(app, a)?.cfg.clone();
    apply_key_fields(app, &mut cfg, a)?;
    Ok(app.keys.save(&app.store, cfg)?.view())
}

pub(super) fn key_remove(app: &App, a: &Value) -> Result<Value, AdminError> {
    let k = find_key(app, a)?;
    Ok(json!({"removed": app.keys.remove(&app.store, &k.cfg.id)?}))
}
