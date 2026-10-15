use parking_lot::Mutex;
use rusqlite::{Connection, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;

pub struct Store {
    conn: Mutex<Connection>,
    tx: mpsc::Sender<Write>,
}

enum Write {
    Usage(Usage),
    Put(&'static str, String, String),
    Del(&'static str, String),
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Usage {
    pub ts: i64,
    pub key_id: String,
    pub account_id: String,
    pub model: String,
    pub endpoint: &'static str,
    pub status: u16,
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cost: f64,
    pub latency_ms: u64,
    pub ttfb_ms: u64,
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS kv (tbl TEXT NOT NULL, id TEXT NOT NULL, json TEXT NOT NULL, PRIMARY KEY (tbl, id));
CREATE TABLE IF NOT EXISTS usage (
  ts INTEGER NOT NULL, key_id TEXT NOT NULL, account_id TEXT NOT NULL, model TEXT NOT NULL,
  endpoint TEXT NOT NULL, status INTEGER NOT NULL, input INTEGER NOT NULL, cached INTEGER NOT NULL,
  output INTEGER NOT NULL, reasoning INTEGER NOT NULL, cost REAL NOT NULL, latency_ms INTEGER NOT NULL, ttfb_ms INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS usage_ts ON usage(ts);
CREATE INDEX IF NOT EXISTS usage_account_ts ON usage(account_id, ts);
";

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        let writer = Connection::open(path)?;
        writer.busy_timeout(std::time::Duration::from_secs(5))?;
        let (tx, rx) = mpsc::channel::<Write>();
        std::thread::spawn(move || write_loop(writer, rx));
        Ok(Self {
            conn: Mutex::new(conn),
            tx,
        })
    }

    pub fn load<T: DeserializeOwned>(&self, tbl: &str) -> Vec<T> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT json FROM kv WHERE tbl=?1").unwrap();
        st.query_map([tbl], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|s| serde_json::from_str(&s.ok()?).ok())
            .collect()
    }

    pub fn load_raw(&self, tbl: &str) -> Vec<(String, Value)> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT id, json FROM kv WHERE tbl=?1").unwrap();
        st.query_map([tbl], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .filter_map(|r| {
                let (id, s) = r.ok()?;
                Some((id, serde_json::from_str(&s).ok()?))
            })
            .collect()
    }

    pub fn put_async<T: Serialize>(&self, tbl: &'static str, id: &str, v: &T) {
        let _ = self.tx.send(Write::Put(tbl, id.to_string(), serde_json::to_string(v).unwrap()));
    }

    pub fn delete_async(&self, tbl: &'static str, id: &str) {
        let _ = self.tx.send(Write::Del(tbl, id.to_string()));
    }

    pub fn prune(&self, tbl: &str, field: &str, older_than: i64) {
        let _ = self.conn.lock().execute(
            &format!("DELETE FROM kv WHERE tbl=?1 AND json_extract(json, '$.{field}') < ?2"),
            params![tbl, older_than],
        );
    }

    pub fn put<T: Serialize>(&self, tbl: &str, id: &str, v: &T) {
        let json = serde_json::to_string(v).unwrap();
        self.conn
            .lock()
            .execute("INSERT OR REPLACE INTO kv (tbl,id,json) VALUES (?1,?2,?3)", params![tbl, id, json])
            .unwrap();
    }

    pub fn delete(&self, tbl: &str, id: &str) {
        self.conn
            .lock()
            .execute("DELETE FROM kv WHERE tbl=?1 AND id=?2", params![tbl, id])
            .unwrap();
    }

    pub fn record(&self, u: Usage) {
        let _ = self.tx.send(Write::Usage(u));
    }

    pub fn key_totals_since(&self, since_ms: i64) -> Vec<(String, u64, f64, u64)> {
        let c = self.conn.lock();
        let mut st = c
            .prepare("SELECT key_id, SUM(input+output), SUM(cost), COUNT(*) FROM usage WHERE ts>=?1 GROUP BY key_id")
            .unwrap();
        st.query_map([since_ms], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get(2)?, r.get::<_, i64>(3)? as u64))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn usage_summary(&self, since_ms: i64, group: &str) -> Value {
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
        let mut st = c.prepare(&sql).unwrap();
        let rows: Vec<Value> = st
            .query_map([since_ms], |r| {
                let g: rusqlite::types::Value = r.get(0)?;
                let group = match g {
                    rusqlite::types::Value::Integer(i) => Value::from(crate::ts_iso(i)),
                    rusqlite::types::Value::Text(s) => Value::from(s),
                    _ => Value::Null,
                };
                Ok(serde_json::json!({
                    "group": group, "requests": r.get::<_, i64>(1)?, "input_tokens": r.get::<_, i64>(2)?,
                    "cached_tokens": r.get::<_, i64>(3)?, "output_tokens": r.get::<_, i64>(4)?,
                    "reasoning_tokens": r.get::<_, i64>(5)?, "cost_usd": r.get::<_, f64>(6)?,
                    "errors": r.get::<_, i64>(7)?, "avg_latency_ms": r.get::<_, i64>(8)?,
                    "avg_ttfb_ms": r.get::<_, i64>(9)?, "last_at": crate::ts_iso(r.get::<_, i64>(10)?),
                }))
            })
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        Value::from(rows)
    }

    pub fn account_tokens_since(&self, account_id: &str, since_ms: i64) -> u64 {
        self.conn
            .lock()
            .query_row(
                "SELECT COALESCE(SUM(input+output),0) FROM usage WHERE account_id=?1 AND ts>=?2",
                params![account_id, since_ms],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64
    }

    fn percentiles(&self, g: &str, metric: &str, since_ms: i64) -> HashMap<String, (i64, i64)> {
        let sql = format!(
            "SELECT g, MIN(CASE WHEN rn*100 >= c*50 THEN v END), MIN(CASE WHEN rn*100 >= c*95 THEN v END) FROM ( \
               SELECT g, v, ROW_NUMBER() OVER (PARTITION BY g ORDER BY v) AS rn, COUNT(*) OVER (PARTITION BY g) AS c FROM ( \
                 SELECT CAST({g} AS TEXT) AS g, {metric} AS v FROM usage WHERE ts>=?1 AND status<400)) \
             GROUP BY g"
        );
        let c = self.conn.lock();
        let mut st = c.prepare(&sql).unwrap();
        st.query_map([since_ms], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    fn rows(&self, sql: &str, since_ms: i64, limit: Option<i64>, cols: &[&str]) -> Vec<Value> {
        let c = self.conn.lock();
        let mut st = c.prepare(sql).unwrap();
        let map = |r: &rusqlite::Row| {
            let mut o = serde_json::Map::new();
            for (i, name) in cols.iter().enumerate() {
                o.insert(
                    (*name).into(),
                    match r.get::<_, rusqlite::types::Value>(i)? {
                        rusqlite::types::Value::Integer(v) if *name == "bucket" => Value::from(crate::ts_iso(v)),
                        rusqlite::types::Value::Integer(v) => Value::from(v),
                        rusqlite::types::Value::Real(v) => Value::from((v * 10_000.0).round() / 10_000.0),
                        rusqlite::types::Value::Text(v) => Value::from(v),
                        _ => Value::Null,
                    },
                );
            }
            Ok(Value::Object(o))
        };
        let it = match limit {
            Some(l) => st.query_map(params![since_ms, l], map),
            None => st.query_map(params![since_ms], map),
        };
        it.unwrap().filter_map(Result::ok).collect()
    }

    pub fn analytics(&self, view: &str, by: &str, bucket: &str, since_ms: i64, limit: i64) -> Result<Value, String> {
        let g = match by {
            "account" => "account_id",
            "model" => "model",
            "key" => "key_id",
            "all" => "'all'",
            _ => return Err(format!("unknown dimension `{by}`")),
        };
        let b = match bucket {
            "hour" => "(ts/3600000)*3600000",
            "day" => "(ts/86400000)*86400000",
            _ => return Err(format!("unknown bucket `{bucket}`")),
        };
        Ok(match view {
            "series" => Value::from(self.rows(
                &format!(
                    "SELECT {b} AS bucket, CAST({g} AS TEXT) AS grp, COUNT(*), SUM(input), SUM(cached), SUM(output), SUM(cost), SUM(status>=400) \
                     FROM usage WHERE ts>=?1 GROUP BY 1,2 ORDER BY 1,2"
                ),
                since_ms,
                None,
                &["bucket", "group", "requests", "input_tokens", "cached_tokens", "output_tokens", "cost_usd", "errors"],
            )),
            "latency" => {
                let lat = self.percentiles(g, "latency_ms", since_ms);
                let ttfb = self.percentiles(g, "ttfb_ms", since_ms);
                let mut keys: Vec<&String> = lat.keys().collect();
                keys.sort();
                Value::from(
                    keys.into_iter()
                        .map(|k| {
                            let (p50, p95) = lat[k];
                            let (t50, t95) = ttfb.get(k).copied().unwrap_or_default();
                            serde_json::json!({"group": k, "latency_p50_ms": p50, "latency_p95_ms": p95, "ttfb_p50_ms": t50, "ttfb_p95_ms": t95})
                        })
                        .collect::<Vec<_>>(),
                )
            }
            "errors" => Value::from(self.rows(
                &format!(
                    "SELECT CAST({g} AS TEXT), COUNT(*), SUM(status>=400), ROUND(SUM(status>=400)*1.0/COUNT(*), 4) \
                     FROM usage WHERE ts>=?1 GROUP BY 1 ORDER BY 1"
                ),
                since_ms,
                None,
                &["group", "requests", "errors", "error_rate"],
            )),
            "cache" => Value::from(self.rows(
                &format!(
                    "SELECT CAST({g} AS TEXT), SUM(input), SUM(cached), ROUND(SUM(cached)*1.0/MAX(SUM(input),1), 4) \
                     FROM usage WHERE ts>=?1 GROUP BY 1 ORDER BY 1"
                ),
                since_ms,
                None,
                &["group", "input_tokens", "cached_tokens", "cache_hit_ratio"],
            )),
            "top" => {
                let top = |col: &str| {
                    self.rows(
                        &format!(
                            "SELECT {col}, COUNT(*), SUM(input+output), SUM(cost) FROM usage WHERE ts>=?1 \
                             GROUP BY 1 ORDER BY 3 DESC LIMIT ?2"
                        ),
                        since_ms,
                        Some(limit),
                        &["id", "requests", "tokens", "cost_usd"],
                    )
                };
                let top_cost = |col: &str| {
                    self.rows(
                        &format!(
                            "SELECT {col}, COUNT(*), SUM(input+output), SUM(cost) FROM usage WHERE ts>=?1 \
                             GROUP BY 1 ORDER BY 4 DESC LIMIT ?2"
                        ),
                        since_ms,
                        Some(limit),
                        &["id", "requests", "tokens", "cost_usd"],
                    )
                };
                serde_json::json!({
                    "keys_by_tokens": top("key_id"), "keys_by_cost": top_cost("key_id"),
                    "models_by_tokens": top("model"), "models_by_cost": top_cost("model"),
                })
            }
            "utilization" => Value::from(self.rows(
                &format!(
                    "SELECT {b} AS bucket, account_id, COUNT(*), SUM(input+output), \
                     ROUND(SUM(input+output)*100.0/MAX(SUM(SUM(input+output)) OVER (PARTITION BY {b}), 1), 2) \
                     FROM usage WHERE ts>=?1 GROUP BY 1,2 ORDER BY 1,2"
                ),
                since_ms,
                None,
                &["bucket", "account", "requests", "tokens", "share_percent"],
            )),
            _ => return Err(format!("unknown view `{view}`")),
        })
    }

    pub fn recent(&self, limit: u32) -> Vec<Usage> {
        let c = self.conn.lock();
        let mut st = c
            .prepare(
                "SELECT ts,key_id,account_id,model,endpoint,status,input,cached,output,reasoning,cost,latency_ms,ttfb_ms \
                 FROM usage ORDER BY ts DESC LIMIT ?1",
            )
            .unwrap();
        st.query_map([limit], |r| {
            Ok(Usage {
                ts: r.get(0)?,
                key_id: r.get(1)?,
                account_id: r.get(2)?,
                model: r.get(3)?,
                endpoint: endpoint_name(&r.get::<_, String>(4)?),
                status: r.get(5)?,
                input: r.get(6)?,
                cached: r.get(7)?,
                output: r.get(8)?,
                reasoning: r.get(9)?,
                cost: r.get(10)?,
                latency_ms: r.get(11)?,
                ttfb_ms: r.get(12)?,
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }
}

fn endpoint_name(s: &str) -> &'static str {
    match s {
        "responses" => "responses",
        "chat" => "chat",
        "openrouter" => "openrouter",
        "anthropic" => "anthropic",
        "warmup" => "warmup",
        "files" => "files",
        "transcribe" => "transcribe",
        "images" => "images",
        "embeddings" => "embeddings",
        "responses_ws" => "responses_ws",
        "realtime" => "realtime",
        "realtime_call" => "realtime_call",
        _ => "other",
    }
}

fn write_loop(conn: Connection, rx: mpsc::Receiver<Write>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter().take(511));
        let res = (|| -> rusqlite::Result<()> {
            let tx = conn.unchecked_transaction()?;
            {
                let mut st = tx.prepare_cached(
                    "INSERT INTO usage (ts,key_id,account_id,model,endpoint,status,input,cached,output,reasoning,cost,latency_ms,ttfb_ms) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                )?;
                let mut kv = tx.prepare_cached("INSERT OR REPLACE INTO kv (tbl,id,json) VALUES (?1,?2,?3)")?;
                let mut del = tx.prepare_cached("DELETE FROM kv WHERE tbl=?1 AND id=?2")?;
                for w in &batch {
                    let u = match w {
                        Write::Usage(u) => u,
                        Write::Put(t, id, json) => {
                            kv.execute(params![t, id, json])?;
                            continue;
                        }
                        Write::Del(t, id) => {
                            del.execute(params![t, id])?;
                            continue;
                        }
                    };
                    st.execute(params![
                        u.ts,
                        u.key_id,
                        u.account_id,
                        u.model,
                        u.endpoint,
                        u.status,
                        u.input,
                        u.cached,
                        u.output,
                        u.reasoning,
                        u.cost,
                        u.latency_ms,
                        u.ttfb_ms
                    ])?;
                }
            }
            tx.commit()
        })();
        if let Err(e) = res {
            tracing::error!(error = %e, rows = batch.len(), "sqlite write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analytics_latency_percentiles() {
        let path = std::env::temp_dir().join(format!("nanolb-test-{}.db", crate::random_hex(6)));
        let store = Store::open(&path).unwrap();
        let now = crate::now_ms();
        for i in 1..=100u64 {
            store.record(Usage {
                ts: now,
                account_id: "a".into(),
                model: "m".into(),
                endpoint: "responses",
                status: 200,
                input: 10,
                cached: 5,
                latency_ms: i * 10,
                ttfb_ms: i,
                ..Default::default()
            });
        }
        for _ in 0..200 {
            if store.recent(1000).len() == 100 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let v = store.analytics("latency", "account", "day", now - 1000, 10).unwrap();
        assert_eq!(v[0]["group"], "a");
        assert_eq!(v[0]["latency_p50_ms"], 500);
        assert_eq!(v[0]["latency_p95_ms"], 950);
        assert_eq!(v[0]["ttfb_p95_ms"], 95);
        let c = store.analytics("cache", "all", "day", now - 1000, 10).unwrap();
        assert_eq!(c[0]["cache_hit_ratio"], 0.5);
        let _ = std::fs::remove_file(&path);
    }
}
