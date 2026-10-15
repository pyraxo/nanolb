use super::auth::{account_from_tokens, exchange_code};
use super::{CLIENT_ID, ISSUER};
use crate::app::{Account, App, LoginState};
use crate::id::random_hex;
use crate::time::{now_ms, ts_iso};
use crate::upstream::UpstreamError;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const LOGIN_TTL_MS: i64 = 15 * 60_000;

struct DeviceCode {
    device_id: String,
    user_code: String,
    interval: u64,
    expires: i64,
}

pub(crate) async fn device_login_start(app: Arc<App>, label: Option<String>) -> Result<Value, UpstreamError> {
    let res = app
        .http
        .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
        .json(&json!({"client_id": CLIENT_ID}))
        .send()
        .await?;
    if res.status() == StatusCode::NOT_FOUND {
        return Err(UpstreamError::DeviceLoginDisabled);
    }
    let v: Value = res.json().await?;
    let device = DeviceCode {
        device_id: str_field(&v, "device_auth_id")?.to_owned(),
        user_code: v["user_code"]
            .as_str()
            .or_else(|| v["usercode"].as_str())
            .ok_or_else(|| UpstreamError::Decode("no user_code".into()))?
            .to_owned(),
        interval: v["interval"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| v["interval"].as_u64())
            .unwrap_or(5),
        expires: now_ms() + LOGIN_TTL_MS,
    };
    let login_id = random_hex(8);
    app.logins.set(login_id.clone(), LoginState::Pending);
    let reply = json!({
        "login_id": login_id, "verification_url": format!("{ISSUER}/codex/device"),
        "user_code": device.user_code, "expires_at": ts_iso(device.expires),
    });
    tokio::spawn(async move {
        let state = match poll_device(&app, &device, label.as_deref()).await {
            Ok(a) => {
                tracing::info!(account = %a.id, "device login completed");
                LoginState::Done { account: a.view() }
            }
            Err(e) => {
                tracing::warn!(error = %e, "device login failed");
                LoginState::Error { error: e.to_string() }
            }
        };
        app.logins.set(login_id, state);
    });
    Ok(reply)
}

fn str_field<'a>(v: &'a Value, key: &str) -> Result<&'a str, UpstreamError> {
    v[key].as_str().ok_or_else(|| UpstreamError::Decode(format!("no {key}")))
}

async fn poll_device(app: &App, device: &DeviceCode, label: Option<&str>) -> Result<Arc<Account>, UpstreamError> {
    while now_ms() < device.expires {
        tokio::time::sleep(Duration::from_secs(device.interval.max(2))).await;
        let Ok(res) = app
            .http
            .post(format!("{ISSUER}/api/accounts/deviceauth/token"))
            .json(&json!({"device_auth_id": device.device_id, "user_code": device.user_code}))
            .send()
            .await
        else {
            continue;
        };
        if matches!(res.status(), StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
            continue;
        }
        if !res.status().is_success() {
            return Err(UpstreamError::DevicePoll(res.status()));
        }
        let v: Value = res.json().await?;
        let code = str_field(&v, "authorization_code")?;
        let verifier = str_field(&v, "code_verifier")?;
        let t = exchange_code(&app.http, code, verifier, &format!("{ISSUER}/deviceauth/callback")).await?;
        let s = |k: &str| t[k].as_str().unwrap_or_default();
        let cfg = account_from_tokens(s("id_token"), s("access_token"), s("refresh_token"), label)?;
        return Ok(app.accounts.add(&app.store, cfg)?);
    }
    Err(UpstreamError::DeviceExpired)
}
