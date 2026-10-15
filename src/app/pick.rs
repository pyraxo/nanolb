use super::App;
use super::account::{Account, Provider, RateLimit};
use super::settings::Strategy;
use crate::time::now_ms;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

#[expect(clippy::cast_possible_truncation, reason = "a used percentage stays within 0 to 100")]
fn used_percent(r: &RateLimit, now: i64) -> i64 {
    r.used(now) as i64
}

impl App {
    pub(crate) fn pick(&self, provider: Provider, allowed: &[String], exclude: &[String], session: Option<&str>) -> Option<Arc<Account>> {
        let now = now_ms();
        let s = self.settings.load();
        let accs = self.accounts.read();
        let ok = |a: &&Arc<Account>| {
            a.provider() == provider && (allowed.is_empty() || allowed.contains(&a.id)) && !exclude.contains(&a.id) && a.available(now)
        };
        let session = session.filter(|_| s.sticky);
        if let Some(sess) = session {
            let pinned = self.sessions.pinned(sess);
            if let Some(a) = pinned.and_then(|id| accs.iter().find(|a| a.id == id).filter(ok).cloned()) {
                self.sessions.touch(sess, now);
                return Some(a);
            }
        }
        let cands: Vec<&Arc<Account>> = accs.iter().filter(ok).collect();
        let chosen = match s.strategy {
            Strategy::RoundRobin => cands.get(self.accounts.next_rr() % cands.len().max(1)).copied(),
            Strategy::Priority => cands
                .iter()
                .copied()
                .min_by_key(|a| (a.cfg.read().priority, a.inflight.load(Relaxed))),
            Strategy::Preserve => cands.iter().copied().min_by(|a, b| {
                let (ua, ub) = (a.rl.lock().used(now), b.rl.lock().used(now));
                ua.total_cmp(&ub).then(a.inflight.load(Relaxed).cmp(&b.inflight.load(Relaxed)))
            }),
            Strategy::Latency => cands
                .iter()
                .map(|a| (a.health.lock().score(now), a.inflight.load(Relaxed), *a))
                .min_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)))
                .map(|x| x.2),
            Strategy::Burn => cands.iter().copied().min_by_key(|a| {
                let r = a.rl.lock();
                (r.weekly_reset(now), -used_percent(&r, now), a.cfg.read().priority)
            }),
        }?
        .clone();
        drop(accs);
        if let Some(sess) = session {
            self.sessions.pin(&self.store, sess, &chosen.id, now);
        }
        Some(chosen)
    }
}

#[cfg(test)]
mod tests {
    use crate::app::account::{AccountCfg, Provider, RateLimit};
    use crate::app::settings::{Settings, Strategy};
    use crate::app::{App, LookupError};
    use crate::store::Store;
    use crate::time::now_ms;
    use std::sync::Arc;

    fn app_with(strategy: Strategy, accs: &[(&str, f64, i64)]) -> (Arc<App>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(Store::open(&dir.path().join("nanolb.db")).unwrap(), String::new()).unwrap();
        app.save_settings(Settings {
            strategy,
            ..Default::default()
        })
        .unwrap();
        let far = now_ms() + 86_400_000;
        for (id, used, reset_in) in accs {
            let a = app
                .accounts
                .upsert(
                    &app.store,
                    AccountCfg {
                        id: (*id).to_owned(),
                        kind: Provider::Codex.into(),
                        enabled: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            *a.rl.lock() = RateLimit {
                secondary_used: *used,
                secondary_window_min: 10_080,
                secondary_reset_at: far + reset_in,
                ..Default::default()
            };
        }
        (app, dir)
    }

    fn pick(app: &App, exclude: &[&str]) -> Option<String> {
        let exclude: Vec<String> = exclude.iter().map(|s| (*s).to_owned()).collect();
        app.pick(Provider::Codex, &[], &exclude, None).map(|a| a.id.clone())
    }

    #[test]
    fn preserve_picks_least_used_and_skips_exhausted() {
        let (app, _dir) = app_with(Strategy::Preserve, &[("a", 60.0, 0), ("b", 10.0, 0), ("c", 100.0, 0)]);
        assert_eq!(pick(&app, &[]).as_deref(), Some("b"));
        assert_eq!(pick(&app, &["b"]).as_deref(), Some("a"));
        assert!(pick(&app, &["a", "b"]).is_none());
    }

    #[test]
    fn latency_prefers_faster_healthy_account() {
        let (app, _dir) = app_with(Strategy::Latency, &[("a", 0.0, 0), ("b", 0.0, 0), ("c", 0.0, 0)]);
        for _ in 0..3 {
            app.accounts.get("a").unwrap().observe(Some(900));
            app.accounts.get("b").unwrap().observe(Some(80));
            app.accounts.get("c").unwrap().observe(Some(60));
        }
        app.accounts.get("c").unwrap().observe(None);
        assert_eq!(pick(&app, &[]).as_deref(), Some("b"));
    }

    #[test]
    fn burn_picks_soonest_weekly_reset() {
        let (app, _dir) = app_with(Strategy::Burn, &[("a", 10.0, 5_000), ("b", 90.0, 9_000), ("c", 50.0, 1_000)]);
        assert_eq!(pick(&app, &[]).as_deref(), Some("c"));
    }

    #[test]
    fn next_available_is_earliest_reset_only_when_every_account_is_limited() {
        let (app, _dir) = app_with(Strategy::Preserve, &[("a", 100.0, 5_000), ("b", 100.0, 1_000)]);
        let b_reset = app.accounts.get("b").unwrap().rl.lock().secondary_reset_at;
        assert_eq!(app.accounts.next_available(Provider::Codex, &[]), Some(b_reset));
        app.accounts.get("a").unwrap().rl.lock().secondary_used = 40.0;
        assert_eq!(app.accounts.next_available(Provider::Codex, &[]), None);
    }

    #[test]
    fn accounts_resolve_by_id_email_or_label_and_reject_ambiguous_names() {
        let (app, _dir) = app_with(Strategy::Preserve, &[("a", 0.0, 0), ("b", 0.0, 0)]);
        for (id, email, label) in [("a", "me@x.io", "Work"), ("b", "me@x.io", "personal")] {
            let acc = app.accounts.get(id).unwrap();
            let mut c = acc.cfg.write();
            c.email = email.into();
            c.label = label.into();
        }
        assert_eq!(app.accounts.resolve("b").unwrap().id, "b");
        assert_eq!(app.accounts.resolve("work").unwrap().id, "a");
        assert!(matches!(app.accounts.resolve("me@x.io"), Err(LookupError::Ambiguous { .. })));
        assert!(app.accounts.resolve("nobody").is_err());
    }
}
