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

/// Split a link's RAW `(dest)` capture into its page path and anchor. CommonMark
/// permits a quoted title after the destination, so the destination proper ends at
/// the first whitespace. The one normalization every consumer of a raw destination
/// goes through — extraction and the merge/normalize rewriters — so they can never
/// disagree about which page a link addresses.
pub fn split_raw_dest(raw: &str) -> (&str, &str) {
    let dest = raw.split_whitespace().next().unwrap_or("");
    split_dest_anchor(dest)
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
/// link's `(dest)` syntax (space, parens), `%` itself, and `#` (which extraction
/// would otherwise read as an anchor separator, truncating the path). Everything
/// else — notably non-ASCII slugs — passes through verbatim, as CommonMark permits.
pub fn encode_dest(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '%' => out.push_str("%25"),
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '#' => out.push_str("%23"),
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
/// page's directory. Destinations are `/`-separated (a backslash is a literal path
/// character, as in CommonMark and POSIX). Purely lexical (`.` and `..` are folded);
/// `None` when the destination escapes the vault root — such a link can't address a
/// vault page.
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

/// An internal page link read back out of a body: the display text with [`md_link`]'s
/// escaping resolved, and the destination decoded with its anchor stripped. Both halves
/// are in the form [`md_link`] accepts, so a link that is read and re-rendered
/// reproduces itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageLink {
    pub text: String,
    pub dest: String,
}

/// Extract every internal inline page link in `body`, in first-appearance order
/// (duplicates preserved — callers dedup as needed). Skips links inside fenced code
/// blocks and inline code spans, image embeds, external destinations, and anchor-only
/// links.
///
/// The text-carrying counterpart of [`extract_dests`], for a consumer that must keep a
/// link it read rather than merely observe where it points — re-deriving the display
/// name instead would rename someone else's link.
pub fn extract_page_links(body: &str) -> Vec<PageLink> {
    let mut links = Vec::new();
    let mut fence = FenceState::new();

    for line in body.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        let is_marker = fence.apply(stripped);
        if is_marker || !fence.is_closed() {
            continue;
        }
        for_each_code_free_segment(line, |segment| {
            for cap in MD_LINK_RE.captures_iter(segment) {
                if let Some(link) = internal_page_link(&cap) {
                    links.push(link);
                }
            }
        });
    }

    links
}

/// Extract the decoded page destination of every internal inline link in `body`,
/// anchors stripped, in first-appearance order (duplicates preserved — callers dedup
/// as needed). Skips links inside fenced code blocks and inline code spans, image
/// embeds, external destinations, and anchor-only links.
pub fn extract_dests(body: &str) -> Vec<String> {
    extract_page_links(body)
        .into_iter()
        .map(|link| link.dest)
        .collect()
}

/// A captured link as a [`PageLink`] — `None` for image embeds and
/// external/empty/anchor-only destinations.
fn internal_page_link(cap: &Captures) -> Option<PageLink> {
    if !cap[1].is_empty() {
        return None; // image embed
    }
    let (page, _anchor) = split_raw_dest(&cap[3]);
    if page.is_empty() || is_external(page) {
        return None;
    }
    Some(PageLink {
        text: unescape_text(&cap[2]),
        dest: decode_dest(page),
    })
}

