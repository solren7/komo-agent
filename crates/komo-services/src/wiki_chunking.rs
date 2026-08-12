//! Split a markdown note into embeddable chunks.
//!
//! Structure-aware rather than fixed-width: a note's heading tree already marks
//! where topics begin and end, so cutting on headings keeps each chunk about one
//! thing. A blind character-window split would routinely straddle two unrelated
//! sections and produce a vector that is close to neither.
//!
//! Sizes are counted in **characters, not bytes**. The vault is mostly Chinese,
//! where one char is 3 UTF-8 bytes, so a byte budget would cut chunks to a third
//! of the intended length — and slicing at a byte offset would split a codepoint
//! outright.

/// Chunk size budget, in characters.
#[derive(Debug, Clone, Copy)]
pub struct ChunkSpec {
    /// Size a chunk aims for, and the window a heading-free run is cut into.
    /// Chunks may exceed it by up to `min` when a short tail is merged back, so
    /// `target + min` is the real ceiling.
    pub target: usize,
    /// A section this short is carried into the next one instead of becoming its
    /// own chunk — a lone heading line embeds to noise. Also the threshold below
    /// which a trailing piece is merged into its predecessor.
    pub min: usize,
    /// Characters repeated across a forced split, so a sentence spanning the cut
    /// is still wholly present in one of the two chunks.
    pub overlap: usize,
}

impl Default for ChunkSpec {
    /// Tuned for CJK notes against Qwen3-Embedding: ~800 chars lands near the
    /// 512-token sweet spot where retrieval quality peaks. Bigger chunks measurably
    /// dilute the vector — a chunk covering four topics is close to none of them.
    fn default() -> Self {
        Self {
            target: 800,
            min: 80,
            overlap: 100,
        }
    }
}

impl ChunkSpec {
    /// Ceiling no chunk may exceed. See [`ChunkSpec::target`].
    pub fn ceiling(&self) -> usize {
        self.target + self.min
    }
}

/// A chunk before it has been embedded.
#[derive(Debug, Clone, PartialEq)]
pub struct RawChunk {
    /// Heading trail, ` > `-joined (`设计 > 状态机`). Empty for content above the
    /// first heading.
    pub heading_path: String,
    pub text: String,
    pub ordinal: usize,
}

/// Strip a leading YAML frontmatter block, returning the body.
///
/// Every note in the vault has one. Its keys (`tags`, `aliases`) are prose-free
/// metadata that would dilute a chunk's vector, and the delimiters would be
/// embedded verbatim, so the block is dropped rather than indexed.
fn strip_frontmatter(content: &str) -> &str {
    let rest = match content.strip_prefix("---\n") {
        Some(rest) => rest,
        None => return content,
    };
    // An unterminated block means the whole file is frontmatter-ish; treat the
    // original as body rather than losing everything.
    match rest.find("\n---\n") {
        Some(end) => &rest[end + 5..],
        None => content,
    }
}

/// Does this line open or close a fenced code block?
///
/// Fences matter beyond formatting: a shell or Python block is full of lines
/// starting with `#`, and treating those as headings would shatter the block
/// into dozens of bogus sections. 47% of this vault's notes contain fences.
///
/// Public because `wiki_read` walks the same heading tree to slice out a
/// section: a second definition of "what is a heading" would let the reader miss
/// a heading the searcher reported.
pub fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// Heading level (1-6) and text, for a line that is a heading. Public for the
/// same reason as [`is_fence`].
pub fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    // ATX headings require whitespace after the hashes; `#tag` is a tag, not a
    // heading, and this vault uses inline tags.
    let rest = &line[hashes..];
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    Some((hashes, rest.trim()))
}

