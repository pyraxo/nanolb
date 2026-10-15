use super::{Store, StoreError};
use crate::time::ts_iso;
use rusqlite::types::Value as Sql;
use rusqlite::{Row, params};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

impl Store {
    fn percentiles(&self, g: &str, metric: &str, since_ms: i64) -> Result<HashMap<String, (i64, i64)>, StoreError> {
        let sql = format!(
            "SELECT g, MIN(CASE WHEN rn*100 >= c*50 THEN v END), MIN(CASE WHEN rn*100 >= c*95 THEN v END) FROM ( \
               SELECT g, v, ROW_NUMBER() OVER (PARTITION BY g ORDER BY v) AS rn, COUNT(*) OVER (PARTITION BY g) AS c FROM ( \
                 SELECT CAST({g} AS TEXT) AS g, {metric} AS v FROM usage WHERE ts>=?1 AND status<400)) \
             GROUP BY g"
        );
        let rows = self.query(&sql, [since_ms], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
        })?;
        Ok(rows.into_iter().collect())
    }

    fn rows(&self, sql: &str, since_ms: i64, limit: Option<i64>, cols: &[&str]) -> Result<Vec<Value>, StoreError> {
        let map = |r: &Row<'_>| {
            let mut o = Map::new();
            for (i, name) in cols.iter().enumerate() {
                let v = match r.get::<_, Sql>(i)? {
                    Sql::Integer(v) if *name == "bucket" => Value::from(ts_iso(v)),
                    Sql::Integer(v) => Value::from(v),
                    Sql::Real(v) => Value::from((v * 10_000.0).round() / 10_000.0),
                    Sql::Text(v) => Value::from(v),
                    _ => Value::Null,
                };
                o.insert((*name).into(), v);
            }
            Ok(Value::Object(o))
        };
        match limit {
            Some(l) => self.query(sql, params![since_ms, l], map),
            None => self.query(sql, params![since_ms], map),
        }
    }

    fn top(&self, col: &str, order: usize, since_ms: i64, limit: i64) -> Result<Vec<Value>, StoreError> {
        self.rows(
            &format!(
                "SELECT {col}, COUNT(*), SUM(input+output), SUM(cost) FROM usage WHERE ts>=?1 \
                 GROUP BY 1 ORDER BY {order} DESC LIMIT ?2"
            ),
            since_ms,
            Some(limit),
            &["id", "requests", "tokens", "cost_usd"],
        )
    }

    pub(crate) fn analytics(&self, view: &str, by: &str, bucket: &str, since_ms: i64, limit: i64) -> Result<Value, StoreError> {
        let g = match by {
            "account" => "account_id",
            "model" => "model",
            "key" => "key_id",
            "all" => "'all'",
            _ => return Err(StoreError::InvalidQuery(format!("unknown dimension `{by}`"))),
        };
        let b = match bucket {
            "hour" => "(ts/3600000)*3600000",
            "day" => "(ts/86400000)*86400000",
            _ => return Err(StoreError::InvalidQuery(format!("unknown bucket `{bucket}`"))),
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
            )?),
            "latency" => {
                let lat = self.percentiles(g, "latency_ms", since_ms)?;
                let ttfb = self.percentiles(g, "ttfb_ms", since_ms)?;
                let mut groups: Vec<(&String, &(i64, i64))> = lat.iter().collect();
                groups.sort_by_key(|(k, _)| *k);
                Value::from(
                    groups
                        .into_iter()
                        .map(|(k, (p50, p95))| {
                            let (t50, t95) = ttfb.get(k).copied().unwrap_or_default();
                            json!({"group": k, "latency_p50_ms": p50, "latency_p95_ms": p95, "ttfb_p50_ms": t50, "ttfb_p95_ms": t95})
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
            )?),
            "cache" => Value::from(self.rows(
                &format!(
                    "SELECT CAST({g} AS TEXT), SUM(input), SUM(cached), ROUND(SUM(cached)*1.0/MAX(SUM(input),1), 4) \
                     FROM usage WHERE ts>=?1 GROUP BY 1 ORDER BY 1"
                ),
                since_ms,
                None,
                &["group", "input_tokens", "cached_tokens", "cache_hit_ratio"],
            )?),
            "top" => json!({
                "keys_by_tokens": self.top("key_id", 3, since_ms, limit)?,
                "keys_by_cost": self.top("key_id", 4, since_ms, limit)?,
                "models_by_tokens": self.top("model", 3, since_ms, limit)?,
                "models_by_cost": self.top("model", 4, since_ms, limit)?,
            }),
            "utilization" => Value::from(self.rows(
                &format!(
                    "SELECT {b} AS bucket, account_id, COUNT(*), SUM(input+output), \
                     ROUND(SUM(input+output)*100.0/MAX(SUM(SUM(input+output)) OVER (PARTITION BY {b}), 1), 2) \
                     FROM usage WHERE ts>=?1 GROUP BY 1,2 ORDER BY 1,2"
                ),
                since_ms,
                None,
                &["bucket", "account", "requests", "tokens", "share_percent"],
            )?),
            _ => return Err(StoreError::InvalidQuery(format!("unknown view `{view}`"))),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::store::{Endpoint, Store, Usage};
    use crate::time::now_ms;

    #[test]
    fn analytics_latency_percentiles() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("nanolb.db")).unwrap();
        let now = now_ms();
        for i in 1..=100u64 {
            store.record(Usage {
                ts: now,
                account_id: "a".into(),
                model: "m".into(),
                endpoint: Endpoint::Responses,
                status: 200,
                input: 10,
                cached: 5,
                latency_ms: i * 10,
                ttfb_ms: i,
                ..Default::default()
            });
        }
        for _ in 0..200 {
            if store.recent(1000).unwrap().len() == 100 {
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
    }
}
