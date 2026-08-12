//! Read one note from the vault, whole or by section.
//!
//! The second half of `search → read`. `wiki_search` returns isolated chunks:
//! enough to know a note answers the question, rarely enough to answer it. The
//! alternative — having search return neighbouring chunks — would make every
//! query pay the context cost of the few that need a whole section, so the
//! widening is a separate, explicit call instead.
//!
//! Reads the markdown on disk rather than the index, because the file is the
//! source of truth: a note edited since the last `wiki_index` run is served
//! current here even while search still matches the stale chunk.
//!
//! Confined to the vault by canonicalized prefix, so a `..` trail or a symlink
//! pointing out of the vault is refused on the resolved path, not on the string.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use komo_core::domain::{
    approval::{ActionRef, ApprovalRequest, Decision},
    context::ToolContext,
    tool::{Tool, ToolError, ToolOutput, parse_args},
};
use komo_services::wiki_chunking::{is_fence, parse_heading};
use serde::Deserialize;
use serde_json::{Value, json};

/// Character cap on returned content. A vault holds notes of 100 KB and more,
/// and a whole one would spend the turn's context on the 95% the question did
/// not ask about. Overflow is reported with the note's outline, so the next call
/// can name a section instead of a file.
const MAX_CHARS: usize = 12_000;

#[derive(Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    heading: Option<String>,
}

pub struct WikiReadTool {
    vault: PathBuf,
}

impl WikiReadTool {
    pub fn new(vault: PathBuf) -> Self {
        Self { vault }
    }

    /// Resolve a model-supplied path to a real file inside the vault.
    ///
    /// Relative paths anchor at the vault root (which is what `wiki_search`
    /// prints); an absolute one is accepted only if it resolves inside the
    /// vault. A missing `.md` is filled in — the model routinely passes a note's
    /// title.
    fn resolve(&self, raw: &str) -> Result<PathBuf, ToolError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ToolError::InvalidInput("`path` is empty".into()));
        }
        let vault = self.vault.canonicalize().map_err(|e| {
            ToolError::Failed(anyhow::anyhow!(
                "the note vault at {} is not readable: {e}",
                self.vault.display()
            ))
        })?;
        let candidate = Path::new(raw);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            vault.join(candidate)
        };
        // `.md` first: a vault directory can share a note's name, and the note is
        // what was asked for.
        let with_ext = joined.with_extension("md");
        let existing = [with_ext, joined]
            .into_iter()
            .find(|p| p.is_file())
            .ok_or_else(|| {
                ToolError::InvalidInput(format!(
                    "no note at `{raw}` in the vault. Paths are vault-relative, exactly as \
                     `wiki_search` reports them."
                ))
            })?;
        // Canonicalize before the prefix check so `..` and symlinks are judged on
        // where they actually land.
        let target = existing
            .canonicalize()
            .map_err(|e| ToolError::Failed(anyhow::anyhow!("resolving `{raw}` failed: {e}")))?;
        if !target.starts_with(&vault) {
            return Err(ToolError::Denied(format!(
                "`{raw}` resolves outside the note vault and was blocked."
            )));
        }
        Ok(target)
    }
}

