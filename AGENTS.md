# AGENTS.md

Guidance for AI agents that change nanolb's code. To install or operate nanolb, read [AGENTS_README.md](AGENTS_README.md) instead.

## What nanolb is

nanolb is a load balancer for Codex (ChatGPT) subscription accounts, with OpenRouter as a second upstream. Users point the Codex CLI, Hermes, Claude Code and OpenAI-style SDKs at it, and nanolb spreads their traffic over the accounts they own. People and agents control it through the CLI, the admin RPC API and MCP. There is no web UI, and the project does not want one.

Speed is the product. On a local benchmark nanolb adds about 0.1 ms per request and sustains tens of thousands of requests per second. A change that makes the request path slower needs a measured reason.

## Checks

Every commit passes all four:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 scripts/compat.py check
```

`scripts/compat.py check` starts a scratch server and compares the external interface with `scripts/compat/baseline/`. The interface includes CLI help, routes, admin ops, MCP tools, `settings_get`, the DB schema and client error bodies. When you change that interface on purpose, run `python3 scripts/compat.py baseline` and commit the new baseline in the same commit. A difference you did not intend is a regression.

## Layout

`src/main.rs` only calls `nanolb::run()`. Each directory under `src/` is one layer:

| Path | Responsibility |
|---|---|
| `app/` | In-memory state: accounts, keys, sticky sessions, file owners, logins, the model catalog, account picking (`pick.rs`) and settings, read without a lock through `ArcSwap` |
| `store/` | SQLite. Reads at startup and for admin queries. Writes go through one background writer thread (`writer.rs`) |
| `upstream/codex/` | ChatGPT backend: OAuth and device login, token refresh, request building, rate-limit parsing, the usage and models endpoints, warmups and the background poller |
| `upstream/openrouter.rs` | OpenRouter requests and key checks |
| `server/http/` | Client-facing HTTP: auth, failover (`failover.rs`), stream relay (`relay.rs`), sticky session keys, model lists |
| `server/ws/` | Responses-over-WebSocket relay and the realtime voice relays |
| `server/endpoints/` | Usage, files, transcription, images and embeddings routes |
| `translate/` | Chat Completions and Anthropic Messages to and from the Responses API, including the streaming transcoders |
| `control/` | Admin ops, the ops features (sessions, reset credits, planner, analytics) and the MCP server |
| `cli/` | The `nanolb` CLI, login flows, `up`/`down`, `service install` and the TUI |
| `pricing.rs` | Model prices used for cost accounting |

A request flows like this: `server/auth.rs` checks the API key, a `server/http` handler prepares the body and session key, `app` picks an account, `upstream` builds the request, and `relay.rs` streams events back while it records usage. Usage rows reach SQLite through the writer thread, never on the request path.

## Constraints and their reasons

- **The request path does not read SQLite, take extra locks or allocate per stream event.** Load settings once per request and pass `&Settings` down. Resolve the API key once in `require_key`; handlers read it from the request extensions.
- **The model the client names is the model that runs.** The model id only selects the provider: an id with `/` goes to OpenRouter, every other id goes to Codex. nanolb never maps one model to another. Users rely on getting exactly what they asked for.
- **Requests pass through unchanged, except for what the upstream requires.** Do not make one client's traffic look like another client's. The project decided against that kind of impersonation when it removed the Claude account upstream.
- **Prompt caching depends on stickiness.** A conversation stays on one account, keyed by its session header, `prompt_cache_key` or a hash of its prefix. nanolb sends a `session_id` header upstream, because the Codex backend keys its cache on it. A change that moves conversations between accounts without need costs users money.
- **Refresh tokens work only once.** Token refresh runs in a spawned task under the account's refresh lock, so a dropped client request cannot lose a rotated token. Keep that shape. For the same reason, never run a development build against a real `~/.nanolb`: two processes refreshing the same account log it out. Use a scratch `NANOLB_HOME` and the mock upstreams in `tests/common/`.
- **Failover happens only before the first byte.** Rate limits, 5xx responses and connect errors put an account in cooldown and retry on another account. Request errors (4xx caused by the client) go back to the client and leave the account in rotation.
- **Client-visible error bodies and status codes are part of the interface.** Each layer has its own `thiserror` type, and `server/error.rs` (`ApiError`) renders the right shape for each client format. The compat check covers these bodies.

## Code style

- Do not write code comments. The reason for a change goes in the commit message.
- Use `pub(crate)` by default. The `unreachable_pub` lint enforces it.
- Fix lints instead of silencing them. For a justified local exception, use `#[expect(lint, reason = "...")]`, never `#[allow]`.
- Keep files at about 300 lines or fewer. Split by responsibility, not by size alone.
- Prefer a well-established crate to hand-written code for protocols, parsing, OAuth, cron and terminal UI.

## Tests

Write few tests, and make each one check behavior a user or client can observe through a public interface. Unit tests live next to their code. Router-level tests live in `tests/` and use the shared harness and mock upstreams in `tests/common/`. Do not mock code that this repository owns, and do not add a test that only checks that removed code stays removed. A bug fix gets one test that fails without the fix.

## Upstream facts

The Codex wire protocol is not documented by OpenAI. When you need to know what the ChatGPT backend expects (headers, body fields, rate-limit headers, WebSocket frames), read the `openai/codex` source (`codex-rs`). Do not guess. nanolb reports the latest `@openai/codex` npm release as its client version (`codex_version = "auto"`), because the backend lists newer models only for recent clients.

## Commits

Use Conventional Commits, as described in [CONTRIBUTING.md](CONTRIBUTING.md), and commit as `pyraxo <pyraxo@pyraxo.moe>`. Make one logical change per commit, and keep every commit green on the four checks.
