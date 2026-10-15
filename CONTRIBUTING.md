# Contributing to nanolb

## Build and check

Run these commands before each commit. All of them must pass.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 scripts/compat.py check
```

- `rustfmt.toml` sets the line width to 140.
- `Cargo.toml` (`[lints]`) sets the lint policy: `clippy::pedantic`, selected `nursery` and `restriction` lints (no `unwrap` or `expect` outside tests), and `unsafe_code = "forbid"`.
- `clippy.toml` sets the lint limits.
- For a local exception, use `#[expect(lint, reason = "...")]`, not `#[allow]`.
- `scripts/compat.py check` compares the external interface (CLI help, routes, admin ops, MCP tools, settings, DB schema, error bodies) with `scripts/compat/baseline/`. When you change that interface on purpose, run `python3 scripts/compat.py baseline` and commit the new baseline with the change.

## Layout

- `src/main.rs` only calls `nanolb::run()`. All code is in the library (`src/lib.rs`).
- Each directory is one layer: `app/` (state), `store/` (SQLite), `upstream/` (Codex, OpenRouter), `server/` (`http`, `ws`, `endpoints`), `translate/`, `control/` (admin, ops, MCP) and `cli/`.
- Each layer has its own error type (`thiserror`). The CLI uses `anyhow`.
- Use `pub(crate)` by default. `unreachable_pub` enforces it.
- Keep files at about 300 lines or fewer. Put unit tests next to the code. Put router-level tests in `tests/`, with shared helpers in `tests/common/`.

## Code rules

- Speed is the first priority. Do not add work to the request path. Do not read from SQLite on the request path. Write through the background writer.
- Use a well-established crate before you write your own code for parsing, protocols, OAuth, cron or terminal UI.
- Do not write code comments. Put the reason for a change in the commit message.
- Keep tests few and strong. Test behavior through the public interface: a pure function, the router or the CLI. Do not mock code that this repo owns. Do not add a test that checks that removed code stays removed.
- Do not substitute one model for another. The model id selects the provider. See [AGENTS.md](AGENTS.md).
- Never log secrets: tokens, API keys, or OAuth codes.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).

```
<type>(<scope>): <subject>

<body>

<footer>
```

### Type

| Type | Use for |
|---|---|
| `feat` | A new feature for users or agents |
| `fix` | A bug fix |
| `perf` | A change that makes nanolb faster or smaller |
| `refactor` | A code change that does not change behavior |
| `test` | Adding or changing tests only |
| `docs` | Documentation only |
| `build` | Cargo, dependencies, or the release profile |
| `ci` | CI configuration |
| `chore` | Other maintenance |

### Scope

The scope is optional. Use the module name: `proxy`, `codex`, `openrouter`, `state`, `store`, `admin`, `mcp`, `cli`, `tui`, `translate`, `pricing`.

### Subject

- Use the imperative mood: "add", not "added" or "adds".
- Start with a lowercase letter. Do not end with a period.
- Keep the first line at 72 characters or fewer.

### Body

- Tell why you made the change and what it changes for the user. Do not repeat the diff.
- Wrap lines at 72 characters.
- Separate the body from the subject with a blank line.

### Footer

- Put `BREAKING CHANGE: <description>` in the footer when a change breaks the CLI, the admin API, the MCP tools, or the stored data format. You can also add `!` after the type, for example `feat(admin)!: rename key_create`.
- Refer to issues as `Refs: #12` or `Closes: #12`.

### Examples

```
feat(openrouter): route vendor-prefixed models to OpenRouter accounts

A model id with a slash now goes to a connected OpenRouter account.
Requests stay on one account per session, so the prompt cache stays
warm.
```

```
fix(codex): read the weekly window from the primary slot

Pro plans report only a weekly window, in the primary slot. The
burn strategy read the secondary slot and saw no reset time.
```

## Commit identity

Make all commits as `pyraxo <pyraxo@pyraxo.moe>`. Set this in your clone:

```sh
git config user.name "pyraxo"
git config user.email "pyraxo@pyraxo.moe"
```

## Scope of a commit

- Make one logical change in each commit.
- Each commit must build and pass the checks.
- Put a refactor in a separate commit from a behavior change.
