# Set up and operate nanolb (guide for AI agents)

This file is for AI agents (Claude Code, Codex, Hermes and others) that install, configure or operate nanolb for a user. Follow it to set up and run nanolb without help. A human is needed only for the steps marked HUMAN. Each HUMAN step needs a browser.

If you change nanolb's code, read [AGENTS.md](AGENTS.md) instead.

## 1. What nanolb is

- nanolb is one Rust binary with a SQLite store. It is a load balancer for Codex (ChatGPT) accounts and OpenRouter keys.
- Clients send OpenAI Responses, OpenAI Chat, OpenRouter or Anthropic Messages requests. nanolb picks an account, fails over before the first byte, and keeps one conversation on one account so the prompt cache stays warm.
- You control it with the CLI, the admin RPC and MCP. There is no web GUI.

## 2. Install, build and run

Build:

```sh
cd /path/to/nanolb
cargo build --release
cargo install --path . --locked
```

`cargo install` puts `nanolb` in `~/.cargo/bin`. Or use `./target/release/nanolb`.

Start and stop it in the background:

```sh
nanolb up      # starts `nanolb serve` detached, logs to ~/.nanolb/nanolb.log, waits for /health
nanolb down    # sends SIGINT, waits for a clean stop (state is saved to SQLite)
```

`nanolb up` accepts `--host` and `--port`. Both commands print JSON and do nothing if the server is already in the requested state. `nanolb serve` writes its PID to `~/.nanolb/nanolb.pid`. nanolb stops cleanly on SIGINT or SIGTERM. Do not use `kill -9`, because it skips the state save.

Check that it runs:

```sh
curl -s http://127.0.0.1:2455/health
nanolb status
```

`/health` returns `ok`. The first start creates `$NANOLB_HOME/admin_token` (mode 0600) and `$NANOLB_HOME/nanolb.db`. The default `NANOLB_HOME` is `~/.nanolb`. The CLI reads `NANOLB_URL` (default `http://127.0.0.1:2455`) and `NANOLB_ADMIN_TOKEN` (default: the token file).

`serve` options: `--host` (env `NANOLB_HOST`, default `127.0.0.1`) and `--port` (env `NANOLB_PORT`, default `2455`). To reach nanolb from other machines, use `--host 0.0.0.0` or a tailnet IP.

Run as a service that starts at boot and restarts on a crash:

```sh
sudo nanolb service install            # Linux: systemd system service
nanolb service install                 # macOS: launchd agent for the current user
nanolb service uninstall               # stop and remove the service, keep the data
nanolb service uninstall --purge       # also remove the data (Linux: also the nanolb user and /usr/local/bin/nanolb)
```

`service install` accepts `--host` and `--port`. It prints JSON and waits for `/health`.

- Linux:
  - It copies the binary to `/usr/local/bin/nanolb`, creates the system user `nanolb`, and writes `/etc/systemd/system/nanolb.service` from `contrib/nanolb.service`.
  - It runs `systemctl enable --now nanolb`.
  - Data is in `/var/lib/nanolb` (mode 0700). Logs go to `journalctl -u nanolb -f`.
  - To use the CLI on the server, run `sudo -u nanolb NANOLB_HOME=/var/lib/nanolb nanolb <command>`.
- macOS:
  - It writes `~/Library/LaunchAgents/moe.pyraxo.nanolb.plist` from `contrib/nanolb.plist` and loads it.
  - Data stays in `~/.nanolb`. Logs go to `~/.nanolb/nanolb.log`.
  - If a `nanolb up` instance runs, `service install` stops it first.
- Under a service, use `service uninstall`, not `nanolb down`. The service manager starts a stopped server again.

Stop nanolb with SIGINT (Ctrl-C or `kill -INT <pid>`). It then saves its runtime state.

## 3. Model-to-provider routing

The model id in the request selects the provider. nanolb never replaces one model with a different model.

| Model id | Provider | Notes |
|---|---|---|
| Contains `/` | OpenRouter accounts | Example: `z-ai/glm-5.3-flash`, `anthropic/claude-opus-5.5`. |
| Any other id | Codex accounts | Example: `gpt-6.1-sol`. |

