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

/// Canonical list of tool names registered by [`register_fs_tools`].
pub const FS_TOOL_NAMES: &[&str] = &["Read", "Write", "Edit", "MultiEdit", "Glob", "Grep"];

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod contract_tests {
    //! End-to-end contract tests that drive each fs tool through
    //! `ToolRegistry::register` + `AgentTool::execute`, mirroring the
    //! gateway's actual call path. These catch registration regressions
    //! and schema drift that the per-module unit tests can miss (they
    //! bypass trait-object dispatch by calling impl methods directly).

    use {super::*, serde_json::json};

    fn build_registry(workspace_root: Option<PathBuf>) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, workspace_root);
        registry
    }

    #[test]
    fn register_fs_tools_adds_all_six_names() {
        let registry = build_registry(None);
        let names = registry.list_names();
        for expected in FS_TOOL_NAMES {
            assert!(
                names.iter().any(|n| n == expected),
                "missing tool: {expected}. Got: {names:?}"
            );
        }
    }

    #[test]
    fn each_tool_has_a_parameters_schema_with_pattern_or_file_path() {
        let registry = build_registry(None);
        for name in FS_TOOL_NAMES {
            let tool = registry.get(name).unwrap();
            let schema = tool.parameters_schema();
            assert_eq!(schema["type"], "object", "{name} schema must be an object");
            let props = schema["properties"].as_object().expect("properties");
            let has_key = props.contains_key("file_path") || props.contains_key("pattern");
            assert!(has_key, "{name} must declare file_path or pattern");
        }
    }

    #[tokio::test]
    async fn read_write_edit_multi_edit_via_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contract.txt");
        let path_str = path.to_str().unwrap().to_string();
        let registry = build_registry(None);

        // Write via the registry.
        let write = registry.get("Write").unwrap();
        let w = write
            .execute(json!({ "file_path": &path_str, "content": "alpha beta gamma" }))
            .await
            .unwrap();
        assert_eq!(w["bytes_written"], 16);

        // Read back.
        let read = registry.get("Read").unwrap();
        let r = read
            .execute(json!({ "file_path": &path_str }))
            .await
            .unwrap();
        assert_eq!(r["kind"], "text");
        assert!(r["content"].as_str().unwrap().contains("alpha"));

        // Edit — unique replacement.
        let edit = registry.get("Edit").unwrap();
        let e = edit
            .execute(json!({
                "file_path": &path_str,
                "old_string": "beta",
                "new_string": "BETA",
            }))
            .await
            .unwrap();
        assert_eq!(e["replacements"], 1);

        // MultiEdit — sequential edits.
        let multi = registry.get("MultiEdit").unwrap();
        let m = multi
            .execute(json!({
                "file_path": &path_str,
                "edits": [
                    { "old_string": "alpha", "new_string": "ALPHA" },
                    { "old_string": "gamma", "new_string": "GAMMA" }
                ]
            }))
            .await
            .unwrap();
        assert_eq!(m["edits_applied"], 2);

        // Final state.
        let final_read = read
            .execute(json!({ "file_path": &path_str }))
            .await
            .unwrap();
        assert!(
            final_read["content"]
                .as_str()
                .unwrap()
                .contains("ALPHA BETA GAMMA")
        );
    }

    #[tokio::test]
    async fn glob_and_grep_via_registry_with_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("one.rs"), "fn alpha() {}")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("two.rs"), "fn beta() {}")
            .await
            .unwrap();

        let registry = build_registry(Some(dir.path().to_path_buf()));

        let glob = registry.get("Glob").unwrap();
        let g = glob.execute(json!({ "pattern": "*.rs" })).await.unwrap();
        let paths = g["paths"].as_array().unwrap();
        assert_eq!(paths.len(), 2);

        let grep = registry.get("Grep").unwrap();
        let gr = grep
            .execute(json!({ "pattern": "alpha", "output_mode": "content", "-n": true }))
            .await
            .unwrap();
        let matches = gr["matches"].as_array().unwrap();
        assert!(!matches.is_empty());
    }

    #[tokio::test]
    async fn typed_not_found_survives_registry_dispatch() {
        let registry = build_registry(None);
        let read = registry.get("Read").unwrap();
        let v = read
            .execute(json!({ "file_path": "/tmp/does-not-exist-contract-99aa1" }))
            .await
            .unwrap();
        assert_eq!(v["kind"], "not_found");
    }
}
