//! Structured remote text keeps source URI authority separate from vault addresses.

use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{NodeData, RcDom, SerializableHandle};
use pulldown_cmark::{Event, Tag};
use url::Url;

use crate::SourceError;

fn dependency_error(detail: &str) -> SourceError {
    SourceError::Parse(format!(
        "remote structured text has an unresolved dependency ({detail}); use absolute source URLs or a self-contained manual capture"
    ))
}

/// A remote source base must be observed/configured HTTP authority, never a vault path.
pub(crate) fn validate_destination(
    destination: &str,
    base: Option<&Url>,
) -> Result<String, SourceError> {
    if base.is_none() && (destination.is_empty() || destination.starts_with('#')) {
        return Ok(destination.to_owned());
    }
    if destination.contains('\\') || destination.trim() != destination {
        return Err(dependency_error("invalid remote URL"));
    }
    if let Ok(url) = Url::parse(destination) {
        return if url.scheme() == "file" {
            Err(dependency_error("local file URL"))
        } else {
            Ok(destination.to_owned())
        };
    }
    let base = base
        .filter(|base| matches!(base.scheme(), "http" | "https"))
        .ok_or_else(|| dependency_error("relative URL has no captured source base"))?;
    base.join(destination)
        .map(|url| url.to_string())
        .map_err(|_| dependency_error("invalid relative URL"))
}

pub(crate) fn is_url_attribute(tag: &str, key: &str, namespace: &str) -> bool {
    let html_url = namespace == "http://www.w3.org/1999/xhtml"
        && matches!(
            (tag, key),
            ("a" | "area" | "link" | "base", "href")
                | (
                    "img"
                        | "audio"
                        | "video"
                        | "source"
                        | "track"
                        | "input"
                        | "embed"
                        | "iframe"
                        | "script",
                    "src"
                )
                | ("video", "poster")
                | ("object", "data")
                | ("form", "action")
                | ("input" | "button", "formaction")
                | ("blockquote" | "q" | "ins" | "del", "cite")
                | ("img" | "iframe", "longdesc")
                | ("img" | "input" | "object", "usemap")
                | ("body" | "table" | "td" | "th", "background")
                | ("html", "manifest")
                | ("object" | "applet", "codebase" | "classid")
                | ("applet", "code")
                | ("command", "icon")
        );
    let svg_url = namespace == "http://www.w3.org/2000/svg"
        && key == "href"
        && matches!(
            tag,
            "a" | "image"
                | "use"
                | "feImage"
                | "linearGradient"
                | "radialGradient"
                | "pattern"
                | "filter"
                | "textPath"
                | "mpath"
                | "animate"
                | "animateMotion"
                | "animateTransform"
                | "set"
                | "discard"
                | "script"
        );
    html_url || svg_url
}

fn elements(dom: &RcDom, include_templates: bool) -> Vec<markup5ever_rcdom::Handle> {
    let mut elements = Vec::new();
    let mut stack = vec![dom.document.clone()];
    while let Some(node) = stack.pop() {
        stack.extend(node.children.borrow().iter().rev().cloned());
        if let NodeData::Element {
            template_contents, ..
        } = &node.data
        {
            if include_templates && let Some(contents) = template_contents.borrow().as_ref() {
                stack.push(contents.clone());
            }
            elements.push(node);
        }
    }
    elements
}

pub(crate) fn visit_html_elements(
    content: &str,
    mut visit: impl FnMut(&str, &str, &[html5ever::Attribute]),
) {
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(content);
    for node in elements(&dom, true) {
        if let NodeData::Element { name, attrs, .. } = &node.data {
            visit(name.local.as_ref(), name.ns.as_ref(), &attrs.borrow());
        }
    }
}

