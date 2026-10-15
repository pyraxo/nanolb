mod admin;
mod cli;
mod client;
mod codex;
mod endpoints;
mod home;
mod id;
mod mcp;
mod ops;
mod pricing;
mod proxy;
mod server;
mod service;
mod sse;
mod state;
mod store;
mod time;
mod translate;
mod tui;
mod ws;

pub use cli::run;
pub use server::router;
pub use state::App;
pub use store::Store;

static APP: std::sync::OnceLock<std::sync::Arc<state::App>> = std::sync::OnceLock::new();

pub(crate) fn app() -> Option<&'static std::sync::Arc<state::App>> {
    APP.get()
}
