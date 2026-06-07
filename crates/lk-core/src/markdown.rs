//! Markdown structure primitives shared across crates: fenced-code tracking,
//! heading demotion, and the vault-text cleanliness contract. Single source of
//! truth — `lk-vault::section` (section locate/replace) and
//! `lk-pipeline::normalize` (source-body sanitisation) build on the fence/heading
//! parsing instead of re-implementing it, and the `scan_defects` contract is shared
//! by the converters that uphold it, the property tests that assert it, and
//! `lore doctor` that checks it on pages at rest.

/// A way a materialized vault page violates the text-cleanliness contract the
/// pipeline guarantees. Each variant names an EXACT property — never a heuristic
/// guess — so a hit is always a real defect: text no honest converter could emit,
/// or a page written before the guarantee existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDefect {
    /// An inlined `data:` URI. Every rich-text converter degrades these to alt text
    /// because an embedded base64 payload is encoded bytes, not retrievable
    /// knowledge, and bloats both the page and every LLM task that reads it.
    InlineDataUri,
}

impl TextDefect {
    /// One-line human description for `lore doctor` output.
    pub fn description(self) -> &'static str {
        match self {
            TextDefect::InlineDataUri => {
                "inlined data: URI — encoded bytes, not knowledge; converters strip these"
            }
        }
    }
}

/// Scan a materialized page's text for cleanliness-contract violations, returning
/// each defect with its 1-based line number (empty result = clean). The SINGLE
/// SOURCE OF TRUTH for the contract: the rich-text converters uphold it
/// (`lk_source::markdown::html_to_markdown` degrades data: URIs to alt text),
/// property tests assert it on converter output, and `lore doctor` checks it on
/// pages at rest — all three call THIS, so they can never disagree about what
/// "clean vault text" means. New invariants are added as `TextDefect` variants,
/// extending every enforcement point at once.
pub fn scan_defects(text: &str) -> Vec<(usize, TextDefect)> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| has_inline_data_uri(line))
        .map(|(i, _)| (i + 1, TextDefect::InlineDataUri))
        .collect()
}

/// Exact `data:`-URI signatures with zero false positives on prose: a markdown
/// image/link target (`](data:`) and an autolink (`<data:`) — the only shapes a
/// converter could emit. Matched case-insensitively to mirror the conversion-time
/// `lk_source::markdown` `is_data_uri` check (which strips `DATA:` too), so the
/// checker and the converter never disagree about a page's cleanliness.
fn has_inline_data_uri(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("](data:") || lower.contains("<data:")
}

/// Tracks whether a line walker is currently inside a fenced code block. Per
/// CommonMark, an opening fence is 3+ consecutive `` ` `` or `~` characters (with
/// optional info string); a closing fence must use the same character, be at least
/// as long as the opener, and carry no info string. Headings (and other structure)
/// inside an open fence are quoted content, not document structure.
#[derive(Debug, Clone, Copy)]
pub enum FenceState {
    Closed,
    Open { marker: char, len: usize },
}

impl FenceState {
    pub fn new() -> Self {
        FenceState::Closed
    }

    pub fn is_closed(self) -> bool {
        matches!(self, FenceState::Closed)
    }

    /// Apply one line to the fence state. Returns true if the line was a fence
    /// marker (and thus must not be treated as document structure).
    pub fn apply(&mut self, line: &str) -> bool {
        let Some((marker, len, info)) = parse_fence(line) else {
            return false;
        };
        match *self {
            FenceState::Closed => {
                *self = FenceState::Open { marker, len };
                true
            }
            FenceState::Open {
                marker: open_marker,
                len: open_len,
            } => {
                // A closing fence must match the opener's character, be at least as
                // long, and carry no info string. Anything else is a marker-shaped
                // line inside the open block and the fence stays open.
                if marker == open_marker && len >= open_len && info.is_empty() {
                    *self = FenceState::Closed;
                }
                true
            }
        }
    }
}

impl Default for FenceState {
    fn default() -> Self {
        Self::new()
    }
}

/// Recognize a fence marker line. Returns `(marker char, marker length, info
/// string)`. CommonMark allows up to three spaces of leading indent before the
/// marker; the info string is everything after the marker run. A backtick fence's
/// info string MUST NOT contain a backtick (ambiguous with an inline code span),
/// so such a line is not a fence. Tilde fences have no such restriction.
pub fn parse_fence(line: &str) -> Option<(char, usize, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let marker_len = trimmed.chars().take_while(|c| *c == marker).count();
    if marker_len < 3 {
        return None;
    }
    let info = trimmed[marker_len..].trim();
    if marker == '`' && info.contains('`') {
        return None;
    }
    Some((marker, marker_len, info))
}

/// The ATX heading level of a line (1–6), or `None` if it isn't a heading.
/// Per CommonMark: up to 3 leading spaces, 1–6 `#`, then a space or end of line.
/// `####### ` (7 hashes) is not a heading. Returns the byte offset where the `#`
/// run starts so callers can rewrite in place.
fn atx_heading(line: &str) -> Option<(usize, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    // The hash run must be followed by a space or the end of the line.
    match rest[level..].chars().next() {
        None | Some(' ') => Some((indent, level)),
        _ => None,
    }
}

