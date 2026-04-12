//! `Read` tool — typed, line-numbered file reads.
//!
//! Matches Claude Code's `Read` tool schema: `file_path`, optional `offset`
//! and `limit`. Returns a structured payload with `content` (cat -n style),
//! `total_lines`, and `truncated` flags so the LLM can tell whether it has
//! the full file.

use {
    async_trait::async_trait,
    moltis_agents::tool_registry::AgentTool,
    serde_json::{Value, json},
    std::{path::Path, sync::Arc},
    tokio::fs,
    tracing::instrument,
};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, labels, tools as tools_metrics};

use crate::{
    Result,
    error::Error,
    fs::{
        sandbox_bridge::{SandboxReadResult, ensure_sandbox, sandbox_read},
        shared::{
            BinaryPolicy, DEFAULT_MAX_READ_BYTES, DEFAULT_READ_LINE_LIMIT, FsErrorKind,
            FsPathPolicy, FsState, MAX_READ_OUTPUT_BYTES, READ_LOOP_THRESHOLD,
            compute_adaptive_read_cap, enforce_path_policy, format_numbered_lines_with_cap,
            fs_error_payload, io_error_to_typed_payload, is_binary_extension, looks_binary,
            require_absolute, session_key_from,
        },
    },
    sandbox::SandboxRouter,
};

/// Native `Read` tool implementation.
#[derive(Default)]
pub struct ReadTool {
    fs_state: Option<FsState>,
    path_policy: Option<FsPathPolicy>,
    binary_policy: BinaryPolicy,
    sandbox_router: Option<Arc<SandboxRouter>>,
    /// Override for the file-size gate. `None` → `DEFAULT_MAX_READ_BYTES`.
    max_read_bytes: Option<u64>,
    /// Optional context window in tokens. When set, Read's byte cap
    /// scales adaptively with the model's working set
    /// (`ctx * 4 chars * 0.2`, clamped to `[50 KB, 512 KB]`).
    context_window_tokens: Option<u64>,
}

impl ReadTool {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a shared [`FsState`] for per-session read tracking and
    /// re-read loop detection.
    #[must_use]
    pub fn with_fs_state(mut self, state: FsState) -> Self {
        self.fs_state = Some(state);
        self
    }

    /// Attach an allow/deny path policy.
    #[must_use]
    pub fn with_path_policy(mut self, policy: FsPathPolicy) -> Self {
        self.path_policy = Some(policy);
        self
    }

    /// Override the binary-file handling policy. Default is
    /// [`BinaryPolicy::Reject`] which returns a typed marker without
    /// content.
    #[must_use]
    pub fn with_binary_policy(mut self, policy: BinaryPolicy) -> Self {
        self.binary_policy = policy;
        self
    }

    /// Attach a shared [`SandboxRouter`]. When the router marks a
    /// session as sandboxed, Read dispatches through the bridge
    /// instead of touching the host filesystem.
    #[must_use]
    pub fn with_sandbox_router(mut self, router: Arc<SandboxRouter>) -> Self {
        self.sandbox_router = Some(router);
        self
    }

    /// Override the maximum file size `Read` will accept. Larger files
    /// return a typed `too_large` payload. Wired from
    /// `[tools.fs].max_read_bytes`. Default: `DEFAULT_MAX_READ_BYTES`.
    #[must_use]
    pub fn with_max_read_bytes(mut self, max: u64) -> Self {
        self.max_read_bytes = Some(max);
        self
    }

    /// Configure the model context window in tokens. Enables the
    /// adaptive byte cap so per-Read payloads scale with the model's
    /// working set instead of using a fixed ceiling.
    #[must_use]
    pub fn with_context_window_tokens(mut self, tokens: u64) -> Self {
        self.context_window_tokens = Some(tokens);
        self
    }

    /// Effective file-size cap: config override or default.
    fn effective_max_read_bytes(&self) -> u64 {
        self.max_read_bytes.unwrap_or(DEFAULT_MAX_READ_BYTES)
    }

