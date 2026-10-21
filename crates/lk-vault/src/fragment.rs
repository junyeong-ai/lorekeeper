//! Embed standalone source Markdown without letting its syntax own the surrounding page.

use lk_core::markdown::{MarkdownInput, OPTIONS, has_sibling_boundary};
use minijinja::{Error, ErrorKind};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Tag, TagEnd};
use std::path::Path;

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

/// Keep vault-local link and image targets when Markdown moves between page addresses.
/// Parsed destinations cover reference links without interpreting literal code or HTML.
pub fn relocate_markdown_links(
    value: &str,
    from_page: &Path,
    to_page: &Path,
) -> Result<String, Error> {
    if from_page.parent() == to_page.parent() {
        return Ok(value.to_string());
    }
    let mut changed = false;
    let mut events = Vec::new();
    let input = MarkdownInput::from_text(value);
    for mut event in input.events(OPTIONS) {
        if let Event::Start(
            Tag::Link {
                link_type,
                dest_url,
                id,
                ..
            }
            | Tag::Image {
                link_type,
                dest_url,
                id,
                ..
            },
        ) = &mut event
        {
            let (destination, anchor) = lk_core::link::split_dest_anchor(dest_url);
            if !destination.is_empty() && !lk_core::link::is_external(destination) {
                let target = lk_core::link::resolve_dest(
                    from_page,
                    &lk_core::link::decode_dest(destination),
                )
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidOperation,
                        "relocated Markdown destination escapes the vault",
                    )
                })?;
                let relocated =
                    format!("{}{anchor}", lk_core::link::relative_dest(to_page, &target));
                if relocated != dest_url.as_ref() {
                    *dest_url = relocated.into();
                    *link_type = LinkType::Inline;
                    *id = "".into();
                    changed = true;
                }
            }
        }
        events.push(event);
    }
    if changed {
        serialize(&events)
    } else {
        Ok(value.to_string())
    }
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