- nanolb removes a leading `openai/` first. So `openai/gpt-6.1-sol` becomes `gpt-6.1-sol` and goes to Codex.
- An id with `/` always goes to OpenRouter. For example, `anthropic/claude-opus-5.5` goes to OpenRouter.
- `POST /v1/messages` accepts the Anthropic format. It serves Codex models (and OpenRouter ids) to Anthropic-format clients. Use a Codex model id, for example `gpt-6.1-sol`.
- If no account of the selected provider is connected, nanolb returns 503.
- To list the models, run `nanolb models` (admin op `models`) or `GET /v1/models`.

### 3.1 Endpoints

All routes in this table need a client key (`Authorization: Bearer nlb-...` or `x-api-key`). The admin routes (`/admin/{op}`, `/mcp`) need the admin token. `/health` needs no key.

| Route | Purpose |
|---|---|
| `POST /v1/responses`, `POST /backend-api/codex/responses` | OpenAI Responses (SSE). Codex CLI. |
| `GET /v1/responses`, `GET /backend-api/codex/responses` | WebSocket upgrade for the Codex CLI (section 7.1). |
| `GET /backend-api/codex/models` | Codex model list. |
| `POST /v1/chat/completions`, `GET /v1/models` | OpenAI Chat Completions and model list. |
| `POST /api/v1/chat/completions`, `GET /api/v1/models`, `GET /api/v1/key`, `GET /api/v1/auth/key` | OpenRouter format. |
| `POST /v1/messages`, `POST /v1/messages/count_tokens` | Anthropic format. Serves Codex models. `GET /v1/models` with an `anthropic-version` header returns the same models. |
| `GET /backend-api/wham/usage`, `GET /api/codex/usage` | Pool usage for the key: plan, primary and secondary window, `allowed`. |
| `GET /v1/usage` | Usage and limits of the calling key, and pool usage. |
| `POST /backend-api/files`, `POST /backend-api/files/{id}/uploaded` | File upload to Codex. nanolb remembers which account owns the file. |
| `POST /backend-api/transcribe`, `POST /v1/audio/transcriptions` | Transcription (multipart). The `/v1` route needs a `model` field. The key's model list must allow that model. The ChatGPT backend selects the transcription model itself. |
| `POST /v1/images/generations`, `POST /backend-api/codex/images/generations` | Image generation. Default model `gpt-image-2`. Not streamed. |
| `POST /v1/images/edits` (multipart), `POST /backend-api/codex/images/edits` (JSON `images[].image_url`) | Image edit. Up to 16 images and an optional mask. |
| `POST /v1/embeddings` | Embeddings. OpenRouter model ids only (with `/`). |
| `POST /v1/realtime/calls`, `POST /backend-api/codex/realtime/calls` | Realtime voice call set-up. |
| `GET /v1/realtime`, `GET /v1/live/{call_id}`, `GET /backend-api/codex/{call_id}` | Realtime WebSocket and call side channel. |
| `GET /health` | Returns `ok`. |

Files: an `input_file` item with a `file_id` goes to the account that got the upload. This applies to `POST` on `/v1/responses` and `/backend-api/codex/responses`. nanolb sets the sticky pin of the conversation to that account. If that account is limited or failing, nanolb can fail over, and the other account does not know the file.

Realtime routes go to `realtime_base` (default `https://api.openai.com/v1`) with a Codex account. A call keeps its account for the side channel (`/v1/live/{call_id}`). An unknown or expired call id returns 404.

## 4. Connect accounts

Each account type has its own login. Run each login one time for each account.

How to hand a URL or code to the human: print it in your reply to the user, or send it through the channel that the user reads. Then wait for the human to answer. Do not open a browser yourself unless you control one that is signed in to the right account.

### 4.1 Codex, device login (best for agents)

1. Call `login_start`:
   ```sh
   nanolb call login_start '{"label":"codex-1"}'
   ```
   The result has `login_id`, `verification_url`, `user_code` and `expires_at`.
