//! Fetching a web page and keeping only what a reader would have read.
//!
//! Two callers need exactly this: an RSS feed that ships an excerpt, and a person handing the
//! vault a link. One implementation, so an article absorbed by hand arrives in the same shape
//! as one the feed brought.

use crate::SourceError;

/// Some sites reject requests with an empty User-Agent (HTTP 403); identify ourselves.
pub const USER_AGENT: &str = concat!("lorekeeper/", env!("CARGO_PKG_VERSION"));

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
    let html = resp.text().await?;
    Ok(crate::markdown::readable_article(&html, &parsed))
}