    #[instrument(skip(self), fields(file_path = %file_path))]
    async fn read_impl(
        &self,
        file_path: &str,
        offset: usize,
        limit: usize,
        session_key: &str,
    ) -> Result<Value> {
        require_absolute(file_path, "file_path")?;

        // Sandbox dispatch: if the session is sandboxed, round-trip through
        // the bridge and render the resulting bytes with the same logic as
        // the host path. Path-policy and binary detection still run on
        // host-side types, so both paths look identical to the LLM.
        if let Some(ref router) = self.sandbox_router
            && router.is_sandboxed(session_key).await
        {
            if let Some(ref policy) = self.path_policy
                && let Some(payload) = enforce_path_policy(policy, Path::new(file_path))
            {
                return Ok(payload);
            }
            let (backend, id) = ensure_sandbox(router, session_key).await?;
            let max = self.effective_max_read_bytes();
            let result = sandbox_read(&backend, &id, file_path, max).await?;
            match result {
                SandboxReadResult::Ok(bytes) => {
                    return Ok(self.render_bytes_to_payload(
                        file_path,
                        offset,
                        limit,
                        &bytes,
                        session_key,
                        true,
                        None, // sandbox mtime unavailable
                    ));
                },
                other => {
                    return Ok(other
                        .into_typed_payload(file_path, max)
                        .unwrap_or(json!({})));
                },
            }
        }

        // Stat first so we can surface not_found / permission_denied as
        // typed Ok payloads rather than Err strings. The chat loop strips
        // Err detail via err.to_string(); typed payloads survive as JSON.
        let meta = match fs::metadata(file_path).await {
            Ok(m) => m,
            Err(e) => {
                if let Some(payload) = io_error_to_typed_payload(&e, file_path) {
                    return Ok(payload);
                }
                return Err(Error::message(format!("cannot stat '{file_path}': {e}")));
            },
        };

        if !meta.is_file() {
            return Ok(fs_error_payload(
                FsErrorKind::NotRegularFile,
                file_path,
                "path is not a regular file",
                None,
            ));
        }

        // Path policy check: canonicalize first so allowlist globs evaluate
        // against the resolved path, not whatever the LLM supplied.
        if let Some(ref policy) = self.path_policy {
            let canonical = fs::canonicalize(file_path)
                .await
                .unwrap_or_else(|_| std::path::PathBuf::from(file_path));
            if let Some(payload) = enforce_path_policy(policy, &canonical) {
                return Ok(payload);
            }
        }

        let size = meta.len();
        let max_read = self.effective_max_read_bytes();
        if size > max_read {
            return Ok(json!({
                "kind": FsErrorKind::TooLarge.as_str(),
                "file_path": file_path,
                "error": format!(
                    "file is too large ({:.1} MB) — maximum is {:.0} MB",
                    size as f64 / (1024.0 * 1024.0),
                    max_read as f64 / (1024.0 * 1024.0),
                ),
                "bytes": size,
                "max_bytes": max_read,
            }));
        }

        let bytes = match fs::read(file_path).await {
            Ok(b) => b,
            Err(e) => {
                if let Some(payload) = io_error_to_typed_payload(&e, file_path) {
                    return Ok(payload);
                }
                return Err(Error::message(format!("failed to read '{file_path}': {e}")));
            },
        };

        let mtime = meta.modified().ok();
        Ok(self.render_bytes_to_payload(
            file_path,
            offset,
            limit,
            &bytes,
            session_key,
            false,
            mtime,
        ))
    }

