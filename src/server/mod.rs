pub(crate) mod auth;
pub(crate) mod endpoints;
pub(crate) mod error;
pub(crate) mod http;
pub(crate) mod ws;

use crate::app::App;
use crate::control::{admin, mcp};
use crate::home;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use std::sync::Arc;
use std::time::Duration;

pub fn router(app: Arc<App>) -> Router {
    let mcp = Router::new()
        .nest_service("/mcp", mcp::http_service(app.clone()))
        .layer(from_fn_with_state(app.clone(), auth::admin_guard));
    let admin = Router::new()
        .route("/admin/{op}", get(admin::http).post(admin::http))
        .route_layer(from_fn_with_state(app.clone(), auth::admin_guard));
    Router::new()
        .route("/v1/responses", post(http::responses).get(ws::upgrade))
        .route("/backend-api/codex/responses", post(http::responses).get(ws::upgrade))
        .route("/backend-api/codex/models", get(http::codex_models))
        .route("/v1/chat/completions", post(http::chat_completions))
        .route("/api/v1/chat/completions", post(http::openrouter))
        .route("/v1/models", get(http::models))
        .route("/api/v1/models", get(http::openrouter_models))
        .route("/api/v1/key", get(http::openrouter_key))
        .route("/api/v1/auth/key", get(http::openrouter_key))
        .route("/v1/messages", post(http::messages))
        .route("/v1/messages/count_tokens", post(http::count_tokens))
        .merge(endpoints::routes())
        .merge(ws::routes())
        .route_layer(from_fn_with_state(app.clone(), auth::require_key))
        .merge(admin)
        .route("/health", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(256 << 20))
        .with_state(app)
        .merge(mcp)
}

pub(crate) async fn serve(host: String, port: u16) -> anyhow::Result<()> {
    let dir = home::home();
    std::fs::create_dir_all(&dir)?;
    let db = dir.join("nanolb.db");
    let store = crate::store::Store::open(&db)?;
    home::secure_permissions(&dir, &db);
    let app = App::new(store, home::admin_token()?)?;
    crate::upstream::codex::spawn_background(app.clone());
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|e| anyhow::anyhow!("bind {host}:{port}: {e}"))?
        .tap_io(|tcp| {
            if let Err(e) = tcp.set_nodelay(true) {
                tracing::warn!(error = %e, "cannot set TCP_NODELAY on a client connection");
            }
        });
    let pid_file = home::pid_path();
    std::fs::write(&pid_file, std::process::id().to_string())?;
    tracing::info!(
        addr = %format!("http://{host}:{port}"), accounts = app.accounts.len(), keys = app.keys.len(),
        home = %dir.display(), "nanolb listening"
    );
    let res = axum::serve(listener, router(app.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    app.persist_runtime();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = std::fs::remove_file(&pid_file);
    tracing::info!("nanolb stopped");
    Ok(res?)
}

pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
