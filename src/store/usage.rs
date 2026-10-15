use super::{Store, StoreError};
use crate::app::pricing::Pricing;
use crate::time::{now_ms, ts_iso};
use rusqlite::params;
use rusqlite::types::Value as Sql;
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Responses,
    Chat,
    OpenRouter,
    Anthropic,
    Warmup,
    Files,
    Transcribe,
    Images,
    Embeddings,
    ResponsesWs,
    Realtime,
    RealtimeCall,
    #[default]
    Other,
}

impl Endpoint {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Chat => "chat",
            Self::OpenRouter => "openrouter",
            Self::Anthropic => "anthropic",
            Self::Warmup => "warmup",
            Self::Files => "files",
            Self::Transcribe => "transcribe",
            Self::Images => "images",
            Self::Embeddings => "embeddings",
            Self::ResponsesWs => "responses_ws",
            Self::Realtime => "realtime",
            Self::RealtimeCall => "realtime_call",
            Self::Other => "other",
        }
    }
}

impl FromStr for Endpoint {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "responses" => Self::Responses,
            "chat" => Self::Chat,
            "openrouter" => Self::OpenRouter,
            "anthropic" => Self::Anthropic,
            "warmup" => Self::Warmup,
            "files" => Self::Files,
            "transcribe" => Self::Transcribe,
            "images" => Self::Images,
            "embeddings" => Self::Embeddings,
            "responses_ws" => Self::ResponsesWs,
            "realtime" => Self::Realtime,
            "realtime_call" => Self::RealtimeCall,
            _ => Self::Other,
        })
    }
}

impl Serialize for Endpoint {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Usage {
    pub(crate) ts: i64,
    pub(crate) key_id: String,
    pub(crate) account_id: String,
    pub(crate) model: String,
    pub(crate) endpoint: Endpoint,
    pub(crate) status: u16,
    pub(crate) input: u64,
    pub(crate) cached: u64,
    pub(crate) output: u64,
    pub(crate) reasoning: u64,
    pub(crate) cost: f64,
    pub(crate) latency_ms: u64,
    pub(crate) ttfb_ms: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UsageMeta<'a> {
    pub(crate) key_id: &'a str,
    pub(crate) account_id: &'a str,
    pub(crate) model: &'a str,
    pub(crate) endpoint: Endpoint,
    pub(crate) started: i64,
    pub(crate) ttfb: i64,
}

fn non_negative(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

impl Usage {
    pub(crate) fn new(meta: UsageMeta<'_>, status: u16, usage: &Value, pricing: &Pricing) -> Self {
        let now = now_ms();
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        Self {
            ts: now,
            key_id: meta.key_id.to_owned(),
            account_id: meta.account_id.to_owned(),
            model: meta.model.to_owned(),
            endpoint: meta.endpoint,
            status,
            input: n(&usage["input_tokens"]),
            cached: n(&usage["input_tokens_details"]["cached_tokens"]),
            output: n(&usage["output_tokens"]),
            reasoning: n(&usage["output_tokens_details"]["reasoning_tokens"]),
            cost: usage["cost"].as_f64().unwrap_or_else(|| pricing.cost(meta.model, usage)),
            latency_ms: non_negative(now - meta.started),
            ttfb_ms: non_negative(meta.ttfb - meta.started),
        }
    }
}

pub(crate) struct KeyTotal {
    pub(crate) key_id: String,
    pub(crate) tokens: u64,
    pub(crate) cost: f64,
    pub(crate) requests: u64,
}

impl Store {
    pub(crate) fn recent(&self, limit: u32) -> Result<Vec<Usage>, StoreError> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT ts,key_id,account_id,model,endpoint,status,input,cached,output,reasoning,cost,latency_ms,ttfb_ms \
             FROM usage ORDER BY ts DESC LIMIT ?1",
        )?;
        let rows = st
            .query_map([limit], |r| {
                Ok(Usage {
                    ts: r.get(0)?,
                    key_id: r.get(1)?,
                    account_id: r.get(2)?,
                    model: r.get(3)?,
                    endpoint: r.get::<_, String>(4)?.parse().unwrap_or_default(),
                    status: r.get(5)?,
                    input: r.get(6)?,
                    cached: r.get(7)?,
                    output: r.get(8)?,
                    reasoning: r.get(9)?,
                    cost: r.get(10)?,
                    latency_ms: r.get(11)?,
                    ttfb_ms: r.get(12)?,
                })
            })?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    }

    pub(crate) fn key_totals_since(&self, since_ms: i64) -> Result<Vec<KeyTotal>, StoreError> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT key_id, SUM(input+output), SUM(cost), COUNT(*) FROM usage WHERE ts>=?1 GROUP BY key_id")?;
        let rows = st
            .query_map([since_ms], |r| {
                Ok(KeyTotal {
                    key_id: r.get(0)?,
                    tokens: non_negative(r.get(1)?),
                    cost: r.get(2)?,
                    requests: non_negative(r.get(3)?),
                })
            })?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    }

    pub(crate) fn usage_summary(&self, since_ms: i64, group: &str) -> Result<Value, StoreError> {
        let col = match group {
            "account" => "account_id",
            "key" => "key_id",
            "model" => "model",
            "hour" => "(ts/3600000)*3600000",
            "day" => "(ts/86400000)*86400000",
            _ => "'all'",
        };
        let sql = format!(
            "SELECT {col}, COUNT(*), SUM(input), SUM(cached), SUM(output), SUM(reasoning), SUM(cost), \
             SUM(status>=400), CAST(AVG(latency_ms) AS INTEGER), CAST(AVG(ttfb_ms) AS INTEGER), MAX(ts) \
             FROM usage WHERE ts>=?1 GROUP BY 1 ORDER BY 1"
        );
        let c = self.conn.lock();
        let mut st = c.prepare(&sql)?;
        let rows: Vec<Value> = st
            .query_map([since_ms], |r| {
                let group = match r.get::<_, Sql>(0)? {
                    Sql::Integer(i) => Value::from(ts_iso(i)),
                    Sql::Text(s) => Value::from(s),
                    _ => Value::Null,
                };
                Ok(json!({
                    "group": group, "requests": r.get::<_, i64>(1)?, "input_tokens": r.get::<_, i64>(2)?,
                    "cached_tokens": r.get::<_, i64>(3)?, "output_tokens": r.get::<_, i64>(4)?,
                    "reasoning_tokens": r.get::<_, i64>(5)?, "cost_usd": r.get::<_, f64>(6)?,
                    "errors": r.get::<_, i64>(7)?, "avg_latency_ms": r.get::<_, i64>(8)?,
                    "avg_ttfb_ms": r.get::<_, i64>(9)?, "last_at": ts_iso(r.get::<_, i64>(10)?),
                }))
            })?
            .filter_map(Result::ok)
            .collect();
        Ok(Value::from(rows))
    }

    pub(crate) fn account_tokens_since(&self, account_id: &str, since_ms: i64) -> u64 {
        self.conn
            .lock()
            .query_row(
                "SELECT COALESCE(SUM(input+output),0) FROM usage WHERE account_id=?1 AND ts>=?2",
                params![account_id, since_ms],
                |r| r.get::<_, i64>(0),
            )
            .map_or(0, non_negative)
    }
}
