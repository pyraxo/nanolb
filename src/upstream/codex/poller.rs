use super::auth::ensure_fresh;
use super::warmup::{mark_fired, warmup, warmup_due, window_idle};
use super::wham::{fetch_models, fetch_usage};
use crate::app::{App, Provider};
use crate::time::now_ms;
use std::sync::Arc;
use std::time::Duration;

const MIN_POLL_SECS: u64 = 15;
const WARMUP_TICK: Duration = Duration::from_secs(20);
const CODEX_RELEASE_URL: &str = "https://registry.npmjs.org/@openai/codex/latest";
const VERSION_CHECK: Duration = Duration::from_hours(24);

pub(crate) fn spawn_background(app: Arc<App>) {
    tokio::spawn(poll_usage(app.clone()));
    tokio::spawn(track_codex_version(app.clone()));
    tokio::spawn(fire_warmups(app));
}

async fn track_codex_version(app: Arc<App>) {
    loop {
        if let Some(v) = latest_codex_version(&app).await {
            tracing::info!(version = %v, "codex client version");
            app.set_codex_latest(v);
        } else {
            tracing::warn!("cannot read the latest codex version, keeping the current one");
        }
        tokio::time::sleep(VERSION_CHECK).await;
    }
}

async fn latest_codex_version(app: &App) -> Option<String> {
    let res = app.http.get(CODEX_RELEASE_URL).timeout(Duration::from_secs(10)).send().await.ok()?;
    let v: serde_json::Value = res.json().await.ok()?;
    let version = v["version"].as_str()?;
    let valid = !version.is_empty() && version.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    valid.then(|| version.to_owned())
}

async fn poll_usage(app: Arc<App>) {
    let _ = app.http.head(app.settings().upstream_base.clone()).send().await;
    loop {
        let accs = app.accounts.snapshot();
        if let Some(a) = accs.iter().find(|a| a.provider() == Provider::Codex && a.available(now_ms())) {
            let _ = fetch_models(&app, a).await;
        }
        for a in accs.iter().filter(|a| a.cfg.read().enabled && a.provider() != Provider::OpenRouter) {
            if ensure_fresh(&app, a, false).await.is_ok()
                && a.provider() == Provider::Codex
                && let Err(e) = fetch_usage(&app, a).await
            {
                tracing::warn!(account = %a.id, error = %e, "usage poll failed");
                *a.last_error.lock() = Some((now_ms(), e.to_string()));
            }
        }
        app.persist_runtime();
        tokio::time::sleep(Duration::from_secs(app.settings().usage_poll_secs.max(MIN_POLL_SECS))).await;
    }
}

async fn fire_warmups(app: Arc<App>) {
    loop {
        tokio::time::sleep(WARMUP_TICK).await;
        let now = chrono::Local::now();
        for a in app.accounts.snapshot() {
            let due = {
                let c = a.cfg.read();
                c.enabled && c.warmup.as_ref().is_some_and(|w| warmup_due(&w.cron, w.last_fired, now))
            };
            if !due {
                continue;
            }
            mark_fired(&app, &a);
            if window_idle(&a) {
                let app = app.clone();
                tokio::spawn(async move {
                    warmup(&app, &a).await;
                });
            }
        }
    }
}
