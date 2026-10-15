use crate::app::{Account, App, Provider};
use crate::control::admin::{AdminError, account_arg};
use crate::upstream::codex;
use serde_json::{Value, json};
use std::sync::Arc;

const LIST_PATH: &str = "rate-limit-reset-credits";
const CONSUME_PATH: &str = "rate-limit-reset-credits/consume";

fn codex_account(app: &App, a: &Value) -> Result<Arc<Account>, AdminError> {
    let acc = account_arg(app, a)?;
    if acc.provider() != Provider::Codex {
        return Err(AdminError::Invalid(format!("account {} is not a codex account", acc.id)));
    }
    Ok(acc)
}

pub(crate) async fn reset_credits_list(app: &Arc<App>, a: &Value) -> Result<Value, AdminError> {
    let acc = codex_account(app, a)?;
    codex::ensure_fresh(app, &acc, false).await?;
    Ok(codex::wham(app, &acc, reqwest::Method::GET, LIST_PATH, None).await?)
}

fn earliest_available(list: &Value) -> Option<String> {
    list["credits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["status"].as_str() == Some("available"))
        .min_by_key(|c| c["expires_at"].as_str().unwrap_or("9999"))
        .and_then(|c| c["id"].as_str().map(str::to_owned))
}

pub(crate) async fn reset_credits_consume(app: &Arc<App>, a: &Value) -> Result<Value, AdminError> {
    let acc = codex_account(app, a)?;
    codex::ensure_fresh(app, &acc, false).await?;
    let credit_id = if let Some(c) = a["credit_id"].as_str() {
        c.to_owned()
    } else {
        let list = codex::wham(app, &acc, reqwest::Method::GET, LIST_PATH, None).await?;
        earliest_available(&list).ok_or_else(|| AdminError::Invalid(format!("account {} has no available reset credit", acc.id)))?
    };
    let body = json!({"credit_id": credit_id, "redeem_request_id": crate::id::uuid_v4()});
    let mut res = codex::wham(app, &acc, reqwest::Method::POST, CONSUME_PATH, Some(body)).await?;
    let _ = codex::fetch_usage(app, &acc).await;
    res["account"] = acc.view();
    Ok(res)
}
