//! Obsidian `[[wikilink]]` extraction.
//!
//! One implementation of the wikilink rule for the whole workspace: the regex that
//! recognizes `[[target]]` / `[[target|display]]`, and the split that strips a
//! `#heading` or `^block` anchor off a target. `lk-graph` (and any future consumer)
//! reuse these instead of re-deriving the syntax.

use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::markdown::FenceState;

/// Matches `[[target]]` and `[[target|display]]`. Capture group 1 is the raw target
/// (everything before an optional `|` and the closing `]]`).
pub static WIKILINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]+)?\]\]").unwrap());

/// Split a raw wikilink target into `(page, anchor)`, where `anchor` is a `#heading`
/// or `^block` suffix (including its leading marker) or the empty string. A target that
/// is anchor-only (e.g. `#section`) yields an empty page.
///
/// The target must be alias-free — [`WIKILINK_RE`] capture group 1 already excludes
/// the `|alias`, so this is correct for that caller. Any code that decomposes the
/// FULL inner text (`page#anchor|alias`) and then *reconstructs* the link must use
/// [`split_wikilink_parts`] so the anchor and alias are never conflated.
pub fn split_wikilink_target(target: &str) -> (&str, &str) {
    if let Some(pos) = target.find('#') {
        (&target[..pos], &target[pos..])
    } else if let Some(pos) = target.find('^') {
        (&target[..pos], &target[pos..])
    } else {
        (target, "")
    }
}

/// Decompose a wikilink's full inner text into `(page, anchor, alias)`, where `anchor`
/// keeps its leading `#`/`^` and `alias` keeps its leading `|` (both empty when
/// absent). Obsidian's grammar is `page[#anchor][|alias]` with the alias always last,
/// so the alias is split off FIRST and the anchor taken from the remainder — the only
/// correct way to rebuild a link without duplicating or dropping a component.
///
/// `Page#Sec|Label` → `("Page", "#Sec", "|Label")`, `Page|Label` →
/// `("Page", "", "|Label")`, `Page#Sec` → `("Page", "#Sec", "")`, `Page` →
/// `("Page", "", "")`.
pub fn split_wikilink_parts(inner: &str) -> (&str, &str, &str) {
    let (link, alias) = match inner.find('|') {
        Some(idx) => (&inner[..idx], &inner[idx..]),
        None => (inner, ""),
    };
    let (page, anchor) = split_wikilink_target(link);
    (page, anchor, alias)
}

/// Extract the page portion of every `[[wikilink]]` in `body`, with anchors stripped.
///
/// Empty / anchor-only targets are skipped. Targets are returned raw (not slugified);
/// callers normalize via [`crate::concept::slugify`] as needed. Order follows first
/// appearance; duplicates are preserved (callers dedup when they need to).
pub fn extract_wikilinks(body: &str) -> Vec<String> {
    let mut targets = Vec::new();
    // Block-fence tracking is shared with the rest of the workspace via `FenceState`
    // (CommonMark-correct: ≤3-space indent, marker char+length match to close, no
    // closing info string, backtick fences reject backtick-bearing info strings).
    // Only the inline code-span scan below is wikilink-specific.
    let mut fence = FenceState::new();

    for line in body.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        // Skip a line that opens/closes a fence OR sits inside an open block — only
        // un-fenced lines carry document-level wikilinks.
        let is_marker = fence.apply(stripped);
        if is_marker || !fence.is_closed() {
            continue;
        }

        extract_line_wikilinks(line, &mut targets);
    }

    targets
}

fn extract_line_wikilinks(line: &str, targets: &mut Vec<String>) {
    let bytes = line.as_bytes();
    let mut outside_start = 0;
    let mut cursor = 0;

    while cursor < bytes.len() {
        if bytes[cursor] != b'`' {
            cursor += 1;
            continue;
        }

        let run_len = count_repeated(bytes, cursor, b'`');
        let search_start = cursor + run_len;
        let Some(close_start) = find_matching_backtick_run(line, search_start, run_len) else {
            cursor = search_start;
            continue;
        };

        extract_segment_wikilinks(&line[outside_start..cursor], targets);
        cursor = close_start + run_len;
        outside_start = cursor;
    }

    extract_segment_wikilinks(&line[outside_start..], targets);
}

fn extract_segment_wikilinks(segment: &str, targets: &mut Vec<String>) {
    targets.extend(WIKILINK_RE.captures_iter(segment).filter_map(|cap| {
        let (page, _) = split_wikilink_target(cap.get(1).map_or("", |m| m.as_str()));
        if page.is_empty() {
            None
        } else {
            Some(page.to_owned())
        }
    }));
}

