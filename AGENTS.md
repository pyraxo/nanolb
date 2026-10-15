# AGENTS.md: set up and operate nanolb

This file is for AI agents (Claude Code, Codex, Hermes and others). Follow it to set up and run nanolb without help. A human is needed only for the steps marked HUMAN. Each HUMAN step needs a browser.

## 1. What nanolb is

- nanolb is one Rust binary with a SQLite store. It is a load balancer for Codex (ChatGPT) accounts, Claude accounts and OpenRouter keys.
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

macOS launchd, `~/Library/LaunchAgents/so.nanolb.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>so.nanolb</string>
  <key>ProgramArguments</key><array>
    <string>/Users/USER/.cargo/bin/nanolb</string><string>serve</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>NANOLB_LOG</key><string>info</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/Users/USER/.nanolb/nanolb.log</string>
</dict></plist>
```

Load it: `launchctl load ~/Library/LaunchAgents/so.nanolb.plist`.

Linux systemd, `~/.config/systemd/user/nanolb.service`:

```ini
[Unit]
Description=nanolb
After=network-online.target

[Service]
ExecStart=%h/.cargo/bin/nanolb serve
Environment=NANOLB_LOG=info
Restart=always

[Install]
WantedBy=default.target
```

Start it: `systemctl --user daemon-reload && systemctl --user enable --now nanolb`.

Stop nanolb with SIGINT (Ctrl-C or `kill -INT <pid>`). It then saves its runtime state.

## 3. Model-to-provider routing

The model id in the request selects the provider. nanolb never replaces one model with a different model.

| Model id | Provider | Notes |
|---|---|---|
| Starts with `claude` | Claude accounts | Only through `POST /v1/messages`. Other endpoints return 400. |
| Contains `/` | OpenRouter accounts | Example: `z-ai/glm-5.3-flash`, `anthropic/claude-opus-5.5`. |
| Any other id | Codex accounts | Example: `gpt-6.1-sol`. |

- nanolb removes a leading `openai/` first. So `openai/gpt-5.5` becomes `gpt-5.5` and goes to Codex.
- The check order is: `/` first, then `claude`, then Codex. Note that `anthropic/claude-opus-5.5` contains `/`, so it goes to OpenRouter.
- If no account of the selected provider is connected, nanolb returns 503.
- To list the models, run `nanolb models` (admin op `models`) or `GET /v1/models`.

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

### 4.3 Claude login (own session per account)

1. Call `claude_login_start`:
   ```sh
   nanolb call claude_login_start '{"label":"claude-1"}'
   ```
   The result has `login_id` and `authorize_url`.
2. HUMAN: the human opens `authorize_url`, signs in to the Claude (Pro or Max) account, and copies the code that the page shows. Ask the human to paste the code to you. The format `code#state` is accepted.
3. Call `claude_login_finish`:
   ```sh
   nanolb call claude_login_finish '{"login_id":"<login_id>","code":"<code>"}'
   ```
   The result is the new account.

The CLI shortcut is `nanolb login --claude --label claude-1`. It needs a human at the terminal to paste the code.

### 4.4 Claude, `claude setup-token`

1. HUMAN: the human runs `claude setup-token` in a terminal and finishes the browser sign-in. The command prints a token that starts with `sk-ant-oat`. The human gives the token to you.
2. Add it:
   ```sh
   nanolb accounts add-claude-token sk-ant-oat... --label claude-2
   ```
   The same is `nanolb call account_add_claude_token '{"token":"sk-ant-oat...","label":"claude-2"}'`.

The token lasts about one year. It does not refresh. It allows inference only.

### 4.5 OpenRouter key

No human browser step is needed if the human gives you the key.

```sh
nanolb accounts add-openrouter sk-or-... --label or-1
```

nanolb checks the key with `GET /key` on OpenRouter. It returns an error if OpenRouter rejects the key.

### 4.6 Warning: do not share a login with a live CLI

