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

use lk_core::i18n::{Locale, Strings};
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
/// An EMPTY body collapses to `## {heading}\n\n## {next}` — the shape a template renders an
/// unfilled section as. The two are writers of the same sections: the pipeline renders a page
/// from a template and this rewrites sections of it in place, so a second spelling of "this
/// section is empty" makes each run undo the other's, and every sweep reports a page nothing
/// changed about as changed.
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
        if end_idx < lines.len() {
            out.push('\n');
        }
    }

    for line in &lines[end_idx..] {
        out.push_str(line);
    }

    out
}

/// A logical section of a page, named independently of the locale it was rendered under —
/// `|s| s.concept_sources` rather than `"Sources"`. A closure rather than a plain function
/// pointer because a section's identity can depend on the page: a daily page's item heading
/// is the source type's (`events` vs `messages`), chosen at render time.
pub trait SectionKey: Fn(&'static Strings) -> &'static str {}
impl<F: Fn(&'static Strings) -> &'static str> SectionKey for F {}

/// The spelling a page gives one logical section, and the body under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSection<'a> {
    /// The heading as the page writes it, which is what a rewrite must target and what a
    /// queued task must carry as its anchor.
    pub heading: &'static str,
    /// The body verbatim, leading and trailing whitespace included.
    pub body: &'a str,
}

/// Every spelling one logical section takes, in [`Locale::ALL`] order and without repeats —
/// the vocabulary a caller needs when it matches headings itself rather than reading a body.
pub fn section_headings(section: impl SectionKey) -> impl Iterator<Item = &'static str> {
    let mut seen: Vec<&'static str> = Vec::new();
    Locale::ALL.iter().filter_map(move |locale| {
        let heading = section(locale.strings());
        (!seen.contains(&heading)).then(|| {
            seen.push(heading);
            heading
        })
    })
}

/// The section `content` carries for one logical section, under whichever locale it was
/// authored in.
///
/// READ tolerates any locale's heading; only WRITE uses the current one. A `vault.locale`
/// switch renames every heading at once, so a lookup that searched only the new spelling
/// finds no body to preserve and none to report — and the caller overwrites an answered
/// section with an empty one, on every page in the vault, reporting a clean run.
///
/// A page mid-switch can carry both spellings: the freshly rendered heading, empty, beside
/// the one holding the answer. So a non-blank body WINS over a blank one, whatever order the
/// locales fall in — otherwise the answer is the thing that gets dropped. A blank body is
/// still an answer when it is the only one there: a section can be legitimately empty (an
/// extraction that found nothing, a focus-filtered summary), and reporting the section absent
/// would re-enqueue it forever.
pub fn resolve_section(content: &str, section: impl SectionKey) -> Option<PageSection<'_>> {
    let mut blank: Option<PageSection> = None;
    for heading in section_headings(&section) {
        let Some(body) = section_body(content, heading) else {
            continue;
        };
        if !body.trim().is_empty() {
            return Some(PageSection { heading, body });
        }
        blank.get_or_insert(PageSection { heading, body });
    }
    blank
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

/// Split off a leading BOM the way `parse_page` does, so it can be restored and the page
/// round-trips byte-faithfully.
fn split_bom(content: &str) -> (&str, &str) {
    content
        .strip_prefix('\u{feff}')
        .map_or(("", content), |rest| ("\u{feff}", rest))
}

/// A line's leading-space indentation.
fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches(' ').len()]
}