/// Rewrite wikilinks that sit OUTSIDE code (block fences AND inline spans), copying every
/// code region verbatim. Shares the exact fence/inline-skip logic with `extract_wikilinks`,
/// so the rule "a `[[link]]` shown inside code is not a graph edge" holds for rewrites too:
/// `graph normalize` / `graph merge` rewrite real citations but never mutate a wikilink a
/// page is merely SHOWING as a code example. `rewrite` is the standard `regex` replacement
/// closure — it receives the whole `[[…]]` capture and returns its replacement.
pub fn rewrite_wikilinks_outside_code(
    body: &str,
    mut rewrite: impl FnMut(&Captures) -> String,
) -> String {
    let mut out = String::with_capacity(body.len());
    let mut fence = FenceState::new();

    for line in body.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        // A fence marker line, or any line inside an open fence, is code — copy it verbatim.
        let is_marker = fence.apply(stripped);
        if is_marker || !fence.is_closed() {
            out.push_str(line);
            continue;
        }
        rewrite_line_wikilinks(line, &mut rewrite, &mut out);
    }

    out
}

fn rewrite_line_wikilinks(
    line: &str,
    rewrite: &mut impl FnMut(&Captures) -> String,
    out: &mut String,
) {
    let bytes = line.as_bytes();
    let mut outside_start = 0;
    let mut cursor = 0;

    while cursor < bytes.len() {
        if bytes[cursor] != b'`' {
            cursor += 1;
            continue;
        }

        let run_len = count_repeated(bytes, cursor, b'`');
        let search_start = cursor + run_len;
        let Some(close_start) = find_matching_backtick_run(line, search_start, run_len) else {
            cursor = search_start;
            continue;
        };

        // Rewrite the non-code segment before this span, then copy the inline code span
        // (backticks included) verbatim.
        out.push_str(&WIKILINK_RE.replace_all(&line[outside_start..cursor], &mut *rewrite));
        out.push_str(&line[cursor..close_start + run_len]);
        cursor = close_start + run_len;
        outside_start = cursor;
    }

    out.push_str(&WIKILINK_RE.replace_all(&line[outside_start..], &mut *rewrite));
}

fn find_matching_backtick_run(line: &str, start: usize, run_len: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut cursor = start;

    while cursor < bytes.len() {
        if bytes[cursor] != b'`' {
            cursor += 1;
            continue;
        }

        let candidate_len = count_repeated(bytes, cursor, b'`');
        if candidate_len == run_len {
            return Some(cursor);
        }

        cursor += candidate_len;
    }

    None
}

