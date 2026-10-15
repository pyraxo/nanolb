#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod admin;
mod cli;
mod client;
mod codex;
mod mcp;
mod pricing;
mod proxy;
mod service;
mod sse;
mod state;
mod store;
mod translate;
mod tui;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use clap::Parser;
use sha2::Digest;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

static APP: OnceLock<Arc<state::App>> = OnceLock::new();

pub fn app() -> Option<&'static Arc<state::App>> {
    APP.get()
}

pub fn home() -> PathBuf {
    std::env::var_os("NANOLB_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".nanolb"))
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn ts_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(sha2::Sha256::digest(s.as_bytes()))
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("os rng");
    hex::encode(b)
}

fn admin_token() -> String {
    let path = home().join("admin_token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        return t.trim().to_string();
    }
    let t = format!("nlb-admin-{}", random_hex(24));
    std::fs::write(&path, &t).expect("write admin token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    t
}

pub fn router(app: Arc<state::App>) -> Router {
    let exec_app = app.clone();
    let exec: mcp::Exec = Arc::new(move |op, args| {
        let app = exec_app.clone();
        Box::pin(async move { admin::op_call(&app, &op, args).await })
    });
    let mcp = Router::new()
        .nest_service("/mcp", mcp::http_service(exec))
        .layer(axum::middleware::from_fn_with_state(app.clone(), admin::guard));
    let admin = Router::new()
        .route("/admin/{op}", get(admin::http).post(admin::http))
        .route_layer(axum::middleware::from_fn_with_state(app.clone(), admin::guard));
    Router::new()
        .route("/v1/responses", post(proxy::responses))
        .route("/backend-api/codex/responses", post(proxy::responses))
        .route("/backend-api/codex/models", get(proxy::codex_models))
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/api/v1/chat/completions", post(proxy::openrouter))
        .route("/v1/models", get(proxy::models))
        .route("/api/v1/models", get(proxy::openrouter_models))
        .route("/api/v1/key", get(proxy::openrouter_key))
        .route("/api/v1/auth/key", get(proxy::openrouter_key))
        .route("/v1/messages", post(proxy::messages))
        .route("/v1/messages/count_tokens", post(proxy::count_tokens))
        .route_layer(axum::middleware::from_fn_with_state(app.clone(), proxy::require_key))
        .merge(admin)
        .route("/health", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(256 << 20))
        .with_state(app)
        .merge(mcp)
}

async fn serve(host: String, port: u16) -> Result<(), String> {
    let dir = home();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let db = dir.join("nanolb.db");
    let store = store::Store::open(&db).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for (p, mode) in [(&dir, 0o700), (&db, 0o600)] {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode));
        }
    }
    let app = state::App::new(store, admin_token());
    let _ = APP.set(app.clone());
    codex::spawn_background(app.clone());
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|e| format!("bind {host}:{port}: {e}"))?;
    let pid_file = dir.join("nanolb.pid");
    std::fs::write(&pid_file, std::process::id().to_string()).map_err(|e| e.to_string())?;
    tracing::info!(
        addr = %format!("http://{host}:{port}"), accounts = app.accounts.read().len(), keys = app.keys.read().len(),
        home = %dir.display(), "nanolb listening"
    );
    let res = axum::serve(listener, router(app.clone()))
        .with_graceful_shutdown(async {
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string());
    app.persist_runtime();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = std::fs::remove_file(&pid_file);
    tracing::info!("nanolb stopped");
    res
}

#[tokio::main]
async fn main() {
    let args = cli::Cli::parse();
    let res = match args.cmd {
        cli::Cmd::Serve { host, port } => {
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
            serve(host, port).await
        }
        cli::Cmd::Mcp => {
            let client = Arc::new(client::Client::from_env());
            let exec: mcp::Exec = Arc::new(move |op, args| {
                let c = client.clone();
                Box::pin(async move { c.call(&op, args).await })
            });
            mcp::serve_stdio(exec).await
        }
        cmd => cli::run(cmd).await,
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