    /// Render raw file bytes into the typed Read payload.
    ///
    /// Shared by the host and sandbox branches so the LLM-facing shape
    /// is identical across routing modes. `from_sandbox` controls the
    /// loop-tracker key (canonicalize is a no-op against sandbox paths
    /// since the LLM-supplied string is already absolute and untouched).
    fn render_bytes_to_payload(
        &self,
        file_path: &str,
        offset: usize,
        limit: usize,
        bytes: &[u8],
        session_key: &str,
        from_sandbox: bool,
        mtime: Option<std::time::SystemTime>,
    ) -> Value {
        // Record the read in the tracker BEFORE any early return (including
        // binary). An operator with must_read_before_write + binary_policy=base64
        // needs binary Reads to count as "this session has read the file."
        if let Some(ref state) = self.fs_state {
            let tracker_path = if from_sandbox {
                std::path::PathBuf::from(file_path)
            } else {
                std::fs::canonicalize(file_path)
                    .unwrap_or_else(|_| std::path::PathBuf::from(file_path))
            };
            let mut guard = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _consecutive = guard.record_read(session_key, tracker_path, offset, limit, mtime);
        }

        if is_binary_extension(file_path) || looks_binary(bytes) {
            #[cfg(feature = "metrics")]
            counter!(
                tools_metrics::EXECUTIONS_TOTAL,
                labels::TOOL => "Read".to_string(),
                labels::SUCCESS => "binary".to_string()
            )
            .increment(1);
            return match self.binary_policy {
                BinaryPolicy::Reject => json!({
                    "kind": "binary",
                    "file_path": file_path,
                    "bytes": bytes.len(),
                    "message": "file appears to be binary; content not returned (binary_policy = reject)",
                }),
                BinaryPolicy::Base64 => {
                    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
                    json!({
                        "kind": "binary",
                        "file_path": file_path,
                        "bytes": bytes.len(),
                        "base64": BASE64.encode(bytes),
                    })
                },
            };
        }

        // Lossy decode so we never fail on invalid UTF-8 — surface
        // whatever the LLM can see and let it decide.
        let text = String::from_utf8(bytes.to_vec()).unwrap_or_else(|e| {
            let bytes = e.into_bytes();
            String::from_utf8_lossy(&bytes).into_owned()
        });

        // Adaptive byte cap: when the operator has told us the model's
        // context window, scale per-call output so Read can't consume
        // more than ~20% of the model's working set. Otherwise fall
        // back to the fixed default.
        let byte_cap = self
            .context_window_tokens
            .map(compute_adaptive_read_cap)
            .unwrap_or(MAX_READ_OUTPUT_BYTES);
        let rendered = format_numbered_lines_with_cap(&text, offset, limit, byte_cap);

        #[cfg(feature = "metrics")]
        counter!(
            tools_metrics::EXECUTIONS_TOTAL,
            labels::TOOL => "Read".to_string(),
            labels::SUCCESS => "true".to_string()
        )
        .increment(1);

        let mut payload = json!({
            "kind": "text",
            "file_path": file_path,
            "content": rendered.text,
            "total_lines": rendered.total_lines,
            "start_line": rendered.start_line,
            "rendered_lines": rendered.rendered_lines,
            "truncated": rendered.truncated,
        });

        // Loop warning: the read was already recorded at the top of
        // this method. Check the consecutive count (which was bumped
        // during that first record_read) and surface a warning if the
        // same (path, offset, limit) has been repeated too many times.
        if let Some(ref state) = self.fs_state {
            let guard = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let n = guard.consecutive_reads(session_key);
            if n >= READ_LOOP_THRESHOLD
                && let Some(obj) = payload.as_object_mut()
            {
                obj.insert(
                    "loop_warning".into(),
                    json!(format!(
                        "This exact read (file_path={file_path}, offset={offset}, limit={limit}) \
                         has been repeated {n} times with no intervening edit. The \
                         file hasn't changed — stop re-reading it and make progress on the task."
                    )),
                );
            }
        }

        payload
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file from the local filesystem with line-numbered output. \
         Supports `offset` (1-indexed line to start at) and `limit` (max lines \
         to return) for paginating large files. Returns structured JSON with \
         the file's content, total line count, and truncation flag. Binary \
         files return a typed marker instead of garbage."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["file_path"],
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to read."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-indexed line number to start reading from."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to return (default 2000)."
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let file_path = params
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'file_path' parameter"))?;
        let offset = params
            .get("offset")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(1)
            .max(1);
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_READ_LINE_LIMIT)
            .max(1);
        let session_key = session_key_from(&params).to_string();

        match self.read_impl(file_path, offset, limit, &session_key).await {
            Ok(value) => Ok(value),
            Err(e) => {
                #[cfg(feature = "metrics")]
                counter!(
                    tools_metrics::EXECUTION_ERRORS_TOTAL,
                    labels::TOOL => "Read".to_string()
                )
                .increment(1);
                Err(e.into())
            },
        }
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {super::*, std::io::Write};

