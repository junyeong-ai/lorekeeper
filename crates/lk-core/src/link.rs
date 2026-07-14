//! Standard markdown link handling — the single implementation of the vault's link
//! vocabulary for the whole workspace.
//!
//! Every internal reference in the vault is an inline markdown link
//! `[display](relative/path.md)` whose destination is relative to the containing
//! page's directory (the one form that resolves identically in Obsidian, GitHub, and
//! any OKF consumer). This module owns all four halves of that contract:
//!
//! - **construction**: [`md_link`] + [`relative_dest`] (destination percent-encoding
//!   included) — every Rust emitter builds links through these;
//! - **extraction**: [`extract_dests`] — fence- and inline-code-aware, images and
//!   external destinations excluded — the one parser `lk-graph` builds edges from;
//! - **rewriting**: [`rewrite_links_outside_code`] — merge/normalize repoint
//!   destinations without touching code regions;
//! - **resolution**: [`resolve_dest`] — pure lexical resolution of a destination
//!   against its page's location, clamped to the vault root.
//!
//! Inline links are single-line (the emitters never span lines; a multi-line link is
//! plain text to this parser). Link text with literal `[`/`]`/`\` is escaped by
//! [`md_link`] and matched by the shared regex.

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::markdown::FenceState;

/// Matches an inline markdown link (or image embed): optional `!`, `[text](dest)`.
/// Group 1 = the `!` of an image embed (empty for links), group 2 = the link text
/// (backslash escapes allowed), group 3 = the raw destination (up to the closing
/// paren — literal parens in destinations are percent-encoded by [`encode_dest`]).
static MD_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(!?)\[((?:[^\]\\\n]|\\.)*)\]\(([^)\n]*)\)").unwrap());

/// Split a raw destination into `(page, anchor)` at the first `#`; the anchor keeps
/// its leading `#` and is the empty string when absent.
pub fn split_dest_anchor(dest: &str) -> (&str, &str) {
    match dest.find('#') {
        Some(pos) => (&dest[..pos], &dest[pos..]),
        None => (dest, ""),
    }
}

/// Whether a destination is external — it carries an RFC 3986 scheme
/// (`https:`, `mailto:`, …). External destinations are never vault pages, so they are
/// invisible to extraction and rewriting.
pub fn is_external(dest: &str) -> bool {
    let mut chars = dest.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for c in chars {
        match c {
            ':' => return true,
            c if c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-') => {}
            _ => return false,
        }
    }
    false
}

/// Percent-encode a destination path: the characters that would break an inline
/// link's `(dest)` syntax (space, parens) plus `%` itself. Everything else — notably
/// non-ASCII slugs — passes through verbatim, as CommonMark permits.
pub fn encode_dest(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '%' => out.push_str("%25"),
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            c => out.push(c),
        }
    }
    out
}

/// Percent-decode a destination. Only valid `%XX` hex pairs are decoded (a lone `%`
/// stays literal, per CommonMark); a sequence that decodes to invalid UTF-8 returns
/// the input unchanged — such a destination simply fails to resolve, which the graph
/// surfaces as a broken link.
pub fn decode_dest(dest: &str) -> String {
    let bytes = dest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| dest.to_owned())
}

/// Render an inline link, escaping `[`/`]`/`\` in the display text. `dest` is taken
/// as already encoded (the output of [`relative_dest`] or [`encode_dest`]).
pub fn md_link(text: &str, dest: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '[' | ']' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    format!("[{escaped}]({dest})")
}

/// The encoded relative destination from `from_page` to `to_page` — both
/// vault-relative page paths (with extension). This is THE address form every
/// emitter writes; it resolves back through [`resolve_dest`].
pub fn relative_dest(from_page: &Path, to_page: &Path) -> String {
    let from_dir: Vec<Component> = from_page
        .parent()
        .unwrap_or(Path::new(""))
        .components()
        .collect();
    let to: Vec<Component> = to_page.components().collect();
    let common = from_dir
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<String> = vec!["..".to_owned(); from_dir.len() - common];
    parts.extend(
        to[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    encode_dest(&parts.join("/"))
}

/// Resolve a DECODED, anchor-free destination against the page that contains it,
/// returning the vault-relative path it addresses. A `/`-leading destination is
/// vault-root-relative (the OKF absolute form); anything else is relative to the
/// page's directory. Purely lexical (`.` and `..` are folded); `None` when the
/// destination escapes the vault root — such a link can't address a vault page.
pub fn resolve_dest(from_page: &Path, dest: &str) -> Option<PathBuf> {
    let (base, dest) = match dest.strip_prefix('/') {
        Some(rooted) => (Path::new(""), rooted),
        None => (from_page.parent().unwrap_or(Path::new("")), dest),
    };
    let mut resolved: Vec<Component> = base.components().collect();
    for comp in Path::new(dest).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop()?;
            }
            Component::Normal(_) => resolved.push(comp),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(resolved.iter().collect())
}

/// Extract the decoded page destination of every internal inline link in `body`,
/// anchors stripped, in first-appearance order (duplicates preserved — callers dedup
/// as needed). Skips links inside fenced code blocks and inline code spans, image
/// embeds, external destinations, and anchor-only links.
pub fn extract_dests(body: &str) -> Vec<String> {
    let mut dests = Vec::new();
    let mut fence = FenceState::new();

    for line in body.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let is_marker = fence.apply(stripped);
        if is_marker || !fence.is_closed() {
            continue;
        }
        for_each_code_free_segment(line, |segment| {
            for cap in MD_LINK_RE.captures_iter(segment) {
                if let Some(dest) = internal_page_dest(&cap) {
                    dests.push(dest);
                }
            }
        });
    }

    dests
}

