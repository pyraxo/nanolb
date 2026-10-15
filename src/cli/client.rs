use crate::control::mcp::Backend;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClientError {
    #[error("cannot reach nanolb at {url}: {source}")]
    Unreachable { url: String, source: reqwest::Error },
    #[error("{0}")]
    Api(String),
    #[error("{0}")]
    Decode(reqwest::Error),
}

#[derive(Clone)]
pub(crate) struct Client {
    pub(crate) url: String,
    token: String,
    http: reqwest::Client,
}

impl Client {
    pub(crate) fn from_env() -> Self {
        let url = std::env::var("NANOLB_URL").unwrap_or_else(|_| "http://127.0.0.1:2455".into());
        let token = std::env::var("NANOLB_ADMIN_TOKEN")
            .ok()
            .or_else(|| std::fs::read_to_string(crate::home::admin_token_path()).ok())
            .unwrap_or_default()
            .trim()
            .to_owned();
        Self {
            url: url.trim_end_matches('/').to_owned(),
            token,
            http: reqwest::Client::new(),
        }
    }

    pub(crate) async fn call(&self, op: &str, args: Value) -> Result<Value, ClientError> {
        let res = self
            .http
            .post(format!("{}/admin/{op}", self.url))
            .bearer_auth(&self.token)
            .json(&args)
            .send()
            .await
            .map_err(|source| ClientError::Unreachable {
                url: self.url.clone(),
                source,
            })?;
        let status = res.status();
        let body: Value = res.json().await.map_err(ClientError::Decode)?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(ClientError::Api(
                body["error"]["message"].as_str().unwrap_or(status.as_str()).to_owned(),
            ))
        }
    }
}

impl Backend for Client {
    type Error = ClientError;

    async fn call(&self, op: String, args: Value) -> Result<Value, ClientError> {
        Self::call(self, &op, args).await
    }
}
