use crate::app::{Account, AccountCfg, App};
use crate::store::Usage;
use crate::time::{now_ms, ts_iso};
use axum::http::HeaderMap;
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

pub const ISSUER: &str = "https://auth.openai.com";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_WINDOW_MS: i64 = 5 * 60_000;

const FORWARD_HEADERS: &[&str] = &[
    "session_id",
    "session-id",
    "thread-id",
    "conversation_id",
    "x-client-request-id",
    "x-openai-subagent",
    "openai-beta",
];

const STRIP_FIELDS: &[&str] = &[
    "max_output_tokens",
    "max_tokens",
    "metadata",
    "prompt_cache_retention",
    "safety_identifier",
    "temperature",
    "top_p",
    "truncation",
    "user",
    "prompt_cache_options",
    "previous_response_id",
    "stream_options",
];

pub struct Claims {
    pub email: String,
    pub account_id: String,
    pub plan: String,
    pub exp: i64,
}

pub fn claims(jwt: &str) -> Claims {
    let payload = jwt
        .split('.')
        .nth(1)
        .and_then(|p| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(p.trim_end_matches('='))
                .ok()
        })
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_default();
    let auth = &payload["https://api.openai.com/auth"];
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    Claims {
        email: payload["email"]
            .as_str()
            .map_or_else(|| s(&payload["https://api.openai.com/profile"]["email"]), str::to_string),
        account_id: s(&auth["chatgpt_account_id"]),
        plan: s(&auth["chatgpt_plan_type"]),
        exp: payload["exp"].as_i64().unwrap_or(0) * 1000,
    }
}

pub fn account_from_tokens(id_token: &str, access: &str, refresh: &str, label: Option<&str>) -> Result<AccountCfg, String> {
    let c = claims(id_token);
    if c.account_id.is_empty() {
        return Err("id_token has no chatgpt_account_id claim".into());
    }
    let exp = claims(access).exp;
    Ok(AccountCfg {
        id: crate::id::sha256_hex(&format!("{}:{}", c.account_id, c.email))[..12].to_string(),
        kind: "codex".into(),
        label: label.map_or_else(|| c.email.clone(), str::to_string),
        email: c.email,
        plan: c.plan,
        chatgpt_account_id: c.account_id,
        access_token: access.into(),
        refresh_token: refresh.into(),
        id_token: id_token.into(),
        expires_at: exp,
        last_refresh: now_ms(),
        enabled: true,
        priority: 0,
        warmup: None,
        created_at: now_ms(),
    })
}

pub fn account_from_auth_json(v: &Value, label: Option<&str>) -> Result<AccountCfg, String> {
    let v = match v {
        Value::String(s) => serde_json::from_str(s).map_err(|e| format!("auth_json: {e}"))?,
        v => v.clone(),
    };
    let t = &v["tokens"];
    let get = |k: &str| t[k].as_str().ok_or_else(|| format!("auth_json.tokens.{k} missing"));
    account_from_tokens(get("id_token")?, get("access_token")?, get("refresh_token")?, label)
}

pub fn add_account(app: &App, mut cfg: AccountCfg) -> Arc<Account> {
    if let Some(old) = app.account(&cfg.id) {
        let o = old.cfg.read();
        cfg.enabled = o.enabled;
        cfg.priority = o.priority;
        cfg.warmup = o.warmup.clone();
        cfg.created_at = o.created_at;
    }
    let a = app.upsert_account(cfg);
    a.cooldown_until.store(0, Relaxed);
    *a.last_error.lock() = None;
    a
}

pub async fn exchange_code(http: &reqwest::Client, code: &str, verifier: &str, redirect_uri: &str) -> Result<Value, String> {
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
        .await
        .map_err(|e| e.to_string())?;
    let status = res.status();
    let body: Value = res.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("token exchange failed ({status}): {body}"));
    }
    Ok(body)
}

