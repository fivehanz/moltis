# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## General

This is doing a Rust version of openclaw. Openclaw documentation is available at
https://docs.openclaw.ai and its code is at https://github.com/openclaw/openclaw

Dig this repo and documentation to figure out how moltbot is working and how
many features it has. `../clawdbot/HOWITWORKS.md` has explaination of how it
works. But feel free to do any improvement and change the way it is to make
it more Rustacean.

Always use traits if possible, to allow other implementations.

Always prefer streaming over non-streaming API calls when possible. Streaming
provides a better, friendlier user experience by showing responses as they
arrive.

All code you write must have test with a high coverage.

## Build and Development Commands

```bash
cargo build              # Build the project
cargo build --release    # Build with optimizations
cargo run                # Run the project
cargo run --release      # Run with optimizations
```

## Testing

```bash
cargo test                           # Run all tests
cargo test <test_name>               # Run a specific test
cargo test <module>::               # Run all tests in a module
cargo test -- --nocapture            # Run tests with stdout visible
```

## Code Quality

```bash
cargo +nightly fmt       # Format code (uses nightly)
cargo +nightly clippy    # Run linter (uses nightly)
cargo check              # Fast compile check without producing binary
taplo fmt                # Format TOML files (Cargo.toml, etc.)
```

When editing `Cargo.toml` or other TOML files, run `taplo fmt` to format them
according to the project's `taplo.toml` configuration.

## Provider Implementation Guidelines

### Async all the way down

Never use `block_on`, `std::thread::scope` + `rt.block_on`, or any blocking
call inside an async context (tokio runtime). This causes a panic:
"Cannot start a runtime from within a runtime". All token exchanges,
HTTP calls, and I/O in provider methods (`complete`, `stream`) must be `async`
and use `.await`. If a helper needs to make HTTP requests, make it `async fn`.

### Model lists for providers

When adding a new LLM provider, make the model list as complete as possible.
Models vary by plan/org and can change, so keep the list intentionally broad —
if a model isn't available the provider API will return an error and the user
can remove it from their config.

To find the correct model IDs:
- Check the upstream open-source implementations in `../clawdbot/` (TypeScript
  reference), as well as projects like OpenAI Codex CLI, Claude Code, opencode,
  etc.
- For "bring your own model" providers (OpenRouter, Venice, Ollama), don't
  hardcode a model list — require the user to specify a model via config.
- Ideally, query the provider's `/models` endpoint at registration time to
  build the list dynamically (not yet implemented).

## Plans and Session History

Plans are stored in `prompts/` (configured via `.claude/settings.json`).
When entering plan mode, plans are automatically saved there. After completing
a significant piece of work, write a brief session summary to
`prompts/session-YYYY-MM-DD-<topic>.md` capturing what was done, key decisions,
and any open items.

## Git Workflow

Follow conventional commit format: `feat|fix|refactor|docs|test|chore(scope): description`

Run all checks before committing:
1. `cargo check`
2. `cargo +nightly clippy`
3. `cargo test`