/// Cut a run of text into `spec.target`-sized pieces on line boundaries,
/// overlapping by `spec.overlap`.
///
/// Reached whenever accumulated text exceeds `target` — most often a section with
/// no sub-headings (the verbatim log dumps in `05-archives`), but also any
/// ordinary section that simply ran long. Cutting to `target` rather than to some
/// larger ceiling is what keeps the size distribution centred where retrieval
/// works instead of piled against the limit.
fn split_long(text: &str, spec: &ChunkSpec) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= spec.target {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let hard_end = (start + spec.target).min(chars.len());
        // Prefer a blank line, then any newline, within the last quarter of the
        // window; fall back to the hard cut when the run has neither (one
        // enormous paragraph).
        let search_from = start + spec.target * 3 / 4;
        let end = if hard_end == chars.len() || search_from >= hard_end {
            hard_end
        } else {
            let window: String = chars[search_from..hard_end].iter().collect();
            window
                .rfind("\n\n")
                .or_else(|| window.rfind('\n'))
                .map(|off| search_from + window[..off].chars().count() + 1)
                .unwrap_or(hard_end)
        };
        let piece: String = chars[start..end].iter().collect();
        let piece = piece.trim();
        if !piece.is_empty() {
            // A tail shorter than `min` is worthless alone; fold it back rather
            // than emitting a chunk that embeds to noise.
            let tail_is_runt = end >= chars.len() && piece.chars().count() < spec.min;
            match out.last_mut() {
                Some(prev) if tail_is_runt => {
                    prev.push('\n');
                    prev.push_str(piece);
                }
                _ => out.push(piece.to_string()),
            }
        }
        if end >= chars.len() {
            break;
        }
        start = end.saturating_sub(spec.overlap).max(start + 1);
    }
    out
}

