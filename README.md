# Moltis

[![CI](https://github.com/penso/moltis/actions/workflows/ci.yml/badge.svg)](https://github.com/penso/moltis/actions/workflows/ci.yml)

A personal AI gateway written in Rust. Moltis provides a unified interface to
multiple LLM providers and communication channels, inspired by
[OpenClaw](https://docs.openclaw.ai).

## Features

- **Multi-provider LLM support** — OpenAI, Anthropic, GitHub Copilot, and more
  through a trait-based provider architecture
- **Streaming responses** — real-time token streaming for a responsive user
  experience
- **Communication channels** — Telegram integration with an extensible channel
  abstraction for adding others
- **Web gateway** — HTTP and WebSocket server with a built-in web UI
- **Session persistence** — SQLite-backed conversation history and session
  management
- **Memory and knowledge base** — embeddings-powered long-term memory
- **Skills and plugins** — extensible skill system and plugin architecture
- **Hook system** — lifecycle hooks with priority ordering, parallel dispatch
  for read-only events, circuit breaker, dry-run mode, HOOK.md-based discovery,
  eligibility checks, bundled hooks (boot-md, session-memory, command-logger),
  and CLI management (`moltis hooks list/info`)
- **Scheduled tasks** — cron-based task execution
- **OAuth flows** — built-in OAuth2 for provider authentication
- **TLS support** — automatic self-signed certificate generation
- **Observability** — OpenTelemetry tracing with OTLP export

## Getting Started

### Build

```bash
cargo build              # Debug build
cargo build --release    # Optimized build
```

### Run

```bash
cargo run -- gateway     # Start the gateway server
```

### Test

```bash
cargo test --all-features
```

## Hooks

Moltis includes a hook dispatch system that lets you react to lifecycle events
with native Rust handlers or external shell scripts. Hooks can observe, modify,
or block actions.

### Events

`BeforeToolCall`, `AfterToolCall`, `BeforeAgentStart`, `AgentEnd`,
`MessageReceived`, `MessageSending`, `MessageSent`, `BeforeCompaction`,
`AfterCompaction`, `ToolResultPersist`, `SessionStart`, `SessionEnd`,
`GatewayStart`, `GatewayStop`, `Command`

### Hook discovery

Hooks are discovered from `HOOK.md` files in these directories (priority order):

1. `<workspace>/.moltis/hooks/<name>/HOOK.md` — project-local
2. `~/.moltis/hooks/<name>/HOOK.md` — user-global

Each `HOOK.md` uses TOML frontmatter:

```toml
+++
name = "my-hook"
description = "What it does"
events = ["BeforeToolCall"]
command = "./handler.sh"
timeout = 5

[requires]
os = ["darwin", "linux"]
bins = ["jq"]
env = ["SLACK_WEBHOOK_URL"]
+++
```

### CLI

```bash
moltis hooks list              # List all discovered hooks
moltis hooks list --eligible   # Show only eligible hooks
moltis hooks list --json       # JSON output
moltis hooks info <name>       # Show hook details
```

### Bundled hooks

- **boot-md** — reads `BOOT.md` from workspace on `GatewayStart`
- **session-memory** — saves session context on `/new` command
- **command-logger** — logs all `Command` events to JSONL

### Shell hook protocol

Shell hooks receive the event payload as JSON on stdin and communicate their
action via exit code and stdout:

| Exit code | Stdout | Action |
|-----------|--------|--------|
| 0 | (empty) | Continue |
| 0 | `{"action":"modify","data":{...}}` | Replace payload data |
| 1 | — | Block (stderr used as reason) |

### Configuration

```toml
[[hooks]]
name = "audit-tool-calls"
command = "./examples/hooks/log-tool-calls.sh"
events = ["BeforeToolCall"]

[[hooks]]
name = "block-dangerous"
command = "./examples/hooks/block-dangerous-commands.sh"
events = ["BeforeToolCall"]
timeout = 5

[[hooks]]
name = "notify-discord"
command = "./examples/hooks/notify-discord.sh"
events = ["SessionEnd"]
env = { DISCORD_WEBHOOK_URL = "https://discord.com/api/webhooks/..." }
```

See `examples/hooks/` for ready-to-use scripts (logging, blocking dangerous
commands, Slack/Discord notifications, secret redaction, session saving).

## Project Structure

Moltis is organized as a Cargo workspace with the following crates:

| Crate | Description |
|-------|-------------|
| `moltis-cli` | Command-line interface and entry point |
| `moltis-gateway` | HTTP/WebSocket server and web UI |
| `moltis-agents` | LLM provider integrations |
| `moltis-channels` | Communication channel abstraction |
| `moltis-telegram` | Telegram integration |
| `moltis-config` | Configuration management |
| `moltis-sessions` | Session persistence |
| `moltis-memory` | Embeddings-based knowledge base |
| `moltis-skills` | Skill/plugin system |
| `moltis-plugins` | Plugin formats, hook handlers, and shell hook runtime |
| `moltis-tools` | Tool/function execution |
| `moltis-routing` | Message routing |
| `moltis-projects` | Project/workspace management |
| `moltis-oauth` | OAuth2 flows |
| `moltis-protocol` | Serializable protocol definitions |
| `moltis-common` | Shared utilities |

## License

MIT
