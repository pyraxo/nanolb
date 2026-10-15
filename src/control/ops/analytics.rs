use crate::app::App;
use crate::control::admin::AdminError;
use crate::time::parse_since;
use serde_json::{Value, json};

pub(crate) fn analytics(app: &App, a: &Value) -> Result<Value, AdminError> {
    let default_since = json!("7d");
    let since = parse_since(a.get("since").filter(|v| !v.is_null()).unwrap_or(&default_since))?;
    let view = a["view"].as_str().unwrap_or("all");
    let by = a["by"].as_str().unwrap_or("account");
    let bucket = a["bucket"].as_str().unwrap_or("day");
    let limit = a["limit"].as_i64().unwrap_or(10).clamp(1, 1000);
    if view != "all" {
        return Ok(app.store.analytics(view, by, bucket, since, limit)?);
    }
    let mut o = serde_json::Map::new();
    for v in ["series", "latency", "errors", "cache", "top", "utilization"] {
        o.insert(v.into(), app.store.analytics(v, by, bucket, since, limit)?);
    }
    Ok(Value::Object(o))
}
