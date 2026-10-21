//! Embed standalone source Markdown without letting its syntax own the surrounding page.

use minijinja::{Error, ErrorKind};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};

const OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_FOOTNOTES);

/// Scalar fields are literal inline text; their original values remain in metadata.
pub(crate) fn inline_text(value: &str) -> String {
    let mut output = String::new();
    for word in value.split_whitespace() {
        if !output.is_empty() {
            output.push(' ');
        }
        for c in word.chars() {
            if matches!(
                c,
                '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '#' | '!' | '|'
            ) {
                output.push('\\');
            }
            output.push(c);
        }
    }
    output
}

fn serialize(events: &[Event<'_>]) -> Result<String, Error> {
    let mut output = String::new();
    pulldown_cmark_to_cmark::cmark(events.iter(), &mut output, OPTIONS)
        .map_err(|error| Error::new(ErrorKind::InvalidOperation, error.to_string()))?;
    Ok(output)
}

/// A root heading at this exact byte offset proves the preceding fragment has ended.
fn has_sibling_boundary(fragment: &str) -> bool {
    let offset = fragment.len() + 2;
    let probe = format!("{fragment}\n\n## boundary\n");
    let mut depth = 0;
    for (event, range) in Parser::new_ext(&probe, OPTIONS).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0
                    && matches!(
                        tag,
                        Tag::Heading {
                            level: HeadingLevel::H2,
                            ..
                        }
                    )
                    && range.start == offset
                {
                    return true;
                }
                depth += 1;
            }
            Event::End(_) => depth -= 1,
            _ => {}
        }
    }
    false
}

fn shift_heading_level(level: HeadingLevel, shift: usize) -> HeadingLevel {
    match (level as usize + shift).min(6) {
        1 => HeadingLevel::H1,
        2 => HeadingLevel::H2,
        3 => HeadingLevel::H3,
        4 => HeadingLevel::H4,
        5 => HeadingLevel::H5,
        _ => HeadingLevel::H6,
    }
}

pub(crate) fn render(value: &str) -> Result<String, Error> {
    let parsed: Vec<_> = Parser::new_ext(value, OPTIONS).collect();
    let shallowest = parsed
        .iter()
        .filter_map(|event| match event {
            Event::Start(Tag::Heading { level, .. }) => Some(*level as usize),
            _ => None,
        })
        .min()
        .unwrap_or(4);
    let shift = 4usize.saturating_sub(shallowest);
    let namespace = blake3::hash(value.as_bytes()).to_hex();
    let footnote = |name: &str| format!("lk-{namespace}-{name}");
    let mut events = Vec::with_capacity(parsed.len());
    let mut input = parsed.into_iter();
    while let Some(event) = input.next() {
        match event {
            Event::Start(Tag::Heading {
                level: old,
                id,
                classes,
                attrs,
            }) => {
                events.push(Event::Start(Tag::Heading {
                    level: shift_heading_level(old, shift),
                    id,
                    classes,
                    attrs,
                }));
            }
            Event::End(TagEnd::Heading(old)) => {
                events.push(Event::End(TagEnd::Heading(shift_heading_level(old, shift))))
            }
            Event::Start(Tag::Link {
                dest_url, title, ..
            }) => events.push(Event::Start(Tag::Link {
                link_type: LinkType::Inline,
                dest_url,
                title,
                id: "".into(),
            })),
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => events.push(Event::Start(Tag::Image {
                link_type: LinkType::Inline,
                dest_url,
                title,
                id: "".into(),
            })),
            Event::Start(Tag::FootnoteDefinition(name)) => events.push(Event::Start(
                Tag::FootnoteDefinition(footnote(&name).into()),
            )),
            Event::FootnoteReference(name) => {
                events.push(Event::FootnoteReference(footnote(&name).into()))
            }
            Event::End(TagEnd::CodeBlock) => {
                if !matches!(events.last(), Some(Event::Text(text)) if text.ends_with('\n')) {
                    events.push(Event::Text("\n".into()));
                }
                events.push(Event::End(TagEnd::CodeBlock));
            }
            Event::Start(Tag::HtmlBlock) => {
                let mut block = String::new();
                for event in input.by_ref() {
                    match event {
                        Event::End(TagEnd::HtmlBlock) => break,
                        Event::Html(text) => block.push_str(&text),
                        _ => {
                            return Err(Error::new(
                                ErrorKind::InvalidOperation,
                                "invalid HTML block events",
                            ));
                        }
                    }
                }
                if has_sibling_boundary(&block) {
                    events.extend([
                        Event::Start(Tag::HtmlBlock),
                        Event::Html(block.into()),
                        Event::End(TagEnd::HtmlBlock),
                    ]);
                } else {
                    // EOF-terminated HTML is shown literally; inventing closing tags could change its meaning.
                    if !block.ends_with('\n') {
                        block.push('\n');
                    }
                    events.extend([
                        Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced("html".into()))),
                        Event::Text(block.into()),
                        Event::End(TagEnd::CodeBlock),
                    ]);
                }
            }
            other => events.push(other),
        }
    }
    let output = serialize(&events)?;
    if !has_sibling_boundary(&output) {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "source Markdown does not preserve the surrounding page boundary",
        ));
    }
    Ok(output)
}

