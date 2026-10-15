use super::App;
use super::account::Provider;
use parking_lot::RwLock;

const CODEX_CONTEXT: u64 = 272_000;
const OPENROUTER_CONTEXT: u64 = 1_000_000;

#[derive(Default)]
pub(crate) struct ModelCatalog {
    live: RwLock<Vec<(String, u64)>>,
}

impl ModelCatalog {
    pub(crate) fn set(&self, models: Vec<(String, u64)>) {
        *self.live.write() = models;
    }
}

pub(crate) fn canonical_model(model: &str) -> &str {
    model.strip_prefix("openai/").unwrap_or(model)
}

impl Provider {
    pub(crate) fn for_model(model: &str) -> Self {
        if model.contains('/') { Self::OpenRouter } else { Self::Codex }
    }
}

impl App {
    pub(crate) fn model_list(&self) -> Vec<(String, u64)> {
        let s = self.settings();
        let mut out = {
            let live = self.models.live.read();
            if live.is_empty() {
                s.models.iter().map(|m| (m.clone(), CODEX_CONTEXT)).collect()
            } else {
                live.clone()
            }
        };
        if self.accounts.any(Provider::OpenRouter) {
            out.extend(s.openrouter_models.iter().map(|m| (m.clone(), OPENROUTER_CONTEXT)));
        }
        out
    }
}