2. HUMAN: the human opens `verification_url` in a browser, signs in to the ChatGPT account, and enters `user_code`. Give both values to the human.
3. Poll every 3 seconds until `status` is `done` or `error`:
   ```sh
   nanolb call login_poll '{"login_id":"<login_id>"}'
   ```
   `done` returns the new `account`. The server polls the provider in the background. The CLI shortcut `nanolb login --label codex-1` does steps 1 and 3 and prints the URL and code on stderr.

### 4.2 Codex, browser login

`nanolb login --browser` starts a PKCE login. It listens on `127.0.0.1:1455` for the callback and opens the browser. HUMAN: the human signs in. The browser must run on the same machine as the CLI. Port 1455 must be free. The command times out after 300 seconds. Use the device login when the browser is on another machine.

### 4.3 OpenRouter key

No human browser step is needed if the human gives you the key.

```sh
nanolb accounts add-openrouter sk-or-... --label or-1
```

nanolb checks the key with `GET /key` on OpenRouter. It returns an error if OpenRouter rejects the key.

### 4.4 Warning: do not share a login with a live CLI

A refresh token can be used one time only. When nanolb refreshes a token, the old token stops working. Do not run `nanolb accounts import` on a `~/.codex/auth.json` that the Codex CLI still uses. If you do, the CLI or nanolb loses its session. Use the logins in 4.1 and 4.2. They give nanolb its own session. `account_import` and `account_add_tokens` are for tokens that no other program uses.

### 4.5 Check the accounts

```sh
nanolb accounts ls
```

An account is usable when `enabled` is true and `available` is true. Other commands: `accounts enable|disable <id>`, `accounts priority <id> <n>`, `accounts refresh <id>`, `accounts rm <id>`. `accounts refresh` also fetches live usage.

## 5. Create API keys with limits

Clients use an `nlb-...` key. The key is shown one time.

```sh
nanolb keys create hermes
nanolb keys create team --account <account_id> --model gpt-6.1-sol \
  --limit day:usd:5 --limit month:tokens:20000000 --expires 2027-01-01T00:00:00Z
```

- `--account` limits the key to some accounts. Repeat the flag. Omit it for all accounts.
- `--model` limits the key to some model ids. Repeat the flag. Omit it for all models.
- `--limit WINDOW:KIND:VALUE`. `WINDOW` is `day`, `week`, `month` or `total`. `KIND` is `tokens`, `requests` or `usd`. Repeat the flag. The key stops when any limit is reached.
- `--expires` takes an RFC3339 time.

Manage keys: `nanolb keys ls`, `keys update <id> [same flags]`, `keys enable|disable <id>`, `keys rm <id>`. `keys update` replaces the lists that you give.

Save the secret in an environment variable, for example `NANOLB_API_KEY`. `keys ls` never shows the secret again.

## 6. Client configs

Base URL: `http://127.0.0.1:2455`. Replace it if nanolb runs on another host. Run `nanolb codex-config --key nlb-...` to print snippets. Always set the model to one that `nanolb models` lists.

### Hermes Agent

`~/.hermes/.env`:

```
NANOLB_API_KEY=nlb-...
```

`~/.hermes/config.yaml`. The first provider uses the Responses API. The second provider serves the same Codex models through the Anthropic Messages API. The Anthropic client adds `/v1/messages` itself, so its `base_url` has no `/v1`. This format was checked against Hermes Agent v0.22.

```yaml
providers:
  nanolb:
    base_url: "http://127.0.0.1:2455/v1"
    key_env: "NANOLB_API_KEY"
    api_mode: codex_responses
    model: gpt-6.1-sol
    discover_models: true
  nanolb-anthropic:
    base_url: "http://127.0.0.1:2455"
    key_env: "NANOLB_API_KEY"
    api_mode: anthropic_messages
    model: gpt-6.1-sol

model:
  default: "gpt-6.1-sol"
  provider: "custom:nanolb"
```

Run one query with a given provider: `hermes chat -q "..." --oneshot --provider custom:nanolb-anthropic -m gpt-6.1-sol`.

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