#[async_trait]
impl Tool for WikiReadTool {
    fn name(&self) -> &'static str {
        "wiki_read"
    }

    fn description(&self) -> &'static str {
        "Read a note from the user's vault by path — the whole note, or one \
         section with `heading`. Use it after `wiki_search` when a matched \
         passage is not enough to answer: search returns isolated fragments, \
         this returns the section they came from. Pass `path` exactly as \
         `wiki_search` reported it, and `heading` as the last part of the \
         heading trail it printed. Also the right way to read a note the user \
         names outright (\"open my note on X\") once search has found its path."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Vault-relative path to the note, as reported by `wiki_search` (e.g. `02-projects/checkout.md`). A missing `.md` is added."
                },
                "heading": {
                    "type": "string",
                    "description": "Read only this section: the heading line and everything under it, up to the next heading of the same or higher level. Accepts a `A > B` trail (its last part is matched) or a bare heading. Omit for the whole note."
                }
            },
            "required": ["path"]
        })
    }

    /// Reading the vault only ever reads, so this consults the approver at
    /// `Risk::Safe`: a `deny wiki` rule fences it off, but it never prompts —
    /// same treatment as `read` and `wiki_index`'s `status`.
    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: Args = parse_args(&input)?;
        let request =
            ApprovalRequest::safe("Read a note from the vault").with_action(ActionRef::Wiki {
                action: "read".to_string(),
            });
        if let Decision::Deny { feedback } = ctx.decide(&request).await {
            return Err(ToolError::Denied(feedback.unwrap_or_else(|| {
                "reading the note vault is denied by policy".into()
            })));
        }

        let target = self.resolve(&args.path)?;
        let body = std::fs::read_to_string(&target)
            .map_err(|e| ToolError::Failed(anyhow::anyhow!("reading `{}`: {e}", args.path)))?;
        // Report the vault-relative path back, never the absolute one: it is what
        // the model must pass on the next call, and the vault root is the
        // operator's private layout.
        let shown = self
            .vault
            .canonicalize()
            .ok()
            .and_then(|v| {
                target
                    .strip_prefix(&v)
                    .ok()
                    .map(|p| p.display().to_string())
            })
            .unwrap_or_else(|| args.path.clone());

        let wanted = args
            .heading
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty());
        let (label, content) = match wanted {
            Some(heading) => match extract_section(&body, heading) {
                Some((trail, text)) => (format!("{shown} > {trail}"), text),
                None => {
                    return Err(ToolError::InvalidInput(format!(
                        "`{shown}` has no heading matching `{heading}`.{}",
                        describe_outline(&body)
                    )));
                }
            },
            None => (format!("{shown} (whole note)"), body.clone()),
        };

        let (content, overflowed) = cap(&content);
        let mut text = format!(
            "── {label} — {} chars\n\n{content}",
            content.chars().count()
        );
        if overflowed {
            text.push_str(&format!(
                "\n\n[truncated at {MAX_CHARS} chars. Re-read one section with `heading`.{}]",
                describe_outline(&body)
            ));
        }
        Ok(ToolOutput::text(text).with_title(label))
    }

    /// Nothing is mutated, so a transient failure is always safe to re-run.
    fn idempotent(&self) -> bool {
        true
    }
}

/// The heading to match: the last component of a ` > `-joined trail, since that
/// is the shape `wiki_search` prints and the model echoes back.
fn section_needle(wanted: &str) -> String {
    wanted
        .rsplit('>')
        .next()
        .unwrap_or(wanted)
        .trim()
        .trim_start_matches('#')
        .trim()
        .to_lowercase()
}

/// Slice out the section under `wanted`: its heading line plus everything up to
/// the next heading at the same or a higher level. Returns the full heading trail
/// alongside the text, so the caller can cite where in the note it came from.
///
/// Matched case-insensitively, exact first and by substring second: an exact
/// pass alone misses a heading the model retyped with different punctuation,
/// while a substring pass alone would let `设计` win over a literal `设计` further
/// down the file.
fn extract_section(body: &str, wanted: &str) -> Option<(String, String)> {
    let needle = section_needle(wanted);
    if needle.is_empty() {
        return None;
    }
    section_pass(body, &needle, true).or_else(|| section_pass(body, &needle, false))
}

fn section_pass(body: &str, needle: &str, exact: bool) -> Option<(String, String)> {
    let mut trail: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut started: Option<(usize, String)> = None;
    let mut out = String::new();
    for line in body.lines() {
        if is_fence(line) {
            in_fence = !in_fence;
        }
        let heading = if in_fence { None } else { parse_heading(line) };
        if let Some((level, text)) = heading {
            trail.truncate(level.saturating_sub(1));
            trail.push(text.to_string());
            match &started {
                Some((start_level, _)) => {
                    // A sibling or an uncle ends the section; a child belongs to it.
                    if level <= *start_level {
                        break;
                    }
                }
                None => {
                    let lower = text.to_lowercase();
                    let hit = if exact {
                        lower == *needle
                    } else {
                        lower.contains(needle)
                    };
                    if !hit {
                        continue;
                    }
                    started = Some((level, trail.join(" > ")));
                }
            }
        }
        if started.is_some() {
            out.push_str(line);
            out.push('\n');
        }
    }
    started.map(|(_, trail)| (trail, out.trim_end().to_string()))
}

