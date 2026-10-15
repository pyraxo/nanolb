pub(crate) mod auth;
pub(crate) mod error;

use crate::state::App;
use crate::{admin, endpoints, home, mcp, proxy, ws};
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use std::sync::Arc;
use std::time::Duration;

pub fn router(app: Arc<App>) -> Router {
    let exec_app = app.clone();
    let exec: mcp::Exec = Arc::new(move |op, args| {
        let app = exec_app.clone();
        Box::pin(async move { admin::op_call(&app, &op, args).await })
    });
    let mcp = Router::new()
        .nest_service("/mcp", mcp::http_service(exec))
        .layer(from_fn_with_state(app.clone(), auth::admin_guard));
    let admin = Router::new()
        .route("/admin/{op}", get(admin::http).post(admin::http))
        .route_layer(from_fn_with_state(app.clone(), auth::admin_guard));
    Router::new()
        .route("/v1/responses", post(proxy::responses).get(ws::upgrade))
        .route("/backend-api/codex/responses", post(proxy::responses).get(ws::upgrade))
        .route("/backend-api/codex/models", get(proxy::codex_models))
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/api/v1/chat/completions", post(proxy::openrouter))
        .route("/v1/models", get(proxy::models))
        .route("/api/v1/models", get(proxy::openrouter_models))
        .route("/api/v1/key", get(proxy::openrouter_key))
        .route("/api/v1/auth/key", get(proxy::openrouter_key))
        .route("/v1/messages", post(proxy::messages))
        .route("/v1/messages/count_tokens", post(proxy::count_tokens))
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
    let app = App::new(store, home::admin_token()?);
    let _ = crate::APP.set(app.clone());
    crate::codex::spawn_background(app.clone());
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|e| anyhow::anyhow!("bind {host}:{port}: {e}"))?;
    let pid_file = home::pid_path();
    std::fs::write(&pid_file, std::process::id().to_string())?;
    tracing::info!(
        addr = %format!("http://{host}:{port}"), accounts = app.accounts.read().len(), keys = app.keys.read().len(),
        home = %dir.display(), "nanolb listening"
    );
    let res = axum::serve(listener, router(app.clone())).with_graceful_shutdown(shutdown_signal()).await;
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
