use super::AdminError;
use crate::app::{App, Provider, Settings};
use crate::time::{parse_since, ts_iso};
use serde_json::{Value, json};

pub(super) fn usage(app: &App, a: &Value) -> Result<Value, AdminError> {
    Ok(app
        .store
        .usage_summary(parse_since(&a["since"])?, a["group"].as_str().unwrap_or("all"))?)
}

pub(super) fn requests(app: &App, a: &Value) -> Result<Value, AdminError> {
    let limit = u32::try_from(a["limit"].as_u64().unwrap_or(50).min(5000)).unwrap_or(5000);
    let rows = app
        .store
        .recent(limit)?
        .into_iter()
        .map(|u| {
            let mut v = serde_json::to_value(&u)?;
            v["ts"] = Value::String(ts_iso(u.ts));
            Ok(v)
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?;
    Ok(json!(rows))
}

pub(super) fn settings_get(app: &App) -> Result<Value, AdminError> {
    Ok(serde_json::to_value(&*app.settings())?)
}

pub(super) fn settings_update(app: &App, a: Value) -> Result<Value, AdminError> {
    let mut cur = serde_json::to_value(&*app.settings())?;
    let Value::Object(upd) = a else {
        return Err(AdminError::Invalid("settings_update takes an object".into()));
    };
    for (k, v) in upd {
        if cur.get(&k).is_none() {
            return Err(AdminError::Invalid(format!("unknown setting `{k}`")));
        }
        cur[&k] = v;
    }
    let s: Settings = serde_json::from_value(cur).map_err(|e| AdminError::Invalid(e.to_string()))?;
    app.save_settings(s)?;
    settings_get(app)
}

pub(super) fn models(app: &App) -> Value {
    json!(
        app.model_list()
            .into_iter()
            .map(|(m, ctx)| json!({"provider": Provider::for_model(&m), "id": m, "context_window": ctx}))
            .collect::<Vec<_>>()
    )
}
