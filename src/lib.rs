mod app;
mod cli;
mod control;
mod home;
mod id;
mod server;
mod sse;
mod store;
mod time;
mod translate;
mod upstream;

pub use app::App;
pub use cli::run;
pub use server::router;
pub use store::Store;

static APP: std::sync::OnceLock<std::sync::Arc<app::App>> = std::sync::OnceLock::new();

pub(crate) fn app() -> Option<&'static std::sync::Arc<app::App>> {
    APP.get()
}
