use super::client::Client;
use crate::upstream::codex::CLIENT_ID;
use anyhow::anyhow;
use oauth2::{AuthUrl, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope, TokenUrl, basic::BasicClient};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::mpsc;

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

type CallbackTx = Arc<mpsc::Sender<HashMap<String, String>>>;

pub(super) async fn login_device(c: &Client, label: Option<String>) -> anyhow::Result<Value> {
    let start = c.call("login_start", json!({ "label": label })).await?;
    eprintln!(
        "Open {} and enter code {}",
        start["verification_url"].as_str().unwrap_or(""),
        start["user_code"].as_str().unwrap_or("")
    );
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let poll = c.call("login_poll", json!({ "login_id": start["login_id"] })).await?;
        match poll["status"].as_str() {
            Some("done") => return Ok(poll["account"].clone()),
            Some("error") => return Err(anyhow!("{}", poll["error"].as_str().unwrap_or("login failed"))),
            _ => {}
        }
    }
}

async fn callback(
    axum::extract::State(tx): axum::extract::State<CallbackTx>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> &'static str {
    let _ = tx.send(q).await;
    "Login complete. You can close this tab."
}

pub(super) async fn login_browser(c: &Client, label: Option<String>) -> anyhow::Result<Value> {
    let oauth = BasicClient::new(ClientId::new(CLIENT_ID.into()))
        .set_auth_uri(AuthUrl::new(AUTHORIZE_URL.into())?)
        .set_token_uri(TokenUrl::new(TOKEN_URL.into())?)
        .set_redirect_uri(RedirectUrl::new(REDIRECT_URI.into())?);
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut req = oauth.authorize_url(CsrfToken::new_random).set_pkce_challenge(challenge);
    for scope in ["openid", "profile", "email", "offline_access"] {
        req = req.add_scope(Scope::new(scope.into()));
    }
    let (url, state) = req
        .add_extra_param("id_token_add_organizations", "true")
        .add_extra_param("codex_cli_simplified_flow", "true")
        .add_extra_param("originator", "codex_cli_rs")
        .url();

    let (tx, mut rx) = mpsc::channel(1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:1455")
        .await
        .map_err(|e| anyhow!("cannot bind 127.0.0.1:1455: {e}"))?;
    let app = axum::Router::new()
        .route("/auth/callback", axum::routing::get(callback))
        .with_state(Arc::new(tx));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    eprintln!("Open {url}");
    let _ = open::that(url.as_str());
    let params = tokio::time::timeout(Duration::from_mins(5), rx.recv()).await;
    server.abort();
    let params = params
        .map_err(|_| anyhow!("login timed out"))?
        .ok_or_else(|| anyhow!("callback closed"))?;
    if params.get("state").map(String::as_str) != Some(state.secret().as_str()) {
        return Err(anyhow!("state mismatch"));
    }
    let code = params
        .get("code")
        .ok_or_else(|| anyhow!("{}", params.get("error").map_or("missing code", String::as_str)))?;

    let tokens: Value = reqwest::Client::new()
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", verifier.secret().as_str()),
        ])
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| anyhow!("token exchange failed: {e}"))?
        .json()
        .await?;
    Ok(c.call(
        "account_add_tokens",
        json!({
            "id_token": tokens["id_token"],
            "access_token": tokens["access_token"],
            "refresh_token": tokens["refresh_token"],
            "label": label,
        }),
    )
    .await?)
}
