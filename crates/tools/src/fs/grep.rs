//! `Grep` tool — regex content search across files.
//!
//! Walks the target with `ignore::WalkBuilder` (respects `.gitignore`),
//! applies an optional `glob` file filter, and searches each file with the
//! `regex` crate. Supports three output modes mirroring Claude Code / rg:
//!
//! - `content` — matching lines with optional line numbers and context.
//! - `files_with_matches` (default) — just the paths of files that matched.
//! - `count` — match count per file.
//!
//! We use the `regex` crate rather than shelling out to `rg` so the tool runs
//! in-process, returns structured results, and honors the same policy layer
//! as every other native tool.

use {
    async_trait::async_trait,
    globset::{Glob as GlobPattern, GlobMatcher},
    ignore::WalkBuilder,
    moltis_agents::tool_registry::AgentTool,
    regex::RegexBuilder,
    serde_json::{Value, json},
    std::path::{Path, PathBuf},
    tracing::instrument,
};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, labels, tools as tools_metrics};

use crate::{Result, error::Error, fs::shared::require_absolute};

/// Maximum bytes of a single file we will load for content searching.
const DEFAULT_GREP_FILE_CAP: u64 = 5 * 1024 * 1024;

/// Maximum match rows returned by a single `content`-mode call.
const DEFAULT_GREP_MATCH_CAP: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Content,
    FilesWithMatches,
    Count,
}

