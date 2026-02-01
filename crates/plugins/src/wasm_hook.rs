//! WASM hook handler (future implementation).
//!
//! This module will support WebAssembly-based hook handlers via `wasmtime`,
//! providing near-native performance with sandboxed execution.
//!
//! **Status**: Stub — not yet implemented. Shell hooks should be solid before
//! investing in WASM support.
//!
//! ## Planned Design
//!
//! ```text
//! [HOOK.md]
//! +++
//! name = "my-wasm-hook"
//! events = ["BeforeToolCall"]
//! wasm = "./handler.wasm"   # <-- instead of `command`
//! +++
//! ```
//!
//! The handler WASM module would export:
//! - `handle(event_json_ptr, event_json_len) -> action_json_ptr`
//!
//! Memory management via the component model or a simple allocator protocol.
