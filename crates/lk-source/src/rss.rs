//! RSS/Atom feed source. Polls one or more public feeds (vendor blogs, news
//! aggregators) and maps each entry to a [`RawItem`]. No credentials — feeds are
//! public HTTP. Each entry's `external_id` is namespaced by feed (`{feed_id}:{guid}`),
//! so the same article carried by two of this source's feeds stays two distinct
//! observations (each feed is its own provenance) rather than being merged.

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use crate::{ExtractContext, Source, SourceError};

/// Some feeds reject requests with an empty User-Agent (HTTP 403); identify ourselves.
const USER_AGENT: &str = concat!("lorekeeper/", env!("CARGO_PKG_VERSION"));

pub struct RssSource {
    http: reqwest::Client,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RssParams {
    feeds: Vec<FeedParams>,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    /// Cap on items kept *per feed* (after the day-window filter), not a global
    /// total — a busy feed can't crowd out quieter ones.
    #[serde(default = "default_max_items")]
    max_items_per_feed: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedParams {
    /// Stable provenance id surfaced in item metadata (e.g. `openai`, `hf-blog`).
    id: String,
    url: String,
    /// When true, fetch the full article HTML from each entry's link URL and
    /// extract readable content via `dom_smoothie`, replacing the feed summary.
    #[serde(default)]
    fetch_full_text: bool,
}

fn default_lookback() -> u32 {
    24
}

fn default_max_items() -> usize {
    50
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    crate::parse_validated::<RssParams>(params).map(|_| ())
}

impl crate::ValidatedParams for RssParams {
    fn validate(&self) -> Result<(), SourceError> {
        if self.feeds.is_empty() {
            return Err(SourceError::InvalidParams(
                "rss `feeds` must list at least one feed".into(),
            ));
        }
        let mut seen_ids = std::collections::HashSet::new();
        for f in &self.feeds {
            if f.id.trim().is_empty() {
                return Err(SourceError::InvalidParams(
                    "rss feed `id` must not be empty".into(),
                ));
            }
            if !(f.url.starts_with("http://") || f.url.starts_with("https://")) {
                return Err(SourceError::InvalidParams(format!(
                    "rss feed `url` must be an http(s) URL, got '{}'",
                    f.url
                )));
            }
            // Feed ids namespace every item's external id (see `map_entry`); a
            // duplicate would let two feeds collide into one EventId.
            if !seen_ids.insert(f.id.as_str()) {
                return Err(SourceError::InvalidParams(format!(
                    "rss feed `id` '{}' is duplicated; ids must be unique within a source",
                    f.id
                )));
            }
        }
        Ok(())
    }
}

impl RssSource {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    /// Fetch the full article from `url` and extract its readable core as Markdown.
    /// Returns `None` when readability can't isolate an article (the caller then keeps
    /// the known-clean feed summary rather than adopting boilerplate).
    async fn fetch_article(&self, article_url: &str) -> Result<Option<String>, SourceError> {
        let resp = self
            .http
            .get(article_url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(SourceError::Api {
                status: resp.status().as_u16(),
                message: format!("fetching article: {article_url}"),
            });
        }
        let html = resp.text().await?;
        let parsed_url = url::Url::parse(article_url)
            .map_err(|e| SourceError::Parse(format!("invalid article URL: {e}")))?;
        Ok(crate::markdown::readable_html_to_markdown(
            &html,
            &parsed_url,
        ))
    }

    async fn fetch_feed(&self, url: &str) -> Result<feed_rs::model::Feed, SourceError> {
        let resp = self
            .http
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(SourceError::Api {
                status: resp.status().as_u16(),
                message: format!("fetching feed {url}"),
            });
        }
        let bytes = resp.bytes().await?;
        feed_rs::parser::parse(&bytes[..]).map_err(|e| SourceError::Parse(e.to_string()))
    }
}

#[async_trait]
impl Source for RssSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: RssParams = crate::parse_validated(params)?;

        // News is past-dated, so pad only the lower bound. The pipeline still
        // splits items onto their own publication date (multi-date batch).
        let (min, max) = ctx.day_window(p.lookback_hours, 0)?;

