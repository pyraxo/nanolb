use serde_json::Value;

pub struct Client {
    pub url: String,
    token: String,
    http: reqwest::Client,
}

impl Client {
    pub fn from_env() -> Self {
        let url = std::env::var("NANOLB_URL").unwrap_or_else(|_| "http://127.0.0.1:2455".into());
        let token = std::env::var("NANOLB_ADMIN_TOKEN")
            .ok()
            .or_else(|| std::fs::read_to_string(crate::home().join("admin_token")).ok())
            .unwrap_or_default()
            .trim()
            .to_string();
        Self {
            url: url.trim_end_matches('/').to_string(),
            token,
            http: reqwest::Client::new(),
        }
    }

    pub async fn call(&self, op: &str, args: Value) -> Result<Value, String> {
        let res = self
            .http
            .post(format!("{}/admin/{op}", self.url))
            .bearer_auth(&self.token)
            .json(&args)
            .send()
            .await
            .map_err(|e| format!("cannot reach nanolb at {}: {e}", self.url))?;
        let status = res.status();
        let body: Value = res.json().await.map_err(|e| e.to_string())?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(body["error"]["message"].as_str().unwrap_or(status.as_str()).to_string())
        }
    }
}
