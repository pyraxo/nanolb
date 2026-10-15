use super::Usage;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::sync::mpsc;

enum Write {
    Usage(Usage),
    Put(&'static str, String, String),
    Del(&'static str, String),
}

#[derive(Clone)]
pub(super) struct Writer {
    tx: mpsc::Sender<Write>,
}

impl Writer {
    pub(super) fn spawn(conn: Connection) -> Self {
        let (tx, rx) = mpsc::channel::<Write>();
        std::thread::spawn(move || write_loop(&conn, &rx));
        Self { tx }
    }

    pub(super) fn put_async<T: Serialize>(&self, tbl: &'static str, id: &str, v: &T) {
        match serde_json::to_string(v) {
            Ok(json) => {
                let _ = self.tx.send(Write::Put(tbl, id.to_owned(), json));
            }
            Err(e) => tracing::error!(error = %e, tbl, "cannot serialize row"),
        }
    }

    pub(super) fn delete_async(&self, tbl: &'static str, id: &str) {
        let _ = self.tx.send(Write::Del(tbl, id.to_owned()));
    }

    pub(super) fn record(&self, u: Usage) {
        let _ = self.tx.send(Write::Usage(u));
    }
}

fn write_batch(conn: &Connection, batch: &[Write]) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut usage = tx.prepare_cached(
            "INSERT INTO usage (ts,key_id,account_id,model,endpoint,status,input,cached,output,reasoning,cost,latency_ms,ttfb_ms) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        )?;
        let mut kv = tx.prepare_cached("INSERT OR REPLACE INTO kv (tbl,id,json) VALUES (?1,?2,?3)")?;
        let mut del = tx.prepare_cached("DELETE FROM kv WHERE tbl=?1 AND id=?2")?;
        for w in batch {
            match w {
                Write::Put(t, id, json) => {
                    kv.execute(params![t, id, json])?;
                }
                Write::Del(t, id) => {
                    del.execute(params![t, id])?;
                }
                Write::Usage(u) => {
                    usage.execute(params![
                        u.ts,
                        u.key_id,
                        u.account_id,
                        u.model,
                        u.endpoint.as_str(),
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
        }
    }
    tx.commit()
}

fn write_loop(conn: &Connection, rx: &mpsc::Receiver<Write>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter().take(511));
        if let Err(e) = write_batch(conn, &batch) {
            tracing::error!(error = %e, rows = batch.len(), "sqlite write failed");
        }
    }
}