/// Demote every ATX heading in `text` so the shallowest sits at `floor` (clamped to
/// H6), preserving relative structure. Headings inside fenced code blocks are left
/// untouched. Used to sanitise embedded source content (Jira ADF, manual `.md`,
/// RSS→Markdown) before it is rendered under a page's `##`-structured sections, so a
/// source body's `## Heading` never collides with the page/section/event heading
/// hierarchy that `lk-vault::section` relies on. A no-op when there are no headings
/// or the shallowest is already at/below `floor`.
pub fn demote_headings(text: &str, floor: usize) -> String {
    // Pass 1: find the shallowest non-fenced heading level.
    let mut fence = FenceState::new();
    let mut min_level: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        // Skip a line that is a fence marker (`apply` true) OR sits inside an open
        // fence (state still Open after applying it) — only un-fenced lines are
        // document structure.
        let is_marker = fence.apply(stripped);
        if is_marker || !fence.is_closed() {
            continue;
        }
        if let Some((_, level)) = atx_heading(stripped) {
            min_level = Some(min_level.map_or(level, |m| m.min(level)));
        }
    }

    let Some(min_level) = min_level else {
        return text.to_string();
    };
    if min_level >= floor {
        return text.to_string();
    }
    let shift = floor - min_level;

    // Pass 2: rewrite each non-fenced heading line, raising its level by `shift`
    // (clamped to 6) by inserting the extra `#`s at the start of the hash run.
    let mut fence = FenceState::new();
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let nl = line.ends_with('\n');
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let is_marker = fence.apply(stripped);
        if is_marker || !fence.is_closed() {
            out.push_str(line);
            continue;
        }
        match atx_heading(stripped) {
            Some((indent, level)) => {
                let new_level = (level + shift).min(6);
                out.push_str(&stripped[..indent]);
                for _ in 0..new_level {
                    out.push('#');
                }
                out.push_str(&stripped[indent + level..]);
                if nl {
                    out.push('\n');
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demotes_h2_to_floor_preserving_relative_structure() {
        // Shallowest is H2 → shift +2 so it lands at H4; H3 follows to H5.
        let input = "## Plan\n\nbody\n\n### Detail\n\nmore\n";
        let out = demote_headings(input, 4);
        assert_eq!(out, "#### Plan\n\nbody\n\n##### Detail\n\nmore\n");
    }

    #[test]
    fn h1_demoted_to_floor() {
        assert_eq!(demote_headings("# Title\ntext\n", 4), "#### Title\ntext\n");
    }

    #[test]
    fn noop_when_already_at_or_below_floor() {
        let input = "#### Already deep\n\n##### Deeper\n";
        assert_eq!(demote_headings(input, 4), input);
    }

    #[test]
    fn noop_when_no_headings() {
        let input = "just a paragraph\n\nand another\n";
        assert_eq!(demote_headings(input, 4), input);
    }

    #[test]
    fn clamps_at_h6() {
        // H2 with shift +2 → H4; an H5 in the same body → H7 clamped to H6.
        let input = "## A\n\n##### Deep\n";
        let out = demote_headings(input, 4);
        assert_eq!(out, "#### A\n\n###### Deep\n");
    }

    #[test]
    fn leaves_headings_inside_fenced_code_untouched() {
        // The `## Inside` is fenced code, not a heading — it must NOT count toward
        // min-level NOR be rewritten. The real `## Real` heading drives the shift.
        let input = "## Real\n\n```\n## Inside\n```\n";
        let out = demote_headings(input, 4);
        assert_eq!(out, "#### Real\n\n```\n## Inside\n```\n");
    }

    #[test]
    fn ignores_non_heading_hash_lines() {
        // `#nospace` and a 7-hash run are not ATX headings.
        let input = "#nospace\n\n####### sevenhashes\n";
        assert_eq!(demote_headings(input, 4), input);
    }

    #[test]
    fn preserves_trailing_text_after_hashes() {
        assert_eq!(
            demote_headings("## Heading text here\n", 4),
            "#### Heading text here\n"
        );
    }

    #[test]
    fn scan_flags_inline_data_uri_image_with_line_number() {
        let page = "# Page\n\nintro\n\n![logo](data:image/png;base64,AAAA)\n\nmore\n";
        assert_eq!(scan_defects(page), vec![(5, TextDefect::InlineDataUri)]);
    }

    #[test]
    fn scan_flags_data_uri_autolink() {
        assert_eq!(
            scan_defects("see <data:text/plain,hi>\n"),
            vec![(1, TextDefect::InlineDataUri)]
        );
    }

    #[test]
    fn scan_clean_page_has_no_defects() {
        // A fetchable http image and the bare word "data" are both fine — only an
        // inlined data: URI target is a defect.
        let page = "# Page\n\n![chart](https://x.io/a.png)\n\nthe data shows growth\n";
        assert!(scan_defects(page).is_empty());
    }

    #[test]
    fn scan_is_case_insensitive_on_the_scheme() {
        // The converter strips `DATA:` (case-insensitive), so the checker must catch it
        // too — otherwise a page the pipeline cleaned could still read as defective.
        assert_eq!(
            scan_defects("![x](DATA:image/png;base64,AA)\n"),
            vec![(1, TextDefect::InlineDataUri)]
        );
    }

    #[test]
    fn scan_reports_correct_line_across_crlf_and_bom() {
        // `str::lines` strips a trailing `\r`, and a leading BOM rides on line 1, so the
        // reported line number is stable regardless of encoding quirks.
        let page = "\u{feff}# Page\r\n\r\nintro\r\n![x](data:text/plain,a)\r\n";
        assert_eq!(scan_defects(page), vec![(4, TextDefect::InlineDataUri)]);
    }
}
