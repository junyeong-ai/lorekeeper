//! Slack's own lexical constructs carry formatting authority. Ordinary text,
//! entity-decoded characters and resolved labels never become CommonMark syntax.

use std::collections::HashMap;

use pulldown_cmark::{CodeBlockKind, Event, LinkType, Tag, TagEnd};

pub(crate) fn render(text: &str, users: &HashMap<String, String>) -> String {
    let mut events = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < text.len() {
        if text.as_bytes()[at] == b'<'
            && let Some(end) = text[at + 1..].find('>').map(|end| at + 1 + end)
            && token(&text[at + 1..end], users, &mut Vec::new())
        {
            at = end + 1;
            continue;
        }
        if text.as_bytes()[at] != b'`' {
            at += text[at..]
                .chars()
                .next()
                .expect("remaining text")
                .len_utf8();
            continue;
        }
        let run = backticks(text, at);
        let Some(end) = closing_backticks(text, at + run, run) else {
            at += run;
            continue;
        };
        if run >= 3 {
            prose(&text[start..at], users, &mut events);
            let content = &text[at + run..end];
            let content = content
                .strip_prefix("\r\n")
                .or_else(|| content.strip_prefix('\r'))
                .or_else(|| content.strip_prefix('\n'))
                .unwrap_or(content);
            events.push(Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(
                "".into(),
            ))));
            let mut content = decode_entities(content);
            if !content.ends_with('\n') {
                content.push('\n');
            }
            events.push(Event::Text(content.into()));
            events.push(Event::End(TagEnd::CodeBlock));
            start = end + run;
        }
        at = end + run;
    }
    prose(&text[start..], users, &mut events);
    let output = lk_core::markdown::serialize(
        &events,
        lk_core::markdown::OPTIONS,
        pulldown_cmark_to_cmark::Options {
            code_block_token_count: 3,
            list_token: '-',
            increment_ordered_list_bullets: true,
            ..Default::default()
        },
    )
    .expect("serializing Slack events into a String cannot fail");
    output.trim_matches(['\r', '\n']).to_owned()
}

fn backticks(text: &str, at: usize) -> usize {
    text.as_bytes()[at..]
        .iter()
        .take_while(|&&byte| byte == b'`')
        .count()
}

fn closing_backticks(text: &str, mut at: usize, run: usize) -> Option<usize> {
    while let Some(next) = text[at..].find('`') {
        at += next;
        let length = backticks(text, at);
        if length == run {
            return Some(at);
        }
        at += length;
    }
    None
}

fn close_paragraph(events: &mut Vec<Event<'static>>, paragraph: &mut bool) {
    if std::mem::take(paragraph) {
        events.push(Event::End(TagEnd::Paragraph));
    }
}

fn close_list(events: &mut Vec<Event<'static>>, list: &mut bool) {
    if std::mem::take(list) {
        events.push(Event::End(TagEnd::List(false)));
    }
}

fn prose(text: &str, users: &HashMap<String, String>, events: &mut Vec<Event<'static>>) {
    let mut quoted = 0;
    let mut paragraph = false;
    let mut list = false;
    for raw in lk_core::markdown::lines_with_endings(text) {
        let mut line = raw.trim_end_matches(['\r', '\n']);
        let mut depth = 0;
        while let Some(rest) = line.strip_prefix('>') {
            depth += 1;
            line = rest.strip_prefix(' ').unwrap_or(rest);
        }
        if depth != quoted {
            close_paragraph(events, &mut paragraph);
            close_list(events, &mut list);
            for _ in 0..quoted {
                events.push(Event::End(TagEnd::BlockQuote(None)));
            }
            for _ in 0..depth {
                events.push(Event::Start(Tag::BlockQuote(None)));
            }
            quoted = depth;
        }
        if line.is_empty() {
            close_paragraph(events, &mut paragraph);
            close_list(events, &mut list);
            continue;
        }
        if let Some(content) = list_item(line) {
            close_paragraph(events, &mut paragraph);
            if !list {
                events.push(Event::Start(Tag::List(None)));
                list = true;
            }
            events.push(Event::Start(Tag::Item));
            events.push(Event::Start(Tag::Paragraph));
            inline(content, users, events);
            events.push(Event::End(TagEnd::Paragraph));
            events.push(Event::End(TagEnd::Item));
        } else {
            close_list(events, &mut list);
            if paragraph {
                events.push(Event::HardBreak);
            } else {
                events.push(Event::Start(Tag::Paragraph));
                paragraph = true;
            }
            inline(line, users, events);
        }
    }
    close_paragraph(events, &mut paragraph);
    close_list(events, &mut list);
    for _ in 0..quoted {
        events.push(Event::End(TagEnd::BlockQuote(None)));
    }
}

