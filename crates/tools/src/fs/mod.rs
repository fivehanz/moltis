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
pub mod sandbox_bridge;
pub mod shared;
pub mod write;

pub use {
    edit::EditTool,
    glob::GlobTool,
    grep::GrepTool,
    multi_edit::MultiEditTool,
    read::ReadTool,
    shared::{BinaryPolicy, FsPathPolicy, FsState, new_fs_state},
    write::WriteTool,
};

use {
    crate::{checkpoints::CheckpointManager, sandbox::SandboxRouter},
    moltis_agents::tool_registry::ToolRegistry,
    std::{path::PathBuf, sync::Arc},
};

/// Aggregated configuration for fs tool registration.
///
/// Phase 1 shipped with three bare positional parameters; phase 4 keeps
/// adding knobs so the registration signature is migrated to a single
/// context struct.
#[derive(Clone)]
pub struct FsToolsContext {
    /// Default search root for `Glob`/`Grep` when the LLM omits `path`.
    /// Must be absolute. When `None`, calls without explicit `path` error.
    pub workspace_root: Option<PathBuf>,
    /// Shared per-session state for read tracking, loop detection, and
    /// must-read-before-write enforcement. `None` disables all trackers.
    pub fs_state: Option<FsState>,
    /// Allow/deny path policy. Empty policy (`None`) permits everything.
    pub path_policy: Option<FsPathPolicy>,
    /// Binary-file handling strategy for `Read`. Default is `Reject`.
    pub binary_policy: BinaryPolicy,
    /// Whether `Glob`/`Grep` honor `.gitignore` while walking. Default
    /// `true`.
    pub respect_gitignore: bool,
    /// When set, `Write`/`Edit`/`MultiEdit` call `checkpoint_path` on
    /// this manager before mutating. `None` disables checkpoints.
    pub checkpoint_manager: Option<Arc<CheckpointManager>>,
    /// Shared [`SandboxRouter`]. When set, fs tools dispatch through
    /// the [`sandbox_bridge`] for sessions the router marks as
    /// sandboxed; unsandboxed sessions still run on the host.
    pub sandbox_router: Option<Arc<SandboxRouter>>,
    /// Model context window in tokens. When set, enables `Read`'s
    /// adaptive byte cap so per-call output scales with the working
    /// set instead of using a fixed 256 KB ceiling.
    pub context_window_tokens: Option<u64>,
}

impl Default for FsToolsContext {
    fn default() -> Self {
        Self {
            workspace_root: None,
            fs_state: None,
            path_policy: None,
            binary_policy: BinaryPolicy::default(),
            // Follow the upstream default: WalkBuilder respects .gitignore
            // unless explicitly disabled.
            respect_gitignore: true,
            checkpoint_manager: None,
            sandbox_router: None,
            context_window_tokens: None,
        }
    }
}

