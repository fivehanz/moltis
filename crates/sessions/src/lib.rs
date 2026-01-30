//! Session storage and management.
//!
//! Sessions are stored as JSONL files (one message per line) at
//! ~/.clawdbot/agents/<agentId>/sessions/<sessionKey>.jsonl
//! with file locking for concurrent access.

pub mod compaction;
pub mod key;
pub mod metadata;
pub mod store;

pub use key::SessionKey;
