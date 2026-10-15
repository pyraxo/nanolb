use crate::codex;
use crate::state::{App, KeyCfg, Limit, Settings, Warmup};
use crate::{now_ms, ts_iso};
use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

fn authorized(app: &App, h: &HeaderMap) -> bool {
    let token = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    token.is_some_and(|t| !app.admin_token.is_empty() && crate::sha256_hex(t) == crate::sha256_hex(&app.admin_token))
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"error": {"message": msg}}))).into_response()
}

pub async fn guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if !authorized(&app, req.headers()) {
        return err(StatusCode::UNAUTHORIZED, "invalid admin token");
    }
    next.run(req).await
}

pub async fn http(
    State(app): State<Arc<App>>,
    Path(op): Path<String>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if !authorized(&app, &h) {
        return err(StatusCode::UNAUTHORIZED, "invalid admin token");
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

fn s<'a>(a: &'a Value, k: &str) -> Result<&'a str, String> {
    a[k].as_str().ok_or_else(|| format!("missing string arg `{k}`"))
}

fn parse_time(v: &Value) -> Result<Option<i64>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(t) => chrono::DateTime::parse_from_rfc3339(t)
            .map(|d| Some(d.timestamp_millis()))
            .map_err(|e| format!("bad timestamp {t}: {e}")),
        Value::Number(n) => Ok(n.as_i64()),
        _ => Err("timestamp must be RFC3339".into()),
    }
}

fn parse_since(v: &Value) -> Result<i64, String> {
    let t = v.as_str().unwrap_or("24h");
    let num = |suffix: char, ms: i64| {
        t.strip_suffix(suffix)
            .and_then(|n| n.parse::<i64>().ok())
            .map(|n| now_ms() - n * ms)
    };
    num('m', 60_000)
        .or_else(|| num('h', 3_600_000))
        .or_else(|| num('d', 86_400_000))
        .map(Ok)
        .unwrap_or_else(|| parse_time(v).map(|t| t.unwrap_or(0)))
}

fn account_or_err(app: &App, a: &Value) -> Result<Arc<crate::state::Account>, String> {
    let id = s(a, "id")?;
    app.account(id).ok_or_else(|| format!("account {id} not found"))
}

fn apply_key_fields(cfg: &mut KeyCfg, a: &Value) -> Result<(), String> {
    if let Some(n) = a["name"].as_str() {
        cfg.name = n.into();
    }
    if !a["accounts"].is_null() {
        cfg.accounts = serde_json::from_value(a["accounts"].clone()).map_err(|e| format!("accounts: {e}"))?;
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
            let day = app.store.usage_summary(now_ms() - 86_400_000, "all");
            json!({
                "version": env!("CARGO_PKG_VERSION"), "started_at": ts_iso(app.started_at),
                "uptime_secs": (now_ms() - app.started_at) / 1000, "strategy": s.strategy, "sticky": s.sticky,
                "accounts": app.accounts.read().iter().map(|a| a.view()).collect::<Vec<_>>(),
                "keys": app.keys.read().len(),
                "requests_24h": day[0]["requests"], "cost_24h": day[0]["cost_usd"],
                "tokens_24h": {"input": day[0]["input_tokens"], "cached": day[0]["cached_tokens"], "output": day[0]["output_tokens"]},
            })
        }
        "accounts_list" => json!(app.accounts.read().iter().map(|a| a.view()).collect::<Vec<_>>()),
        "account_import" => {
            let cfg = codex::account_from_auth_json(&a["auth_json"], a["label"].as_str())?;
            let acc = codex::add_account(app, cfg);
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
            let acc = codex::add_account(app, cfg);
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
            let cfg = crate::state::AccountCfg {
                id: crate::sha256_hex(key)[..12].to_string(),
                kind: "openrouter".into(),
                label,
                plan: "openrouter".into(),
                access_token: key.into(),
                expires_at: i64::MAX / 2,
                last_refresh: now_ms(),
                enabled: true,
                created_at: now_ms(),
                ..Default::default()
            };
            codex::add_account(app, cfg).view()
        }
        "claude_login_start" => crate::claude::login_start(app, a["label"].as_str().map(str::to_string)),
        "claude_login_finish" => crate::claude::login_finish(app, s(&a, "login_id")?, s(&a, "code")?).await?.view(),
        "account_add_claude_token" => {
            let cfg = crate::claude::account_from_token(s(&a, "token")?.trim(), a["label"].as_str())?;
            codex::add_account(app, cfg).view()
        }
        "login_start" => codex::device_login_start(app.clone(), a["label"].as_str().map(str::to_string)).await?,
        "login_poll" => {
            let id = s(&a, "login_id")?;
            let mut v = app.logins.lock().get(id).cloned().ok_or_else(|| format!("login {id} not found"))?;
            if let Some(o) = v.as_object_mut() {
                o.remove("verifier");
                o.remove("state");
            }
            v
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
                acc.cooldown_until.store(0, Relaxed);
            }
            app.save_account(&acc);
            acc.view()
        }
        "account_remove" => json!({"removed": app.remove_account(&account_or_err(app, &a)?.id)}),
        "account_refresh" => {
            let acc = account_or_err(app, &a)?;
            codex::ensure_fresh(app, &acc, true).await?;
            codex::fetch_usage(app, &acc).await?;
            acc.cooldown_until.store(0, Relaxed);
            acc.view()
        }
        "account_warmup" => codex::warmup(app, &account_or_err(app, &a)?).await,
        "keys_list" => json!(app.keys.read().values().map(|k| k.view()).collect::<Vec<_>>()),
        "key_create" => {
            let secret = format!("nlb-{}", crate::random_hex(24));
            let mut cfg = KeyCfg {
                id: crate::random_hex(6),
                name: s(&a, "name")?.into(),
                hash: crate::sha256_hex(&secret),
                prefix: secret[..12].into(),
                accounts: vec![],
                models: vec![],
                limits: vec![],
                enabled: true,
                expires_at: None,
                created_at: now_ms(),
            };
            apply_key_fields(&mut cfg, &a)?;
            let mut v = app.save_key(cfg).view();
            v["key"] = Value::String(secret);
            v
        }
        "key_update" => {
            let id = s(&a, "id")?;
            let k = app.find_key(id).ok_or_else(|| format!("key {id} not found"))?;
            let mut cfg = k.cfg.clone();
            apply_key_fields(&mut cfg, &a)?;
            app.save_key(cfg).view()
        }
        "key_remove" => {
            let id = s(&a, "id")?;
            let k = app.find_key(id).ok_or_else(|| format!("key {id} not found"))?;
            json!({"removed": app.remove_key(&k.cfg.id)})
        }
        "usage" => app
            .store
            .usage_summary(parse_since(&a["since"])?, a["group"].as_str().unwrap_or("all")),
        "requests" => {
            let rows = app.store.recent(a["limit"].as_u64().unwrap_or(50).min(5000) as u32);
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
            app.save_settings(s);
            serde_json::to_value(&*app.settings()).unwrap()
        }
        "models" => json!(
            app.model_list()
                .into_iter()
                .map(|(m, ctx)| json!({"provider": App::provider_for(&m), "id": m, "context_window": ctx}))
                .collect::<Vec<_>>()
        ),
        _ => return Err(format!("unknown op `{op}`")),
    })
}
