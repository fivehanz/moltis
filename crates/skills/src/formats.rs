//! Plugin format enum shared between skills and plugins crates.

use serde::{Deserialize, Serialize};

/// Detected format of a plugin repository.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginFormat {
    /// Native `SKILL.md` format (single or multi-skill repo).
    #[default]
    Skill,
    /// Claude Code plugin: `.claude-plugin/plugin.json` + `agents/`, `commands/`, `skills/` dirs.
    ClaudeCode,
    /// Codex plugin: `codex-plugin.json` or `.codex/plugin.json` (future).
    Codex,
    /// Fallback: `.md` files treated as generic skill prompts.
    Generic,
}

impl std::fmt::Display for PluginFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Skill => write!(f, "skill"),
            Self::ClaudeCode => write!(f, "claude_code"),
            Self::Codex => write!(f, "codex"),
            Self::Generic => write!(f, "generic"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plugin_format_display() {
        assert_eq!(PluginFormat::Skill.to_string(), "skill");
        assert_eq!(PluginFormat::ClaudeCode.to_string(), "claude_code");
        assert_eq!(PluginFormat::Codex.to_string(), "codex");
        assert_eq!(PluginFormat::Generic.to_string(), "generic");
    }
}