/// Strip a line's terminator, leaving the text the YAML sees.
fn unterminated(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

/// True when this line opens a BLOCK SCALAR (`key: |`, `key: >-`, `- |`, `- key: |2`, …) —
/// the one construct whose following `#` lines are value text rather than comments. Decided
/// from the indicator, since indentation cannot tell a comment sitting inside a value apart
/// from one merely indented past it.
///
/// Sequence markers are stripped first so a list ITEM is read like the entry it holds: a
/// keyless `- |` has no colon to look behind, and skipping it leaves its content lines to be
/// mistaken for comments. An anchor or tag may then sit between the colon and the indicator
/// (`key: &a |`, `- !!str >`), so the first token that is neither is the one to test.
///
/// Deliberately NOT a YAML parser: a key that is itself quoted AND contains a colon
/// (`"a:b": |`) reads as no indicator here. Every writer in this repo emits plain
/// `snake_case` keys, and the alternative is a quoting-aware tokenizer maintained against
/// hand-edited input — more surface than the case it defends. A page like that loses a
/// trailing comment inside the scalar; it does not become unparseable.
fn opens_block_scalar(line: &str) -> bool {
    let mut rest = line.trim_start();
    while let Some(item) = rest.strip_prefix('-') {
        // `-foo` is a plain scalar, not a marker; `-` alone is an empty item.
        if !item.is_empty() && !item.starts_with([' ', '\t']) {
            break;
        }
        rest = item.trim_start();
    }
    let value = rest.split_once(':').map_or(rest, |(_, v)| v);
    value
        .split_whitespace()
        .find(|token| !token.starts_with(['&', '!']))
        .is_some_and(|token| token.starts_with(['|', '>']))
}

/// The terminator a written line takes, copied from the line it replaces or joins, so a
/// CRLF page does not come back with one stray LF among its lines.
fn terminator(line: &str) -> &str {
    if line.ends_with("\r\n") { "\r\n" } else { "\n" }
}

/// The frontmatter block's key lines, as a range over `lines`: it starts after the opening
/// `---` and ends AT the closing one, so `Range::end` doubles as the insertion point for a
/// key the block does not have yet. `None` when the document has no frontmatter block.
///
/// The opening delimiter must be the document's FIRST line — the same rule as
/// [`lk_core::frontmatter::parse_page`], so a body thematic break is never mistaken for an
/// opener — and delimiter recognition is single-sourced with it, so the two can never
/// disagree on what closes a block.
///
/// Every frontmatter writer here is bounded by this range. That is what keeps a key the
/// writer cannot place from landing in the body instead: a write that does not fit inside
/// the block does not happen at all.
fn frontmatter_block(lines: &[&str]) -> Option<std::ops::Range<usize>> {
    use lk_core::frontmatter::is_delimiter_line;
    if !is_delimiter_line(lines.first()?) {
        return None;
    }
    let closing = lines[1..].iter().position(|l| is_delimiter_line(l))? + 1;
    Some(1..closing)
}

/// Rebuild the document with `line` written in place of `replaced`. An empty range inserts
/// before its start instead. Every other line is copied byte for byte.
fn splice_lines(bom: &str, lines: &[&str], replaced: std::ops::Range<usize>, line: &str) -> String {
    let mut out = String::with_capacity(bom.len() + line.len() + content_len(lines));
    out.push_str(bom);
    for existing in &lines[..replaced.start] {
        out.push_str(existing);
    }
    out.push_str(line);
    for existing in &lines[replaced.end..] {
        out.push_str(existing);
    }
    out
}

fn content_len(lines: &[&str]) -> usize {
    lines.iter().map(|l| l.len()).sum()
}

/// The lines that belong to the entry starting at `start`, bounded by `end`: a block list's
/// items (`  - RAG`), a nested mapping's children, a folded scalar's text. `None` when the
/// entry is a single line.
///
/// A continuation is a line indented MORE than the entry's own line, which is what makes
/// this work at any depth: for a top-level key the next column-0 key ends it, for a child
/// the next sibling at the same indent does.
///
/// Blank lines and comments do NOT end an entry — YAML permits both at any column, so
/// treating one as the end would take a later continuation for a separate entry. Writing
/// there leaves two copies of the same key in one block, and the stale one wins on the next
/// parse: the write silently undone. The span stops at the LAST real continuation, so
/// trailing blanks and comments stay with whatever follows them rather than being swallowed
/// by a rewrite.
fn continuation_span(lines: &[&str], start: usize, end: usize) -> Option<std::ops::Range<usize>> {
    let head = unterminated(lines[start]);
    let base = indent_of(head).len();
    // Indentation of the key line that opened the block scalar we are currently inside, if
    // any. A `#` line is VALUE TEXT exactly when it sits inside one, and only the nearest
    // preceding shallower key can answer that — never the entry this span belongs to. A span
    // over `llm_inputs:` walks its children, and `llm_inputs:` is not a block scalar even
    // when one of them is.
    let mut scalar = opens_block_scalar(head).then_some(base);

    let (mut first, mut last) = (None, None);
    for (i, line) in lines.iter().enumerate().take(end).skip(start + 1) {
        let line = unterminated(line);
        let trimmed = line.trim();
        // A blank line neither ends the entry nor belongs to it.
        if trimmed.is_empty() {
            continue;
        }
        let indent = indent_of(line).len();
        if !scalar.is_some_and(|open| indent > open) {
            // Out of any block scalar, so this line is structure.
            scalar = None;
            // A comment belongs to whatever FOLLOWS it, at any indentation: it neither ends
            // this entry nor is carried away with it.
            if trimmed.starts_with('#') {
                continue;
            }
            if indent <= base {
                break;
            }
            if opens_block_scalar(line) {
                scalar = Some(indent);
            }
        }
        first.get_or_insert(i);
        last = Some(i);
    }
    Some(first?..last? + 1)
}

/// Set a scalar `key: value` inside the frontmatter block ONLY. Matches a TOP-LEVEL
/// (column-0) key, so a `key:` in body prose, a code fence, or nested under a mapping
/// (e.g. `summary:` under `llm_inputs:`) is never touched. If the field is absent it is
/// inserted just before the closing `---`, so a later run sees it and the operation is
/// idempotent. A leading BOM is preserved. The single source of truth for setting a
/// frontmatter scalar — used by `backlinks-sync` (`source_count`), `record_llm_input` (which
/// composes it to add an absent `llm_inputs:` mapping) and the merge's `aliases`.
///
/// `value` is written verbatim, so the caller owns serialization (`serde_json::to_string`
/// for anything that could need quoting).
///
/// Replacing a key takes its whole value with it, including a BLOCK-style one spread over
/// the lines below (`aliases:` then `  - RAG`). Rewriting only the `key:` line would leave
/// those items orphaned under the new inline value and the page would no longer parse.
///
/// `None` when the page has no frontmatter block — there is nowhere to put the field, and
/// a caller that dropped it silently would report a write it never made.
pub fn set_frontmatter_field(content: &str, key: &str, value: &str) -> Option<String> {
    let (bom, body) = split_bom(content);
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let block = frontmatter_block(&lines)?;

    let prefix = format!("{key}:");
    let existing = lines[block.clone()]
        .iter()
        .position(|line| line.starts_with(&prefix))
        .map(|i| i + block.start);

    let replaced = match existing {
        Some(at) => at..continuation_span(&lines, at, block.end).map_or(at + 1, |s| s.end),
        None => block.end..block.end,
    };
    let eol = terminator(lines[replaced.start]);
    // An empty value writes a bare `key:` — the YAML for a key whose value is the block
    // below it. Writing `key: ` instead would leave a trailing space on every page carrying
    // one, invisible in an editor and a diff away from the form a render produces.
    let line = match value {
        "" => format!("{key}:{eol}"),
        value => format!("{key}: {value}{eol}"),
    };
    Some(splice_lines(bom, &lines, replaced, &line))
}

/// Set `llm_inputs.<key>` in a page's frontmatter, leaving every other line byte-identical.
///
/// The completion markers live one level down, so [`set_frontmatter_field`] — which owns
/// top-level keys — cannot reach them. Whoever fills a section must stamp its marker in the
/// same edit: `llm_cache::lookup` decides purely on the marker, never on whether the body
/// looks filled, so a section written without one is erased by the next re-render and its
/// task re-enqueued forever.
///
/// `value` is written verbatim, as in [`set_frontmatter_field`].
///
/// `None` when the frontmatter carries no block-style `llm_inputs:` mapping to write into.
/// Inventing one would misrepresent an unrendered page as cached, and writing the marker
/// anywhere else would leave the loop above running forever against a page that never
/// records completion — so the caller is told instead.
pub fn set_llm_input(content: &str, key: &str, value: &str) -> Option<String> {
    let (bom, body) = split_bom(content);
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let (parent_idx, children) = llm_inputs_mapping(&lines)?;
    let existing = llm_input_entry(&lines, &children, key);

    // A replacement keeps its own line's indentation; a new key joins the mapping directly
    // after its last child, at the indentation the first one already uses.
    let children_end = children.clone().map_or(parent_idx + 1, |s| s.end);
    let at = existing.clone().map_or(children_end, |s| s.start);
    let indent = existing
        .clone()
        .map(|s| s.start)
        .or_else(|| children.map(|s| s.start))
        .map(|i| indent_of(unterminated(lines[i])))
        .unwrap_or("  ");
    let eol = terminator(lines[at]);

    Some(splice_lines(
        bom,
        &lines,
        existing.unwrap_or(at..at),
        &format!("{indent}{key}: {value}{eol}"),
    ))
}

/// Remove `llm_inputs.<key>`, value continuation included, leaving every other line
/// byte-identical. `None` where the mapping or the key is absent.
///
/// The counterpart to [`record_llm_input`], and needed for the same reason that writer exists:
/// an input is a PROMISE that something will answer it, so whoever recorded one that can no
/// longer be answered is the one that has to withdraw it. Left behind, it is a task the queue
/// keeps calling current and a section `lore doctor` keeps calling unanswered — neither of
/// which any later run can clear, because nothing else knows the promise was made.
pub fn clear_llm_input(content: &str, key: &str) -> Option<String> {
    let (bom, body) = split_bom(content);
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let (_, children) = llm_inputs_mapping(&lines)?;
    let entry = llm_input_entry(&lines, &children, key)?;
    Some(splice_lines(bom, &lines, entry, ""))
}

/// The block-style `llm_inputs:` mapping: the line it is declared on, and the span its
/// entries occupy (`None` where it has none yet). Both writers locate it through here, so
/// what one of them can reach is exactly what the other can.
///
/// Trailing whitespace after the key is invisible and legal, so it must not decide whether
/// the mapping is found at all.
fn llm_inputs_mapping(lines: &[&str]) -> Option<(usize, Option<std::ops::Range<usize>>)> {
    let block = frontmatter_block(lines)?;
    let parent = format!("{}:", lk_core::frontmatter::field::LLM_INPUTS);
    let parent_idx = lines[block.clone()]
        .iter()
        .position(|line| unterminated(line).trim_end() == parent)
        .map(|i| i + block.start)?;
    Some((parent_idx, continuation_span(lines, parent_idx, block.end)))
}

/// The lines one entry of that mapping occupies, its value's continuation included — so a
/// caller replacing or removing a key whose value is a block scalar never orphans the
/// value's lines under it.
///
/// The mapping's span reaches everything under it, grandchildren included. An entry lives one
/// level down, so only that level is searched: otherwise a same-named key nested deeper would
/// be matched first and the real one left stale.
fn llm_input_entry(
    lines: &[&str],
    children: &Option<std::ops::Range<usize>>,
    key: &str,
) -> Option<std::ops::Range<usize>> {
    let children = children.clone()?;
    let child_indent = indent_of(unterminated(lines[children.start])).len();
    let prefix = format!("{key}:");
    let at = children.clone().find(|&i| {
        let line = unterminated(lines[i]);
        indent_of(line).len() == child_indent && line.trim_start().starts_with(&prefix)
    })?;
    Some(at..continuation_span(lines, at, children.end).map_or(at + 1, |s| s.end))
}

/// Set `llm_inputs.<key>`, adding the mapping only when the page carries NO `llm_inputs` key
/// at all.
///
/// [`set_llm_input`] refuses a page without a block-style mapping, and rightly: the
/// pipeline's pages always render one, so its absence means the page is not one the pipeline
/// wrote, and a marker stamped there would claim a render that never happened. A concept
/// page's synthesis input is the other shape — `graph backlinks-sync` DERIVES it from the
/// link graph, so the writer establishes the mapping rather than a reader hoping to find it.
///
/// The two reasons that writer can refuse are NOT interchangeable. A page with no key has
/// nothing to lose by gaining one. A page whose key is a FLOW mapping (`llm_inputs: {summary:
/// "abc"}`) is refused because this module cannot write into that shape — and creating the
/// key there would replace every entry it already holds with a bare heading. So absence is
/// the only case that creates; an unwritable shape is reported to the caller, which is what
/// [`set_llm_input`] already meant by refusing.
///
/// `None` when the page has no frontmatter block, or carries an `llm_inputs` key this cannot
/// write into.
pub fn record_llm_input(content: &str, key: &str, value: &str) -> Option<String> {
    if let Some(updated) = set_llm_input(content, key, value) {
        return Some(updated);
    }
    if has_frontmatter_key(content, lk_core::frontmatter::field::LLM_INPUTS) {
        return None;
    }
    let created = set_frontmatter_field(content, lk_core::frontmatter::field::LLM_INPUTS, "")?;
    set_llm_input(&created, key, value)
}

/// Whether the frontmatter block carries `key` as a TOP-LEVEL entry, by the same column-0
/// match [`set_frontmatter_field`] uses to find one — so what this reports present is exactly
/// what that writer would replace.
fn has_frontmatter_key(content: &str, key: &str) -> bool {
    let (_, body) = split_bom(content);
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let Some(block) = frontmatter_block(&lines) else {
        return false;
    };
    let prefix = format!("{key}:");
    lines[block].iter().any(|line| line.starts_with(&prefix))
}

#[cfg(test)]
mod tests {
    /// Test-only predicate over `section_body`, used to express the fence/heading
    /// boundary cases below as "is there body text under this heading?". Note the
    /// pipeline cache does NOT use section emptiness for completion — that is
    /// marker-signalled (`llm_cache`) — so this is purely a `section_body` boundary test.
    fn is_section_filled(content: &str, heading: &str) -> bool {
        super::section_body(content, heading).is_some_and(|b| !b.trim().is_empty())
    }

    use super::*;

    #[test]
    fn set_frontmatter_field_inserts_when_absent() {
        let doc = "---\nid: x\n---\n\n# X\n";
        let out = set_frontmatter_field(doc, "source_count", "3").unwrap();
        assert_eq!(out, "---\nid: x\nsource_count: 3\n---\n\n# X\n");
    }

    #[test]
    fn set_frontmatter_field_replaces_when_present() {
        let doc = "---\nid: x\nsource_count: 1\n---\n\n# X\n";
        let out = set_frontmatter_field(doc, "source_count", "9").unwrap();
        assert_eq!(out, "---\nid: x\nsource_count: 9\n---\n\n# X\n");
    }

    #[test]
    fn set_frontmatter_field_is_idempotent() {
        let doc = "---\nid: x\n---\n\n# X\n";
        let once = set_frontmatter_field(doc, "h", "abc").unwrap();
        let twice = set_frontmatter_field(&once, "h", "abc").unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn set_frontmatter_field_handles_crlf_delimiters() {
        // `set_frontmatter_field` scans raw bytes line-by-line, so the shared
        // `is_delimiter_line` must recognize a CRLF (`---\r\n`) delimiter — otherwise a
        // Windows/`autocrlf`-edited page's frontmatter block is never found and the update
        // silently no-ops inside what looks like body text. Regression guard for that.
        let doc = "---\r\nid: x\r\nsource_count: 1\r\n---\r\n\r\n# X\r\n";
        let out = set_frontmatter_field(doc, "source_count", "9").unwrap();
        // The field was updated INSIDE the frontmatter block (not appended/ignored), and the
        // body survives — i.e. the CRLF delimiters were recognized.
        assert!(
            out.contains("source_count: 9"),
            "field must be updated: {out:?}"
        );
        assert!(
            !out.contains("source_count: 1"),
            "old value must be gone: {out:?}"
        );
        assert!(out.contains("# X"), "body must survive: {out:?}");
        let page = lk_core::frontmatter::parse_page(&out).unwrap();
        assert_eq!(
            page.frontmatter
                .get("source_count")
                .and_then(|v| v.as_i64()),
            Some(9)
        );
    }

    #[test]
    fn set_frontmatter_field_ignores_body_occurrences() {
        // A `source_count:` in the BODY (prose or a code fence) must never be mutated —
        // only the frontmatter block between the first two `---` lines is touched.
        let doc = "---\nid: x\nsource_count: 1\n---\n\n# X\n\n```\nsource_count: 999\n```\n";
        let out = set_frontmatter_field(doc, "source_count", "2").unwrap();
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
        assert_eq!(set_frontmatter_field(doc, "k", "v"), None);
    }

    #[test]
    fn set_frontmatter_field_ignores_nested_keys() {
        // An indented key nested under a mapping must NOT be matched; the top-level field
        // is inserted independently (keeps the op idempotent on pages with nested YAML).
        let doc = "---\nllm_inputs:\n  source_count: 5\n---\n";
        let out = set_frontmatter_field(doc, "source_count", "2").unwrap();
        assert_eq!(
            out,
            "---\nllm_inputs:\n  source_count: 5\nsource_count: 2\n---\n"
        );
    }

    #[test]
    fn set_frontmatter_field_replaces_a_block_style_value_whole() {
        // Obsidian's property editor writes lists block-style. Rewriting only the `aliases:`
        // line would strand the items under the new inline value and the page would stop
        // parsing — on `graph merge`, that is the canonical concept page.
        let doc = "---\nid: x\naliases:\n  - RAG\n  - IR\nsource_count: 2\n---\n\n# X\n";
        let out = set_frontmatter_field(doc, "aliases", r#"["RAG","IR","Retrieval"]"#).unwrap();
        assert_eq!(
            out,
            "---\nid: x\naliases: [\"RAG\",\"IR\",\"Retrieval\"]\nsource_count: 2\n---\n\n# X\n"
        );
        let page = lk_core::frontmatter::parse_page(&out).expect("page must still parse");
        assert_eq!(
            page.frontmatter
                .get("aliases")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(3)
        );
    }

    #[test]
    fn a_keyless_block_scalar_in_a_list_is_replaced_whole() {
        let doc = "---\naliases:\n  - |\n    multi\n    # y\nid: x\n---\n";
        let out = set_frontmatter_field(doc, "aliases", "[]").unwrap();
        assert_eq!(out, "---\naliases: []\nid: x\n---\n");
    }

    #[test]
    fn a_plain_scalar_starting_with_a_dash_is_not_a_sequence_marker() {
        // `-foo` is a value, not a list item, so nothing may be stripped off it looking for
        // an indicator behind.
        let doc = "---\nnote: -foo\nid: x\n---\n";
        let out = set_frontmatter_field(doc, "id", "y").unwrap();
        assert_eq!(out, "---\nnote: -foo\nid: y\n---\n");
    }

    #[test]
    fn a_block_scalar_containing_dashes_does_not_end_the_entry_or_the_block() {
        // `---` indented inside a block scalar is content, not a delimiter. If the span or
        // the block boundary took it for one, the rewrite would land outside the frontmatter
        // — the class of bug `frontmatter_block` exists to make impossible.
        let doc = "---\nnote: |\n  ---\n  still yaml\nid: x\n---\n\nbody\n";
        let out = set_frontmatter_field(doc, "id", "y").unwrap();
        assert_eq!(
            out,
            "---\nnote: |\n  ---\n  still yaml\nid: y\n---\n\nbody\n"
        );
        let page = lk_core::frontmatter::parse_page(&out).unwrap();
        assert_eq!(
            page.frontmatter.get("id").and_then(|v| v.as_str()),
            Some("y")
        );
        assert!(page.body.contains("body"));
    }

    #[test]
    fn set_frontmatter_field_leaves_a_trailing_comment_with_what_follows_it() {
        // The span ends at the last real continuation, so a comment sitting between two
        // keys belongs to the one below it and survives the rewrite.
        let doc = "---\naliases:\n  - RAG\n# about the count\nsource_count: 2\n---\n";
        let out = set_frontmatter_field(doc, "aliases", "[]").unwrap();
        assert_eq!(
            out,
            "---\naliases: []\n# about the count\nsource_count: 2\n---\n"
        );
    }

    #[test]
    fn set_frontmatter_field_recognizes_and_preserves_bom() {
        let doc = "\u{feff}---\nid: x\n---\n";
        let out = set_frontmatter_field(doc, "source_count", "2").unwrap();
        assert_eq!(out, "\u{feff}---\nid: x\nsource_count: 2\n---\n");
    }

    #[test]
    fn set_frontmatter_field_does_not_false_match_key_prefix() {
        // Setting `source_count` must not touch a different key that merely shares a
        // prefix — the trailing `:` anchors the match.
        let doc = "---\nsource_count_extra: keep\nsource_count: 1\n---\n";
        let out = set_frontmatter_field(doc, "source_count", "5").unwrap();
        assert_eq!(out, "---\nsource_count_extra: keep\nsource_count: 5\n---\n");
    }

    #[test]
    fn replaces_body_between_headings() {
        let doc = "# Title\n\n## Sources\n\n- [old](old.md)\n\n## Meta\n\n- key: value\n";
        let out = replace_section(doc, "Sources", "- [new1](new1.md)\n- [new2](new2.md)");
        assert_eq!(
            out,
            "# Title\n\n## Sources\n\n- [new1](new1.md)\n- [new2](new2.md)\n\n## Meta\n\n- key: value\n"
        );
    }

    #[test]
    fn matches_headings_with_trailing_spaces() {
        let doc = "# Title\n\n## Sources  \n\n- [old](old.md)\n\n## Meta   \n\n- key: value\n";
        let out = replace_section(doc, "Sources", "- [new](new.md)");
        assert_eq!(
            out,
            "# Title\n\n## Sources  \n\n- [new](new.md)\n\n## Meta   \n\n- key: value\n"
        );
    }

    #[test]
    fn replaces_body_at_end_of_file() {
        let doc = "# Title\n\n## Sources\n\n- [old](old.md)\n";
        let out = replace_section(doc, "Sources", "- [new](new.md)");
        assert_eq!(out, "# Title\n\n## Sources\n\n- [new](new.md)\n");
    }

    /// A flow mapping is REFUSED, never replaced. `set_llm_input` cannot write into
    /// `llm_inputs: {summary: "abc"}`, and creating the key there would drop every entry it
    /// holds — so the two reasons that writer refuses are kept apart: absence creates, an
    /// unwritable shape is reported.
    #[test]
    fn a_flow_mapping_is_refused_rather_than_replaced() {
        let page =
            "---\nid: x\nllm_inputs: {summary: \"abc\", summary_done: \"abc\"}\n---\n\n# X\n";
        assert_eq!(record_llm_input(page, "synthesis", "\"d\""), None);
    }

    /// A page with no `llm_inputs` key gains one — the case the creation exists for.
    #[test]
    fn an_absent_mapping_is_created() {
        let page = "---\nid: x\nsource_count: 0\n---\n\n# X\n";
        let out = record_llm_input(page, "synthesis", "\"d\"").expect("created");
        assert!(out.contains("llm_inputs:\n  synthesis: \"d\""), "{out}");
        assert!(out.contains("source_count: 0"), "{out}");
    }

    /// An existing block mapping is joined, not rebuilt.
    #[test]
    fn a_block_mapping_keeps_every_entry_it_holds() {
        let page = "---\nid: x\nllm_inputs:\n  summary: \"abc\"\n---\n\n# X\n";
        let out = record_llm_input(page, "synthesis", "\"d\"").expect("joined");
        assert!(out.contains("summary: \"abc\""), "{out}");
        assert!(out.contains("synthesis: \"d\""), "{out}");
    }

    /// A page renamed by a `vault.locale` switch carries the answer under its old heading.
    /// Finding it is the whole point: a lookup that searched only the current spelling would
    /// hand the caller nothing to preserve, and an answered section would be overwritten
    /// empty on every page in the vault, reported as a clean run.
    #[test]
    fn a_section_authored_under_another_locale_is_found() {
        let page = "# RAG\n\n## 핵심\n\nThe durable record.\n\n## 출처\n";
        let found = resolve_section(page, |s| s.concept_synthesis).expect("found");
        assert_eq!(found.heading, "핵심");
        assert_eq!(found.body.trim(), "The durable record.");
    }

    /// Mid-switch a page carries BOTH spellings — the freshly rendered heading, empty, beside
    /// the one holding the answer. The answer wins whatever order the locales fall in, because
    /// the alternative is that the answer is the thing dropped.
    #[test]
    fn a_filled_section_outranks_an_empty_one_in_another_locale() {
        for page in [
            "# RAG\n\n## Synthesis\n\n## 핵심\n\nThe durable record.\n\n## 출처\n",
            "# RAG\n\n## 핵심\n\nThe durable record.\n\n## Synthesis\n\n## 출처\n",
        ] {
            let found = resolve_section(page, |s| s.concept_synthesis).expect("found");
            assert_eq!(found.heading, "핵심", "{page}");
            assert_eq!(found.body.trim(), "The durable record.");
        }
    }

    /// A section can be legitimately empty — an extraction that found nothing, a
    /// focus-filtered summary — and completion is marker-signalled, never inferred from a
    /// body. So the only spelling present answers even when it is blank; reporting the
    /// section absent would re-enqueue it forever.
    #[test]
    fn an_empty_section_is_still_the_answer_when_it_is_the_only_one() {
        let page = "# Daily\n\n## Related Concepts\n\n## Key Events\n\n- a\n";
        let found = resolve_section(page, |s| s.related_concepts).expect("found");
        assert_eq!(found.heading, "Related Concepts");
        assert!(found.body.trim().is_empty());

        assert!(resolve_section(page, |s| s.concept_sources).is_none());
    }

    /// One vocabulary, no repeats: a section whose spelling is shared across locales is
    /// offered once, so a caller matching headings itself never tests the same one twice.
    #[test]
    fn section_headings_lists_each_spelling_once() {
        let spellings: Vec<&str> = section_headings(|s| s.concept_synthesis).collect();
        assert!(spellings.contains(&"Synthesis"), "{spellings:?}");
        assert!(spellings.contains(&"핵심"), "{spellings:?}");
        let mut deduped = spellings.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), spellings.len(), "{spellings:?}");
    }

    /// Emptying a section leaves it spelled the way a template renders an unfilled one.
    /// The pipeline writes pages from templates and this rewrites sections of those pages,
    /// so a second spelling of "empty" would have each run undo the other's and every sweep
    /// report a page nothing changed about as changed.
    #[test]
    fn empty_body_produces_blank_section() {
        let doc = "## Sources\n\n- [old](old.md)\n\n## Meta\n";
        let out = replace_section(doc, "Sources", "");
        assert_eq!(out, "## Sources\n\n## Meta\n");
        assert_eq!(
            replace_section(&out, "Sources", ""),
            out,
            "and emptying an empty section is a no-op"
        );
    }

    #[test]
    fn missing_heading_returns_unchanged() {
        let doc = "# Title\n\n## Other\n\n- a\n";
        let out = replace_section(doc, "Sources", "- [new](new.md)");
        assert_eq!(out, doc);
    }

    #[test]
    fn does_not_match_partial_heading_prefix() {
        // `## Sourcesy` must not be treated as `## Sources`.
        let doc = "## Sourcesy\n\nbody\n\n## Sources\n\n- [old](old.md)\n";
        let out = replace_section(doc, "Sources", "- [new](new.md)");
        assert!(out.contains("## Sourcesy\n\nbody\n"));
        assert!(out.contains("- [new](new.md)"));
        assert!(!out.contains("- [old](old.md)"));
    }

    #[test]
    fn does_not_match_h3() {
        // `### Sources` is a different heading level and must be ignored.
        let doc = "### Sources\n\n- [keep](keep.md)\n";
        let out = replace_section(doc, "Sources", "- [new](new.md)");
        assert_eq!(out, doc);
    }

    #[test]
    fn preserves_frontmatter_and_other_sections() {
        let doc = "---\nid: x\n---\n\n# Title\n\n## Summary\n\nsumtext\n\n## Sources\n\n- [a](a.md)\n\n## Meta\n\nmeta\n";
        let out = replace_section(doc, "Sources", "- [b](b.md)\n- [c](c.md)");
        assert!(out.starts_with("---\nid: x\n---\n"));
        assert!(out.contains("## Summary\n\nsumtext\n"));
        assert!(out.contains("## Sources\n\n- [b](b.md)\n- [c](c.md)\n\n## Meta\n"));
    }

    #[test]
    fn code_fence_inside_section_body_does_not_end_section_early() {
        // A `## Sources` line inside a code fence must NOT terminate the section —
        // the real boundary is `## Meta` after the fence closes. The entire old
        // body (including the code fence) is replaced.
        let doc = "## Sources\n\n- [old](old.md)\n\n```\n## Sources (quoted)\n```\n\n## Meta\n";
        let out = replace_section(doc, "Sources", "- [new](new.md)");
        assert_eq!(out, "## Sources\n\n- [new](new.md)\n\n## Meta\n");
    }

    #[test]
    fn skips_target_heading_inside_fenced_code_block() {
        // The FIRST `## Sources` appears inside a fence — it must be skipped and
        // the second (real) one used.
        let doc = "```\n## Sources\nquoted\n```\n\n## Sources\n\n- [old](old.md)\n";
        let out = replace_section(doc, "Sources", "- [new](new.md)");
        assert_eq!(
            out,
            "```\n## Sources\nquoted\n```\n\n## Sources\n\n- [new](new.md)\n"
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
        let doc = "## Sources\n\n- [a](a.md)\n- [b](b.md)\n\n## Meta\n";
        let out = replace_section(doc, "Sources", "- [a](a.md)\n- [b](b.md)");
        assert_eq!(out, doc);
    }

    #[test]
    fn is_filled_true_for_real_content() {
        let doc = "## Summary\n\nReal content here.\n\n## Meta\n";
        assert!(is_section_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_false_for_missing_heading() {
        let doc = "## Other\n\nbody\n";
        assert!(!is_section_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_false_for_empty_body() {
        let doc = "## Summary\n\n## Meta\n";
        assert!(!is_section_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_false_for_whitespace_only_body() {
        let doc = "## Summary\n\n   \n\t\n\n## Meta\n";
        assert!(!is_section_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_false_when_heading_is_inside_fenced_code() {
        // The only `## Summary` is quoted code — there is no real section.
        let doc = "```\n## Summary\nquoted body\n```\n";
        assert!(!is_section_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_true_when_real_heading_follows_a_quoted_one() {
        let doc = "```\n## Summary\nquoted\n```\n\n## Summary\n\nreal body\n";
        assert!(is_section_filled(doc, "Summary"));
    }

    #[test]
    fn is_filled_true_at_end_of_file() {
        let doc = "# Title\n\n## Summary\n\nbody at EOF\n";
        assert!(is_section_filled(doc, "Summary"));
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
        let out = replace_section(doc, "Sources", "- [new](new.md)");
        assert_eq!(
            out, "## Sources\n\n- [new](new.md)\n\n## Meta\n\n- key: value\n",
            "outer quad fence content must be replaced wholesale, not split by inner triple"
        );
        // And `## Inner` must not be findable as a real section.
        assert!(!is_section_filled(doc, "Inner"));
    }

    #[test]
    fn tilde_fence_inside_backtick_fence_does_not_toggle() {
        // Mismatched marker characters must not close the open fence.
        let doc = "## Sources\n\n```\n~~~\n## Trap\n~~~\n```\n\n## Meta\n\n- v\n";
        let out = replace_section(doc, "Sources", "- [new](new.md)");
        assert!(out.contains("## Sources\n\n- [new](new.md)\n\n## Meta\n"));
        assert!(!is_section_filled(doc, "Trap"));
    }

    #[test]
    fn backtick_line_with_inline_backtick_is_not_a_fence() {
        // A backtick-fence info string can't contain a backtick (CommonMark), so this
        // line never opens a fence — and the `## B` after it stays a real section
        // boundary rather than being swallowed as fenced content.
        let doc = "## A\n\nintro\n\n```not`a`fence\n\n## B\n\nbody B\n";
        assert!(
            is_section_filled(doc, "B"),
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
        let out = replace_section(doc, "Sources", "- [new](new.md)");
        assert!(out.contains("## Sources\n\n- [new](new.md)\n\n## Meta\n"));
        assert!(!is_section_filled(doc, "Not a heading"));
    }

    #[test]
    fn body_returns_none_for_missing_heading() {
        let doc = "## Other\n\nbody\n";
        assert!(section_body(doc, "Summary").is_none());
    }
}

#[cfg(test)]
mod llm_input_tests {
    use super::set_llm_input;

    const PAGE: &str = "---\nid: d\ntype: daily\nllm_inputs:\n  summary: \"a\"\n  concepts: \"b\"\n---\n\n## Summary\n\nx\n";

    /// The marker as the pipeline stamps it — the caller serializes, as for
    /// `set_frontmatter_field`.
    fn stamp(content: &str, key: &str, hash: &str) -> Option<String> {
        set_llm_input(content, key, &serde_json::to_string(hash).unwrap())
    }

    #[test]
    fn replaces_an_existing_marker_in_place() {
        let out = stamp(PAGE, "concepts", "z").unwrap();
        assert!(out.contains("  concepts: \"z\""));
        assert!(
            out.contains("  summary: \"a\""),
            "siblings untouched: {out}"
        );
        assert!(out.contains("## Summary\n\nx\n"), "body untouched: {out}");
    }

    #[test]
    fn appends_a_marker_the_block_does_not_have_yet() {
        let out = stamp(PAGE, "concepts_done", "z").unwrap();
        assert!(out.contains("  concepts_done: \"z\""));
        assert!(out.contains("  concepts: \"b\""));
        let page = lk_core::frontmatter::parse_page(&out).unwrap();
        assert_eq!(
            page.frontmatter
                .get("llm_inputs")
                .and_then(|v| v.get("concepts_done"))
                .and_then(|v| v.as_str()),
            Some("z"),
            "the marker must be readable as frontmatter, not merely present: {out}"
        );
    }

    #[test]
    fn a_page_with_no_llm_inputs_block_is_refused() {
        // Inventing the block would claim the page is cached when nothing rendered it;
        // writing the marker anywhere else would leave the task re-enqueueing forever.
        let plain = "---\nid: d\n---\n\n## Summary\n\nx\n";
        assert_eq!(stamp(plain, "concepts_done", "z"), None);
    }

    #[test]
    fn a_flow_style_mapping_is_refused_rather_than_written_past() {
        // A user template may emit `llm_inputs` as a flow mapping. There is no child line
        // to join, so the marker cannot be placed — and a writer that fell through to the
        // end of the document would append it to the BODY, where `llm_cache` never reads
        // it: the task re-enqueues forever and each run appends another stray line.
        let flow = "---\nid: d\nllm_inputs: {summary: \"a\"}\n---\n\n## Summary\n\nx\n";
        assert_eq!(stamp(flow, "concepts_done", "z"), None);
    }

    #[test]
    fn a_body_llm_inputs_line_is_out_of_reach() {
        // The scan is bounded by the frontmatter block, so prose that happens to contain
        // the parent key is not a place to write into.
        let doc = "---\nid: d\n---\n\n## Notes\n\nllm_inputs:\n  summary: \"a\"\n";
        assert_eq!(stamp(doc, "concepts_done", "z"), None);
    }

    #[test]
    fn indentation_follows_the_block_it_joins() {
        let four = "---\nid: d\nllm_inputs:\n    summary: \"a\"\n---\n\nbody\n";
        let out = stamp(four, "concepts_done", "z").unwrap();
        assert!(out.contains("    concepts_done: \"z\""), "{out}");
    }

    #[test]
    fn a_sibling_key_sharing_a_prefix_is_not_overwritten() {
        let out = stamp(PAGE, "concepts_done", "z").unwrap();
        let out = stamp(&out, "concepts", "b2").unwrap();
        assert!(out.contains("  concepts: \"b2\""), "{out}");
        assert!(out.contains("  concepts_done: \"z\""), "{out}");
    }

    #[test]
    fn stamping_the_same_value_twice_is_idempotent() {
        let once = stamp(PAGE, "concepts_done", "z").unwrap();
        let twice = stamp(&once, "concepts_done", "z").unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn a_following_top_level_key_bounds_the_mapping() {
        let doc = "---\nllm_inputs:\n  summary: \"a\"\nsource_count: 3\n---\n\nbody\n";
        let out = stamp(doc, "concepts_done", "z").unwrap();
        assert_eq!(
            out,
            "---\nllm_inputs:\n  summary: \"a\"\n  concepts_done: \"z\"\nsource_count: 3\n---\n\nbody\n"
        );
    }

    #[test]
    fn a_mapping_with_no_children_yet_gets_one() {
        let doc = "---\nllm_inputs:\n---\n\nbody\n";
        let out = stamp(doc, "concepts_done", "z").unwrap();
        assert_eq!(
            out,
            "---\nllm_inputs:\n  concepts_done: \"z\"\n---\n\nbody\n"
        );
    }

    #[test]
    fn crlf_frontmatter_is_written_inside_the_block_and_keeps_its_terminators() {
        let doc = "---\r\nid: d\r\nllm_inputs:\r\n  summary: \"a\"\r\n---\r\n\r\nbody\r\n";
        let out = stamp(doc, "concepts_done", "z").unwrap();
        let page = lk_core::frontmatter::parse_page(&out).unwrap();
        assert_eq!(
            page.frontmatter
                .get("llm_inputs")
                .and_then(|v| v.get("concepts_done"))
                .and_then(|v| v.as_str()),
            Some("z"),
            "{out:?}"
        );
        assert!(
            !out.replace("\r\n", "").contains('\n'),
            "a CRLF page must not come back with a stray LF: {out:?}"
        );
    }

    #[test]
    fn a_blank_line_does_not_end_the_mapping() {
        // A blank line is legal inside a YAML block mapping. Treating it as the end would
        // write the key above the child below it — two copies in one mapping, and the
        // stale one wins on the next parse, silently undoing the write.
        let doc = "---\nid: d\nllm_inputs:\n  summary: \"a\"\n\n  concepts: \"b\"\n---\n\nbody\n";
        let out = stamp(doc, "concepts", "NEW").unwrap();
        assert_eq!(
            out.matches("concepts:").count(),
            1,
            "the key must be replaced, not duplicated: {out}"
        );
        let page = lk_core::frontmatter::parse_page(&out).unwrap();
        assert_eq!(
            page.frontmatter
                .get("llm_inputs")
                .and_then(|v| v.get("concepts"))
                .and_then(|v| v.as_str()),
            Some("NEW"),
            "{out}"
        );
    }

    #[test]
    fn a_comment_does_not_end_the_mapping() {
        // A `#` comment is legal at any column. Treating a column-0 one as the next
        // top-level key hides the child below it, and the duplicate that gets written
        // above loses to the stale value on the next parse.
        let doc =
            "---\nid: d\nllm_inputs:\n  summary: \"a\"\n# note\n  concepts: \"b\"\n---\n\nbody\n";
        let out = stamp(doc, "concepts", "NEW").unwrap();
        assert_eq!(
            out.matches("concepts:").count(),
            1,
            "the key must be replaced, not duplicated: {out}"
        );
        let page = lk_core::frontmatter::parse_page(&out).unwrap();
        assert_eq!(
            page.frontmatter
                .get("llm_inputs")
                .and_then(|v| v.get("concepts"))
                .and_then(|v| v.as_str()),
            Some("NEW"),
            "{out}"
        );
    }

    #[test]
    fn trailing_whitespace_after_the_parent_key_still_finds_the_mapping() {
        // Invisible and legal. Refusing here fails `queue apply` on every run until a
        // human notices a space.
        let doc = "---\nid: d\nllm_inputs: \n  summary: \"a\"\n---\n\nbody\n";
        let out = stamp(doc, "concepts_done", "z").expect("mapping must still be found");
        assert!(out.contains("  concepts_done: \"z\""), "{out}");
    }

    #[test]
    fn a_comment_indented_past_the_children_is_still_a_comment() {
        // Valid YAML, and a plausible human edit ("do not edit" notes go above the block).
        // Inferring the children's indentation from it puts the marker at the comment's
        // depth, which leaves a duplicate key and a page that no longer parses.
        let doc = "---\nid: d\nllm_inputs:\n      # written by lore; do not edit\n  summary: \"a\"\n  concepts: \"b\"\n---\n";
        let out = stamp(doc, "concepts", "NEW").unwrap();
        assert_eq!(
            out.matches("concepts:").count(),
            1,
            "must replace in place, not duplicate at the comment's indent: {out}"
        );
        let page = lk_core::frontmatter::parse_page(&out).expect("page must still parse");
        assert_eq!(
            page.frontmatter
                .get("llm_inputs")
                .and_then(|v| v.get("concepts"))
                .and_then(|v| v.as_str()),
            Some("NEW")
        );
        assert!(out.contains("      # written by lore"), "{out}");
    }

    #[test]
    fn a_comment_trailing_a_block_value_survives_its_replacement() {
        // The comment annotates the list, but it is user-authored text: deleting it as part
        // of rewriting the value is a silent loss. Only a block SCALAR carries `#` content.
        let doc =
            "---\naliases:\n  - RAG\n  # human note: keep RAG spelled out\nsource_count: 2\n---\n";
        let out = super::set_frontmatter_field(doc, "aliases", "[]").unwrap();
        assert_eq!(
            out,
            "---\naliases: []\n  # human note: keep RAG spelled out\nsource_count: 2\n---\n"
        );
    }

    #[test]
    fn an_anchor_between_the_colon_and_the_indicator_does_not_hide_it() {
        let doc = "---\nllm_inputs:\n  summary: &anch |\n    real\n    # tail\nid: d\n---\n";
        let out = stamp(doc, "concepts_done", "z").unwrap();
        assert_eq!(
            out,
            "---\nllm_inputs:\n  summary: &anch |\n    real\n    # tail\n  concepts_done: \"z\"\nid: d\n---\n"
        );
    }

    #[test]
    fn a_keyless_block_scalar_owns_its_hash_lines_too() {
        // `- |` has no colon to look behind, so a rule that reads the indicator after a key
        // skips it and its content lines fall back to being read as comments — the same
        // corruption, one construct over.
        let doc = "---\nllm_inputs:\n  summary:\n    - |\n      x\n      # y\nid: d\n---\n";
        let out = stamp(doc, "concepts_done", "z").unwrap();
        assert_eq!(
            out,
            "---\nllm_inputs:\n  summary:\n    - |\n      x\n      # y\n  concepts_done: \"z\"\nid: d\n---\n",
            "the new key joins after the list item's scalar, not inside it"
        );
    }

    #[test]
    fn a_block_scalar_child_with_no_sibling_after_it_still_owns_its_hash_lines() {
        // The span here is over `llm_inputs:`, which is NOT a block scalar — but its child
        // is. Reading the indicator off the entry the span belongs to answers for the parent
        // and gets the child wrong, so the mapping ends before the scalar's trailing `#`
        // lines: an insert lands INSIDE the value, and a replace strands them.
        let doc = "---\nllm_inputs:\n  summary: |\n    real\n    # hashline\nid: d\n---\n";

        let inserted = stamp(doc, "concepts_done", "z").unwrap();
        assert_eq!(
            inserted,
            "---\nllm_inputs:\n  summary: |\n    real\n    # hashline\n  concepts_done: \"z\"\nid: d\n---\n",
            "the new key joins after the whole scalar, not inside it"
        );

        let replaced = stamp(doc, "summary", "z").unwrap();
        assert_eq!(
            replaced, "---\nllm_inputs:\n  summary: \"z\"\nid: d\n---\n",
            "replacing the child takes every line of its scalar"
        );
    }

    #[test]
    fn a_hash_line_inside_a_block_scalar_is_value_not_comment() {
        // Indentation is what separates the two: a `#` line indented into the value is
        // content. Leaving it behind turns value text into a comment, or — on an insert —
        // splits the scalar around the new key.
        let doc =
            "---\nllm_inputs:\n  summary: |\n    real\n    # hashline\n  concepts: \"b\"\n---\n";
        let out = stamp(doc, "summary", "z").unwrap();
        assert_eq!(
            out,
            "---\nllm_inputs:\n  summary: \"z\"\n  concepts: \"b\"\n---\n"
        );
        let page = lk_core::frontmatter::parse_page(&out).unwrap();
        assert_eq!(
            page.frontmatter
                .get("llm_inputs")
                .and_then(|v| v.get("summary"))
                .and_then(|v| v.as_str()),
            Some("z"),
            "{out}"
        );
    }

    #[test]
    fn a_same_named_key_nested_deeper_is_not_the_marker() {
        // The marker lives one level under `llm_inputs`. A grandchild sharing its name must
        // not be matched first, or the real child stays stale and the task re-enqueues.
        let doc =
            "---\nllm_inputs:\n  nested:\n    concepts: \"deep\"\n  concepts: \"real\"\n---\n";
        let out = stamp(doc, "concepts", "NEW").unwrap();
        assert_eq!(
            out,
            "---\nllm_inputs:\n  nested:\n    concepts: \"deep\"\n  concepts: \"NEW\"\n---\n"
        );
    }

    #[test]
    fn replacing_a_child_takes_its_block_value_with_it() {
        // A child whose value spans lines is orphaned under the new inline one if only its
        // key line is replaced — the same corruption as the top-level block-list case, one
        // level down. A sibling at the same indent must still bound the span.
        let doc = "---\nllm_inputs:\n  summary: |\n    line one\n    line two\n  concepts: \"b\"\n---\n\nbody\n";
        let out = stamp(doc, "summary", "z").unwrap();
        assert_eq!(
            out,
            "---\nllm_inputs:\n  summary: \"z\"\n  concepts: \"b\"\n---\n\nbody\n"
        );
        let page = lk_core::frontmatter::parse_page(&out).expect("page must still parse");
        assert_eq!(
            page.frontmatter
                .get("llm_inputs")
                .and_then(|v| v.get("concepts"))
                .and_then(|v| v.as_str()),
            Some("b"),
            "the sibling must survive: {out}"
        );
    }

    #[test]
    fn a_new_key_joins_after_the_last_child_across_a_blank_line() {
        let doc = "---\nllm_inputs:\n  summary: \"a\"\n\n  concepts: \"b\"\nid: d\n---\n\nbody\n";
        let out = stamp(doc, "concepts_done", "z").unwrap();
        assert_eq!(
            out,
            "---\nllm_inputs:\n  summary: \"a\"\n\n  concepts: \"b\"\n  concepts_done: \"z\"\nid: d\n---\n\nbody\n"
        );
    }

    #[test]
    fn a_bom_is_preserved() {
        let doc = format!("\u{feff}{PAGE}");
        let out = stamp(&doc, "concepts_done", "z").unwrap();
        assert!(out.starts_with('\u{feff}'), "{out:?}");
        assert!(out.contains("  concepts_done: \"z\""));
    }
}
