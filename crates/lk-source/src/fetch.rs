//! Fetching a web page and keeping only what a reader would have read.
//!
//! Two callers need exactly this: an RSS feed that ships an excerpt, and a person handing the
//! vault a link. One implementation, so an article absorbed by hand arrives in the same shape
//! as one the feed brought.
//!
//! Redirects are followed wherever they lead, including to a private address. A rule refusing
//! those would break the deployments this tool supports — an on-prem Atlassian instance and an
//! intranet-hosted feed both live at private addresses — to close a shape with no channel back:
//! what a redirected fetch returns is written into the reader's OWN vault, on their own
//! machine, and reaches nobody who could have arranged it. The size cap below is the bound
//! that does earn its place, because an enormous body costs an unattended run its ingest
//! whoever served it.

use crate::SourceError;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Some sites reject requests with an empty User-Agent (HTTP 403); identify ourselves.
pub const USER_AGENT: &str = concat!("lorekeeper/", env!("CARGO_PKG_VERSION"));

/// How much of a page is read before the fetch is abandoned.
///
/// A timeout bounds a stalled response and not a fast enormous one, and the URLs this follows
/// are third-party: a feed entry links wherever its publisher points it. Sixteen mebibytes is
/// far past any article — the largest in the reference vault is under one — and an unattended
/// ingest that met a video behind an `<a>` would otherwise buffer it whole.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The encoding a response declares, falling back to UTF-8 where it declares none — the same
/// default `Response::text` applies, and the one the web has settled on.
///
/// Read the HTTP declaration once; unsupported labels are errors. A document's
/// `<meta charset>` never reinterprets already decoded text.
fn declared_charset(
    headers: &reqwest::header::HeaderMap,
) -> Result<&'static encoding_rs::Encoding, SourceError> {
    let media = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok());
    let Some(charset) = media
        .as_ref()
        .and_then(|mime| mime.get_param(mime::CHARSET))
    else {
        return Ok(encoding_rs::UTF_8);
    };
    encoding_rs::Encoding::for_label(charset.as_str().as_bytes())
        .ok_or_else(|| SourceError::Parse("unsupported response charset".into()))
}

/// A page's readable core.
pub struct ReadableArticle {
    /// The title readability recovered, where the page stated one.
    pub title: Option<String>,
    pub markdown: String,
}

enum DocumentFormat {
    Html,
    Markdown,
    Text,
}

impl DocumentFormat {
    fn from_response(
        headers: &reqwest::header::HeaderMap,
        url: &url::Url,
    ) -> Result<Self, SourceError> {
        let media = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<mime::Mime>().ok())
            .ok_or_else(|| SourceError::Parse("response needs a valid Content-Type".into()))?;
        match media.essence_str() {
            "text/html" | "application/xhtml+xml" => Ok(Self::Html),
            "text/markdown" | "text/x-markdown" => Ok(Self::Markdown),
            "text/plain" => {
                let extension = std::path::Path::new(url.path())
                    .extension()
                    .and_then(|value| value.to_str());
                if extension.is_some_and(|value| {
                    value.eq_ignore_ascii_case("md") || value.eq_ignore_ascii_case("markdown")
                }) {
                    Ok(Self::Markdown)
                } else {
                    Ok(Self::Text)
                }
            }
            other => Err(SourceError::Parse(format!(
                "unsupported document Content-Type: {other}"
            ))),
        }
    }
}

fn remote_markdown(text: &str, base: &url::Url) -> Result<ReadableArticle, SourceError> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES;
    let mut changed = false;
    let mut link_error = None;
    let mut title = None;
    let mut heading = None;
    let events = Parser::new_ext(text, options).map(|event| {
        match &event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H1,
                ..
            }) if title.is_none() => heading = Some(String::new()),
            Event::End(TagEnd::Heading(HeadingLevel::H1)) => {
                if let Some(value) = heading.take().filter(|value| !value.trim().is_empty()) {
                    title = Some(value.trim().to_owned());
                }
            }
            Event::Text(value) | Event::Code(value) => {
                if let Some(heading) = &mut heading {
                    heading.push_str(value);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(heading) = &mut heading {
                    heading.push(' ');
                }
            }
            _ => {}
        }
        if let Event::Start(mut tag @ (Tag::Link { .. } | Tag::Image { .. })) = event {
            let destination = match &mut tag {
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => dest_url,
                _ => unreachable!(),
            };
            match base.join(destination) {
                Ok(absolute) if absolute.as_str() != destination.as_ref() => {
                    *destination = absolute.to_string().into();
                    changed = true;
                }
                Err(error) => link_error = Some(error),
                _ => {}
            }
            Event::Start(tag)
        } else {
            event
        }
    });
    let mut markdown = String::new();
    pulldown_cmark_to_cmark::cmark(events, &mut markdown, options)
        .map_err(|error| SourceError::Parse(format!("remote Markdown: {error}")))?;
    if let Some(error) = link_error {
        return Err(SourceError::Parse(format!("remote Markdown link: {error}")));
    }
    Ok(ReadableArticle {
        title,
        markdown: if changed { markdown } else { text.into() },
    })
}

