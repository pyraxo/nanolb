# Contributing to nanolb

## Build and check

Run these commands before each commit. All of them must pass.

```sh
cargo fmt --check
cargo clippy --release -- -D warnings
cargo test
```

`rustfmt.toml` sets the line width to 140. `clippy.toml` sets the lint limits.

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

The scope is optional. Use the module name: `proxy`, `codex`, `claude`, `openrouter`, `state`, `store`, `admin`, `mcp`, `cli`, `tui`, `translate`, `pricing`.

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
feat(claude): route claude models to Claude subscription accounts

Claude Code requests for claude-* models now go to connected Claude
accounts through /v1/messages. Requests stay on one account per
session, so the prompt cache stays warm.
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
