//! Markdown structure primitives shared across crates: fenced-code tracking and
//! heading demotion. Single source of truth — `lk-vault::section` (section
//! locate/replace) and `lk-pipeline::normalize` (source-body sanitisation) both
//! build on these instead of re-implementing fence/heading parsing.

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
}
