//! Gateway: central WebSocket/HTTP server, protocol dispatch, session/node registry.
//!
//! Lifecycle:
//! 1. Load + validate config
//! 2. Resolve auth, bind address
//! 3. Start HTTP server (health, control UI, hooks)
//! 4. Attach WebSocket upgrade handler
//! 5. Start channel accounts, cron, maintenance timers
//!
//! All domain logic (agents, channels, etc.) lives in other crates and is
//! invoked through method handlers registered in `methods.rs`.

pub mod auth;
pub mod chat;
pub mod broadcast;
pub mod methods;
pub mod nodes;
pub mod pairing;
pub mod server;
pub mod services;
pub mod state;
pub mod ws;