        let mut items = Vec::new();
        for feed_cfg in &p.feeds {
            let feed = match self.fetch_feed(&feed_cfg.url).await {
                Ok(f) => f,
                Err(e) => {
                    // One unreachable/malformed feed must not abort the source.
                    tracing::warn!(
                        feed = %feed_cfg.id,
                        url = %feed_cfg.url,
                        error = %e,
                        "rss: skipping feed (fetch/parse failed)"
                    );
                    continue;
                }
            };
            let feed_title = feed.title.map(|t| t.content);

            let mut kept = 0usize;
            for entry in feed.entries {
                if kept >= p.max_items_per_feed {
                    break;
                }
                let mut item = match map_entry(&feed_cfg.id, &feed_title, entry, min, max) {
                    Some(item) => item,
                    None => continue,
                };
                // Optionally replace the feed summary with the full article.
                if feed_cfg.fetch_full_text
                    && let Some(ref article_url) = item.url
                {
                    match self.fetch_article(article_url).await {
                        // Replace the feed summary only when the full-text extraction is
                        // at least as substantial. Readability can mis-extract a short
                        // wrong node from the article page, and the feed summary is
                        // known-clean content we'd otherwise lose.
                        Ok(Some(full)) if full.trim().len() >= item.body.trim().len() => {
                            item.body = full;
                        }
                        Ok(Some(_)) => {
                            tracing::warn!(
                                feed = %feed_cfg.id,
                                url = %article_url,
                                "rss: full-text extraction shorter than feed summary; keeping summary"
                            );
                        }
                        Ok(None) => {
                            tracing::warn!(
                                feed = %feed_cfg.id,
                                url = %article_url,
                                "rss: readability found no article core; keeping feed summary"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                feed = %feed_cfg.id,
                                url = %article_url,
                                error = %e,
                                "rss: full-text fetch failed, keeping feed content"
                            );
                        }
                    }
                }
                items.push(item);
                kept += 1;
            }
            tracing::info!(feed = %feed_cfg.id, kept, "rss: feed extracted");
        }

        tracing::info!(
            total = items.len(),
            feeds = p.feeds.len(),
            "rss: extraction complete"
        );
        Ok(items)
    }
}

