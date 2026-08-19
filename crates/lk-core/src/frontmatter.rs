use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Names of this system's PRIVATE machine-coordination frontmatter fields — keys invented
/// by the tooling with no meaning outside the vault — that cross a crate boundary (one
/// crate writes, another reads). A bare literal on either side would let an internal
/// refactor rename break the contract with no compile error, so they are single-sourced
/// here and referenced by symbol everywhere.
///
/// The criterion is *internal protocol crossing a crate boundary*, not mere reuse.
/// Standard published vault vocabulary (`created`, `updated`, `title`, `id`, `aliases` —
/// keys Obsidian and human editors also read) is read across crates too, yet stays a
/// plain literal on purpose: it is anchored to the external page format, never the target
/// of a silent internal rename, so a constant would add no protection — only noise.
pub mod field {
    /// A concept's incoming-citation count. Written by `graph backlinks-sync`; read by
    /// the ingest concept merge and `wiki index`.
    pub const SOURCE_COUNT: &str = "source_count";
    /// The map of LLM-task cache hashes that drives materialized-view completion
    /// detection. Written by the pipeline render/work-log/synthesis stages and by `graph
    /// backlinks-sync`; read by the pipeline's own `llm_cache` and, across the crate
    /// boundary, by `queue status` (lk-cli) to classify a pending task current/stale. Its
    /// inner per-kind keys are single-sourced by `lk_queue::TargetKind::llm_inputs_key`.
    pub const LLM_INPUTS: &str = "llm_inputs";
    /// The `llm_inputs` child key a concept page's synthesis is cached under. Named here
    /// rather than only in `TargetKind` because `graph backlinks-sync` writes it and cannot
    /// see that enum: a concept's synthesis is owed against its citation set, and the crate
    /// that derives that set is the one that records it.
    pub const SYNTHESIS: &str = "synthesis";

    /// The build that rendered a generated page, carried on the page itself.
    ///
    /// `<wiki>/AGENTS.md` is the contract every skill reads, and it is rendered from the
    /// binary's page-format table — so a binary that gained a format and did not rewrite it
    /// leaves the skills reading a contract that predates it. Nothing rejects that the way a
    /// vault rejects a broken link, so the page states which build wrote it and the
    /// installation check compares that rather than guessing from the prose.
    pub const GENERATOR: &str = "generator";