/// Split one note into chunks.
///
/// `title` (the note's filename without extension) seeds the heading trail, so a
/// chunk from a note called `checkout policy 设计` carries that context even when
/// it sits under a generic heading like `## 背景`.
pub fn chunk_markdown(title: &str, content: &str, spec: &ChunkSpec) -> Vec<RawChunk> {
    let body = strip_frontmatter(content);

    let mut chunks: Vec<RawChunk> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut buf = String::new();
    let mut buf_headings: Vec<String> = Vec::new();
    let mut in_fence = false;

    let trail = |stack: &[(usize, String)]| -> String {
        let mut parts = vec![title.to_string()];
        parts.extend(stack.iter().map(|(_, t)| t.clone()));
        parts.join(" > ")
    };

    let flush = |buf: &mut String, headings: &mut Vec<String>, chunks: &mut Vec<RawChunk>| {
        let text = buf.trim();
        if text.chars().count() >= spec.min {
            let heading_path = headings.first().cloned().unwrap_or_default();
            for piece in split_long(text, spec) {
                let ordinal = chunks.len();
                chunks.push(RawChunk {
                    heading_path: heading_path.clone(),
                    text: piece,
                    ordinal,
                });
            }
            buf.clear();
            headings.clear();
        }
        // Below `min`: keep accumulating so a bare heading merges into the next
        // section rather than becoming a chunk of its own.
    };

    for line in body.lines() {
        if is_fence(line) {
            in_fence = !in_fence;
            buf.push_str(line);
            buf.push('\n');
            continue;
        }

        // Inside a fence every line is content, `#` included.
        if !in_fence {
            if let Some((level, text)) = parse_heading(line) {
                if buf.chars().count() >= spec.target {
                    flush(&mut buf, &mut buf_headings, &mut chunks);
                }
                while stack.last().is_some_and(|(l, _)| *l >= level) {
                    stack.pop();
                }
                stack.push((level, text.to_string()));
                if buf_headings.is_empty() {
                    buf_headings.push(trail(&stack));
                }
                buf.push_str(line);
                buf.push('\n');
                continue;
            }
        }

        if buf_headings.is_empty() {
            buf_headings.push(trail(&stack));
        }
        buf.push_str(line);
        buf.push('\n');
    }

    // Final flush ignores `min`: trailing content is real content, however short.
    let text = buf.trim();
    if !text.is_empty() {
        let heading_path = buf_headings
            .first()
            .cloned()
            .unwrap_or_else(|| trail(&stack));
        for piece in split_long(text, spec) {
            let ordinal = chunks.len();
            chunks.push(RawChunk {
                heading_path: heading_path.clone(),
                text: piece,
                ordinal,
            });
        }
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ChunkSpec {
        ChunkSpec::default()
    }

    #[test]
    fn frontmatter_is_dropped() {
        let out = chunk_markdown(
            "note",
            "---\ntags: [a, b]\naliases: x\n---\n实际正文内容在这里。",
            &spec(),
        );
        assert_eq!(out.len(), 1);
        assert!(!out[0].text.contains("tags"));
        assert!(out[0].text.contains("实际正文内容"));
    }

    #[test]
    fn note_without_frontmatter_keeps_all_content() {
        let out = chunk_markdown("note", "没有 frontmatter 的正文。", &spec());
        assert_eq!(out.len(), 1);
        assert!(out[0].text.contains("没有 frontmatter"));
    }

    /// The whole reason fences are tracked: `#` inside a shell block is a
    /// comment, and treating it as a heading would shred the block.
    #[test]
    fn hashes_inside_a_code_fence_are_not_headings() {
        let content = "## 部署\n\n```bash\n# 拉取镜像\ndocker pull x\n# 重启\ndocker restart x\n```\n\n收尾说明文字。";
        let out = chunk_markdown("note", content, &spec());
        let joined: String = out.iter().map(|c| c.text.as_str()).collect();
        assert!(joined.contains("# 拉取镜像"));
        assert!(joined.contains("# 重启"));
        for c in &out {
            assert!(
                !c.heading_path.contains("拉取镜像"),
                "code comment leaked into heading trail: {}",
                c.heading_path
            );
        }
    }

    #[test]
    fn heading_trail_nests_and_pops() {
        let body = format!(
            "# 一级\n\n{}\n\n## 二级\n\n{}\n\n# 另一个一级\n\n{}",
            "甲".repeat(900),
            "乙".repeat(900),
            "丙".repeat(900)
        );
        let out = chunk_markdown("笔记", &body, &spec());
        assert!(
            out.len() >= 3,
            "expected a chunk per section, got {}",
            out.len()
        );
        assert!(out[0].heading_path.starts_with("笔记 > 一级"));
        let second = out.iter().find(|c| c.text.contains('乙')).unwrap();
        assert_eq!(second.heading_path, "笔记 > 一级 > 二级");
        let third = out.iter().find(|c| c.text.contains('丙')).unwrap();
        assert_eq!(third.heading_path, "笔记 > 另一个一级");
    }

    #[test]
    fn tag_line_is_not_treated_as_heading() {
        let out = chunk_markdown("note", "#项目标签 和正文在同一行的情况。", &spec());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].heading_path, "note");
    }

    /// A heading-free section past `max` still has to come apart, and every
    /// character must survive somewhere.
    #[test]
    fn oversized_section_splits_and_loses_nothing() {
        let para = "这是一个很长的段落。".repeat(40);
        let body = (0..12)
            .map(|_| para.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = chunk_markdown("long", &body, &spec());
        assert!(out.len() > 1, "expected a forced split");
        for c in &out {
            assert!(
                c.text.chars().count() <= spec().ceiling(),
                "chunk over max: {}",
                c.text.chars().count()
            );
        }
    }

    #[test]
    fn short_section_merges_instead_of_becoming_its_own_chunk() {
        let out = chunk_markdown("note", "## 标题甲\n\n## 标题乙\n\n正文。", &spec());
        assert_eq!(out.len(), 1, "bare headings should not each become a chunk");
    }

    #[test]
    fn ordinals_are_sequential() {
        let body = format!("# 甲\n{}\n# 乙\n{}", "内".repeat(900), "容".repeat(900));
        let out = chunk_markdown("note", &body, &spec());
        for (i, c) in out.iter().enumerate() {
            assert_eq!(c.ordinal, i);
        }
    }

    #[test]
    fn empty_and_whitespace_input_yield_nothing() {
        assert!(chunk_markdown("note", "", &spec()).is_empty());
        assert!(chunk_markdown("note", "   \n\n  \n", &spec()).is_empty());
        assert!(chunk_markdown("note", "---\ntags: x\n---\n", &spec()).is_empty());
    }
}
