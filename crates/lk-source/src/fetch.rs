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
/// The HEADER only. A `<meta charset>` inside the document is the other place a page can say
/// this, and honouring it means decoding twice — a guess, then the real one — which is the
/// browser's job rather than this one's. A page whose header and body disagree is rare and
/// says so loudly in the extracted text.
fn declared_charset(headers: &reqwest::header::HeaderMap) -> &'static encoding_rs::Encoding {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .and_then(|mime| {
            mime.get_param(mime::CHARSET)
                .and_then(|charset| encoding_rs::Encoding::for_label(charset.as_str().as_bytes()))
        })
        .unwrap_or(encoding_rs::UTF_8)
}

/// A page's readable core.
pub struct ReadableArticle {
    /// The title readability recovered, where the page stated one.
    pub title: Option<String>,
    pub markdown: String,
}

/// Fetch `url` and extract its readable core as Markdown.
///
/// `None` when readability cannot isolate an article — the page is a listing, a shell, or
/// something else with no body to keep. Each caller decides what that means: RSS keeps the
/// feed's own summary, and a person's link is refused rather than absorbed as boilerplate.
pub async fn readable(
    http: &reqwest::Client,
    url: &str,
) -> Result<Option<ReadableArticle>, SourceError> {
    let parsed = url::Url::parse(url)
        .map_err(|e| SourceError::Parse(format!("invalid URL '{url}': {e}")))?;
    let resp = http
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(SourceError::Api {
            status: resp.status().as_u16(),
            message: format!("fetching {url}"),
        });
    }
    let html = read_capped(resp, url).await?;
    Ok(crate::markdown::readable_article(&html, &parsed))
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
    let encoding = declared_charset(resp.headers());
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(SourceError::Parse(format!(
                "{url} is larger than {MAX_BODY_BYTES} bytes — not an article this can read"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(encoding.decode(&body).0.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let encoding = declared_charset(&headers);
        let (bytes, _, _) = encoding_rs::EUC_KR.encode("한글 기사 제목입니다");
        assert_eq!(encoding.decode(&bytes).0, "한글 기사 제목입니다");
        assert_ne!(String::from_utf8_lossy(&bytes), "한글 기사 제목입니다");
    }

    /// The web's default, and `Response::text`'s — a page saying nothing is UTF-8.
    #[test]
    fn a_response_declaring_no_charset_is_read_as_utf8() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::CONTENT_TYPE, "text/html".parse().unwrap());
        assert_eq!(declared_charset(&headers), encoding_rs::UTF_8);
        assert_eq!(
            declared_charset(&reqwest::header::HeaderMap::new()),
            encoding_rs::UTF_8
        );
    }
}
