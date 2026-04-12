//! Session-scoped filesystem access inside sandboxes.
//!
//! This provides a first-class file service for sandboxed tools so callers do
//! not need to assemble shell snippets or juggle `(backend, sandbox_id)` pairs
//! directly. The current implementation still uses `Sandbox::exec` under the
//! hood, but the shell transport is now hidden behind a stable Rust interface.

use {
    async_trait::async_trait,
    base64::{Engine as _, engine::general_purpose::STANDARD as BASE64},
    serde_json::{Value, json},
    std::{
        io,
        io::Write as _,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    },
};

use crate::{
    Result,
    error::Error,
    exec::ExecOpts,
    sandbox::{Sandbox, SandboxId, SandboxRouter},
};

/// Maximum file size Write/Edit/MultiEdit can send into a sandbox in a
/// single call. Base64 expands by ~33%, so 512 KB raw becomes ~683 KB
/// of shell arg, comfortably under typical `ARG_MAX` limits.
pub const MAX_SANDBOX_WRITE_BYTES: usize = 512 * 1024;

const DEFAULT_SANDBOX_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SANDBOX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

// Exit codes used by the bridge scripts to encode typed errors.
const EXIT_NOT_FOUND: i32 = 10;
const EXIT_PERMISSION_DENIED: i32 = 11;
const EXIT_NOT_REGULAR_FILE: i32 = 12;
const EXIT_TOO_LARGE: i32 = 13;
const EXIT_SYMLINK: i32 = 14;
const EXIT_PARENT_MISSING: i32 = 20;

fn default_opts() -> ExecOpts {
    ExecOpts {
        timeout: DEFAULT_SANDBOX_TIMEOUT,
        max_output_bytes: DEFAULT_SANDBOX_OUTPUT_BYTES,
        working_dir: Some(PathBuf::from("/home/sandbox")),
        env: Vec::new(),
    }
}

/// Escape a string for safe use inside single quotes in a POSIX shell.
#[must_use]
pub fn shell_single_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

/// Outcome of a sandbox read call.
#[derive(Debug)]
pub enum SandboxReadResult {
    Ok(Vec<u8>),
    NotFound,
    PermissionDenied,
    NotRegularFile,
    TooLarge(u64),
}

impl SandboxReadResult {
    /// Convert a non-`Ok` variant into the typed JSON payload the fs
    /// tools return. `None` for `Ok` (caller handles success).
    #[must_use]
    pub fn into_typed_payload(self, file_path: &str, max_bytes: u64) -> Option<Value> {
        match self {
            Self::Ok(_) => None,
            Self::NotFound => Some(json!({
                "kind": "not_found",
                "file_path": file_path,
                "error": "file does not exist",
                "detail": "",
            })),
            Self::PermissionDenied => Some(json!({
                "kind": "permission_denied",
                "file_path": file_path,
                "error": "insufficient permissions to access file",
                "detail": "",
            })),
            Self::NotRegularFile => Some(json!({
                "kind": "not_regular_file",
                "file_path": file_path,
                "error": "path is not a regular file",
                "detail": "",
            })),
            Self::TooLarge(size) => Some(json!({
                "kind": "too_large",
                "file_path": file_path,
                "error": format!(
                    "file is too large ({:.1} MB) — maximum is {:.0} MB",
                    size as f64 / (1024.0 * 1024.0),
                    max_bytes as f64 / (1024.0 * 1024.0),
                ),
                "bytes": size,
                "max_bytes": max_bytes,
            })),
        }
    }
}

/// Output-mode discriminator for sandbox grep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxGrepMode {
    Content,
    FilesWithMatches,
    Count,
}

/// Options for sandbox grep.
#[derive(Debug, Clone)]
pub struct SandboxGrepOptions {
    pub pattern: String,
    pub path: String,
    pub mode: SandboxGrepMode,
    pub case_insensitive: bool,
    pub include_globs: Vec<String>,
    pub offset: usize,
    pub head_limit: Option<usize>,
    pub match_cap: Option<usize>,
}

/// Abstract filesystem access for a prepared sandbox session.
#[async_trait]
pub trait SandboxFileSystem: Send + Sync {
    async fn read_file(&self, file_path: &str, max_bytes: u64) -> Result<SandboxReadResult>;

    async fn write_file(&self, file_path: &str, content: &[u8]) -> Result<Option<Value>>;

    async fn list_files(&self, root: &str) -> Result<Vec<String>>;

    async fn grep(&self, opts: SandboxGrepOptions) -> Result<Value>;
}

