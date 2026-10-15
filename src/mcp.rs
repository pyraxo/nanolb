use std::sync::{Arc, LazyLock};

use futures::future::BoxFuture;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
        ServerConfig, Tool,
    },
    service::RequestContext,
    transport::{
        stdio,
        streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager},
    },
};
use serde_json::{Value, json};

pub type Exec = Arc<dyn Fn(String, Value) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

fn limits() -> Value {
    json!({
        "type": "array",
        "description": "Usage caps. Each item is {window, max_tokens?, max_cost_usd?, max_requests?}; the key is blocked once any set max is reached in that window.",
        "items": {
            "type": "object",
            "properties": {
                "window": {"type": "string", "enum": ["day", "week", "month", "total"]},
                "max_tokens": {"type": "integer"},
                "max_cost_usd": {"type": "number"},
                "max_requests": {"type": "integer"}
            },
            "required": ["window"]
        }
    })
}

static TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| {
    let id = |what: &str| schema(json!({"id": {"type": "string", "description": what}}), &["id"]);
    let label = json!({"type": "string", "description": "Human-readable label"});
    let ids = json!({"type": "array", "items": {"type": "string"}});
    let none = schema(json!({}), &[]);
    let table: Vec<(&str, &str, Value)> = vec![
        (
            "status",
            "Show server status: version, uptime, routing strategy, sticky mode, accounts with live rate limits, key count, and requests and cost over the last 24h.",
            none.clone(),
        ),
        (
            "accounts_list",
            "List all upstream ChatGPT/Codex accounts with plan, enabled state, priority, availability, cooldown, in-flight count, rate-limit usage, last error and warmup schedule.",
            none.clone(),
        ),
        (
            "account_import",
            "Add an account from the contents of a Codex CLI ~/.codex/auth.json (pass the parsed object or the raw JSON string).",
            schema(
                json!({"auth_json": {"type": ["object", "string"], "description": "Contents of ~/.codex/auth.json"}, "label": label}),
                &["auth_json"],
            ),
        ),
        (
            "claude_login_start",
            "Start a Claude (Pro/Max subscription) OAuth login. Returns {login_id, authorize_url}. A human opens authorize_url, signs in, and copies the code shown; then call claude_login_finish.",
            schema(json!({"label": label}), &[]),
        ),
        (
            "claude_login_finish",
            "Finish a Claude login with the code the human copied from the Claude authorize page (format code#state is accepted). Adds the account.",
            schema(
                json!({"login_id": {"type": "string"}, "code": {"type": "string"}}),
                &["login_id", "code"],
            ),
        ),
        (
            "account_add_claude_token",
            "Add a Claude account from a long-lived `claude setup-token` token (sk-ant-oat...). Inference only; no refresh needed for one year.",
            schema(json!({"token": {"type": "string"}, "label": label}), &["token"]),
        ),
        (
            "account_add_openrouter",
            "Add an OpenRouter API key as an upstream account. Requests for vendor-prefixed models other than openai/ (e.g. z-ai/glm-5.3-flash, deepseek/deepseek-v4.1-flash, anthropic/claude-opus-5.5) route to OpenRouter accounts.",
            schema(json!({"api_key": {"type": "string"}, "label": label}), &["api_key"]),
        ),
        (
            "account_add_tokens",
            "Add an account from raw OAuth tokens.",
            schema(
                json!({"id_token": {"type": "string"}, "access_token": {"type": "string"}, "refresh_token": {"type": "string"}, "label": label}),
                &["id_token", "access_token", "refresh_token"],
            ),
        ),
        (
            "login_start",
            "Start an OpenAI device-code login. Returns a verification_url and user_code for the human to enter; the server polls in the background. Follow with login_poll.",
            schema(json!({"label": label}), &[]),
        ),
        (
            "login_poll",
            "Check a device-code login started with login_start. Returns status pending, done (with the new account) or error.",
            schema(json!({"login_id": {"type": "string"}}), &["login_id"]),
        ),
        (
            "account_update",
            "Update an account. enabled toggles routing; priority (lower is used first) matters for the priority strategy; warmup schedules a periodic tiny prompt that starts the account's rate-limit window, or null to remove it.",
            schema(
                json!({
                    "id": {"type": "string"},
                    "enabled": {"type": "boolean"},
                    "priority": {"type": "integer"},
                    "label": label,
                    "warmup": {
                        "type": ["object", "null"],
                        "description": "Cron schedule for the warmup prompt, or null to disable",
                        "properties": {
                            "cron": {"type": "string", "description": "Standard 5-field cron expression (minute hour day-of-month month day-of-week), e.g. \"0 4 * * *\" for daily at 04:00"},
                            "model": {"type": "string"},
                            "prompt": {"type": "string"}
                        },
                        "required": ["cron"]
                    }
                }),
                &["id"],
            ),
        ),
        ("account_remove", "Remove an account permanently.", id("Account id")),
        (
            "account_refresh",
            "Refresh an account's OAuth tokens and fetch its live usage and rate limits.",
            id("Account id"),
        ),
        (
            "account_warmup",
            "Fire the account's warmup prompt immediately. Returns {ok, status, error?}.",
            id("Account id"),
        ),
        (
            "keys_list",
            "List client API keys with limits, allowed accounts and models, and current day/week/month/total usage. Secrets are never shown.",
            none.clone(),
        ),
        (
            "key_create",
            "Create a client API key. The secret (nlb-...) is returned once in the \"key\" field; store it immediately. Optional accounts and models restrict what the key may use; limits cap spend.",
            schema(
                json!({
                    "name": {"type": "string"},
                    "accounts": {"type": "array", "items": {"type": "string"}, "description": "Account ids the key may use; omit for all"},
                    "models": {"type": "array", "items": {"type": "string"}, "description": "Model ids the key may use; omit for all"},
                    "limits": limits(),
                    "expires_at": {"type": "string", "description": "RFC3339 timestamp"}
                }),
                &["name"],
            ),
        ),
        (
            "key_update",
            "Update a client API key; only the given fields change.",
            schema(
                json!({
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "accounts": ids,
                    "models": {"type": "array", "items": {"type": "string"}},
                    "limits": limits(),
                    "enabled": {"type": "boolean"},
                    "expires_at": {"type": "string", "description": "RFC3339 timestamp"}
                }),
                &["id"],
            ),
        ),
        ("key_remove", "Delete a client API key.", id("Key id")),
        (
            "usage",
            "Aggregate usage (requests, tokens, cost, errors, average latency) over a time range, grouped as requested.",
            schema(
                json!({
                    "since": {"type": "string", "description": "1h, 24h, 7d, 30d or an RFC3339 timestamp (default 24h)"},
                    "group": {"type": "string", "enum": ["account", "key", "model", "hour", "day", "all"], "description": "Default all"}
                }),
                &[],
            ),
        ),
        (
            "requests",
            "List the most recent proxied requests with key, account, model, status, token counts, cost and latency.",
            schema(json!({"limit": {"type": "integer", "description": "Default 50"}}), &[]),
        ),
        (
            "settings_get",
            "Get load-balancer settings: strategy, sticky, max_attempts, models and pricing.",
            none.clone(),
        ),
        (
            "settings_update",
            "Update load-balancer settings; only the given fields change. strategy picks the account for a new conversation: preserve = least-used account first (spreads load); burn = account whose weekly window resets soonest first (uses quota before it resets); round_robin = rotate; priority = lowest account priority number first, falling through when limited. sticky (default on) keeps a conversation on one account to keep its prompt cache warm, moving only when that account is limited or failing.",
            schema(
                json!({
                    "strategy": {"type": "string", "enum": ["preserve", "burn", "round_robin", "priority"]},
                    "sticky": {"type": "boolean"},
                    "max_attempts": {"type": "integer", "description": "Accounts to try per request before failing"},
                    "models": {"type": "array", "items": {"type": "string"}, "description": "Models exposed to clients"},
                    "pricing": {"type": "object", "additionalProperties": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}, "description": "Model to [input, cached_input, output] USD per 1M tokens"}
                }),
                &[],
            ),
        ),
        ("models", "List the model ids served to clients.", none),
    ];
    table
        .into_iter()
        .map(|(name, description, schema)| {
            Tool::new(
                name,
                description,
                serde_json::from_value::<rmcp::model::JsonObject>(schema).expect("schema is an object"),
            )
        })
        .collect()
});

#[derive(Clone)]
struct Handler {
    exec: Exec,
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Admin tools for the nanolb LLM load balancer: manage accounts, API keys, routing settings and usage.")
    }

    async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(TOOLS.clone()))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _: RequestContext<RoleServer>) -> Result<CallToolResponse, ErrorData> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        let result = match (self.exec)(request.name.into_owned(), args).await {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(&value).unwrap_or_default())]),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        };
        Ok(result.into())
    }
}

pub async fn serve_stdio(exec: Exec) -> Result<(), String> {
    Handler { exec }
        .serve(stdio())
        .await
        .map_err(|e| e.to_string())?
        .waiting()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn http_service(exec: Exec) -> StreamableHttpService<impl ServerHandler, NeverSessionManager> {
    StreamableHttpService::new(
        move || Ok(Handler { exec: exec.clone() }),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .disable_allowed_hosts(),
    )
}