`supports_websockets = true` makes the Codex CLI use a WebSocket on `/responses` (section 7.1). If the WebSocket fails, the CLI uses HTTP. The `capabilities` table turns on remote compaction (v2). Codex then sends `compaction_trigger` items on `/responses`, and nanolb forwards them like any other request.

Then: `export NANOLB_API_KEY=nlb-...`.

### Claude Code

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:2455
export ANTHROPIC_AUTH_TOKEN=nlb-...
```

Set a Codex model id: `export ANTHROPIC_MODEL=gpt-6.1-sol`. nanolb translates the Anthropic request to the Codex format. A Codex account must be connected (section 4.1 or 4.2).

### OpenAI SDK

Base URL `http://127.0.0.1:2455/v1`, API key `nlb-...`. Use `POST /v1/chat/completions` or `POST /v1/responses`. Use Codex model ids, or OpenRouter ids with `/`.

### OpenRouter SDK

Base URL `http://127.0.0.1:2455/api/v1`, API key `nlb-...`. Endpoints: `POST /api/v1/chat/completions`, `GET /api/v1/models`, `GET /api/v1/key`.

### Anthropic SDK

Base URL `http://127.0.0.1:2455`. nanolb accepts the key in `x-api-key` or `Authorization: Bearer`. Endpoints: `POST /v1/messages`, `POST /v1/messages/count_tokens`, `GET /v1/models` (send the `anthropic-version` header). Use Codex model ids.

### Sticky key

nanolb keeps a conversation on one account. The sticky key is the first one that exists: header `session_id`, `session-id`, `thread-id` or `conversation_id`; then `prompt_cache_key`; then Anthropic `metadata.user_id`; then a hash of the instructions and the first input items. Send a stable value in one of these for best cache hits.

## 7. Operate

```sh
nanolb status                         # accounts, rate limits, 24h requests and cost
nanolb usage --since 7d --group account   # group: account|key|model|hour|day|all
nanolb requests --limit 20            # most recent requests
nanolb top                            # live TUI for humans
nanolb settings get
nanolb settings set strategy burn
nanolb settings set sticky false
```

`--since` takes `1h`, `24h`, `7d`, `30d` or an RFC3339 time. `settings set <key> <value>` parses the value as JSON, and uses a plain string if the parse fails. For example: `nanolb settings set max_attempts 6`.

### Analytics, planner, sessions and reset credits

```sh
nanolb analytics --since 7d --view all|series|latency|errors|cache|top|utilization --by account|model|key --bucket hour|day
nanolb plan [--work-start 09:00] [--lookback 6h] [--apply]
nanolb sessions ls [--key <substring>] [--account <id>] [--limit 100]
nanolb sessions rm <key>
nanolb sessions purge --all | --account <id> | --older-than 7d
nanolb reset-credits ls <account>
nanolb reset-credits consume <account> [--credit <id>]
```

- `analytics` prints JSON. `latency` gives p50 and p95 of latency and TTFB (successful requests only). `cache` gives `cached/input`. `utilization` gives each account's share of tokens in each bucket.
- `plan` projects when each rate-limit window runs out, from the recent token rate in the `usage` table and the live `rate_limit`. It also recommends a daily warmup cron for each Codex account. The crons are spread over the window length from `--work-start`. `--apply` writes them as the account warmups.
- `sessions` manages the sticky pins. A removed pin is also removed from SQLite.
- `reset-credits consume` spends one banked reset credit. Without `--credit` it uses the available credit that expires first.

### 7.1 Speed and compatibility features

