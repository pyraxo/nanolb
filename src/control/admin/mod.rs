use crate::app::{App, KeyCfg, Limit, Provider, Settings, Warmup};
use crate::server::auth::{admin_denied, is_admin};
use crate::time::{now_ms, ts_iso};
use crate::upstream::codex;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

fn err(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"error": {"message": msg}}))).into_response()
}

pub async fn http(
    State(app): State<Arc<App>>,
    Path(op): Path<String>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if !is_admin(&app, &h) {
        return admin_denied();
    }
    let args = if body.is_empty() {
        Value::Object(
            q.into_iter()
                .map(|(k, v)| (k, serde_json::from_str(&v).unwrap_or(Value::String(v))))
                .collect(),
        )
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid json: {e}")),
        }
    };
    tracing::info!(%op, "admin op");
    match op_call(&app, &op, args).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) if e.starts_with("unknown op") => err(StatusCode::NOT_FOUND, &e),
        Err(e) if e.contains("not found") => err(StatusCode::NOT_FOUND, &e),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

pub(crate) fn s<'a>(a: &'a Value, k: &str) -> Result<&'a str, String> {
    a[k].as_str().ok_or_else(|| format!("missing string arg `{k}`"))
}

pub(crate) fn parse_since(v: &Value) -> Result<i64, String> {
    crate::time::parse_since(v).map_err(|e| e.to_string())
}

fn parse_time(v: &Value) -> Result<Option<i64>, String> {
    crate::time::parse_time(v).map_err(|e| e.to_string())
}

pub(crate) fn account_or_err(app: &App, a: &Value) -> Result<Arc<crate::app::Account>, String> {
    app.accounts.resolve(s(a, "id")?).map_err(|e| e.to_string())
}