A refresh token can be used one time only. When nanolb refreshes a token, the old token stops working. Do not run `nanolb accounts import` on a `~/.codex/auth.json` that the Codex CLI still uses. Do not copy Claude credentials from the keychain or from `.credentials.json` if Claude Code still uses them. If you do, the CLI or nanolb loses its session. Use the logins in 4.1 to 4.3. They give nanolb its own session. `account_import` and `account_add_tokens` are for tokens that no other program uses.

### 4.7 Check the accounts

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

`~/.hermes/config.yaml`. The first provider serves gpt models with the Responses transport. The second serves claude models with the Anthropic transport. The Anthropic client adds `/v1/messages`, so its `api` has no `/v1`.

```yaml
providers:
  nanolb:
    api: http://127.0.0.1:2455/v1
    key_env: NANOLB_API_KEY
    transport: codex_responses
    default_model: gpt-6.1-sol
    discover_models: true
  nanolb-claude:
    api: http://127.0.0.1:2455
    key_env: NANOLB_API_KEY
    transport: anthropic_messages
    default_model: claude-sonnet-5-5
model:
  default: gpt-6.1-sol
  provider: custom:nanolb
```

To use Claude: `hermes chat --provider custom:nanolb-claude --model claude-sonnet-5-5`. Check the flag names with `hermes --help`.

### Codex CLI

`~/.codex/config.toml`:

```toml
model_provider = "nanolb"

[model_providers.nanolb]
name = "nanolb"
base_url = "http://127.0.0.1:2455/backend-api/codex"
wire_api = "responses"
env_key = "NANOLB_API_KEY"
supports_websockets = false
```

Then: `export NANOLB_API_KEY=nlb-...`.

### Claude Code

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:2455
export ANTHROPIC_AUTH_TOKEN=nlb-...
```

Use `claude-*` model ids. A Claude account must be connected (section 4.3 or 4.4).

### OpenAI SDK

Base URL `http://127.0.0.1:2455/v1`, API key `nlb-...`. Use `POST /v1/chat/completions` or `POST /v1/responses`. Use Codex model ids, or OpenRouter ids with `/`.

### OpenRouter SDK

Base URL `http://127.0.0.1:2455/api/v1`, API key `nlb-...`. Endpoints: `POST /api/v1/chat/completions`, `GET /api/v1/models`, `GET /api/v1/key`.

### Anthropic SDK

Base URL `http://127.0.0.1:2455`. nanolb accepts the key in `x-api-key` or `Authorization: Bearer`. Endpoints: `POST /v1/messages`, `POST /v1/messages/count_tokens`.

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

### Strategies

The strategy picks the account for a new conversation. A sticky conversation keeps its account until that account is limited or failing.

| Strategy | Behavior |
|---|---|
| `preserve` (default) | Use the least-used account first. |
| `burn` | Use the account whose weekly window resets first. |
| `round_robin` | Use the accounts in turn. |
| `priority` | Use the lowest `priority` number first. Go to the next when limited. |

### Settings fields (defaults)

`strategy` (`preserve`), `sticky` (`true`), `max_attempts` (4, accounts tried for each request), `models`, `pricing` (model to `[input, cached, output]` USD per 1M tokens), `upstream_base`, `codex_version` (`0.160.0`), `usage_poll_secs` (60), `warmup_model` (`gpt-6-luna`), `openrouter_base`, `openrouter_models`, `first_event_timeout_secs` (90), `idle_timeout_secs` (600), `claude_base`, `claude_cli_version`, `claude_models`, `claude_warmup_model` (`claude-haiku-5-5`). An unknown setting name returns an error.

### Warmup

A rate-limit window starts at the first request in it. A warmup sends a small prompt on a cron schedule (5 fields, local time) so the window starts at a known time.

```sh
nanolb accounts set-warmup <id> "0 4 * * *" [--model <model>] [--prompt <text>]
nanolb accounts warmup <id>          # fire now
nanolb accounts clear-warmup <id>
```