impl OutputMode {
    fn parse(raw: Option<&str>) -> Result<Self> {
        match raw {
            None | Some("files_with_matches") => Ok(Self::FilesWithMatches),
            Some("content") => Ok(Self::Content),
            Some("count") => Ok(Self::Count),
            Some(other) => Err(Error::message(format!(
                "invalid output_mode '{other}' — expected 'content', 'files_with_matches', or 'count'"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
struct GrepOptions {
    pattern: String,
    path: PathBuf,
    glob: Option<GlobMatcher>,
    file_type: Option<String>,
    output_mode: OutputMode,
    case_insensitive: bool,
    show_line_numbers: bool,
    before_context: usize,
    after_context: usize,
    multiline: bool,
    head_limit: Option<usize>,
    offset: usize,
}

/// Native `Grep` tool implementation.
#[derive(Default)]
pub struct GrepTool {
    /// Optional default root used when the LLM call omits `path`.
    ///
    /// Set via [`with_workspace_root`](Self::with_workspace_root) at
    /// registration time. When `None`, callers must supply an absolute `path`.
    workspace_root: Option<PathBuf>,
}

impl GrepTool {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the default search root for calls that omit `path`.
    #[must_use]
    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace_root = Some(root);
        self
    }

    #[instrument(skip(self, opts), fields(pattern = %opts.pattern, mode = ?opts.output_mode))]
    fn grep_impl(&self, opts: GrepOptions) -> Result<Value> {
        let regex = RegexBuilder::new(&opts.pattern)
            .case_insensitive(opts.case_insensitive)
            .multi_line(opts.multiline)
            .dot_matches_new_line(opts.multiline)
            .build()
            .map_err(|e| Error::message(format!("invalid regex '{}': {e}", opts.pattern)))?;

        let root_canonical = std::fs::canonicalize(&opts.path).map_err(|e| {
            Error::message(format!(
                "cannot resolve grep path '{}': {e}",
                opts.path.display()
            ))
        })?;

        // If the root is a single file, search just that file.
        let is_file = std::fs::metadata(&root_canonical)
            .map(|m| m.is_file())
            .unwrap_or(false);

        let mut files: Vec<PathBuf> = Vec::new();
        if is_file {
            files.push(root_canonical.clone());
        } else {
            let walker = WalkBuilder::new(&root_canonical)
                .hidden(false)
                .git_ignore(true)
                .git_exclude(true)
                .git_global(true)
                .build();
            for entry in walker {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let path = entry.path().to_path_buf();
                if let Some(ref glob) = opts.glob {
                    let relative = path.strip_prefix(&root_canonical).unwrap_or(&path);
                    if !glob.is_match(relative) {
                        continue;
                    }
                }
                if let Some(ref ft) = opts.file_type
                    && !file_matches_type(&path, ft)
                {
                    continue;
                }
                files.push(path);
            }
        }

        // Sort files for deterministic output.
        files.sort();

        match opts.output_mode {
            OutputMode::FilesWithMatches => {
                let mut matched: Vec<String> = Vec::new();
                for path in &files {
                    if let Some(bytes) = read_bounded(path)
                        && let Ok(text) = std::str::from_utf8(&bytes)
                        && regex.is_match(text)
                    {
                        matched.push(path.to_string_lossy().into_owned());
                    }
                }
                let (rows, truncated) = apply_head_offset(matched, opts.offset, opts.head_limit);
                Ok(json!({
                    "mode": "files_with_matches",
                    "files": rows,
                    "truncated": truncated,
                }))
            },
            OutputMode::Count => {
                let mut counts: Vec<Value> = Vec::new();
                for path in &files {
                    let Some(bytes) = read_bounded(path) else {
                        continue;
                    };
                    let Ok(text) = std::str::from_utf8(&bytes) else {
                        continue;
                    };
                    let count = regex.find_iter(text).count();
                    if count > 0 {
                        counts.push(json!({
                            "path": path.to_string_lossy(),
                            "count": count,
                        }));
                    }
                }
                let (rows, truncated) = apply_head_offset(counts, opts.offset, opts.head_limit);
                Ok(json!({
                    "mode": "count",
                    "counts": rows,
                    "truncated": truncated,
                }))
            },
            OutputMode::Content => {
                let mut rows: Vec<Value> = Vec::new();
                'outer: for path in &files {
                    let Some(bytes) = read_bounded(path) else {
                        continue;
                    };
                    let Ok(text) = std::str::from_utf8(&bytes) else {
                        continue;
                    };
                    let file_rows = collect_content_matches(
                        path,
                        text,
                        &regex,
                        opts.show_line_numbers,
                        opts.before_context,
                        opts.after_context,
                    );
                    for row in file_rows {
                        if rows.len() >= DEFAULT_GREP_MATCH_CAP {
                            break 'outer;
                        }
                        rows.push(row);
                    }
                }
                let (rows, truncated) = apply_head_offset(rows, opts.offset, opts.head_limit);
                Ok(json!({
                    "mode": "content",
                    "matches": rows,
                    "truncated": truncated,
                }))
            },
        }
    }
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > DEFAULT_GREP_FILE_CAP {
        return None;
    }
    std::fs::read(path).ok()
}

fn collect_content_matches(
    path: &Path,
    text: &str,
    regex: &regex::Regex,
    show_line_numbers: bool,
    before: usize,
    after: usize,
) -> Vec<Value> {
    let lines: Vec<&str> = text.lines().collect();
    let mut rows: Vec<Value> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if !regex.is_match(line) {
            continue;
        }
        let start = idx.saturating_sub(before);
        let end = (idx + after + 1).min(lines.len());
        let block: Vec<String> = lines[start..end]
            .iter()
            .enumerate()
            .map(|(offset, line)| {
                let ctx_idx = start + offset;
                if show_line_numbers {
                    format!("{}:{line}", ctx_idx + 1)
                } else {
                    (*line).to_string()
                }
            })
            .collect();
        rows.push(json!({
            "path": path.to_string_lossy(),
            "line": idx + 1,
            "match": line,
            "block": block,
        }));
    }
    rows
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

fn file_matches_type(path: &Path, ty: &str) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        (ty, ext),
        ("rust", "rs")
            | ("py", "py")
            | ("python", "py")
            | ("js", "js" | "mjs" | "cjs")
            | ("ts", "ts" | "tsx")
            | ("tsx", "tsx")
            | ("jsx", "jsx")
            | ("go", "go")
            | ("java", "java")
            | ("c", "c" | "h")
            | ("cpp", "cpp" | "cc" | "cxx" | "hpp" | "hh")
            | ("md", "md" | "markdown")
            | ("toml", "toml")
            | ("yaml", "yaml" | "yml")
            | ("json", "json")
            | ("html", "html" | "htm")
            | ("css", "css")
            | ("sh", "sh" | "bash")
    )
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression. Three output modes: \
         'files_with_matches' (default, lists matching file paths), 'content' \
         (lines that matched, with optional context), 'count' (match count \
         per file). Supports glob and file-type filters, case-insensitive \
         matching, line numbers, and context lines."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression pattern (Rust regex syntax)."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search. Defaults to the current working directory."
                },
                "glob": {
                    "type": "string",
                    "description": "Glob filter for file paths (e.g. '**/*.rs')."
                },
                "type": {
                    "type": "string",
                    "description": "File type filter (e.g. 'rust', 'py', 'ts')."
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "default": "files_with_matches"
                },
                "-i": {
                    "type": "boolean",
                    "description": "Case-insensitive match."
                },
                "-n": {
                    "type": "boolean",
                    "description": "Show line numbers (content mode)."
                },
                "-A": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Lines of trailing context (content mode)."
                },
                "-B": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Lines of leading context (content mode)."
                },
                "-C": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Lines of context on both sides (content mode)."
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Enable multi-line mode (`.` matches newlines)."
                },
                "head_limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum rows to return after offset."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Skip this many rows before applying head_limit."
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let pattern = params
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'pattern' parameter"))?
            .to_string();

        let path = match params.get("path").and_then(Value::as_str) {
            Some(raw) => {
                require_absolute(raw, "path")?;
                PathBuf::from(raw)
            },
            None => self.workspace_root.clone().ok_or_else(|| {
                Error::message(
                    "Grep requires an absolute 'path' argument (no workspace root is configured)",
                )
            })?,
        };

        let glob = params
            .get("glob")
            .and_then(Value::as_str)
            .map(|raw| -> Result<GlobMatcher> {
                Ok(GlobPattern::new(raw)
                    .map_err(|e| Error::message(format!("invalid glob '{raw}': {e}")))?
                    .compile_matcher())
            })
            .transpose()?;

        let file_type = params
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);

        let output_mode = OutputMode::parse(params.get("output_mode").and_then(Value::as_str))?;

        let case_insensitive = params.get("-i").and_then(Value::as_bool).unwrap_or(false);
        let show_line_numbers = params.get("-n").and_then(Value::as_bool).unwrap_or(false);
        let context_symmetric = params
            .get("-C")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(0);
        let before_context = params
            .get("-B")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(context_symmetric);
        let after_context = params
            .get("-A")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(context_symmetric);
        let multiline = params
            .get("multiline")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let head_limit = params
            .get("head_limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let offset = params
            .get("offset")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(0);

        let opts = GrepOptions {
            pattern,
            path,
            glob,
            file_type,
            output_mode,
            case_insensitive,
            show_line_numbers,
            before_context,
            after_context,
            multiline,
            head_limit,
            offset,
        };

        let workspace_root = self.workspace_root.clone();
        let result = tokio::task::spawn_blocking(move || {
            let tool = Self { workspace_root };
            tool.grep_impl(opts)
        })
        .await
        .map_err(|e| Error::message(format!("grep task failed: {e}")))?;

        match result {
            Ok(value) => {
                #[cfg(feature = "metrics")]
                counter!(
                    tools_metrics::EXECUTIONS_TOTAL,
                    labels::TOOL => "Grep".to_string(),
                    labels::SUCCESS => "true".to_string()
                )
                .increment(1);
                Ok(value)
            },
            Err(e) => {
                #[cfg(feature = "metrics")]
                counter!(
                    tools_metrics::EXECUTION_ERRORS_TOTAL,
                    labels::TOOL => "Grep".to_string()
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
    use super::*;

    async fn setup_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("lib.rs"),
            "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n",
        )
        .await
        .unwrap();
        tokio::fs::write(dir.path().join("notes.md"), "alpha notes\nbeta notes\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("binary.bin"), [0u8, 1, 2, 3, 4, 5])
            .await
            .unwrap();
        dir
    }

    #[tokio::test]
    async fn grep_files_with_matches_default() {
        let dir = setup_tree().await;
        let tool = GrepTool::new();
        let value = tool
            .execute(json!({
                "pattern": "beta",
                "path": dir.path().to_str().unwrap(),
            }))
            .await
            .unwrap();

        assert_eq!(value["mode"], "files_with_matches");
        let files: Vec<String> = value["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|p| p.ends_with("lib.rs")));
        assert!(files.iter().any(|p| p.ends_with("notes.md")));
    }

    #[tokio::test]
    async fn grep_count_mode() {
        let dir = setup_tree().await;
        let tool = GrepTool::new();
        let value = tool
            .execute(json!({
                "pattern": "fn ",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "count",
            }))
            .await
            .unwrap();

        let counts = value["counts"].as_array().unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0]["count"], 3);
    }

    #[tokio::test]
    async fn grep_content_mode_with_line_numbers() {
        let dir = setup_tree().await;
        let tool = GrepTool::new();
        let value = tool
            .execute(json!({
                "pattern": "beta",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
                "-n": true,
            }))
            .await
            .unwrap();

        let matches = value["matches"].as_array().unwrap();
        assert!(!matches.is_empty());
        let first = &matches[0];
        let block = first["block"].as_array().unwrap();
        assert!(block[0].as_str().unwrap().contains(':'));
    }

    #[tokio::test]
    async fn grep_case_insensitive() {
        let dir = setup_tree().await;
        let tool = GrepTool::new();
        let value = tool
            .execute(json!({
                "pattern": "ALPHA",
                "path": dir.path().to_str().unwrap(),
                "-i": true,
            }))
            .await
            .unwrap();

        let files = value["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
    }

    #[tokio::test]
    async fn grep_glob_filter() {
        let dir = setup_tree().await;
        let tool = GrepTool::new();
        let value = tool
            .execute(json!({
                "pattern": "alpha",
                "path": dir.path().to_str().unwrap(),
                "glob": "*.rs",
            }))
            .await
            .unwrap();

        let files = value["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].as_str().unwrap().ends_with("lib.rs"));
    }

    #[tokio::test]
    async fn grep_type_filter() {
        let dir = setup_tree().await;
        let tool = GrepTool::new();
        let value = tool
            .execute(json!({
                "pattern": "alpha",
                "path": dir.path().to_str().unwrap(),
                "type": "rust",
            }))
            .await
            .unwrap();

        let files = value["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].as_str().unwrap().ends_with("lib.rs"));
    }

    #[tokio::test]
    async fn grep_head_limit_and_offset() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            tokio::fs::write(dir.path().join(format!("f{i}.txt")), "match\n")
                .await
                .unwrap();
        }
        let tool = GrepTool::new();
        let value = tool
            .execute(json!({
                "pattern": "match",
                "path": dir.path().to_str().unwrap(),
                "head_limit": 2,
                "offset": 1,
            }))
            .await
            .unwrap();

        let files = value["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(value["truncated"], true);
    }

    #[tokio::test]
    async fn grep_invalid_regex_errors() {
        let dir = tempfile::tempdir().unwrap();
        let tool = GrepTool::new();
        let err = tool
            .execute(json!({
                "pattern": "[invalid",
                "path": dir.path().to_str().unwrap(),
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid regex"));
    }

    #[tokio::test]
    async fn grep_missing_path_without_workspace_root_errors() {
        let tool = GrepTool::new();
        let err = tool.execute(json!({ "pattern": "foo" })).await.unwrap_err();
        assert!(err.to_string().contains("no workspace root"));
    }

    #[tokio::test]
    async fn grep_rejects_relative_path() {
        let tool = GrepTool::new();
        let err = tool
            .execute(json!({
                "pattern": "foo",
                "path": "rel/dir",
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be an absolute path"));
    }

    #[tokio::test]
    async fn grep_falls_back_to_workspace_root() {
        let dir = setup_tree().await;
        let tool = GrepTool::new().with_workspace_root(dir.path().to_path_buf());
        let value = tool.execute(json!({ "pattern": "alpha" })).await.unwrap();
        let files = value["files"].as_array().unwrap();
        assert!(!files.is_empty());
    }

    #[tokio::test]
    async fn grep_single_file_path() {
        let dir = setup_tree().await;
        let tool = GrepTool::new();
        let value = tool
            .execute(json!({
                "pattern": "alpha",
                "path": dir.path().join("lib.rs").to_str().unwrap(),
            }))
            .await
            .unwrap();

        let files = value["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
    }
}