/// Command-based [`SandboxFileSystem`] implementation backed by `Sandbox::exec`.
pub struct CommandSandboxFileSystem {
    backend: Arc<dyn Sandbox>,
    id: SandboxId,
}

impl CommandSandboxFileSystem {
    #[must_use]
    pub fn new(backend: Arc<dyn Sandbox>, id: SandboxId) -> Self {
        Self { backend, id }
    }
}

/// Prepare a session sandbox and return a file service for it.
pub async fn sandbox_file_system_for_session(
    router: &SandboxRouter,
    session_key: &str,
) -> Result<Arc<dyn SandboxFileSystem>> {
    let id = router.sandbox_id_for(session_key);
    let image = router.resolve_image(session_key, None).await;
    let backend = Arc::clone(router.backend());
    backend.ensure_ready(&id, Some(&image)).await?;
    Ok(Arc::new(CommandSandboxFileSystem::new(backend, id)))
}

/// Default command-based sandbox read implementation used by the file service
/// and by `Sandbox` trait default methods.
pub async fn command_read_file<S: Sandbox + ?Sized>(
    backend: &S,
    id: &SandboxId,
    file_path: &str,
    max_bytes: u64,
) -> Result<SandboxReadResult> {
    let quoted = shell_single_quote(file_path);
    let script = format!(
        "path={quoted}; max={max_bytes}; \
         if [ ! -e \"$path\" ]; then exit {EXIT_NOT_FOUND}; fi; \
         if [ ! -r \"$path\" ]; then exit {EXIT_PERMISSION_DENIED}; fi; \
         if [ ! -f \"$path\" ]; then exit {EXIT_NOT_REGULAR_FILE}; fi; \
         size=$(wc -c < \"$path\"); \
         if [ \"$size\" -gt \"$max\" ]; then echo \"$size\" >&2; exit {EXIT_TOO_LARGE}; fi; \
         base64 < \"$path\" | tr -d '\\n'"
    );

    let result = backend.exec(id, &script, &default_opts()).await?;
    match result.exit_code {
        0 => {
            let bytes = BASE64.decode(result.stdout.trim()).map_err(|e| {
                Error::message(format!(
                    "failed to decode sandbox read for '{file_path}': {e}"
                ))
            })?;
            Ok(SandboxReadResult::Ok(bytes))
        },
        EXIT_NOT_FOUND => Ok(SandboxReadResult::NotFound),
        EXIT_PERMISSION_DENIED => Ok(SandboxReadResult::PermissionDenied),
        EXIT_NOT_REGULAR_FILE => Ok(SandboxReadResult::NotRegularFile),
        EXIT_TOO_LARGE => {
            let size = result.stderr.trim().parse::<u64>().unwrap_or(0);
            Ok(SandboxReadResult::TooLarge(size))
        },
        other => {
            let detail = if result.stderr.trim().is_empty() {
                format!("sandbox read exited with code {other}")
            } else {
                result.stderr.trim().to_string()
            };
            Err(Error::message(format!(
                "sandbox read of '{file_path}' failed: {detail}"
            )))
        },
    }
}

/// Default command-based sandbox write implementation used by the file service
/// and by `Sandbox` trait default methods.
pub async fn command_write_file<S: Sandbox + ?Sized>(
    backend: &S,
    id: &SandboxId,
    file_path: &str,
    content: &[u8],
) -> Result<Option<Value>> {
    if content.len() > MAX_SANDBOX_WRITE_BYTES {
        return Err(Error::message(format!(
            "sandbox Write is limited to {} KB per call (got {:.1} KB); \
             larger writes will ship in a follow-up that chunks content",
            MAX_SANDBOX_WRITE_BYTES / 1024,
            content.len() as f64 / 1024.0,
        )));
    }

    let encoded = BASE64.encode(content);
    let quoted_path = shell_single_quote(file_path);
    let script = format!(
        "path={quoted_path}; \
         parent=$(dirname \"$path\"); \
         if [ ! -d \"$parent\" ]; then exit {EXIT_PARENT_MISSING}; fi; \
         if [ -L \"$path\" ]; then exit {EXIT_SYMLINK}; fi; \
         tmp=\"$path.moltis.$$\"; \
         if ! printf '%s' '{encoded}' | base64 -d > \"$tmp\"; then rm -f \"$tmp\"; exit 1; fi; \
         sync \"$tmp\" 2>/dev/null || sync; \
         if [ -L \"$path\" ]; then rm -f \"$tmp\"; exit {EXIT_SYMLINK}; fi; \
         mv \"$tmp\" \"$path\""
    );

    let result = backend.exec(id, &script, &default_opts()).await?;
    match result.exit_code {
        0 => Ok(None),
        EXIT_PARENT_MISSING => Err(Error::message(format!(
            "cannot resolve parent of '{file_path}': directory does not exist in sandbox"
        ))),
        EXIT_SYMLINK => Ok(Some(json!({
            "kind": "path_denied",
            "file_path": file_path,
            "error": "target is a symbolic link; refusing to follow",
            "detail": "sandbox Write rejects symlinks",
        }))),
        other => {
            let detail = if result.stderr.trim().is_empty() {
                format!("sandbox write exited with code {other}")
            } else {
                result.stderr.trim().to_string()
            };
            Err(Error::message(format!(
                "sandbox write of '{file_path}' failed: {detail}"
            )))
        },
    }
}

