//! Normalize source-specific rich text into standard Markdown so downstream LLM steps and
//! the Obsidian vault receive clean, AI-friendly input instead of ADF JSON, raw HTML, or
//! Slack's `<…>` token soup. Conversions are loss-averse: any construct without a Markdown
//! equivalent degrades to its text content rather than being dropped.

use std::collections::HashMap;

use lk_core::text::collapse_blank_lines;
use serde_json::Value;

/// Convert an HTML fragment to Markdown via `htmd`. `htmd::convert` reads from an
/// in-memory string, whose only fallible step is a `Read` that cannot fail, and HTML5
/// parsing always recovers from malformed markup — so conversion is infallible here; the
/// `Result` collapses to an empty string for the unreachable error case.
pub fn html_to_markdown(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }
    // Dash bullets to match the ADF converter's list style (one consistent Markdown
    // dialect across all sources).
    let converter = htmd::HtmlToMarkdown::builder()
        .options(htmd::options::Options {
            bullet_list_marker: htmd::options::BulletListMarker::Dash,
            ..Default::default()
        })
        .add_handler(vec!["img"], img_without_data_uris)
        .add_handler(MACHINE_STATE_ELEMENTS.to_vec(), drop_element)
        .add_handler(vec!["ri:page", "ri:attachment"], resource_label)
        .build();
    converter
        .convert(&normalize_storage_format(html))
        .map(|md| md.trim().to_string())
        .unwrap_or_default()
}

/// Elements whose TEXT is machine state rather than prose, so the loss-averse rule above —
/// degrade an unmapped construct to its text — is wrong for them.
///
/// Confluence storage format is XHTML. A macro's `<ac:parameter>` children hold its
/// settings (a status macro's colour, a roadmap's base64 state blob) and a task's
/// `<ac:task-id>`/`<ac:task-uuid>`/`<ac:task-status>` hold its identity. Degraded, they are
/// emitted INLINE and unseparated: a page reads `검증JTdCJTIybmFtZSUyMi…` or
/// `170e6f1a-9cincompleteShip the thing`. One real page came out 30% encoded settings.
///
/// This is a deliberate trade, not a free win. A macro WITH a body loses nothing — the body
/// is `ac:rich-text-body`/`ac:plain-text-body` and is untouched. A macro WITHOUT one loses
/// its visible value: a `status` lozenge's label and a `jira` macro's issue key are
/// parameters, so `State: APPROVED` becomes `State:`. Keeping those would take a whitelist
/// of parameter names per macro type — real machinery, permanently incomplete — and a
/// kilobyte of base64 mid-sentence is the worse of the two costs.
const MACHINE_STATE_ELEMENTS: &[&str] = &[
    "ac:parameter",
    "ac:task-id",
    "ac:task-uuid",
    "ac:task-status",
    // Cloud smart-links carry their settings the same way, welding a URL onto the fallback
    // text they sit beside (`https://x.example/1fallback text`). `ac:adf-fallback` is the
    // human-readable half and is left alone.
    "ac:adf-attribute",
];

fn drop_element(
    _: &dyn htmd::element_handler::Handlers,
    _: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    Some(String::new().into())
}

/// The mirror of the rule above: a resource identifier carries its label in an ATTRIBUTE,
/// so degrading it to its (empty) text drops the reference entirely — `See <ac:link><ri:page
/// ri:content-title="Design Notes"/></ac:link>` becomes a dangling `See`. Every
/// Confluence→Confluence cross-reference is lost that way, which is precisely the material a
/// knowledge vault wants.
///
/// Only identifiers whose attribute IS a human label are recovered. `ri:user` carries an
/// opaque account id, and emitting that would commit the same defect this file exists to
/// prevent, so it stays dropped.
fn resource_label(
    _: &dyn htmd::element_handler::Handlers,
    element: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    // The parser keeps the `ri:` prefix in the attribute's local name, since storage
    // format declares no namespace an HTML parser would resolve.
    let label = element
        .attrs
        .iter()
        .find(|a| matches!(&*a.name.local, "ri:content-title" | "ri:filename"))
        .map(|a| a.value.to_string())
        .unwrap_or_default();
    Some(label.into())
}

/// Rewrite the parts of Confluence storage format that an HTML parser reads wrongly, so the
/// shared converter sees ordinary HTML. Two rewrites, both literal-token exact rather than
/// guesses, and an input carrying neither is returned untouched.
///
/// **CDATA.** Storage format is XHTML and puts a code macro's body — and a link's display
/// text — in `<![CDATA[…]]>`. An HTML5 parser has no CDATA outside foreign content: it reads
/// the whole section as a bogus COMMENT and drops the contents, so every Confluence code
/// block arrived empty, silently, which on an engineering wiki is the most valuable text on
/// the page. CDATA cannot nest and ends at the first `]]>`.
///
/// **A code body is a code block.** `<ac:plain-text-body>` is where a `code`/`noformat`
/// macro keeps its body; left as an unknown element it degrades to a paragraph, and the
/// converter then MARKDOWN-ESCAPES it — `[1, 2]` arrives as `\[1, 2\]`, backslashes
/// injected into JSON. Mapping it to `<pre><code>` is what makes the converter emit a fenced
/// block and stop escaping, which is the only form that reproduces the source text.
fn normalize_storage_format(html: &str) -> std::borrow::Cow<'_, str> {
    let unwrapped = unwrap_cdata(html);
    if !unwrapped.contains(PLAIN_TEXT_BODY_OPEN) {
        return unwrapped;
    }
    std::borrow::Cow::Owned(
        unwrapped
            .replace(PLAIN_TEXT_BODY_OPEN, "<pre><code>")
            .replace("</ac:plain-text-body>", "</code></pre>"),
    )
}

const PLAIN_TEXT_BODY_OPEN: &str = "<ac:plain-text-body>";

