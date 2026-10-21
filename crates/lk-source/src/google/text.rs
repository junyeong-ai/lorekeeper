use pulldown_cmark::{Event, Tag};

use crate::SourceError;

/// The declared text format, independent of filename and HTTP charset metadata.
pub(super) enum ContentFormat {
    Plain,
    Markdown,
    Html,
}

impl ContentFormat {
    pub(super) fn from_mime(mime_type: &str) -> Result<Self, SourceError> {
        match mime_type {
            "text/plain" => Ok(Self::Plain),
            "text/markdown" | "text/x-markdown" => Ok(Self::Markdown),
            "text/html" => Ok(Self::Html),
            _ => Err(SourceError::Parse(format!(
                "unsupported Google text MIME type {mime_type:?}"
            ))),
        }
    }

    pub(super) fn markdown(&self, content: &str) -> Result<String, SourceError> {
        let markdown = match self {
            Self::Plain => return crate::fetch::literal_markdown(content),
            Self::Markdown => content.to_owned(),
            Self::Html => {
                // Conversion can discard <base>, while its meaning still affects links.
                if crate::manual_bundle::remote_html_references(content).1 {
                    return Err(dependency_error(
                        "HTML base has no captured source authority",
                    ));
                }
                crate::markdown::html_to_markdown(content)
            }
        };
        let mut has_raw_html = false;
        let input = lk_core::markdown::MarkdownInput::from_text(&markdown);
        for event in input.events(lk_core::markdown::OPTIONS) {
            match event {
                Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                    validate_destination(&dest_url)?;
                }
                Event::Html(_) | Event::InlineHtml(_) => has_raw_html = true,
                _ => {}
            }
        }
        if has_raw_html {
            // Render the complete stream so generated paragraphs and escaped code
            // retain the same DOM/namespace context as the actual Markdown page.
            let mut html = String::new();
            pulldown_cmark::html::push_html(&mut html, input.events(lk_core::markdown::OPTIONS));
            let (references, has_base) = crate::manual_bundle::remote_html_references(&html);
            if has_base {
                return Err(dependency_error(
                    "HTML base has no captured source authority",
                ));
            }
            for destination in references {
                validate_destination(&destination)?;
            }
        }
        Ok(markdown)
    }
}

fn dependency_error(detail: &str) -> SourceError {
    SourceError::Parse(format!(
        "Google structured text has an unresolved remote dependency ({detail}); use absolute source URLs or a self-contained manual capture"
    ))
}

fn validate_destination(destination: &str) -> Result<(), SourceError> {
    if destination.is_empty() || destination.starts_with('#') {
        return Ok(());
    }
    if lk_core::link::is_external(destination)
        && !destination.contains('\\')
        && url::Url::parse(destination).is_ok_and(|url| url.scheme() != "file")
    {
        return Ok(());
    }
    Err(dependency_error(
        "relative or local URL has no captured source base",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::Parser;

    #[test]
    fn cr_fenced_examples_are_literal_and_do_not_hide_later_dependencies() {
        for literal in [
            "```html\r[ghost](missing.md)\r<img src='missing.png'>\r```\r\r<pre>\r[html literal](missing-html.md)\r</pre>\r\r",
            "```html\r\n[ghost](missing.md)\r<img src='missing.png'>\r```\n\r<pre>\r[html literal](missing-html.md)\n</pre>\r\n\r",
        ] {
            let valid = format!("{literal}[live](https://example.com/live.md)\r");
            assert_eq!(ContentFormat::Markdown.markdown(&valid).unwrap(), valid);
            let invalid = format!("{literal}[live](relative.md)\r");
            assert!(
                ContentFormat::Markdown.markdown(&invalid).is_err(),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn structured_text_requires_explicit_dependency_addresses() {
        for source in [
            "[link](../concepts/owned.md)",
            "![image](/assets/remote.png)",
            "[link][ref]\n\n[ref]: relative.md",
            "![image][ref]\n\n[ref]: relative.png",
            "<a href=\"relative.md\">link</a>",
            "<img src=\"relative.png\">",
            "<iframe src=\"relative.html\"></iframe>",
            "<svg><image href=\"relative.png\" /></svg>",
            "<svg><use href=\"relative.svg#shape\" /></svg>",
            "<table background=\"relative.png\"><tr><td>x</td></tr></table>",
            "<base href=\"https://example.com/\">\n\n[link](#part)",
            "[link](file:///tmp/local.md)",
            "[link](//example.com/remote.md)",
            "[link](< https://example.com/>)",
        ] {
            assert!(
                ContentFormat::Markdown.markdown(source).is_err(),
                "{source}"
            );
        }
        assert!(
            ContentFormat::Html
                .markdown("<base href=\"https://example.com/\"><p>body</p>")
                .is_err()
        );
    }

    #[test]
    fn explicit_links_and_literal_examples_keep_their_source_bytes() {
        let markdown = "[remote](https://example.com/a) ![image](https://example.com/a.png) [part](#part) [mail](mailto:a@example.com)\n\n`[literal](relative.md)`\n\n```html\n<base href=\"relative\"><img src=\"relative.png\">\n```\n\n<!-- <a href=\"relative.md\"> -->\n\n<div style=\"color:red\">text</div>\n<script>const example = 'relative.md';</script>\n<span href=\"literal\" src=\"literal.md\">unknown attributes</span>\n<svg><text href=\"literal.md\" src=\"literal.png\">label</text></svg>\n";
        assert_eq!(
            ContentFormat::Markdown.markdown(markdown).unwrap(),
            markdown
        );
        let namespace_exit = "<svg>\n\nA paragraph exits the SVG namespace.\n\n<image href=\"literal.md\">\n</svg>\n";
        assert_eq!(
            ContentFormat::Markdown.markdown(namespace_exit).unwrap(),
            namespace_exit
        );
        let html = "<p style=\"color:red\"><a href=\"https://example.com/a\">remote</a> <a href=\"#part\">part</a></p><pre><code>&lt;a href=\"relative.md\"&gt;</code></pre><!-- <base href=\"relative\"> -->";
        let projected = ContentFormat::Html.markdown(html).unwrap();
        assert_eq!(
            Parser::new_ext(&projected, lk_core::markdown::OPTIONS)
                .filter_map(|event| match event {
                    Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.into_string()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["https://example.com/a", "#part"]
        );
    }
}
