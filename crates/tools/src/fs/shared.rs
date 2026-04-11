//! Shared helpers for the native filesystem tools (Read/Write/Edit/MultiEdit/Glob/Grep).
//!
//! Kept deliberately small: path canonicalization, binary detection, and the
//! `cat -n` style line-number formatter that callers use to render file
//! contents back to the LLM.

use {
    std::path::{Path, PathBuf},
    tokio::fs,
};

use crate::{Result, error::Error};

/// Maximum file size the fs tools will read in a single call (phase 1 cap).
///
/// Phase 4 will make this configurable via `[tools.fs] max_read_bytes`.
pub const DEFAULT_MAX_READ_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum number of lines returned by a single `Read` call when no explicit
/// limit is provided.
pub const DEFAULT_READ_LINE_LIMIT: usize = 2000;

/// Maximum characters per line before the line is truncated in `Read` output.
///
/// Mirrors Claude Code's behavior so LLMs trained on it encounter the same
/// shape of response.
pub const MAX_LINE_CHARS: usize = 2000;

/// Number of bytes to inspect when heuristically detecting binary content.
const BINARY_SNIFF_BYTES: usize = 8192;

/// Reject relative paths at the tool boundary.
///
/// Claude Code's fs tools require absolute paths. We enforce the same to
/// avoid silent resolution against the gateway's process cwd, which is
/// almost never what the LLM means. See `moltis-ung` for context.
pub fn require_absolute(path: &str, field: &str) -> Result<()> {
    if path.is_empty() {
        return Err(Error::message(format!("{field} must not be empty")));
    }
    if !Path::new(path).is_absolute() {
        return Err(Error::message(format!(
            "{field} must be an absolute path (got '{path}'); relative paths are not supported \
             to avoid silent resolution against the gateway process working directory"
        )));
    }
    Ok(())
}

/// Canonicalize a user-supplied path. Requires the path to exist and be absolute.
///
/// Returns an absolute, symlink-resolved `PathBuf`. All fs tools canonicalize
/// at the boundary so symlink escapes can't bypass future path allowlists.
pub async fn canonicalize_existing(path: &str) -> Result<PathBuf> {
    require_absolute(path, "file_path")?;
    fs::canonicalize(path)
        .await
        .map_err(|e| Error::message(format!("cannot resolve path '{path}': {e}")))
}

/// Canonicalize a path that may not exist yet (e.g. for `Write` to a new file).
///
/// Requires the path to be absolute. Canonicalizes the parent directory and
/// appends the final component. Returns an error if the parent does not
/// exist or is not a directory.
pub async fn canonicalize_for_create(path: &str) -> Result<PathBuf> {
    require_absolute(path, "file_path")?;
    let pb = PathBuf::from(path);
    let parent = pb
        .parent()
        .ok_or_else(|| Error::message(format!("path '{path}' has no parent directory")))?;
    let file_name = pb
        .file_name()
        .ok_or_else(|| Error::message(format!("path '{path}' has no file name")))?;

    let parent_canonical = fs::canonicalize(parent)
        .await
        .map_err(|e| Error::message(format!("cannot resolve parent of '{path}': {e}")))?;

    Ok(parent_canonical.join(file_name))
}

/// Reject paths that resolve through a symlink.
///
/// Called after `canonicalize_existing` when policy requires the final path to
/// be a real file. The canonicalized path itself is not a symlink (canonicalize
/// resolves them), so this checks the *original* path's metadata separately.
pub async fn reject_if_symlink(original: &str) -> Result<()> {
    let meta = fs::symlink_metadata(original)
        .await
        .map_err(|e| Error::message(format!("cannot stat '{original}': {e}")))?;
    if meta.file_type().is_symlink() {
        return Err(Error::message(format!(
            "'{original}' is a symbolic link; refusing to follow"
        )));
    }
    Ok(())
}

