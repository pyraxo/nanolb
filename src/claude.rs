use crate::proxy::{Inflight, Kind, error};
use crate::state::{Account, AccountCfg, App, Key};
use crate::{now_ms, sse, ts_iso};
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use oauth2::PkceCodeChallenge;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const SCOPES: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const IDENTITIES: &[&str] = &[
    "You are Claude Code, Anthropic's official CLI for Claude.",
    "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
];
const PRICES: &[(&str, [f64; 5])] = &[
    ("claude-opus", [4.0, 20.0, 0.2, 5.0, 8.0]),
    ("claude-sonnet", [2.0, 10.0, 0.1, 2.5, 4.0]),
    ("claude-haiku", [0.1, 0.5, 0.01, 0.125, 0.2]),
];

pub fn login_start(app: &App, label: Option<String>) -> Value {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let state = crate::random_hex(16);
    let login_id = crate::random_hex(8);
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).unwrap();
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge.as_str())
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    app.logins.lock().insert(
        login_id.clone(),
        json!({"status": "pending", "kind": "claude", "verifier": verifier.secret(), "state": state, "label": label}),
    );
    json!({
        "login_id": login_id, "authorize_url": url.as_str(),
        "next": "open authorize_url, sign in to the Claude account, copy the code shown, then call claude_login_finish {login_id, code}",
    })
}

pub async fn login_finish(app: &App, login_id: &str, code: &str) -> Result<Arc<Account>, String> {
    let pending = app
        .logins
        .lock()
        .get(login_id)
        .cloned()
        .ok_or_else(|| format!("login {login_id} not found"))?;
    if pending["kind"] != "claude" {
        return Err("login_id is not a claude login".into());
    }
    let (code, state) = code
        .trim()
        .split_once('#')
        .unwrap_or((code.trim(), pending["state"].as_str().unwrap_or_default()));
    let body = json!({
        "grant_type": "authorization_code", "code": code, "state": state, "redirect_uri": REDIRECT_URI,
        "client_id": CLIENT_ID, "code_verifier": pending["verifier"],
    });
    let mut attempt = 0;
    let res = loop {
        match app.http.post(TOKEN_URL).json(&body).send().await {
            Ok(r) => break r,
            Err(e) if attempt < 2 && (e.is_connect() || e.is_timeout() || e.is_request()) => {
                attempt += 1;
                tracing::warn!(error = ?e, attempt, "claude token exchange transport error, retrying");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => return Err(format!("claude token exchange: {e:?}")),
        }
    };
    let status = res.status();
    let t: Value = res.json().await.unwrap_or_default();
    if !status.is_success() || t["access_token"].as_str().is_none() {
        return Err(format!("claude token exchange failed ({status}): {}", t["error"]));
    }
    let email = t["account"]["email_address"].as_str().unwrap_or_default().to_string();
    let uuid = t["account"]["uuid"].as_str().unwrap_or_default().to_string();
    let refresh = t["refresh_token"].as_str().unwrap_or_default().to_string();
    let cfg = AccountCfg {
        id: crate::sha256_hex(&format!("claude:{}", if uuid.is_empty() { &refresh } else { &uuid }))[..12].to_string(),
        kind: "claude".into(),
        label: pending["label"].as_str().map_or_else(|| email.clone(), str::to_string),
        email,
        plan: t["organization"]["organization_type"].as_str().unwrap_or("claude").to_string(),
        chatgpt_account_id: uuid,
        access_token: t["access_token"].as_str().unwrap_or_default().into(),
        refresh_token: refresh,
        expires_at: now_ms() + t["expires_in"].as_i64().unwrap_or(28_800) * 1000,
        last_refresh: now_ms(),
        enabled: true,
        created_at: now_ms(),
        ..Default::default()
    };
    let acc = crate::codex::add_account(app, cfg);
    app.logins
        .lock()
        .insert(login_id.into(), json!({"status": "done", "account": acc.view()}));
    tracing::info!(account = %acc.id, "claude login completed");
    Ok(acc)
}

pub fn account_from_token(token: &str, label: Option<&str>) -> Result<AccountCfg, String> {
    if !token.starts_with("sk-ant-oat") {
        return Err("expected a `claude setup-token` token starting with sk-ant-oat".into());
    }
    Ok(AccountCfg {
        id: crate::sha256_hex(&format!("claude:{token}"))[..12].to_string(),
        kind: "claude".into(),
        label: label.unwrap_or("claude").into(),
        plan: "claude (setup-token)".into(),
        access_token: token.into(),
        expires_at: now_ms() + 365 * 86_400_000,
        last_refresh: now_ms(),
        enabled: true,
        created_at: now_ms(),
        ..Default::default()
    })
}

pub async fn refresh(app: &App, a: &Account) -> Result<(), String> {
    let rt = a.cfg.read().refresh_token.clone();
    if rt.is_empty() {
        return Ok(());
    }
    let res = app
        .http
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(15))
        .json(&json!({"grant_type": "refresh_token", "refresh_token": rt, "client_id": CLIENT_ID, "scope": SCOPES}))
        .send()
        .await
        .map_err(|e| format!("refresh: {e}"))?;
    let status = res.status();
    let t: Value = res.json().await.unwrap_or_default();
    let Some(access) = t["access_token"].as_str().filter(|_| status.is_success()) else {
        let msg = format!("claude token refresh failed ({status}): {}", t["error"]);
        tracing::error!(account = %a.id, %status, "claude token refresh failed");
        a.fail(now_ms() + if status.is_client_error() { 3_600_000 } else { 30_000 }, msg.clone());
        return Err(msg);
    };
    {
        let mut c = a.cfg.write();
        if let Some(r) = t["refresh_token"].as_str() {
            c.refresh_token = r.into();
        }
        c.access_token = access.into();
        c.expires_at = now_ms() + t["expires_in"].as_i64().unwrap_or(28_800) * 1000;
        c.last_refresh = now_ms();
    }
    app.save_account(a);
    tracing::info!(account = %a.id, expires_at = %ts_iso(a.cfg.read().expires_at), "claude token refreshed");
    Ok(())
}