/// Default command-based sandbox list-files implementation used by the file service
/// and by `Sandbox` trait default methods.
pub async fn command_list_files<S: Sandbox + ?Sized>(
    backend: &S,
    id: &SandboxId,
    root: &str,
) -> Result<Vec<String>> {
    let quoted = shell_single_quote(root);
    let script = format!("find {quoted} -type f 2>/dev/null");
    let result = backend.exec(id, &script, &default_opts()).await?;
    if result.exit_code != 0 && result.stdout.trim().is_empty() {
        let detail = if result.stderr.trim().is_empty() {
            format!("find exited with code {}", result.exit_code)
        } else {
            result.stderr.trim().to_string()
        };
        return Err(Error::message(format!(
            "sandbox list_files '{root}' failed: {detail}"
        )));
    }
    Ok(result
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

/// Default command-based sandbox grep implementation used by the file service
/// and by `Sandbox` trait default methods.
pub async fn command_grep<S: Sandbox + ?Sized>(
    backend: &S,
    id: &SandboxId,
    opts: SandboxGrepOptions,
) -> Result<Value> {
    let pattern_q = shell_single_quote(&opts.pattern);
    let path_q = shell_single_quote(&opts.path);
    let mut flags: Vec<&str> = vec!["-r", "-P"];
    if opts.case_insensitive {
        flags.push("-i");
    }
    match opts.mode {
        SandboxGrepMode::Content => {
            flags.push("-n");
            flags.push("-H");
        },
        SandboxGrepMode::FilesWithMatches => {
            flags.push("-l");
        },
        SandboxGrepMode::Count => {
            flags.push("-c");
            flags.push("-H");
        },
    }
    let include_args = if opts.include_globs.is_empty() {
        String::new()
    } else {
        opts.include_globs
            .iter()
            .map(|glob| format!("--include={}", shell_single_quote(glob)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let flags_str = flags.join(" ");
    let flags_str_ere = flags_str.replace("-P", "-E");
    let script = format!(
        "grep {flags_str} {include_args} -- {pattern_q} {path_q} 2>/dev/null; \
         rc=$?; \
         if [ $rc -eq 2 ]; then \
           grep {flags_str_ere} {include_args} -- {pattern_q} {path_q} 2>/dev/null; \
           rc=$?; \
         fi; \
         if [ $rc -eq 1 ]; then exit 0; else exit $rc; fi"
    );
    let result = backend.exec(id, &script, &default_opts()).await?;
    if result.exit_code != 0 {
        let detail = if result.stderr.trim().is_empty() {
            format!("grep exited with code {}", result.exit_code)
        } else {
            result.stderr.trim().to_string()
        };
        return Err(Error::message(format!("sandbox grep failed: {detail}")));
    }

    let lines: Vec<&str> = result
        .stdout
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();

    match opts.mode {
        SandboxGrepMode::FilesWithMatches => {
            let files = lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>();
            let (files, truncated) = apply_head_offset(files, opts.offset, opts.head_limit);
            Ok(json!({
                "mode": "files_with_matches",
                "files": files,
                "truncated": truncated,
            }))
        },
        SandboxGrepMode::Count => {
            let mut counts = Vec::new();
            for line in &lines {
                if let Some((path, count_str)) = line.rsplit_once(':')
                    && let Ok(count) = count_str.parse::<usize>()
                    && count > 0
                {
                    counts.push(json!({
                        "path": path,
                        "count": count,
                    }));
                }
            }
            let (counts, truncated) = apply_head_offset(counts, opts.offset, opts.head_limit);
            Ok(json!({
                "mode": "count",
                "counts": counts,
                "truncated": truncated,
            }))
        },
        SandboxGrepMode::Content => {
            let mut matches = Vec::new();
            for line in &lines {
                let mut parts = line.splitn(3, ':');
                let (Some(path), Some(lineno_str), Some(text)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                let Ok(lineno) = lineno_str.parse::<usize>() else {
                    continue;
                };
                matches.push(json!({
                    "path": path,
                    "line": lineno,
                    "match": text,
                    "block": vec![format!("{lineno}:{text}")],
                }));
            }
            let (matches, cap_truncated) = apply_match_cap(matches, opts.match_cap);
            let (matches, page_truncated) =
                apply_head_offset(matches, opts.offset, opts.head_limit);
            Ok(json!({
                "mode": "content",
                "matches": matches,
                "truncated": cap_truncated || page_truncated,
            }))
        },
    }
}

#[async_trait]
impl SandboxFileSystem for CommandSandboxFileSystem {
    async fn read_file(&self, file_path: &str, max_bytes: u64) -> Result<SandboxReadResult> {
        self.backend.read_file(&self.id, file_path, max_bytes).await
    }

    async fn write_file(&self, file_path: &str, content: &[u8]) -> Result<Option<Value>> {
        self.backend.write_file(&self.id, file_path, content).await
    }

    async fn list_files(&self, root: &str) -> Result<Vec<String>> {
        self.backend.list_files(&self.id, root).await
    }

    async fn grep(&self, opts: SandboxGrepOptions) -> Result<Value> {
        self.backend.grep(&self.id, opts).await
    }
}

fn apply_match_cap<T>(mut rows: Vec<T>, match_cap: Option<usize>) -> (Vec<T>, bool) {
    match match_cap {
        Some(limit) if rows.len() > limit => {
            rows.truncate(limit);
            (rows, true)
        },
        _ => (rows, false),
    }
}

fn apply_head_offset<T: Clone>(
    rows: Vec<T>,
    offset: usize,
    head_limit: Option<usize>,
) -> (Vec<T>, bool) {
    let total = rows.len();
    let start = offset.min(total);
    let slice = &rows[start..];
    let (capped, truncated) = match head_limit {
        Some(limit) if slice.len() > limit => (&slice[..limit], true),
        _ => (slice, false),
    };
    (capped.to_vec(), truncated)
}

enum NativeHostWriteOutcome {
    Written,
    SymlinkDenied,
}

fn path_denied_payload(file_path: &str, detail: &str) -> Value {
    json!({
        "kind": "path_denied",
        "file_path": file_path,
        "error": "target is a symbolic link; refusing to follow",
        "detail": detail,
    })
}

fn permission_denied_payload(file_path: &str, detail: &str) -> Value {
    json!({
        "kind": "permission_denied",
        "file_path": file_path,
        "error": "insufficient permissions to access file",
        "detail": detail,
    })
}

fn not_regular_file_payload(file_path: &str, detail: &str) -> Value {
    json!({
        "kind": "not_regular_file",
        "file_path": file_path,
        "error": "path is not a regular file",
        "detail": detail,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerCopyErrorKind {
    NotFound,
    PermissionDenied,
}

fn classify_container_copy_error(stderr: &str) -> Option<ContainerCopyErrorKind> {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("permission denied") {
        return Some(ContainerCopyErrorKind::PermissionDenied);
    }
    if lower.contains("no such file or directory")
        || lower.contains("not found")
        || lower.contains("could not find the file")
    {
        return Some(ContainerCopyErrorKind::NotFound);
    }
    None
}

fn container_copy_local_root(destination_dir: &Path, source_path: &str) -> PathBuf {
    let basename = Path::new(source_path)
        .components()
        .rev()
        .find_map(|component| {
            if let std::path::Component::Normal(value) = component {
                Some(value.to_owned())
            } else {
                None
            }
        });

    match basename {
        Some(basename) => {
            let candidate = destination_dir.join(basename);
            if candidate.exists() {
                candidate
            } else {
                destination_dir.to_path_buf()
            }
        },
        None => destination_dir.to_path_buf(),
    }
}

async fn copy_container_path_to_host(
    cli: &str,
    container_name: &str,
    container_path: &str,
    follow_links: bool,
    destination_dir: &Path,
) -> Result<PathBuf> {
    let mut args = vec!["cp".to_string()];
    if follow_links {
        args.push("-L".to_string());
    }
    args.push(format!("{container_name}:{container_path}"));
    args.push(destination_dir.display().to_string());

    let output = tokio::process::Command::new(cli)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if let Some(kind) = classify_container_copy_error(detail) {
            return Err(Error::message(format!("container-copy:{kind:?}:{detail}")));
        }
        return Err(Error::message(format!(
            "{cli} cp failed for '{container_path}': {detail}"
        )));
    }

    Ok(container_copy_local_root(destination_dir, container_path))
}

fn decode_container_copy_error(error: &Error) -> Option<(ContainerCopyErrorKind, String)> {
    let message = error.to_string();
    let rest = message.strip_prefix("container-copy:")?;
    let (kind, detail) = rest.split_once(':')?;
    let kind = match kind {
        "NotFound" => ContainerCopyErrorKind::NotFound,
        "PermissionDenied" => ContainerCopyErrorKind::PermissionDenied,
        _ => return None,
    };
    Some((kind, detail.to_string()))
}

/// Native host-backed read implementation for sandbox backends whose paths
/// are just host paths.
pub async fn native_host_read_file(file_path: &str, max_bytes: u64) -> Result<SandboxReadResult> {
    let meta = match tokio::fs::metadata(file_path).await {
        Ok(meta) => meta,
        Err(error) => return Ok(map_io_error_to_read_result(&error)),
    };

    if !meta.is_file() {
        return Ok(SandboxReadResult::NotRegularFile);
    }
    if meta.len() > max_bytes {
        return Ok(SandboxReadResult::TooLarge(meta.len()));
    }

    let bytes = match tokio::fs::read(file_path).await {
        Ok(bytes) => bytes,
        Err(error) => return Ok(map_io_error_to_read_result(&error)),
    };
    if bytes.len() as u64 > max_bytes {
        return Ok(SandboxReadResult::TooLarge(bytes.len() as u64));
    }
    Ok(SandboxReadResult::Ok(bytes))
}

/// Native host-backed write implementation for sandbox backends whose paths
/// are just host paths.
pub async fn native_host_write_file(file_path: &str, content: &[u8]) -> Result<Option<Value>> {
    let path = PathBuf::from(file_path);
    let parent = path.parent().ok_or_else(|| {
        Error::message(format!(
            "cannot resolve parent of '{file_path}': directory does not exist on host"
        ))
    })?;
    match tokio::fs::metadata(parent).await {
        Ok(metadata) if metadata.is_dir() => {},
        Ok(_) => {
            return Err(Error::message(format!(
                "cannot resolve parent of '{file_path}': parent is not a directory on host"
            )));
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Error::message(format!(
                "cannot resolve parent of '{file_path}': directory does not exist on host"
            )));
        },
        Err(error) => {
            return Err(Error::message(format!(
                "failed to inspect parent of '{file_path}': {error}"
            )));
        },
    }
    if is_symlink(&path).await? {
        return Ok(Some(path_denied_payload(
            file_path,
            "native host write rejects symlinks",
        )));
    }

    let bytes = content.to_vec();
    let path_for_blocking = path.clone();
    let parent_for_blocking = parent.to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || -> Result<NativeHostWriteOutcome> {
        let mut tmp = tempfile::NamedTempFile::new_in(&parent_for_blocking).map_err(|error| {
            Error::message(format!(
                "failed to create temp file in '{}': {error}",
                parent_for_blocking.display()
            ))
        })?;
        tmp.write_all(&bytes)
            .map_err(|error| Error::message(format!("failed to write temp file: {error}")))?;
        tmp.as_file()
            .sync_all()
            .map_err(|error| Error::message(format!("failed to fsync temp file: {error}")))?;
        if std::fs::symlink_metadata(&path_for_blocking)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Ok(NativeHostWriteOutcome::SymlinkDenied);
        }
        tmp.persist(&path_for_blocking).map_err(|error| {
            Error::message(format!(
                "failed to persist file '{}': {error}",
                path_for_blocking.display()
            ))
        })?;
        Ok(NativeHostWriteOutcome::Written)
    })
    .await
    .map_err(|error| Error::message(format!("blocking write task failed: {error}")))??;

    match outcome {
        NativeHostWriteOutcome::Written => Ok(None),
        NativeHostWriteOutcome::SymlinkDenied => Ok(Some(path_denied_payload(
            file_path,
            "native host write rejects symlinks",
        ))),
    }
}

/// Native host-backed file listing implementation for sandbox backends whose
/// paths are just host paths.
pub async fn native_host_list_files(root: &str) -> Result<Vec<String>> {
    let root = PathBuf::from(root);
    tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        match std::fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(Vec::new()),
            Ok(metadata) if metadata.is_file() => return Ok(vec![root.display().to_string()]),
            Ok(_) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(Error::message(format!(
                    "sandbox list_files '{}' failed: {error}",
                    root.display()
                )));
            },
        }

        let mut stack = vec![root];
        let mut files = Vec::new();

        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) => {
                    if error.kind() == io::ErrorKind::NotFound {
                        continue;
                    }
                    return Err(Error::message(format!(
                        "sandbox list_files '{}' failed: {error}",
                        dir.display()
                    )));
                },
            };

            for entry in entries {
                let entry = entry.map_err(|error| {
                    Error::message(format!(
                        "sandbox list_files '{}' failed: {error}",
                        dir.display()
                    ))
                })?;
                let path = entry.path();
                let file_type = entry.file_type().map_err(|error| {
                    Error::message(format!(
                        "sandbox list_files '{}' failed: {error}",
                        path.display()
                    ))
                })?;
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    stack.push(path);
                } else if file_type.is_file() {
                    files.push(path.display().to_string());
                }
            }
        }

        files.sort();
        Ok(files)
    })
    .await
    .map_err(|error| Error::message(format!("blocking list_files task failed: {error}")))?
}

