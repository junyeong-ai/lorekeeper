//! Replace the body of a `## <heading>` section in a markdown page.
//!
//! Used by structural maintenance commands (e.g. `lore graph backlinks-sync`) that
//! need to rewrite one section of a page in place without touching others. The
//! "body" of a section is everything between its `## <heading>` line and the next
//! `## ` heading (or end of file).

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
    let target = format!("## {heading}");
    let lines: Vec<&str> = content.split_inclusive('\n').collect();

    // Walk lines while tracking fenced-code state (``` or ~~~ blocks). Headings
    // that appear inside a fenced block are quoted code, not document structure,
    // and must not be treated as section boundaries.
    let mut in_fence = false;
    let mut start_idx = None;
    for (i, line) in lines.iter().enumerate() {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let heading_line = stripped.trim_end();
        let trimmed = stripped.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && heading_line == target {
            start_idx = Some(i);
            break;
        }
    }
    let Some(start_idx) = start_idx else {
        return content.to_string();
    };

    // End of section = the next live H2 (a "## " line outside any fenced block).
    // End of file counts as a section terminator too.
    let mut in_fence = false;
    let mut end_idx = lines.len();
    for (rel, line) in lines[start_idx + 1..].iter().enumerate() {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let heading_line = stripped.trim_end();
        let trimmed = stripped.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && heading_line.starts_with("## ") {
            end_idx = rel + start_idx + 1;
            break;
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn idempotent_when_body_is_already_correct() {
        let doc = "## Sources\n\n- [[a]]\n- [[b]]\n\n## Meta\n";
        let out = replace_section(doc, "Sources", "- [[a]]\n- [[b]]");
        assert_eq!(out, doc);
    }
}
