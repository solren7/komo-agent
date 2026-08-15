//! The `read` tool: page through a text file, list a directory, and refuse
//! binaries with an explanation.
//!
//! Split out of the old `file{action:"read"}`, which read whole files and cut
//! them off at 64 KB — so the tail of anything larger was simply unreachable.
//! Modeled on opencode v2's `read` (`packages/core/src/tool/read.ts` +
//! `read-filesystem.ts`): `offset`/`limit` paging with a `next` hint, directory
//! listing, magic-byte binary detection, per-line truncation.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fs_common;
use komo_core::domain::{
    context::ToolContext,
    tool::{Tool, ToolError, ToolOutput, parse_args},
    workspace::Workspace,
};

/// Lines returned by one call, unless `limit` asks for fewer.
const MAX_LINES: usize = 2_000;
/// Byte ceiling on one page's content — whichever limit is hit first wins, so a
/// file of very long lines can't blow the context window on 2000 of them.
const MAX_PAGE_BYTES: usize = 50 * 1024;
/// A single line longer than this is truncated: one minified bundle line would
/// otherwise consume the whole page budget.
const MAX_LINE_CHARS: usize = 2_000;
/// Files above this never get read into memory. `grep` (bounded output) or a
/// `shell` pipeline is the right tool for those.
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// Directory entries returned by one call.
const MAX_ENTRIES: usize = 500;

/// Extensions that are binary regardless of content sniffing.
const BINARY_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib", "class", "jar",
    "war", "bin", "dat", "obj", "o", "a", "lib", "wasm", "pyc", "pyo", "pdf", "doc", "docx", "xls",
    "xlsx", "ppt", "pptx", "odt", "ods", "odp", "sqlite", "db",
];

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    /// 1-based first line (or directory entry) to return.
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// Reads text files and directories from the local filesystem.
pub struct ReadTool {
    workspace: Arc<Workspace>,
}

impl ReadTool {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a text file with line numbers, or list a directory. Absolute paths may \
         be anywhere locally; relative paths resolve inside the workspace. Long files are paged: pass `offset` (the \
         1-based line to start at) and `limit` to walk through one; the result \
         tells you the next offset when there is more. Binary files are refused \
         rather than dumped as garbage."
    }

    /// Read-only: safe to retry after an ambiguous transient failure.
    fn idempotent(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File or directory path. Absolute paths may be anywhere locally; relative paths resolve from the workspace root."
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based line (or directory entry) to start at. Default 1."
                },
                "limit": {
                    "type": "integer",
                    "description": format!("Maximum lines to return (default and maximum {MAX_LINES}).")
                }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ReadArgs = parse_args(&input)?;
        let path = fs_common::resolve_readable(&self.workspace, ctx, &args.path)?;

        if let Some(refusal) = fs_common::allow_read(ctx, &path).await {
            return Ok(ToolOutput::text(refusal));
        }

        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| ToolError::InvalidInput(format!("cannot read {}: {e}", path.display())))?;

        if meta.is_dir() {
            return list_dir(&path, args.offset, args.limit).await;
        }

        if meta.len() > MAX_FILE_BYTES {
            return Err(ToolError::InvalidInput(format!(
                "{} is {} bytes — too large to read ({} byte limit). Use `grep` to find \
                 what you need, or a `shell` pipeline to extract a slice.",
                path.display(),
                meta.len(),
                MAX_FILE_BYTES
            )));
        }

        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            ToolError::Failed(anyhow::anyhow!("failed to read {}: {e}", path.display()))
        })?;

        // Binary check before any decoding: a lossy decode would hand the model
        // a page of replacement characters and call it success.
        if let Some(kind) = binary_kind(&path, &bytes) {
            return Ok(ToolOutput::text(format!(
                "{} is a {kind} file ({} bytes), not text — nothing was read. \
                 komo cannot show binary content to the model yet.",
                path.display(),
                bytes.len()
            )));
        }

        let text = String::from_utf8(bytes).map_err(|_| {
            ToolError::InvalidInput(format!(
                "{} is not valid UTF-8; it cannot be read as text.",
                path.display()
            ))
        })?;

        Ok(page(&path, &text, args.offset, args.limit))
    }
}