fn map_io_error_to_read_result(error: &io::Error) -> SandboxReadResult {
    match error.kind() {
        io::ErrorKind::NotFound => SandboxReadResult::NotFound,
        io::ErrorKind::PermissionDenied => SandboxReadResult::PermissionDenied,
        _ => SandboxReadResult::NotRegularFile,
    }
}

async fn is_symlink(path: &Path) -> Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => Ok(meta.file_type().is_symlink()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::message(format!(
            "failed to inspect '{}': {error}",
            path.display()
        ))),
    }
}

pub fn remap_host_files_to_guest(
    guest_root: &str,
    host_root: &Path,
    host_files: Vec<String>,
) -> Result<Vec<String>> {
    let mut guest_files = Vec::with_capacity(host_files.len());
    for host_file in host_files {
        let relative = Path::new(&host_file)
            .strip_prefix(host_root)
            .map_err(|error| {
                Error::message(format!(
                    "failed to relativize host path '{host_file}' against '{}': {error}",
                    host_root.display()
                ))
            })?;
        let guest_path = if relative.as_os_str().is_empty() {
            PathBuf::from(guest_root)
        } else {
            Path::new(guest_root).join(relative)
        };
        guest_files.push(guest_path.display().to_string());
    }
    guest_files.sort();
    Ok(guest_files)
}

