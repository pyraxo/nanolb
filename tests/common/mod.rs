#![expect(clippy::expect_used, reason = "test harness")]

use axum::Router;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use nanolb::{App, Store, router};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

pub(crate) async fn spawn(router: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = l.local_addr().expect("local addr");
    tokio::spawn(async move { axum::serve(l, router).await.expect("serve") });
    format!("http://{addr}")
}

pub(crate) fn fake_jwt(account_id: &str, email: &str, exp_ms: i64) -> String {
    let claims = json!({
        "email": email,
        "exp": exp_ms / 1000,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id, "chatgpt_plan_type": "plus"},
    });
    format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

fn in_a_day_ms() -> i64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_millis();
    i64::try_from(now).expect("ms fit in i64") + 86_400_000
}

pub(crate) struct Harness {
    pub(crate) url: String,
    http: reqwest::Client,
    _home: TempDir,
}

impl Harness {
    pub(crate) async fn start(upstream_base: &str, mut settings: Value) -> Self {
        let home = tempfile::tempdir().expect("temp home");
        let store = Store::open(&home.path().join("nanolb.db")).expect("open store");
        let app = App::new(store, "admin".into()).expect("app");
        let h = Self {
            url: spawn(router(app)).await,
            http: reqwest::Client::new(),
            _home: home,
        };
        settings["upstream_base"] = json!(upstream_base);
        h.op("settings_update", settings).await;
        h
    }

    pub(crate) async fn op(&self, name: &str, args: Value) -> Value {
        let res = self
            .http
            .post(format!("{}/admin/{name}", self.url))
            .bearer_auth("admin")
            .json(&args)
            .send()
            .await
            .expect("admin call");
        let status = res.status();
        let body: Value = res.json().await.expect("admin json");
        assert!(status.is_success(), "{name} failed: {body}");
        body
    }

    pub(crate) async fn codex_account(&self, chatgpt_account_id: &str, priority: i64, enabled: bool) -> String {
        let jwt = fake_jwt(chatgpt_account_id, &format!("{chatgpt_account_id}@test"), in_a_day_ms());
        let acc = self
            .op(
                "account_add_tokens",
                json!({"id_token": jwt, "access_token": jwt, "refresh_token": "refresh"}),
            )
            .await;
        let id = acc["id"].as_str().expect("account id").to_owned();
        self.op("account_update", json!({"id": id, "priority": priority, "enabled": enabled}))
            .await;
        id
    }

    pub(crate) async fn key(&self) -> String {
        self.op("key_create", json!({"name": "test"})).await["key"]
            .as_str()
            .expect("key secret")
            .to_owned()
    }
}
