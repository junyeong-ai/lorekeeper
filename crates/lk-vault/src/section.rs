//! Operate on the body of a `## <heading>` section in a markdown page.
//!
//! Used by structural maintenance commands (e.g. `lore graph backlinks-sync`) that
//! need to rewrite one section of a page in place without touching others, and by
//! the pipeline's two-phase render that preserves LLM-filled sections across
//! re-ingests. The "body" of a section is everything between its `## <heading>`
//! line and the next `## ` heading (or end of file).
//!
//! Both operations (`replace_section`, `section_body`)
//! share `find_section`, which tracks fenced-code state via
//! `lk_core::markdown::FenceState` — a `## ` line inside an open fence is quoted
//! content, not document structure. Heading lines are trimmed of trailing
//! whitespace before matching.

use lk_core::markdown::FenceState;

/// Locate the H2 section whose heading line is exactly `## {heading}`. Returns
/// `(heading_line_idx, section_end_idx)` over the line array — `heading_line_idx`
/// points at the `## {heading}` line itself; `section_end_idx` is the first line
/// outside the section (the next `## ` heading or `lines.len()`).
///
/// Tracks fenced-code state so a `## ` line inside a code fence (e.g. a quoted
/// four-backtick block containing a three-backtick snippet) is not mistaken for a
/// section boundary.
fn find_section(lines: &[&str], heading: &str) -> Option<(usize, usize)> {
    let target = format!("## {heading}");
    let mut fence = FenceState::new();

    let mut start_idx = None;
    for (i, line) in lines.iter().enumerate() {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        if fence.apply(stripped) {
            continue;
        }
        if fence.is_closed() && stripped.trim_end() == target {
            start_idx = Some(i);
            break;
        }
    }
    let start_idx = start_idx?;

    let mut end_idx = lines.len();
    for (rel, line) in lines[start_idx + 1..].iter().enumerate() {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        if fence.apply(stripped) {
            continue;
        }
        if fence.is_closed() && stripped.trim_end().starts_with("## ") {
            end_idx = rel + start_idx + 1;
            break;
        }
    }

    Some((start_idx, end_idx))
}

/// Replace the body of the H2 section whose heading line is exactly `## {heading}`
/// with `new_body`. The heading line itself, and the rest of the document around
/// the section, are preserved verbatim.
///
/// Layout produced: `## {heading}\n\n{new_body}\n\n## {next}` — one blank line
/// between the heading and the body, and one blank line before the following H2
/// (or trailing newline at EOF). `new_body` must not contain its own leading or
/// trailing blank lines; this function owns the separators.
///
/// If no line matches `## {heading}`, the document is returned unchanged. This is
/// the documented contract so callers can run the function unconditionally.
pub fn replace_section(content: &str, heading: &str, new_body: &str) -> String {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let Some((start_idx, end_idx)) = find_section(&lines, heading) else {
        return content.to_string();
    };

    let mut out = String::with_capacity(content.len());
    for line in &lines[..=start_idx] {
        out.push_str(line);
    }

    // Body block: blank line, body text, blank line before the next H2. If we're
    // at EOF (`end_idx == lines.len()`) the trailing blank would create a phantom
    // empty line — collapse it to a single final newline instead.
    out.push('\n');
    if !new_body.is_empty() {
        out.push_str(new_body);
        if !new_body.ends_with('\n') {
            out.push('\n');
        }
    }
    if end_idx < lines.len() {
        out.push('\n');
    }

    for line in &lines[end_idx..] {
        out.push_str(line);
    }

    out
}