### Logs

nanolb writes logs to stderr. Set `NANOLB_LOG` to a tracing filter (default `info`, for example `nanolb=debug`). Set `NANOLB_LOG_FORMAT=json` for JSON lines.

Each finished request logs one line with message `request` and these fields: `endpoint` (`responses`, `chat`, `openrouter` or `anthropic`), `key`, `account`, `model`, `status`, `input`, `cached`, `output`, `reasoning`, `cost_usd`, `latency_ms`, `ttfb_ms`. Other lines: `upstream error, failing over`, `account cooling down`, `token refresh failed`, `token refreshed`, `warmup sent`, `request failed`.

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
| `account_add_claude_token` | `token`, `label?` | account |
| `login_start` | `label?` | `login_id`, `verification_url`, `user_code`, `expires_at` |
| `login_poll` | `login_id` | `status` (`pending`, `done`, `error`), `account?`, `error?` |
| `claude_login_start` | `label?` | `login_id`, `authorize_url` |
| `claude_login_finish` | `login_id`, `code` | account |
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

An account object has `id`, `kind` (`codex`, `claude` or `openrouter`), `email`, `label`, `plan`, `enabled`, `priority`, `available`, `cooldown_until`, `inflight`, `rate_limit`, `last_error` and `warmup`.

## 9. Troubleshooting

| Message or symptom | Meaning and fix |
|---|---|
| `401 invalid or missing api key` | The client key is wrong, removed or missing. Check `Authorization: Bearer nlb-...` or `x-api-key`. Run `nanolb keys ls`. |
| `401 invalid admin token` | The admin token is wrong. Read `$NANOLB_HOME/admin_token` again. It must match the server that you call. |
| `503 no codex accounts connected to serve model X` | No account of that provider. Connect one (section 4). Check the model id against section 3. A `claude*` id needs a Claude account. An id with `/` needs an OpenRouter account. |
| `400 X is served on POST /v1/messages` | A `claude*` model was sent to a non-Anthropic endpoint. Use `/v1/messages`. |
| `403 model X not allowed for this key` | The key has a `models` list. Run `nanolb keys update <id> --model X`. |
| `429 api key day limit reached` (or week, month, total) | The key hit a limit. Raise it with `keys update`, or wait for the window. |
| `429 api key disabled` or `expired` | Run `nanolb keys enable <id>` or set a new `--expires`. |
| 503 `no available accounts` (or `no available claude accounts for X`) | All accounts are disabled, cooling down or already tried. Run `nanolb accounts ls`, check `available`, `cooldown_until` and `last_error`. Run `accounts refresh <id>` to clear a cooldown. |
| Codex returns an error that the model is not supported | The model id is not valid for the ChatGPT account. nanolb does not substitute models. Pick an id from `nanolb models`. |
| Account cools down (log `account cooling down`) | The upstream returned 429 (until the limit reset, default 60 s), 401 (5 min), 5xx (5 s). nanolb tries the next account. If it stays, check `rate_limit` in `accounts ls`. |
| `token refresh failed` in logs, account unavailable | 400 or 401 on refresh is permanent: the refresh token was used elsewhere or revoked. The account cools down for 1 hour. Log in again (section 4) and remove the old account. Do not share logins (section 4.6). 5xx or network errors retry after 30 s. |
| `openrouter rejected key: 401` | The OpenRouter key is invalid. |
| `cannot bind 127.0.0.1:1455` | Port 1455 is in use during `login --browser`. Stop the other program or use the device login. |
| `login ... not found` | The server restarted, or the id is wrong. Login state is in memory. Start the login again. |
| `claude token exchange failed` | The code is wrong, expired or used. Run `claude_login_start` again and use a fresh code. |
| Poor cache hits | The client sends no stable session id. See "Sticky key" in section 6. Check `cached` in `nanolb requests`. |
| `bind host:port: ...` at start | The port is in use. Use `--port`. |