fn apply_key_fields(app: &App, cfg: &mut KeyCfg, a: &Value) -> Result<(), String> {
    if let Some(n) = a["name"].as_str() {
        cfg.name = n.into();
    }
    if !a["accounts"].is_null() {
        let names: Vec<String> = serde_json::from_value(a["accounts"].clone()).map_err(|e| format!("accounts: {e}"))?;
        cfg.accounts = names
            .iter()
            .map(|n| app.accounts.resolve(n).map(|acc| acc.id.clone()).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
    }
    if !a["models"].is_null() {
        cfg.models = serde_json::from_value(a["models"].clone()).map_err(|e| format!("models: {e}"))?;
    }
    if !a["limits"].is_null() {
        cfg.limits = serde_json::from_value::<Vec<Limit>>(a["limits"].clone()).map_err(|e| format!("limits: {e}"))?;
    }
    if let Some(e) = a["enabled"].as_bool() {
        cfg.enabled = e;
    }
    if a.get("expires_at").is_some() {
        cfg.expires_at = parse_time(&a["expires_at"])?;
    }
    Ok(())
}

pub async fn op_call(app: &Arc<App>, op: &str, a: Value) -> Result<Value, String> {
    Ok(match op {
        "status" => {
            let s = app.settings();
            let day = app.store.usage_summary(now_ms() - 86_400_000, "all").map_err(|e| e.to_string())?;
            json!({
                "version": env!("CARGO_PKG_VERSION"), "started_at": ts_iso(app.started_at),
                "uptime_secs": (now_ms() - app.started_at) / 1000, "strategy": s.strategy, "sticky": s.sticky,
                "accounts": app.accounts.read().iter().map(|a| a.view()).collect::<Vec<_>>(),
                "keys": app.keys.len(),
                "requests_24h": day[0]["requests"], "cost_24h": day[0]["cost_usd"],
                "tokens_24h": {"input": day[0]["input_tokens"], "cached": day[0]["cached_tokens"], "output": day[0]["output_tokens"]},
            })
        }
        "accounts_list" => json!(app.accounts.read().iter().map(|a| a.view()).collect::<Vec<_>>()),
        "account_import" => {
            let cfg = codex::account_from_auth_json(&a["auth_json"], a["label"].as_str())?;
            let acc = app.accounts.add(&app.store, cfg).map_err(|e| e.to_string())?;
            let _ = codex::fetch_usage(app, &acc).await;
            acc.view()
        }
        "account_add_tokens" => {
            let cfg = codex::account_from_tokens(
                s(&a, "id_token")?,
                s(&a, "access_token")?,
                s(&a, "refresh_token")?,
                a["label"].as_str(),
            )?;
            let acc = app.accounts.add(&app.store, cfg).map_err(|e| e.to_string())?;
            let _ = codex::fetch_usage(app, &acc).await;
            acc.view()
        }
        "account_add_openrouter" => {
            let key = s(&a, "api_key")?;
            let res = app
                .http
                .get(format!("{}/key", app.settings().openrouter_base))
                .bearer_auth(key)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !res.status().is_success() {
                return Err(format!("openrouter rejected key: {}", res.status()));
            }
            let info: Value = res.json().await.unwrap_or_default();
            let label = a["label"]
                .as_str()
                .or(info["data"]["label"].as_str())
                .unwrap_or("openrouter")
                .to_string();
            let cfg = crate::app::AccountCfg {
                id: crate::id::sha256_hex(key)[..12].to_string(),
                provider: crate::app::Provider::OpenRouter,
                label,
                plan: "openrouter".into(),
                access_token: key.into(),
                expires_at: i64::MAX / 2,
                last_refresh: now_ms(),
                enabled: true,
                created_at: now_ms(),
                ..Default::default()
            };
            app.accounts.add(&app.store, cfg).map_err(|e| e.to_string())?.view()
        }
        "login_start" => codex::device_login_start(app.clone(), a["label"].as_str().map(str::to_string)).await?,
        "login_poll" => {
            let id = s(&a, "login_id")?;
            let state = app.logins.get(id).ok_or_else(|| format!("login {id} not found"))?;
            serde_json::to_value(state).map_err(|e| e.to_string())?
        }
        "account_update" => {
            let acc = account_or_err(app, &a)?;
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
                if let Some(w) = a.get("warmup") {
                    c.warmup = if w.is_null() {
                        None
                    } else {
                        let mut w = w.clone();
                        if let Some(o) = w.as_object_mut() {
                            o.retain(|_, v| !v.is_null());
                        }
                        let w: Warmup = serde_json::from_value(w).map_err(|e| format!("warmup: {e}"))?;
                        w.cron.parse::<croner::Cron>().map_err(|e| format!("warmup.cron: {e}"))?;
                        Some(Warmup { last_fired: now_ms(), ..w })
                    };
                }
            }
            if a["enabled"].as_bool() == Some(true) {
                acc.clear_cooldown();
            }
            app.accounts.save(&app.store, &acc).map_err(|e| e.to_string())?;
            acc.view()
        }
        "account_remove" => {
            let id = account_or_err(app, &a)?.id.clone();
            json!({"removed": app.accounts.remove(&app.store, &id).map_err(|e| e.to_string())?})
        }
        "account_refresh" => {
            let acc = account_or_err(app, &a)?;
            codex::ensure_fresh(app, &acc, true).await?;
            codex::fetch_usage(app, &acc).await?;
            acc.clear_cooldown();
            acc.view()
        }
        "account_warmup" => codex::warmup(app, &account_or_err(app, &a)?).await,
        "keys_list" => json!(app.keys.list().iter().map(|k| k.view()).collect::<Vec<_>>()),
        "key_create" => {
            let secret = format!("nlb-{}", crate::id::random_hex(24));
            let mut cfg = KeyCfg {
                id: crate::id::random_hex(6),
                name: s(&a, "name")?.into(),
                hash: crate::id::sha256_hex(&secret),
                prefix: secret[..12].into(),
                accounts: vec![],
                models: vec![],
                limits: vec![],
                enabled: true,
                expires_at: None,
                created_at: now_ms(),
            };
            apply_key_fields(app, &mut cfg, &a)?;
            let mut v = app.keys.save(&app.store, cfg).map_err(|e| e.to_string())?.view();
            v["key"] = Value::String(secret);
            v
        }
        "key_update" => {
            let id = s(&a, "id")?;
            let k = app.keys.find(id).ok_or_else(|| format!("key {id} not found"))?;
            let mut cfg = k.cfg.clone();
            apply_key_fields(app, &mut cfg, &a)?;
            app.keys.save(&app.store, cfg).map_err(|e| e.to_string())?.view()
        }
        "key_remove" => {
            let id = s(&a, "id")?;
            let k = app.keys.find(id).ok_or_else(|| format!("key {id} not found"))?;
            json!({"removed": app.keys.remove(&app.store, &k.cfg.id).map_err(|e| e.to_string())?})
        }
        "usage" => app
            .store
            .usage_summary(parse_since(&a["since"])?, a["group"].as_str().unwrap_or("all"))
            .map_err(|e| e.to_string())?,
        "requests" => {
            let rows = app
                .store
                .recent(a["limit"].as_u64().unwrap_or(50).min(5000) as u32)
                .map_err(|e| e.to_string())?;
            json!(
                rows.into_iter()
                    .map(|u| {
                        let mut v = serde_json::to_value(&u).unwrap();
                        v["ts"] = Value::String(ts_iso(u.ts));
                        v
                    })
                    .collect::<Vec<_>>()
            )
        }
        "settings_get" => serde_json::to_value(&*app.settings()).unwrap(),
        "settings_update" => {
            let mut cur = serde_json::to_value(&*app.settings()).unwrap();
            let Value::Object(upd) = a else {
                return Err("settings_update takes an object".into());
            };
            for (k, v) in upd {
                if cur.get(&k).is_none() {
                    return Err(format!("unknown setting `{k}`"));
                }
                cur[&k] = v;
            }
            let s: Settings = serde_json::from_value(cur).map_err(|e| e.to_string())?;
            app.save_settings(s).map_err(|e| e.to_string())?;
            serde_json::to_value(&*app.settings()).unwrap()
        }
        "models" => json!(
            app.model_list()
                .into_iter()
                .map(|(m, ctx)| json!({"provider": Provider::for_model(&m), "id": m, "context_window": ctx}))
                .collect::<Vec<_>>()
        ),
        "sessions_list" => crate::control::ops::sessions_list(app, &a),
        "session_delete" => crate::control::ops::session_delete(app, &a)?,
        "sessions_purge" => crate::control::ops::sessions_purge(app, &a)?,
        "reset_credits_list" => crate::control::ops::reset_credits_list(app, &a).await?,
        "reset_credits_consume" => crate::control::ops::reset_credits_consume(app, &a).await?,
        "plan" => crate::control::ops::plan(app, &a)?,
        "analytics" => crate::control::ops::analytics(app, &a)?,
        _ => return Err(format!("unknown op `{op}`")),
    })
}
