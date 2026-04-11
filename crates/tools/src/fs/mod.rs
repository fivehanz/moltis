//! Native filesystem tools: `Read`, `Write`, `Edit`, `MultiEdit`, `Glob`, `Grep`.
//!
//! These are the structured, typed alternative to shell-based file I/O via
//! `exec`. They match Claude Code's tool schemas exactly so LLMs trained on
//! those tools encounter the same shape of parameters and responses.
//!
//! See GH moltis-org/moltis#657 for context.
//!
//! Phase 1 (this module) covers host-path execution only. Sandbox routing
//! arrives in phase 2, UX polish (adaptive paging, edit recovery, re-read
//! detection) in phase 3, and operator-facing `[tools.fs]` config in phase 4.

pub mod edit;
pub mod glob;
pub mod grep;
pub mod multi_edit;
pub mod read;
pub mod shared;
pub mod write;

pub use {
    edit::EditTool, glob::GlobTool, grep::GrepTool, multi_edit::MultiEditTool, read::ReadTool,
    write::WriteTool,
};

use {moltis_agents::tool_registry::ToolRegistry, std::path::PathBuf};

/// Register every native filesystem tool on a [`ToolRegistry`].
///
/// `workspace_root`, when set, is used as the default search root for
/// `Glob` and `Grep` calls that omit the `path` argument. All fs tools
/// still require absolute paths for any explicit `file_path` / `path`
/// argument — the workspace root only affects the default for
/// `Glob`/`Grep`, never silently resolves relative paths.
///
/// The `tools.policy` allow/deny layer still gates access per-agent, so
/// registration is independent of authorization.
pub fn register_fs_tools(registry: &mut ToolRegistry, workspace_root: Option<PathBuf>) {
    registry.register(Box::new(ReadTool::new()));
    registry.register(Box::new(WriteTool::new()));
    registry.register(Box::new(EditTool::new()));
    registry.register(Box::new(MultiEditTool::new()));

    let glob = match workspace_root.clone() {
        Some(root) => GlobTool::new().with_workspace_root(root),
        None => GlobTool::new(),
    };
    registry.register(Box::new(glob));

    let grep = match workspace_root {
        Some(root) => GrepTool::new().with_workspace_root(root),
        None => GrepTool::new(),
    };
    registry.register(Box::new(grep));
}