- **Header passthrough.** nanolb copies these upstream response headers to the client: `x-request-id`, `openai-model`, `x-reasoning-included`, `x-models-etag` and every `x-codex-*` header (for example `x-codex-turn-state`) except the `x-codex-primary-*` and `x-codex-secondary-*` rate-limit headers. nanolb also forwards the `x-codex-*` request headers of the client upstream.
- **Pool rate-limit headers.** On Codex responses nanolb sets `x-codex-primary-used-percent`, `-window-minutes` and `-reset-at`, and the same three for `secondary`. The values are the pool total for the Codex accounts that the key may use. Plan weights apply. The Codex `/status` command shows the pool. The Codex CLI reads these headers over HTTP. Over a WebSocket nanolb sends `codex.rate_limits` events.
- **SSE keepalive.** nanolb sends `: keepalive` (an SSE comment) when a stream is idle for `keepalive_secs` (default 15). Set `0` to turn it off. It stops proxies and Cloudflare tunnels from closing a quiet stream.
- **zstd.** With `compress_upstream` true (default) nanolb compresses the Codex upstream request body with zstd (level 3) and sets `Content-Encoding: zstd`. Set it to `false` if an upstream rejects the body.
- **WebSocket.** `GET /v1/responses` and `GET /backend-api/codex/responses` upgrade to a WebSocket. nanolb opens a WebSocket to the Codex upstream with a pool account, relays the frames, and fails over on a handshake 429 or 5xx and on a close before the first event. A conversation keeps its account (sticky key from the session headers, or from the first frame). Usage rows have `endpoint` `responses_ws`.
- **Prewarm.** Setting `prewarm` (default `false`). When true, nanolb sends one extra upstream request with `generate=false` for each client WebSocket whose first request is not already a prewarm. The cost is at most 1 extra upstream request for each client connection. The Codex CLI sends its own prewarm, so the cost is 0 for the Codex CLI. HTTP clients cost 0.
- **No HTTP-to-WebSocket bridge.** nanolb does not turn HTTP requests into WebSocket requests. HTTP/2 connection pooling already removes the per-request set-up cost. HTTP clients also send the full history in each request, so a WebSocket upstream gives no gain.

### Strategies

The strategy picks the account for a new conversation. A sticky conversation keeps its account until that account is limited or failing.

| Strategy | Behavior |
|---|---|
| `preserve` (default) | Use the least-used account first. |
| `burn` | Use the account whose weekly window resets first. |
| `round_robin` | Use the accounts in turn. |
| `priority` | Use the lowest `priority` number first. Go to the next when limited. |
| `latency` | Use the account with the lowest score: `(ttfb average + 1000 ms) * (1 + 4 * recent error rate)`. Ties go to the account with fewer in-flight requests. |

Each account in `accounts ls` and `status` has `latency.ttfb_ewma_ms` and `latency.error_rate`. These are the inputs of the `latency` strategy.

### Settings fields (defaults)

`strategy` (`preserve`), `sticky` (`true`), `max_attempts` (4, accounts tried for each request), `models`, `pricing` (model to `[input, cached, output]` USD per 1M tokens), `upstream_base`, `codex_version` (`auto`: nanolb reads the latest `@openai/codex` release from npm at startup and every 24 hours, with `0.162.1` as the fallback; set a fixed version to pin it), `usage_poll_secs` (60), `warmup_model` (`gpt-6-luna`), `openrouter_base`, `openrouter_models`, `first_event_timeout_secs` (90), `idle_timeout_secs` (600), `keepalive_secs` (15), `compress_upstream` (`true`), `realtime_base` (`https://api.openai.com/v1`), `prewarm` (`false`). An unknown setting name returns an error.

### Warmup

A rate-limit window starts at the first request in it. A warmup sends a small prompt on a cron schedule (5 fields, local time) so the window starts at a known time.

```sh
nanolb accounts set-warmup <id> "0 4 * * *" [--model <model>] [--prompt <text>]
nanolb accounts warmup <id>          # fire now
nanolb accounts clear-warmup <id>
```

### Logs

nanolb writes logs to stderr. Set `NANOLB_LOG` to a tracing filter (default `info`, for example `nanolb=debug`). Set `NANOLB_LOG_FORMAT=json` for JSON lines.

Each finished request logs one line with message `request` and these fields: `endpoint` (`responses`, `chat`, `openrouter`, `anthropic`, `warmup`, `files`, `transcribe`, `images`, `embeddings`, `responses_ws`, `realtime` or `realtime_call`), `key`, `account`, `model`, `status`, `input`, `cached`, `output`, `reasoning`, `cost_usd`, `latency_ms`, `ttfb_ms`. Other lines: `upstream error, failing over`, `account cooling down`, `token refresh failed`, `token refreshed`, `warmup sent`, `request failed`.