/// The note's headings, as a hint appended to a miss or a truncation. Empty when
/// the note has none (nothing to suggest, so say nothing).
fn describe_outline(body: &str) -> String {
    let mut in_fence = false;
    let mut headings: Vec<String> = Vec::new();
    for line in body.lines() {
        if is_fence(line) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some((level, text)) = parse_heading(line) {
            headings.push(format!("{}{text}", "  ".repeat(level.saturating_sub(1))));
        }
    }
    if headings.is_empty() {
        return String::new();
    }
    // Bounded: an outline is a navigation aid, and a 200-heading note would
    // otherwise blow the same budget the cap exists to defend.
    const OUTLINE_CAP: usize = 40;
    let shown = headings.len().min(OUTLINE_CAP);
    let mut out = format!(" Headings:\n{}", headings[..shown].join("\n"));
    if headings.len() > shown {
        out.push_str(&format!("\n… {} more", headings.len() - shown));
    }
    out
}

/// Truncate to [`MAX_CHARS`] on a line boundary, reporting whether it happened.
fn cap(content: &str) -> (String, bool) {
    let trimmed = content.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return (trimmed.to_string(), false);
    }
    let head: String = trimmed.chars().take(MAX_CHARS).collect();
    let cut = match head.rfind('\n') {
        // Only back up to a line boundary when one is reasonably close, so a
        // note that is one enormous paragraph is not cut to nothing.
        Some(at) if at > MAX_CHARS / 2 => at,
        _ => head.len(),
    };
    (head[..cut].trim_end().to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_core::domain::approval::{Approver, Decision};
    use komo_core::domain::context::SessionContext;
    use std::sync::Arc;

    const NOTE: &str = "---\ntags: [x]\n---\n\
        # Checkout\n\
        intro line\n\n\
        ## 设计\n\
        design body\n\n\
        ### 状态机\n\
        state machine body\n\n\
        ## 验证\n\
        verification body\n";

    fn vault(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("02-projects")).unwrap();
        std::fs::write(dir.join("02-projects/checkout.md"), NOTE).unwrap();
        dir
    }

    fn ctx(allow: bool) -> ToolContext {
        struct Fixed(bool);
        #[async_trait]
        impl Approver for Fixed {
            async fn decide(&self, _: &komo_core::domain::approval::ApprovalRequest) -> Decision {
                if self.0 {
                    Decision::Allow
                } else {
                    Decision::deny()
                }
            }
        }
        ToolContext::new(
            SessionContext::detached("cli:test"),
            None,
            Arc::new(Fixed(allow)),
        )
    }

    #[tokio::test]
    async fn reads_a_whole_note_by_vault_relative_path() {
        let tool = WikiReadTool::new(vault("komo_wiki_read_whole"));
        let out = tool
            .call(json!({"path": "02-projects/checkout.md"}), &ctx(true))
            .await
            .unwrap();
        assert!(out.text.contains("design body"), "{}", out.text);
        assert!(out.text.contains("verification body"), "{}", out.text);
        assert!(out.text.contains("02-projects/checkout.md"), "{}", out.text);
    }

    /// The model routinely passes a note's name without its extension.
    #[tokio::test]
    async fn a_missing_md_extension_is_filled_in() {
        let tool = WikiReadTool::new(vault("komo_wiki_read_ext"));
        let out = tool
            .call(json!({"path": "02-projects/checkout"}), &ctx(true))
            .await
            .unwrap();
        assert!(out.text.contains("design body"), "{}", out.text);
    }

    /// The point of the tool: one section, not the file — and not the *next*
    /// section either.
    #[tokio::test]
    async fn a_heading_returns_that_section_and_its_children_only() {
        let tool = WikiReadTool::new(vault("komo_wiki_read_section"));
        let out = tool
            .call(
                json!({"path": "02-projects/checkout.md", "heading": "设计"}),
                &ctx(true),
            )
            .await
            .unwrap();
        assert!(out.text.contains("design body"), "{}", out.text);
        // A child heading is part of the section.
        assert!(out.text.contains("state machine body"), "{}", out.text);
        // A sibling ends it.
        assert!(!out.text.contains("verification body"), "{}", out.text);
        // The intro above the heading is not part of it.
        assert!(!out.text.contains("intro line"), "{}", out.text);
    }

    /// `wiki_search` prints a trail, so the model echoes one back.
    #[tokio::test]
    async fn a_heading_trail_matches_on_its_last_component() {
        let tool = WikiReadTool::new(vault("komo_wiki_read_trail"));
        let out = tool
            .call(
                json!({"path": "02-projects/checkout.md", "heading": "设计 > 状态机"}),
                &ctx(true),
            )
            .await
            .unwrap();
        assert!(out.text.contains("state machine body"), "{}", out.text);
        assert!(!out.text.contains("design body"), "{}", out.text);
        // The reported trail is the real one, not just the matched component.
        assert!(out.text.contains("设计 > 状态机"), "{}", out.text);
    }

    #[tokio::test]
    async fn an_unknown_heading_lists_the_real_ones() {
        let tool = WikiReadTool::new(vault("komo_wiki_read_miss"));
        let err = tool
            .call(
                json!({"path": "02-projects/checkout.md", "heading": "部署"}),
                &ctx(true),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{msg}");
        assert!(msg.contains("设计"), "{msg}");
        assert!(msg.contains("验证"), "{msg}");
    }

    /// Confinement is on the resolved path, so a traversal to a real file
    /// outside the vault is refused rather than served.
    #[tokio::test]
    async fn a_path_escaping_the_vault_is_denied() {
        let dir = vault("komo_wiki_read_escape");
        let outside = dir.parent().unwrap().join("komo_wiki_read_outside.md");
        std::fs::write(&outside, "secret").unwrap();
        let tool = WikiReadTool::new(dir);
        let err = tool
            .call(json!({"path": "../komo_wiki_read_outside.md"}), &ctx(true))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err}");
        assert!(!err.to_string().contains("secret"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_note_is_invalid_input_not_a_failure() {
        let tool = WikiReadTool::new(vault("komo_wiki_read_absent"));
        let err = tool
            .call(json!({"path": "nope/absent.md"}), &ctx(true))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");
    }

    /// Deny-only: policy can fence the vault off, and then nothing is read.
    #[tokio::test]
    async fn a_policy_deny_blocks_the_read() {
        let tool = WikiReadTool::new(vault("komo_wiki_read_denied"));
        let err = tool
            .call(json!({"path": "02-projects/checkout.md"}), &ctx(false))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err}");
        assert!(!err.to_string().contains("design body"), "{err}");
    }

    /// A heading inside a fenced code block is not a heading — the same rule the
    /// chunker follows, which is why both share one parser.
    #[test]
    fn headings_inside_code_fences_are_not_sections() {
        let body = "# Real\nbody\n\n```sh\n# not a heading\n```\n\n## Also real\ntail\n";
        let outline = describe_outline(body);
        assert!(outline.contains("Real"), "{outline}");
        assert!(outline.contains("Also real"), "{outline}");
        assert!(!outline.contains("not a heading"), "{outline}");
        assert!(extract_section(body, "not a heading").is_none());
    }

    #[test]
    fn overflowing_content_is_capped_on_a_line_boundary() {
        let long = "line of text\n".repeat(4_000);
        let (out, overflowed) = cap(&long);
        assert!(overflowed);
        assert!(out.chars().count() <= MAX_CHARS);
        assert!(out.ends_with("line of text"), "cut mid-line");
    }

    #[test]
    fn content_under_the_cap_is_untouched() {
        let (out, overflowed) = cap("short note\n");
        assert!(!overflowed);
        assert_eq!(out, "short note");
    }
}