    #[tokio::test]
    async fn read_small_text_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"line one\nline two\nline three\n").unwrap();

        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": tmp.path().to_str().unwrap() }))
            .await
            .unwrap();

        assert_eq!(value["kind"], "text");
        assert_eq!(value["total_lines"], 3);
        assert_eq!(value["rendered_lines"], 3);
        assert_eq!(value["truncated"], false);
        assert!(value["content"].as_str().unwrap().contains("→line one"));
    }

    #[tokio::test]
    async fn read_paginated() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(tmp, "line {i}").unwrap();
        }

        let tool = ReadTool::new();
        let value = tool
            .execute(json!({
                "file_path": tmp.path().to_str().unwrap(),
                "offset": 3,
                "limit": 2,
            }))
            .await
            .unwrap();

        assert_eq!(value["total_lines"], 10);
        assert_eq!(value["rendered_lines"], 2);
        assert_eq!(value["start_line"], 3);
        assert_eq!(value["truncated"], true);
        let content = value["content"].as_str().unwrap();
        assert!(content.contains("line 3"));
        assert!(content.contains("line 4"));
        assert!(!content.contains("line 5"));
    }

    #[tokio::test]
    async fn read_binary_base64_policy_returns_encoded_bytes() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&[0u8, 1, 2, 3, 0, 4, 5]).unwrap();

        let tool = ReadTool::new().with_binary_policy(BinaryPolicy::Base64);
        let value = tool
            .execute(json!({ "file_path": tmp.path().to_str().unwrap() }))
            .await
            .unwrap();

        assert_eq!(value["kind"], "binary");
        assert_eq!(value["bytes"], 7);
        let encoded = value["base64"].as_str().unwrap();
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
        let decoded = BASE64.decode(encoded).unwrap();
        assert_eq!(decoded, [0u8, 1, 2, 3, 0, 4, 5]);
    }

    #[tokio::test]
    async fn read_binary_returns_typed_marker() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&[0u8, 1, 2, 3, 0, 4, 5]).unwrap();

        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": tmp.path().to_str().unwrap() }))
            .await
            .unwrap();

        assert_eq!(value["kind"], "binary");
        assert_eq!(value["bytes"], 7);
    }

    #[tokio::test]
    async fn read_missing_file_returns_typed_not_found() {
        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": "/tmp/does-not-exist-read-test-xyz-123" }))
            .await
            .unwrap();
        assert_eq!(value["kind"], "not_found");
        assert_eq!(value["file_path"], "/tmp/does-not-exist-read-test-xyz-123");
    }

    #[tokio::test]
    async fn read_directory_returns_typed_not_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": dir.path().to_str().unwrap() }))
            .await
            .unwrap();
        assert_eq!(value["kind"], "not_regular_file");
    }

    #[tokio::test]
    async fn read_too_large_returns_typed_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let f = std::fs::File::create(&path).unwrap();
        // One byte past the cap.
        f.set_len(DEFAULT_MAX_READ_BYTES + 1).unwrap();

        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": path.to_str().unwrap() }))
            .await
            .unwrap();
        assert_eq!(value["kind"], "too_large");
        assert!(value["bytes"].as_u64().unwrap() > DEFAULT_MAX_READ_BYTES);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_permission_denied_returns_typed_payload() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        fs::write(&path, "secret").await.unwrap();
        let mut perms = fs::metadata(&path).await.unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&path, perms).await.unwrap();

        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": path.to_str().unwrap() }))
            .await
            .unwrap();

        // Root bypasses permission checks; tolerate either typed error
        // or a successful text read so the test is CI-safe.
        let kind = value["kind"].as_str().unwrap();
        assert!(
            kind == "permission_denied" || kind == "text",
            "unexpected kind: {kind}"
        );

        // Restore perms so tempdir cleanup works.
        let mut restore = fs::metadata(&path).await.unwrap().permissions();
        restore.set_mode(0o644);
        let _ = fs::set_permissions(&path, restore).await;
    }

    #[tokio::test]
    async fn read_relative_path_errors() {
        let tool = ReadTool::new();
        let err = tool
            .execute(json!({ "file_path": "relative.txt" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be an absolute path"));
    }

    #[tokio::test]
    async fn read_missing_file_path_errors() {
        let tool = ReadTool::new();
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("missing 'file_path'"));
    }
}