pub async fn ensure_fresh(app: &Arc<App>, a: &Arc<Account>, force: bool) -> Result<(), String> {
    if a.cfg.read().kind == "openrouter" {
        return Ok(());
    }
    let (app, a) = (app.clone(), a.clone());
    tokio::spawn(async move { refresh_inner(&app, &a, force).await })
        .await
        .map_err(|e| e.to_string())?
}

async fn refresh_inner(app: &App, a: &Account, force: bool) -> Result<(), String> {
    let stale = |a: &Account| {
        let c = a.cfg.read();
        force || c.expires_at - now_ms() < REFRESH_WINDOW_MS || now_ms() - c.last_refresh > 8 * 86_400_000
    };
    if !stale(a) {
        return Ok(());
    }
    let before = a.cfg.read().last_refresh;
    let _g = a.refresh_lock.lock().await;
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
        .map_err(|e| format!("refresh: {e}"))?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or_default();
    if !status.is_success() || body["access_token"].as_str().is_none() {
        let msg = format!("token refresh failed ({status}): {}", body["error"]);
        let permanent = status == 401 || status == 400;
        tracing::error!(account = %a.id, %status, permanent, "token refresh failed");
        a.fail(now_ms() + if permanent { 3_600_000 } else { 30_000 }, msg.clone());
        return Err(msg);
    }
    {
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
    app.save_account(a);
    tracing::info!(account = %a.id, expires_at = %ts_iso(a.cfg.read().expires_at), "token refreshed");
    Ok(())
}

pub fn prepare_body(body: &mut Value) {
    if let Some(o) = body.as_object_mut() {
        for f in STRIP_FIELDS {
            o.remove(*f);
        }
        o.insert("store".into(), Value::Bool(false));
        o.insert("stream".into(), Value::Bool(true));
        if !o.contains_key("instructions")
            && !o
                .get("input")
                .and_then(Value::as_array)
                .is_some_and(|i| i.iter().any(|m| m["role"] == "developer" || m["role"] == "system"))
        {
            o.insert("instructions".into(), Value::String(String::new()));
        }
        if let Some(Value::String(s)) = o.get("input") {
            let s = s.clone();
            o.insert(
                "input".into(),
                json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": s}]}]),
            );
        }
    }
}

pub fn request(app: &App, a: &Account, method: reqwest::Method, path: &str, client: Option<&HeaderMap>) -> reqwest::RequestBuilder {
    let s = app.settings();
    let c = a.cfg.read();
    let mut rb = app
        .http
        .request(method, format!("{}{path}", s.upstream_base))
        .bearer_auth(&c.access_token)
        .header("chatgpt-account-id", &c.chatgpt_account_id)
        .header("accept", "text/event-stream")
        .header("originator", "codex_cli_rs")
        .header("version", &s.codex_version);
    let mut ua = None;
    if let Some(h) = client {
        for (k, v) in h {
            let n = k.as_str();
            if FORWARD_HEADERS.contains(&n) || n.starts_with("x-codex-") {
                rb = rb.header(k, v);
            }
        }
        ua = h
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .filter(|u| u.starts_with("codex_"))
            .map(str::to_string);
    }
    rb.header(
        "user-agent",
        ua.unwrap_or_else(|| format!("codex_cli_rs/{} (Mac OS 26.5.0; arm64) nanolb", s.codex_version)),
    )
}

pub fn apply_rate_headers(a: &Account, h: &reqwest::header::HeaderMap) {
    let f = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
    let Some(pu) = f("x-codex-primary-used-percent") else { return };
    let now = now_ms();
    let reset = |p: &str| {
        f(&format!("x-codex-{p}-reset-at"))
            .map(|s| (s * 1000.0) as i64)
            .or_else(|| f(&format!("x-codex-{p}-reset-after-seconds")).map(|s| now + (s * 1000.0) as i64))
            .unwrap_or(0)
    };
    let mut r = a.rl.lock();
    r.primary_used = pu;
    r.primary_window_min = f("x-codex-primary-window-minutes").unwrap_or(0.0) as i64;
    r.primary_reset_at = reset("primary");
    r.secondary_window_min = f("x-codex-secondary-window-minutes").unwrap_or(0.0) as i64;
    if r.secondary_window_min > 0 {
        r.secondary_used = f("x-codex-secondary-used-percent").unwrap_or(0.0);
        r.secondary_reset_at = reset("secondary");
    } else {
        r.secondary_used = 0.0;
        r.secondary_reset_at = 0;
    }
    r.updated_at = now;
}

fn apply_window(a: &Account, primary: &Value, secondary: &Value) {
    let now = now_ms();
    let reset = |w: &Value| {
        w["reset_at"]
            .as_f64()
            .map(|s| (s * 1000.0) as i64)
            .or_else(|| w["reset_after_seconds"].as_f64().map(|s| now + (s * 1000.0) as i64))
            .unwrap_or(0)
    };
    let mins = |w: &Value| {
        w["window_minutes"]
            .as_i64()
            .or_else(|| w["limit_window_seconds"].as_i64().map(|s| s / 60))
            .unwrap_or(0)
    };
    let mut r = a.rl.lock();
    if primary.is_object() {
        r.primary_used = primary["used_percent"].as_f64().unwrap_or(0.0);
        r.primary_window_min = mins(primary);
        r.primary_reset_at = reset(primary);
    }
    if secondary.is_object() {
        r.secondary_used = secondary["used_percent"].as_f64().unwrap_or(0.0);
        r.secondary_window_min = mins(secondary);
        r.secondary_reset_at = reset(secondary);
    }
    r.updated_at = now;
}

pub fn apply_rate_event(a: &Account, ev: &Value) {
    let rl = &ev["rate_limits"];
    apply_window(a, &rl["primary"], &rl["secondary"]);
}

pub fn limit_reset_from_error(body: &Value) -> Option<i64> {
    let e = &body["error"];
    e["resets_at"]
        .as_i64()
        .map(|s| s * 1000)
        .or_else(|| e["resets_in_seconds"].as_i64().map(|s| now_ms() + s * 1000))
}

pub async fn fetch_usage(app: &App, a: &Account) -> Result<Value, String> {
    let (token, acct) = {
        let c = a.cfg.read();
        (c.access_token.clone(), c.chatgpt_account_id.clone())
    };
    let res = app
        .http
        .get(format!("{}/wham/usage", app.settings().upstream_base))
        .bearer_auth(token)
        .header("chatgpt-account-id", acct)
        .header("accept", "application/json")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("usage fetch: {}", res.status()));
    }
    let v: Value = res.json().await.map_err(|e| e.to_string())?;
    apply_window(a, &v["rate_limit"]["primary_window"], &v["rate_limit"]["secondary_window"]);
    if let Some(p) = v["plan_type"].as_str() {
        a.cfg.write().plan = p.into();
    }
    Ok(v)
}

