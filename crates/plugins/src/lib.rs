//! Plugin system: format detection, installation, and management.
//!
//! Plugins are multi-format repos (Claude Code, Codex, etc.) that are normalized
//! into the skills system. They install to `~/.moltis/installed-plugins` with
//! their own manifest at `~/.moltis/plugins-manifest.json`.

pub mod api;
pub mod formats;
pub mod hooks;
pub mod install;
pub mod loader;
pub mod provider;