/// Replace every CDATA section with its text, escaped for HTML — see
/// [`normalize_storage_format`] for why.
fn unwrap_cdata(html: &str) -> std::borrow::Cow<'_, str> {
    const OPEN: &str = "<![CDATA[";
    const CLOSE: &str = "]]>";
    if !html.contains(OPEN) {
        return std::borrow::Cow::Borrowed(html);
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        let (text, tail) = match after.find(CLOSE) {
            Some(end) => (&after[..end], &after[end + CLOSE.len()..]),
            // Unterminated: the rest of the document is its content, which is what an XML
            // parser would also conclude.
            None => (after, ""),
        };
        for c in text.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                _ => out.push(c),
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

/// Whether a URL is a `data:` URI, tested on the prefix of a borrowed `&str` so a
/// multi-kilobyte base64 payload is never copied just to classify it (the case this
/// exists for). Case-insensitive and tolerant of leading whitespace, matching the
/// leniency browsers apply to the scheme.
fn is_data_uri(url: &str) -> bool {
    let url = url.trim_start();
    url.len() >= 5 && url.as_bytes()[..5].eq_ignore_ascii_case(b"data:")
}

/// `img` handler that drops `data:` URIs — upholding the `lk_core::markdown`
/// cleanliness contract (`scan_defects` finds no `InlineDataUri`) at the conversion
/// boundary. An inlined base64 image (HTML email trackers, embedded logos) converts
/// to a multi-kilobyte single line that bloats vault pages and LLM task inputs while
/// carrying zero retrievable knowledge — so it degrades to its alt text (the
/// loss-averse rule: keep the text content). A fetchable `http(s)` image keeps the
/// standard `![alt](src "title")` form.
fn img_without_data_uris(
    _: &dyn htmd::element_handler::Handlers,
    element: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    let attr = |name: &str| {
        element
            .attrs
            .iter()
            .find(|a| &a.name.local == name)
            .map(|a| a.value.as_ref())
    };
    let src = attr("src")?;
    let alt = attr("alt").unwrap_or_default();

    // A data: URI degrades to its alt text as PLAIN body content — not inside
    // `![…]` — so it is emitted verbatim. Markdown escaping here would surface as
    // literal backslashes in the rendered text.
    if is_data_uri(src) {
        return Some(alt.to_string().into());
    }

    // A fetchable image: emit `![alt](src "title")`. Escape only what the syntax
    // demands — parens in the URL, a space-bearing URL wrapped in `<…>`, and the
    // `"`-delimited title's own quotes.
    let src = src.replace('(', "\\(").replace(')', "\\)");
    let (open, close) = if src.contains(' ') {
        ("<", ">")
    } else {
        ("", "")
    };
    let title = attr("title").map_or(String::new(), |t| {
        format!(" \"{}\"", t.replace('"', "\\\""))
    });
    Some(format!("![{alt}]({open}{src}{title}{close})").into())
}

/// Extract the readable article core from a full HTML page and convert it to
/// Markdown, stripping boilerplate (nav, ads, footers) via `dom_smoothie`.
///
/// Returns `None` when extraction fails or yields empty content — it is heuristic
/// and mis-extracts on non-article pages (a sidebar, a near-empty node). The caller
/// owns the fallback because the right one differs: a feed reader keeps its
/// known-clean summary, an importer of a user-chosen file converts the whole page.
/// Folding a full-page fallback in here would let boilerplate longer than a clean
/// summary silently replace it.
pub fn readable_html_to_markdown(html: &str, base_url: &url::Url) -> Option<String> {
    let mut readability = match dom_smoothie::Readability::new(html, Some(base_url.as_str()), None)
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(url = %base_url, error = %e, "readability extraction failed");
            return None;
        }
    };
    match readability.parse() {
        Ok(article) => {
            let extracted = html_to_markdown(&article.content);
            if extracted.trim().is_empty() {
                tracing::warn!(url = %base_url, "readability extracted empty content");
                None
            } else {
                Some(extracted)
            }
        }
        Err(e) => {
            tracing::warn!(url = %base_url, error = %e, "readability extraction failed");
            None
        }
    }
}

/// Convert an Atlassian Document Format node tree (Jira rich text) to Markdown.
pub fn adf_to_markdown(node: &Value) -> String {
    let mut out = String::new();
    render_adf(node, &mut out);
    collapse_blank_lines(out.trim())
}

fn render_adf(node: &Value, out: &mut String) {
    match node.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => out.push_str(&apply_marks(node)),
        "hardBreak" => out.push('\n'),
        "rule" => out.push_str("\n---\n\n"),
        "mention" => out.push_str(adf_attr(node, "text").unwrap_or("")),
        "emoji" => out.push_str(
            adf_attr(node, "text")
                .or_else(|| adf_attr(node, "shortName"))
                .unwrap_or(""),
        ),
        "heading" => {
            let level = node
                .get("attrs")
                .and_then(|a| a.get("level"))
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 6) as usize;
            out.push_str(&"#".repeat(level));
            out.push(' ');
            render_adf_children(node, out);
            out.push_str("\n\n");
        }
        "paragraph" => {
            render_adf_children(node, out);
            out.push_str("\n\n");
        }
        "bulletList" => render_adf_list(node, out, None),
        "orderedList" => {
            // ADF carries the list's first number in `attrs.order` (Confluence/Jira split a
            // long list by starting the next block at N). Honor it so the number isn't lost;
            // default to 1 when absent.
            let start = node
                .get("attrs")
                .and_then(|a| a.get("order"))
                .and_then(Value::as_u64)
                .map_or(1, |n| n as usize);
            render_adf_list(node, out, Some(start))
        }
        "taskList" => {
            render_adf_children(node, out);
            out.push('\n');
        }
        "taskItem" => {
            let checked = adf_attr(node, "state") == Some("DONE");
            out.push_str(if checked { "- [x] " } else { "- [ ] " });
            render_adf_children(node, out);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        "codeBlock" => {
            let lang = adf_attr(node, "language").unwrap_or("");
            out.push_str("```");
            out.push_str(lang);
            out.push('\n');
            render_adf_children(node, out);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n\n");
        }
        "blockquote" => {
            let mut inner = String::new();
            render_adf_children(node, &mut inner);
            for line in inner.trim_end().lines() {
                out.push_str("> ");
                out.push_str(line);
                out.push('\n');
            }
            out.push('\n');
        }
        "table" => render_adf_table(node, out),
        // doc, panel, and anything unrecognized: recurse so text is preserved.
        // For unknown leaf nodes (no content array), rescue common attrs.
        _ => {
            if node.get("content").and_then(Value::as_array).is_some() {
                render_adf_children(node, out);
            } else if let Some(attrs) = node.get("attrs").and_then(Value::as_object) {
                let rescued = attrs
                    .get("url")
                    .or_else(|| attrs.get("href"))
                    .or_else(|| attrs.get("text"))
                    .or_else(|| attrs.get("title"))
                    .and_then(Value::as_str);
                if let Some(value) = rescued {
                    out.push_str(value);
                }
            }
        }
    }
}

