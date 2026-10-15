use super::args::{AccountsCmd, KeyOpts, KeysCmd, ResetCreditsCmd, SessionsCmd};
use super::client::Client;
use super::show;
use anyhow::anyhow;
use serde_json::{Map, Value, json};
use std::path::PathBuf;

pub(super) fn id(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| json!(s))
}

fn parse_limit(s: &str) -> anyhow::Result<Value> {
    let bad = || anyhow!("invalid limit '{s}', expected WINDOW:KIND:VALUE");
    let [window, kind, value] = s.split(':').collect::<Vec<_>>()[..] else {
        return Err(bad());
    };
    let (field, value) = match kind {
        "tokens" => ("max_tokens", json!(value.parse::<u64>().map_err(|_| bad())?)),
        "requests" => ("max_requests", json!(value.parse::<u64>().map_err(|_| bad())?)),
        "usd" => ("max_cost_usd", json!(value.parse::<f64>().map_err(|_| bad())?)),
        _ => return Err(bad()),
    };
    Ok(json!({ "window": window, field: value }))
}

impl KeyOpts {
    fn fill(self, args: &mut Map<String, Value>) -> anyhow::Result<()> {
        if !self.accounts.is_empty() {
            args.insert("accounts".into(), self.accounts.iter().map(|a| id(a)).collect());
        }
        if !self.models.is_empty() {
            args.insert("models".into(), json!(self.models));
        }
        if !self.limits.is_empty() {
            let limits = self.limits.iter().map(|l| parse_limit(l)).collect::<Result<Vec<_>, _>>()?;
            args.insert("limits".into(), json!(limits));
        }
        if let Some(e) = self.expires {
            args.insert("expires_at".into(), json!(e));
        }
        Ok(())
    }
}

pub(super) async fn accounts(c: &Client, cmd: AccountsCmd) -> anyhow::Result<()> {
    match cmd {
        AccountsCmd::Ls => show(c, "accounts_list", json!({})).await,
        AccountsCmd::Import { path, label } => {
            let path = path.unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex/auth.json"));
            let auth_json = std::fs::read_to_string(&path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
            show(c, "account_import", json!({ "auth_json": auth_json, "label": label })).await
        }
        AccountsCmd::AddOpenrouter { api_key, label } => {
            show(c, "account_add_openrouter", json!({ "api_key": api_key, "label": label })).await
        }
        AccountsCmd::Rm { id: i } => show(c, "account_remove", json!({ "id": id(&i) })).await,
        AccountsCmd::Enable { id: i } => show(c, "account_update", json!({ "id": id(&i), "enabled": true })).await,
        AccountsCmd::Disable { id: i } => show(c, "account_update", json!({ "id": id(&i), "enabled": false })).await,
        AccountsCmd::Priority { id: i, n } => show(c, "account_update", json!({ "id": id(&i), "priority": n })).await,
        AccountsCmd::Refresh { id: i } => show(c, "account_refresh", json!({ "id": id(&i) })).await,
        AccountsCmd::Warmup { id: i } => show(c, "account_warmup", json!({ "id": id(&i) })).await,
        AccountsCmd::SetWarmup {
            id: i,
            cron,
            model,
            prompt,
        } => {
            let warmup = json!({ "cron": cron, "model": model, "prompt": prompt });
            show(c, "account_update", json!({ "id": id(&i), "warmup": warmup })).await
        }
        AccountsCmd::ClearWarmup { id: i } => show(c, "account_update", json!({ "id": id(&i), "warmup": null })).await,
    }
}

pub(super) async fn keys(c: &Client, cmd: KeysCmd) -> anyhow::Result<()> {
    match cmd {
        KeysCmd::Ls => show(c, "keys_list", json!({})).await,
        KeysCmd::Create { name, opts } => {
            let mut args = Map::new();
            args.insert("name".into(), json!(name));
            opts.fill(&mut args)?;
            show(c, "key_create", Value::Object(args)).await
        }
        KeysCmd::Rm { id: i } => show(c, "key_remove", json!({ "id": id(&i) })).await,
        KeysCmd::Enable { id: i } => show(c, "key_update", json!({ "id": id(&i), "enabled": true })).await,
        KeysCmd::Disable { id: i } => show(c, "key_update", json!({ "id": id(&i), "enabled": false })).await,
        KeysCmd::Update { id: i, opts } => {
            let mut args = Map::new();
            args.insert("id".into(), id(&i));
            opts.fill(&mut args)?;
            show(c, "key_update", Value::Object(args)).await
        }
    }
}

pub(super) async fn reset_credits(c: &Client, cmd: ResetCreditsCmd) -> anyhow::Result<()> {
    match cmd {
        ResetCreditsCmd::Ls { account } => show(c, "reset_credits_list", json!({ "id": id(&account) })).await,
        ResetCreditsCmd::Consume { account, credit } => {
            show(c, "reset_credits_consume", json!({ "id": id(&account), "credit_id": credit })).await
        }
    }
}

pub(super) async fn sessions(c: &Client, cmd: SessionsCmd) -> anyhow::Result<()> {
    match cmd {
        SessionsCmd::Ls { key, account, limit } => {
            show(c, "sessions_list", json!({ "key": key, "account": account, "limit": limit })).await
        }
        SessionsCmd::Rm { key } => show(c, "session_delete", json!({ "key": key })).await,
        SessionsCmd::Purge { all, account, older_than } => {
            show(
                c,
                "sessions_purge",
                json!({ "all": all, "account": account, "older_than": older_than }),
            )
            .await
        }
    }
}