pub async fn fetch_models(app: &App, a: &Account) -> Result<(), String> {
    let path = format!("/codex/models?client_version={}", app.settings().codex_version);
    let res = request(app, a, reqwest::Method::GET, &path, None)
        .header("accept", "application/json")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let v: Value = res.json().await.map_err(|e| e.to_string())?;
    let models: Vec<(String, u64)> = v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| Some((m["slug"].as_str()?.to_string(), m["context_window"].as_u64().unwrap_or(272_000))))
        .collect();
    if !models.is_empty() {
        *app.live_models.write() = models;
    }
    Ok(())
}

pub async fn warmup(app: &Arc<App>, a: &Arc<Account>) -> Value {
    let w = a.cfg.read().warmup.clone();
    let (model, prompt) = w.map_or_else(|| (app.settings().warmup_model.clone(), "hi".to_string()), |w| (w.model, w.prompt));
    let model = if model.is_empty() {
        app.settings().warmup_model.clone()
    } else {
        model
    };
    let started = now_ms();
    if let Err(e) = ensure_fresh(app, a, false).await {
        return json!({"ok": false, "error": e});
    }
    let mut body = json!({
        "model": model, "instructions": "Reply with OK only.",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": prompt}]}],
        "reasoning": {"effort": "low"},
    });
    prepare_body(&mut body);
    let res = match request(app, a, reqwest::Method::POST, "/codex/responses", None)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    let status = res.status().as_u16();
    apply_rate_headers(a, res.headers());
    if status >= 400 {
        let err = res.text().await.unwrap_or_default();
        a.fail(now_ms() + 30_000, format!("warmup {status}: {err}"));
        return json!({"ok": false, "status": status, "error": err});
    }
    let mut usage = Value::Null;
    let mut events = eventsource_stream::Eventsource::eventsource(res.bytes_stream());
    while let Some(Ok(ev)) = events.next().await {
        if (ev.event == "response.completed" || ev.data.contains("\"response.completed\""))
            && let Ok(v) = serde_json::from_str::<Value>(&ev.data)
        {
            usage = v["response"]["usage"].clone();
        }
    }
    tracing::info!(account = %a.id, %model, status, "warmup sent");
    app.record(None, usage_row("", &a.id, &model, "warmup", status, &usage, started, started));
    if let Some(w) = a.cfg.write().warmup.as_mut() {
        w.last_fired = now_ms();
    }
    app.save_account(a);
    json!({"ok": true, "status": status, "at": ts_iso(now_ms()), "usage": usage})
}

