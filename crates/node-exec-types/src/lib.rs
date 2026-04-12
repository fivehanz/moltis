//! Core types and constants for node execution.
//!
//! This crate contains the shared types and constants used by the gateway
//! and other crates for remote node execution.

use {
    serde::{Deserialize, Serialize},
    std::collections::HashMap,
};

/// Result of a remote command execution on a node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Environment variables that are safe to forward to a remote node.
pub const SAFE_ENV_ALLOWLIST: &[&str] = &["TERM", "LANG", "COLORTERM", "NO_COLOR", "FORCE_COLOR"];

/// Environment variable prefixes that are safe to forward.
pub const SAFE_ENV_PREFIX_ALLOWLIST: &[&str] = &["LC_"];

/// Environment variable patterns that must NEVER be forwarded to a remote node.
pub const BLOCKED_ENV_PREFIXES: &[&str] = &[
    "DYLD_",
    "LD_",
    "NODE_OPTIONS",
    "PYTHON",
    "PERL",
    "RUBYOPT",
    "SHELLOPTS",
    "PS4",
    // Security-sensitive keys
    "MOLTIS_",
    "OPENAI_",
    "ANTHROPIC_",
    "AWS_",
    "GOOGLE_",
    "AZURE_",
];

/// SSH node ID prefix.
pub const SSH_ID_PREFIX: &str = "ssh:";

/// SSH target ID prefix.
pub const SSH_TARGET_ID_PREFIX: &str = "ssh:target:";

/// Generate a node ID for an SSH target.
pub fn ssh_node_id(target: &str) -> String {
    format!("{SSH_ID_PREFIX}{target}")
}

/// Generate a stored node ID from a database ID.
pub fn ssh_stored_node_id(id: i64) -> String {
    format!("{SSH_TARGET_ID_PREFIX}{id}")
}

/// Check if a node reference matches an SSH target.
pub fn ssh_target_matches(node_ref: &str, target: &str) -> bool {
    node_ref == "ssh" || node_ref == target || node_ref.strip_prefix(SSH_ID_PREFIX) == Some(target)
}

/// Filter environment variables to only include safe ones.
pub fn filter_env(env: &HashMap<String, String>) -> HashMap<String, String> {
    env.iter()
        .filter(|(key, _)| is_safe_env(key) && is_valid_env_key(key))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Check if an environment variable key is safe to forward.
pub fn is_safe_env(key: &str) -> bool {
    // Block dangerous prefixes first.
    for prefix in BLOCKED_ENV_PREFIXES {
        if key.starts_with(prefix) {
            return false;
        }
    }

    // Allow exact matches.
    if SAFE_ENV_ALLOWLIST.contains(&key) {
        return true;
    }

    // Allow prefix matches.
    for prefix in SAFE_ENV_PREFIX_ALLOWLIST {
        if key.starts_with(prefix) {
            return true;
        }
    }

    false
}

/// Check if an environment variable key has valid format.
pub fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}