impl FsToolsContext {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Register every native filesystem tool on a [`ToolRegistry`].
///
/// See [`FsToolsContext`] for the individual knobs. The `tools.policy`
/// allow/deny layer (per-tool names, not paths) still gates access
/// per-agent; registration is independent of authorization.
pub fn register_fs_tools(registry: &mut ToolRegistry, context: FsToolsContext) {
    let FsToolsContext {
        workspace_root,
        fs_state,
        path_policy,
        binary_policy,
        respect_gitignore,
        checkpoint_manager,
        sandbox_router,
        context_window_tokens,
    } = context;

    let mut read = ReadTool::new().with_binary_policy(binary_policy);
    if let Some(tokens) = context_window_tokens {
        read = read.with_context_window_tokens(tokens);
    }
    if let Some(ref s) = fs_state {
        read = read.with_fs_state(s.clone());
    }
    if let Some(ref p) = path_policy {
        read = read.with_path_policy(p.clone());
    }
    if let Some(ref r) = sandbox_router {
        read = read.with_sandbox_router(r.clone());
    }
    registry.register(Box::new(read));

    let mut write = WriteTool::new();
    if let Some(ref s) = fs_state {
        write = write.with_fs_state(s.clone());
    }
    if let Some(ref p) = path_policy {
        write = write.with_path_policy(p.clone());
    }
    if let Some(ref m) = checkpoint_manager {
        write = write.with_checkpoint_manager(m.clone());
    }
    if let Some(ref r) = sandbox_router {
        write = write.with_sandbox_router(r.clone());
    }
    registry.register(Box::new(write));

    let mut edit = EditTool::new();
    if let Some(ref s) = fs_state {
        edit = edit.with_fs_state(s.clone());
    }
    if let Some(ref p) = path_policy {
        edit = edit.with_path_policy(p.clone());
    }
    if let Some(ref m) = checkpoint_manager {
        edit = edit.with_checkpoint_manager(m.clone());
    }
    if let Some(ref r) = sandbox_router {
        edit = edit.with_sandbox_router(r.clone());
    }
    registry.register(Box::new(edit));

    let mut multi_edit = MultiEditTool::new();
    if let Some(ref s) = fs_state {
        multi_edit = multi_edit.with_fs_state(s.clone());
    }
    if let Some(ref p) = path_policy {
        multi_edit = multi_edit.with_path_policy(p.clone());
    }
    if let Some(ref m) = checkpoint_manager {
        multi_edit = multi_edit.with_checkpoint_manager(m.clone());
    }
    if let Some(ref r) = sandbox_router {
        multi_edit = multi_edit.with_sandbox_router(r.clone());
    }
    registry.register(Box::new(multi_edit));

    let mut glob = GlobTool::new().with_respect_gitignore(respect_gitignore);
    if let Some(ref root) = workspace_root {
        glob = glob.with_workspace_root(root.clone());
    }
    if let Some(ref p) = path_policy {
        glob = glob.with_path_policy(p.clone());
    }
    if let Some(ref r) = sandbox_router {
        glob = glob.with_sandbox_router(r.clone());
    }
    registry.register(Box::new(glob));

    let mut grep = GrepTool::new().with_respect_gitignore(respect_gitignore);
    if let Some(root) = workspace_root {
        grep = grep.with_workspace_root(root);
    }
    if let Some(p) = path_policy {
        grep = grep.with_path_policy(p);
    }
    if let Some(r) = sandbox_router {
        grep = grep.with_sandbox_router(r);
    }
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
        register_fs_tools(&mut registry, FsToolsContext {
            workspace_root,
            ..FsToolsContext::default()
        });
        registry
    }

