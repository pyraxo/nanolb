use super::limits::apply_window;
use super::request;
use crate::app::{Account, App};
use crate::upstream::UpstreamError;
use reqwest::{Method, RequestBuilder};
use serde_json::Value;
use std::time::Duration;

const DEFAULT_CONTEXT_WINDOW: u64 = 272_000;

fn json_request(app: &App, a: &Account, method: Method, path: &str, timeout: Duration) -> RequestBuilder {
    let (token, acct) = {
        let c = a.cfg.read();
        (c.access_token.clone(), c.chatgpt_account_id.clone())
    };
    app.http
        .request(method, format!("{}/wham/{path}", app.settings().upstream_base))
        .bearer_auth(token)
        .header("chatgpt-account-id", acct)
        .header("accept", "application/json")
        .timeout(timeout)
}

pub(crate) async fn fetch_usage(app: &App, a: &Account) -> Result<Value, UpstreamError> {
    let res = json_request(app, a, Method::GET, "usage", Duration::from_secs(10)).send().await?;
    if !res.status().is_success() {
        return Err(UpstreamError::UsageFetch(res.status()));
    }
    let v: Value = res.json().await?;
    let rl = &v["rate_limit"];
    apply_window(a, &rl["primary_window"], &rl["secondary_window"]);
    if let Some(p) = v["plan_type"].as_str() {
        a.cfg.write().plan = p.into();
    }
    Ok(v)
}

pub(super) async fn fetch_models(app: &App, a: &Account) -> Result<(), UpstreamError> {
    let s = app.settings();
    let path = format!("/codex/models?client_version={}", s.codex_version);
    let res = request(app, &s, a, Method::GET, &path, None)
        .header("accept", "application/json")
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let v: Value = res.json().await?;
    let models: Vec<(String, u64)> = v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| {
            let slug = m["slug"].as_str()?.to_owned();
            Some((slug, m["context_window"].as_u64().unwrap_or(DEFAULT_CONTEXT_WINDOW)))
        })
        .collect();
    if !models.is_empty() {
        app.models.set(models);
    }
    Ok(())
}

pub(crate) async fn wham(app: &App, a: &Account, method: Method, path: &str, body: Option<Value>) -> Result<Value, UpstreamError> {
    let mut rb = json_request(app, a, method, path, Duration::from_secs(15));
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    let res = rb.send().await?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        return Err(UpstreamError::Wham {
            path: path.to_owned(),
            status,
            body: text,
        });
    }
    serde_json::from_str(&text).map_err(|source| UpstreamError::WhamJson {
        path: path.to_owned(),
        source,
    })
}