/// Render one page of `text` with line numbers, plus a continuation hint.
fn page(path: &Path, text: &str, offset: Option<usize>, limit: Option<usize>) -> ToolOutput {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    // A 1-based offset; 0 and absent both mean "from the top".
    let start = offset.unwrap_or(1).max(1);
    let want = limit.unwrap_or(MAX_LINES).clamp(1, MAX_LINES);

    if total == 0 {
        return ToolOutput::text(format!("{} is empty (0 lines).", path.display()))
            .with_structured(json!({ "total_lines": 0 }));
    }
    if start > total {
        return ToolOutput::text(format!(
            "{}: offset {start} is past the end of the file ({total} lines).",
            path.display()
        ))
        .with_structured(json!({ "total_lines": total, "offset": start }));
    }

    // Widest line number in this page, so the gutter doesn't jitter.
    let last_possible = (start + want - 1).min(total);
    let width = last_possible.to_string().len();

    let mut body = String::new();
    let mut shown = 0usize;
    let mut bytes = 0usize;
    let mut byte_capped = false;
    for (i, line) in lines[start - 1..].iter().take(want).enumerate() {
        let (line, clipped) = truncate_chars(line, MAX_LINE_CHARS);
        let rendered = format!(
            "{:>width$}│{line}{}\n",
            start + i,
            if clipped { " …[line truncated]" } else { "" },
            width = width
        );
        if bytes + rendered.len() > MAX_PAGE_BYTES && shown > 0 {
            byte_capped = true;
            break;
        }
        bytes += rendered.len();
        body.push_str(&rendered);
        shown += 1;
    }

    let end = start + shown - 1;
    let next = (end < total).then_some(end + 1);
    let mut header = format!("{} (lines {start}-{end} of {total})", path.display());
    if byte_capped {
        header.push_str(&format!(
            " — page cut at the {} KB limit",
            MAX_PAGE_BYTES / 1024
        ));
    }
    let mut out = format!("{header}\n{body}");
    if let Some(next) = next {
        out.push_str(&format!(
            "…{} more lines. Continue with offset={next}.",
            total - end
        ));
    }

    ToolOutput::text(out)
        .with_title(format!("read {} ({shown} lines)", path.display()))
        .with_structured(json!({
            "total_lines": total,
            "offset": start,
            "shown": shown,
            "next_offset": next,
            // The page as data: the same lines, without the header or the line
            // gutter, and without the per-line clipping — which is layout, and a
            // reader's concern rather than a program's. This is what a `run_code`
            // program computes on; parsing the rendered page instead is how the
            // first ones got their line counts wrong. Never sent to the model.
            "text": lines[start - 1..][..shown].join("\n"),
        }))
}