/// The inverse of the display-text escaping [`md_link`] applies.
fn unescape_text(text: &str) -> String {
    text.replace("\\[", "[")
        .replace("\\]", "]")
        .replace("\\\\", "\\")
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
        unescape_text(&cap[2])
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

    /// A link read back out and re-rendered must reproduce itself — otherwise a consumer
    /// that carries someone else's link forward renames it a little on every pass.
    #[test]
    fn a_page_link_survives_being_read_and_written_again() {
        for name in [
            r"RAG [retrieval]",
            r"C:\path",
            "Wisely — 지식베이스",
            "Claude 3.5",
        ] {
            let dest = relative_dest(
                Path::new("daily/ai-news/2026-07-15.md"),
                Path::new("wiki/concepts/x y.md"),
            );
            let rendered = md_link(name, &dest);
            let read = extract_page_links(&rendered);
            assert_eq!(
                read,
                vec![PageLink {
                    text: name.to_string(),
                    dest: decode_dest(&dest),
                }],
                "reading {rendered} back"
            );
            assert_eq!(
                md_link(&read[0].text, &encode_dest(&read[0].dest)),
                rendered,
                "re-rendering {rendered}"
            );
        }
    }

    /// `extract_dests` is defined on top of the pair extractor, so the two can never
    /// disagree about which links a body carries.
    #[test]
    fn page_links_and_dests_report_the_same_links() {
        let text = "\
[A](../wiki/concepts/a.md) and `[code](no.md)` and ![img](i.png)
[ext](https://x.y/z.md) and [B](b.md#sec)
```
[fenced](f.md)
```
";
        assert_eq!(
            extract_page_links(text)
                .into_iter()
                .map(|l| l.dest)
                .collect::<Vec<_>>(),
            extract_dests(text)
        );
        assert_eq!(extract_dests(text), vec!["../wiki/concepts/a.md", "b.md"]);
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

        // The answer decides whether a destination is a vault page at all, so it follows
        // RFC 3986's scheme grammar exactly rather than looking for a colon. A scheme starts
        // ALPHA — a destination beginning with anything else has none, however it continues.
        assert!(!is_external("2026:notes.md"));
        assert!(!is_external("-tricky:x"));
        assert!(!is_external(".hidden:x"));
        // …and continues ALPHA / DIGIT / `+` `-` `.` only. A colon reached over any other
        // character is part of a path, and calling that path a scheme would hide the link
        // from extraction and from every rewriter, silently dropping the citation.
        assert!(!is_external("../wiki/con cepts:a.md"));
        assert!(!is_external("wiki/concepts/a:b.md"));
        assert!(is_external("h2+x.y-z:payload"));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let path = "wiki/my dir/mo(e) 100%.md";
        let encoded = encode_dest(path);
        assert_eq!(encoded, "wiki/my%20dir/mo%28e%29%20100%25.md");
        assert_eq!(decode_dest(&encoded), path);
        // A lone `%` stays literal.
        assert_eq!(decode_dest("100%.md"), "100%.md");
        // `#` is encoded so a path containing it survives the anchor split.
        let hashy = "wiki/a#b.md";
        let encoded = encode_dest(hashy);
        assert_eq!(encoded, "wiki/a%23b.md");
        assert_eq!(split_raw_dest(&encoded), ("wiki/a%23b.md", ""));
        assert_eq!(decode_dest(&encoded), hashy);
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

    /// A code span is closed by a backtick run of the SAME length, which is how CommonMark
    /// lets a span contain backticks of its own. The rewriters that use this are `merge` and
    /// `normalize` — both repoint links across the whole vault in place — so a run whose end
    /// is misread hands them a slice of quoted example markdown as if it were live text, and
    /// the page's own illustration of a link silently becomes a different link.
    #[test]
    fn a_code_span_ends_only_on_a_backtick_run_of_its_own_length() {
        let body = "\
Live [Old](../concepts/old.md).
Quoted ``a ` b [Old](../concepts/old.md)`` then live [Old](../concepts/old.md).
Triple ```x ` y `` z [Old](../concepts/old.md)``` done.
";
        let out = rewrite_links_outside_code(body, |text, dest| {
            (dest == "../concepts/old.md").then(|| md_link(text, "../concepts/new.md"))
        });
        assert_eq!(
            out.matches("../concepts/new.md").count(),
            2,
            "only the two live links are repointed:\n{out}"
        );
        assert!(
            out.contains("``a ` b [Old](../concepts/old.md)``"),
            "double-backtick span untouched:\n{out}"
        );
        assert!(
            out.contains("```x ` y `` z [Old](../concepts/old.md)```"),
            "triple-backtick span untouched:\n{out}"
        );
    }

    #[test]
    fn rewrite_is_identity_when_closure_declines() {
        let body = "A [x](x.md).\n```\n[y](y.md)\n```\nB `[z](z.md)` C [w](w.md).\n";
        let out = rewrite_links_outside_code(body, |_, _| None);
        assert_eq!(out, body);
    }

    #[test]
    fn split_raw_dest_drops_commonmark_title_before_anchor_split() {
        // The title must be dropped BEFORE the anchor split, or the anchor would
        // swallow the title text.
        assert_eq!(split_raw_dest("a.md \"tip\""), ("a.md", ""));
        assert_eq!(split_raw_dest("a.md#sec \"tip\""), ("a.md", "#sec"));
        assert_eq!(split_raw_dest("a.md"), ("a.md", ""));
        assert_eq!(split_raw_dest(""), ("", ""));
    }

    #[test]
    fn split_dest_anchor_basic() {
        assert_eq!(split_dest_anchor("a.md#Sec"), ("a.md", "#Sec"));
        assert_eq!(split_dest_anchor("a.md"), ("a.md", ""));
        assert_eq!(split_dest_anchor("#only"), ("", "#only"));
    }
}