/// The decoded, anchor-free page destination of a captured link — `None` for image
/// embeds, external/empty/anchor-only destinations.
fn internal_page_dest(cap: &Captures) -> Option<String> {
    if !cap[1].is_empty() {
        return None; // image embed
    }
    // CommonMark permits a quoted title after the destination; the page part ends at
    // the first whitespace.
    let raw = cap[3].split_whitespace().next().unwrap_or("");
    let (page, _anchor) = split_dest_anchor(raw);
    if page.is_empty() || is_external(page) {
        return None;
    }
    Some(decode_dest(page))
}

/// Rewrite inline links that sit OUTSIDE code (block fences AND inline spans),
/// copying every code region — and every image embed — verbatim. `rewrite` receives
/// each link's `(text, raw dest)` — both exactly as written, escapes and encoding
/// included — and returns the full replacement link, or `None` to keep the original.
/// A rewriter that only repoints the destination must reassemble with the text
/// verbatim (`format!("[{text}]({new_dest})")`), NOT through [`md_link`], which
/// would escape it a second time. The counterpart of [`extract_dests`]: the exact
/// same links form graph edges and are eligible for rewriting, so `graph
/// merge`/`normalize` repoint real citations but never mutate a link a page merely
/// shows as code.
pub fn rewrite_links_outside_code(
    body: &str,
    mut rewrite: impl FnMut(&str, &str) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(body.len());
    let mut fence = FenceState::new();

    for line in body.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let is_marker = fence.apply(stripped);
        if is_marker || !fence.is_closed() {
            out.push_str(line);
            continue;
        }
        for_each_line_region(line, &mut out, |segment, out| {
            out.push_str(&MD_LINK_RE.replace_all(segment, |cap: &Captures| {
                let full = cap.get(0).unwrap().as_str();
                if !cap[1].is_empty() {
                    return full.to_owned();
                }
                rewrite(&cap[2], &cap[3]).unwrap_or_else(|| full.to_owned())
            }));
        });
    }

    out
}

/// Replace every non-image inline link with its display text (unescaped) — used
/// where prose must render as plain text on a single catalog line.
pub fn strip_links(text: &str) -> String {
    let stripped = MD_LINK_RE.replace_all(text, |cap: &Captures| {
        if !cap[1].is_empty() {
            return cap[0].to_owned();
        }
        cap[2]
            .replace("\\[", "[")
            .replace("\\]", "]")
            .replace("\\\\", "\\")
    });
    stripped.into_owned()
}

/// Invoke `f` on each segment of `line` that lies outside inline code spans.
fn for_each_code_free_segment(line: &str, mut f: impl FnMut(&str)) {
    for_each_line_region(line, &mut String::new(), |segment, _| f(segment));
}