/// List one page of a directory: names only, sorted, directories marked `/`.
async fn list_dir(
    path: &Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<ToolOutput, ToolError> {
    let mut entries: Vec<String> = Vec::new();
    let mut dir = tokio::fs::read_dir(path).await.map_err(|e| {
        ToolError::Failed(anyhow::anyhow!("failed to list {}: {e}", path.display()))
    })?;
    while let Some(entry) = dir
        .next_entry()
        .await
        .map_err(|e| ToolError::Failed(anyhow::anyhow!("failed to list {}: {e}", path.display())))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        entries.push(if is_dir { format!("{name}/") } else { name });
    }
    entries.sort();

    let total = entries.len();
    if total == 0 {
        return Ok(ToolOutput::text(format!("{} is empty.", path.display())));
    }
    let start = offset.unwrap_or(1).max(1);
    if start > total {
        return Ok(ToolOutput::text(format!(
            "{}: offset {start} is past the last entry ({total} entries).",
            path.display()
        )));
    }
    let want = limit.unwrap_or(MAX_ENTRIES).clamp(1, MAX_ENTRIES);
    let page: Vec<&String> = entries[start - 1..].iter().take(want).collect();
    let end = start + page.len() - 1;

    let mut out = format!(
        "{} (directory, entries {start}-{end} of {total})\n{}",
        path.display(),
        page.iter()
            .map(|e| format!("  {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    if end < total {
        out.push_str(&format!(
            "\n…{} more entries. Continue with offset={}.",
            total - end,
            end + 1
        ));
    }
    Ok(ToolOutput::text(out)
        .with_title(format!("list {} ({} entries)", path.display(), page.len()))
        .with_structured(json!({ "total_entries": total, "offset": start })))
}

/// What kind of binary this is, or `None` when it reads as text. Extension first
/// (cheap and certain), then magic bytes for the common image formats, then the
/// content heuristic opencode uses: any NUL byte, or >30% non-printable.
fn binary_kind(path: &Path, bytes: &[u8]) -> Option<&'static str> {
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && BINARY_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    {
        return Some("binary");
    }
    if let Some(image) = image_kind(bytes) {
        return Some(image);
    }
    if bytes.is_empty() {
        return None;
    }
    let mut nonprintable = 0usize;
    for &b in bytes.iter().take(8 * 1024) {
        if b == 0 {
            return Some("binary");
        }
        if b < 9 || (b > 13 && b < 32) {
            nonprintable += 1;
        }
    }
    let sampled = bytes.len().min(8 * 1024);
    (nonprintable * 10 > sampled * 3).then_some("binary")
}

/// Magic-byte sniff for the image formats a future attachment path will care
/// about (issue 16); for now it only makes the refusal message specific.
fn image_kind(bytes: &[u8]) -> Option<&'static str> {
    let starts = |prefix: &[u8]| bytes.starts_with(prefix);
    if starts(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("PNG image");
    }
    if starts(&[0xff, 0xd8, 0xff]) {
        return Some("JPEG image");
    }
    if starts(b"GIF8") {
        return Some("GIF image");
    }
    if starts(b"RIFF") && bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        return Some("WebP image");
    }
    None
}

