//! Normalize source-specific rich text into standard Markdown so downstream LLM steps and
//! the Obsidian vault receive clean, AI-friendly input instead of ADF JSON, raw HTML, or
//! Slack's `<…>` token soup. Conversions are loss-averse: any construct without a Markdown
//! equivalent degrades to its text content rather than being dropped.

use std::collections::HashMap;

use serde_json::Value;

/// Convert an HTML fragment to Markdown via `htmd`. On any conversion error the original
/// HTML is returned so content is never lost.
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
        .build();
    converter
        .convert(html)
        .map(|md| md.trim().to_string())
        .unwrap_or_else(|_| html.trim().to_string())
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
        "orderedList" => render_adf_list(node, out, Some(1)),
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
        // doc, panel, table cells, and anything unrecognized: recurse so text is preserved.
        _ => render_adf_children(node, out),
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
/// commands, links) and decode HTML entities. User-id mentions are resolved to display
/// names via `users`. Emphasis markers (`*`, `_`, `~`) are left as-is — they remain
/// readable text, so no information is lost.
pub fn slack_to_markdown(text: &str, users: &HashMap<String, String>) -> String {
    let rewritten = rewrite_angle_tokens(text, users);
    let decoded = decode_entities(&rewritten);
    strip_emoji_shortcodes(&decoded)
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

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Strip Slack emoji shortcodes (`:grin:`, `:custom_emoji:`). Emoji aren't core
/// information in long-lived documents — they're decorative — so removing them is
/// cleaner than maintaining an ever-growing mapping table (standard + per-workspace
/// custom emoji make exhaustive conversion infeasible and a maintenance burden).
fn strip_emoji_shortcodes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find(':') {
            Some(end) if end > 0 && end <= 40 => {
                let code = &after[..end];
                let is_shortcode = !code.contains(' ')
                    && code
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '+');
                if is_shortcode {
                    // Drop the shortcode entirely (no replacement needed).
                    rest = &after[end + 1..];
                } else {
                    out.push(':');
                    rest = after;
                }
            }
            _ => {
                out.push(':');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Squeeze 3+ consecutive newlines down to a paragraph break so converted blocks don't
/// accumulate excess vertical whitespace.
pub fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut newlines = 0;
    for ch in text.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push(ch);
            }
        } else {
            newlines = 0;
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn slack_decodes_entities() {
        assert_eq!(
            slack_to_markdown("a &lt;b&gt; &amp; c", &no_users()),
            "a <b> & c"
        );
    }

    #[test]
    fn slack_emoji_shortcodes_stripped() {
        let u = no_users();
        // Emoji shortcodes are stripped entirely (not converted) — they're decorative,
        // not core document information, and maintaining a mapping is infeasible.
        assert_eq!(slack_to_markdown(":+1: great", &u), " great");
        assert_eq!(slack_to_markdown(":pray::fire:", &u), "");
        assert_eq!(slack_to_markdown(":custom_parrot:", &u), "");
    }

    #[test]
    fn html_to_markdown_converts_list_and_link() {
        let md = html_to_markdown("<ul><li>one</li><li>two</li></ul>");
        assert!(md.contains("one") && md.contains("two") && md.contains("-"));
        let link = html_to_markdown(r#"<a href="https://x.io">x</a>"#);
        assert!(link.contains("[x](https://x.io)"));
    }

    #[test]
    fn empty_inputs_are_empty() {
        assert_eq!(html_to_markdown("   "), "");
        assert_eq!(slack_to_markdown("", &no_users()), "");
    }
}