    /// The `llm_inputs.<key>_done` marker naming the input a section was written from.
    ///
    /// Completion is uniformly marker-signalled, never inferred from a body being non-empty,
    /// so every input key has exactly this companion. Derived in one place so a writer in
    /// one crate and a reader in another cannot spell it differently.
    pub fn completion(input_key: &str) -> String {
        format!("{input_key}_done")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frontmatter {
    #[serde(flatten)]
    pub fields: BTreeMap<String, serde_json::Value>,
}

impl Frontmatter {
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.fields.get(key)
    }

    /// The incoming-citation count (`source_count`): `None` when absent or not an
    /// integer. The one place key and parse are defined for every reader.
    pub fn source_count(&self) -> Option<u64> {
        self.get(field::SOURCE_COUNT)
            .and_then(serde_json::Value::as_u64)
    }
}

#[derive(Debug, Clone)]
pub struct VaultPage {
    pub frontmatter: Frontmatter,
    pub body: String,
}

/// True when `line` is a standalone frontmatter delimiter — exactly `---` once its line
/// terminator (`\n`, or `\r\n`) is stripped. No other trailing-whitespace tolerance (a
/// `--- ` with a space is NOT a delimiter), matching Obsidian/CommonMark strictness. The
/// `\r` strip matters because `set_frontmatter_field` scans raw bytes line-by-line (it
/// never normalizes CRLF, to round-trip a CRLF file faithfully), so it can hand this a
/// `---\r\n` line. Single-sourced here so `parse_page` and
/// `lk_vault::set_frontmatter_field` can never disagree on what closes a frontmatter block.
pub fn is_delimiter_line(line: &str) -> bool {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    line == "---"
}

/// A page split into its frontmatter block and its body — the recognition rule alone, with no
/// judgment about whether the YAML parses.
///
/// Separate from [`parse_page`] because the two answer different questions. A page's LINKS live
/// in its body, so a consumer asking what a page points at needs the body whether or not the
/// YAML above it is valid; only the frontmatter's own fields depend on that.
pub struct PageParts {
    pub yaml: String,
    pub body: String,
    /// False when a block opens and never closes, which is a page with NO frontmatter rather
    /// than one whose frontmatter runs to the end: nothing delimits a block that never closed,
    /// so every line of it is text a reader sees and a link in it is a link they can follow.
    /// [`parse_page`] refuses such a page — no fields can be read from it, and a writer cannot
    /// show it is preserving a format the bytes never stated.
    pub closed: bool,
}

/// Split a markdown page on its frontmatter delimiters.
///
/// Frontmatter is recognized only when the document's FIRST line is exactly `---` and a later
/// line is exactly `---` (delimiters must stand alone on their line). This avoids two false
/// detections a substring scan would make: a body that merely begins with whitespace then
/// `---`, and a `---` appearing inside a YAML value or a `---not-a-delimiter` line. CRLF is
/// normalized to LF up front.
pub fn split_page(content: &str) -> PageParts {
    // Strip a leading UTF-8 BOM so a BOM-prefixed file's frontmatter is still recognized.
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let normalized = content.replace("\r\n", "\n");

    let Some(rest) = normalized.strip_prefix("---\n") else {
        return PageParts {
            yaml: String::new(),
            body: normalized,
            closed: true,
        };
    };

    // Locate the closing delimiter: a line equal to exactly "---".
    let mut offset = 0usize;
    let mut closing = None;
    for line in rest.split_inclusive('\n') {
        if is_delimiter_line(line) {
            closing = Some(offset);
            break;
        }
        offset += line.len();
    }
    let Some(closing) = closing else {
        return PageParts {
            yaml: String::new(),
            body: rest.to_string(),
            closed: false,
        };
    };

    let after_delim = &rest[closing..];
    let body = after_delim
        .strip_prefix("---\n")
        .or_else(|| after_delim.strip_prefix("---"))
        .unwrap_or(after_delim);

    PageParts {
        yaml: rest[..closing].to_string(),
        body: body.strip_prefix('\n').unwrap_or(body).to_string(),
        closed: true,
    }
}

/// Parse a markdown page into its YAML frontmatter and body.
pub fn parse_page(content: &str) -> Result<VaultPage, String> {
    let parts = split_page(content);
    if !parts.closed {
        return Err("unclosed frontmatter block".to_string());
    }

    let fields: BTreeMap<String, serde_json::Value> = serde_yaml_ng::from_str(&parts.yaml)
        .map_err(|e| format!("invalid frontmatter YAML: {e}"))?;

    Ok(VaultPage {
        frontmatter: Frontmatter { fields },
        body: parts.body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let original = "---\nid: test-123\ntitle: \"Hello World\"\nlabels:\n- ai\n- ml\n---\n\n# Body\n\nContent here.\n";
        let page = parse_page(original).unwrap();
        assert_eq!(
            page.frontmatter.get("id").and_then(|v| v.as_str()),
            Some("test-123")
        );
        assert!(page.body.contains("# Body"));
    }

    #[test]
    fn no_frontmatter() {
        let page = parse_page("# Just a heading\n\nContent.").unwrap();
        assert!(page.frontmatter.fields.is_empty());
        assert!(page.body.contains("Just a heading"));
    }

    /// A block that opens and never closes delimits nothing, so the text below it is TEXT —
    /// a reader sees it and a link in it is one they can follow. Taking it as frontmatter
    /// running to the end left every such page with an empty body, so its links vanished from
    /// the scan: `graph broken` reported none, and `graph merge` deleted a concept while a live
    /// citation still pointed at it.
    #[test]
    fn an_unclosed_block_is_a_page_with_no_frontmatter() {
        let parts = split_page("---\nid: notes\ntitle: unclosed\n\n# Notes\n\n[a](b.md)\n");
        assert!(!parts.closed);
        assert!(
            parts.yaml.is_empty(),
            "nothing delimits a block that never closed"
        );
        assert!(parts.body.contains("[a](b.md)"), "{:?}", parts.body);

        // And no fields can be read from it, so the page still has no frontmatter to trust.
        assert!(parse_page("---\nid: notes\n\n# Notes\n").is_err());
    }

    #[test]
    fn delimiter_predicate_is_exact_and_newline_tolerant_only() {
        // Single source of truth for the `---` delimiter, shared with
        // `lk_vault::set_frontmatter_field` so the two can't disagree. Exactly `---`
        // (optionally with one trailing newline); no trailing-whitespace tolerance.
        assert!(is_delimiter_line("---"));
        assert!(is_delimiter_line("---\n"));
        assert!(is_delimiter_line("---\r\n")); // CRLF: set_frontmatter_field scans raw bytes
        assert!(!is_delimiter_line("--- ")); // trailing space is NOT a delimiter
        assert!(!is_delimiter_line("----"));
        assert!(!is_delimiter_line("---x"));
    }

    #[test]
    fn crlf_normalized() {
        let crlf = "---\r\nid: test\r\ntitle: \"CRLF\"\r\n---\r\n\r\n# Body\r\n\r\nContent.\r\n";
        let page = parse_page(crlf).unwrap();
        assert_eq!(
            page.frontmatter.get("id").and_then(|v| v.as_str()),
            Some("test")
        );
        assert!(!page.body.contains('\r'));
        assert!(page.body.contains("# Body"));
    }

    #[test]
    fn crlf_without_frontmatter() {
        let page = parse_page("# Title\r\n\r\nContent.\r\n").unwrap();
        assert!(page.frontmatter.fields.is_empty());
        assert!(!page.body.contains('\r'));
    }

    #[test]
    fn leading_whitespace_before_dashes_is_not_frontmatter() {
        // A body that merely starts with whitespace then `---` must not be parsed
        // as frontmatter.
        let page = parse_page("  \n---\nnot: frontmatter\n").unwrap();
        assert!(page.frontmatter.fields.is_empty());
        assert!(page.body.contains("not: frontmatter"));
    }

    #[test]
    fn dashes_inside_yaml_value_do_not_close_early() {
        // An indented `---` inside a block scalar is not a standalone delimiter.
        let doc = "---\nnote: |\n  ---\n  still in yaml\nid: x\n---\n\nbody\n";
        let page = parse_page(doc).unwrap();
        assert_eq!(
            page.frontmatter.get("id").and_then(|v| v.as_str()),
            Some("x")
        );
        assert_eq!(page.body, "body\n");
    }

    #[test]
    fn unclosed_frontmatter_errors() {
        assert!(parse_page("---\nid: x\nno closing delimiter\n").is_err());
    }

    #[test]
    fn bom_prefixed_frontmatter_is_recognized() {
        let page = parse_page("\u{feff}---\nid: bom\n---\n\n# Body\n").unwrap();
        assert_eq!(
            page.frontmatter.get("id").and_then(|v| v.as_str()),
            Some("bom")
        );
    }
}
