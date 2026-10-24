//! CommonMark document boundaries for source and page composition.

use super::{MarkdownInput, OPTIONS, has_sibling_boundary, serialize};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Tag, TagEnd};

#[derive(Debug, thiserror::Error)]
pub enum FragmentError {
    #[error("invalid HTML block events")]
    HtmlBlock,
    #[error(transparent)]
    Serialization(#[from] pulldown_cmark_to_cmark::Error),
    #[error("source Markdown does not preserve the surrounding page boundary")]
    Boundary,
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

/// Contain one standalone Markdown document before composing it with other content.
/// References stay local and EOF syntax cannot consume a later sibling; headings
/// shift together only when shallower than the requested minimum. The namespace
/// identifies the original document when independent sources contain equal text.
pub fn render_fragment(
    value: &str,
    minimum_heading_level: HeadingLevel,
    namespace: &str,
) -> Result<String, FragmentError> {
    let markdown = MarkdownInput::from_text(value);
    let parsed: Vec<_> = markdown.events(OPTIONS).collect();
    let shallowest = parsed
        .iter()
        .filter_map(|event| match event {
            Event::Start(Tag::Heading { level, .. }) => Some(*level as usize),
            _ => None,
        })
        .min()
        .unwrap_or(minimum_heading_level as usize);
    let shift = (minimum_heading_level as usize).saturating_sub(shallowest);
    let namespace = blake3::hash(namespace.as_bytes()).to_hex();
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
                        Event::Html(text) | Event::Text(text) => block.push_str(&text),
                        _ => {
                            return Err(FragmentError::HtmlBlock);
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
    let output = serialize(&events, OPTIONS, Default::default())?;
    if !has_sibling_boundary(&output) {
        return Err(FragmentError::Boundary);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_references_images_and_footnotes_survive_composition_and_page_rendering() {
        for (first_ending, second_ending) in [("\n", "\n"), ("\r", "\r\n"), ("\r\n", "\r")] {
            let first = "# First\n\n[A][ref] ![A image][ref] [unresolved][late] note[^1]\n\n[ref]: https://one.example/\n\n[^1]: First footnote\n".replace('\n', first_ending);
            let second = "## Second\n\n[B][ref] ![B image][ref] note[^1]\n\n[ref]: https://two.example/\n[late]: https://late.example/\n\n[^1]: Second footnote\n".replace('\n', second_ending);
            let first = render_fragment(&first, HeadingLevel::H1, "first").unwrap();
            let second = render_fragment(&second, HeadingLevel::H1, "second").unwrap();
            let composed = format!("{first}\n\n{second}");
            let page = render_fragment(&composed, HeadingLevel::H4, "page").unwrap();
            for output in [&composed, &page] {
                let input = MarkdownInput::from_text(output);
                let events = input.events(OPTIONS).collect::<Vec<_>>();
                let destinations = events
                    .iter()
                    .filter_map(|event| match event {
                        Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                            Some(dest_url.as_ref())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    destinations,
                    [
                        "https://one.example/",
                        "https://one.example/",
                        "https://two.example/",
                        "https://two.example/"
                    ],
                    "{output}"
                );
                let footnotes = events
                    .iter()
                    .filter_map(|event| match event {
                        Event::Start(Tag::FootnoteDefinition(name)) => Some(name.as_ref()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(footnotes.len(), 2, "{output}");
                assert_ne!(footnotes[0], footnotes[1], "{output}");
                assert!(
                    events.iter().any(
                        |event| matches!(event, Event::Text(text) if text.contains("unresolved"))
                    ),
                    "{output}"
                );
            }
            assert_eq!(
                super::super::root_headings(&composed)[0].level,
                HeadingLevel::H1
            );
            assert_eq!(
                super::super::root_headings(&page)[0].level,
                HeadingLevel::H4
            );
        }
    }

    #[test]
    fn equal_notes_from_distinct_documents_keep_distinct_footnotes() {
        let source = "Note[^1]\n\n[^1]: Same footnote";
        let first = render_fragment(source, HeadingLevel::H1, "first-file").unwrap();
        let second = render_fragment(source, HeadingLevel::H1, "second-file").unwrap();
        let composed = format!("{first}\n\n{second}");
        let input = MarkdownInput::from_text(&composed);
        let names = input
            .events(OPTIONS)
            .filter_map(|event| match event {
                Event::Start(Tag::FootnoteDefinition(name)) => Some(name.into_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2, "{composed}");
        assert_ne!(names[0], names[1]);
    }

    #[test]
    fn eof_code_and_html_cannot_consume_the_next_document() {
        for source in [
            "```text\nFirst code",
            "~~~\nFirst code",
            "<!-- unclosed",
            "<script>\nFirst code",
            "<style>\nFirst code",
        ] {
            let first = render_fragment(source, HeadingLevel::H1, "first").unwrap();
            let second = render_fragment(
                "## Second\n\n[Second](https://two.example/)",
                HeadingLevel::H1,
                "second",
            )
            .unwrap();
            let composed = format!("{first}\n\n{second}");
            assert!(
                super::super::root_headings(&composed)
                    .iter()
                    .any(|heading| heading.level == HeadingLevel::H2),
                "{composed}"
            );
            assert_eq!(
                crate::link::first_external_dest(&composed).as_deref(),
                Some("https://two.example/"),
                "{composed}"
            );
            assert!(has_sibling_boundary(&composed), "{composed}");
        }
    }
}