/// Copy-based read implementation for OCI-compatible container CLIs.
pub async fn oci_container_read_file(
    cli: &str,
    container_name: &str,
    file_path: &str,
    max_bytes: u64,
) -> Result<SandboxReadResult> {
    let temp_dir = tempfile::tempdir()?;
    let local_path =
        match copy_container_path_to_host(cli, container_name, file_path, true, temp_dir.path())
            .await
        {
            Ok(local_path) => local_path,
            Err(error) => {
                if let Some((kind, _detail)) = decode_container_copy_error(&error) {
                    return Ok(match kind {
                        ContainerCopyErrorKind::NotFound => SandboxReadResult::NotFound,
                        ContainerCopyErrorKind::PermissionDenied => {
                            SandboxReadResult::PermissionDenied
                        },
                    });
                }
                return Err(error);
            },
        };

    let metadata = match tokio::fs::metadata(&local_path).await {
        Ok(metadata) => metadata,
        Err(error) => return Ok(map_io_error_to_read_result(&error)),
    };
    if !metadata.is_file() {
        return Ok(SandboxReadResult::NotRegularFile);
    }

    native_host_read_file(
        local_path
            .to_str()
            .ok_or_else(|| Error::message("copied container path contains invalid UTF-8"))?,
        max_bytes,
    )
    .await
}