pub fn usage_row(key: &str, account: &str, model: &str, endpoint: &'static str, status: u16, u: &Value, started: i64, ttfb: i64) -> Usage {
    let now = now_ms();
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    Usage {
        ts: now,
        key_id: key.into(),
        account_id: account.into(),
        model: model.into(),
        endpoint,
        status,
        input: n(&u["input_tokens"]),
        cached: n(&u["input_tokens_details"]["cached_tokens"]),
        output: n(&u["output_tokens"]),
        reasoning: n(&u["output_tokens_details"]["reasoning_tokens"]),
        cost: u["cost"].as_f64().unwrap_or_else(|| crate::app::pricing::cost(model, u)),
        latency_ms: (now - started).max(0) as u64,
        ttfb_ms: (ttfb - started).max(0) as u64,
    }
}

pub async fn device_login_start(app: Arc<App>, label: Option<String>) -> Result<Value, String> {
    let res = app
        .http
        .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
        .json(&json!({"client_id": CLIENT_ID}))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if res.status() == 404 {
        return Err("device code login is not enabled for this ChatGPT workspace; use `nanolb login --browser` or import auth.json".into());
    }
    let v: Value = res.json().await.map_err(|e| e.to_string())?;
    let device_id = v["device_auth_id"].as_str().ok_or("no device_auth_id")?.to_string();
    let user_code = v["user_code"]
        .as_str()
        .or(v["usercode"].as_str())
        .ok_or("no user_code")?
        .to_string();
    let interval = v["interval"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or(v["interval"].as_u64())
        .unwrap_or(5);
    let login_id = crate::id::random_hex(8);
    let expires = now_ms() + 15 * 60_000;
    app.logins.lock().insert(login_id.clone(), json!({"status": "pending"}));
    let (app2, id2, code2) = (app.clone(), login_id.clone(), user_code.clone());
    tokio::spawn(async move {
        let result = poll_device(&app2, &device_id, &code2, interval, expires, label.as_deref()).await;
        let v = match result {
            Ok(a) => {
                tracing::info!(account = %a.id, "device login completed");
                json!({"status": "done", "account": a.view()})
            }
            Err(e) => {
                tracing::warn!(error = %e, "device login failed");
                json!({"status": "error", "error": e})
            }
        };
        app2.logins.lock().insert(id2, v);
    });
    Ok(json!({
        "login_id": login_id, "verification_url": format!("{ISSUER}/codex/device"),
        "user_code": user_code, "expires_at": ts_iso(expires),
    }))
}

async fn poll_device(
    app: &Arc<App>,
    device_id: &str,
    code: &str,
    interval: u64,
    expires: i64,
    label: Option<&str>,
) -> Result<Arc<Account>, String> {
    while now_ms() < expires {
        tokio::time::sleep(Duration::from_secs(interval.max(2))).await;
        let Ok(res) = app
            .http
            .post(format!("{ISSUER}/api/accounts/deviceauth/token"))
            .json(&json!({"device_auth_id": device_id, "user_code": code}))
            .send()
            .await
        else {
            continue;
        };
        if matches!(res.status().as_u16(), 403 | 404) {
            continue;
        }
        if !res.status().is_success() {
            return Err(format!("device token poll failed: {}", res.status()));
        }
        let v: Value = res.json().await.map_err(|e| e.to_string())?;
        let code = v["authorization_code"].as_str().ok_or("no authorization_code")?;
        let verifier = v["code_verifier"].as_str().ok_or("no code_verifier")?;
        let t = exchange_code(&app.http, code, verifier, &format!("{ISSUER}/deviceauth/callback")).await?;
        let s = |k: &str| t[k].as_str().unwrap_or_default().to_string();
        let cfg = account_from_tokens(&s("id_token"), &s("access_token"), &s("refresh_token"), label)?;
        return Ok(add_account(app, cfg));
    }
    Err("device login expired".into())
}

pub fn spawn_background(app: Arc<App>) {
    let a1 = app.clone();
    tokio::spawn(async move {
        let _ = a1.http.head(a1.settings().upstream_base.clone()).send().await;
        loop {
            let accs = a1.accounts.read().clone();
            if let Some(a) = accs.iter().find(|a| a.cfg.read().kind == "codex" && a.available(now_ms())) {
                let _ = fetch_models(&a1, a).await;
            }
            for a in accs.iter().filter(|a| a.cfg.read().enabled && a.cfg.read().kind != "openrouter") {
                if ensure_fresh(&a1, a, false).await.is_ok()
                    && a.cfg.read().kind == "codex"
                    && let Err(e) = fetch_usage(&a1, a).await
                {
                    tracing::warn!(account = %a.id, error = %e, "usage poll failed");
                    *a.last_error.lock() = Some((now_ms(), e));
                }
            }
            a1.persist_runtime();
            tokio::time::sleep(Duration::from_secs(a1.settings().usage_poll_secs.max(15))).await;
        }
    });
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            let now = chrono::Local::now();
            let accs = app.accounts.read().clone();
            for a in accs {
                let due = {
                    let c = a.cfg.read();
                    c.enabled && c.warmup.as_ref().is_some_and(|w| warmup_due(&w.cron, w.last_fired, now))
                };
                if !due {
                    continue;
                }
                if let Some(w) = a.cfg.write().warmup.as_mut() {
                    w.last_fired = now_ms();
                }
                app.save_account(&a);
                if window_idle(&a) {
                    let app = app.clone();
                    tokio::spawn(async move {
                        warmup(&app, &a).await;
                    });
                }
            }
        }
    });
}

