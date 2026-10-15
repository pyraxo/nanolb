mod analytics;
mod usage;
mod writer;

pub(crate) use usage::{Endpoint, Usage, UsageMeta};

use parking_lot::Mutex;
use rusqlite::{Connection, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use writer::Writer;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    InvalidQuery(String),
}

pub struct Store {
    conn: Mutex<Connection>,
    writer: Writer,
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
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        let writer = Connection::open(path)?;
        writer.busy_timeout(Duration::from_secs(5))?;
        Ok(Self {
            conn: Mutex::new(conn),
            writer: Writer::spawn(writer),
        })
    }

    pub(crate) fn load<T: DeserializeOwned>(&self, tbl: &str) -> Result<Vec<T>, StoreError> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT json FROM kv WHERE tbl=?1")?;
        let rows = st
            .query_map([tbl], |r| r.get::<_, String>(0))?
            .filter_map(|s| serde_json::from_str(&s.ok()?).ok())
            .collect();
        Ok(rows)
    }

    pub(crate) fn load_raw(&self, tbl: &str) -> Result<Vec<(String, Value)>, StoreError> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT id, json FROM kv WHERE tbl=?1")?;
        let rows = st
            .query_map([tbl], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| {
                let (id, s) = r.ok()?;
                Some((id, serde_json::from_str(&s).ok()?))
            })
            .collect();
        Ok(rows)
    }

    pub(crate) fn put<T: Serialize>(&self, tbl: &str, id: &str, v: &T) -> Result<(), StoreError> {
        let json = serde_json::to_string(v)?;
        self.conn
            .lock()
            .execute("INSERT OR REPLACE INTO kv (tbl,id,json) VALUES (?1,?2,?3)", params![tbl, id, json])?;
        Ok(())
    }

    pub(crate) fn delete(&self, tbl: &str, id: &str) -> Result<(), StoreError> {
        self.conn
            .lock()
            .execute("DELETE FROM kv WHERE tbl=?1 AND id=?2", params![tbl, id])?;
        Ok(())
    }

    pub(crate) fn prune(&self, tbl: &str, field: &str, older_than: i64) -> Result<(), StoreError> {
        self.conn.lock().execute(
            &format!("DELETE FROM kv WHERE tbl=?1 AND json_extract(json, '$.{field}') < ?2"),
            params![tbl, older_than],
        )?;
        Ok(())
    }

    pub(crate) fn put_async<T: Serialize>(&self, tbl: &'static str, id: &str, v: &T) {
        self.writer.put_async(tbl, id, v);
    }

    pub(crate) fn delete_async(&self, tbl: &'static str, id: &str) {
        self.writer.delete_async(tbl, id);
    }

    pub(crate) fn record(&self, u: Usage) {
        self.writer.record(u);
    }
}
