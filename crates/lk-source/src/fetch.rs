//! Fetching a web page and keeping only what a reader would have read.
//!
//! Two callers need exactly this: an RSS feed that ships an excerpt, and a person handing the
//! vault a link. One implementation, so an article absorbed by hand arrives in the same shape
//! as one the feed brought.

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

/// The response body, refused rather than truncated past [`MAX_BODY_BYTES`].
///
/// Truncated HTML is worse than none: readability would extract whatever the prefix happened
/// to contain and the page would assert an article nobody published. So the cap is an error,
/// and it is checked while streaming — a declared `Content-Length` is the sender's claim and
/// a chunked response makes none.
async fn read_capped(mut resp: reqwest::Response, url: &str) -> Result<String, SourceError> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(SourceError::Parse(format!(
                "{url} is larger than {MAX_BODY_BYTES} bytes — not an article this can read"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}
