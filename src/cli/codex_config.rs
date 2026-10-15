pub(super) fn codex_config(url: &str, key: Option<String>) {
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
    base_url: "{url}/v1"
    key_env: "NANOLB_API_KEY"
    api_mode: codex_responses
    model: gpt-6.1-sol
    discover_models: true
model:
  default: "gpt-6.1-sol"
  provider: "custom:nanolb""#
    );
}
