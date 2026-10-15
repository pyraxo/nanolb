use parking_lot::Mutex;
use rusqlite::{Connection, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::path::Path;
use std::sync::mpsc;

pub struct Store {
    conn: Mutex<Connection>,
    tx: mpsc::Sender<Write>,
}

enum Write {
    Usage(Usage),
    Put(&'static str, String, String),
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
    pub billing: &'static str,
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
";

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        let _ = conn.execute("ALTER TABLE usage ADD COLUMN billing TEXT NOT NULL DEFAULT ''", []);
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
            "billing" => "billing",
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

    pub fn recent(&self, limit: u32) -> Vec<Usage> {
        let c = self.conn.lock();
        let mut st = c
            .prepare(
                "SELECT ts,key_id,account_id,model,endpoint,status,input,cached,output,reasoning,cost,latency_ms,ttfb_ms,billing \
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
                billing: billing_name(&r.get::<_, String>(13)?),
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }
}

fn billing_name(s: &str) -> &'static str {
    match s {
        "plan" => "plan",
        "extra_usage" => "extra_usage",
        _ => "",
    }
}

fn endpoint_name(s: &str) -> &'static str {
    match s {
        "responses" => "responses",
        "chat" => "chat",
        "openrouter" => "openrouter",
        "anthropic" => "anthropic",
        "warmup" => "warmup",
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
                    "INSERT INTO usage (ts,key_id,account_id,model,endpoint,status,input,cached,output,reasoning,cost,latency_ms,ttfb_ms,billing) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                )?;
                let mut kv = tx.prepare_cached("INSERT OR REPLACE INTO kv (tbl,id,json) VALUES (?1,?2,?3)")?;
                for w in &batch {
                    let u = match w {
                        Write::Usage(u) => u,
                        Write::Put(t, id, json) => {
                            kv.execute(params![t, id, json])?;
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
                        u.ttfb_ms,
                        u.billing
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