fn window_idle(a: &Account) -> bool {
    let r = a.rl.lock();
    let now = now_ms();
    r.updated_at == 0 || r.primary_reset_at <= now || r.secondary_reset_at <= now
}

pub fn warmup_due(cron: &str, last_fired: i64, now: chrono::DateTime<chrono::Local>) -> bool {
    use chrono::TimeZone;
    let Ok(c) = cron.parse::<croner::Cron>() else { return false };
    let since = if last_fired > 0 {
        chrono::Local.timestamp_millis_opt(last_fired).unwrap()
    } else {
        now - chrono::Duration::minutes(1)
    };
    c.find_next_occurrence(&since, false).is_ok_and(|t| t <= now)
}

pub async fn wham(app: &App, a: &Account, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
    let (token, acct) = {
        let c = a.cfg.read();
        (c.access_token.clone(), c.chatgpt_account_id.clone())
    };
    let mut rb = app
        .http
        .request(method, format!("{}/wham/{path}", app.settings().upstream_base))
        .bearer_auth(token)
        .header("chatgpt-account-id", acct)
        .header("accept", "application/json")
        .timeout(Duration::from_secs(15));
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    let res = rb.send().await.map_err(|e| e.to_string())?;
    let status = res.status();
    let text = res.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("wham/{path} {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("wham/{path}: invalid json: {e}"))
}