fn literal_markdown(text: &str) -> Result<String, SourceError> {
    let normalized = text.replace("\r\n", "\n");
    let events = normalized.split("\n\n").flat_map(|paragraph| {
        let lines = paragraph.lines().enumerate().flat_map(|(index, line)| {
            (index != 0)
                .then_some(Event::HardBreak)
                .into_iter()
                .chain(std::iter::once(Event::Text(line.into())))
        });
        std::iter::once(Event::Start(Tag::Paragraph))
            .chain(lines)
            .chain(std::iter::once(Event::End(TagEnd::Paragraph)))
    });
    let mut markdown = String::new();
    pulldown_cmark_to_cmark::cmark(events, &mut markdown, Options::empty())
        .map_err(|error| SourceError::Parse(format!("plain text: {error}")))?;
    Ok(markdown)
}

/// Fetch a declared HTML, Markdown or plain-text document as Markdown.
///
/// Empty text or an HTML page without a readable article returns `None`.
/// Unsupported media is an error; RSS callers retain their feed summary.
pub async fn readable(
    http: &reqwest::Client,
    url: &str,
) -> Result<Option<ReadableArticle>, SourceError> {
    let response = http
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(SourceError::Api {
            status: response.status().as_u16(),
            message: format!("fetching {url}"),
        });
    }
    let base = response.url().clone();
    let format = DocumentFormat::from_response(response.headers(), &base)?;
    let text = read_capped(response, url).await?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    match format {
        DocumentFormat::Html => Ok(crate::markdown::readable_article(&text, &base)),
        DocumentFormat::Markdown => remote_markdown(&text, &base).map(Some),
        DocumentFormat::Text => Ok(Some(ReadableArticle {
            title: None,
            markdown: literal_markdown(&text)?,
        })),
    }
}