/// Walk `line` splitting it at inline code spans: non-code segments go through `f`
/// (which may transform them into `out`); code spans (backticks included) are copied
/// to `out` verbatim. Shares the backtick-run matching rule with extraction so the
/// two can never disagree about what is code.
fn for_each_line_region(line: &str, out: &mut String, mut f: impl FnMut(&str, &mut String)) {
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
        f(&line[outside_start..cursor], out);
        out.push_str(&line[cursor..close_start + run_len]);
        cursor = close_start + run_len;
        outside_start = cursor;
    }

    f(&line[outside_start..], out);
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
    fn extract_basic_and_order() {
        let text = "See [A](../wiki/concepts/a.md) then [B](b.md#sec) and [A again](../wiki/concepts/a.md).";
        assert_eq!(
            extract_dests(text),
            vec!["../wiki/concepts/a.md", "b.md", "../wiki/concepts/a.md"]
        );
    }

    #[test]
    fn extract_skips_external_images_and_anchor_only() {
        let text = "\
[ext](https://example.com/x.md) and [mail](mailto:a@b.c)
![embed](../img.png) and [anchor](#section) and [ok](./x.md)
";
        assert_eq!(extract_dests(text), vec!["./x.md"]);
    }

    #[test]
    fn extract_decodes_percent_encoding() {
        let text = "[T](../my%20dir/some%28x%29.md)";
        assert_eq!(extract_dests(text), vec!["../my dir/some(x).md"]);
    }

    #[test]
    fn extract_drops_commonmark_title() {
        let text = "[T](x.md \"a title\")";
        assert_eq!(extract_dests(text), vec!["x.md"]);
    }

    #[test]
    fn extract_skips_fenced_and_inline_code() {
        let text = "\
[live](a.md)
```
[fenced](b.md)
```
Inline `[span](c.md)` and [live2](d.md).
";
        assert_eq!(extract_dests(text), vec!["a.md", "d.md"]);
    }

    #[test]
    fn extract_handles_escaped_brackets_in_text() {
        let text = r"[RAG \[retrieval\]](rag.md)";
        assert_eq!(extract_dests(text), vec!["rag.md"]);
    }

    #[test]
    fn external_scheme_detection() {
        assert!(is_external("https://x.y/z.md"));
        assert!(is_external("mailto:a@b.c"));
        assert!(is_external("obsidian://open"));
        assert!(!is_external("../concepts/a.md"));
        assert!(!is_external("concepts/a.md"));
        // A Korean-slug destination has no scheme.
        assert!(!is_external("개념/에이전트.md"));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let path = "wiki/my dir/mo(e) 100%.md";
        let encoded = encode_dest(path);
        assert_eq!(encoded, "wiki/my%20dir/mo%28e%29%20100%25.md");
        assert_eq!(decode_dest(&encoded), path);
        // A lone `%` stays literal.
        assert_eq!(decode_dest("100%.md"), "100%.md");
    }

    #[test]
    fn md_link_escapes_text() {
        assert_eq!(
            md_link("RAG [retrieval]", "rag.md"),
            r"[RAG \[retrieval\]](rag.md)"
        );
        assert_eq!(md_link("plain", "a/b.md"), "[plain](a/b.md)");
    }

    #[test]
    fn strip_links_keeps_display_text() {
        assert_eq!(
            strip_links("See [A](a.md) and [B](../b.md#x), not ![img](i.png)."),
            "See A and B, not ![img](i.png)."
        );
        assert_eq!(strip_links(r"[RAG \[r\]](x.md)"), "RAG [r]");
    }

    #[test]
    fn relative_dest_computes_updir_prefix() {
        let daily = Path::new("daily/team-slack/2026-05-22.md");
        let concept = Path::new("wiki/concepts/kubernetes.md");
        assert_eq!(
            relative_dest(daily, concept),
            "../../wiki/concepts/kubernetes.md"
        );
        // Same directory → bare filename.
        let a = Path::new("wiki/concepts/a.md");
        let b = Path::new("wiki/concepts/b.md");
        assert_eq!(relative_dest(a, b), "b.md");
        // From the wiki root file into a subdir.
        let index = Path::new("wiki/index.md");
        assert_eq!(relative_dest(index, concept), "concepts/kubernetes.md");
        // Encoding applies to the joined path.
        let spaced = Path::new("wiki/concepts/a b.md");
        assert_eq!(relative_dest(index, spaced), "concepts/a%20b.md");
    }

    #[test]
    fn resolve_dest_roundtrips_relative_dest() {
        let from = Path::new("daily/team-slack/2026-05-22.md");
        let to = Path::new("wiki/concepts/kubernetes.md");
        let dest = relative_dest(from, to);
        assert_eq!(
            resolve_dest(from, &decode_dest(&dest)),
            Some(to.to_path_buf())
        );
    }

    #[test]
    fn resolve_dest_accepts_okf_absolute_form() {
        let from = Path::new("daily/x/2026-01-01.md");
        assert_eq!(
            resolve_dest(from, "/wiki/concepts/a.md"),
            Some(PathBuf::from("wiki/concepts/a.md"))
        );
    }

    #[test]
    fn resolve_dest_rejects_escape_and_folds_dots() {
        let from = Path::new("wiki/index.md");
        assert_eq!(resolve_dest(from, "../../etc/passwd"), None);
        assert_eq!(
            resolve_dest(from, "./concepts/../concepts/a.md"),
            Some(PathBuf::from("wiki/concepts/a.md"))
        );
    }

    #[test]
    fn rewrite_repoints_dest_and_leaves_code_and_images() {
        let body = "\
Cites [Old](../concepts/old.md).
```
code [Old](../concepts/old.md)
```
Inline `[Old](../concepts/old.md)` and ![img](old.png).
";
        let out = rewrite_links_outside_code(body, |text, dest| {
            (dest == "../concepts/old.md").then(|| md_link(text, "../concepts/new.md"))
        });
        assert!(out.contains("Cites [Old](../concepts/new.md)."));
        assert!(out.contains("code [Old](../concepts/old.md)"));
        assert!(out.contains("Inline `[Old](../concepts/old.md)`"));
        assert!(out.contains("![img](old.png)"));
    }

    #[test]
    fn rewrite_is_identity_when_closure_declines() {
        let body = "A [x](x.md).\n```\n[y](y.md)\n```\nB `[z](z.md)` C [w](w.md).\n";
        let out = rewrite_links_outside_code(body, |_, _| None);
        assert_eq!(out, body);
    }

    #[test]
    fn split_dest_anchor_basic() {
        assert_eq!(split_dest_anchor("a.md#Sec"), ("a.md", "#Sec"));
        assert_eq!(split_dest_anchor("a.md"), ("a.md", ""));
        assert_eq!(split_dest_anchor("#only"), ("", "#only"));
    }
}
