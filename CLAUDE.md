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

## Web UI Assets

Assets live in `crates/gateway/src/assets/` (JS, CSS, HTML). The gateway
serves them in two modes:

- **Dev (filesystem)**: When `cargo run` detects the source tree, assets are
  served directly from disk. Edit JS/CSS and reload the browser — no Rust
  recompile needed. You can also set `MOLTIS_ASSETS_DIR` to point elsewhere.
- **Release (embedded)**: When the binary runs outside the repo, assets are
  served from the copy embedded at compile time via `include_dir!`. URLs are
  versioned (`/assets/v/<hash>/...`) with immutable caching; the hash changes
  automatically on each build.

When editing JavaScript files, run `biome check --write` to lint and format
them. No separate asset build step is required.

**HTML in JS**: Avoid creating HTML elements from JavaScript. Instead, add
hidden elements in `index.html` (with `style="display:none"`) and have JS
toggle their visibility. This keeps markup in HTML where it belongs and makes
the structure easier to inspect. Preact components (HTM templates) are the
exception — they use `html` tagged templates by design.

### Server-Injected Data (gon pattern)

When the frontend needs server-side data **at page load** (before any async
fetch completes), use the gon pattern instead of inline `<script>` DOM
manipulation or extra API calls:

**Rust side** — add a field to `GonData` in `server.rs` and populate it in
`build_gon_data()`. The struct is serialized and injected into `<head>` as
`<script>window.__MOLTIS__={...};</script>` on every page serve. Only put
request-independent data here (no cookies, no sessions — those still need
`/api/auth/status`).

```rust
// server.rs
#[derive(serde::Serialize)]
struct GonData {
    identity: moltis_config::ResolvedIdentity,
    // add new fields here
}
```

**JS side** — import `gon.js`:

```js
import * as gon from "./gon.js";

// Read server-injected data synchronously at module load.
var identity = gon.get("identity");

// React to changes (from set() or refresh()).
gon.onChange("identity", (id) => { /* update DOM */ });

// After a mutation (e.g. saving identity), refresh all gon data
// from the server. This re-fetches /api/gon and notifies all
// onChange listeners — no need to update specific fields manually.
gon.refresh();
```

**Do NOT**: inject inline `<script>` tags with `document.getElementById`
calls, build HTML strings in Rust, or use `body.replace` for DOM side effects.
All of those are fragile. The gon blob is the single injection point.
When data changes at runtime, call `gon.refresh()` instead of manually
updating individual fields — it keeps everything consistent.

## Authentication Architecture

The gateway supports password and passkey (WebAuthn) authentication, managed
in `crates/gateway/src/auth.rs` with routes in `auth_routes.rs` and middleware
in `auth_middleware.rs`.

Key concepts:

- **Setup code**: On first run (no password set), a random code is printed to
  the terminal. The user enters it on the `/setup` page to set a password or
  register a passkey. The code is single-use and cleared after setup.
- **Auth states**: `auth_disabled` (explicit `[auth] disabled = true` in
  config) and localhost-no-password (safe default) are distinct states.
  `auth_disabled` is a deliberate user choice; localhost-no-password is the
  initial state before setup.
- **Session cookies**: HTTP-only `moltis_session` cookie, validated by the
  auth middleware.
- **API keys**: Bearer token auth via `Authorization: Bearer <key>` header,
  managed through the settings UI.
- **Credential store**: `CredentialStore` in `auth.rs` persists passwords
  (argon2 hashed), passkeys, API keys, and session tokens to a JSON file.

The auth middleware (`RequireAuth`) protects all `/api/*` routes except
`/api/auth/*` and `/api/gon`.

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
biome check --write      # Lint & format JavaScript files (installed via mise)
```

When editing `Cargo.toml` or other TOML files, run `taplo fmt` to format them
according to the project's `taplo.toml` configuration.

## CLI Auth Commands

The `auth` subcommand (`crates/cli/src/auth_commands.rs`) provides:

- `moltis auth reset-password` — clear the stored password
- `moltis auth reset-identity` — clear identity and user profile (triggers
  onboarding on next load)

## Sensitive Data Handling

Never use plain `String` for passwords, API keys, tokens, or any secret
material. Use `secrecy::Secret<String>` instead — it redacts `Debug` output,
prevents accidental `Display`, and zeroes memory on drop.

```rust
use secrecy::{ExposeSecret, Secret};

// Store secrets wrapped
struct Config {
    api_key: Secret<String>,
}

// Construct: wrap at the boundary
let cfg = Config { api_key: Secret::new(raw_key) };

// Use: expose only at the point of consumption
req.header("Authorization", format!("Bearer {}", cfg.api_key.expose_secret()));
```

Rules:
- **Struct fields** holding secrets must be `Secret<String>` (or
  `Option<Secret<String>>`).
- **Function parameters** can stay `&str`; call `.expose_secret()` at the call
  site.
- **Serde deserialize** works automatically (secrecy's `serde` feature).
- **Serde serialize** requires a custom helper when round-tripping is needed
  (config files, token storage). See `serialize_secret` /
  `serialize_option_secret` in `crates/oauth/src/types.rs`.
- **Debug impls**: replace `#[derive(Debug)]` with a manual impl that prints
  `[REDACTED]` for secret fields.
- **RwLock guards**: when a `RwLock<Option<Secret<String>>>` read guard is
  followed by a write in the same function, scope the read guard in a block
  `{ let guard = lock.read().await; ... }` to avoid deadlocks.

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

When adding a new feature (`feat` commits), update the features list in
`README.md` as part of the same branch/PR.

**You MUST run all checks before every commit and fix any issues they report:**
1. `cargo +nightly fmt --all` — format all Rust code (CI runs `cargo fmt --all -- --check`)
2. `cargo +nightly clippy --all-targets --all-features -- -D warnings` — run linter (must pass with zero warnings)
3. `cargo test --all-features` — run all tests
4. `biome check --write` (when JS files were modified; CI runs `biome ci`)
5. `taplo fmt` (when TOML files were modified)
