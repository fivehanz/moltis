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
| `moltis-tools` | Tool/function execution |
| `moltis-routing` | Message routing |
| `moltis-projects` | Project/workspace management |
| `moltis-oauth` | OAuth2 flows |
| `moltis-protocol` | Serializable protocol definitions |
| `moltis-common` | Shared utilities |

## License

MIT