### Where state lives

`$NANOLB_HOME/nanolb.db` (SQLite, WAL). Table `kv(tbl, id, json)` has these `tbl` values: `account` (accounts and tokens), `key` (API keys, stored as a hash), `settings`, `account_state` (cooldowns, rate limits, last errors), `sticky` (conversation to account, pruned after 7 days). Table `usage` has one row for each request: `ts, key_id, account_id, model, endpoint, status, input, cached, output, reasoning, cost, latency_ms, ttfb_ms`. `$NANOLB_HOME/admin_token` holds the admin token. Stop nanolb before you copy or edit the database. The files contain secrets. Do not print or commit them.

## 8. Admin RPC and MCP

### Transport and auth

- RPC: `POST http://127.0.0.1:2455/admin/{op}` with a JSON body. `GET` with a query string also works. Header: `Authorization: Bearer <admin_token>`.
- CLI: `nanolb call <op> '<json>'` prints the JSON result.
- MCP over HTTP: `http://127.0.0.1:2455/mcp`, same admin token. MCP over stdio: `nanolb mcp`. Register it with `claude mcp add nanolb -- nanolb mcp`. MCP tool names equal op names.
- The admin token is different from the client `nlb-` keys. A client key cannot call admin ops. An admin token cannot call the proxy endpoints.
- Success: 200 and JSON. Error: `{"error":{"message":"..."}}` with 400 (bad args), 401 (bad token) or 404 (unknown op or id).

```sh
TOKEN=$(cat ~/.nanolb/admin_token)
curl -s -X POST http://127.0.0.1:2455/admin/status -H "Authorization: Bearer $TOKEN"
```

### Ops

`id` is an account id or key id. `?` means optional.

| Op | Args | Result |
|---|---|---|
| `status` | none | version, uptime, strategy, sticky, accounts, keys, requests_24h, cost_24h, tokens_24h |
| `accounts_list` | none | list of accounts |
| `account_import` | `auth_json` (object or string), `label?` | account |
| `account_add_tokens` | `id_token`, `access_token`, `refresh_token`, `label?` | account |
| `account_add_openrouter` | `api_key`, `label?` | account |
| `login_start` | `label?` | `login_id`, `verification_url`, `user_code`, `expires_at` |
| `login_poll` | `login_id` | `status` (`pending`, `done`, `error`), `account?`, `error?` |
| `account_update` | `id`, `enabled?`, `priority?`, `label?`, `warmup?` (`{cron, model?, prompt?}` or null) | account |
| `account_remove` | `id` | `{removed}` |
| `account_refresh` | `id` | account (refreshes tokens, fetches usage, clears cooldown) |
| `account_warmup` | `id` | `{ok, status, error?}` |
| `keys_list` | none | list of keys |
| `key_create` | `name`, `accounts?`, `models?`, `limits?` (`[{window, max_tokens?, max_cost_usd?, max_requests?}]`), `expires_at?` | key and `key` secret |
| `key_update` | `id`, `name?`, `accounts?`, `models?`, `limits?`, `enabled?`, `expires_at?` | key |
| `key_remove` | `id` | `{removed}` |
| `usage` | `since?` (default `24h`), `group?` (default `all`) | rows with requests, tokens, cost_usd, errors, avg latency |
| `requests` | `limit?` (default 50, max 5000) | recent request rows |
| `settings_get` | none | settings |
| `settings_update` | any settings field | settings |
| `models` | none | list of `{id, context_window}` |
| `sessions_list` | `key?` (substring), `account?`, `limit?` (default 100) | `[{key, account, last_used}]` |
| `session_delete` | `key` | `{removed}` |
| `sessions_purge` | `all?`, `account?`, `older_than?` (one at least) | `{removed}` |
| `reset_credits_list` | `id` (account) | `{credits, available_count}` |
| `reset_credits_consume` | `id` (account), `credit_id?` | upstream result and `account` |
| `plan` | `work_start?`, `lookback?`, `apply?` | per account: `windows` with `exhausts_at`, `recommended_warmup` |
| `analytics` | `since?` (default `7d`), `view?` (default `all`), `by?`, `bucket?`, `limit?` | rows for the view |

