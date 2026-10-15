use super::AdminError;
use super::args::{account_arg, invalid, str_arg};
use crate::app::{App, Warmup};
use crate::time::{now_ms, ts_iso};
use crate::upstream::{codex, openrouter};
use serde_json::{Value, json};
use std::sync::Arc;

fn views(app: &App) -> Vec<Value> {
    app.accounts.read().iter().map(|a| a.view()).collect()
}

pub(super) fn status(app: &App) -> Result<Value, AdminError> {
    let s = app.settings();
    let day = app.store.usage_summary(now_ms() - 86_400_000, "all")?;
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"), "started_at": ts_iso(app.started_at),
        "uptime_secs": (now_ms() - app.started_at) / 1000, "strategy": s.strategy, "sticky": s.sticky,
        "accounts": views(app),
        "keys": app.keys.len(),
        "requests_24h": day[0]["requests"], "cost_24h": day[0]["cost_usd"],
        "tokens_24h": {"input": day[0]["input_tokens"], "cached": day[0]["cached_tokens"], "output": day[0]["output_tokens"]},
    }))
}

pub(super) fn accounts_list(app: &App) -> Value {
    json!(views(app))
}

pub(super) async fn account_import(app: &Arc<App>, a: &Value) -> Result<Value, AdminError> {
    let cfg = codex::account_from_auth_json(&a["auth_json"], a["label"].as_str())?;
    let acc = app.accounts.add(&app.store, cfg)?;
    let _ = codex::fetch_usage(app, &acc).await;
    Ok(acc.view())
}

pub(super) async fn account_add_tokens(app: &Arc<App>, a: &Value) -> Result<Value, AdminError> {
    let cfg = codex::account_from_tokens(
        str_arg(a, "id_token")?,
        str_arg(a, "access_token")?,
        str_arg(a, "refresh_token")?,
        a["label"].as_str(),
    )?;
    let acc = app.accounts.add(&app.store, cfg)?;
    let _ = codex::fetch_usage(app, &acc).await;
    Ok(acc.view())
}

pub(super) async fn account_add_openrouter(app: &Arc<App>, a: &Value) -> Result<Value, AdminError> {
    let key = str_arg(a, "api_key")?;
    let info = openrouter::verify_key(app, key).await?;
    let cfg = openrouter::account_cfg(key, a["label"].as_str(), &info);
    Ok(app.accounts.add(&app.store, cfg)?.view())
}

pub(super) async fn login_start(app: &Arc<App>, a: &Value) -> Result<Value, AdminError> {
    Ok(codex::device_login_start(app.clone(), a["label"].as_str().map(str::to_owned)).await?)
}

pub(super) fn login_poll(app: &App, a: &Value) -> Result<Value, AdminError> {
    let id = str_arg(a, "login_id")?;
    let state = app
        .logins
        .get(id)
        .ok_or_else(|| AdminError::NotFound(format!("login {id} not found")))?;
    Ok(serde_json::to_value(state)?)
}

fn parse_warmup(w: &Value) -> Result<Option<Warmup>, AdminError> {
    if w.is_null() {
        return Ok(None);
    }
    let mut w = w.clone();
    if let Some(o) = w.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    let w: Warmup = serde_json::from_value(w).map_err(invalid("warmup"))?;
    w.cron
        .parse::<croner::Cron>()
        .map_err(|e| AdminError::Invalid(format!("warmup.cron: {e}")))?;
    Ok(Some(Warmup { last_fired: now_ms(), ..w }))
}

pub(super) fn account_update(app: &App, a: &Value) -> Result<Value, AdminError> {
    let acc = account_arg(app, a)?;
    let warmup = a.get("warmup").map(parse_warmup).transpose()?;
    {
        let mut c = acc.cfg.write();
        if let Some(e) = a["enabled"].as_bool() {
            c.enabled = e;
        }
        if let Some(p) = a["priority"].as_i64() {
            c.priority = p;
        }
        if let Some(l) = a["label"].as_str() {
            c.label = l.into();
        }
        if let Some(w) = warmup {
            c.warmup = w;
        }
    }
    if a["enabled"].as_bool() == Some(true) {
        acc.clear_cooldown();
    }
    app.accounts.save(&app.store, &acc)?;
    Ok(acc.view())
}

pub(super) fn account_remove(app: &App, a: &Value) -> Result<Value, AdminError> {
    let id = account_arg(app, a)?.id.clone();
    Ok(json!({"removed": app.accounts.remove(&app.store, &id)?}))
}

pub(super) async fn account_refresh(app: &Arc<App>, a: &Value) -> Result<Value, AdminError> {
    let acc = account_arg(app, a)?;
    codex::ensure_fresh(app, &acc, true).await?;
    codex::fetch_usage(app, &acc).await?;
    acc.clear_cooldown();
    Ok(acc.view())
}

pub(super) async fn account_warmup(app: &Arc<App>, a: &Value) -> Result<Value, AdminError> {
    Ok(codex::warmup(app, &account_arg(app, a)?).await)
}