fn list_item(line: &str) -> Option<&str> {
    line.strip_prefix("- ").or_else(|| line.strip_prefix("* "))
}

fn inline(text: &str, users: &HashMap<String, String>, events: &mut Vec<Event<'static>>) {
    let mut start = 0;
    let mut at = 0;
    while at < text.len() {
        let character = text[at..].chars().next().expect("remaining text");
        if character == '`' {
            let run = backticks(text, at);
            if let Some(end) = closing_backticks(text, at + run, run) {
                plain(&text[start..at], events);
                events.push(Event::Code(decode_entities(&text[at + run..end]).into()));
                at = end + run;
                start = at;
                continue;
            }
            at += run;
            continue;
        }
        if character == '<'
            && let Some(end) = text[at + 1..].find('>').map(|end| at + 1 + end)
        {
            let mut token_events = Vec::new();
            if token(&text[at + 1..end], users, &mut token_events) {
                plain(&text[start..at], events);
                events.extend(token_events);
                at = end + 1;
                start = at;
                continue;
            }
        }
        if matches!(character, '*' | '_' | '~') && boundary_before(text, at) {
            let end = closing_marker(text, at + 1, character);
            if let Some(end) = end {
                plain(&text[start..at], events);
                let tag = match character {
                    '*' => Tag::Strong,
                    '_' => Tag::Emphasis,
                    '~' => Tag::Strikethrough,
                    _ => unreachable!(),
                };
                events.push(Event::Start(tag.clone()));
                inline(&text[at + 1..end], users, events);
                events.push(Event::End(tag.to_end()));
                at = end + 1;
                start = at;
                continue;
            }
        }
        at += character.len_utf8();
    }
    plain(&text[start..], events);
}

fn token(text: &str, users: &HashMap<String, String>, events: &mut Vec<Event<'static>>) -> bool {
    let (value, label) = text.split_once('|').map_or((text, None), |(value, label)| {
        (value, (!label.is_empty()).then_some(label))
    });
    if let Some(id) = value.strip_prefix('@')
        && label.is_none()
        && let Some(name) = users.get(id)
    {
        events.push(Event::Text(format!("@{name}").into()));
        return true;
    }
    let literal = if let Some(id) = value.strip_prefix('@') {
        Some(format!("@{}", label.unwrap_or(id)))
    } else if let Some(id) = value.strip_prefix('#') {
        Some(format!("#{}", label.unwrap_or(id)))
    } else if let Some(date) = value.strip_prefix("!date^") {
        Some(
            label
                .unwrap_or_else(|| date.split('^').next().unwrap_or(date))
                .to_owned(),
        )
    } else {
        value.strip_prefix('!').map(|special| {
            format!(
                "@{}",
                label
                    .unwrap_or_else(|| special.split('^').next().unwrap_or(special))
                    .trim_start_matches('@')
            )
        })
    };
    if let Some(literal) = literal {
        plain(&literal, events);
        return true;
    }
    let destination = decode_entities(value);
    if !url::Url::parse(&destination).is_ok_and(|url| url.scheme() != "file") {
        return false;
    }
    let tag = Tag::Link {
        link_type: if label.is_some() {
            LinkType::Inline
        } else {
            LinkType::Autolink
        },
        dest_url: destination.clone().into(),
        title: "".into(),
        id: "".into(),
    };
    events.push(Event::Start(tag));
    if let Some(label) = label {
        plain(label, events);
    } else {
        events.push(Event::Text(destination.into()));
    }
    events.push(Event::End(TagEnd::Link));
    true
}