/// Read the body of the H2 section whose heading line is exactly `## {heading}`.
/// Returns the substring between the heading line and the next `## ` (or EOF),
/// or `None` if the heading is missing. The body includes leading/trailing
/// whitespace verbatim — callers usually `.trim()`.
pub fn section_body<'a>(content: &'a str, heading: &str) -> Option<&'a str> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let (start_idx, end_idx) = find_section(&lines, heading)?;

    // Slice indices into the original string by accumulating line lengths. The
    // body starts after the heading line and ends at end_idx's line start.
    let body_start: usize = lines[..=start_idx].iter().map(|l| l.len()).sum();
    let body_end: usize = lines[..end_idx].iter().map(|l| l.len()).sum();
    Some(&content[body_start..body_end])
}

/// Set a scalar `key: value` inside the frontmatter block ONLY (between the first two
/// `---` lines). Matches only a TOP-LEVEL (column-0) key, so a `key:` in body prose, a
/// code fence, or nested under a mapping (e.g. `summary:` under `llm_inputs:`) is never
/// touched. If the field is absent it is inserted just before the closing `---`, so a
/// later run sees it and the operation is idempotent. A page with no frontmatter block is
/// returned unchanged. A leading BOM is recognized (mirroring `parse_page`) and preserved.
/// The single source of truth for setting a frontmatter scalar — used by `backlinks-sync`
/// (`source_count`) and the audit marker (`audited_sources_hash`).
pub fn set_frontmatter_field(content: &str, key: &str, value: &str) -> String {
    // Recognize a leading BOM the way `parse_page` does, then restore it so the page
    // round-trips byte-faithfully.
    let (bom, body) = content
        .strip_prefix('\u{feff}')
        .map_or(("", content), |rest| ("\u{feff}", rest));
    let mut out = String::with_capacity(content.len());
    out.push_str(bom);
    let mut in_frontmatter = false;
    let mut seen_open = false;
    let mut done = false;
    let mut first_line = true;
    let field_prefix = format!("{key}:");
    for line in body.split_inclusive('\n') {
        let is_fence = line.strip_suffix('\n').unwrap_or(line).trim_end() == "---";
        // The opening fence must be the document's FIRST line — same rule as
        // `frontmatter::parse_page` — so a body thematic break (`---`) in a page with no
        // frontmatter is never mistaken for a frontmatter opener.
        let was_first = first_line;
        first_line = false;
        if is_fence && !seen_open && was_first {
            seen_open = true;
            in_frontmatter = true;
            out.push_str(line);
            continue;
        }
        if is_fence && in_frontmatter {
            if !done {
                out.push_str(key);
                out.push_str(": ");
                out.push_str(value);
                out.push('\n');
                done = true;
            }
            in_frontmatter = false;
            out.push_str(line);
            continue;
        }
        // Match ONLY a top-level key (no leading whitespace). An indented key nested under
        // a mapping must be left alone — rewriting it would orphan the real top-level field
        // and break idempotency.
        if in_frontmatter && !done && line.starts_with(&field_prefix) {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(value);
            if line.ends_with('\n') {
                out.push('\n');
            }
            done = true;
        } else {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// Test-only mirror of the pipeline's "is this section filled?" check
    /// (`llm_cache` owns the production form). Kept here so the fence/heading
    /// boundary cases below stay expressed in terms of a filled-section predicate.
    fn section_is_filled(content: &str, heading: &str) -> bool {
        super::section_body(content, heading).is_some_and(|b| !b.trim().is_empty())
    }

    use super::*;

    #[test]
    fn set_frontmatter_field_inserts_when_absent() {
        let doc = "---\nid: x\n---\n\n# X\n";
        let out = set_frontmatter_field(doc, "source_count", "3");
        assert_eq!(out, "---\nid: x\nsource_count: 3\n---\n\n# X\n");
    }

    #[test]
    fn set_frontmatter_field_replaces_when_present() {
        let doc = "---\nid: x\nsource_count: 1\n---\n\n# X\n";
        let out = set_frontmatter_field(doc, "source_count", "9");
        assert_eq!(out, "---\nid: x\nsource_count: 9\n---\n\n# X\n");
    }

    #[test]
    fn set_frontmatter_field_is_idempotent() {
        let doc = "---\nid: x\n---\n\n# X\n";
        let once = set_frontmatter_field(doc, "h", "abc");
        let twice = set_frontmatter_field(&once, "h", "abc");
        assert_eq!(once, twice);
    }

    #[test]
    fn set_frontmatter_field_ignores_body_occurrences() {
        // A `source_count:` in the BODY (prose or a code fence) must never be mutated —
        // only the frontmatter block between the first two `---` lines is touched.
        let doc = "---\nid: x\nsource_count: 1\n---\n\n# X\n\n```\nsource_count: 999\n```\n";
        let out = set_frontmatter_field(doc, "source_count", "2");
        assert_eq!(
            out,
            "---\nid: x\nsource_count: 2\n---\n\n# X\n\n```\nsource_count: 999\n```\n"
        );
    }

    #[test]
    fn set_frontmatter_field_no_block_is_unchanged() {
        // First line is not a `---` fence → no frontmatter; a later body `---` thematic
        // break must not be mistaken for a frontmatter opener.
        let doc = "# X\n\nbody\n\n---\n\nmore\n";
        assert_eq!(set_frontmatter_field(doc, "k", "v"), doc);
    }

    #[test]
    fn set_frontmatter_field_ignores_nested_keys() {
        // An indented key nested under a mapping must NOT be matched; the top-level field
        // is inserted independently (keeps the op idempotent on pages with nested YAML).
        let doc = "---\nllm_inputs:\n  source_count: 5\n---\n";
        let out = set_frontmatter_field(doc, "source_count", "2");
        assert_eq!(
            out,
            "---\nllm_inputs:\n  source_count: 5\nsource_count: 2\n---\n"
        );
    }

    #[test]
    fn set_frontmatter_field_recognizes_and_preserves_bom() {
        let doc = "\u{feff}---\nid: x\n---\n";
        let out = set_frontmatter_field(doc, "source_count", "2");
        assert_eq!(out, "\u{feff}---\nid: x\nsource_count: 2\n---\n");
    }

    #[test]
    fn set_frontmatter_field_does_not_false_match_key_prefix() {
        // Setting `source_count` must not touch a different key that merely shares a
        // prefix — the trailing `:` anchors the match.
        let doc = "---\nsource_count_extra: keep\nsource_count: 1\n---\n";
        let out = set_frontmatter_field(doc, "source_count", "5");
        assert_eq!(out, "---\nsource_count_extra: keep\nsource_count: 5\n---\n");
    }

    #[test]
    fn replaces_body_between_headings() {
        let doc = "# Title\n\n## Sources\n\n- [[old]]\n\n## Meta\n\n- key: value\n";
        let out = replace_section(doc, "Sources", "- [[new1]]\n- [[new2]]");
        assert_eq!(
            out,
            "# Title\n\n## Sources\n\n- [[new1]]\n- [[new2]]\n\n## Meta\n\n- key: value\n"
        );
    }

    #[test]
    fn matches_headings_with_trailing_spaces() {
        let doc = "# Title\n\n## Sources  \n\n- [[old]]\n\n## Meta   \n\n- key: value\n";
        let out = replace_section(doc, "Sources", "- [[new]]");
        assert_eq!(
            out,
            "# Title\n\n## Sources  \n\n- [[new]]\n\n## Meta   \n\n- key: value\n"
        );
    }

    #[test]
    fn replaces_body_at_end_of_file() {
        let doc = "# Title\n\n## Sources\n\n- [[old]]\n";
        let out = replace_section(doc, "Sources", "- [[new]]");
        assert_eq!(out, "# Title\n\n## Sources\n\n- [[new]]\n");
    }

    #[test]
    fn empty_body_produces_blank_section() {
        let doc = "## Sources\n\n- [[old]]\n\n## Meta\n";
        let out = replace_section(doc, "Sources", "");
        assert_eq!(out, "## Sources\n\n\n## Meta\n");
    }

    #[test]
    fn missing_heading_returns_unchanged() {
        let doc = "# Title\n\n## Other\n\n- a\n";
        let out = replace_section(doc, "Sources", "- [[new]]");
        assert_eq!(out, doc);
    }

    #[test]
    fn does_not_match_partial_heading_prefix() {
        // `## Sourcesy` must not be treated as `## Sources`.
        let doc = "## Sourcesy\n\nbody\n\n## Sources\n\n- [[old]]\n";
        let out = replace_section(doc, "Sources", "- [[new]]");
        assert!(out.contains("## Sourcesy\n\nbody\n"));
        assert!(out.contains("- [[new]]"));
        assert!(!out.contains("- [[old]]"));
    }

    #[test]
    fn does_not_match_h3() {
        // `### Sources` is a different heading level and must be ignored.
        let doc = "### Sources\n\n- [[keep]]\n";
        let out = replace_section(doc, "Sources", "- [[new]]");
        assert_eq!(out, doc);
    }

    #[test]
    fn preserves_frontmatter_and_other_sections() {
        let doc = "---\nid: x\n---\n\n# Title\n\n## Summary\n\nsumtext\n\n## Sources\n\n- [[a]]\n\n## Meta\n\nmeta\n";
        let out = replace_section(doc, "Sources", "- [[b]]\n- [[c]]");
        assert!(out.starts_with("---\nid: x\n---\n"));
        assert!(out.contains("## Summary\n\nsumtext\n"));
        assert!(out.contains("## Sources\n\n- [[b]]\n- [[c]]\n\n## Meta\n"));
    }

    #[test]
    fn code_fence_inside_section_body_does_not_end_section_early() {
        // A `## Sources` line inside a code fence must NOT terminate the section —
        // the real boundary is `## Meta` after the fence closes. The entire old
        // body (including the code fence) is replaced.
        let doc = "## Sources\n\n- [[old]]\n\n```\n## Sources (quoted)\n```\n\n## Meta\n";
        let out = replace_section(doc, "Sources", "- [[new]]");
        assert_eq!(out, "## Sources\n\n- [[new]]\n\n## Meta\n");
    }

    #[test]
    fn skips_target_heading_inside_fenced_code_block() {
        // The FIRST `## Sources` appears inside a fence — it must be skipped and
        // the second (real) one used.
        let doc = "```\n## Sources\nquoted\n```\n\n## Sources\n\n- [[old]]\n";
        let out = replace_section(doc, "Sources", "- [[new]]");
        assert_eq!(
            out,
            "```\n## Sources\nquoted\n```\n\n## Sources\n\n- [[new]]\n"
        );
    }

    #[test]
    fn setext_underline_in_body_is_not_a_section_boundary() {
        // A source body can carry setext headings (`Subhead\n---`). Section boundaries
        // are ATX `## ` lines only, so a setext underline is ordinary body content —
        // the real boundary is `## Meta`. The whole body is replaced as one span.
        let doc = "## Key Events\n\nSubhead\n---\n\nbody\n\n## Meta\n\nm\n";
        let out = replace_section(doc, "Key Events", "rewritten");
        assert_eq!(out, "## Key Events\n\nrewritten\n\n## Meta\n\nm\n");
    }

    #[test]
    fn idempotent_when_body_is_already_correct() {
        let doc = "## Sources\n\n- [[a]]\n- [[b]]\n\n## Meta\n";
        let out = replace_section(doc, "Sources", "- [[a]]\n- [[b]]");
        assert_eq!(out, doc);
    }

    #[test]
    fn is_filled_true_for_real_content() {
        let doc = "## Summary\n\nReal content here.\n\n## Meta\n";
        assert!(section_is_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_false_for_missing_heading() {
        let doc = "## Other\n\nbody\n";
        assert!(!section_is_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_false_for_empty_body() {
        let doc = "## Summary\n\n## Meta\n";
        assert!(!section_is_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_false_for_whitespace_only_body() {
        let doc = "## Summary\n\n   \n\t\n\n## Meta\n";
        assert!(!section_is_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_false_when_heading_is_inside_fenced_code() {
        // The only `## Summary` is quoted code — there is no real section.
        let doc = "```\n## Summary\nquoted body\n```\n";
        assert!(!section_is_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_true_when_real_heading_follows_a_quoted_one() {
        let doc = "```\n## Summary\nquoted\n```\n\n## Summary\n\nreal body\n";
        assert!(section_is_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_true_at_end_of_file() {
        let doc = "# Title\n\n## Summary\n\nbody at EOF\n";
        assert!(section_is_filled(doc, "Summary"));
    }

    #[test]
    fn body_returns_text_between_heading_and_next_h2() {
        let doc = "# Title\n\n## Summary\n\nfirst line\nsecond line\n\n## Meta\n\n- k: v\n";
        assert_eq!(
            section_body(doc, "Summary"),
            Some("\nfirst line\nsecond line\n\n")
        );
    }

    #[test]
    fn outer_quad_fence_with_inner_triple_fence_does_not_desync() {
        // Four-backtick OPEN, three-backtick line inside is content (not a closing
        // fence), four-backtick CLOSE. The `## Meta` after the outer fence is a real
        // section boundary; the `## Inner` line inside the quad fence must NOT be
        // recognized as the section start.
        let doc =
            "## Sources\n\n````\n## Inner\n```\nnested\n```\n````\n\n## Meta\n\n- key: value\n";
        let out = replace_section(doc, "Sources", "- [[new]]");
        assert_eq!(
            out, "## Sources\n\n- [[new]]\n\n## Meta\n\n- key: value\n",
            "outer quad fence content must be replaced wholesale, not split by inner triple"
        );
        // And `## Inner` must not be findable as a real section.
        assert!(!section_is_filled(doc, "Inner"));
    }

    #[test]
    fn tilde_fence_inside_backtick_fence_does_not_toggle() {
        // Mismatched marker characters must not close the open fence.
        let doc = "## Sources\n\n```\n~~~\n## Trap\n~~~\n```\n\n## Meta\n\n- v\n";
        let out = replace_section(doc, "Sources", "- [[new]]");
        assert!(out.contains("## Sources\n\n- [[new]]\n\n## Meta\n"));
        assert!(!section_is_filled(doc, "Trap"));
    }

    #[test]
    fn backtick_line_with_inline_backtick_is_not_a_fence() {
        // A backtick-fence info string can't contain a backtick (CommonMark), so this
        // line never opens a fence — and the `## B` after it stays a real section
        // boundary rather than being swallowed as fenced content.
        let doc = "## A\n\nintro\n\n```not`a`fence\n\n## B\n\nbody B\n";
        assert!(
            section_is_filled(doc, "B"),
            "## B must be a real section, not swallowed by a fake fence"
        );
        // Replacing A leaves B intact, proving the boundary held.
        let out = replace_section(doc, "A", "new A");
        assert!(out.contains("## A\n\nnew A\n\n## B\n\nbody B\n"));
    }

    #[test]
    fn closing_fence_with_info_string_does_not_close() {
        // CommonMark says the closing fence must have no info string. A line that
        // looks like a fence but carries text after the markers is content, not a
        // close — so a `## Heading` further down should still be inside the fence.
        let doc = "## Sources\n\n```\nopen\n``` info string\n## Not a heading\n```\n\n## Meta\n";
        let out = replace_section(doc, "Sources", "- [[new]]");
        assert!(out.contains("## Sources\n\n- [[new]]\n\n## Meta\n"));
        assert!(!section_is_filled(doc, "Not a heading"));
    }

    #[test]
    fn body_returns_none_for_missing_heading() {
        let doc = "## Other\n\nbody\n";
        assert!(section_body(doc, "Summary").is_none());
    }
}