fn apply_rate_headers(a: &Account, h: &reqwest::header::HeaderMap) {
    let f = |k: &str| {
        h.get(format!("anthropic-ratelimit-unified-{k}"))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok())
    };
    let (Some(u5), Some(u7)) = (f("5h-utilization"), f("7d-utilization")) else {
        return;
    };
    let mut r = a.rl.lock();
    r.primary_used = u5 * 100.0;
    r.primary_window_min = 300;
    r.primary_reset_at = f("5h-reset").map_or(0, |s| (s * 1000.0) as i64);
    r.secondary_used = u7 * 100.0;
    r.secondary_window_min = 10_080;
    r.secondary_reset_at = f("7d-reset").map_or(0, |s| (s * 1000.0) as i64);
    r.updated_at = now_ms();
}

fn limited_until(h: &reqwest::header::HeaderMap) -> i64 {
    let g = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
    g("anthropic-ratelimit-unified-reset")
        .map(|s| (s * 1000.0) as i64)
        .or_else(|| g("retry-after").map(|s| now_ms() + (s * 1000.0) as i64))
        .unwrap_or(now_ms() + 60_000)
}

fn ensure_identity(body: &mut Value) {
    let blocks = match body.get("system") {
        Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };
    let has = blocks
        .iter()
        .any(|b| b["text"].as_str().is_some_and(|t| IDENTITIES.iter().any(|i| t.starts_with(i))));
    if has && !body["system"].is_string() {
        return;
    }
    let mut out = Vec::with_capacity(blocks.len() + 1);
    if !has {
        out.push(json!({"type": "text", "text": IDENTITY}));
    }
    out.extend(blocks);
    body["system"] = Value::Array(out);
}

fn session_key(h: &HeaderMap, body: &Value) -> String {
    if let Some(s) = h
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        return s.to_string();
    }
    if let Some(uid) = body["metadata"]["user_id"].as_str() {
        if let Some(s) = serde_json::from_str::<Value>(uid)
            .ok()
            .and_then(|v| v["session_id"].as_str().map(str::to_string))
        {
            return s;
        }
        if let Some((_, s)) = uid.split_once("_session_") {
            return s.to_string();
        }
    }
    let seed = format!(
        "{}{}{}",
        body["model"],
        body["system"],
        body["messages"].get(0).unwrap_or(&Value::Null)
    );
    crate::sha256_hex(&seed)[..24].to_string()
}

