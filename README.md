# nanolb

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.97+](https://img.shields.io/badge/rust-1.97%2B-orange.svg)](https://www.rust-lang.org)

A fast load balancer for Codex (ChatGPT) subscription accounts.

## Features

- **Low overhead**: about 0.1 ms added per request, streaming passthrough, pooled HTTP/2 connections and no database reads on the request path. See [Performance](#performance).
- **Cache-aware routing**: a conversation stays on one account until that account is limited, so upstream prompt caching keeps working.
- **Automatic failover**: rate limits, server errors and connection failures are retried on another account before the first byte reaches the client.
- **Multiple API formats**: OpenAI Responses (HTTP and WebSocket), OpenAI Chat Completions, OpenRouter and Anthropic Messages, plus files, transcription, images, embeddings and realtime voice.
- **Routing strategies**: `preserve`, `burn`, `latency`, `round_robin` and `priority`.
- **API keys with limits**: per-key caps on tokens, cost or requests per day, week or month, and per-key account and model allow-lists.
- **Usage tracking**: tokens, cached tokens, cost, latency and time to first byte for every request, with analytics by account, key and model.
- **Scheduled warmups**: cron-based warmup requests so usage windows start and reset at predictable times.
- **OpenRouter upstream**: model ids with a vendor prefix (for example `z-ai/glm-5.3-flash`) are routed to OpenRouter keys.

## Table of contents

- [Requirements](#requirements)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Connecting clients](#connecting-clients)
- [Usage](#usage)
- [Running as a service](#running-as-a-service)
- [Configuration](#configuration)
- [Performance](#performance)
- [Documentation](#documentation)
- [Contributing](#contributing)
- [License](#license)

## Requirements

- Rust 1.97 or later
- One or more ChatGPT accounts with Codex access
- macOS or Linux

## Installation

```sh
git clone https://github.com/pyraxo/nanolb
cd nanolb
cargo install --path . --locked
```

This installs the `nanolb` binary to `~/.cargo/bin`.

## Quick start

```sh
# Start the server in the background (127.0.0.1:2455, data in ~/.nanolb)
nanolb up

# Add a ChatGPT account (repeat for each account)
nanolb login

# Create an API key for your clients (the key is printed once)
nanolb keys create laptop

# Print ready-to-use client configuration
nanolb codex-config --key nlb-...
```

`nanolb login` uses the device-code flow: open the printed URL, sign in and enter the code. Use `nanolb login --browser` if device-code login is disabled for your workspace.

> [!WARNING]
> Do not import the `~/.codex/auth.json` that your Codex CLI is still using. Refresh tokens are single-use, so two programs sharing one login will sign each other out. `nanolb login` creates a separate session.

## Connecting clients

| Client | Base URL |
|---|---|
| Codex CLI | `http://127.0.0.1:2455/backend-api/codex` |
| OpenAI SDKs and OpenAI-compatible tools | `http://127.0.0.1:2455/v1` |
| Anthropic SDKs and Claude Code | `http://127.0.0.1:2455` |
| OpenRouter clients | `http://127.0.0.1:2455/api/v1` |

All clients authenticate with an `nlb-...` key, sent as a bearer token or in `x-api-key`.

### Codex CLI

`~/.codex/config.toml`:

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

```sh
export NANOLB_API_KEY=nlb-...
```

### Hermes Agent

`~/.hermes/config.yaml` (set `NANOLB_API_KEY` in `~/.hermes/.env`):

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

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:2455
export ANTHROPIC_AUTH_TOKEN=nlb-...
export ANTHROPIC_MODEL=gpt-6.1-sol
```

## Usage

### Models

The model id in the request selects the upstream. nanolb never substitutes one model for another.

| Model id | Upstream |
|---|---|
| Contains `/`, e.g. `deepseek/deepseek-v4.1-flash` | OpenRouter accounts |
| Anything else, e.g. `gpt-6.1-sol` | Codex accounts |

List the models your accounts can use with `nanolb models`. Add an OpenRouter key with `nanolb accounts add-openrouter sk-or-...`.

### Common commands

```sh
nanolb status                            # accounts, rate limits and 24h totals
nanolb top                               # live terminal view
nanolb accounts ls                       # accounts can be referred to by id, email or label
nanolb usage --since 7d --group account  # usage by account, key, model, hour or day
nanolb analytics --since 7d              # latency percentiles, error rates, cache hit ratio
nanolb requests --limit 20               # most recent requests
nanolb down                              # stop the background server
```

### Routing strategies

New conversations are assigned using the configured strategy (`nanolb settings set strategy <name>`):

| Strategy | Behavior |
|---|---|
| `preserve` (default) | Pick the least-used account |
| `burn` | Pick the account whose weekly window resets soonest |
| `latency` | Pick the account with the lowest recent latency, penalizing recent errors |
| `round_robin` | Rotate through accounts |
| `priority` | Use accounts in priority order, falling through when one is limited |

### API keys

```sh
nanolb keys create teammate \
  --model gpt-6.1-sol \
  --limit day:usd:5 \
  --limit month:tokens:20000000
```

### Warmups

A usage window starts with its first request. A scheduled warmup controls when windows start and reset:

```sh
nanolb accounts set-warmup work@example.com "0 4 * * *"   # daily at 04:00 local time
nanolb plan                                               # projected quota exhaustion per account
```

## Running as a service

```sh
nanolb service install          # macOS: launchd agent for the current user
sudo nanolb service install     # Linux: systemd service with a dedicated user
nanolb service uninstall        # add --purge to also remove data
```

| Platform | Data | Logs |
|---|---|---|
| macOS | `~/.nanolb` | `~/.nanolb/nanolb.log` |
| Linux | `/var/lib/nanolb` | `journalctl -u nanolb` |

The service starts at boot and is restarted on failure. On shutdown, nanolb saves rate-limit state, cooldowns and conversation pins.

### Remote access

nanolb binds to `127.0.0.1` by default. For Tailscale, start it with `--host <tailnet-ip>`. For a Cloudflare Tunnel, point the tunnel at `http://127.0.0.1:2455`.

## Configuration

| Variable | Default | Description |
|---|---|---|
| `NANOLB_HOME` | `~/.nanolb` | Data directory (SQLite database, admin token, logs) |
| `NANOLB_HOST` | `127.0.0.1` | Listen address |
| `NANOLB_PORT` | `2455` | Listen port |
| `NANOLB_LOG` | `info` | Log filter |
| `NANOLB_LOG_FORMAT` | text | Set to `json` for JSON logs |
| `NANOLB_URL` | `http://127.0.0.1:2455` | Server URL used by the CLI |

Runtime settings (strategy, timeouts, pricing, compression and others) are stored in the database. View them with `nanolb settings get` and change them with `nanolb settings set <key> <value>`.

## Performance

Measured on Linux (Docker, aarch64) against a mock upstream that answers instantly, so the numbers show only the overhead nanolb adds. Streaming requests, two accounts, median of three runs.

| Endpoint | Concurrency | Added latency (p50) | p99 | Throughput |
|---|---|---|---|---|
| `/v1/responses` | 1 | 0.10 ms | | 6,800 req/s |
| `/v1/responses` | 64 | 1.8 ms | 5.5 ms | 26,400 req/s |
| `/v1/chat/completions` | 1 | 0.15 ms | | 4,300 req/s |
| `/v1/chat/completions` | 64 | 1.5 ms | 5.8 ms | 30,300 req/s |

| Resource | Value |
|---|---|
| CPU per request | 0.13 to 0.21 ms |
| Memory, idle | 30 MB |
| Memory, under load | 40 to 95 MB |
| Startup | 0.1 s |

## Documentation

- [AGENTS_README.md](AGENTS_README.md): full setup and operations guide, including every admin API operation, MCP tool and a troubleshooting table. Written for AI agents that operate nanolb, and useful as a reference for people.
- [AGENTS.md](AGENTS.md): architecture and conventions for agents that modify the code.
- [CONTRIBUTING.md](CONTRIBUTING.md): required checks, code style and commit conventions.

The MCP server is available over stdio (`nanolb mcp`) and over HTTP at `/mcp`, authenticated with the admin token in `$NANOLB_HOME/admin_token`.

## Contributing

Contributions are welcome. Every change must pass:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 scripts/compat.py check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for details.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