An account object has `id`, `kind` (`codex` or `openrouter`), `email`, `label`, `plan`, `enabled`, `priority`, `available`, `cooldown_until`, `inflight`, `rate_limit`, `latency` (`ttfb_ewma_ms`, `error_rate`), `last_error` and `warmup`.

## 9. Troubleshooting

| Message or symptom | Meaning and fix |
|---|---|
| `401 invalid or missing api key` | The client key is wrong, removed or missing. Check `Authorization: Bearer nlb-...` or `x-api-key`. Run `nanolb keys ls`. |
| `401 invalid admin token` | The admin token is wrong. Read `$NANOLB_HOME/admin_token` again. It must match the server that you call. |
| `429 all <provider> accounts are rate limited until T` | Every account of that provider is at its limit or cooling down. The `retry-after` header gives the seconds until the first reset. Wait, or connect another account. |
| `503 no codex accounts connected to serve model X` | No account of that provider. Connect one (section 4). Check the model id against section 3. An id with `/` needs an OpenRouter account. Any other id needs a Codex account. |
| `403 model X not allowed for this key` | The key has a `models` list. Run `nanolb keys update <id> --model X`. |
| `429 api key day limit reached` (or week, month, total) | The key hit a limit. Raise it with `keys update`, or wait for the window. |
| `429 api key disabled` or `expired` | Run `nanolb keys enable <id>` or set a new `--expires`. |
| 503 `no available accounts` | All accounts are disabled, cooling down or already tried. Run `nanolb accounts ls`, check `available`, `cooldown_until` and `last_error`. Run `accounts refresh <id>` to clear a cooldown. |
| Codex returns an error that the model is not supported | The model id is not valid for the ChatGPT account. nanolb does not substitute models. Pick an id from `nanolb models`. |
| Account cools down (log `account cooling down`) | The upstream returned 429 (until the limit reset, default 60 s), 401 (5 min), 5xx (5 s). nanolb tries the next account. If it stays, check `rate_limit` in `accounts ls`. |
| `token refresh failed` in logs, account unavailable | 400 or 401 on refresh is permanent: the refresh token was used elsewhere or revoked. The account cools down for 1 hour. Log in again (section 4) and remove the old account. Do not share logins (section 4.4). 5xx or network errors retry after 30 s. |
| `openrouter rejected key: 401` | The OpenRouter key is invalid. |
| `cannot bind 127.0.0.1:1455` | Port 1455 is in use during `login --browser`. Stop the other program or use the device login. |
| `login ... not found` | The server restarted, or the id is wrong. Login state is in memory. Start the login again. |
| WebSocket in the Codex CLI fails or falls back to HTTP | Check `supports_websockets = true`. Check the log for `upstream rejected websocket` or `websocket handshake failed`. An HTTP fallback still works. |
| `404 realtime call binding not found or expired` | The call id is unknown or old. Create the call again with `POST /v1/realtime/calls`. |
| `400 model "X" is not supported for embeddings` | Embeddings need an OpenRouter model id with `/`, for example `openai/text-embedding-3-small`, and an OpenRouter account. |
| Image request returns no image | Image calls are not streamed. The message gives the upstream error. Check `model` and the account plan. |
| A request with `file_id` fails after a failover | The file belongs to one account, and that account was limited or failing. Wait for it, or upload the file again. |
| Stream closes in a tunnel while the model thinks | Set `keepalive_secs` to a value below the tunnel idle timeout. |
| Upstream rejects the request body | Run `nanolb settings set compress_upstream false`. |
| Poor cache hits | The client sends no stable session id. See "Sticky key" in section 6. Check `cached` in `nanolb requests`. |
| `bind host:port: ...` at start | The port is in use. Use `--port`. |
