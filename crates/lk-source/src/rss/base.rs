//! Feed parsers retain rich body text but not its inherited XML base.
use std::sync::Arc;

use quick_xml::encoding::DecodingReader;
use quick_xml::events::{BytesDecl, BytesStart, Event};
use quick_xml::name::ResolveResult;
use url::Url;

use crate::SourceError;

#[derive(Clone, Default)]
pub(super) struct EntryBases {
    pub content: Option<Arc<Url>>,
    pub summary: Option<Arc<Url>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Namespace {
    Unbound,
    Atom,
    Rss,
    Rdf,
    Content,
    Html,
    Svg,
    Other,
}

impl Namespace {
    fn from_uri(uri: &str) -> Self {
        match uri {
            "http://www.w3.org/2005/Atom" => Self::Atom,
            "http://purl.org/rss/1.0/" => Self::Rss,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#" => Self::Rdf,
            "http://purl.org/rss/1.0/modules/content/" => Self::Content,
            "http://www.w3.org/1999/xhtml" => Self::Html,
            "http://www.w3.org/2000/svg" => Self::Svg,
            _ => Self::Other,
        }
    }

    fn url_namespace(self) -> &'static str {
        match self {
            Self::Html => "http://www.w3.org/1999/xhtml",
            Self::Svg => "http://www.w3.org/2000/svg",
            _ => "",
        }
    }
}

struct Frame {
    local: String,
    namespace: Namespace,
    base: Arc<Url>,
    entry: Option<usize>,
    is_entry: bool,
    xhtml: bool,
}

fn error(detail: impl std::fmt::Display) -> SourceError {
    SourceError::Parse(format!("feed content base: {detail}"))
}

pub(super) fn capture(
    bytes: &[u8],
    final_url: &Url,
) -> Result<(Vec<EntryBases>, Option<Vec<u8>>), SourceError> {
    if bytes
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'{')
    {
        return Ok((Vec::new(), None));
    }
    let final_url = Arc::new(final_url.clone());
    let mut reader = quick_xml::NsReader::from_reader(DecodingReader::new(bytes));
    reader.config_mut().trim_markup_names_in_closing_tags = true;
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut entries = Vec::new();
    let mut writer = quick_xml::Writer::new(Vec::new());
    let mut changed = false;
    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer).map_err(error)? {
            Event::Start(element) => {
                let frame = open(&reader, &element, &stack, &mut entries, &final_url)?;
                let (element, rewritten) = resolve_xhtml(&reader, element, &frame)?;
                changed |= rewritten;
                writer.write_event(Event::Start(element)).map_err(error)?;
                stack.push(frame);
            }
            Event::Empty(element) => {
                let frame = open(&reader, &element, &stack, &mut entries, &final_url)?;
                let (element, rewritten) = resolve_xhtml(&reader, element, &frame)?;
                changed |= rewritten;
                writer.write_event(Event::Empty(element)).map_err(error)?;
            }
            Event::End(element) => {
                stack.pop().ok_or_else(|| error("unbalanced XML scope"))?;
                writer.write_event(Event::End(element)).map_err(error)?;
            }
            Event::Decl(declaration) => {
                if let Some(encoding) = declaration.encoder() {
                    reader.get_mut().set_encoding(encoding);
                } else if declaration.encoding().is_some() {
                    return Err(error("unsupported XML declaration encoding"));
                }
                let version = declaration.version().map_err(error)?;
                let standalone = declaration.standalone().transpose().map_err(error)?;
                writer
                    .write_event(Event::Decl(BytesDecl::new(
                        &version,
                        Some("UTF-8"),
                        standalone.as_deref(),
                    )))
                    .map_err(error)?;
            }
            Event::Eof => break,
            event => writer.write_event(event).map_err(error)?,
        }
        if writer.get_ref().len() > super::MAX_FEED_BYTES {
            return Err(error(
                "decoded or resolved feed exceeds the bounded feed representation",
            ));
        }
    }
    Ok((entries, changed.then(|| writer.into_inner())))
}

