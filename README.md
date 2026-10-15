# nanolb

A small, fast load balancer for your Codex (ChatGPT) subscriptions.

You sign in to nanolb with each ChatGPT account you own. Point your tools at nanolb instead of OpenAI. When one account runs into its usage limit, nanolb moves new work to the next one. Your agent keeps going and nobody has to switch accounts by hand.

It is one Rust binary with a SQLite file next to it. There is no web dashboard. You drive it from the command line, an HTTP admin API, or MCP, so AI agents can run it as easily as you can.

## Why nanolb

nanolb adds about 0.1 ms to a request. Streams pass straight through, connections to OpenAI are reused, and nothing on the request path waits on the database.

A conversation stays on one account until that account hits a limit, so long agent sessions keep their prompt cache and stay cheap. If an account is rate limited, returns an error or drops the connection before the reply starts, nanolb quietly retries on another account.

Your tools don't need to change. nanolb accepts OpenAI Responses (over HTTP or WebSocket), Chat Completions, OpenRouter and Anthropic Messages requests, so the Codex CLI, Hermes, Claude Code, the OpenAI SDKs and OpenRouter clients all work as they are.

Every request is recorded with its tokens, cache hits, cost and latency, and you can break that down by account, API key or model.

nanolb started as a rewrite of [codex-lb](https://github.com/Soju06/codex-lb), with speed and agent control as the main goals.

## Quick start

You need Rust 1.97 or newer.

```sh
git clone https://github.com/pyraxo/nanolb && cd nanolb
cargo install --path . --locked

nanolb up                     # start the server on 127.0.0.1:2455
nanolb login                  # sign in to a ChatGPT account (repeat for each account)
nanolb keys create laptop     # make an API key for your tools; it's shown once
nanolb codex-config --key nlb-...   # print ready-to-paste config for your tools
```

`nanolb login` shows a code and a link. Open the link, sign in and enter the code. If your workspace doesn't allow device codes, use `nanolb login --browser`.

Use `nanolb login` rather than importing `~/.codex/auth.json`. Each sign-in token can be refreshed only once, so if nanolb and your Codex CLI share a login, one of them gets signed out.

## Connect your tools

Each tool needs a base URL and the `nlb-...` key you created.

| Tool | Base URL |
|---|---|
| Codex CLI | `http://127.0.0.1:2455/backend-api/codex` |
| OpenAI SDKs and anything with an "OpenAI base URL" setting | `http://127.0.0.1:2455/v1` |
| Anthropic SDKs and Claude Code (`ANTHROPIC_BASE_URL`) | `http://127.0.0.1:2455` |
| OpenRouter clients | `http://127.0.0.1:2455/api/v1` |

### Codex CLI

Add this to `~/.codex/config.toml`, then `export NANOLB_API_KEY=nlb-...`:

```toml
model_provider = "nanolb"

[model_providers.nanolb]
name = "nanolb"
base_url = "http://127.0.0.1:2455/backend-api/codex"
wire_api = "responses"
env_key = "NANOLB_API_KEY"
supports_websockets = true

[model_providers.nanolb.capabilities]
remote_compaction = "v2"
```

### Hermes Agent

Put `NANOLB_API_KEY=nlb-...` in `~/.hermes/.env` and add this to `~/.hermes/config.yaml`:

```yaml
providers:
  nanolb:
    base_url: "http://127.0.0.1:2455/v1"
    key_env: "NANOLB_API_KEY"
    api_mode: codex_responses
    model: gpt-6.1-sol
    discover_models: true

model:
  default: "gpt-6.1-sol"
  provider: "custom:nanolb"
```

### Claude Code

Claude Code can use your Codex models through nanolb:

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:2455
export ANTHROPIC_AUTH_TOKEN=nlb-...
export ANTHROPIC_MODEL=gpt-6.1-sol
```

## Models

The model name you send picks where the request goes. nanolb never swaps one model for another.

- Plain names like `gpt-6.1-sol` go to your Codex accounts. Run `nanolb models` to see what your accounts can use right now.
- Names with a slash, like `z-ai/glm-5.3-flash` or `deepseek/deepseek-v4.1-flash`, go to OpenRouter. Add a key first with `nanolb accounts add-openrouter sk-or-...`.

## Day to day

```sh
nanolb status                       # accounts, limits and today's totals
nanolb top                          # live view in the terminal
nanolb usage --since 7d --group account
nanolb analytics --since 7d         # latency, errors, cache hits, top keys and models
nanolb accounts ls                  # refer to accounts by id, email or label
nanolb settings get                 # every setting; change one with `settings set <key> <value>`
```

### Routing

nanolb places each new conversation using the strategy you set with `nanolb settings set strategy <name>`:

| Strategy | Picks |
|---|---|
| `preserve` (default) | the account with the most headroom |
| `burn` | the account whose weekly limit resets soonest, so its quota isn't wasted |
| `latency` | the account that has been answering fastest |
| `round_robin` | each account in turn |
| `priority` | your preferred order, falling through when one is limited |

### Limits on keys

A key can have its own daily, weekly or monthly caps on tokens, cost or requests, and you can restrict which accounts and models it may use:

```sh
nanolb keys create teammate --model gpt-6.1-sol --limit day:usd:5 --limit month:tokens:20000000
```

### Warmups

A usage window starts with the first request in it. A scheduled warmup sends a tiny request so your windows start, and therefore reset, at a time that suits you:

```sh
nanolb accounts set-warmup work@example.com "0 4 * * *"   # every day at 04:00 local time
nanolb plan                                               # when will each account run out?
```

## Run it as a service

```sh
nanolb service install        # macOS: a launchd agent for your user
sudo nanolb service install   # Linux: a systemd service with its own user
nanolb service uninstall      # add --purge to delete the data too
```

The service starts at boot and comes back after a crash. On Linux, data lives in `/var/lib/nanolb` and logs in `journalctl -u nanolb`. Everywhere else, data and logs are in `~/.nanolb`. Stopping cleanly saves rate limits, cooldowns and conversation pins, so restarts don't cost you cache hits.

## Reaching it from other machines

nanolb listens on `127.0.0.1` by default. To use it over Tailscale, start it with `--host` set to your tailnet IP. To expose it through a Cloudflare Tunnel, point the tunnel at `http://127.0.0.1:2455`. Every request needs an API key, and the admin API needs the admin token in `~/.nanolb/admin_token`.

## For AI agents

Agents that install or run nanolb for someone should read [AGENTS_README.md](AGENTS_README.md). It covers every command, admin API call and MCP tool, and has a troubleshooting table. Agents that change nanolb's code should read [AGENTS.md](AGENTS.md).

nanolb also runs as an MCP server, either over stdio (`claude mcp add nanolb -- nanolb mcp`) or at `http://127.0.0.1:2455/mcp` with the admin token.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the checks every change has to pass and the commit conventions.
