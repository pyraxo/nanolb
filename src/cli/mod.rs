pub(crate) mod client;

mod args;
mod codex_config;
mod commands;
mod daemon;
mod login;
mod service;
mod tui;

use anyhow::anyhow;
use args::{Cli, Cmd, SettingsCmd};
use clap::Parser;
use client::Client;
use serde_json::{Value, json};
use std::process::ExitCode;

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

async fn show(c: &Client, op: &str, args: Value) -> anyhow::Result<()> {
    print(&c.call(op, args).await?);
    Ok(())
}

pub async fn run() -> ExitCode {
    let args = Cli::parse();
    let res = match args.cmd {
        Cmd::Serve { host, port } => {
            init_tracing();
            crate::server::serve(host, port).await
        }
        Cmd::Mcp => crate::control::mcp::serve_stdio(Client::from_env())
            .await
            .map_err(anyhow::Error::from),
        cmd => dispatch(cmd).await,
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("NANOLB_LOG").unwrap_or_else(|_| "info".into());
    let fmt = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);
    if std::env::var("NANOLB_LOG_FORMAT").is_ok_and(|f| f == "json") {
        fmt.json().init();
    } else {
        fmt.init();
    }
}

async fn dispatch(cmd: Cmd) -> anyhow::Result<()> {
    let c = Client::from_env();
    match cmd {
        Cmd::Serve { .. } | Cmd::Mcp => Ok(()),
        Cmd::Up { host, port } => daemon::up(&c, host, port).await,
        Cmd::Down => daemon::down().await,
        Cmd::Service { cmd } => service::service(cmd).await,
        Cmd::Status => show(&c, "status", json!({})).await,
        Cmd::Login { label, browser } => {
            let account = if browser {
                login::login_browser(&c, label).await?
            } else {
                login::login_device(&c, label).await?
            };
            print(&account);
            Ok(())
        }
        Cmd::Accounts { cmd } => commands::accounts(&c, cmd).await,
        Cmd::Keys { cmd } => commands::keys(&c, cmd).await,
        Cmd::Usage { since, group } => {
            let mut args = json!({ "since": since });
            if let Some(g) = group {
                args["group"] = json!(g);
            }
            show(&c, "usage", args).await
        }
        Cmd::Requests { limit } => show(&c, "requests", json!({ "limit": limit })).await,
        Cmd::Settings { cmd: SettingsCmd::Get } => show(&c, "settings_get", json!({})).await,
        Cmd::Settings {
            cmd: SettingsCmd::Set { key, value },
        } => {
            let value = serde_json::from_str(&value).unwrap_or_else(|_| json!(value));
            show(&c, "settings_update", json!({ key: value })).await
        }
        Cmd::Models => show(&c, "models", json!({})).await,
        Cmd::ResetCredits { cmd } => commands::reset_credits(&c, cmd).await,
        Cmd::Plan {
            work_start,
            lookback,
            apply,
        } => {
            let args = json!({ "work_start": work_start, "lookback": lookback, "apply": apply });
            show(&c, "plan", args).await
        }
        Cmd::Analytics {
            since,
            view,
            by,
            bucket,
            limit,
        } => {
            let args = json!({ "since": since, "view": view, "by": by, "bucket": bucket, "limit": limit });
            show(&c, "analytics", args).await
        }
        Cmd::Sessions { cmd } => commands::sessions(&c, cmd).await,
        Cmd::Call { op, json } => {
            let args = match json {
                Some(j) => serde_json::from_str(&j).map_err(|e| anyhow!("invalid JSON: {e}"))?,
                None => json!({}),
            };
            show(&c, &op, args).await
        }
        Cmd::CodexConfig { key } => {
            codex_config::codex_config(&c.url, key);
            Ok(())
        }
        Cmd::Top { interval } => tui::run(c, interval).await,
    }
}