/// Render standalone source Markdown inside a page's owned sections. Source text
/// remains separate from this presentation; headings, references and EOF syntax
/// cannot claim the surrounding document.
pub fn render_markdown_fragment(value: &str) -> Result<String, Error> {
    let markdown = MarkdownInput::from_text(value);
    let parsed: Vec<_> = markdown.events(OPTIONS).collect();
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
    use pulldown_cmark::Parser;

    #[test]
    fn cr_code_and_html_literals_survive_fragment_rendering_and_relocation() {
        for source in [
            "```text\r# Literal\r[ghost](ghost.md)\r```\r\r<pre>\r[html literal](html.md)\r</pre>\r\r[live](live.md)\r",
            "```text\r\n# Literal\r[ghost](ghost.md)\r```\n\r<pre>\r[html literal](html.md)\r</pre>\r\n\r[live](live.md)\n",
        ] {
            let relocated = relocate_markdown_links(
                source,
                Path::new("wiki/documents/a.md"),
                Path::new("wiki/documents/nested/a.md"),
            )
            .unwrap();
            for (output, expected_destination) in [
                (relocated, "../live.md"),
                (render_markdown_fragment(source).unwrap(), "live.md"),
            ] {
                let input = MarkdownInput::from_text(&output);
                let mut destinations = Vec::new();
                let mut code = String::new();
                let mut html = String::new();
                let mut in_code = false;
                for event in input.events(OPTIONS) {
                    match event {
                        Event::Start(Tag::Link { dest_url, .. }) => {
                            destinations.push(dest_url.into_string())
                        }
                        Event::Start(Tag::CodeBlock(_)) => in_code = true,
                        Event::End(TagEnd::CodeBlock) => in_code = false,
                        Event::Text(text) if in_code => code.push_str(&text),
                        Event::Html(text) => html.push_str(&text),
                        _ => {}
                    }
                }
                assert_eq!(destinations, [expected_destination], "{output:?}");
                assert_eq!(code, "# Literal\n[ghost](ghost.md)\n", "{output:?}");
                assert_eq!(
                    html.replace("\r\n", "\n"),
                    "<pre>\n[html literal](html.md)\n</pre>\n",
                    "{output:?}"
                );
                assert!(has_sibling_boundary(&output), "{output:?}");
            }
        }
    }

    #[test]
    fn cr_nested_code_and_html_stay_inside_their_quote_or_list() {
        for (source, expected_code, expected_html) in [
            (
                "> ```python\r> print('first')\r> \r> print('second')\r> ```\r\r[live](live.md)\r",
                "print('first')\n\nprint('second')\n",
                "",
            ),
            (
                "- ```python\r\n  print('first')\r  \r  print('second')\r  ```\n\r[live](live.md)\n",
                "print('first')\n\nprint('second')\n",
                "",
            ),
            (
                "> <div>\r> literal\r> </div>\r\r[live](live.md)\r",
                "",
                "<div>\nliteral\n</div>\n",
            ),
            (
                "- <div>\r\n  literal\r  </div>\r\n\r[live](live.md)\n",
                "",
                "<div>\nliteral\n</div>\n",
            ),
        ] {
            for output in [
                render_markdown_fragment(source).unwrap(),
                relocate_markdown_links(
                    source,
                    Path::new("wiki/documents/a.md"),
                    Path::new("wiki/documents/nested/a.md"),
                )
                .unwrap(),
            ] {
                let input = MarkdownInput::from_text(&output);
                let mut depth = 0;
                let mut in_code = false;
                let mut code = String::new();
                let mut html = String::new();
                for event in input.events(OPTIONS) {
                    match event {
                        Event::Start(Tag::BlockQuote(_) | Tag::Item) => depth += 1,
                        Event::End(TagEnd::BlockQuote(_) | TagEnd::Item) => depth -= 1,
                        Event::Start(Tag::CodeBlock(_)) => {
                            assert!(depth > 0, "{output:?}");
                            in_code = true;
                        }
                        Event::End(TagEnd::CodeBlock) => in_code = false,
                        Event::Text(text) if in_code => code.push_str(&text),
                        Event::Html(text) => {
                            assert!(depth > 0, "{output:?}");
                            html.push_str(&text);
                        }
                        _ => {}
                    }
                }
                assert_eq!(code, expected_code, "{output:?}");
                assert_eq!(html.replace("\r\n", "\n"), expected_html, "{output:?}");
                assert!(has_sibling_boundary(&output), "{output:?}");
            }
        }
    }

    #[test]
    fn relocation_keeps_parsed_link_and_image_addresses_with_titles_and_anchors() {
        let source = "[Proof](../../.lorekeeper/files/a%20b.md#part \"A (title)\")\n\n![Image][asset]\n\n[asset]: ../../.lorekeeper/files/image.png \"Figure\"\n\n[Here](#local) [Web](https://example.com/a)\n\n`[literal](../../keep.md)`\n\n```text\n![literal](../../keep.png)\n```\n";
        let output = relocate_markdown_links(
            source,
            Path::new("wiki/documents/document.md"),
            Path::new("wiki/documents/nested/guide.md"),
        )
        .unwrap();
        let targets: Vec<_> = Parser::new_ext(&output, OPTIONS)
            .filter_map(|event| match event {
                Event::Start(
                    Tag::Link {
                        dest_url, title, ..
                    }
                    | Tag::Image {
                        dest_url, title, ..
                    },
                ) => Some((dest_url.into_string(), title.into_string())),
                _ => None,
            })
            .collect();
        assert_eq!(
            targets,
            [
                (
                    "../../../.lorekeeper/files/a%20b.md#part".into(),
                    "A (title)".into()
                ),
                (
                    "../../../.lorekeeper/files/image.png".into(),
                    "Figure".into()
                ),
                ("#local".into(), String::new()),
                ("https://example.com/a".into(), String::new()),
            ]
        );
        assert!(output.contains("`[literal](../../keep.md)`"));
        assert!(output.contains("![literal](../../keep.png)\n"));
        assert!(
            relocate_markdown_links(
                "[Escape](../../../outside.md)",
                Path::new("wiki/documents/document.md"),
                Path::new("wiki/documents/nested/guide.md"),
            )
            .is_err()
        );
    }

    #[test]
    fn relocation_preserves_bytes_when_destinations_do_not_move() {
        for source in [
            "[Web](https://example.com)\r\n\r\n[Here](#local)\r\n",
            "```text\r\n[Literal](relative.md)\r\n```\r\n",
            "<div>\n[Literal](relative.md)\n</div>\n",
            "```text\r[Literal](relative.md)\r```\r",
            "> <div>\r> literal\r> </div>\r",
        ] {
            assert_eq!(
                relocate_markdown_links(
                    source,
                    Path::new("wiki/documents/document.md"),
                    Path::new("wiki/documents/nested/guide.md"),
                )
                .unwrap(),
                source
            );
        }
        let source = "[Proof](../../.lorekeeper/files/proof.txt)\r\n";
        assert_eq!(
            relocate_markdown_links(
                source,
                Path::new("wiki/documents/document.md"),
                Path::new("wiki/documents/guide.md"),
            )
            .unwrap(),
            source
        );
    }

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
            let output = render_markdown_fragment(value)
                .unwrap_or_else(|error| panic!("{value:?}: {error}"));
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
        assert!(
            render_markdown_fragment(table)
                .unwrap()
                .contains(table.trim())
        );
        for value in ["<!-- unclosed", "<script>unterminated"] {
            let output = render_markdown_fragment(value).unwrap();
            assert!(output.contains("```html"), "{output}");
            assert!(output.contains(value), "{output}");
        }
    }

    #[test]
    fn independently_resolved_references_and_footnotes_do_not_mix() {
        let a = render_markdown_fragment(
            "[link][r] ![image][r] note[^1]\n\n[r]: https://a.test\n\n[^1]: first",
        )
        .unwrap();
        let b = render_markdown_fragment(
            "[link][r] ![image][r] note[^1]\n\n[r]: https://b.test\n\n[^1]: second",
        )
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
