use super::AdminError;
use crate::app::{Account, App};
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn str_arg<'a>(a: &'a Value, k: &str) -> Result<&'a str, AdminError> {
    a[k].as_str()
        .ok_or_else(|| AdminError::Invalid(format!("missing string arg `{k}`")))
}

pub(crate) fn account_arg(app: &App, a: &Value) -> Result<Arc<Account>, AdminError> {
    Ok(app.accounts.resolve(str_arg(a, "id")?)?)
}

pub(super) fn invalid(what: &str) -> impl FnOnce(serde_json::Error) -> AdminError + '_ {
    move |e| AdminError::Invalid(format!("{what}: {e}"))
}
