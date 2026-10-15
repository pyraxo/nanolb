use crate::app::App;
use crate::control::admin::{AdminError, str_arg};
use crate::time::{parse_since, ts_iso};
use serde_json::{Value, json};

pub(crate) fn sessions_list(app: &App, a: &Value) -> Value {
    let key = a["key"].as_str();
    let resolved = a["account"]
        .as_str()
        .map(|q| app.accounts.resolve(q).map_or_else(|_| q.to_owned(), |acc| acc.id.clone()));
    let account = resolved.as_deref();
    let limit = usize::try_from(a["limit"].as_u64().unwrap_or(100)).unwrap_or(usize::MAX);
    let mut rows: Vec<(String, String, i64)> = app
        .sessions
        .list()
        .into_iter()
        .filter(|(k, p)| key.is_none_or(|q| k.contains(q)) && account.is_none_or(|q| p.account == q))
        .map(|(k, p)| (k, p.account, p.at))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    json!(
        rows.into_iter()
            .take(limit)
            .map(|(k, acc, t)| json!({"key": k, "account": acc, "last_used": ts_iso(t)}))
            .collect::<Vec<_>>()
    )
}

pub(crate) fn session_delete(app: &App, a: &Value) -> Result<Value, AdminError> {
    let key = str_arg(a, "key")?;
    let removed = app.sessions.remove(&app.store, key);
    Ok(json!({"removed": removed}))
}

pub(crate) fn sessions_purge(app: &App, a: &Value) -> Result<Value, AdminError> {
    let all = a["all"].as_bool().unwrap_or(false);
    let resolved = match a["account"].as_str() {
        Some(q) => Some(app.accounts.resolve(q)?.id.clone()),
        None => None,
    };
    let account = resolved.as_deref();
    let cutoff = match a.get("older_than").filter(|v| !v.is_null()) {
        Some(v) => Some(parse_since(v)?),
        None => None,
    };
    if !all && account.is_none() && cutoff.is_none() {
        return Err(AdminError::Invalid("sessions_purge needs all, account or older_than".into()));
    }
    let removed = app.sessions.purge(&app.store, |p| {
        all || account.is_some_and(|q| p.account == q) || cutoff.is_some_and(|c| p.at < c)
    });
    Ok(json!({"removed": removed}))
}
