use super::pricing::{DEFAULT_MODELS, Pricing};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    pub(crate) strategy: Strategy,
    pub(crate) sticky: bool,
    pub(crate) max_attempts: u32,
    pub(crate) models: Vec<String>,
    pub(crate) pricing: Pricing,
    pub(crate) upstream_base: String,
    pub(crate) codex_version: String,
    pub(crate) usage_poll_secs: u64,
    pub(crate) warmup_model: String,
    pub(crate) openrouter_base: String,
    pub(crate) openrouter_models: Vec<String>,
    pub(crate) first_event_timeout_secs: u64,
    pub(crate) idle_timeout_secs: u64,
    pub(crate) keepalive_secs: u64,
    pub(crate) compress_upstream: bool,
    pub(crate) realtime_base: String,
    pub(crate) prewarm: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Strategy {
    Preserve,
    Burn,
    RoundRobin,
    Priority,
    Latency,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            strategy: Strategy::Preserve,
            sticky: true,
            max_attempts: 4,
            models: DEFAULT_MODELS.iter().map(|s| (*s).to_owned()).collect(),
            pricing: Pricing::defaults(),
            upstream_base: "https://chatgpt.com/backend-api".into(),
            codex_version: "0.160.0".into(),
            usage_poll_secs: 60,
            warmup_model: "gpt-6-luna".into(),
            first_event_timeout_secs: 90,
            idle_timeout_secs: 600,
            openrouter_base: "https://openrouter.ai/api/v1".into(),
            openrouter_models: ["z-ai/glm-5.3-flash", "deepseek/deepseek-v4.1-flash"]
                .into_iter()
                .map(String::from)
                .collect(),
            keepalive_secs: 15,
            compress_upstream: true,
            realtime_base: "https://api.openai.com/v1".into(),
            prewarm: false,
        }
    }
}
