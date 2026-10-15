pub(crate) mod account;
pub(crate) mod accounts;
pub(crate) mod files;
pub(crate) mod key;
pub(crate) mod keys;
pub(crate) mod logins;
pub(crate) mod models;
mod pick;
pub(crate) mod pricing;
pub(crate) mod sessions;
pub(crate) mod settings;

pub(crate) use account::{Account, AccountCfg, Inflight, Provider, RateLimit, Warmup};
pub(crate) use accounts::{Accounts, LookupError};
pub(crate) use files::FileOwners;
pub(crate) use key::{Key, KeyCfg, KeyRejected, Limit, Window};
pub(crate) use keys::Keys;
pub(crate) use logins::{LoginState, Logins};
pub(crate) use models::{ModelCatalog, canonical_model};
pub(crate) use sessions::{Pin, Sessions};
pub(crate) use settings::{Settings, Strategy};

use crate::store::{Store, StoreError, Usage};
use crate::time::now_ms;
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::Duration;

pub struct App {
    pub(crate) store: Store,
    settings: ArcSwap<Settings>,
    pub(crate) accounts: Accounts,
    pub(crate) keys: Keys,
    pub(crate) sessions: Sessions,
    pub(crate) files: FileOwners,
    pub(crate) logins: Logins,
    pub(crate) models: ModelCatalog,
    pub(crate) http: reqwest::Client,
    pub(crate) admin_token: String,
    pub(crate) started_at: i64,
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .tcp_nodelay(true)
        .pool_idle_timeout(Duration::from_secs(300))
        .pool_max_idle_per_host(64)
        .http2_adaptive_window(true)
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "cannot build the tuned http client, using the default");
            reqwest::Client::new()
        })
}

impl App {
    pub fn new(store: Store, admin_token: String) -> Result<Arc<Self>, StoreError> {
        let now = now_ms();
        let accounts = Accounts::load(&store)?;
        let sessions = Sessions::load(&store)?;
        let settings = store.load::<Settings>("settings")?.pop().unwrap_or_default();
        let keys = Keys::load(&store, now)?;
        let files = FileOwners::load(&store)?;
        Ok(Arc::new(Self {
            store,
            settings: ArcSwap::from_pointee(settings),
            accounts,
            keys,
            sessions,
            files,
            logins: Logins::default(),
            models: ModelCatalog::default(),
            http: http_client(),
            admin_token,
            started_at: now,
        }))
    }

    pub(crate) fn settings(&self) -> Arc<Settings> {
        self.settings.load_full()
    }

    pub(crate) fn save_settings(&self, s: Settings) -> Result<(), StoreError> {
        self.store.put("settings", "settings", &s)?;
        self.settings.store(Arc::new(s));
        Ok(())
    }

    pub(crate) fn persist_runtime(&self) {
        self.accounts.persist_runtime(&self.store);
    }

    pub(crate) fn record(&self, key: Option<&Key>, u: Usage) {
        tracing::info!(
            endpoint = u.endpoint.as_str(), key = %u.key_id, account = %u.account_id, model = %u.model, status = u.status,
            input = u.input, cached = u.cached, output = u.output, reasoning = u.reasoning, cost_usd = u.cost,
            latency_ms = u.latency_ms, ttfb_ms = u.ttfb_ms, "request"
        );
        if let Some(k) = key {
            k.add(u.ts, u.input + u.output, u.cost);
        }
        self.store.record(u);
    }
}
