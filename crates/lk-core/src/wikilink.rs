//! Obsidian `[[wikilink]]` extraction.
//!
//! One implementation of the wikilink rule for the whole workspace: the regex that
//! recognizes `[[target]]` / `[[target|display]]`, and the split that strips a
//! `#heading` or `^block` anchor off a target. `lk-graph` (and any future consumer)
//! reuse these instead of re-deriving the syntax.

use std::sync::LazyLock;

use regex::Regex;

use crate::markdown::FenceState;

/// Matches `[[target]]` and `[[target|display]]`. Capture group 1 is the raw target
/// (everything before an optional `|` and the closing `]]`).
pub static WIKILINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]+)?\]\]").unwrap());

/// Split a raw wikilink target into `(page, anchor)`, where `anchor` is a `#heading`
/// or `^block` suffix (including its leading marker) or the empty string. A target that
/// is anchor-only (e.g. `#section`) yields an empty page.
pub fn split_wikilink_target(target: &str) -> (&str, &str) {
    if let Some(pos) = target.find('#') {
        (&target[..pos], &target[pos..])
    } else if let Some(pos) = target.find('^') {
        (&target[..pos], &target[pos..])
    } else {
        (target, "")
    }
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
}