fn render_adf_children(node: &Value, out: &mut String) {
    if let Some(content) = node.get("content").and_then(Value::as_array) {
        for child in content {
            render_adf(child, out);
        }
    }
}

fn render_adf_list(node: &Value, out: &mut String, ordered_start: Option<usize>) {
    let Some(items) = node.get("content").and_then(Value::as_array) else {
        return;
    };
    let mut idx = ordered_start.unwrap_or(0);
    for item in items {
        let mut inner = String::new();
        render_adf_children(item, &mut inner);
        let marker = match ordered_start {
            Some(_) => {
                let m = format!("{idx}. ");
                idx += 1;
                m
            }
            None => "- ".to_string(),
        };
        let mut lines = inner.trim().lines();
        if let Some(first) = lines.next() {
            out.push_str(&marker);
            out.push_str(first);
            out.push('\n');
            // Continuation lines (nested lists, multi-paragraph items) align under the text.
            for line in lines {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out.push('\n');
}

/// Render an ADF table as a GFM pipe table. Each cell is flattened to a single line
/// (cell separators escaped); the first row becomes the GFM header. Ragged rows are
/// padded to the widest row so the column count is consistent.
fn render_adf_table(node: &Value, out: &mut String) {
    let Some(content) = node.get("content").and_then(Value::as_array) else {
        return;
    };
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row in content {
        if row.get("type").and_then(Value::as_str) != Some("tableRow") {
            continue;
        }
        let Some(cells) = row.get("content").and_then(Value::as_array) else {
            continue;
        };
        let rendered: Vec<String> = cells
            .iter()
            .map(|cell| {
                let mut inner = String::new();
                render_adf_children(cell, &mut inner);
                // A GFM cell is single-line; collapse whitespace and escape the pipe.
                inner
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .replace('|', "\\|")
            })
            .collect();
        if !rendered.is_empty() {
            rows.push(rendered);
        }
    }
    let Some(cols) = rows.iter().map(Vec::len).max() else {
        return;
    };

    for (i, row) in rows.iter().enumerate() {
        out.push('|');
        for c in 0..cols {
            out.push(' ');
            out.push_str(row.get(c).map(String::as_str).unwrap_or(""));
            out.push_str(" |");
        }
        out.push('\n');
        if i == 0 {
            out.push('|');
            for _ in 0..cols {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
    }
    out.push('\n');
}

fn adf_attr<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.get("attrs")
        .and_then(|a| a.get(key))
        .and_then(Value::as_str)
}

/// Wrap a text leaf's content in the Markdown for each of its ADF marks. Unknown marks
/// (underline, color, …) leave the text untouched so nothing is lost.
fn apply_marks(text_node: &Value) -> String {
    let mut s = text_node
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(marks) = text_node.get("marks").and_then(Value::as_array) {
        for mark in marks {
            s = match mark.get("type").and_then(Value::as_str).unwrap_or("") {
                "strong" => format!("**{s}**"),
                "em" => format!("*{s}*"),
                "code" => format!("`{s}`"),
                "strike" => format!("~~{s}~~"),
                "link" => {
                    let href = mark
                        .get("attrs")
                        .and_then(|a| a.get("href"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    format!("[{s}]({href})")
                }
                _ => s,
            };
        }
    }
    s
}

/// Convert Slack mrkdwn to Markdown: rewrite `<…>` tokens (user/channel mentions, special
/// commands, links), convert emphasis markers (`*bold*` → `**bold**`,
/// `~strike~` → `~~strike~~`), and decode HTML entities. User-id mentions are resolved to
/// display names via `users`.
pub fn slack_to_markdown(text: &str, users: &HashMap<String, String>) -> String {
    let rewritten = rewrite_angle_tokens(text, users);
    let converted = convert_mrkdwn_formatting(&rewritten);
    let decoded = decode_entities(&converted);
    render_emoji_shortcodes(&decoded)
}

fn rewrite_angle_tokens(text: &str, users: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt + 1..];
        match after.find('>') {
            Some(gt) => {
                out.push_str(&convert_slack_token(&after[..gt], users));
                rest = &after[gt + 1..];
            }
            // Unbalanced '<' — emit literally and continue past it.
            None => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `token` is the text between `<` and `>`. Forms: `@U123`/`@U123|name` (user),
/// `#C123`/`#C123|name` (channel), `!here`/`!subteam^S1|@team` (special), or `url`/
/// `url|label` (link).
/// The label after a `|` in a Slack token (`U123|name` → `name`), if non-empty.
fn label_after_pipe(s: &str) -> Option<&str> {
    s.split('|').nth(1).filter(|l| !l.is_empty())
}

fn convert_slack_token(token: &str, users: &HashMap<String, String>) -> String {
    if let Some(rest) = token.strip_prefix('@') {
        // Prefer the pipe-label, then the resolved display name, then the raw user id.
        let user_id = rest.split('|').next().unwrap_or(rest);
        let name = label_after_pipe(rest)
            .map(|s| s.to_string())
            .or_else(|| users.get(user_id).cloned())
            .unwrap_or_else(|| user_id.to_string());
        format!("@{name}")
    } else if let Some(rest) = token.strip_prefix('#') {
        format!(
            "#{}",
            label_after_pipe(rest).unwrap_or_else(|| rest.split('|').next().unwrap_or(rest))
        )
    } else if let Some(rest) = token.strip_prefix("!date^") {
        // Date tokens: `!date^timestamp^format|fallback` — emit the fallback text or the
        // raw unix timestamp when no fallback is given.
        match label_after_pipe(rest) {
            Some(fallback) => fallback.to_string(),
            None => rest.split('^').next().unwrap_or(rest).to_string(),
        }
    } else if let Some(rest) = token.strip_prefix('!') {
        match label_after_pipe(rest) {
            Some(label) => format!("@{}", label.trim_start_matches('@')),
            None => format!("@{}", rest.split('^').next().unwrap_or(rest)),
        }
    } else {
        let url = token.split('|').next().unwrap_or("");
        match label_after_pipe(token) {
            Some(label) => format!("[{label}]({url})"),
            None => url.to_string(),
        }
    }
}

/// Convert Slack mrkdwn emphasis markers to standard Markdown:
/// - `*text*` (Slack bold) → `**text**` (Markdown bold)
/// - `~text~` (Slack strikethrough) → `~~text~~` (Markdown strikethrough)
///
/// Markers must be at word boundaries: preceded by whitespace/start-of-string and followed
/// by whitespace/end-of-string (after the closing marker). Content inside backtick code
/// spans and triple-backtick code blocks is left untouched.
fn convert_mrkdwn_formatting(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Skip triple-backtick code blocks.
        if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            out.push_str("```");
            i += 3;
            // Copy until closing ```.
            while i < len {
                if i + 2 < len && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
                    out.push_str("```");
                    i += 3;
                    break;
                }
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }

        // Skip inline code spans.
        if chars[i] == '`' {
            out.push('`');
            i += 1;
            while i < len && chars[i] != '`' {
                out.push(chars[i]);
                i += 1;
            }
            if i < len {
                out.push('`');
                i += 1;
            }
            continue;
        }

        // Try to convert *bold* or ~strike~.
        if (chars[i] == '*' || chars[i] == '~') && is_word_boundary_before(i, &chars) {
            let marker = chars[i];
            if let Some(end) = find_closing_marker(i + 1, marker, &chars) {
                // The closing marker must be followed by a word boundary.
                if is_word_boundary_after(end, &chars) {
                    let inner: String = chars[i + 1..end].iter().collect();
                    if marker == '*' {
                        out.push_str("**");
                        out.push_str(&inner);
                        out.push_str("**");
                    } else {
                        out.push_str("~~");
                        out.push_str(&inner);
                        out.push_str("~~");
                    }
                    i = end + 1;
                    continue;
                }
            }
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

/// CJK characters act as word boundaries for inline formatting — Korean, Chinese, and
/// Japanese text typically has no whitespace between words, but Slack treats CJK characters
/// as natural boundaries for `*bold*` and `~strike~` markers.
fn is_cjk_char(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF    // Hangul Jamo
        | 0x2E80..=0x9FFF  // CJK Radicals through Unified Ideographs (includes Hiragana/Katakana)
        | 0xAC00..=0xD7AF  // Hangul Syllables
        | 0xD7B0..=0xD7FF  // Hangul Jamo Extended-B
        | 0xF900..=0xFAFF  // CJK Compatibility Ideographs
        | 0xFF65..=0xFFDC  // Halfwidth Katakana + Hangul
        | 0x20000..=0x2FA1F // CJK Extension B–F
    )
}

/// The position before `pos` is a word boundary (start of string, whitespace, CJK character,
/// or punctuation that commonly precedes emphasis).
fn is_word_boundary_before(pos: usize, chars: &[char]) -> bool {
    if pos == 0 {
        return true;
    }
    let prev = chars[pos - 1];
    prev.is_whitespace() || is_cjk_char(prev) || matches!(prev, '(' | '[' | '{' | '"' | '\'' | '\n')
}

/// The position after `pos` is a word boundary (end of string, whitespace, CJK character,
/// or punctuation that commonly follows emphasis).
fn is_word_boundary_after(pos: usize, chars: &[char]) -> bool {
    let next_pos = pos + 1;
    if next_pos >= chars.len() {
        return true;
    }
    let next = chars[next_pos];
    next.is_whitespace()
        || is_cjk_char(next)
        || matches!(
            next,
            ')' | ']' | '}' | '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\''
        )
}

/// Find the closing marker character that is not preceded by whitespace (Slack rule:
/// closing markers must be adjacent to the word they wrap). Returns the index of the
/// closing marker, or `None`.
fn find_closing_marker(start: usize, marker: char, chars: &[char]) -> Option<usize> {
    let mut j = start;
    while j < chars.len() {
        if chars[j] == '\n' {
            // Slack inline formatting doesn't span lines.
            return None;
        }
        if chars[j] == marker && j > start && !chars[j - 1].is_whitespace() {
            return Some(j);
        }
        j += 1;
    }
    None
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Render Slack emoji shortcodes (`:tada:`, `:+1:`) as their Unicode glyph — exactly
/// what the author saw — instead of dropping them, since a shortcode like `:100:` or
/// `:rocket:` can carry real meaning. Only shortcodes in the standard Unicode emoji set
/// are converted, and only in prose: a shortcode written as a code literal is content, so
/// code spans are preserved verbatim. Colon-delimited prose (`:default:`, a `key:value:`
/// token) and Slack-specific or workspace-custom names (not in the standard set) are left
/// untouched.
fn render_emoji_shortcodes(text: &str) -> String {
    // Code spans are skipped using the CommonMark rule: a run of N backticks opens a span
    // that closes at the next run of EXACTLY N backticks. This keeps inline (`` `…` ``) and
    // fenced (```` ```…``` ````) code intact even when a fenced body itself contains
    // backticks — a naive split on `` ` `` miscounts parity there and would eat a shortcode
    // inside the fence.
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut prose_start = 0;
    let mut i = 0;
    while i < n {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        // Convert the prose run preceding this backtick run.
        render_prose_shortcodes(&text[prose_start..i], &mut out);
        let open = i;
        while i < n && bytes[i] == b'`' {
            i += 1;
        }
        let run = i - open;
        // Seek the matching closing run of exactly `run` backticks.
        let mut close_end = None;
        let mut j = i;
        while j < n {
            if bytes[j] != b'`' {
                j += 1;
                continue;
            }
            let cstart = j;
            while j < n && bytes[j] == b'`' {
                j += 1;
            }
            if j - cstart == run {
                close_end = Some(j);
                break;
            }
        }
        match close_end {
            // Emit the whole code span (both fences + body) verbatim.
            Some(end) => {
                out.push_str(&text[open..end]);
                i = end;
                prose_start = end;
            }
            // Unclosed run: the backticks are literal text, so the rest is prose again.
            None => {
                out.push_str(&text[open..i]);
                prose_start = i;
            }
        }
    }
    render_prose_shortcodes(&text[prose_start..], &mut out);
    out
}

/// Convert recognized emoji shortcodes in one prose run (no code spans) to their glyph,
/// writing into `out`.
fn render_prose_shortcodes(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        // A shortcode is delimited, not embedded mid-word: the opening colon must not
        // directly follow an alphanumeric, so `key:tada:value` is left intact.
        let boundary = !rest[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric());
        match after.find(':') {
            Some(end) if boundary && end > 0 => match emojis::get_by_shortcode(&after[..end]) {
                Some(emoji) => {
                    out.push_str(emoji.as_str());
                    rest = &after[end + 1..];
                }
                None => {
                    out.push(':');
                    rest = after;
                }
            },
            _ => {
                out.push(':');
                rest = after;
            }
        }
    }
    out.push_str(rest);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Machine state is not prose. Degraded to text it welds onto the surrounding words
    /// with no separator — a status macro's base64 state blob and a task's UUID land
    /// mid-sentence, and one real Confluence page came out 30% that. The macro's and the
    /// task's actual content must survive intact.
    #[test]
    fn machine_state_never_becomes_body_text() {
        let macro_html = concat!(
            "<p>Before</p>",
            r#"<ac:structured-macro ac:name="status">"#,
            r#"<ac:parameter ac:name="title">검증</ac:parameter>"#,
            r#"<ac:parameter ac:name="source">JTdCJTIybmFtZSUyMiUzQQ==</ac:parameter>"#,
            "<ac:rich-text-body><p>Real body</p></ac:rich-text-body>",
            "</ac:structured-macro>",
            "<p>After</p>",
        );
        let md = html_to_markdown(macro_html);
        assert!(
            !md.contains("JTdC"),
            "no encoded settings may reach the page:\n{md}"
        );
        assert!(
            !md.contains("검증"),
            "nor a parameter's display value:\n{md}"
        );
        assert!(
            md.contains("Before") && md.contains("After"),
            "prose survives:\n{md}"
        );
        assert!(
            md.contains("Real body"),
            "the macro's own content survives:\n{md}"
        );

        // A task list is far more common than a roadmap macro, and carries three of these.
        let task_html = concat!(
            "<ac:task-list><ac:task>",
            "<ac:task-id>17</ac:task-id>",
            "<ac:task-uuid>0e6f1a-9c</ac:task-uuid>",
            "<ac:task-status>incomplete</ac:task-status>",
            "<ac:task-body><span>Ship the thing</span></ac:task-body>",
            "</ac:task></ac:task-list>",
        );
        let md = html_to_markdown(task_html);
        assert_eq!(
            md, "Ship the thing",
            "a task keeps its prose and nothing of its identity:\n{md}"
        );
    }

    /// Storage format puts a code macro's body in CDATA, which an HTML5 parser reads as a
    /// bogus COMMENT and drops — so every Confluence code block arrived empty, silently.
    /// Recovering it is only half: left as an unknown element the body degrades to a
    /// paragraph and the converter MARKDOWN-ESCAPES it, injecting backslashes into the
    /// code. It has to arrive as a fenced block, which is the only form that reproduces
    /// the source text.
    #[test]
    fn a_code_body_survives_cdata_and_arrives_as_code_not_escaped_prose() {
        let md = html_to_markdown(concat!(
            r#"<ac:structured-macro ac:name="code">"#,
            r#"<ac:parameter ac:name="language">json</ac:parameter>"#,
            r#"<ac:plain-text-body><![CDATA[{ "a": [1, 2], "b": "x_y", "c": a < b }]]></ac:plain-text-body>"#,
            "</ac:structured-macro>",
        ));
        assert!(md.starts_with("```"), "a code body is a code block:\n{md}");
        assert!(
            md.contains(r#"{ "a": [1, 2], "b": "x_y", "c": a < b }"#),
            "reproduced verbatim — no markdown escaping, no lost `<`:\n{md}"
        );
        assert!(!md.contains("\\["), "no injected backslashes:\n{md}");
    }

    /// A Cloud smart-link carries its settings the same way a macro does, welding a URL
    /// onto the fallback text beside it. The fallback is the readable half and stays.
    #[test]
    fn a_smart_link_keeps_its_fallback_and_drops_its_settings() {
        let md = html_to_markdown(concat!(
            "<p>See <ac:adf-extension><ac:adf-node type=\"inline-card\">",
            r#"<ac:adf-attribute key="url">https://x.example/1</ac:adf-attribute>"#,
            "</ac:adf-node><ac:adf-fallback>the card</ac:adf-fallback></ac:adf-extension></p>",
        ));
        assert_eq!(md, "See the card", "settings out, fallback kept:\n{md}");
    }

    /// An unterminated section is what an XML parser would also read to the end, and a
    /// document with no CDATA at all must come back untouched.
    #[test]
    fn cdata_edges_are_exact() {
        assert!(html_to_markdown("<p><![CDATA[tail forever</p>").contains("tail forever"));
        assert_eq!(html_to_markdown("<p>plain</p>"), "plain");
        assert_eq!(html_to_markdown("<p><![CDATA[]]>empty</p>"), "empty");
    }

    /// A resource identifier carries its label in an ATTRIBUTE, so degrading it to its
    /// empty text drops the reference entirely — every Confluence→Confluence
    /// cross-reference, which is exactly the material a knowledge vault wants.
    #[test]
    fn a_resource_identifier_keeps_the_label_its_attribute_carries() {
        assert_eq!(
            html_to_markdown(
                r#"<p>See <ac:link><ri:page ri:content-title="Design Notes"/></ac:link></p>"#
            ),
            "See Design Notes"
        );
        assert_eq!(
            html_to_markdown(
                r#"<p>File <ac:link><ri:attachment ri:filename="spec.pdf"/></ac:link></p>"#
            ),
            "File spec.pdf"
        );
        // An opaque account id is machine state; emitting it would be the very defect the
        // rest of this file prevents, so a user mention stays dropped.
        assert_eq!(
            html_to_markdown(
                r#"<p>Ask <ac:link><ri:user ri:account-id="557058:abc"/></ac:link></p>"#
            ),
            "Ask"
        );
    }

    use serde_json::json;

    #[test]
    fn adf_paragraph_with_marks_and_link() {
        let adf = json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [
                    {"type": "text", "text": "see "},
                    {"type": "text", "text": "the docs", "marks": [
                        {"type": "strong"},
                        {"type": "link", "attrs": {"href": "https://x.io"}}
                    ]},
                    {"type": "text", "text": " now"}
                ]
            }]
        });
        assert_eq!(
            adf_to_markdown(&adf),
            "see [**the docs**](https://x.io) now"
        );
    }

    #[test]
    fn adf_heading_and_bullets() {
        let adf = json!({
            "type": "doc",
            "content": [
                {"type": "heading", "attrs": {"level": 2},
                 "content": [{"type": "text", "text": "Plan"}]},
                {"type": "bulletList", "content": [
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "first"}]}]},
                    {"type": "listItem", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "second"}]}]}
                ]}
            ]
        });
        assert_eq!(adf_to_markdown(&adf), "## Plan\n\n- first\n- second");
    }

    #[test]
    fn adf_ordered_list_honors_start_number() {
        // `attrs.order` is the list's first number (a split/continued list); honor it.
        let adf = json!({
            "type": "doc",
            "content": [{"type": "orderedList", "attrs": {"order": 3}, "content": [
                {"type": "listItem", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "a"}]}]},
                {"type": "listItem", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "b"}]}]}
            ]}]
        });
        assert_eq!(adf_to_markdown(&adf), "3. a\n4. b");

        // No `order` attr → default first number 1.
        let adf = json!({
            "type": "doc",
            "content": [{"type": "orderedList", "content": [
                {"type": "listItem", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "x"}]}]}
            ]}]
        });
        assert_eq!(adf_to_markdown(&adf), "1. x");
    }

    #[test]
    fn adf_code_block_keeps_content() {
        let adf = json!({
            "type": "doc",
            "content": [{
                "type": "codeBlock", "attrs": {"language": "rust"},
                "content": [{"type": "text", "text": "let x = 1;"}]
            }]
        });
        assert_eq!(adf_to_markdown(&adf), "```rust\nlet x = 1;\n```");
    }

    #[test]
    fn adf_unknown_node_rescues_url_attr() {
        let adf = json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [
                    {"type": "text", "text": "see "},
                    {"type": "inlineCard", "attrs": {"url": "https://example.com/page"}},
                ]
            }]
        });
        assert_eq!(adf_to_markdown(&adf), "see https://example.com/page");
    }

    #[test]
    fn adf_unknown_node_without_attrs_is_silent() {
        let adf = json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "unknownThing"},
                ]
            }]
        });
        assert_eq!(adf_to_markdown(&adf), "ok");
    }

    #[test]
    fn adf_table_renders_as_pipe_table() {
        let adf = json!({
            "type": "table",
            "content": [
                {"type": "tableRow", "content": [
                    {"type": "tableHeader", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "Name"}]}
                    ]},
                    {"type": "tableHeader", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "Role"}]}
                    ]}
                ]},
                {"type": "tableRow", "content": [
                    {"type": "tableCell", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "Ada"}]}
                    ]},
                    {"type": "tableCell", "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "Eng"}]}
                    ]}
                ]}
            ]
        });
        assert_eq!(
            adf_to_markdown(&adf),
            "| Name | Role |\n| --- | --- |\n| Ada | Eng |"
        );
    }

    fn no_users() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn slack_tokens_rewritten() {
        let u = no_users();
        assert_eq!(slack_to_markdown("hi <@U123>", &u), "hi @U123");
        assert_eq!(slack_to_markdown("<@U1|june> shipped", &u), "@june shipped");
        assert_eq!(slack_to_markdown("see <#C1|general>", &u), "see #general");
        assert_eq!(
            slack_to_markdown("docs <https://x.io|here>", &u),
            "docs [here](https://x.io)"
        );
        assert_eq!(
            slack_to_markdown("raw <https://x.io>", &u),
            "raw https://x.io"
        );
        assert_eq!(slack_to_markdown("<!here> ping", &u), "@here ping");
    }

    #[test]
    fn slack_resolves_user_id_to_display_name() {
        let mut users = HashMap::new();
        users.insert("U123".to_string(), "Alice".to_string());
        assert_eq!(slack_to_markdown("hi <@U123>", &users), "hi @Alice");
        // Pipe-label still takes priority over the resolved name.
        assert_eq!(slack_to_markdown("<@U123|bob> said", &users), "@bob said");
    }

    #[test]
    fn slack_date_token_with_fallback() {
        let u = no_users();
        assert_eq!(
            slack_to_markdown("due <!date^1234567890^{date}|May 24, 2026>", &u),
            "due May 24, 2026"
        );
    }

    #[test]
    fn slack_date_token_without_fallback() {
        let u = no_users();
        assert_eq!(
            slack_to_markdown("at <!date^1234567890^{date}>", &u),
            "at 1234567890"
        );
    }

    #[test]
    fn slack_bold_converted() {
        let u = no_users();
        assert_eq!(
            slack_to_markdown("this is *bold* text", &u),
            "this is **bold** text"
        );
    }

    #[test]
    fn slack_strike_converted() {
        let u = no_users();
        assert_eq!(
            slack_to_markdown("this is ~struck~ out", &u),
            "this is ~~struck~~ out"
        );
    }

    #[test]
    fn slack_bold_not_converted_mid_word() {
        let u = no_users();
        assert_eq!(slack_to_markdown("file*name*here", &u), "file*name*here");
    }

    #[test]
    fn slack_bold_in_code_not_converted() {
        let u = no_users();
        assert_eq!(slack_to_markdown("`*bold*`", &u), "`*bold*`");
    }

    #[test]
    fn slack_bold_with_cjk_boundary() {
        let u = no_users();
        // CJK characters act as word boundaries — Slack renders *bold* adjacent to Korean text.
        assert_eq!(
            slack_to_markdown("한글*bold*텍스트", &u),
            "한글**bold**텍스트"
        );
        assert_eq!(
            slack_to_markdown("결과~삭제~했습니다", &u),
            "결과~~삭제~~했습니다"
        );
    }

    #[test]
    fn slack_decodes_entities() {
        assert_eq!(
            slack_to_markdown("a &lt;b&gt; &amp; c", &no_users()),
            "a <b> & c"
        );
    }

    #[test]
    fn slack_emoji_shortcodes_render_to_glyphs() {
        let u = no_users();
        // Recognized Unicode emoji shortcodes render as their glyph — faithful to what the
        // author saw, never dropped (a `:100:`/`:rocket:` can carry real meaning).
        assert_eq!(slack_to_markdown(":+1: great", &u), "👍 great");
        assert_eq!(slack_to_markdown(":pray::fire:", &u), "🙏🔥");
        // A workspace-custom emoji is NOT in the standard set, so it survives as literal
        // text rather than risk mangling a real word that looks like a shortcode.
        assert_eq!(slack_to_markdown(":custom_parrot:", &u), ":custom_parrot:");
    }

    #[test]
    fn slack_emoji_render_preserves_prose_and_colon_delimited_words() {
        let u = no_users();
        // A colon embedded mid-word is not an emoji shortcode — `key:value:pair`
        // technical text must keep its middle token, not be touched.
        assert_eq!(
            slack_to_markdown("config:value:here", &u),
            "config:value:here"
        );
        assert_eq!(slack_to_markdown("app:icon:large", &u), "app:icon:large");
        // A delimited word that merely LOOKS like a shortcode but isn't a real emoji
        // (a Ruby symbol, colon-emphasis) must be preserved, not converted.
        assert_eq!(
            slack_to_markdown("the :default: value", &u),
            "the :default: value"
        );
        assert_eq!(
            slack_to_markdown("mark :important:", &u),
            "mark :important:"
        );
        // Delimited shortcodes render to glyphs, even adjacent to punctuation.
        assert_eq!(slack_to_markdown("nice (:tada:)", &u), "nice (🎉)");
        assert_eq!(slack_to_markdown("done :+1:", &u), "done 👍");
    }

    #[test]
    fn slack_emoji_render_preserves_shortcodes_in_code_spans() {
        let u = no_users();
        // A shortcode written as a code literal is content, not decoration — a `code`
        // span must survive verbatim even when it holds a real emoji shortcode, while a
        // bare prose shortcode on the same line still renders to its glyph.
        assert_eq!(
            slack_to_markdown("use `:tada:` here :tada:", &u),
            "use `:tada:` here 🎉"
        );
        assert_eq!(
            slack_to_markdown("the `:x:` marker", &u),
            "the `:x:` marker"
        );
        // A fenced block whose body itself contains backticks: the shortcode inside must
        // survive (the opening ``` matches the closing ```; the inner single backticks
        // don't close it). A naive backtick-parity split mis-counts here and converts it.
        assert_eq!(
            slack_to_markdown("```\nlet s = `:tada:`;\n```", &u),
            "```\nlet s = `:tada:`;\n```"
        );
        // A double-backtick inline span is a code span too (run length 2).
        assert_eq!(slack_to_markdown("``:tada:``", &u), "``:tada:``");
    }

    #[test]
    fn html_to_markdown_converts_list_and_link() {
        let md = html_to_markdown("<ul><li>one</li><li>two</li></ul>");
        assert!(md.contains("one") && md.contains("two") && md.contains("-"));
        let link = html_to_markdown(r#"<a href="https://x.io">x</a>"#);
        assert!(link.contains("[x](https://x.io)"));
    }

    #[test]
    fn html_img_data_uri_degrades_to_alt_text() {
        // Exercised on `html_to_markdown` directly — the single seam every HTML-consuming
        // adapter (Gmail, RSS, Calendar, Manual) shares — so the policy holds for all of
        // them, not just the Gmail path where the bloat was first observed.
        // An inlined base64 image (an HTML email's embedded logo/table art) would
        // otherwise become a multi-kilobyte single markdown line. It carries no
        // retrievable knowledge — only its alt text survives.
        let md = html_to_markdown(
            r#"<p>before</p><img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUg" alt="회사 로고"><p>after</p>"#,
        );
        assert!(!md.contains("data:"), "data: URI must be dropped:\n{md}");
        assert!(md.contains("회사 로고"), "alt text must survive:\n{md}");
        assert!(md.contains("before") && md.contains("after"));

        // Without alt text the image vanishes entirely.
        let bare = html_to_markdown(r#"<img src="data:image/gif;base64,R0lGOD">"#);
        assert_eq!(bare, "");
    }

    #[test]
    fn html_img_http_src_keeps_standard_markdown() {
        let md = html_to_markdown(r#"<img src="https://x.io/a.png" alt="chart" title="Q2">"#);
        assert_eq!(md, "![chart](https://x.io/a.png \"Q2\")");
        // Parentheses in the URL are escaped, as in htmd's built-in handler.
        let parens = html_to_markdown(r#"<img src="https://x.io/a(1).png">"#);
        assert!(parens.contains("![](https://x.io/a\\(1\\).png)"));
    }

    #[test]
    fn html_img_title_quote_is_escaped_so_markdown_stays_valid() {
        // A quote inside the title would otherwise close the `"`-delimited title
        // early and emit broken markdown — it must be escaped. (Single-quoted HTML
        // attribute so the inner double quotes are real, not attribute delimiters.)
        let md = html_to_markdown(r#"<img src='https://x.io/a.png' title='he said "hi"'>"#);
        assert_eq!(md, r#"![](https://x.io/a.png "he said \"hi\"")"#);
    }

    #[test]
    fn html_img_data_uri_with_leading_space_and_caps_is_dropped() {
        // The scheme test tolerates leading whitespace and case, so a `  DATA:`
        // payload can't slip through as a giant inline blob.
        let md = html_to_markdown(r#"<img src="  DATA:image/png;base64,AAAA" alt="x">"#);
        assert_eq!(md, "x");
    }

    #[test]
    fn html_img_data_uri_alt_is_plain_text_not_markdown_escaped() {
        // The degraded alt becomes PLAIN body text (not inside `![…]`), so a quote in
        // it must appear verbatim — escaping it here would leak a literal backslash
        // into the rendered prose.
        let md = html_to_markdown(r#"<img src='data:image/png;base64,AAAA' alt='he said "hi"'>"#);
        assert_eq!(md, r#"he said "hi""#);
    }

    #[test]
    fn empty_inputs_are_empty() {
        assert_eq!(html_to_markdown("   "), "");
        assert_eq!(slack_to_markdown("", &no_users()), "");
    }

    #[test]
    fn readable_returns_none_when_no_article_core() {
        // No extractable article body — the helper signals failure rather than
        // falling back to boilerplate, so the caller keeps its known-clean content.
        let base = url::Url::parse("https://example.com/").unwrap();
        assert_eq!(
            readable_html_to_markdown("<html><body></body></html>", &base),
            None
        );
    }

    #[test]
    fn readable_returns_some_for_an_article() {
        let base = url::Url::parse("https://example.com/post").unwrap();
        let html = format!(
            "<html><body><article><h1>Title</h1>{}</article></body></html>",
            "<p>This is a substantial paragraph of article prose worth extracting.</p>".repeat(6)
        );
        let extracted = readable_html_to_markdown(&html, &base).expect("article extracted");
        assert!(extracted.contains("substantial paragraph"));
    }

    #[test]
    fn readable_absolutizes_relative_urls_against_base() {
        // dom_smoothie resolves relative URLs against base_url during extraction. RSS
        // full-text feeds rely on this (links/images must be clickable once detached from
        // the feed). Locked here because the other readable_* tests don't assert on URLs, so
        // a future dom_smoothie upgrade that changes URL resolution would otherwise pass CI.
        let base = url::Url::parse("https://example.com/post").unwrap();
        let para = "<p>This is a substantial paragraph of article prose with a \
                    <a href=\"/rel/page\">relative link</a> worth extracting as content.</p>";
        let html = format!(
            "<html><body><article><h1>Title</h1>{}</article></body></html>",
            para.repeat(6)
        );
        let extracted = readable_html_to_markdown(&html, &base).expect("article extracted");
        assert!(
            extracted.contains("https://example.com/rel/page"),
            "relative link must be absolutized against base_url:\n{extracted}"
        );
    }

    // Adversarial property tests: throw randomized hostile HTML at the converter and
    // assert the vault-text cleanliness contract holds for ANY input — closing the
    // whole "raw source bytes leak into a page" class instead of one example at a
    // time. The invariant is `lk_core::markdown::scan_defects` (the SAME predicate
    // `lore doctor` checks at rest), so code-side and data-side can't drift.
    mod properties {
        use super::*;
        use proptest::prelude::*;

        /// A fragment of attacker-controlled base64-ish payload.
        fn base64_blob() -> impl Strategy<Value = String> {
            "[A-Za-z0-9+/]{0,300}"
        }

        /// Arbitrary alt/surrounding text. Excludes only `<`, `>`, and `]` — the
        /// characters that START a defect signature (`<data:`, `](data:`) — so the
        /// adversarial text can never itself spell a data: URI and produce a FALSE
        /// positive. Everything else (quotes, parens, colons, `data:` as a word) is
        /// fair game, because the property under test is "the CONVERTER never
        /// introduces a data: URI", not "no input ever mentions one".
        fn loose_text() -> impl Strategy<Value = String> {
            r#"[\PC]{0,40}"#.prop_map(|s| s.replace(['<', '>', ']'], ""))
        }

        proptest! {
            #[test]
            fn html_to_markdown_never_emits_a_data_uri(
                blob in base64_blob(),
                alt in loose_text(),
                lead in loose_text(),
            ) {
                // An inlined base64 image embedded anywhere in a fragment must never
                // survive into the output, regardless of alt text or surrounding prose.
                let html = format!(
                    "<p>{lead}</p><img src=\"data:image/png;base64,{blob}\" alt=\"{alt}\">"
                );
                let md = html_to_markdown(&html);
                prop_assert!(
                    lk_core::markdown::scan_defects(&md).is_empty(),
                    "data: URI survived conversion:\n{md}"
                );
            }

            #[test]
            fn html_to_markdown_keeps_fetchable_image_links(
                blob in base64_blob(),
            ) {
                // A real http(s) image is knowledge-bearing and must be preserved as a
                // standard link — proving the filter targets data: URIs specifically,
                // not all images (which would be an over-broad, lossy constraint).
                let html = format!("<img src=\"https://x.io/{blob}.png\" alt=\"chart\">");
                let md = html_to_markdown(&html);
                if !blob.is_empty() {
                    prop_assert!(md.contains("https://x.io/"), "http image dropped:\n{md}");
                }
                prop_assert!(lk_core::markdown::scan_defects(&md).is_empty());
            }
        }
    }
}
