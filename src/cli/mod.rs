pub(crate) mod client;
mod service;
mod tui;

use client::Client;
use clap::{Args, Parser, Subcommand};
use oauth2::{AuthUrl, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope, TokenUrl, basic::BasicClient};
use serde_json::{Map, Value, json};
use std::{collections::HashMap, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};
use tokio::sync::mpsc;

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

#[derive(Parser)]
#[command(name = "nanolb", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    Serve {
        #[arg(long, env = "NANOLB_HOST", default_value = "127.0.0.1")]
        host: String,
        #[arg(long, env = "NANOLB_PORT", default_value_t = 2455)]
        port: u16,
    },
    Mcp,
    Up {
        #[arg(long, env = "NANOLB_HOST", default_value = "127.0.0.1")]
        host: String,
        #[arg(long, env = "NANOLB_PORT", default_value_t = 2455)]
        port: u16,
    },
    Down,
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    Status,
    Login {
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        browser: bool,
    },
    Accounts {
        #[command(subcommand)]
        cmd: AccountsCmd,
    },
    Keys {
        #[command(subcommand)]
        cmd: KeysCmd,
    },
    Usage {
        #[arg(long, default_value = "24h")]
        since: String,
        #[arg(long)]
        group: Option<String>,
    },
    Requests {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    Settings {
        #[command(subcommand)]
        cmd: SettingsCmd,
    },
    Models,
    Sessions {
        #[command(subcommand)]
        cmd: SessionsCmd,
    },
    ResetCredits {
        #[command(subcommand)]
        cmd: ResetCreditsCmd,
    },
    Plan {
        #[arg(long)]
        work_start: Option<String>,
        #[arg(long)]
        lookback: Option<String>,
        #[arg(long)]
        apply: bool,
    },
    Analytics {
        #[arg(long, default_value = "7d")]
        since: String,
        #[arg(long, default_value = "all")]
        view: String,
        #[arg(long, default_value = "account")]
        by: String,
        #[arg(long, default_value = "day")]
        bucket: String,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    Call {
        op: String,
        json: Option<String>,
    },
    CodexConfig {
        #[arg(long)]
        key: Option<String>,
    },
    Top {
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
}

#[derive(Subcommand)]
pub enum ServiceCmd {
    Install {
        #[arg(long, env = "NANOLB_HOST", default_value = "127.0.0.1")]
        host: String,
        #[arg(long, env = "NANOLB_PORT", default_value_t = 2455)]
        port: u16,
    },
    Uninstall {
        #[arg(long)]
        purge: bool,
    },
}

#[derive(Subcommand)]
pub enum AccountsCmd {
    Ls,
    Import {
        path: Option<PathBuf>,
        #[arg(long)]
        label: Option<String>,
    },
    AddOpenrouter {
        api_key: String,
        #[arg(long)]
        label: Option<String>,
    },
    Rm {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Priority {
        id: String,
        n: i64,
    },
    Refresh {
        id: String,
    },
    Warmup {
        id: String,
    },
    SetWarmup {
        id: String,
        cron: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
    },
    ClearWarmup {
        id: String,
    },
}

#[derive(Args)]
pub struct KeyOpts {
    #[arg(long = "account")]
    accounts: Vec<String>,
    #[arg(long = "model")]
    models: Vec<String>,
    #[arg(long = "limit")]
    limits: Vec<String>,
    #[arg(long)]
    expires: Option<String>,
}

#[derive(Subcommand)]
pub enum KeysCmd {
    Ls,
    Create {
        name: String,
        #[command(flatten)]
        opts: KeyOpts,
    },
    Rm {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Update {
        id: String,
        #[command(flatten)]
        opts: KeyOpts,
    },
}

#[derive(Subcommand)]
pub enum ResetCreditsCmd {
    Ls {
        account: String,
    },
    Consume {
        account: String,
        #[arg(long)]
        credit: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum SessionsCmd {
    Ls {
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    Rm {
        key: String,
    },
    Purge {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        older_than: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum SettingsCmd {
    Get,
    Set { key: String, value: String },
}

fn id(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| json!(s))
}

fn parse_limit(s: &str) -> Result<Value, String> {
    let bad = || format!("invalid limit '{s}', expected WINDOW:KIND:VALUE");
    let [window, kind, value] = s.split(':').collect::<Vec<_>>()[..] else {
        return Err(bad());
    };
    let (field, value) = match kind {
        "tokens" => ("max_tokens", json!(value.parse::<u64>().map_err(|_| bad())?)),
        "requests" => ("max_requests", json!(value.parse::<u64>().map_err(|_| bad())?)),
        "usd" => ("max_cost_usd", json!(value.parse::<f64>().map_err(|_| bad())?)),
        _ => return Err(bad()),
    };
    Ok(json!({ "window": window, field: value }))
}

impl KeyOpts {
    fn fill(self, args: &mut Map<String, Value>) -> Result<(), String> {
        if !self.accounts.is_empty() {
            args.insert("accounts".into(), self.accounts.iter().map(|a| id(a)).collect());
        }
        if !self.models.is_empty() {
            args.insert("models".into(), json!(self.models));
        }
        if !self.limits.is_empty() {
            let limits = self.limits.iter().map(|l| parse_limit(l)).collect::<Result<Vec<_>, _>>()?;
            args.insert("limits".into(), json!(limits));
        }
        if let Some(e) = self.expires {
            args.insert("expires_at".into(), json!(e));
        }
        Ok(())
    }
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

async fn show(c: &Client, op: &str, args: Value) -> Result<(), String> {
    print(&c.call(op, args).await?);
    Ok(())
}

pub async fn run() -> ExitCode {
    let args = Cli::parse();
    let res = match args.cmd {
        Cmd::Serve { host, port } => {
            init_tracing();
            crate::server::serve(host, port).await.map_err(|e| e.to_string())
        }
        Cmd::Mcp => {
            let client = Arc::new(Client::from_env());
            let exec: crate::control::mcp::Exec = Arc::new(move |op, args| {
                let c = client.clone();
                Box::pin(async move { c.call(&op, args).await })
            });
            crate::control::mcp::serve_stdio(exec).await
        }
        cmd => dispatch(cmd).await,
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("NANOLB_LOG").unwrap_or_else(|_| "info".into());
    let fmt = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);
    if std::env::var("NANOLB_LOG_FORMAT").is_ok_and(|f| f == "json") {
        fmt.json().init();
    } else {
        fmt.init();
    }
}

async fn dispatch(cmd: Cmd) -> Result<(), String> {
    let c = Client::from_env();
    match cmd {
        Cmd::Serve { .. } | Cmd::Mcp => Ok(()),
        Cmd::Up { host, port } => up(&c, host, port).await,
        Cmd::Down => down().await,
        Cmd::Service { cmd } => service(cmd).await,
        Cmd::Status => show(&c, "status", json!({})).await,
        Cmd::Login { label, browser } => {
            let account = if browser {
                login_browser(&c, label).await?
            } else {
                login_device(&c, label).await?
            };
            print(&account);
            Ok(())
        }
        Cmd::Accounts { cmd } => accounts(&c, cmd).await,
        Cmd::Keys { cmd } => keys(&c, cmd).await,
        Cmd::Usage { since, group } => {
            let mut args = json!({ "since": since });
            if let Some(g) = group {
                args["group"] = json!(g);
            }
            show(&c, "usage", args).await
        }
        Cmd::Requests { limit } => show(&c, "requests", json!({ "limit": limit })).await,
        Cmd::Settings { cmd: SettingsCmd::Get } => show(&c, "settings_get", json!({})).await,
        Cmd::Settings {
            cmd: SettingsCmd::Set { key, value },
        } => {
            let value = serde_json::from_str(&value).unwrap_or(json!(value));
            show(&c, "settings_update", json!({ key: value })).await
        }
        Cmd::Models => show(&c, "models", json!({})).await,
        Cmd::ResetCredits { cmd } => match cmd {
            ResetCreditsCmd::Ls { account } => show(&c, "reset_credits_list", json!({ "id": id(&account) })).await,
            ResetCreditsCmd::Consume { account, credit } => {
                show(&c, "reset_credits_consume", json!({ "id": id(&account), "credit_id": credit })).await
            }
        },
        Cmd::Plan {
            work_start,
            lookback,
            apply,
        } => {
            show(
                &c,
                "plan",
                json!({ "work_start": work_start, "lookback": lookback, "apply": apply }),
            )
            .await
        }
        Cmd::Analytics {
            since,
            view,
            by,
            bucket,
            limit,
        } => {
            show(
                &c,
                "analytics",
                json!({ "since": since, "view": view, "by": by, "bucket": bucket, "limit": limit }),
            )
            .await
        }
        Cmd::Sessions { cmd } => match cmd {
            SessionsCmd::Ls { key, account, limit } => {
                show(&c, "sessions_list", json!({ "key": key, "account": account, "limit": limit })).await
            }
            SessionsCmd::Rm { key } => show(&c, "session_delete", json!({ "key": key })).await,
            SessionsCmd::Purge { all, account, older_than } => {
                show(
                    &c,
                    "sessions_purge",
                    json!({ "all": all, "account": account, "older_than": older_than }),
                )
                .await
            }
        },
        Cmd::Call { op, json } => {
            let args = match json {
                Some(j) => serde_json::from_str(&j).map_err(|e| format!("invalid JSON: {e}"))?,
                None => json!({}),
            };
            show(&c, &op, args).await
        }
        Cmd::CodexConfig { key } => {
            codex_config(&c.url, key);
            Ok(())
        }
        Cmd::Top { interval } => crate::cli::tui::run(c, interval).await,
    }
}

async fn accounts(c: &Client, cmd: AccountsCmd) -> Result<(), String> {
    match cmd {
        AccountsCmd::Ls => show(c, "accounts_list", json!({})).await,
        AccountsCmd::Import { path, label } => {
            let path = path.unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex/auth.json"));
            let auth_json = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            show(c, "account_import", json!({ "auth_json": auth_json, "label": label })).await
        }
        AccountsCmd::AddOpenrouter { api_key, label } => {
            show(c, "account_add_openrouter", json!({ "api_key": api_key, "label": label })).await
        }
        AccountsCmd::Rm { id: i } => show(c, "account_remove", json!({ "id": id(&i) })).await,
        AccountsCmd::Enable { id: i } => show(c, "account_update", json!({ "id": id(&i), "enabled": true })).await,
        AccountsCmd::Disable { id: i } => show(c, "account_update", json!({ "id": id(&i), "enabled": false })).await,
        AccountsCmd::Priority { id: i, n } => show(c, "account_update", json!({ "id": id(&i), "priority": n })).await,
        AccountsCmd::Refresh { id: i } => show(c, "account_refresh", json!({ "id": id(&i) })).await,
        AccountsCmd::Warmup { id: i } => show(c, "account_warmup", json!({ "id": id(&i) })).await,
        AccountsCmd::SetWarmup {
            id: i,
            cron,
            model,
            prompt,
        } => {
            let warmup = json!({ "cron": cron, "model": model, "prompt": prompt });
            show(c, "account_update", json!({ "id": id(&i), "warmup": warmup })).await
        }
        AccountsCmd::ClearWarmup { id: i } => show(c, "account_update", json!({ "id": id(&i), "warmup": null })).await,
    }
}

async fn keys(c: &Client, cmd: KeysCmd) -> Result<(), String> {
    match cmd {
        KeysCmd::Ls => show(c, "keys_list", json!({})).await,
        KeysCmd::Create { name, opts } => {
            let mut args = Map::new();
            args.insert("name".into(), json!(name));
            opts.fill(&mut args)?;
            show(c, "key_create", Value::Object(args)).await
        }
        KeysCmd::Rm { id: i } => show(c, "key_remove", json!({ "id": id(&i) })).await,
        KeysCmd::Enable { id: i } => show(c, "key_update", json!({ "id": id(&i), "enabled": true })).await,
        KeysCmd::Disable { id: i } => show(c, "key_update", json!({ "id": id(&i), "enabled": false })).await,
        KeysCmd::Update { id: i, opts } => {
            let mut args = Map::new();
            args.insert("id".into(), id(&i));
            opts.fill(&mut args)?;
            show(c, "key_update", Value::Object(args)).await
        }
    }
}

async fn login_device(c: &Client, label: Option<String>) -> Result<Value, String> {
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
            Some("error") => return Err(poll["error"].as_str().unwrap_or("login failed").to_string()),
            _ => {}
        }
    }
}

fn running_pid() -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(crate::home::pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()?;
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?
        .success()
        .then_some(pid)
}

async fn up(c: &Client, host: String, port: u16) -> Result<(), String> {
    if let Some(pid) = running_pid() {
        print(&json!({ "status": "already running", "pid": pid, "url": c.url }));
        return Ok(());
    }
    let home = crate::home::home();
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let log_path = home.join("nanolb.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["serve", "--host", &host, "--port", &port.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let child = cmd.spawn().map_err(|e| e.to_string())?;
    let url = format!("http://{host}:{port}");
    let http = reqwest::Client::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if http
            .get(format!("{url}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            print(&json!({ "status": "up", "pid": child.id(), "url": url, "log": log_path }));
            return Ok(());
        }
    }
    Err(format!("nanolb did not become healthy within 5s; see {}", log_path.display()))
}

async fn service(cmd: ServiceCmd) -> Result<(), String> {
    match cmd {
        ServiceCmd::Install { host, port } => {
            if running_pid().is_some() {
                down().await?;
            }
            let res = crate::cli::service::install(&host, port)?;
            let http = reqwest::Client::new();
            for _ in 0..50 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if http
                    .get(format!("http://{host}:{port}/health"))
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success())
                {
                    print(&res);
                    return Ok(());
                }
            }
            Err(format!("service installed but not healthy within 5s: {res}"))
        }
        ServiceCmd::Uninstall { purge } => {
            print(&crate::cli::service::uninstall(purge)?);
            Ok(())
        }
    }
}

async fn down() -> Result<(), String> {
    let Some(pid) = running_pid() else {
        print(&json!({ "status": "not running" }));
        return Ok(());
    };
    std::process::Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .map_err(|e| e.to_string())?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if running_pid().is_none() {
            print(&json!({ "status": "down", "pid": pid }));
            return Ok(());
        }
    }
    Err(format!("nanolb (pid {pid}) did not stop within 10s"))
}

type CallbackTx = Arc<mpsc::Sender<HashMap<String, String>>>;

async fn callback(
    axum::extract::State(tx): axum::extract::State<CallbackTx>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> &'static str {
    let _ = tx.send(q).await;
    "Login complete. You can close this tab."
}

async fn login_browser(c: &Client, label: Option<String>) -> Result<Value, String> {
    let oauth = BasicClient::new(ClientId::new(CLIENT_ID.into()))
        .set_auth_uri(AuthUrl::new(AUTHORIZE_URL.into()).map_err(|e| e.to_string())?)
        .set_token_uri(TokenUrl::new(TOKEN_URL.into()).map_err(|e| e.to_string())?)
        .set_redirect_uri(RedirectUrl::new(REDIRECT_URI.into()).map_err(|e| e.to_string())?);
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
        .map_err(|e| format!("cannot bind 127.0.0.1:1455: {e}"))?;
    let app = axum::Router::new()
        .route("/auth/callback", axum::routing::get(callback))
        .with_state(Arc::new(tx));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    eprintln!("Open {url}");
    let _ = open::that(url.as_str());
    let params = tokio::time::timeout(Duration::from_secs(300), rx.recv()).await;
    server.abort();
    let params = params.map_err(|_| "login timed out".to_string())?.ok_or("callback closed")?;
    if params.get("state").map(String::as_str) != Some(state.secret().as_str()) {
        return Err("state mismatch".into());
    }
    let code = params
        .get("code")
        .ok_or_else(|| params.get("error").cloned().unwrap_or("missing code".into()))?;

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
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("token exchange failed: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    c.call(
        "account_add_tokens",
        json!({
            "id_token": tokens["id_token"],
            "access_token": tokens["access_token"],
            "refresh_token": tokens["refresh_token"],
            "label": label,
        }),
    )
    .await
}

fn codex_config(url: &str, key: Option<String>) {
    let key = key.unwrap_or_else(|| std::env::var("NANOLB_API_KEY").unwrap_or_else(|_| "nlb-...".into()));
    println!(
        r#"# ~/.codex/config.toml
model_provider = "nanolb"

[model_providers.nanolb]
name = "nanolb"
base_url = "{url}/backend-api/codex"
wire_api = "responses"
env_key = "NANOLB_API_KEY"
supports_websockets = true

[model_providers.nanolb.capabilities]
remote_compaction = "v2"

export NANOLB_API_KEY={key}

# Claude Code (Anthropic format, served by your Codex accounts)
export ANTHROPIC_BASE_URL={url}
export ANTHROPIC_AUTH_TOKEN={key}
export ANTHROPIC_MODEL=gpt-6.1-sol

# OpenRouter-compatible base URL
{url}/api/v1

# OpenAI-compatible base URL
{url}/v1

# Hermes Agent: ~/.hermes/.env
NANOLB_API_KEY={key}

# Hermes Agent: ~/.hermes/config.yaml
providers:
  nanolb:
    api: {url}/v1
    key_env: NANOLB_API_KEY
    transport: codex_responses
    default_model: gpt-6.1-sol
    discover_models: true
model:
  default: gpt-6.1-sol
  provider: custom:nanolb"#
    );
}