/// Truncate to `max` **characters** (never mid-codepoint), reporting whether it
/// clipped.
fn truncate_chars(line: &str, max: usize) -> (String, bool) {
    if line.chars().count() <= max {
        return (line.to_string(), false);
    }
    (line.chars().take(max).collect(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::detached_ctx;
    use std::path::PathBuf;

    /// A workspace rooted at a fresh temp dir, plus the tool over it.
    fn tool_in(tag: &str) -> (ReadTool, PathBuf) {
        let dir = std::env::temp_dir().join(format!("komo_read_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (
            ReadTool::new(Arc::new(Workspace::new(vec![dir.clone()]))),
            dir,
        )
    }

    /// The other half of the tool-output store: a preview hands the model a path
    /// **outside** the workspace, so `read` has to be able to open it — otherwise
    /// the pointer is decoration.
    #[tokio::test]
    async fn reads_a_managed_path_outside_the_workspace() {
        let base = std::env::temp_dir().join("komo_read_managed");
        let _ = std::fs::remove_dir_all(&base);
        let workspace_dir = base.join("project");
        let managed = base.join("tool-output");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::create_dir_all(&managed).unwrap();
        let stored = managed.join("cli-t").join("run-0000.txt");
        std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
        std::fs::write(&stored, "stored line\n").unwrap();

        let tool = ReadTool::new(Arc::new(
            Workspace::new(vec![workspace_dir]).with_readonly(vec![managed.clone()]),
        ));
        let out = tool
            .call(
                json!({ "path": stored.display().to_string() }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();
        assert!(out.text.contains("stored line"), "{}", out.text);

        // A sibling of the managed root is still refused — the widening is scoped.
        let outside = base.join("elsewhere.txt");
        std::fs::write(&outside, "secret").unwrap();
        let err = tool
            .call(
                json!({ "path": outside.display().to_string() }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn reads_with_line_numbers_and_a_header() {
        let (tool, dir) = tool_in("basic");
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let out = tool
            .call(json!({ "path": "a.txt" }), &detached_ctx("cli:t"))
            .await
            .unwrap();
        assert!(out.text.contains("lines 1-3 of 3"), "{}", out.text);
        assert!(out.text.contains("1│one"), "{}", out.text);
        assert!(out.text.contains("3│three"), "{}", out.text);
        // Nothing more to read, so no continuation hint.
        assert!(!out.text.contains("Continue with"));
    }

    /// The regression this tool exists for: the old `file` read truncated at
    /// 64 KB and the tail was simply unreachable.
    #[tokio::test]
    async fn pages_a_long_file_and_the_tail_is_reachable() {
        let (tool, dir) = tool_in("paging");
        let body: String = (1..=5000).map(|i| format!("line{i}\n")).collect();
        std::fs::write(dir.join("big.txt"), &body).unwrap();
        let ctx = detached_ctx("cli:t");

        let first = tool.call(json!({ "path": "big.txt" }), &ctx).await.unwrap();
        assert!(
            first.text.contains("lines 1-2000 of 5000"),
            "{}",
            first.text
        );
        assert!(first.text.contains("Continue with offset=2001"));

        let last = tool
            .call(json!({ "path": "big.txt", "offset": 4999 }), &ctx)
            .await
            .unwrap();
        assert!(last.text.contains("line5000"), "the tail is reachable");
        assert!(!last.text.contains("Continue with"));
        assert_eq!(last.structured["next_offset"], Value::Null);
    }

    #[tokio::test]
    async fn limit_bounds_the_page_and_reports_the_next_offset() {
        let (tool, dir) = tool_in("limit");
        std::fs::write(dir.join("a.txt"), "1\n2\n3\n4\n5\n").unwrap();
        let out = tool
            .call(
                json!({ "path": "a.txt", "offset": 2, "limit": 2 }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();
        assert!(out.text.contains("lines 2-3 of 5"), "{}", out.text);
        assert_eq!(out.structured["next_offset"], 4);
    }

    #[tokio::test]
    async fn offset_past_the_end_says_so_instead_of_erroring() {
        let (tool, dir) = tool_in("past_end");
        std::fs::write(dir.join("a.txt"), "only\n").unwrap();
        let out = tool
            .call(
                json!({ "path": "a.txt", "offset": 99 }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();
        assert!(out.text.contains("past the end"), "{}", out.text);
    }

    #[tokio::test]
    async fn directories_are_listed_not_rejected() {
        let (tool, dir) = tool_in("dir");
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let out = tool
            .call(json!({ "path": "." }), &detached_ctx("cli:t"))
            .await
            .unwrap();
        assert!(out.text.contains("directory"), "{}", out.text);
        assert!(out.text.contains("sub/"), "dirs are marked: {}", out.text);
        assert!(out.text.contains("a.txt"));
    }

    #[tokio::test]
    async fn binary_files_are_refused_with_an_explanation() {
        let (tool, dir) = tool_in("binary");
        // PNG magic bytes + a NUL run.
        std::fs::write(
            dir.join("img.png"),
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0],
        )
        .unwrap();
        let out = tool
            .call(json!({ "path": "img.png" }), &detached_ctx("cli:t"))
            .await
            .unwrap();
        assert!(out.text.contains("not text"), "{}", out.text);
        assert!(!out.text.contains("│"), "no garbage page: {}", out.text);
    }

    /// A file with no known extension and a NUL byte is still binary.
    #[tokio::test]
    async fn nul_bytes_make_a_file_binary_without_an_extension_hint() {
        let (tool, dir) = tool_in("nul");
        std::fs::write(dir.join("blob"), b"text\0more").unwrap();
        let out = tool
            .call(json!({ "path": "blob" }), &detached_ctx("cli:t"))
            .await
            .unwrap();
        assert!(out.text.contains("not text"), "{}", out.text);
    }

    #[tokio::test]
    async fn invalid_utf8_is_reported_not_lossily_decoded() {
        let (tool, dir) = tool_in("utf8");
        // Lone continuation bytes: not valid UTF-8, but no NULs and printable
        // enough to get past the binary heuristic.
        std::fs::write(dir.join("bad.txt"), b"caf\xe9 latte, tr\xe8s bon").unwrap();
        let err = tool
            .call(json!({ "path": "bad.txt" }), &detached_ctx("cli:t"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");
    }

    /// The rendered page is for a reader; a `run_code` program computes on the
    /// structured view instead, so the page's own lines have to be in it —
    /// without the header, the gutter, or the per-line clipping, all of which
    /// are layout.
    #[tokio::test]
    async fn the_structured_view_carries_the_page_as_data() {
        let (tool, dir) = tool_in("structured");
        std::fs::write(dir.join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let out = tool
            .call(
                json!({ "path": "f.txt", "offset": 2, "limit": 2 }),
                &detached_ctx("cli:t"),
            )
            .await
            .unwrap();

        assert_eq!(out.structured["text"], "two\nthree");
        assert_eq!(out.structured["total_lines"], 4);
        // The text the model sees still carries the layout.
        assert!(out.text.contains("2│two"), "{}", out.text);
    }

    #[tokio::test]
    async fn overlong_lines_are_truncated_not_dropped() {
        let (tool, dir) = tool_in("longline");
        std::fs::write(dir.join("min.js"), "x".repeat(5000)).unwrap();
        let out = tool
            .call(json!({ "path": "min.js" }), &detached_ctx("cli:t"))
            .await
            .unwrap();
        assert!(out.text.contains("line truncated"), "{}", out.text);
        assert!(out.text.len() < 5000);
    }

    #[tokio::test]
    async fn paths_outside_the_workspace_are_denied() {
        let (tool, _dir) = tool_in("escape");
        let err = tool
            .call(json!({ "path": "/etc/passwd" }), &detached_ctx("cli:t"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)));
    }

    #[tokio::test]
    async fn a_policy_deny_blocks_the_read_without_leaking_content() {
        struct DenyReads;
        #[async_trait::async_trait]
        impl komo_core::domain::approval::Approver for DenyReads {
            async fn decide(
                &self,
                _r: &komo_core::domain::approval::ApprovalRequest,
            ) -> komo_core::domain::approval::Decision {
                komo_core::domain::approval::Decision::deny_because("secrets are off limits")
            }
        }
        let (tool, dir) = tool_in("denied");
        std::fs::write(dir.join("s.txt"), "TOP-SECRET").unwrap();
        let ctx = komo_core::domain::context::ToolContext::new(
            komo_core::domain::context::SessionContext::detached("cli:t"),
            None,
            Arc::new(DenyReads),
        );
        let out = tool.call(json!({ "path": "s.txt" }), &ctx).await.unwrap();
        assert!(out.text.contains("secrets are off limits"), "{}", out.text);
        assert!(!out.text.contains("TOP-SECRET"));
    }

    #[tokio::test]
    async fn an_empty_file_says_so_rather_than_reporting_a_bad_offset() {
        let (tool, dir) = tool_in("empty");
        std::fs::write(dir.join("e.txt"), "").unwrap();
        let out = tool
            .call(json!({ "path": "e.txt" }), &detached_ctx("cli:t"))
            .await
            .unwrap();
        assert!(out.text.contains("is empty"), "{}", out.text);
    }

    #[test]
    fn char_truncation_never_splits_a_codepoint() {
        let (out, clipped) = truncate_chars("日本語テキスト", 3);
        assert_eq!(out, "日本語");
        assert!(clipped);
    }
}