fn count_repeated(bytes: &[u8], start: usize, byte: u8) -> usize {
    bytes[start..].iter().take_while(|&&b| b == byte).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_targets_strips_display_and_anchors() {
        let text = "Link to [[Foo]] and [[Bar|display]] and [[Baz#heading]] and [[Qux^block]]";
        let targets = extract_wikilinks(text);
        assert_eq!(targets, vec!["Foo", "Bar", "Baz", "Qux"]);
    }

    #[test]
    fn extract_skips_anchor_only() {
        let text = "See [[#just-a-heading]] inline.";
        let targets = extract_wikilinks(text);
        assert!(targets.is_empty());
    }

    #[test]
    fn extract_skips_wikilinks_in_fenced_code_blocks() {
        let text = "Before\n```rust\nlet link = \"[[InsideFence]]\";\n```\nAfter";
        let targets = extract_wikilinks(text);
        assert!(targets.is_empty());
    }

    #[test]
    fn extract_skips_wikilinks_in_inline_code() {
        let text = "Before `[[InsideInline]]` after.";
        let targets = extract_wikilinks(text);
        assert!(targets.is_empty());
    }

    #[test]
    fn extract_keeps_wikilinks_outside_code() {
        let text = "Before [[Outside]] `[[InsideInline]]` after [[AlsoOutside]].";
        let targets = extract_wikilinks(text);
        assert_eq!(targets, vec!["Outside", "AlsoOutside"]);
    }

    #[test]
    fn extract_handles_mixed_documents() {
        let text = "\
[[Top]]

```text
[[InBacktickFence]]
```

Text `[[Inline]]` and [[Middle|display]].

   ~~~
[[InTildeFence]]
   ~~~

[[Bottom#anchor]]
";
        let targets = extract_wikilinks(text);
        assert_eq!(targets, vec!["Top", "Middle", "Bottom"]);
    }

    #[test]
    fn extract_treats_nested_fence_types_as_content() {
        let text = "\
~~~text
[[InTildeFence]]
```
[[StillInTildeFence]]
~~~
[[Outside]]
";
        let targets = extract_wikilinks(text);
        assert_eq!(targets, vec!["Outside"]);
    }

    #[test]
    fn extract_ignores_info_string_as_closing_fence() {
        let text = "\
```rust
[[InBlock]]
```rust
[[StillInBlock]]
```
[[Outside]]
";
        let targets = extract_wikilinks(text);
        assert_eq!(targets, vec!["Outside"]);
    }

    #[test]
    fn backtick_fence_with_backtick_in_info_is_not_a_fence() {
        // CommonMark: a backtick fence's info string must not contain a backtick, so
        // this opens no block — the following wikilink is document content.
        // (The previous bespoke parser treated it as a fence and dropped the link.)
        let text = "```not`a`fence\n[[Live]]\n";
        assert_eq!(extract_wikilinks(text), vec!["Live"]);
    }

    #[test]
    fn over_indented_fence_is_not_a_fence() {
        // 4+ leading spaces is an indented code block, not a fence opener — the marker
        // line and the following line are both treated as content.
        let text = "    ```\n[[Live]]\n";
        assert_eq!(extract_wikilinks(text), vec!["Live"]);
    }

    #[test]
    fn tab_indented_fence_is_not_a_fence() {
        // Leading tab (not a space) is not the ≤3-space fence indent CommonMark allows,
        // so this is not a fence opener.
        let text = "\t```\n[[Live]]\n";
        assert_eq!(extract_wikilinks(text), vec!["Live"]);
    }

    #[test]
    fn wikilink_regex_capture() {
        let text = "Link to [[Foo]] and [[Bar|display]] and [[Baz]]";
        let targets: Vec<&str> = WIKILINK_RE
            .captures_iter(text)
            .map(|c| c.get(1).unwrap().as_str())
            .collect();
        assert_eq!(targets, vec!["Foo", "Bar", "Baz"]);
    }

    #[test]
    fn split_target_heading() {
        assert_eq!(split_wikilink_target("Page#Heading"), ("Page", "#Heading"));
        assert_eq!(split_wikilink_target("Page#"), ("Page", "#"));
    }

    #[test]
    fn split_target_block_ref() {
        assert_eq!(
            split_wikilink_target("Page^block-id"),
            ("Page", "^block-id")
        );
    }

    #[test]
    fn split_target_plain() {
        assert_eq!(split_wikilink_target("plain-page"), ("plain-page", ""));
    }

    #[test]
    fn split_target_anchor_only() {
        assert_eq!(
            split_wikilink_target("#heading-only"),
            ("", "#heading-only")
        );
    }
    #[test]
    fn split_parts_separates_page_anchor_alias() {
        assert_eq!(split_wikilink_parts("Page"), ("Page", "", ""));
        assert_eq!(
            split_wikilink_parts("Page#Sec|My Alias"),
            ("Page", "#Sec", "|My Alias")
        );
        assert_eq!(
            split_wikilink_parts("Page^blk|My Alias"),
            ("Page", "^blk", "|My Alias")
        );
        assert_eq!(split_wikilink_parts("Page|Label"), ("Page", "", "|Label"));
    }

    // Uppercase every wikilink target — a stand-in for the real rewrites
    // (normalize/merge) so the test asserts the fence/inline-skip rule itself.
    fn upcase_targets(body: &str) -> String {
        rewrite_wikilinks_outside_code(body, |caps: &Captures| {
            format!("[[{}]]", caps[1].to_uppercase())
        })
    }

    #[test]
    fn rewrite_outside_code_leaves_fenced_and_inline_code_verbatim() {
        let body = "\
Real [[foo]] link.
```
code [[foo]] example
```
Inline `[[foo]]` span and another [[bar]].
";
        let out = upcase_targets(body);
        // Outside code → rewritten.
        assert!(out.contains("Real [[FOO]] link."));
        assert!(out.contains("another [[BAR]]."));
        // Inside the fence and the inline span → untouched.
        assert!(out.contains("code [[foo]] example"));
        assert!(out.contains("Inline `[[foo]]` span"));
    }

    #[test]
    fn rewrite_outside_code_is_identity_when_closure_is() {
        // A no-op closure must round-trip the document byte-for-byte (incl. code regions).
        let body = "A [[x]].\n```\n[[y]]\n```\nB `[[z]]` C [[w]].\n";
        let out = rewrite_wikilinks_outside_code(body, |caps: &Captures| caps[0].to_owned());
        assert_eq!(out, body);
    }
}