/// Ensure the path is a regular file (not a directory, fifo, socket, etc).
pub async fn ensure_regular_file(path: &Path) -> Result<u64> {
    let meta = fs::metadata(path)
        .await
        .map_err(|e| Error::message(format!("cannot stat '{}': {e}", path.display())))?;
    if !meta.is_file() {
        return Err(Error::message(format!(
            "'{}' is not a regular file",
            path.display()
        )));
    }
    Ok(meta.len())
}

/// Heuristic binary detection: if the first `BINARY_SNIFF_BYTES` contain a null
/// byte, treat as binary. Mirrors what `grep` and most text editors do.
#[must_use]
pub fn looks_binary(bytes: &[u8]) -> bool {
    let limit = bytes.len().min(BINARY_SNIFF_BYTES);
    bytes[..limit].contains(&0)
}

/// Render file contents with `cat -n` style 1-indexed line numbers.
///
/// The line-number column is right-padded to the width of the last visible
/// line number so columns align. Lines longer than [`MAX_LINE_CHARS`] are
/// truncated with a `…` marker appended.
#[must_use]
pub fn format_numbered_lines(content: &str, start_line: usize, max_lines: usize) -> NumberedLines {
    // Split on '\n' so that a trailing newline does not create an extra empty
    // line in the output (common cat -n behavior).
    let mut lines: Vec<&str> = content.split('\n').collect();
    let had_trailing_newline = content.ends_with('\n');
    if had_trailing_newline {
        lines.pop();
    }

    let total_lines = lines.len();
    let start = start_line.max(1);
    if start > total_lines {
        return NumberedLines {
            text: String::new(),
            total_lines,
            start_line: start,
            rendered_lines: 0,
            truncated: false,
        };
    }

    let end_exclusive = total_lines.min(start.saturating_sub(1).saturating_add(max_lines));
    let visible = &lines[start.saturating_sub(1)..end_exclusive];
    let last_line_number = start.saturating_add(visible.len().saturating_sub(1));
    let width = decimal_width(last_line_number);

    let mut out = String::new();
    for (offset, raw) in visible.iter().enumerate() {
        let line_number = start.saturating_add(offset);
        // Strip a trailing '\r' so CRLF-authored files render cleanly.
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let (body, truncated) = truncate_line(raw);
        // 6-space padding minimum to match Claude Code's visual style.
        let pad_width = width.max(6);
        out.push_str(&format!("{line_number:>pad_width$}→{body}"));
        if truncated {
            out.push('…');
        }
        out.push('\n');
    }

    NumberedLines {
        text: out,
        total_lines,
        start_line: start,
        rendered_lines: visible.len(),
        truncated: end_exclusive < total_lines,
    }
}

/// Result of [`format_numbered_lines`], including enough metadata for the
/// tool response payload to describe what was rendered.
#[derive(Debug, Clone)]
pub struct NumberedLines {
    pub text: String,
    pub total_lines: usize,
    pub start_line: usize,
    pub rendered_lines: usize,
    pub truncated: bool,
}

fn truncate_line(line: &str) -> (&str, bool) {
    if line.chars().count() <= MAX_LINE_CHARS {
        return (line, false);
    }
    // Find the byte index that covers MAX_LINE_CHARS chars.
    let mut iter = line.char_indices();
    let cutoff_byte = iter.nth(MAX_LINE_CHARS).map_or(line.len(), |(idx, _)| idx);
    (&line[..cutoff_byte], true)
}

