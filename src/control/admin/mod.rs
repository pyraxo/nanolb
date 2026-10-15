use crate::app::{Account, App, KeyCfg, Limit, LookupError, Provider, Settings, Warmup};
use crate::control::ops;
use crate::server::auth::{admin_denied, is_admin};
use crate::store::StoreError;
use crate::time::{TimeError, now_ms, parse_since, parse_time, ts_iso};
use crate::upstream::{UpstreamError, codex, openrouter};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub(crate) enum AdminError {
    #[error("unknown op `{0}`")]
    UnknownOp(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Lookup(#[from] LookupError),
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Time(#[from] TimeError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl AdminError {
    pub(crate) const fn status(&self) -> StatusCode {
        match self {
            Self::UnknownOp(_) | Self::NotFound(_) | Self::Lookup(LookupError::NotFound(_)) => StatusCode::NOT_FOUND,
            Self::Store(StoreError::InvalidQuery(_))
            | Self::Invalid(_)
            | Self::Lookup(LookupError::Ambiguous { .. })
            | Self::Upstream(_)
            | Self::Time(_) => StatusCode::BAD_REQUEST,
            Self::Store(_) | Self::Json(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (self.status(), axum::Json(json!({"error": {"message": self.to_string()}}))).into_response()
    }
}

pub(crate) async fn http(
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
            Err(e) => return AdminError::Invalid(format!("invalid json: {e}")).into_response(),
        }
    };
    tracing::info!(%op, "admin op");
    match dispatch(&app, &op, args).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(crate) fn str_arg<'a>(a: &'a Value, k: &str) -> Result<&'a str, AdminError> {
    a[k].as_str()
        .ok_or_else(|| AdminError::Invalid(format!("missing string arg `{k}`")))
}

pub(crate) fn account_arg(app: &App, a: &Value) -> Result<Arc<Account>, AdminError> {
    Ok(app.accounts.resolve(str_arg(a, "id")?)?)
}

fn invalid(what: &str) -> impl FnOnce(serde_json::Error) -> AdminError + '_ {
    move |e| AdminError::Invalid(format!("{what}: {e}"))
}

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

fn status(app: &App) -> Result<Value, AdminError> {
    let s = app.settings();
    let day = app.store.usage_summary(now_ms() - 86_400_000, "all")?;
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"), "started_at": ts_iso(app.started_at),
        "uptime_secs": (now_ms() - app.started_at) / 1000, "strategy": s.strategy, "sticky": s.sticky,
        "accounts": app.accounts.read().iter().map(|a| a.view()).collect::<Vec<_>>(),
        "keys": app.keys.len(),
        "requests_24h": day[0]["requests"], "cost_24h": day[0]["cost_usd"],
        "tokens_24h": {"input": day[0]["input_tokens"], "cached": day[0]["cached_tokens"], "output": day[0]["output_tokens"]},
    }))
}