    fn build_registry_with_state(fs_state: FsState) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, FsToolsContext {
            fs_state: Some(fs_state),
            ..FsToolsContext::default()
        });
        registry
    }

    fn build_registry_with_policy(policy: FsPathPolicy) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, FsToolsContext {
            path_policy: Some(policy),
            ..FsToolsContext::default()
        });
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
    async fn must_read_before_write_rejects_unread_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        tokio::fs::write(&path, "original").await.unwrap();

        let state = new_fs_state(true);
        let registry = build_registry_with_state(state);

        let write = registry.get("Write").unwrap();
        let value = write
            .execute(json!({
                "file_path": path.to_str().unwrap(),
                "content": "overwritten",
                "_session_key": "s1",
            }))
            .await
            .unwrap();

        assert_eq!(value["kind"], "must_read_before_write");
        // File must be unchanged.
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "original");
    }

    #[tokio::test]
    async fn must_read_before_write_allows_after_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.txt");
        tokio::fs::write(&path, "original").await.unwrap();

        let state = new_fs_state(true);
        let registry = build_registry_with_state(state);

        // Read first, same session_key.
        let read = registry.get("Read").unwrap();
        let r = read
            .execute(json!({
                "file_path": path.to_str().unwrap(),
                "_session_key": "s1",
            }))
            .await
            .unwrap();
        assert_eq!(r["kind"], "text");

        // Now Write succeeds.
        let write = registry.get("Write").unwrap();
        let w = write
            .execute(json!({
                "file_path": path.to_str().unwrap(),
                "content": "overwritten",
                "_session_key": "s1",
            }))
            .await
            .unwrap();
        assert_eq!(w["bytes_written"], 11);
    }

    #[tokio::test]
    async fn must_read_before_write_allows_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");

        let state = new_fs_state(true);
        let registry = build_registry_with_state(state);

        let write = registry.get("Write").unwrap();
        let value = write
            .execute(json!({
                "file_path": path.to_str().unwrap(),
                "content": "hello",
                "_session_key": "s1",
            }))
            .await
            .unwrap();
        // New file bypasses the check — nothing to have read yet.
        assert_eq!(value["bytes_written"], 5);
    }

    #[tokio::test]
    async fn re_read_loop_detection_fires_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot.txt");
        tokio::fs::write(&path, "content").await.unwrap();

        let state = new_fs_state(false);
        let registry = build_registry_with_state(state);
        let read = registry.get("Read").unwrap();

        for _ in 0..2 {
            let v = read
                .execute(json!({
                    "file_path": path.to_str().unwrap(),
                    "_session_key": "s1",
                }))
                .await
                .unwrap();
            assert!(v.get("loop_warning").is_none(), "warning too early: {v:?}");
        }

        let third = read
            .execute(json!({
                "file_path": path.to_str().unwrap(),
                "_session_key": "s1",
            }))
            .await
            .unwrap();
        assert!(
            third.get("loop_warning").is_some(),
            "warning missing: {third:?}"
        );
    }

    #[tokio::test]
    async fn path_policy_denies_read_and_write() {
        let dir = tempfile::tempdir().unwrap();
        let real = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let secret = real.join("secret.txt");
        let public = real.join("public.txt");
        tokio::fs::write(&secret, "top secret").await.unwrap();
        tokio::fs::write(&public, "open").await.unwrap();

        // Deny the secret file specifically.
        let policy = FsPathPolicy::new(&[], &[secret.to_string_lossy().into_owned()]).unwrap();
        let registry = build_registry_with_policy(policy);

        // Read on the denied path returns a typed path_denied payload.
        let read = registry.get("Read").unwrap();
        let r = read
            .execute(json!({ "file_path": secret.to_str().unwrap() }))
            .await
            .unwrap();
        assert_eq!(r["kind"], "path_denied");

        // Write on the denied path is rejected (file unchanged).
        let write = registry.get("Write").unwrap();
        let w = write
            .execute(json!({
                "file_path": secret.to_str().unwrap(),
                "content": "overwritten",
            }))
            .await
            .unwrap();
        assert_eq!(w["kind"], "path_denied");
        let contents = tokio::fs::read_to_string(&secret).await.unwrap();
        assert_eq!(contents, "top secret");

        // The non-denied file still works.
        let r2 = read
            .execute(json!({ "file_path": public.to_str().unwrap() }))
            .await
            .unwrap();
        assert_eq!(r2["kind"], "text");
    }

    #[tokio::test]
    async fn path_policy_allow_list_filters_glob_results() {
        let dir = tempfile::tempdir().unwrap();
        let real = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let allowed = real.join("allowed.rs");
        let blocked = real.join("blocked.rs");
        tokio::fs::write(&allowed, "// a").await.unwrap();
        tokio::fs::write(&blocked, "// b").await.unwrap();

        // Allow only *.rs files whose stem starts with "allowed".
        let allow_glob = real.join("allowed*.rs").to_string_lossy().into_owned();
        let policy = FsPathPolicy::new(&[allow_glob], &[]).unwrap();

        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, FsToolsContext {
            workspace_root: Some(real.clone()),
            path_policy: Some(policy),
            ..FsToolsContext::default()
        });

        let glob_tool = registry.get("Glob").unwrap();
        let value = glob_tool
            .execute(json!({ "pattern": "*.rs" }))
            .await
            .unwrap();
        let paths: Vec<String> = value["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("allowed.rs"));
    }

    #[tokio::test]
    async fn checkpoint_before_mutation_returns_id_and_backs_up_file() {
        use {crate::checkpoints::CheckpointManager, std::sync::Arc};

        let dir = tempfile::tempdir().unwrap();
        let real = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let target = real.join("important.txt");
        tokio::fs::write(&target, "original state").await.unwrap();

        let checkpoint_dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(CheckpointManager::new(checkpoint_dir.path().to_path_buf()));

        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, FsToolsContext {
            checkpoint_manager: Some(manager.clone()),
            ..FsToolsContext::default()
        });

        // Write should checkpoint before overwriting.
        let write = registry.get("Write").unwrap();
        let value = write
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "content": "new state",
            }))
            .await
            .unwrap();

        let checkpoint_id = value["checkpoint_id"].as_str().unwrap();
        assert!(!checkpoint_id.is_empty());

        // The file is now overwritten.
        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "new state");

        // Restore from the checkpoint and verify the original state is back.
        manager.restore(checkpoint_id).await.unwrap();
        let restored = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(restored, "original state");
    }

    #[tokio::test]
    async fn write_new_file_skips_checkpoint() {
        use {crate::checkpoints::CheckpointManager, std::sync::Arc};

        let dir = tempfile::tempdir().unwrap();
        let real = tokio::fs::canonicalize(dir.path()).await.unwrap();
        let target = real.join("brand-new.txt");

        let checkpoint_dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(CheckpointManager::new(checkpoint_dir.path().to_path_buf()));

        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, FsToolsContext {
            checkpoint_manager: Some(manager),
            ..FsToolsContext::default()
        });

        let write = registry.get("Write").unwrap();
        let value = write
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "content": "hello",
            }))
            .await
            .unwrap();

        // No checkpoint for new files — nothing to back up.
        assert!(value["checkpoint_id"].is_null());
    }

    #[tokio::test]
    async fn sandbox_read_via_registry_round_trips_through_bridge() {
        use {
            crate::{
                exec::ExecResult,
                fs::sandbox_bridge::test_helpers::MockSandbox,
                sandbox::{Sandbox, SandboxConfig, SandboxRouter},
            },
            base64::{Engine as _, engine::general_purpose::STANDARD as BASE64},
            std::sync::Arc,
        };

        // Pre-program a successful base64 response for the sandbox read.
        let content = b"hello from inside the sandbox";
        let mock = MockSandbox::new(vec![ExecResult {
            stdout: BASE64.encode(content),
            stderr: String::new(),
            exit_code: 0,
        }]);
        let backend: Arc<dyn Sandbox> = mock;
        let router = Arc::new(SandboxRouter::with_backend(
            SandboxConfig::default(),
            backend,
        ));
        router.set_override("sandboxed", true).await;

        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, FsToolsContext {
            sandbox_router: Some(router),
            ..FsToolsContext::default()
        });

        let read = registry.get("Read").unwrap();
        let value = read
            .execute(json!({
                "file_path": "/data/hello.txt",
                "_session_key": "sandboxed",
            }))
            .await
            .unwrap();

        assert_eq!(value["kind"], "text");
        assert!(
            value["content"]
                .as_str()
                .unwrap()
                .contains("hello from inside the sandbox")
        );
    }

    #[tokio::test]
    async fn sandbox_read_routes_to_host_when_session_not_sandboxed() {
        use {
            crate::{
                fs::sandbox_bridge::test_helpers::MockSandbox,
                sandbox::{Sandbox, SandboxConfig, SandboxMode, SandboxRouter},
            },
            std::sync::Arc,
        };

        // Mode = Off and no override → not sandboxed; mock never called.
        let mock = MockSandbox::new(vec![]);
        let backend: Arc<dyn Sandbox> = mock.clone();
        let cfg = SandboxConfig {
            mode: SandboxMode::Off,
            ..SandboxConfig::default()
        };
        let router = Arc::new(SandboxRouter::with_backend(cfg, backend));

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("on_host.txt");
        tokio::fs::write(&target, "hosted content").await.unwrap();

        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, FsToolsContext {
            sandbox_router: Some(router),
            ..FsToolsContext::default()
        });

        let read = registry.get("Read").unwrap();
        let value = read
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "_session_key": "not-sandboxed",
            }))
            .await
            .unwrap();

        assert_eq!(value["kind"], "text");
        assert!(
            value["content"]
                .as_str()
                .unwrap()
                .contains("hosted content")
        );
        assert!(
            mock.last_command().is_none(),
            "mock sandbox should not be called for non-sandboxed sessions"
        );
    }

    #[tokio::test]
    async fn adaptive_read_cap_shrinks_output_for_small_context_window() {
        // Build a file with enough lines to blow any small adaptive cap.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("big.txt");
        let line = "x".repeat(100);
        let content: String = std::iter::repeat_n(line.as_str(), 2000)
            .collect::<Vec<&str>>()
            .join("\n");
        tokio::fs::write(&target, &content).await.unwrap();

        // Tight context window: 8K tokens → clamps to MIN_ADAPTIVE_READ_BYTES (50 KB).
        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, FsToolsContext {
            context_window_tokens: Some(8_000),
            ..FsToolsContext::default()
        });

        let read = registry.get("Read").unwrap();
        let value = read
            .execute(json!({ "file_path": target.to_str().unwrap() }))
            .await
            .unwrap();

        assert_eq!(value["kind"], "text");
        assert_eq!(value["truncated"], true);
        let content_str = value["content"].as_str().unwrap();
        assert!(
            content_str.len() <= 50 * 1024,
            "output {} exceeded 50 KB floor",
            content_str.len()
        );
        // Must render *some* lines — small cap should still be usable.
        assert!(value["rendered_lines"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn sandbox_grep_via_registry_dispatches_through_bridge() {
        use {
            crate::{
                exec::ExecResult,
                fs::sandbox_bridge::test_helpers::MockSandbox,
                sandbox::{Sandbox, SandboxConfig, SandboxRouter},
            },
            std::sync::Arc,
        };

        let mock = MockSandbox::new(vec![ExecResult {
            stdout: "/data/lib.rs:3:fn alpha()\n/data/lib.rs:9:fn beta()\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
        }]);
        let backend: Arc<dyn Sandbox> = mock.clone();
        let router = Arc::new(SandboxRouter::with_backend(
            SandboxConfig::default(),
            backend,
        ));
        router.set_override("sandboxed", true).await;

        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, FsToolsContext {
            sandbox_router: Some(router),
            ..FsToolsContext::default()
        });

        let grep = registry.get("Grep").unwrap();
        let value = grep
            .execute(json!({
                "pattern": "fn",
                "path": "/data",
                "output_mode": "content",
                "type": "rust",
                "_session_key": "sandboxed",
            }))
            .await
            .unwrap();

        assert_eq!(value["mode"], "content");
        let matches = value["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0]["line"], 3);
        let cmd = mock.last_command().unwrap();
        assert!(cmd.contains("-n"));
        // `type=rust` → `--include='*.rs'`.
        assert!(cmd.contains("--include="));
    }

    #[tokio::test]
    async fn sandbox_write_via_registry_sends_base64_to_bridge() {
        use {
            crate::{
                exec::ExecResult,
                fs::sandbox_bridge::test_helpers::MockSandbox,
                sandbox::{Sandbox, SandboxConfig, SandboxRouter},
            },
            base64::{Engine as _, engine::general_purpose::STANDARD as BASE64},
            std::sync::Arc,
        };

        let mock = MockSandbox::new(vec![ExecResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }]);
        let backend: Arc<dyn Sandbox> = mock.clone();
        let router = Arc::new(SandboxRouter::with_backend(
            SandboxConfig::default(),
            backend,
        ));
        router.set_override("sandboxed", true).await;

        let mut registry = ToolRegistry::new();
        register_fs_tools(&mut registry, FsToolsContext {
            sandbox_router: Some(router),
            ..FsToolsContext::default()
        });

        let write = registry.get("Write").unwrap();
        let value = write
            .execute(json!({
                "file_path": "/data/out.txt",
                "content": "sandboxed write",
                "_session_key": "sandboxed",
            }))
            .await
            .unwrap();

        assert_eq!(value["bytes_written"], 15);
        let cmd = mock.last_command().unwrap();
        assert!(cmd.contains("/data/out.txt"));
        assert!(cmd.contains(&BASE64.encode(b"sandboxed write")));
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