/// Resolve declared HTML before conversion can discard its base element or URL attributes.
pub(crate) fn html(content: &str, base: Option<&Url>) -> Result<String, SourceError> {
    let normalized = crate::markdown::normalize_storage_format(content);
    let dom =
        html5ever::parse_document(RcDom::default(), Default::default()).one(normalized.as_ref());
    let active_elements = elements(&dom, false);
    let elements = elements(&dom, true);
    let declared_base = active_elements.iter().find_map(|node| match &node.data {
        NodeData::Element { name, attrs, .. }
            if name.ns.as_ref() == "http://www.w3.org/1999/xhtml"
                && name.local.as_ref() == "base" =>
        {
            attrs
                .borrow()
                .iter()
                .find(|attr| attr.name.local.as_ref() == "href")
                .map(|attr| attr.value.to_string())
        }
        _ => None,
    });
    // Without captured source authority a base element is not permission to reinterpret
    // fragments or relative dependencies, even when the supplied href is absolute.
    if declared_base.is_some() && base.is_none() {
        return Err(dependency_error(
            "HTML base has no captured source authority",
        ));
    }
    let declared_base = declared_base
        .map(|destination| {
            validate_destination(&destination, base)?;
            let url = base
                .ok_or_else(|| dependency_error("HTML base has no captured source authority"))?
                .join(&destination)
                .map_err(|_| dependency_error("invalid HTML base"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(dependency_error("HTML base is not HTTP(S)"));
            }
            Ok(url)
        })
        .transpose()?;
    let effective_base = declared_base.as_ref().or(base);
    let mut changed = false;
    for node in elements {
        if let NodeData::Element { name, attrs, .. } = &node.data {
            let mut attrs = attrs.borrow_mut();
            for attr in attrs.iter_mut() {
                if is_url_attribute(
                    name.local.as_ref(),
                    attr.name.local.as_ref(),
                    name.ns.as_ref(),
                ) {
                    if name.ns.as_ref() == "http://www.w3.org/1999/xhtml"
                        && name.local.as_ref() == "base"
                    {
                        continue;
                    }
                    let resolved = validate_destination(attr.value.as_ref(), effective_base)?;
                    if resolved != attr.value.as_ref() {
                        attr.value = resolved.into();
                        changed = true;
                    }
                }
            }
        }
    }
    if !changed {
        return Ok(crate::markdown::html_to_markdown(content));
    }
    let mut serialized = Vec::new();
    html5ever::serialize(
        &mut serialized,
        &SerializableHandle::from(dom.document.clone()),
        Default::default(),
    )
    .map_err(|error| SourceError::Parse(format!("remote HTML serialization: {error}")))?;
    let serialized = String::from_utf8(serialized)
        .map_err(|error| SourceError::Parse(format!("remote HTML serialization: {error}")))?;
    Ok(crate::markdown::html_to_markdown(&serialized))
}

pub(crate) fn markdown(content: &str, base: Option<&Url>) -> Result<String, SourceError> {
    let input = lk_core::markdown::MarkdownInput::from_text(content);
    let mut changed = false;
    let mut has_raw_html = false;
    let mut events = Vec::new();
    for event in input.events(lk_core::markdown::OPTIONS) {
        let event = match event {
            Event::Start(mut tag @ (Tag::Link { .. } | Tag::Image { .. })) => {
                let (Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) = &mut tag else {
                    unreachable!()
                };
                let resolved = validate_destination(dest_url, base)?;
                if resolved != dest_url.as_ref() {
                    *dest_url = resolved.into();
                    changed = true;
                }
                Event::Start(tag)
            }
            Event::Html(_) | Event::InlineHtml(_) => {
                has_raw_html = true;
                event
            }
            _ => event,
        };
        events.push(event);
    }
    if has_raw_html {
        // The complete rendered DOM preserves real namespace context, code and comments.
        // Raw HTML remains byte-preserved; local dependencies require a declared HTML input.
        let mut rendered = String::new();
        pulldown_cmark::html::push_html(&mut rendered, events.iter().cloned());
        let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(rendered);
        for node in elements(&dom, true) {
            if let NodeData::Element { name, attrs, .. } = &node.data {
                if name.ns.as_ref() == "http://www.w3.org/1999/xhtml"
                    && name.local.as_ref() == "base"
                    && attrs
                        .borrow()
                        .iter()
                        .any(|attr| attr.name.local.as_ref() == "href")
                {
                    return Err(dependency_error(
                        "HTML base in Markdown has no captured source authority",
                    ));
                }
                for attr in attrs.borrow().iter() {
                    if is_url_attribute(
                        name.local.as_ref(),
                        attr.name.local.as_ref(),
                        name.ns.as_ref(),
                    ) {
                        validate_destination(attr.value.as_ref(), None)?;
                    }
                }
            }
        }
    }
    if !changed {
        return Ok(content.to_owned());
    }
    let markdown =
        lk_core::markdown::serialize(&events, lk_core::markdown::OPTIONS, Default::default())
            .map_err(|error| {
                SourceError::Parse(format!("remote Markdown serialization: {error}"))
            })?;
    Ok(markdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(markdown: &str) -> Vec<String> {
        let input = lk_core::markdown::MarkdownInput::from_text(markdown);
        input
            .events(lk_core::markdown::OPTIONS)
            .filter_map(|event| match event {
                Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                    Some(dest_url.to_string())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn html_links_use_captured_source_and_declared_base() {
        let base = Url::parse("https://source.example/wiki/spaces/space/page").unwrap();
        let rendered = html(
            "<p><a href='/wiki/concepts/target.md'>root</a> <a href='../sibling'>relative</a></p>",
            Some(&base),
        )
        .unwrap();
        assert_eq!(
            links(&rendered),
            [
                "https://source.example/wiki/concepts/target.md",
                "https://source.example/wiki/spaces/sibling"
            ]
        );
        let rendered = html(
            "<base href='../assets/'><p><a href='guide.md'>guide</a> <a href='#part'>part</a></p>",
            Some(&base),
        )
        .unwrap();
        assert_eq!(
            links(&rendered),
            [
                "https://source.example/wiki/spaces/assets/guide.md",
                "https://source.example/wiki/spaces/assets/#part"
            ]
        );
    }

    #[test]
    fn known_source_fragments_and_empty_destinations_do_not_point_into_the_vault() {
        let base = Url::parse("https://source.example/docs/page").unwrap();
        let projected = html("<template><base href='https://wrong.example/'></template><p><a href='#part'>part</a> <a href=''>same</a> <a href='next'>next</a></p>", Some(&base)).unwrap();
        assert_eq!(
            links(&projected),
            [
                "https://source.example/docs/page#part",
                "https://source.example/docs/page",
                "https://source.example/docs/next"
            ]
        );
        let projected = html("<base target='_blank'><base href='https://source.example/first/'><base href='file:///ignored/'><p><a href='next'>next</a></p>", Some(&base)).unwrap();
        assert_eq!(links(&projected), ["https://source.example/first/next"]);
        assert!(html("<base target='_blank'><p>body</p>", None).is_ok());
        assert_eq!(
            links(&markdown("[part](#part) [same]()", Some(&base)).unwrap()),
            [
                "https://source.example/docs/page#part",
                "https://source.example/docs/page"
            ]
        );
        assert_eq!(
            markdown("[part](#part) [same]()", None).unwrap(),
            "[part](#part) [same]()"
        );
    }

    #[test]
    fn html_url_surfaces_require_authority_even_when_converter_drops_them() {
        for source in [
            "<a href='../topic.md'>topic</a>",
            "<img src='/image.png'>",
            "<svg><use href='shape.svg#id'></use></svg>",
            "<base href='https://source.example/'><p>body</p>",
        ] {
            assert!(html(source, None).is_err(), "{source}");
        }
        let base = Url::parse("https://source.example/docs/").unwrap();
        for source in [
            "<a href='file:///tmp/secret'>local</a>",
            "<base href='file:///tmp/'><p>body</p>",
        ] {
            assert!(html(source, Some(&base)).is_err(), "{source}");
        }
        let rendered = html("<p>before</p><pre><code>&lt;a href='../literal.md'&gt;literal&lt;/a&gt;</code></pre><!-- <a href='../comment.md'> -->", None).unwrap();
        assert!(links(&rendered).is_empty(), "{rendered}");
        assert!(rendered.contains("../literal.md"), "{rendered}");
    }

    #[test]
    fn markdown_rebases_parsed_destinations_without_giving_literal_examples_link_authority() {
        let base = Url::parse("https://source.example/docs/note.md").unwrap();
        let source = "[link][ref] ![image](../a.png)\n\n[ref]: <a b.md>\n\n```text\n[literal](../../wiki/concepts/topic.md)\n```\n\n<pre>[literal](local.md)</pre>\n";
        let projected = markdown(source, Some(&base)).unwrap();
        assert_eq!(
            links(&projected),
            [
                "https://source.example/docs/a%20b.md",
                "https://source.example/a.png"
            ]
        );
        assert!(
            projected.contains("[literal](../../wiki/concepts/topic.md)"),
            "{projected}"
        );
        let already_absolute =
            "[link](https://source.example/a)\r\n\r\n```\r\n[literal](missing.md)\r\n```\r\n";
        assert_eq!(
            markdown(already_absolute, Some(&base)).unwrap(),
            already_absolute
        );
    }

    #[test]
    fn raw_html_in_markdown_keeps_namespace_and_byte_semantics() {
        let with_footnotes = "<p>raw HTML</p>\n\nA note[^1].\n\n[^1]: Footnote content.\n";
        assert_eq!(
            markdown(
                with_footnotes,
                Some(&Url::parse("https://source.example/page").unwrap())
            )
            .unwrap(),
            with_footnotes
        );

        let base = Url::parse("https://source.example/page").unwrap();
        assert_eq!(
            markdown("<a href='#part'>part</a>", Some(&base)).unwrap(),
            "<a href='#part'>part</a>"
        );
        assert_eq!(
            markdown("<a href='#part'>part</a>", None).unwrap(),
            "<a href='#part'>part</a>"
        );

        let source =
            "<svg>\n\nA paragraph exits the SVG namespace.\n\n<image href='literal.md'>\n</svg>\n";
        assert_eq!(markdown(source, None).unwrap(), source);
        let source = "[link](https://source.example/a)\n\n<!-- <a href='local.md'> -->\n\n<pre>&lt;a href='local.md'&gt;</pre>\n";
        assert_eq!(markdown(source, None).unwrap(), source);
        assert!(
            markdown(
                "<a href='local.md'>link</a>",
                Some(&Url::parse("https://source.example/").unwrap())
            )
            .is_err()
        );
        assert!(
            markdown(
                "<base href='https://source.example/'><a href='#part'>part</a>",
                None
            )
            .is_err()
        );
    }
}