fn closing_marker(text: &str, start: usize, marker: char) -> Option<usize> {
    let mut at = start;
    while at < text.len() {
        let character = text[at..].chars().next().expect("remaining text");
        if character == '`' {
            let run = backticks(text, at);
            if let Some(end) = closing_backticks(text, at + run, run) {
                at = end + run;
                continue;
            }
            at += run;
            continue;
        }
        if character == '<'
            && let Some(end) = text[at + 1..].find('>').map(|end| at + 1 + end)
        {
            at = end + 1;
            continue;
        }
        if character == marker
            && at > start
            && !text[..at]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
            && boundary_after(text, at + 1)
        {
            return Some(at);
        }
        at += character.len_utf8();
    }
    None
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn plain(text: &str, events: &mut Vec<Event<'static>>) {
    if text.is_empty() {
        return;
    }
    let text = decode_entities(text);
    let mut output = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(start) = rest.find(':') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let boundary = !rest[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        if let Some(end) = after.find(':')
            && boundary
            && end > 0
            && let Some(emoji) = emojis::get_by_shortcode(&after[..end])
        {
            output.push_str(emoji.as_str());
            rest = &after[end + 1..];
        } else {
            output.push(':');
            rest = after;
        }
    }
    output.push_str(rest);
    events.push(Event::Text(output.into()));
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32,
        0x1100..=0x11FF | 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xD7B0..=0xD7FF
        | 0xF900..=0xFAFF | 0xFF65..=0xFFDC | 0x20000..=0x2FA1F)
}

fn boundary_before(text: &str, at: usize) -> bool {
    text[..at].chars().next_back().is_none_or(|character| {
        character.is_whitespace()
            || is_cjk(character)
            || matches!(character, '(' | '[' | '{' | '"' | '\'')
    })
}

