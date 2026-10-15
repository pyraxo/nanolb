use super::{CLIENT_ID, ISSUER};
use crate::app::{Account, AccountCfg, App, Provider};
use crate::id::sha256_hex;
use crate::time::{now_ms, ts_iso};
use crate::upstream::UpstreamError;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const REFRESH_WINDOW_MS: i64 = 5 * 60_000;
const REFRESH_MAX_AGE_MS: i64 = 8 * 86_400_000;
const PERMANENT_COOLDOWN_MS: i64 = 3_600_000;
const TRANSIENT_COOLDOWN_MS: i64 = 30_000;

pub(super) struct Claims {
    pub(super) email: String,
    pub(super) account_id: String,
    pub(super) plan: String,
    pub(super) exp: i64,
}

pub(super) fn claims(jwt: &str) -> Claims {
    let payload = jwt
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_default();
    let auth = &payload["https://api.openai.com/auth"];
    let s = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    Claims {
        email: payload["email"]
            .as_str()
            .map_or_else(|| s(&payload["https://api.openai.com/profile"]["email"]), str::to_owned),
        account_id: s(&auth["chatgpt_account_id"]),
        plan: s(&auth["chatgpt_plan_type"]),
        exp: payload["exp"].as_i64().unwrap_or(0) * 1000,
    }
}

pub(crate) fn account_from_tokens(id_token: &str, access: &str, refresh: &str, label: Option<&str>) -> Result<AccountCfg, UpstreamError> {
    let c = claims(id_token);
    if c.account_id.is_empty() {
        return Err(UpstreamError::Decode("id_token has no chatgpt_account_id claim".into()));
    }
    let now = now_ms();
    Ok(AccountCfg {
        id: sha256_hex(&format!("{}:{}", c.account_id, c.email))[..12].to_owned(),
        provider: Provider::Codex,
        label: label.map_or_else(|| c.email.clone(), str::to_owned),
        email: c.email,
        plan: c.plan,
        chatgpt_account_id: c.account_id,
        access_token: access.into(),
        refresh_token: refresh.into(),
        id_token: id_token.into(),
        expires_at: claims(access).exp,
        last_refresh: now,
        enabled: true,
        priority: 0,
        warmup: None,
        created_at: now,
    })
}

pub(crate) fn account_from_auth_json(v: &Value, label: Option<&str>) -> Result<AccountCfg, UpstreamError> {
    let parsed;
    let v = match v {
        Value::String(s) => {
            parsed = serde_json::from_str::<Value>(s).map_err(|e| UpstreamError::Decode(format!("auth_json: {e}")))?;
            &parsed
        }
        v => v,
    };
    let t = &v["tokens"];
    let get = |k: &str| {
        t[k].as_str()
            .ok_or_else(|| UpstreamError::Decode(format!("auth_json.tokens.{k} missing")))
    };
    account_from_tokens(get("id_token")?, get("access_token")?, get("refresh_token")?, label)
}

pub(super) async fn exchange_code(http: &reqwest::Client, code: &str, verifier: &str, redirect_uri: &str) -> Result<Value, UpstreamError> {
    let res = http
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ])
        .send()
        .await?;
    let status = res.status();
    let body: Value = res.json().await?;
    if !status.is_success() {
        return Err(UpstreamError::Exchange { status, body });
    }
    Ok(body)
}

pub(crate) async fn ensure_fresh(app: &Arc<App>, a: &Arc<Account>, force: bool) -> Result<(), UpstreamError> {
    if a.provider() == Provider::OpenRouter {
        return Ok(());
    }
    let (app, a) = (app.clone(), a.clone());
    tokio::spawn(async move { refresh(&app, &a, force).await }).await?
}

fn stale(a: &Account, force: bool) -> bool {
    let c = a.cfg.read();
    let now = now_ms();
    force || c.expires_at - now < REFRESH_WINDOW_MS || now - c.last_refresh > REFRESH_MAX_AGE_MS
}

async fn refresh(app: &App, a: &Account, force: bool) -> Result<(), UpstreamError> {
    if !stale(a, force) {
        return Ok(());
    }
    let before = a.cfg.read().last_refresh;
    let _guard = a.refresh_lock.lock().await;
    if a.cfg.read().last_refresh != before {
        return Ok(());
    }
    let refresh_token = a.cfg.read().refresh_token.clone();
    let res = app
        .http
        .post(format!("{ISSUER}/oauth/token"))
        .timeout(Duration::from_secs(15))
        .json(&json!({"grant_type": "refresh_token", "client_id": CLIENT_ID, "refresh_token": refresh_token, "scope": "openid profile email"}))
        .send()
        .await
        .map_err(UpstreamError::RefreshSend)?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or_default();
    if !status.is_success() || body["access_token"].as_str().is_none() {
        return Err(refresh_failed(a, status, &body));
    }
    apply_tokens(a, &body);
    app.accounts.save(&app.store, a)?;
    tracing::info!(account = %a.id, expires_at = %ts_iso(a.cfg.read().expires_at), "token refreshed");
    Ok(())
}

fn refresh_failed(a: &Account, status: StatusCode, body: &Value) -> UpstreamError {
    let err = UpstreamError::Refresh {
        status,
        body: body["error"].clone(),
    };
    let permanent = status == StatusCode::UNAUTHORIZED || status == StatusCode::BAD_REQUEST;
    tracing::error!(account = %a.id, %status, permanent, "token refresh failed");
    let cooldown = if permanent { PERMANENT_COOLDOWN_MS } else { TRANSIENT_COOLDOWN_MS };
    a.fail(now_ms() + cooldown, err.to_string());
    err
}

fn apply_tokens(a: &Account, body: &Value) {
    let mut c = a.cfg.write();
    if let Some(t) = body["access_token"].as_str() {
        c.access_token = t.into();
        c.expires_at = claims(t).exp;
    }
    if let Some(t) = body["refresh_token"].as_str() {
        c.refresh_token = t.into();
    }
    if let Some(t) = body["id_token"].as_str() {
        c.id_token = t.into();
        let cl = claims(t);
        if !cl.plan.is_empty() {
            c.plan = cl.plan;
        }
    }
    c.last_refresh = now_ms();
}
