use chrono::{DateTime, SecondsFormat};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub(crate) enum TimeError {
    #[error("bad timestamp {text}: {source}")]
    Invalid { text: String, source: chrono::ParseError },
    #[error("timestamp must be RFC3339")]
    NotRfc3339,
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn ts_iso(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) fn parse_time(v: &Value) -> Result<Option<i64>, TimeError> {
    match v {
        Value::Null => Ok(None),
        Value::String(t) => DateTime::parse_from_rfc3339(t)
            .map(|d| Some(d.timestamp_millis()))
            .map_err(|source| TimeError::Invalid { text: t.clone(), source }),
        Value::Number(n) => Ok(n.as_i64()),
        _ => Err(TimeError::NotRfc3339),
    }
}

pub(crate) fn parse_since(v: &Value) -> Result<i64, TimeError> {
    let t = v.as_str().unwrap_or("24h");
    let num = |suffix: char, ms: i64| {
        t.strip_suffix(suffix)
            .and_then(|n| n.parse::<i64>().ok())
            .map(|n| now_ms() - n * ms)
    };
    match num('m', 60_000).or_else(|| num('h', 3_600_000)).or_else(|| num('d', 86_400_000)) {
        Some(ms) => Ok(ms),
        None => parse_time(v).map(Option::unwrap_or_default),
    }
}