fn decimal_width(n: usize) -> usize {
    let mut n = n.max(1);
    let mut width = 0usize;
    while n > 0 {
        width = width.saturating_add(1);
        n /= 10;
    }
    width
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {super::*, std::io::Write};

    #[test]
    fn decimal_width_examples() {
        assert_eq!(decimal_width(0), 1);
        assert_eq!(decimal_width(1), 1);
        assert_eq!(decimal_width(9), 1);
        assert_eq!(decimal_width(10), 2);
        assert_eq!(decimal_width(999), 3);
    }

    #[test]
    fn format_numbered_lines_basic() {
        let content = "alpha\nbeta\ngamma\n";
        let rendered = format_numbered_lines(content, 1, 100);
        assert_eq!(rendered.total_lines, 3);
        assert_eq!(rendered.rendered_lines, 3);
        assert!(!rendered.truncated);
        assert_eq!(rendered.text, "     1→alpha\n     2→beta\n     3→gamma\n");
    }

    #[test]
    fn format_numbered_lines_respects_offset_and_limit() {
        let content = "a\nb\nc\nd\ne\n";
        let rendered = format_numbered_lines(content, 2, 2);
        assert_eq!(rendered.rendered_lines, 2);
        assert!(rendered.truncated);
        assert_eq!(rendered.text, "     2→b\n     3→c\n");
    }

    #[test]
    fn format_numbered_lines_truncates_long_lines() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let content = format!("{long}\n");
        let rendered = format_numbered_lines(&content, 1, 100);
        // Should end with the ellipsis marker before the newline.
        assert!(rendered.text.trim_end().ends_with('…'));
    }

    #[test]
    fn format_numbered_lines_strips_crlf() {
        let content = "alpha\r\nbeta\r\n";
        let rendered = format_numbered_lines(content, 1, 100);
        assert_eq!(rendered.text, "     1→alpha\n     2→beta\n");
    }

    #[test]
    fn format_numbered_lines_handles_no_trailing_newline() {
        let content = "alpha\nbeta";
        let rendered = format_numbered_lines(content, 1, 100);
        assert_eq!(rendered.total_lines, 2);
        assert_eq!(rendered.rendered_lines, 2);
        assert_eq!(rendered.text, "     1→alpha\n     2→beta\n");
    }

    #[test]
    fn format_numbered_lines_start_beyond_end() {
        let content = "a\nb\n";
        let rendered = format_numbered_lines(content, 10, 5);
        assert_eq!(rendered.total_lines, 2);
        assert_eq!(rendered.rendered_lines, 0);
        assert!(rendered.text.is_empty());
    }

    #[test]
    fn looks_binary_detects_null_bytes() {
        assert!(!looks_binary(b"hello world"));
        assert!(looks_binary(b"hello\0world"));
    }

    #[test]
    fn looks_binary_empty() {
        assert!(!looks_binary(b""));
    }

    #[tokio::test]
    async fn canonicalize_existing_resolves_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"hi").unwrap();
        let resolved = canonicalize_existing(tmp.path().to_str().unwrap())
            .await
            .unwrap();
        assert!(resolved.is_absolute());
    }

    #[tokio::test]
    async fn canonicalize_existing_missing_errors() {
        let err = canonicalize_existing("/tmp/definitely-does-not-exist-5b7c1")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot resolve path"));
    }

    #[tokio::test]
    async fn canonicalize_existing_rejects_relative() {
        let err = canonicalize_existing("relative/path.txt")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be an absolute path"));
    }

    #[tokio::test]
    async fn canonicalize_existing_rejects_empty() {
        let err = canonicalize_existing("").await.unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[tokio::test]
    async fn canonicalize_for_create_resolves_parent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("new-file.txt");
        let resolved = canonicalize_for_create(target.to_str().unwrap())
            .await
            .unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved.file_name().unwrap(), "new-file.txt");
    }

    #[tokio::test]
    async fn canonicalize_for_create_missing_parent_errors() {
        let err = canonicalize_for_create("/tmp/does-not-exist-parent-99/out.txt")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot resolve parent"));
    }

    #[tokio::test]
    async fn canonicalize_for_create_rejects_relative() {
        let err = canonicalize_for_create("out.txt").await.unwrap_err();
        assert!(err.to_string().contains("must be an absolute path"));
    }

    #[tokio::test]
    async fn ensure_regular_file_rejects_dir() {
        let dir = tempfile::tempdir().unwrap();
        let err = ensure_regular_file(dir.path()).await.unwrap_err();
        assert!(err.to_string().contains("not a regular file"));
    }
}