fn open(
    reader: &quick_xml::NsReader<DecodingReader<&[u8]>>,
    element: &BytesStart<'_>,
    stack: &[Frame],
    entries: &mut Vec<EntryBases>,
    final_url: &Arc<Url>,
) -> Result<Frame, SourceError> {
    let (namespace, local) = reader.resolver().resolve_element(element.name());
    let namespace = match namespace {
        ResolveResult::Bound(namespace) => Namespace::from_uri(namespace.as_ref()),
        ResolveResult::Unbound => Namespace::Unbound,
        ResolveResult::Unknown(_) => return Err(error("unresolved element namespace")),
    };
    let mut base = stack
        .last()
        .map_or(final_url, |parent| &parent.base)
        .clone();
    let mut xhtml_format = false;
    for attr in element.attributes() {
        let attr = attr.map_err(error)?;
        if attr.key.as_ref() == "type" {
            xhtml_format = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(error)?
                == "xhtml";
        }
        if attr.key.as_ref() == "xml:base" {
            let value = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(error)?;
            crate::remote_text::validate_destination(&value, Some(&base))?;
            base = Arc::new(base.join(&value).map_err(error)?);
            if !matches!(base.scheme(), "http" | "https") {
                return Err(error("XML base is not HTTP(S)"));
            }
        }
    }
    let atom = namespace == Namespace::Atom;
    let rss = matches!(namespace, Namespace::Unbound | Namespace::Rss);
    let parent = stack.last();
    let is_entry = (atom
        && local.as_ref() == "entry"
        && (stack.is_empty()
            || (stack.len() == 1
                && parent.is_some_and(|parent| {
                    parent.local == "feed" && parent.namespace == namespace
                }))))
        || (rss
            && local.as_ref() == "item"
            && ((stack.len() == 2
                && parent.is_some_and(|parent| parent.local == "channel")
                && stack[0].local == "rss")
                || (stack.len() == 1
                    && parent.is_some_and(|parent| {
                        parent.local == "RDF" && parent.namespace == Namespace::Rdf
                    }))));
    let entry = if is_entry {
        entries.push(EntryBases {
            content: Some(base.clone()),
            summary: Some(base.clone()),
        });
        Some(entries.len() - 1)
    } else {
        parent.and_then(|parent| parent.entry)
    };
    if let (Some(index), Some(parent)) = (entry, parent)
        && parent.is_entry
    {
        if (atom && local.as_ref() == "content")
            || (namespace == Namespace::Content && local.as_ref() == "encoded")
        {
            entries[index].content = Some(base.clone());
        } else if (atom && local.as_ref() == "summary") || (rss && local.as_ref() == "description")
        {
            entries[index].summary = Some(base.clone());
        }
    }
    let xhtml = parent.is_some_and(|parent| parent.xhtml)
        || (atom
            && parent.is_some_and(|parent| parent.is_entry)
            && matches!(local.as_ref(), "content" | "summary")
            && xhtml_format);
    Ok(Frame {
        local: local.as_ref().to_owned(),
        namespace,
        base,
        entry,
        is_entry,
        xhtml,
    })
}

