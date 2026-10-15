# nanolb

nanolb is a small, fast load balancer for Codex (ChatGPT) accounts and OpenRouter keys. It is one Rust binary with a SQLite store. It has no web GUI: you control it with the CLI, an admin RPC API and MCP.

AI agents: read [AGENTS.md](AGENTS.md). It has the full setup and operation guide, the admin op list and a troubleshooting table.

nanolb accepts four client API formats and sends each request to an upstream account:

| Client format | Endpoint |
|---|---|
| Codex CLI / OpenAI Responses | `POST /backend-api/codex/responses`, `POST /v1/responses`, `GET /backend-api/codex/models` |
| OpenAI Chat Completions | `POST /v1/chat/completions`, `GET /v1/models` |
| OpenRouter | `POST /api/v1/chat/completions`, `GET /api/v1/models`, `GET /api/v1/key` |
| Anthropic Messages | `POST /v1/messages`, `POST /v1/messages/count_tokens`, `GET /v1/models` (with an `anthropic-version` header) |

## Quick start

```sh
cargo install --path . --locked                 # installs to ~/.cargo/bin; reruns replace it safely
nanolb up                                        # background server on 127.0.0.1:2455, data in ~/.nanolb
nanolb down                                      # clean stop
nanolb service install                           # or run as a service: launchd on macOS, systemd on Linux (as root)
nanolb login                                     # device-code login, one per account
nanolb login --browser                           # or a browser PKCE login
nanolb accounts add-openrouter sk-or-...         # optional: OpenRouter upstream
nanolb keys create hermes                        # prints the nlb-... key one time
nanolb codex-config --key nlb-...                # prints client config snippets
```

Do not use `nanolb accounts import` on the `~/.codex/auth.json` that your Codex CLI still uses. A refresh token can be used one time only. When nanolb refreshes the token, the Codex CLI copy stops working. Use `nanolb login` to give nanolb its own session.

## Connect a client

Hermes Agent (`~/.hermes/config.yaml`, with `NANOLB_API_KEY=nlb-...` in `~/.hermes/.env`):

```yaml
providers:
  nanolb:
    api: http://127.0.0.1:2455/v1
    key_env: NANOLB_API_KEY
    transport: codex_responses
    default_model: gpt-6.1-sol
    discover_models: true
model:
  default: gpt-6.1-sol
  provider: custom:nanolb
```

Codex CLI (`~/.codex/config.toml`):

```toml
model_provider = "nanolb"
[model_providers.nanolb]
name = "nanolb"
base_url = "http://127.0.0.1:2455/backend-api/codex"
wire_api = "responses"
env_key = "NANOLB_API_KEY"
```

Claude Code: set `ANTHROPIC_BASE_URL=http://127.0.0.1:2455`, `ANTHROPIC_AUTH_TOKEN=nlb-...` and `ANTHROPIC_MODEL=gpt-6.1-sol`. nanolb serves Codex models on `/v1/messages`. Hermes can use the same endpoint with a provider that has `api: http://127.0.0.1:2455` and `transport: anthropic_messages` (see AGENTS.md).

OpenAI SDKs: use base URL `http://127.0.0.1:2455/v1`. OpenRouter SDKs: use `http://127.0.0.1:2455/api/v1`.

## Models and providers

- nanolb gets the Codex model list from the live backend. To see it, run `nanolb models`.
- A model id that contains `/` goes to an OpenRouter account. For example: `z-ai/glm-5.3-flash`, `deepseek/deepseek-v4.1-flash`, `anthropic/claude-opus-5.5`. nanolb removes an `openai/` prefix first, so `openai/gpt-5.5` goes to Codex.
- nanolb never replaces one model with a different model. The model id that the client sends selects the provider:

  | Model id | Provider accounts |
  |---|---|
  | contains `/` (for example `deepseek/deepseek-v4.1-flash`) | OpenRouter |
  | all other ids (for example `gpt-6.1-sol`) | Codex |

  If no account of that provider is connected, nanolb returns an error.

## Routing

nanolb routes for cache first. All requests of one conversation stay on one account, so the upstream prompt cache stays warm. The sticky key is the first of these that exists:

1. The `session_id`, `session-id`, `thread-id` or `conversation_id` header.
2. `prompt_cache_key`.
3. Anthropic `metadata.user_id`.
4. A hash of the instructions and the first input items.

nanolb moves a conversation to a different account only when its account is rate limited, cooling down or failing. The conversation then stays on the new account.

For a new conversation, nanolb uses the strategy you set with `nanolb settings set strategy <name>`:

| Strategy | Behavior |
|---|---|
| `preserve` (default) | Use the account with the lowest usage. |
| `burn` | Use the account whose weekly window resets first, so its quota is not wasted. |
| `round_robin` | Use the accounts in turn. |
| `priority` | Use the lowest `priority` number first. Change to the next account when it is limited. |

Before the first byte goes to the client, nanolb fails over to the next account when the upstream returns 429, 5xx, 401/403 or a connection error. On a 401, nanolb refreshes the token one time and tries again. Access tokens are also refreshed before they expire. One refresh at a time runs for each account.

## Warmup

A rate-limit window starts at the first request in it. A warmup sends a very small prompt at a set time, so the window starts and resets at a time you can predict. Schedules use cron syntax in local time:

```sh
nanolb accounts set-warmup <id> "0 4 * * *"      # 04:00 every day
nanolb accounts warmup <id>                      # send one now
```

nanolb skips a scheduled warmup when both windows are already active.

## API keys and usage

```sh
nanolb keys create team --account <id> --model gpt-6.1-sol --limit day:usd:5 --limit month:tokens:20000000
nanolb usage --since 7d --group account          # also: key | model | hour | day
nanolb requests --limit 20
nanolb top                                       # live TUI
```

nanolb records each request with a timestamp, account, key, model, input/cached/output/reasoning tokens, cost, latency and time to first byte.

## Agent access

- **Admin RPC:** `POST /admin/{op}` with a JSON body and `Authorization: Bearer $(cat ~/.nanolb/admin_token)`. Run `nanolb call <op> '<json>'` from the shell. The list of ops is in [AGENTS.md](AGENTS.md).
- **MCP over HTTP:** `http://127.0.0.1:2455/mcp`. Use the same admin token.
- **MCP over stdio:** `nanolb mcp`. Example: `claude mcp add nanolb -- nanolb mcp`.

## Network

The server listens on `127.0.0.1:2455` by default.

- **Tailnet:** run `nanolb serve --host <tailscale-ip>` or `--host 0.0.0.0`.
- **Cloudflare Tunnel:** point the tunnel at `http://127.0.0.1:2455`.

Each proxy request needs an API key. Each admin or MCP request needs the admin token.

## Configuration

| Variable | Default |
|---|---|
| `NANOLB_HOME` | `~/.nanolb` |
| `NANOLB_HOST` | `127.0.0.1` |
| `NANOLB_PORT` | `2455` |
| `NANOLB_URL` (CLI) | `http://127.0.0.1:2455` |
| `NANOLB_ADMIN_TOKEN` (CLI) | contents of `$NANOLB_HOME/admin_token` |

To see all runtime settings, run `nanolb settings get`. They include the strategy, stickiness, retry attempts, model lists, pricing, upstream URLs, the Codex client version and the warmup models.
