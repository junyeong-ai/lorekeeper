//! Obsidian `[[wikilink]]` extraction.
//!
//! One implementation of the wikilink rule for the whole workspace: the regex that
//! recognizes `[[target]]` / `[[target|display]]`, and the split that strips a
//! `#heading` or `^block` anchor off a target. `lk-graph` (and any future consumer)
//! reuse these instead of re-deriving the syntax.

use std::sync::LazyLock;

use regex::Regex;

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
pub fn extract_wikilinks(body: &str) -> impl Iterator<Item = &str> {
    WIKILINK_RE.captures_iter(body).filter_map(|cap| {
        let (page, _) = split_wikilink_target(cap.get(1).map_or("", |m| m.as_str()));
        if page.is_empty() { None } else { Some(page) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_targets_strips_display_and_anchors() {
        let text = "Link to [[Foo]] and [[Bar|display]] and [[Baz#heading]] and [[Qux^block]]";
        let targets: Vec<&str> = extract_wikilinks(text).collect();
        assert_eq!(targets, vec!["Foo", "Bar", "Baz", "Qux"]);
    }

    #[test]
    fn extract_skips_anchor_only() {
        let text = "See [[#just-a-heading]] inline.";
        let targets: Vec<&str> = extract_wikilinks(text).collect();
        assert!(targets.is_empty());
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