/// Serialize the complete link so provider URL punctuation cannot become page syntax.
pub(crate) fn link(url: &str, label: &str) -> Result<String, Error> {
    serialize(&[
        Event::Start(Tag::Paragraph),
        Event::Start(Tag::Link {
            link_type: LinkType::Inline,
            dest_url: url.into(),
            title: "".into(),
            id: "".into(),
        }),
        Event::Text(label.into()),
        Event::End(TagEnd::Link),
        Event::End(TagEnd::Paragraph),
    ])
    .map(|value| value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_are_literal_single_line_text() {
        let value = "Report\n\n## Summary\n\n*TITLE* [x](evil.md) <script>";
        let rendered = format!("# {}\n\n## Summary\n\nowned\n", inline_text(value));
        let headings: Vec<_> = Parser::new(&rendered)
            .filter_map(|event| match event {
                Event::Start(Tag::Heading { level, .. }) => Some(level),
                _ => None,
            })
            .collect();
        assert_eq!(headings, [HeadingLevel::H1, HeadingLevel::H2]);
        assert!(
            !Parser::new(&rendered).any(|event| matches!(
                event,
                Event::Start(Tag::Link { .. }) | Event::InlineHtml(_)
            ))
        );
    }

    #[test]
    fn standalone_fragments_preserve_sibling_sections() {
        for value in [
            "# Topic\n\n```rust\nlet answer = 42;",
            "~~~\ncode\n## Not a heading",
            "Topic\n=====\n\nDetails\n-------\n\ntext",
            "<!-- unclosed\n\n## not a heading",
            "<script>\nlet x = 1;",
            "<table><tr><td>value</td></tr></table>\n",
            "<div>\n```\n## raw HTML\n</div>\n",
            "ordinary **text**",
        ] {
            let output = render(value).unwrap_or_else(|error| panic!("{value:?}: {error}"));
            assert!(has_sibling_boundary(&output), "{value:?}: {output}");
            let page = format!("## Source\n\n{output}\n\n## Related\n\nowned\n");
            assert_eq!(
                crate::section_body(&page, "Related").unwrap().trim(),
                "owned",
                "{page}"
            );
            assert!(!Parser::new_ext(&output, OPTIONS).any(|event| matches!(
                event,
                Event::Start(Tag::Heading {
                    level: HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3,
                    ..
                })
            )));
        }
    }

    #[test]
    fn raw_html_is_preserved_unless_it_consumes_the_next_section() {
        let table = "<table><tr><td colspan=\"2\">value</td></tr></table>\n";
        assert!(render(table).unwrap().contains(table.trim()));
        for value in ["<!-- unclosed", "<script>unterminated"] {
            let output = render(value).unwrap();
            assert!(output.contains("```html"), "{output}");
            assert!(output.contains(value), "{output}");
        }
    }

    #[test]
    fn independently_resolved_references_and_footnotes_do_not_mix() {
        let a =
            render("[link][r] ![image][r] note[^1]\n\n[r]: https://a.test\n\n[^1]: first").unwrap();
        let b = render("[link][r] ![image][r] note[^1]\n\n[r]: https://b.test\n\n[^1]: second")
            .unwrap();
        let combined = format!("{a}\n\n{b}");
        let destinations: Vec<_> = Parser::new_ext(&combined, OPTIONS)
            .filter_map(|event| match event {
                Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                    Some(dest_url.into_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            destinations,
            [
                "https://a.test",
                "https://a.test",
                "https://b.test",
                "https://b.test"
            ]
        );
        let names: Vec<_> = Parser::new_ext(&combined, OPTIONS)
            .filter_map(|event| match event {
                Event::Start(Tag::FootnoteDefinition(name)) => Some(name.into_string()),
                _ => None,
            })
            .collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
    }

    #[test]
    fn provider_links_escape_structure_without_reencoding_existing_urls() {
        for url in [
            "https://host.test/a%20b#section",
            "https://host.test/a)b",
            "https://host.test/\n\n## Summary\n",
        ] {
            let output = link(url, "Original [source]").unwrap();
            assert!(
                !Parser::new(&output)
                    .any(|event| matches!(event, Event::Start(Tag::Heading { .. })))
            );
            let dest = Parser::new(&output)
                .find_map(|event| match event {
                    Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.into_string()),
                    _ => None,
                })
                .unwrap();
            assert_eq!(dest, url);
        }
    }
}