fn usage_value(model: &str, u: &Value) -> Value {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let (input, read, write, out) = (
        n("input_tokens"),
        n("cache_read_input_tokens"),
        n("cache_creation_input_tokens"),
        n("output_tokens"),
    );
    let w1h = u["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0);
    let cost = PRICES.iter().find(|(p, _)| model.starts_with(p)).map_or(0.0, |(_, r)| {
        (input as f64 * r[0] + out as f64 * r[1] + read as f64 * r[2] + write.saturating_sub(w1h) as f64 * r[3] + w1h as f64 * r[4]) / 1e6
    });
    json!({
        "input_tokens": input + read + write, "input_tokens_details": {"cached_tokens": read},
        "output_tokens": out, "output_tokens_details": {"reasoning_tokens": 0}, "cost": cost,
    })
}

fn merge_usage(acc: &mut Value, u: &Value) {
    if let Some(o) = u.as_object() {
        for (k, v) in o {
            if !v.is_null() {
                acc[k] = v.clone();
            }
        }
    }
}

fn request(app: &App, a: &Account, h: &HeaderMap, body: Bytes) -> reqwest::RequestBuilder {
    let s = app.settings();
    let mut betas: Vec<&str> = h
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').map(str::trim).filter(|b| !b.is_empty()).collect())
        .unwrap_or_default();
    if !betas.contains(&OAUTH_BETA) {
        betas.push(OAUTH_BETA);
    }
    let ua = h
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .filter(|u| u.starts_with("claude-cli/"))
        .map_or_else(|| format!("claude-cli/{} (external, cli)", s.claude_cli_version), str::to_string);
    let mut rb = app
        .http
        .post(format!("{}/v1/messages?beta=true", s.claude_base))
        .bearer_auth(&a.cfg.read().access_token)
        .header(
            "anthropic-version",
            h.get("anthropic-version").and_then(|v| v.to_str().ok()).unwrap_or("2023-06-01"),
        )
        .header("anthropic-beta", betas.join(","))
        .header("x-app", "cli")
        .header(header::USER_AGENT, ua)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body);
    for (k, v) in h {
        if k.as_str().starts_with("x-claude-code-") {
            rb = rb.header(k, v);
        }
    }
    rb
}

pub async fn warmup(app: &Arc<App>, a: &Arc<Account>) -> Value {
    let w = a.cfg.read().warmup.clone();
    let model = w
        .as_ref()
        .map(|w| w.model.clone())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| app.settings().claude_warmup_model.clone());
    let prompt = w.map_or_else(|| "hi".to_string(), |w| w.prompt);
    if let Err(e) = crate::codex::ensure_fresh(app, a, false).await {
        return json!({"ok": false, "error": e});
    }
    let started = now_ms();
    let mut body = json!({"model": model, "max_tokens": 1, "messages": [{"role": "user", "content": prompt}]});
    ensure_identity(&mut body);
    let res = match request(app, a, &HeaderMap::new(), Bytes::from(body.to_string())).send().await {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    apply_rate_headers(a, res.headers());
    let status = res.status().as_u16();
    let v: Value = res.json().await.unwrap_or_default();
    if status >= 400 {
        a.fail(now_ms() + 30_000, format!("warmup {status}: {}", v["error"]["message"]));
        return json!({"ok": false, "status": status, "error": v["error"]});
    }
    tracing::info!(account = %a.id, %model, status, "warmup sent");
    app.record(
        None,
        crate::codex::usage_row(
            "",
            &a.id,
            &model,
            "warmup",
            status,
            &usage_value(&model, &v["usage"]),
            started,
            started,
        ),
    );
    if let Some(w) = a.cfg.write().warmup.as_mut() {
        w.last_fired = now_ms();
    }
    app.save_account(a);
    json!({"ok": true, "status": status, "at": ts_iso(now_ms())})
}

