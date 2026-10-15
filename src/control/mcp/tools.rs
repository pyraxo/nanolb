use rmcp::model::Tool;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

fn schema(properties: Value, required: &[&str]) -> Value {
    let mut o = Map::new();
    o.insert("type".into(), json!("object"));
    o.insert("properties".into(), properties);
    o.insert("required".into(), json!(required));
    Value::Object(o)
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

pub(super) static TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| {
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
            "account_add_openrouter",
            "Add an OpenRouter API key as an upstream account. Requests for vendor-prefixed models other than openai/ (e.g. z-ai/glm-5.3-flash, deepseek/deepseek-v4.1-flash) route to OpenRouter accounts.",
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
                    "strategy": {"type": "string", "enum": ["preserve", "burn", "round_robin", "priority", "latency"]},
                    "sticky": {"type": "boolean"},
                    "max_attempts": {"type": "integer", "description": "Accounts to try per request before failing"},
                    "models": {"type": "array", "items": {"type": "string"}, "description": "Models exposed to clients"},
                    "pricing": {"type": "object", "additionalProperties": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}, "description": "Model to [input, cached_input, output] USD per 1M tokens"}
                }),
                &[],
            ),
        ),
        ("models", "List the model ids served to clients.", none),
        (
            "reset_credits_list",
            "List the rate-limit reset credits banked on a Codex account: {credits: [{id, status, expires_at, ...}], available_count}.",
            id("Account id"),
        ),
        (
            "reset_credits_consume",
            "Spend one rate-limit reset credit on a Codex account to reset its usage windows now. Without credit_id the available credit that expires first is used. Returns the upstream result and the refreshed account.",
            schema(
                json!({"id": {"type": "string", "description": "Account id"}, "credit_id": {"type": "string"}}),
                &["id"],
            ),
        ),
        (
            "plan",
            "Quota planner. For each Codex account, projects when each rate-limit window will be exhausted from recent usage and recommends a daily warmup cron so that windows are staggered and reset at useful times during the working day. With apply=true the recommended cron is written to each account's warmup schedule.",
            schema(
                json!({
                    "work_start": {"type": "string", "description": "Start of the working day, HH:MM local time (default 09:00)"},
                    "lookback": {"type": "string", "description": "Recent-usage span for the burn rate, such as 2h or 6h (default 6h)"},
                    "apply": {"type": "boolean", "description": "Write the recommended warmup cron to the accounts"}
                }),
                &[],
            ),
        ),
        (
            "analytics",
            "Usage analytics from the request log. Views: series (time series per account, model or key), latency (p50 and p95 latency and TTFB), errors (error rate), cache (cached/input token ratio), top (top keys and models by tokens and cost), utilization (each account's share of tokens per bucket), all (default: every view).",
            schema(
                json!({
                    "since": {"type": "string", "description": "1h, 24h, 7d, 30d or an RFC3339 timestamp (default 7d)"},
                    "view": {"type": "string", "enum": ["series", "latency", "errors", "cache", "top", "utilization", "all"]},
                    "by": {"type": "string", "enum": ["account", "model", "key", "all"], "description": "Grouping dimension (default account)"},
                    "bucket": {"type": "string", "enum": ["hour", "day"], "description": "Time bucket (default day)"},
                    "limit": {"type": "integer", "description": "Rows for the top view (default 10)"}
                }),
                &[],
            ),
        ),
        (
            "sessions_list",
            "List sticky-session pins (conversation key, pinned account, last used), newest first. Filter by key substring or account id.",
            schema(
                json!({
                    "key": {"type": "string", "description": "Substring of the session key"},
                    "account": {"type": "string", "description": "Account id"},
                    "limit": {"type": "integer", "description": "Default 100"}
                }),
                &[],
            ),
        ),
        (
            "session_delete",
            "Delete one sticky-session pin so the conversation is routed fresh on its next request.",
            schema(json!({"key": {"type": "string"}}), &["key"]),
        ),
        (
            "sessions_purge",
            "Delete many sticky-session pins: all, those pinned to one account, or those not used for longer than older_than. At least one criterion is required.",
            schema(
                json!({
                    "all": {"type": "boolean"},
                    "account": {"type": "string", "description": "Account id"},
                    "older_than": {"type": "string", "description": "Age such as 30m, 12h or 7d"}
                }),
                &[],
            ),
        ),
    ];
    table
        .into_iter()
        .filter_map(|(name, description, schema)| match schema {
            Value::Object(o) => Some(Tool::new(name, description, o)),
            _ => None,
        })
        .collect()
});