fn resolve_xhtml<'a>(
    reader: &quick_xml::NsReader<DecodingReader<&[u8]>>,
    element: BytesStart<'a>,
    frame: &Frame,
) -> Result<(BytesStart<'a>, bool), SourceError> {
    if !frame.xhtml {
        return Ok((element, false));
    }
    let mut attributes = Vec::new();
    let mut changed = false;
    for attr in element.attributes() {
        let attr = attr.map_err(error)?;
        let (attribute_namespace, local) = reader.resolver().resolve_attribute(attr.key);
        let url_attribute = matches!(attribute_namespace, ResolveResult::Unbound)
            || matches!(attribute_namespace, ResolveResult::Bound(namespace) if namespace.as_ref() == "http://www.w3.org/1999/xlink" && frame.namespace == Namespace::Svg);
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(error)?;
        let resolved = if url_attribute
            && crate::remote_text::is_url_attribute(
                &frame.local,
                local.as_ref(),
                frame.namespace.url_namespace(),
            ) {
            crate::remote_text::validate_destination(&value, Some(&frame.base))?
        } else {
            value.to_string()
        };
        changed |= resolved != value;
        attributes.push((attr.key.as_ref().to_owned(), resolved));
    }
    if !changed {
        return Ok((element, false));
    }
    let mut rewritten = BytesStart::new(element.name().as_ref().to_owned());
    for (name, value) in &attributes {
        rewritten.push_attribute((name.as_str(), value.as_str()));
    }
    Ok((rewritten, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_declared_encodings_remain_correct_before_and_after_xhtml_rewriting() {
        for (encoding, label, text) in [
            (encoding_rs::WINDOWS_1252, "ISO-8859-1", "café"),
            (encoding_rs::EUC_KR, "EUC-KR", "한글"),
            (encoding_rs::UTF_16LE, "UTF-16LE", "한글"),
        ] {
            for rewrite in [false, true] {
                let body = if rewrite {
                    format!(
                        r#"<summary type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml" xml:base="nested/"><p><a href="child.md">{text}</a></p></div></summary>"#
                    )
                } else {
                    format!("<summary>{text}</summary>")
                };
                let xml = format!(
                    r#"<?xml version="1.0" encoding="{label}"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><id>encoded</id><title>{text}</title><updated>2026-05-23T00:00:00Z</updated>{body}</entry></feed>"#
                );
                let bytes = if encoding == encoding_rs::UTF_16LE {
                    [
                        vec![0xff, 0xfe],
                        xml.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                    ]
                    .concat()
                } else {
                    let (bytes, _, malformed) = encoding.encode(&xml);
                    assert!(!malformed);
                    bytes.into_owned()
                };
                let base = Url::parse("https://source.example/feed.xml").unwrap();
                let (bases, rewritten) = capture(&bytes, &base).unwrap();
                assert_eq!(rewritten.is_some(), rewrite, "{label}");
                let feed = feed_rs::parser::parse(rewritten.as_deref().unwrap_or(&bytes)).unwrap();
                assert_eq!(
                    feed.entries[0].title.as_ref().unwrap().content,
                    text,
                    "{label}"
                );
                let item = super::super::map_entry(
                    "fixture",
                    &None,
                    feed.entries.into_iter().next().unwrap(),
                    &bases[0],
                )
                .unwrap()
                .item;
                assert!(item.body.contains(text), "{label}: {}", item.body);
                if rewrite {
                    assert!(
                        item.body.contains("https://source.example/nested/child.md"),
                        "{label}: {}",
                        item.body
                    );
                }
            }
        }
        let malformed = b"<rss><channel><item><title>invalid \xff</title></item></channel></rss>";
        assert!(capture(malformed, &Url::parse("https://source.example/").unwrap()).is_err());
    }

    #[test]
    fn atom_namespace_and_all_ancestor_scopes_are_resolved() {
        for prefix in ["", "a:"] {
            let declaration = if prefix.is_empty() {
                "xmlns"
            } else {
                "xmlns:a"
            };
            let xml = format!(
                r#"<{prefix}feed {declaration}="http://www.w3.org/2005/Atom" xml:base="../root/"><{prefix}entry xml:base="entry/"><{prefix}content xml:base="../../content/" type="html">&lt;a href='child.md'&gt;x&lt;/a&gt;</{prefix}content><{prefix}summary xml:base="summary/" type="html">x</{prefix}summary></{prefix}entry></{prefix}feed>"#
            );
            let bases = capture(
                xml.as_bytes(),
                &Url::parse("https://example.com/redirected/feed.xml").unwrap(),
            )
            .unwrap()
            .0;
            assert_eq!(
                bases[0].content.as_ref().unwrap().as_str(),
                "https://example.com/content/"
            );
            assert_eq!(
                bases[0].summary.as_ref().unwrap().as_str(),
                "https://example.com/root/entry/summary/"
            );
        }
    }

    #[test]
    fn xhtml_attributes_are_resolved_before_feed_parser_removes_base_bearing_div() {
        let xml = br##"<feed xmlns="http://www.w3.org/2005/Atom" xml:base="https://remote.example/root/"><entry><id>x</id><title>Scoped</title><updated>2026-05-23T00:00:00Z</updated><summary type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml" xml:base="wrapper/"><p xml:base="nested/"><a href="child.md">child</a><a href="#part">part</a></p><pre><code>&lt;a href='literal.md'&gt;</code></pre><!-- <a href='comment.md'> --></div></summary></entry></feed>"##;
        let (bases, rewritten) =
            capture(xml, &Url::parse("https://elsewhere.example/feed").unwrap()).unwrap();
        let rewritten = rewritten.unwrap();
        let feed = feed_rs::parser::parse(&rewritten[..]).unwrap();
        let body = &feed.entries[0].summary.as_ref().unwrap().content;
        assert!(
            body.contains("https://remote.example/root/wrapper/nested/child.md"),
            "{body}"
        );
        assert!(
            body.contains("https://remote.example/root/wrapper/nested/#part"),
            "{body}"
        );
        let item = super::super::map_entry(
            "fixture",
            &None,
            feed.entries.into_iter().next().unwrap(),
            &bases[0],
        )
        .unwrap()
        .item;
        assert!(
            lk_core::link::extract_page_links(&item.body).is_empty(),
            "{}",
            item.body
        );
        assert!(item.body.contains("literal.md"), "{}", item.body);
        assert!(
            !item
                .body
                .contains("https://remote.example/root/wrapper/literal.md"),
            "{}",
            item.body
        );
    }

    #[test]
    fn rss_body_scopes_ignore_comments_and_cdata_markup() {
        let xml = br#"<rss xmlns:c="http://purl.org/rss/1.0/modules/content/" xml:base="https://example.com/root/"><channel><item xml:base="item/"><description xml:base="summary/"><![CDATA[<entry xml:base='fake/'/>]]></description><!-- <item/> --><c:encoded xml:base="../content/"><![CDATA[<p>body</p>]]></c:encoded></item></channel></rss>"#;
        let bases = capture(xml, &Url::parse("https://elsewhere.example/feed").unwrap())
            .unwrap()
            .0;
        assert_eq!(
            bases[0].content.as_ref().unwrap().as_str(),
            "https://example.com/root/content/"
        );
        assert_eq!(
            bases[0].summary.as_ref().unwrap().as_str(),
            "https://example.com/root/item/summary/"
        );
    }
}