fn account_update(app: &App, a: &Value) -> Result<Value, AdminError> {
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

fn key_create(app: &App, a: &Value) -> Result<Value, AdminError> {
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

fn find_key(app: &App, a: &Value) -> Result<Arc<crate::app::Key>, AdminError> {
    let id = str_arg(a, "id")?;
    app.keys.find(id).ok_or_else(|| AdminError::NotFound(format!("key {id} not found")))
}

fn requests(app: &App, a: &Value) -> Result<Value, AdminError> {
    let limit = u32::try_from(a["limit"].as_u64().unwrap_or(50).min(5000)).unwrap_or(5000);
    let rows = app
        .store
        .recent(limit)?
        .into_iter()
        .map(|u| {
            let mut v = serde_json::to_value(&u)?;
            v["ts"] = Value::String(ts_iso(u.ts));
            Ok(v)
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?;
    Ok(json!(rows))
}

fn settings_update(app: &App, a: Value) -> Result<Value, AdminError> {
    let mut cur = serde_json::to_value(&*app.settings())?;
    let Value::Object(upd) = a else {
        return Err(AdminError::Invalid("settings_update takes an object".into()));
    };
    for (k, v) in upd {
        if cur.get(&k).is_none() {
            return Err(AdminError::Invalid(format!("unknown setting `{k}`")));
        }
        cur[&k] = v;
    }
    let s: Settings = serde_json::from_value(cur).map_err(|e| AdminError::Invalid(e.to_string()))?;
    app.save_settings(s)?;
    Ok(serde_json::to_value(&*app.settings())?)
}

pub(crate) async fn dispatch(app: &Arc<App>, op: &str, a: Value) -> Result<Value, AdminError> {
    Ok(match op {
        "status" => status(app)?,
        "accounts_list" => json!(app.accounts.read().iter().map(|a| a.view()).collect::<Vec<_>>()),
        "account_import" => {
            let cfg = codex::account_from_auth_json(&a["auth_json"], a["label"].as_str())?;
            let acc = app.accounts.add(&app.store, cfg)?;
            let _ = codex::fetch_usage(app, &acc).await;
            acc.view()
        }
        "account_add_tokens" => {
            let cfg = codex::account_from_tokens(
                str_arg(&a, "id_token")?,
                str_arg(&a, "access_token")?,
                str_arg(&a, "refresh_token")?,
                a["label"].as_str(),
            )?;
            let acc = app.accounts.add(&app.store, cfg)?;
            let _ = codex::fetch_usage(app, &acc).await;
            acc.view()
        }
        "account_add_openrouter" => {
            let key = str_arg(&a, "api_key")?;
            let info = openrouter::verify_key(app, key).await?;
            let cfg = openrouter::account_cfg(key, a["label"].as_str(), &info);
            app.accounts.add(&app.store, cfg)?.view()
        }
        "login_start" => codex::device_login_start(app.clone(), a["label"].as_str().map(str::to_owned)).await?,
        "login_poll" => {
            let id = str_arg(&a, "login_id")?;
            let state = app
                .logins
                .get(id)
                .ok_or_else(|| AdminError::NotFound(format!("login {id} not found")))?;
            serde_json::to_value(state)?
        }
        "account_update" => account_update(app, &a)?,
        "account_remove" => {
            let id = account_arg(app, &a)?.id.clone();
            json!({"removed": app.accounts.remove(&app.store, &id)?})
        }
        "account_refresh" => {
            let acc = account_arg(app, &a)?;
            codex::ensure_fresh(app, &acc, true).await?;
            codex::fetch_usage(app, &acc).await?;
            acc.clear_cooldown();
            acc.view()
        }
        "account_warmup" => codex::warmup(app, &account_arg(app, &a)?).await,
        "keys_list" => json!(app.keys.list().iter().map(|k| k.view()).collect::<Vec<_>>()),
        "key_create" => key_create(app, &a)?,
        "key_update" => {
            let mut cfg = find_key(app, &a)?.cfg.clone();
            apply_key_fields(app, &mut cfg, &a)?;
            app.keys.save(&app.store, cfg)?.view()
        }
        "key_remove" => {
            let k = find_key(app, &a)?;
            json!({"removed": app.keys.remove(&app.store, &k.cfg.id)?})
        }
        "usage" => app
            .store
            .usage_summary(parse_since(&a["since"])?, a["group"].as_str().unwrap_or("all"))?,
        "requests" => requests(app, &a)?,
        "settings_get" => serde_json::to_value(&*app.settings())?,
        "settings_update" => settings_update(app, a)?,
        "models" => json!(
            app.model_list()
                .into_iter()
                .map(|(m, ctx)| json!({"provider": Provider::for_model(&m), "id": m, "context_window": ctx}))
                .collect::<Vec<_>>()
        ),
        "sessions_list" => ops::sessions_list(app, &a),
        "session_delete" => ops::session_delete(app, &a)?,
        "sessions_purge" => ops::sessions_purge(app, &a)?,
        "reset_credits_list" => ops::reset_credits_list(app, &a).await?,
        "reset_credits_consume" => ops::reset_credits_consume(app, &a).await?,
        "plan" => ops::plan(app, &a)?,
        "analytics" => ops::analytics(app, &a)?,
        _ => return Err(AdminError::UnknownOp(op.to_owned())),
    })
}