/// The response body as text, refused rather than truncated past [`MAX_BODY_BYTES`].
///
/// Truncated HTML is worse than none: readability would extract whatever the prefix happened
/// to contain and the page would assert an article nobody published. So the cap is an error,
/// and it is checked while streaming — a declared `Content-Length` is the sender's claim and
/// a chunked response makes none.
///
/// Decoded through the charset the response DECLARES, which is what `Response::text` does and
/// what streaming to cap the bytes would otherwise cost. Reading a EUC-KR page as UTF-8 does
/// not degrade it — `한글 기사 제목입니다` becomes `�ѱ� ��� �����Դϴ�`, every character
/// replaced — and the page would enter the vault as garbage no error announced. Korean and
/// Japanese sites still serve those encodings.
async fn read_capped(mut resp: reqwest::Response, url: &str) -> Result<String, SourceError> {
    let encoding = declared_charset(resp.headers())?;
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(SourceError::Parse(format!(
                "{url} is larger than {MAX_BODY_BYTES} bytes — not an article this can read"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    let (decoded, _, malformed) = encoding.decode(&body);
    if malformed {
        return Err(SourceError::Parse(format!(
            "response is not valid {} text",
            encoding.name()
        )));
    }
    Ok(decoded.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serve(content_type: Option<&str>, body: impl AsRef<[u8]>, redirect: bool) -> String {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let content_type = content_type.map(str::to_owned);
        let body = body.as_ref().to_owned();
        std::thread::spawn(move || {
            for index in 0..=usize::from(redirect) {
                let (mut stream, _) = listener.accept().unwrap();
                {
                    let mut reader = std::io::BufReader::new(&mut stream);
                    let mut request = String::new();
                    while !request.ends_with("\r\n\r\n") {
                        assert!(reader.read_line(&mut request).unwrap() > 0);
                        assert!(request.len() <= 8192);
                    }
                }
                if redirect && index == 0 {
                    write!(stream, "HTTP/1.1 302 Found\r\nLocation: /docs/guide.MD\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    let header = content_type
                        .as_deref()
                        .map_or(String::new(), |value| format!("Content-Type: {value}\r\n"));
                    write!(stream, "HTTP/1.1 200 OK\r\n{header}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                    let _ = stream.write_all(&body);
                }
            }
        });
        format!("http://{address}/input")
    }

    #[tokio::test]
    async fn markdown_preserves_structure_and_rebases_links_after_redirects() {
        let body = "---\norigin: provider\n---\n\n# **Guide**\n\n```rust\nlet x = 1 < 2;\n```\n\n[topic][ref]\n\n[ref]: ../topic.md?q=1#part\n\n![chart](images/a.png)\n\n| A | B |\n|---|---|\n| one | two |\n\n- [x] done\n\nNote[^a]\n\n[^a]: retained footnote\n\n<div>retained raw HTML</div>\n";
        let url = serve(Some("text/plain; charset=utf-8"), body, true);
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let article = readable(&http, &url).await.unwrap().unwrap();
        assert_eq!(article.title.as_deref(), Some("Guide"));
        assert!(article.markdown.contains("let x = 1 < 2;"));
        let events: Vec<_> = Parser::new_ext(&article.markdown, Options::all()).collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Start(Tag::CodeBlock(_))))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Start(Tag::Table(_))))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::TaskListMarker(true)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::FootnoteReference(_)))
        );
        for content in [
            "origin: provider",
            "retained footnote",
            "<div>retained raw HTML</div>",
        ] {
            assert!(article.markdown.contains(content));
        }
        let base = url::Url::parse(&url).unwrap();
        for expected in ["/topic.md?q=1#part", "/docs/images/a.png"] {
            let expected = base.join(expected).unwrap();
            assert!(events.iter().any(|event| matches!(event,
                Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. })
                    if dest_url.as_ref() == expected.as_str()
            )));
        }
    }

    #[tokio::test]
    async fn declared_markdown_keeps_unmodified_source_bytes() {
        let body = "# Title\r\n\r\n- [x] task\r\n\r\n```text\r\n<x> & value\r\n```\r\n";
        for media in ["text/markdown; charset=utf-8", "text/x-markdown"] {
            let url = serve(Some(media), body, false);
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let article = readable(&http, &url).await.unwrap().unwrap();
            assert_eq!(article.title.as_deref(), Some("Title"));
            assert_eq!(article.markdown, body);
        }
    }

    #[tokio::test]
    async fn plain_text_markup_is_literal_visible_text() {
        let body = "# literal\n\n<script>keep</script>\n*not emphasis*\n[not a link](target)\n";
        let url = serve(Some("text/plain"), body, false);
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let article = readable(&http, &url).await.unwrap().unwrap();
        assert!(article.title.is_none());
        let mut visible = String::new();
        for event in Parser::new(&article.markdown) {
            match event {
                Event::Text(text) => visible.push_str(&text),
                Event::SoftBreak | Event::HardBreak => visible.push('\n'),
                Event::Start(Tag::Paragraph) => {
                    if !visible.is_empty() {
                        visible.push_str("\n\n");
                    }
                }
                Event::End(TagEnd::Paragraph) => {}
                other => panic!("plain text became markup: {other:?}"),
            }
        }
        assert_eq!(visible, body.trim_end_matches('\n'));
    }

    #[tokio::test]
    async fn unsupported_or_missing_media_is_refused_before_extraction() {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        for media in [
            None,
            Some("application/pdf"),
            Some("application/json"),
            Some("invalid"),
        ] {
            let url = serve(media, "not an HTML article", false);
            assert!(matches!(
                readable(&http, &url).await,
                Err(SourceError::Parse(_))
            ));
        }
    }

    #[tokio::test]
    async fn html_media_wins_over_a_markdown_url_and_empty_text_is_absent() {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let html = format!(
            "<html><head><title>Article</title></head><body><article><h1>Article</h1><p>{}</p><a href='more'>Read more</a></article></body></html>",
            "A substantive article about a real subject. ".repeat(40)
        );
        let url = serve(Some("text/html"), &html, true);
        let article = readable(&http, &url).await.unwrap().unwrap();
        assert_eq!(article.title.as_deref(), Some("Article"));
        assert!(article.markdown.contains("/docs/more"));
        let url = serve(Some("text/markdown"), " \n\t", false);
        assert!(readable(&http, &url).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn declared_text_encoding_is_strict_and_body_limits_still_apply() {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let (korean, _, _) = encoding_rs::EUC_KR.encode("# 한글 제목\n\n한글 본문");
        let url = serve(Some("text/markdown; charset=EUC-KR"), korean, false);
        let article = readable(&http, &url).await.unwrap().unwrap();
        assert_eq!(article.title.as_deref(), Some("한글 제목"));
        assert!(article.markdown.contains("한글 본문"));
        for (media, body) in [
            ("text/plain; charset=not-a-charset", vec![b'x']),
            ("text/markdown; charset=utf-8", vec![0xff]),
            ("text/markdown", vec![b'x'; MAX_BODY_BYTES + 1]),
        ] {
            let url = serve(Some(media), body, false);
            assert!(matches!(
                readable(&http, &url).await,
                Err(SourceError::Parse(_))
            ));
        }
    }

    /// A page that declares its encoding is decoded in it. Reading EUC-KR as UTF-8 replaces
    /// every Korean character rather than degrading gracefully, so the article would enter the
    /// vault as garbage with no error announced.
    #[test]
    fn a_declared_charset_is_the_one_the_body_is_read_in() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "text/html; charset=EUC-KR".parse().unwrap(),
        );
        let encoding = declared_charset(&headers).unwrap();
        let (bytes, _, _) = encoding_rs::EUC_KR.encode("한글 기사 제목입니다");
        assert_eq!(encoding.decode(&bytes).0, "한글 기사 제목입니다");
        assert_ne!(String::from_utf8_lossy(&bytes), "한글 기사 제목입니다");
    }

    /// The web's default, and `Response::text`'s — a page saying nothing is UTF-8.
    #[test]
    fn a_response_declaring_no_charset_is_read_as_utf8() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::CONTENT_TYPE, "text/html".parse().unwrap());
        assert_eq!(declared_charset(&headers).unwrap(), encoding_rs::UTF_8);
        assert_eq!(
            declared_charset(&reqwest::header::HeaderMap::new()).unwrap(),
            encoding_rs::UTF_8
        );
    }
}