/// Copy-based write implementation for OCI-compatible container CLIs.
pub async fn oci_container_write_file(
    cli: &str,
    container_name: &str,
    file_path: &str,
    content: &[u8],
) -> Result<Option<Value>> {
    let path = Path::new(file_path);
    let parent = path.parent().ok_or_else(|| {
        Error::message(format!(
            "cannot resolve parent of '{file_path}': directory does not exist in container"
        ))
    })?;

    let target_temp_dir = tempfile::tempdir()?;
    match copy_container_path_to_host(
        cli,
        container_name,
        file_path,
        false,
        target_temp_dir.path(),
    )
    .await
    {
        Ok(local_target) => {
            let metadata = tokio::fs::symlink_metadata(&local_target)
                .await
                .map_err(|error| {
                    Error::message(format!(
                        "failed to inspect copied container path '{file_path}': {error}"
                    ))
                })?;
            if metadata.file_type().is_symlink() {
                return Ok(Some(path_denied_payload(
                    file_path,
                    "OCI Write rejects symlinks",
                )));
            }
            if !metadata.is_file() {
                return Ok(Some(not_regular_file_payload(
                    file_path,
                    "OCI Write requires a regular file target",
                )));
            }
        },
        Err(error) => {
            if let Some((kind, detail)) = decode_container_copy_error(&error) {
                match kind {
                    ContainerCopyErrorKind::PermissionDenied => {
                        return Ok(Some(permission_denied_payload(file_path, &detail)));
                    },
                    ContainerCopyErrorKind::NotFound => {
                        let parent_temp_dir = tempfile::tempdir()?;
                        let parent_string = parent.display().to_string();
                        let local_parent = match copy_container_path_to_host(
                            cli,
                            container_name,
                            &parent_string,
                            true,
                            parent_temp_dir.path(),
                        )
                        .await
                        {
                            Ok(local_parent) => local_parent,
                            Err(parent_error) => {
                                if let Some((parent_kind, parent_detail)) =
                                    decode_container_copy_error(&parent_error)
                                {
                                    return match parent_kind {
                                        ContainerCopyErrorKind::NotFound => {
                                            Err(Error::message(format!(
                                                "cannot resolve parent of '{file_path}': directory does not exist in container"
                                            )))
                                        },
                                        ContainerCopyErrorKind::PermissionDenied => Ok(Some(
                                            permission_denied_payload(file_path, &parent_detail),
                                        )),
                                    };
                                }
                                return Err(parent_error);
                            },
                        };

                        let parent_metadata =
                            tokio::fs::metadata(&local_parent).await.map_err(|error| {
                                Error::message(format!(
                                    "failed to inspect copied container parent '{parent_string}': {error}"
                                ))
                            })?;
                        if !parent_metadata.is_dir() {
                            return Err(Error::message(format!(
                                "cannot resolve parent of '{file_path}': parent is not a directory in container"
                            )));
                        }
                    },
                }
            } else {
                return Err(error);
            }
        },
    }

    let mut temp_file = tempfile::NamedTempFile::new()?;
    temp_file.write_all(content)?;
    temp_file.as_file().sync_all()?;

    let args = vec![
        "cp".to_string(),
        temp_file.path().display().to_string(),
        format!("{container_name}:{file_path}"),
    ];
    let output = tokio::process::Command::new(cli)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await?;

    if output.status.success() {
        return Ok(None);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if let Some(kind) = classify_container_copy_error(detail) {
        return match kind {
            ContainerCopyErrorKind::NotFound => Err(Error::message(format!(
                "cannot resolve parent of '{file_path}': directory does not exist in container"
            ))),
            ContainerCopyErrorKind::PermissionDenied => {
                Ok(Some(permission_denied_payload(file_path, detail)))
            },
        };
    }

    Err(Error::message(format!(
        "{cli} cp failed for '{file_path}': {detail}"
    )))
}

/// Copy-based list-files implementation for OCI-compatible container CLIs.
pub async fn oci_container_list_files(
    cli: &str,
    container_name: &str,
    root: &str,
) -> Result<Vec<String>> {
    let temp_dir = tempfile::tempdir()?;
    let local_root = copy_container_path_to_host(cli, container_name, root, true, temp_dir.path())
        .await
        .map_err(|error| {
            if let Some((kind, detail)) = decode_container_copy_error(&error) {
                match kind {
                    ContainerCopyErrorKind::NotFound => {
                        Error::message(format!("sandbox list_files '{root}' failed: {detail}"))
                    },
                    ContainerCopyErrorKind::PermissionDenied => {
                        Error::message(format!("sandbox list_files '{root}' failed: {detail}"))
                    },
                }
            } else {
                error
            }
        })?;

    let metadata = tokio::fs::metadata(&local_root).await.map_err(|error| {
        Error::message(format!(
            "failed to inspect copied container tree '{root}': {error}"
        ))
    })?;

    if metadata.is_file() {
        return Ok(vec![root.to_string()]);
    }
    if !metadata.is_dir() {
        return Err(Error::message(format!(
            "sandbox list_files '{root}' failed: copied path is not a directory"
        )));
    }

    let local_files = native_host_list_files(
        local_root
            .to_str()
            .ok_or_else(|| Error::message("copied container path contains invalid UTF-8"))?,
    )
    .await?;

    let mut mapped_files = Vec::with_capacity(local_files.len());
    for local_file in local_files {
        let relative = Path::new(&local_file)
            .strip_prefix(&local_root)
            .map_err(|error| {
                Error::message(format!(
                    "failed to relativize copied container path '{local_file}': {error}"
                ))
            })?;
        let mapped = if relative.as_os_str().is_empty() {
            PathBuf::from(root)
        } else {
            Path::new(root).join(relative)
        };
        mapped_files.push(mapped.display().to_string());
    }
    mapped_files.sort();
    Ok(mapped_files)
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
pub(crate) mod test_helpers {
    use {
        super::*,
        crate::{exec::ExecResult, sandbox::types::BuildImageResult},
        std::sync::Mutex,
    };

    pub struct MockSandbox {
        pub calls: Mutex<Vec<String>>,
        pub responses: Mutex<Vec<ExecResult>>,
    }

    impl MockSandbox {
        pub fn new(responses: Vec<ExecResult>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            })
        }

        pub fn last_command(&self) -> Option<String> {
            self.calls.lock().unwrap().last().cloned()
        }
    }

    #[async_trait]
    impl Sandbox for MockSandbox {
        fn backend_name(&self) -> &'static str {
            "mock"
        }

        fn is_real(&self) -> bool {
            true
        }

        async fn ensure_ready(&self, _id: &SandboxId, _image_override: Option<&str>) -> Result<()> {
            Ok(())
        }

        async fn exec(
            &self,
            _id: &SandboxId,
            command: &str,
            _opts: &ExecOpts,
        ) -> Result<ExecResult> {
            self.calls.lock().unwrap().push(command.to_string());
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(ExecResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                })
            } else {
                Ok(responses.remove(0))
            }
        }

        async fn cleanup(&self, _id: &SandboxId) -> Result<()> {
            Ok(())
        }

        async fn build_image(
            &self,
            _base: &str,
            _packages: &[String],
        ) -> Result<Option<BuildImageResult>> {
            Ok(None)
        }
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {
        super::{test_helpers::MockSandbox, *},
        crate::{exec::ExecResult, sandbox::types::SandboxScope},
    };

    fn test_id() -> SandboxId {
        SandboxId {
            scope: SandboxScope::Session,
            key: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn read_file_decodes_base64() {
        let encoded = BASE64.encode(b"hello sandbox");
        let mock = MockSandbox::new(vec![ExecResult {
            stdout: encoded,
            stderr: String::new(),
            exit_code: 0,
        }]);
        let fs = CommandSandboxFileSystem::new(mock.clone(), test_id());

        let result = fs.read_file("/data/x.txt", 1024).await.unwrap();
        match result {
            SandboxReadResult::Ok(bytes) => assert_eq!(bytes, b"hello sandbox"),
            other => panic!("expected Ok, got {other:?}"),
        }
        assert!(mock.last_command().unwrap().contains("/data/x.txt"));
    }

    #[tokio::test]
    async fn read_file_maps_too_large() {
        let mock = MockSandbox::new(vec![ExecResult {
            stdout: String::new(),
            stderr: "12345\n".to_string(),
            exit_code: EXIT_TOO_LARGE,
        }]);
        let fs = CommandSandboxFileSystem::new(mock, test_id());

        let result = fs.read_file("/big", 100).await.unwrap();
        assert!(matches!(result, SandboxReadResult::TooLarge(12345)));
    }

    #[tokio::test]
    async fn write_file_encodes_content() {
        let mock = MockSandbox::new(vec![ExecResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }]);
        let fs = CommandSandboxFileSystem::new(mock.clone(), test_id());

        let result = fs.write_file("/data/out.txt", b"abc").await.unwrap();
        assert!(result.is_none());
        let cmd = mock.last_command().unwrap();
        assert!(cmd.contains("/data/out.txt"));
        assert!(cmd.contains(&BASE64.encode(b"abc")));
        assert!(cmd.contains("sync \"$tmp\""));
    }

    #[tokio::test]
    async fn list_files_reads_find_output() {
        let mock = MockSandbox::new(vec![ExecResult {
            stdout: "/data/a.rs\n/data/b.rs\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
        }]);
        let fs = CommandSandboxFileSystem::new(mock, test_id());

        let files = fs.list_files("/data").await.unwrap();
        assert_eq!(files, vec!["/data/a.rs", "/data/b.rs"]);
    }

    #[tokio::test]
    async fn grep_content_applies_paging() {
        let mock = MockSandbox::new(vec![ExecResult {
            stdout: "/data/lib.rs:3:fn alpha()\n/data/lib.rs:9:fn beta()\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
        }]);
        let fs = CommandSandboxFileSystem::new(mock, test_id());

        let value = fs
            .grep(SandboxGrepOptions {
                pattern: "fn".to_string(),
                path: "/data".to_string(),
                mode: SandboxGrepMode::Content,
                case_insensitive: false,
                include_globs: Vec::new(),
                offset: 1,
                head_limit: Some(1),
                match_cap: None,
            })
            .await
            .unwrap();

        assert_eq!(value["mode"], "content");
        assert_eq!(value["truncated"], false);
        let matches = value["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["line"], 9);
    }
}