/// Map one feed entry to a [`RawItem`], or `None` if it can't be placed/used:
/// undated (can't assign a day — never defaults to `now`), out of the day window,
/// or title-less.
fn map_entry(
    feed_id: &str,
    feed_title: &Option<String>,
    entry: feed_rs::model::Entry,
    min: jiff::Timestamp,
    max: jiff::Timestamp,
) -> Option<RawItem> {
    let dt = entry.published.or(entry.updated)?;
    let ts = jiff::Timestamp::from_millisecond(dt.timestamp_millis()).ok()?;
    if ts < min || ts >= max {
        return None;
    }

    let title = entry.title.map(|t| t.content)?;
    if title.trim().is_empty() {
        return None;
    }

    let body_html = entry
        .content
        .and_then(|c| c.body)
        .or_else(|| entry.summary.map(|s| s.content))
        .unwrap_or_default();
    let body = if body_html.is_empty() {
        String::new()
    } else {
        crate::markdown::html_to_markdown(&body_html)
    };

    let url = entry
        .links
        .iter()
        .find(|l| l.rel.as_deref() == Some("alternate"))
        .or_else(|| entry.links.first())
        .map(|l| l.href.clone());

    // Provenance: entry author → publication (feed title) → configured feed id.
    let author = entry
        .authors
        .first()
        .map(|a| a.name.clone())
        .or_else(|| feed_title.clone())
        .unwrap_or_else(|| feed_id.to_owned());

    // Namespace the external id by feed: entry ids are unique within a feed but
    // not across feeds, and one source polls many feeds — without the prefix two
    // feeds sharing an id on the same day would collapse to a single EventId.
    let raw_id = if entry.id.trim().is_empty() {
        url.clone().unwrap_or_else(|| title.clone())
    } else {
        entry.id
    };
    let external_id = format!("{feed_id}:{raw_id}");

    let categories: Vec<String> = entry.categories.into_iter().map(|c| c.term).collect();

    Some(RawItem {
        external_id: Some(external_id),
        title,
        body,
        url,
        author: Some(author),
        timestamp: ts,
        is_self: false,
        metadata: serde_json::json!({
            "feed_id": feed_id,
            "feed_title": feed_title,
            "categories": categories,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> (jiff::Timestamp, jiff::Timestamp) {
        (
            "2026-05-20T00:00:00Z".parse().unwrap(),
            "2026-05-22T00:00:00Z".parse().unwrap(),
        )
    }

    fn parse_one(xml: &str) -> feed_rs::model::Entry {
        feed_rs::parser::parse(xml.as_bytes())
            .unwrap()
            .entries
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn validate_rejects_empty_feeds_and_bad_url() {
        assert!(validate_params(&serde_json::json!({"feeds": []})).is_err());
        assert!(
            validate_params(&serde_json::json!({"feeds": [{"id": "x", "url": "ftp://x"}]}))
                .is_err()
        );
        assert!(
            validate_params(&serde_json::json!({"feeds": [{"id": "", "url": "https://x"}]}))
                .is_err()
        );
        assert!(
            validate_params(&serde_json::json!({
                "feeds": [{"id": "openai", "url": "https://openai.com/news/rss.xml"}],
                "lookback_hours": 24
            }))
            .is_ok()
        );
    }

    #[test]
    fn validate_rejects_duplicate_feed_ids() {
        assert!(
            validate_params(&serde_json::json!({"feeds": [
                {"id": "x", "url": "https://a"},
                {"id": "x", "url": "https://b"}
            ]}))
            .is_err()
        );
    }

    #[test]
    fn validate_rejects_unknown_keys() {
        assert!(
            validate_params(&serde_json::json!({
                "feeds": [{"id": "x", "url": "https://x", "typo": 1}]
            }))
            .is_err()
        );
    }

    #[test]
    fn maps_atom_entry_within_window() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Vendor Blog</title>
  <entry>
    <title>New model released</title>
    <id>tag:vendor,2026:1</id>
    <updated>2026-05-21T08:00:00Z</updated>
    <link rel="alternate" href="https://vendor.example/post/1"/>
    <summary type="html">&lt;p&gt;A &lt;b&gt;big&lt;/b&gt; release.&lt;/p&gt;</summary>
  </entry>
</feed>"#;
        let (min, max) = window();
        let item = map_entry(
            "vendor",
            &Some("Vendor Blog".into()),
            parse_one(xml),
            min,
            max,
        )
        .expect("entry maps");
        assert_eq!(item.title, "New model released");
        // external id is namespaced by feed id to avoid cross-feed collisions.
        assert_eq!(
            item.external_id.as_deref(),
            Some("vendor:tag:vendor,2026:1")
        );
        assert_eq!(item.url.as_deref(), Some("https://vendor.example/post/1"));
        assert!(item.body.contains("**big**")); // HTML → Markdown
        assert_eq!(item.timestamp.to_string(), "2026-05-21T08:00:00Z");
    }

    #[test]
    fn skips_out_of_window_entry() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>Old</title><id>a</id><updated>2026-01-01T00:00:00Z</updated></entry>
</feed>"#;
        let (min, max) = window();
        assert!(map_entry("v", &None, parse_one(xml), min, max).is_none());
    }

    #[test]
    fn skips_undated_entry_without_defaulting_to_now() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>No date</title><id>a</id></entry>
</feed>"#;
        let (min, max) = window();
        assert!(map_entry("v", &None, parse_one(xml), min, max).is_none());
    }

    #[test]
    fn falls_back_to_feed_id_for_provenance() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>T</title><id>a</id><updated>2026-05-21T00:00:00Z</updated></entry>
</feed>"#;
        let (min, max) = window();
        let item = map_entry("feedx", &None, parse_one(xml), min, max).unwrap();
        assert_eq!(item.author.as_deref(), Some("feedx"));
    }
}