pub async fn handle(app: Arc<App>, key: Arc<Key>, h: HeaderMap, mut req: Value, started: i64) -> Response {
    let model = req["model"].as_str().unwrap_or_default().to_string();
    let stream = req["stream"].as_bool().unwrap_or(false);
    ensure_identity(&mut req);
    let session = format!("{}\u{1f}claude\u{1f}{}", key.cfg.id, session_key(&h, &req));
    let body = Bytes::from(serde_json::to_vec(&req).unwrap());
    let first_timeout = Duration::from_secs(app.settings().first_event_timeout_secs.max(5));
    let mut tried: Vec<String> = vec![];
    let mut refreshed: Vec<String> = vec![];
    let mut last = (503u16, format!("no available claude accounts for {model}"));
    for _ in 0..app.settings().max_attempts.max(1) {
        let Some(acc) = app.pick("claude", &key.cfg.accounts, &tried, Some(&session)) else {
            break;
        };
        if let Err(e) = crate::codex::ensure_fresh(&app, &acc, false).await {
            tried.push(acc.id.clone());
            last = (502, e);
            continue;
        }
        acc.inflight.fetch_add(1, Relaxed);
        let guard = Inflight(acc.clone());
        let res = match tokio::time::timeout(first_timeout, request(&app, &acc, &h, body.clone()).send()).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                acc.fail(now_ms() + 15_000, format!("connect: {e}"));
                tried.push(acc.id.clone());
                last = (502, format!("upstream connect failed: {e}"));
                continue;
            }
            Err(_) => {
                acc.fail(now_ms() + 15_000, "timed out waiting for response headers".into());
                tried.push(acc.id.clone());
                last = (504, "upstream timed out".into());
                continue;
            }
        };
        apply_rate_headers(&acc, res.headers());
        let status = res.status().as_u16();
        if status == 401 && !refreshed.contains(&acc.id) {
            refreshed.push(acc.id.clone());
            if crate::codex::ensure_fresh(&app, &acc, true).await.is_ok() {
                continue;
            }
        }
        if status >= 400 {
            let until = match status {
                429 => Some(limited_until(res.headers())),
                401 => Some(now_ms() + 300_000),
                500.. => Some(now_ms() + 5_000),
                _ => None,
            };
            let text = res.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.chars().take(500).collect());
            let Some(until) = until else {
                app.record(
                    Some(&key),
                    crate::codex::usage_row(&key.cfg.id, &acc.id, &model, "anthropic", status, &Value::Null, started, now_ms()),
                );
                return error(Kind::Anthropic, status, &msg);
            };
            acc.fail(until, format!("{status}: {msg}"));
            tracing::warn!(account = %acc.id, status, error = %msg, "claude upstream error, failing over");
            tried.push(acc.id.clone());
            last = (status, msg);
            continue;
        }
        let ttfb = now_ms();
        if !stream {
            let bytes = res.bytes().await.unwrap_or_default();
            let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
            app.record(
                Some(&key),
                crate::codex::usage_row(
                    &key.cfg.id,
                    &acc.id,
                    &model,
                    "anthropic",
                    status,
                    &usage_value(&model, &v["usage"]),
                    started,
                    ttfb,
                ),
            );
            drop(guard);
            let mut r = (StatusCode::OK, bytes).into_response();
            r.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            return r;
        }
        let idle = Duration::from_secs(app.settings().idle_timeout_secs.max(30));
        let (sender, rx) = tokio::sync::mpsc::channel::<Bytes>(128);
        let (app2, key2, model2) = (app.clone(), key.clone(), model.clone());
        tokio::spawn(async move {
            let _guard = guard;
            let mut events = res.bytes_stream().eventsource();
            let mut usage = json!({});
            let mut ok = false;
            while let Ok(Some(Ok(ev))) = tokio::time::timeout(idle, events.next()).await {
                match ev.event.as_str() {
                    "message_start" | "message_delta" => {
                        if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                            merge_usage(
                                &mut usage,
                                if ev.event == "message_start" {
                                    &v["message"]["usage"]
                                } else {
                                    &v["usage"]
                                },
                            );
                        }
                    }
                    "message_stop" => ok = true,
                    _ => {}
                }
                if sender.send(sse::frame(Some(&ev.event), &ev.data)).await.is_err() {
                    break;
                }
            }
            let status = if ok { 200 } else { 499 };
            app2.record(
                Some(&key2),
                crate::codex::usage_row(
                    &key2.cfg.id,
                    &acc.id,
                    &model2,
                    "anthropic",
                    status,
                    &usage_value(&model2, &usage),
                    started,
                    ttfb,
                ),
            );
        });
        let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|b| (Ok::<_, Infallible>(b), rx))
        }));
        let mut r = Response::new(body);
        r.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        return r;
    }
    tracing::warn!(%model, status = last.0, error = %last.1, "claude request failed");
    app.record(
        Some(&key),
        crate::codex::usage_row(&key.cfg.id, "", &model, "anthropic", last.0, &Value::Null, started, now_ms()),
    );
    error(Kind::Anthropic, last.0, &last.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_block_is_prepended_once_and_existing_identity_is_kept() {
        let mut plain = json!({"system": "Be terse."});
        ensure_identity(&mut plain);
        assert_eq!(
            plain["system"],
            json!([{"type": "text", "text": IDENTITY}, {"type": "text", "text": "Be terse."}])
        );

        let cc = json!([{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.300"}, {"type": "text", "text": IDENTITY}]);
        let mut from_claude_code = json!({"system": cc.clone()});
        ensure_identity(&mut from_claude_code);
        assert_eq!(from_claude_code["system"], cc);
    }

    #[test]
    fn cost_counts_cache_reads_and_writes_at_their_own_rates() {
        let u = usage_value(
            "claude-sonnet-5-5",
            &json!({
                "input_tokens": 1000, "cache_read_input_tokens": 10000, "cache_creation_input_tokens": 2000,
                "cache_creation": {"ephemeral_1h_input_tokens": 1000}, "output_tokens": 500,
            }),
        );
        assert_eq!(u["input_tokens"], 13000);
        assert!((u["cost"].as_f64().unwrap() - 0.0145).abs() < 1e-9);
    }
}