fn boundary_after(text: &str, at: usize) -> bool {
    text[at..].chars().next().is_none_or(|character| {
        character.is_whitespace()
            || is_cjk(character)
            || matches!(
                character,
                ')' | ']' | '}' | '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\''
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str, users: &HashMap<String, String>) -> Vec<Event<'static>> {
        let rendered = render(text, users);
        lk_core::markdown::MarkdownInput::from_text(&rendered)
            .events(lk_core::markdown::OPTIONS)
            .map(Event::into_static)
            .collect()
    }

    fn visible_text(events: &[Event<'_>]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Text(text) | Event::Code(text) => Some(text.as_ref()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn literal_slack_text_cannot_create_markdown_or_html_authority() {
        for source in [
            "[claim](../../wiki/concepts/topic.md)",
            "&lt;a href=\"../../wiki/concepts/topic.md\"&gt;claim&lt;/a&gt;",
            "&lt;!-- hidden --&gt;",
            "[claim][ref]\n\n[ref]: ../../wiki/concepts/topic.md",
            "# ordinary text",
        ] {
            let events = parsed(source, &HashMap::new());
            assert!(
                !events.iter().any(|event| matches!(
                    event,
                    Event::Start(Tag::Link { .. } | Tag::Heading { .. })
                        | Event::Html(_)
                        | Event::InlineHtml(_)
                )),
                "{source:?}: {events:?}"
            );
            let visible = events
                .iter()
                .filter_map(|event| match event {
                    Event::Text(text) => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<String>();
            assert!(
                visible.contains(&decode_entities(source).replace('\n', "")),
                "{source:?}: {visible:?}"
            );
        }
    }

    #[test]
    fn slack_tokens_emit_only_their_explicit_external_link() {
        let users = HashMap::from([(
            "U1".into(),
            "[claim](../../wiki/concepts/topic.md) <b> &lt;b&gt;".into(),
        )]);
        let source = "<@U1> <#C1|[room](local.md)> <!date^1^{date}|[day](local.md)> <https://example.com/a_(b)?x=1&amp;y=2|[claim](local.md) &lt;b&gt;>";
        let events = parsed(source, &users);
        let links = events
            .iter()
            .filter_map(|event| match event {
                Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(links, ["https://example.com/a_(b)?x=1&y=2"]);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Html(_) | Event::InlineHtml(_)))
        );
        assert!(visible_text(&events).contains("&lt;b&gt;"));
    }

    #[test]
    fn bare_link_destinations_never_receive_prose_transforms() {
        for (source, destination) in [
            ("https://example.com/:tada:", "https://example.com/:tada:"),
            ("mailto:person@example.com", "mailto:person@example.com"),
            (
                "https://example.com/?x=&amp;amp;y",
                "https://example.com/?x=&amp;y",
            ),
        ] {
            let events = parsed(&format!("<{source}>"), &HashMap::new());
            let links = events
                .iter()
                .filter_map(|event| match event {
                    Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(links, [destination]);
        }
    }

    #[test]
    fn code_delimiters_in_link_labels_remain_literal() {
        let events = parsed("<https://example.com|```literal```>", &HashMap::new());
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Start(Tag::Link { .. })))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Code(_) | Event::Start(Tag::CodeBlock(_))))
        );
        assert_eq!(visible_text(&events), "```literal```");
    }

    #[test]
    fn slack_code_blocks_and_spans_preserve_literal_tokens() {
        let users = HashMap::from([("U1".into(), "Alice".into())]);
        let source = "`<@U1> *bold* :tada: [claim](local.md)`\n\n```<@U1> &lt;b&gt;\n`literal` :tada:```\n\n<@U1> :tada:";
        let events = parsed(source, &users);
        assert!(events.iter().any(|event| matches!(event, Event::Code(text) if text.as_ref() == "<@U1> *bold* :tada: [claim](local.md)")));
        assert!(events.iter().any(|event| matches!(event, Event::Text(text) if text.contains("<@U1> <b>\n`literal` :tada:"))));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Text(text) if text.contains("@Alice 🎉")))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Start(Tag::Link { .. })))
        );
        let mut code = false;
        let mut following_prose = false;
        for event in &events {
            match event {
                Event::Start(Tag::CodeBlock(_)) => code = true,
                Event::End(TagEnd::CodeBlock) => code = false,
                Event::Text(text) if !code && text.contains("@Alice 🎉") => {
                    following_prose = true
                }
                _ => {}
            }
        }
        assert!(
            following_prose,
            "following text escaped its Slack code block: {events:?}"
        );
    }

    #[test]
    fn slack_styles_quotes_and_lists_remain_structured() {
        let events = parsed(
            "> *bold* _italic_ ~deleted~\n> second\n\n- first\n- second\n\n1. one\n2. two",
            &HashMap::new(),
        );
        for expected in [
            Tag::Strong,
            Tag::Emphasis,
            Tag::Strikethrough,
            Tag::BlockQuote(None),
            Tag::List(None),
        ] {
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, Event::Start(tag) if tag == &expected)),
                "{expected:?}: {events:?}"
            );
        }
    }

    #[test]
    fn ordered_markers_preserve_declared_numbers_and_line_breaks() {
        let events = parsed("1. first\n5. fifth", &HashMap::new());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Start(Tag::List(Some(_)))))
        );
        let mut visible = String::new();
        for event in events {
            match event {
                Event::Text(text) => visible.push_str(&text),
                Event::HardBreak => visible.push('\n'),
                _ => {}
            }
        }
        assert_eq!(visible, "1. first\n5. fifth");
    }

    #[test]
    fn markers_inside_code_do_not_close_surrounding_styles() {
        let events = parsed("*bold `a * b` tail*", &HashMap::new());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Code(text) if text.as_ref() == "a * b"))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Text(text) if text.ends_with(" tail")))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Start(Tag::Strong)))
                .count(),
            1
        );
    }
}
