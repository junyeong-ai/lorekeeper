//! RSS/Atom feed source. Polls one or more public feeds (vendor blogs, news
//! aggregators) and maps each entry to a [`RawItem`]. No credentials — feeds are
//! public HTTP. Each entry's `external_id` frames its feed and entry identity as a tuple,
//! so the same article carried by two of this source's feeds stays two distinct
//! observations (each feed is its own provenance) rather than being merged.

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use crate::{ExtractContext, Source, SourceError};

mod base;

const MAX_FEED_BYTES: usize = 16 * 1024 * 1024;

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
        // Every per-source cap is validated `> 0` (a `0` cap drops every item from the first
        // entry on — an entire-feed silent loss, not a guard). RSS keeps that invariant.
        if self.max_items_per_feed == 0 {
            return Err(SourceError::InvalidParams(
                "rss `max_items_per_feed` must be > 0".into(),
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

    /// The article behind a feed entry, as Markdown.
    ///
    /// `None` when readability finds no article core: the caller then keeps the known-clean
    /// feed summary rather than adopting the page's boilerplate.
    async fn fetch_article(&self, article_url: &str) -> Result<Option<String>, SourceError> {
        Ok(crate::fetch::readable(&self.http, article_url)
            .await?
            .map(|a| a.markdown))
    }

    async fn fetch_feed(
        &self,
        url: &str,
    ) -> Result<(feed_rs::model::Feed, Vec<base::EntryBases>), SourceError> {
        let mut resp = self
            .http
            .get(url)
            .header(reqwest::header::USER_AGENT, crate::fetch::USER_AGENT)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(SourceError::Api {
                status: resp.status().as_u16(),
                message: format!("fetching feed {url}"),
            });
        }
        if resp
            .content_length()
            .is_some_and(|length| length > MAX_FEED_BYTES as u64)
        {
            return Err(SourceError::ResponseTooLarge {
                max_bytes: MAX_FEED_BYTES,
            });
        }
        let final_url = resp.url().clone();
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            if chunk.len() > MAX_FEED_BYTES - bytes.len() {
                return Err(SourceError::ResponseTooLarge {
                    max_bytes: MAX_FEED_BYTES,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        let (mut bases, rewritten) = base::capture(&bytes, &final_url)?;
        let feed_bytes = rewritten.as_deref().unwrap_or(&bytes);
        let feed = feed_rs::parser::Builder::new()
            .base_uri(Some(final_url.as_str()))
            .build()
            .parse(feed_bytes)
            .map_err(|e| SourceError::Parse(e.to_string()))?;
        if bytes
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace())
            == Some(b'{')
        {
            let final_url = std::sync::Arc::new(final_url);
            bases = vec![
                base::EntryBases {
                    content: Some(final_url.clone()),
                    summary: Some(final_url.clone())
                };
                feed.entries.len()
            ];
        }
        if bases.len() != feed.entries.len() {
            return Err(SourceError::Parse(
                "feed content base: parsed entry inventory differs from XML base scopes".into(),
            ));
        }
        Ok((feed, bases))
    }
}

#[async_trait]
impl Source for RssSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        Ok(self.observe(params, ctx).await?.items)
    }

    async fn observe(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<crate::SourceObservation, SourceError> {
        let mut partial = false;
        let p: RssParams = crate::parse_validated(params)?;

        // News is past-dated, so pad only the lower bound. The pipeline still
        // splits items onto their own publication date (multi-date batch).
        let (min, max) = ctx.day_window(p.lookback_hours, 0)?;

        let mut items = crate::SourceItems::new(ctx);
        // Counted upward, never derived by subtraction — see `require_any_observation`.
        let mut feeds_read = 0usize;
        for feed_cfg in &p.feeds {
            let (feed, bases) = match self.fetch_feed(&feed_cfg.url).await {
                Ok(f) => f,
                Err(e) => {
                    partial = true;
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

            // Per feed: how many entries this feed offered, against how many became an
            // observation (kept, or dated for another day). `seen` is incremented before
            // any branch and `observed` only where an entry actually became one, so a skip
            // added later cannot quietly go uncounted, and a success path added without
            // marking itself fails loudly rather than silently.
            let (mut seen, mut observed) = (0usize, 0usize);
            let mut kept = 0usize;
            for (entry, bases) in feed.entries.into_iter().zip(bases) {
                seen += 1;
                let Some(MappedEntry {
                    mut item,
                    supplied_unrepresentable_body,
                }) = map_entry(&feed_cfg.id, &feed_title, entry, &bases)
                else {
                    partial = true;
                    continue;
                };
                if item.timestamp < min || item.timestamp >= max {
                    // Another day's item. This is what a quiet feed is made of, so it is
                    // never a sign that anything is wrong.
                    observed += 1;
                    continue;
                }
                if kept >= p.max_items_per_feed {
                    partial = true;
                    tracing::warn!(
                        feed = %feed_cfg.id,
                        max = p.max_items_per_feed,
                        "rss: per-feed cap omitted an in-window entry; raise max_items_per_feed"
                    );
                    break;
                }
                let mut article_recovered_body = false;
                // Article enrichment is separate from feed/query reach. Failure keeps
                // the collected summary and its warning, just like optional meeting notes.
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
                            article_recovered_body = true;
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
                if supplied_unrepresentable_body && !article_recovered_body {
                    partial = true;
                    continue;
                }
                items.push(item)?;
                kept += 1;
                observed += 1;
            }
            // A feed that returned entries and yielded not one observation was not read,
            // whatever the transport said — the same state as an unreachable feed, reached
            // by a format change instead of a moved URL. An EMPTY feed is observed: there
            // was nothing to misread.
            if seen > observed && observed == 0 {
                tracing::warn!(
                    feed = %feed_cfg.id,
                    unusable = seen,
                    "rss: every entry was unusable (date, title, or inline representation); treating the feed as unread"
                );
            } else {
                feeds_read += 1;
            }
            tracing::info!(
                feed = %feed_cfg.id,
                kept,
                unobserved = seen - observed,
                "rss: feed extracted"
            );
        }

        crate::require_any_observation("feed", feeds_read, p.feeds.len())?;

        tracing::info!(
            total = items.len(),
            feeds = p.feeds.len(),
            feeds_read,
            "rss: extraction complete"
        );
        Ok(crate::SourceObservation {
            items: items.into_vec(),
            collection_identity: None,
            // A feed is a finite current inventory, not a historical day query.
            // min/max only filter returned entries and cannot establish date coverage.
            scope: crate::ObservationScope::ConfiguredQuery,
            outcome: if partial {
                crate::ObservationOutcome::Partial
            } else {
                crate::ObservationOutcome::Observed
            },
        })
    }
}

struct MappedEntry {
    item: RawItem,
    /// Defer refusal until configured article enrichment can recover readable text.
    supplied_unrepresentable_body: bool,
}

/// Map one feed entry, or `None` when its publication date/title is unusable.
/// Nonempty unsupported inline content still has a chance to be recovered by
/// configured article enrichment. The day window is deliberately NOT decided
/// here — an entry outside it is a perfectly good observation belonging to another day, and
/// conflating the two would make a quiet feed indistinguishable from a broken one.
fn map_entry(
    feed_id: &str,
    feed_title: &Option<String>,
    entry: feed_rs::model::Entry,
    bases: &base::EntryBases,
) -> Option<MappedEntry> {
    let dt = entry.published.or(entry.updated)?;
    let ts = jiff::Timestamp::from_millisecond(dt.timestamp_millis()).ok()?;

    let title = entry.title.map(|t| t.content)?;
    if title.trim().is_empty() {
        return None;
    }

    let content = entry.content.as_ref().and_then(|content| {
        content
            .body
            .as_deref()
            .map(|body| (body, content.content_type.as_str()))
    });
    let summary = entry
        .summary
        .as_ref()
        .map(|summary| (summary.content.as_str(), summary.content_type.as_str()));
    let body = content
        .and_then(|(text, media)| feed_text(text, media, bases.content.as_deref()))
        .or_else(|| {
            summary.and_then(|(text, media)| feed_text(text, media, bases.summary.as_deref()))
        });
    let supplied_unrepresentable_body = body.is_none()
        && content
            .into_iter()
            .chain(summary)
            .any(|(text, _)| !text.trim().is_empty());
    let body = body.unwrap_or_default();

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
        .and_then(|a| a.name.clone().or_else(|| a.email.clone()))
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
    let identity = lk_core::rss_original::RssEntryIdentity::new(feed_id, &raw_id);
    let external_id = identity.external_id();

    let categories: Vec<String> = entry.categories.into_iter().map(|c| c.term).collect();

    Some(MappedEntry {
        supplied_unrepresentable_body,
        item: RawItem {
            labels: Vec::new(),
            external_id: Some(external_id),
            title,
            body,
            url,
            author: Some(author),
            timestamp: ts,
            is_self: false,
            work: None,
            metadata: serde_json::json!({
                "feed_id": feed_id,
                "rss_identity": identity,
                "feed_title": feed_title,
                "categories": categories,
            }),
        },
    })
}

/// Feed parsers already decode Atom text constructs. Only declared HTML is markup;
/// literal text must also stay literal when embedded in the Markdown daily page.
fn feed_text(text: &str, content_type: &str, base: Option<&url::Url>) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let media = match content_type.parse::<mime::Mime>() {
        Ok(media) => media,
        Err(error) => {
            tracing::warn!(content_type, %error, "rss: invalid feed content type; trying summary");
            return None;
        }
    };
    match media.essence_str() {
        // feed-rs represents Atom type="xhtml" as application/xhtml.
        "text/html" | "application/xhtml" | "application/xhtml+xml" => {
            project_feed_text(crate::remote_text::html(text, base))
        }
        "text/markdown" | "text/x-markdown" => {
            project_feed_text(crate::remote_text::markdown(text, base))
        }
        "text/plain" => match crate::fetch::literal_markdown(text) {
            Ok(body) => Some(body),
            Err(error) => {
                tracing::warn!(%error, "rss: could not project plain feed text as Markdown");
                None
            }
        },
        _ => {
            tracing::warn!(
                content_type,
                "rss: unsupported feed content type; trying summary"
            );
            None
        }
    }
}

fn project_feed_text(result: Result<String, SourceError>) -> Option<String> {
    match result {
        Ok(body) => Some(body),
        Err(error) => {
            tracing::warn!(%error, "rss: unresolved inline source dependencies; trying summary");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_entry(
        feed_id: &str,
        feed_title: &Option<String>,
        entry: feed_rs::model::Entry,
    ) -> Option<RawItem> {
        super::map_entry(feed_id, feed_title, entry, &base::EntryBases::default())
            .map(|mapped| mapped.item)
    }

    #[tokio::test]
    async fn redirected_feed_and_xml_body_bases_remain_remote_uri_authority() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for expected in ["/start", "/feeds/current.xml"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    assert_ne!(stream.read(&mut byte).await.unwrap(), 0);
                    request.push(byte[0]);
                    assert!(request.len() < 16 * 1024);
                }
                assert!(
                    String::from_utf8(request)
                        .unwrap()
                        .starts_with(&format!("GET {expected} "))
                );
                if expected == "/start" {
                    stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /feeds/current.xml\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                } else {
                    let body = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>Redirected</title><id>redirected</id><updated>2026-05-23T01:00:00Z</updated><summary type="html">&lt;a href='child.md'&gt;child&lt;/a&gt;</summary></entry><entry xml:base="https://remote.example/root/"><title>Scoped</title><id>scoped</id><updated>2026-05-23T01:00:00Z</updated><summary type="html" xml:base="summary/">&lt;a href='../../wiki/concepts/topic.md'&gt;remote topic&lt;/a&gt;</summary></entry></feed>"#;
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/atom+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(header.as_bytes()).await.unwrap();
                    stream.write_all(body.as_bytes()).await.unwrap();
                }
                stream.shutdown().await.unwrap();
            }
        });
        let source = RssSource::new(loopback_client());
        let params = serde_json::json!({"feeds": [{"id": "base-fixture", "url": format!("http://{address}/start")}], "lookback_hours": 0});
        let observed = source.observe(&params, &test_ctx()).await.unwrap();
        server.await.unwrap();
        assert_eq!(observed.outcome, crate::ObservationOutcome::Observed);
        assert_eq!(observed.items.len(), 2);
        assert!(
            observed.items[0]
                .body
                .contains(&format!("http://{address}/feeds/child.md")),
            "{}",
            observed.items[0].body
        );
        assert!(
            observed.items[1]
                .body
                .contains("https://remote.example/wiki/concepts/topic.md"),
            "{}",
            observed.items[1].body
        );
        for item in observed.items {
            assert!(
                lk_core::link::extract_page_links(&item.body).is_empty(),
                "{}",
                item.body
            );
        }
    }

    /// Per-feed isolation must not extend to a source that reached NOTHING. An empty
    /// success there is indistinguishable from a quiet day, and the ingest log records only
    /// that one bit — so `lore health`, whose only evidence is that log, would report a
    /// source whose every feed URL has moved as fresh indefinitely.
    #[tokio::test]
    async fn feed_response_limit_refuses_declared_and_streamed_overflow_without_truncation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let feed = "<feed xmlns=\"http://www.w3.org/2005/Atom\"><entry><title>Complete evidence</title><id>a</id><updated>2026-05-23T00:00:00Z</updated></entry></feed>";
        let source = RssSource::new(loopback_client());
        for mode in ["declared", "streamed", "exact"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                let received = stream.read(&mut request).await.unwrap();
                assert!(received > 0);
                let result = async {
                    if mode == "declared" {
                        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", MAX_FEED_BYTES + 1).as_bytes()).await?;
                    } else {
                        let size = MAX_FEED_BYTES + usize::from(mode == "streamed");
                        let mut body = feed.as_bytes().to_vec();
                        body.resize(size, b' ');
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/atom+xml\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await?;
                        stream.write_all(format!("{:x}\r\n", body.len()).as_bytes()).await?;
                        stream.write_all(&body).await?;
                        stream.write_all(b"\r\n0\r\n\r\n").await?;
                    }
                    stream.shutdown().await
                }.await;
                if mode == "exact" {
                    result.unwrap();
                }
            });
            let result = source.fetch_feed(&format!("http://{address}/feed")).await;
            server.await.unwrap();
            if mode == "exact" {
                assert_eq!(result.unwrap().0.entries.len(), 1);
            } else {
                assert!(
                    matches!(
                        result,
                        Err(SourceError::ResponseTooLarge {
                            max_bytes: MAX_FEED_BYTES
                        })
                    ),
                    "{mode}: overflow must be a typed error, even though a truncated prefix would contain a valid complete feed"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_source_whose_every_feed_is_unreachable_fails_rather_than_reporting_nothing() {
        // Connection-refused on the discard port: no network egress, no timeout wait.
        let params = serde_json::json!({
            "feeds": [
                {"id": "a", "url": "http://127.0.0.1:1/a.xml"},
                {"id": "b", "url": "http://127.0.0.1:1/b.xml"},
            ]
        });
        let source = RssSource::new(loopback_client());
        let err = source
            .extract(&params, &test_ctx())
            .await
            .expect_err("every feed failed, so nothing was observed");
        assert!(
            err.to_string()
                .contains("no feed could be read (of 2 tried)"),
            "{err}"
        );
    }

    /// A feed that answered but whose every entry is unreadable was not read either — the
    /// same state as an unreachable one, reached by a format change instead of a moved URL.
    /// Guarding only the fetch would leave that total outage reporting a clean, quiet run.
    #[tokio::test]
    async fn a_feed_whose_every_entry_is_unusable_counts_as_unread() {
        let server = tiny_feed_server(
            r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>No date</title><id>a</id></entry>
  <entry><title>Also undated</title><id>b</id></entry>
</feed>"#,
        )
        .await;
        let params = serde_json::json!({
            "feeds": [{"id": "a", "url": format!("http://{}/f.xml", server.addr)}]
        });
        let source = RssSource::new(loopback_client());
        let err = source
            .extract(&params, &test_ctx())
            .await
            .expect_err("no entry could become an observation");
        assert!(
            err.to_string()
                .contains("no feed could be read (of 1 tried)"),
            "{err}"
        );
    }

    /// A feed whose entries are all dated for OTHER days is a quiet feed, not a broken one.
    /// This is the false positive the split between mapping and windowing exists to prevent.
    #[tokio::test]
    async fn a_feed_with_only_out_of_window_entries_is_quiet_not_broken() {
        let server = tiny_feed_server(
            r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>Old news</title><id>a</id><updated>2020-01-01T00:00:00Z</updated></entry>
</feed>"#,
        )
        .await;
        let params = serde_json::json!({
            "feeds": [{"id": "a", "url": format!("http://{}/f.xml", server.addr)}]
        });
        let source = RssSource::new(loopback_client());
        let observed = source
            .observe(&params, &test_ctx())
            .await
            .expect("a quiet feed is a success");
        assert!(
            observed.items.is_empty(),
            "and it yields nothing for this day"
        );
        assert_eq!(observed.scope, crate::ObservationScope::ConfiguredQuery);
        assert_eq!(observed.outcome, crate::ObservationOutcome::Observed);
    }

    #[tokio::test]
    async fn exact_feed_cap_with_only_older_remaining_entries_is_observed() {
        let server = tiny_feed_server(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
          <entry><title>Recent</title><id>recent</id><updated>2026-05-23T01:00:00Z</updated></entry>
          <entry><title>Old</title><id>old</id><updated>2026-05-01T01:00:00Z</updated></entry>
        </feed>"#,
        )
        .await;
        let params = serde_json::json!({
            "feeds": [{"id": "a", "url": format!("http://{}/f.xml", server.addr)}],
            "max_items_per_feed": 1,
        });
        let source = RssSource::new(loopback_client());
        let observation = source.observe(&params, &test_ctx()).await.unwrap();
        assert_eq!(observation.items.len(), 1);
        assert_eq!(observation.items[0].title, "Recent");
        assert_eq!(observation.scope, crate::ObservationScope::ConfiguredQuery);
        assert_eq!(observation.outcome, crate::ObservationOutcome::Observed);
    }

    #[tokio::test]
    async fn feed_cap_preserves_matching_overflow_and_malformed_tail_diagnostics() {
        for tail in [
            r#"<entry><title>Later matching</title><id>later</id><updated>2026-05-23T02:00:00Z</updated></entry>"#,
            r#"<entry><title>Undated</title><id>undated</id></entry>"#,
        ] {
            let server = tiny_feed_server(format!(
                r#"<feed xmlns="http://www.w3.org/2005/Atom">
              <entry><title>Recent</title><id>recent</id><updated>2026-05-23T01:00:00Z</updated></entry>
              <entry><title>Old</title><id>old</id><updated>2026-05-01T01:00:00Z</updated></entry>
              {tail}
            </feed>"#
            ))
            .await;
            let params = serde_json::json!({
                "feeds": [{"id": "a", "url": format!("http://{}/f.xml", server.addr)}],
                "max_items_per_feed": 1,
            });
            let source = RssSource::new(loopback_client());
            let observation = source.observe(&params, &test_ctx()).await.unwrap();
            assert_eq!(observation.items.len(), 1);
            assert_eq!(observation.items[0].title, "Recent");
            assert_eq!(observation.scope, crate::ObservationScope::ConfiguredQuery);
            assert_eq!(observation.outcome, crate::ObservationOutcome::Partial);
        }
    }

    #[tokio::test]
    async fn feed_reach_reports_cap_invalid_entries_and_failed_feed_without_inventing_day_coverage()
    {
        let server = tiny_feed_server(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
          <entry><title>One</title><id>a</id><updated>2026-05-23T01:00:00Z</updated></entry>
          <entry><title>Two</title><id>b</id><updated>2026-05-23T02:00:00Z</updated></entry>
        </feed>"#,
        )
        .await;
        let source = RssSource::new(loopback_client());
        let params = serde_json::json!({"feeds": [{"id": "a", "url": format!("http://{}/f.xml", server.addr)}], "max_items_per_feed": 1});
        let observation = source.observe(&params, &test_ctx()).await.unwrap();
        assert_eq!(observation.items.len(), 1);
        assert_eq!(observation.scope, crate::ObservationScope::ConfiguredQuery);
        assert_eq!(observation.outcome, crate::ObservationOutcome::Partial);
        let mut full = params.clone();
        full["max_items_per_feed"] = 2.into();
        assert_eq!(
            source.observe(&full, &test_ctx()).await.unwrap().outcome,
            crate::ObservationOutcome::Observed,
            "exactly reaching a cap without remaining entries is complete"
        );
        full["feeds"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id": "broken", "url": "http://127.0.0.1:1/missing"}));
        assert_eq!(
            source.observe(&full, &test_ctx()).await.unwrap().outcome,
            crate::ObservationOutcome::Partial
        );
        let mixed = tiny_feed_server(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
          <entry><title>Good</title><id>a</id><updated>2026-05-23T01:00:00Z</updated></entry>
          <entry><title>Undated</title><id>b</id></entry>
        </feed>"#,
        )
        .await;
        let params = serde_json::json!({"feeds": [{"id": "a", "url": format!("http://{}/f.xml", mixed.addr)}]});
        assert_eq!(
            source.observe(&params, &test_ctx()).await.unwrap().outcome,
            crate::ObservationOutcome::Partial
        );
    }

    #[tokio::test]
    async fn unsupported_inline_content_with_valid_summary_is_observed() {
        let server = tiny_feed_server(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
<entry><title>Attachment</title><id>a</id><updated>2026-05-23T01:00:00Z</updated>
<content type="application/octet-stream">Ym9keQ==</content>
<summary type="text">&lt;b&gt;literal fallback&lt;/b&gt;</summary></entry>
</feed>"#,
        )
        .await;
        let source = RssSource::new(loopback_client());
        let params = serde_json::json!({"feeds": [{"id": "a", "url": format!("http://{}/f.xml", server.addr)}]});
        let observation = source.observe(&params, &test_ctx()).await.unwrap();
        assert_eq!(observation.items.len(), 1);
        assert_eq!(observation.outcome, crate::ObservationOutcome::Observed);
        let visible = pulldown_cmark::Parser::new(&observation.items[0].body)
            .filter_map(|event| match event {
                pulldown_cmark::Event::Text(text) => Some(text.into_string()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(visible, "<b>literal fallback</b>");
    }

    #[tokio::test]
    async fn unsupported_inline_content_without_summary_is_unobserved() {
        let bad_entry = r#"<entry><title>Unsupported</title><id>bad</id><updated>2026-05-23T01:00:00Z</updated>
<content type="application/octet-stream">Ym9keQ==</content></entry>"#;
        let entry_xml = format!(r#"<feed xmlns="http://www.w3.org/2005/Atom">{bad_entry}</feed>"#);
        assert!(
            super::map_entry(
                "fixture",
                &None,
                parse_one(&entry_xml),
                &base::EntryBases::default()
            )
            .unwrap()
            .supplied_unrepresentable_body
        );

        let only_bad = tiny_feed_server(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
<entry><title>Unsupported</title><id>bad</id><updated>2026-05-23T01:00:00Z</updated>
<content type="application/octet-stream">Ym9keQ==</content></entry></feed>"#,
        )
        .await;
        let source = RssSource::new(loopback_client());
        let params = serde_json::json!({"feeds": [{"id": "a", "url": format!("http://{}/f.xml", only_bad.addr)}]});
        assert!(matches!(
            source.observe(&params, &test_ctx()).await,
            Err(SourceError::NothingObserved { .. })
        ));

        let mixed = tiny_feed_server(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
<entry><title>Good</title><id>good</id><updated>2026-05-23T01:00:00Z</updated><summary>Readable</summary></entry>
<entry><title>Unsupported</title><id>bad</id><updated>2026-05-23T01:00:00Z</updated>
<content type="application/octet-stream">Ym9keQ==</content></entry></feed>"#,
        )
        .await;
        let params = serde_json::json!({"feeds": [{"id": "a", "url": format!("http://{}/f.xml", mixed.addr)}]});
        let observation = source.observe(&params, &test_ctx()).await.unwrap();
        assert_eq!(observation.items.len(), 1);
        assert_eq!(observation.items[0].title, "Good");
        assert_eq!(observation.outcome, crate::ObservationOutcome::Partial);
    }

    #[tokio::test]
    async fn configured_article_can_rescue_unsupported_inline_content() {
        let article = tiny_server(
            "Readable article text recovered through configured enrichment.",
            "text/plain",
        )
        .await;
        for (fetch_full_text, article_url, recovered) in [
            (true, format!("http://{}/article", article.addr), true),
            (false, format!("http://{}/article", article.addr), false),
            (true, "http://127.0.0.1:1/unavailable".into(), false),
        ] {
            let feed = tiny_feed_server(format!(
                r#"<feed xmlns="http://www.w3.org/2005/Atom">
<entry><title>Article</title><id>a</id><updated>2026-05-23T01:00:00Z</updated>
<content type="application/octet-stream">Ym9keQ==</content>
<link rel="alternate" href="{article_url}"/></entry></feed>"#
            ))
            .await;
            let source = RssSource::new(loopback_client());
            let params = serde_json::json!({"feeds": [{"id": "a", "url": format!("http://{}/f.xml", feed.addr), "fetch_full_text": fetch_full_text}]});
            match source.observe(&params, &test_ctx()).await {
                Ok(observation) => {
                    assert!(recovered);
                    assert_eq!(observation.outcome, crate::ObservationOutcome::Observed);
                    assert_eq!(observation.items.len(), 1);
                    assert!(
                        observation.items[0]
                            .body
                            .contains("Readable article text recovered")
                    );
                }
                Err(SourceError::NothingObserved { .. }) => assert!(!recovered),
                Err(error) => panic!("unexpected observation error: {error}"),
            }
        }
    }

    #[tokio::test]
    async fn requested_article_failure_keeps_summary_without_changing_feed_reach() {
        let server = tiny_feed_server(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
          <entry><title>Article</title><id>a</id><updated>2026-05-23T01:00:00Z</updated>
          <summary>Preserved feed summary</summary><link href="http://127.0.0.1:1/article"/></entry>
        </feed>"#,
        )
        .await;
        let source = RssSource::new(loopback_client());
        let params = serde_json::json!({"feeds": [{"id": "a", "url": format!("http://{}/f.xml", server.addr), "fetch_full_text": true}]});
        let observation = source.observe(&params, &test_ctx()).await.unwrap();
        assert_eq!(observation.items[0].body, "Preserved feed summary");
        assert_eq!(observation.outcome, crate::ObservationOutcome::Observed);
        assert_eq!(observation.scope, crate::ObservationScope::ConfiguredQuery);
    }

    /// A client for the loopback fixtures below, which differs from the production one in the
    /// single respect that matters here: `reqwest` honours `http_proxy`/`ALL_PROXY` from the
    /// environment, so on a machine that sets either, a fetch aimed at `127.0.0.1` leaves for
    /// the proxy and the fixture never answers. Production keeps that behaviour deliberately —
    /// a real feed may only be reachable through a proxy — so the fixture opts out rather than
    /// the code.
    fn loopback_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap()
    }

    /// A single-response HTTP server on loopback — enough to answer one feed fetch without
    /// taking a mocking dependency or leaving the machine.
    struct TinyServer {
        addr: std::net::SocketAddr,
    }

    async fn tiny_feed_server(body: impl Into<String>) -> TinyServer {
        tiny_server(body, "application/atom+xml").await
    }

    async fn tiny_server(body: impl Into<String>, content_type: &'static str) -> TinyServer {
        let body = body.into();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        TinyServer { addr }
    }

    fn test_ctx() -> ExtractContext {
        ExtractContext {
            target_date: jiff::civil::date(2026, 5, 23),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::En,
            identity: lk_core::config::Identity {
                name: "T".into(),
                email: "t@example.com".into(),
                slack_id: None,
            },
            vault_root: std::path::PathBuf::from("."),
            vault_dirs: Default::default(),
            documents: None,
            projection_cache: None,
            originals: Default::default(),
            max_input_bytes: lk_core::config::IngestInputLimits::default()
                .max_source_bytes
                .get(),
        }
    }

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
    fn validate_rejects_zero_max_items_per_feed() {
        // Every per-source cap is validated `> 0`; a `0` cap silently drops every entry.
        assert!(
            validate_params(&serde_json::json!({
                "feeds": [{"id": "x", "url": "https://x"}],
                "max_items_per_feed": 0
            }))
            .is_err()
        );
        // Omitted → default (50), valid.
        assert!(
            validate_params(&serde_json::json!({
                "feeds": [{"id": "x", "url": "https://x"}]
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
    fn an_email_only_feed_author_keeps_provenance_and_entry_identity() {
        let xml = r#"<rss version="2.0"><channel><title>Publication</title>
<link>https://example.com</link><description>Feed</description><item>
<title>Post</title><guid isPermaLink="false">stable-guid</guid>
<pubDate>Thu, 21 May 2026 08:00:00 GMT</pubDate>
<author>editor@example.com</author><description>Body</description>
</item></channel></rss>"#;
        let item = map_entry("fixture", &Some("Publication".into()), parse_one(xml)).unwrap();
        assert_eq!(item.author.as_deref(), Some("editor@example.com"));
        assert_eq!(
            item.external_id.as_deref(),
            Some(r#"["fixture","stable-guid"]"#)
        );
        assert_eq!(item.timestamp.to_string(), "2026-05-21T08:00:00Z");
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
        let item =
            map_entry("vendor", &Some("Vendor Blog".into()), parse_one(xml)).expect("entry maps");
        assert_eq!(item.title, "New model released");
        // external id is namespaced by feed id to avoid cross-feed collisions.
        assert_eq!(
            item.external_id.as_deref(),
            Some(r#"["vendor","tag:vendor,2026:1"]"#)
        );
        assert_eq!(item.url.as_deref(), Some("https://vendor.example/post/1"));
        assert!(item.body.contains("**big**")); // HTML → Markdown
        assert_eq!(item.timestamp.to_string(), "2026-05-21T08:00:00Z");
    }

    #[tokio::test]
    async fn distinct_feed_entry_tuples_survive_collection_and_normalization_identity() {
        let first = tiny_feed_server(r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>First</title><id>item1</id><updated>2026-05-23T01:00:00Z</updated><summary>First evidence</summary></entry></feed>"#).await;
        let second = tiny_feed_server(r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>Second</title><id>news:item1</id><updated>2026-05-23T01:00:00Z</updated><summary>Second evidence</summary></entry></feed>"#).await;
        let params = serde_json::json!({"feeds":[
            {"id":"org:news", "url":format!("http://{}/feed",first.addr)},
            {"id":"org", "url":format!("http://{}/feed",second.addr)},
        ]});
        let observed = RssSource::new(loopback_client())
            .observe(&params, &test_ctx())
            .await
            .unwrap();
        assert_eq!(observed.items.len(), 2);
        assert_ne!(observed.items[0].external_id, observed.items[1].external_id);
        let identities = observed
            .items
            .iter()
            .map(|item| {
                let identity =
                    lk_core::rss_original::RssEntryIdentity::from_metadata(&item.metadata)
                        .unwrap()
                        .unwrap();
                assert_eq!(
                    item.external_id.as_deref(),
                    Some(identity.external_id().as_str())
                );
                identity.event_id("rss", test_ctx().target_date)
            })
            .collect::<Vec<_>>();
        assert_ne!(identities[0], identities[1]);
    }

    #[test]
    fn atom_plain_content_and_summary_remain_literal_markdown() {
        for element in [
            "content type=\"text\"",
            "content",
            "summary type=\"text\"",
            "summary",
        ] {
            let tag = element.split_whitespace().next().unwrap();
            let xml = format!(
                r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry>
<title>Literal syntax</title><id>literal</id><updated>2026-05-21T08:00:00Z</updated>
<{element}>Use &lt;script&gt;safe literal&lt;/script&gt; and &lt;b&gt;literal tags&lt;/b&gt;.

# Not a heading
**not emphasis** [not a link](https://example.com) `not code`
</{tag}></entry></feed>"#
            );
            let item = map_entry("fixture", &None, parse_one(&xml)).unwrap();
            let events: Vec<_> = pulldown_cmark::Parser::new(&item.body).collect();
            assert!(
                events.iter().all(|event| matches!(
                    event,
                    pulldown_cmark::Event::Start(pulldown_cmark::Tag::Paragraph)
                        | pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Paragraph)
                        | pulldown_cmark::Event::Text(_)
                        | pulldown_cmark::Event::HardBreak
                        | pulldown_cmark::Event::SoftBreak
                )),
                "{element}: {:?}",
                events
            );
            let visible = events
                .iter()
                .filter_map(|event| match event {
                    pulldown_cmark::Event::Text(text) => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<String>();
            for literal in [
                "<script>safe literal</script>",
                "<b>literal tags</b>",
                "# Not a heading",
                "**not emphasis** [not a link](https://example.com) `not code`",
            ] {
                assert!(visible.contains(literal), "{element}: {visible}");
            }
        }
    }

    #[test]
    fn atom_declared_html_and_xhtml_remain_markup() {
        for markup in [
            r#"<content type="html">&lt;p&gt;A &lt;b&gt;big&lt;/b&gt; release.&lt;/p&gt;</content>"#,
            r#"<content type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml"><p>A <b>big</b> release.</p></div></content>"#,
            r#"<summary type="html">&lt;p&gt;A &lt;b&gt;big&lt;/b&gt; release.&lt;/p&gt;</summary>"#,
            r#"<summary type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml"><p>A <b>big</b> release.</p></div></summary>"#,
        ] {
            let xml = format!(
                r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry>
<title>Markup</title><id>markup</id><updated>2026-05-21T08:00:00Z</updated>
{markup}</entry></feed>"#
            );
            let item = map_entry("fixture", &None, parse_one(&xml)).unwrap();
            assert!(
                item.body.contains("A **big** release."),
                "{markup}: {}",
                item.body
            );
        }
    }

    #[test]
    fn atom_unusable_content_falls_back_to_typed_summary() {
        assert!(feed_text("", "text/plain", None).is_none());
        for content in [
            r#"<content type="application/octet-stream">Ym9keQ==</content>"#,
            r#"<content type="text/plain" src="https://example.com/body"/>"#,
        ] {
            let xml = format!(
                r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry>
<title>Fallback</title><id>fallback</id><updated>2026-05-21T08:00:00Z</updated>
{content}<summary type="text">&lt;b&gt;literal fallback&lt;/b&gt;</summary>
</entry></feed>"#
            );
            let item = map_entry("fixture", &None, parse_one(&xml)).unwrap();
            let visible = pulldown_cmark::Parser::new(&item.body)
                .filter_map(|event| match event {
                    pulldown_cmark::Event::Text(text) => Some(text.into_string()),
                    _ => None,
                })
                .collect::<String>();
            assert!(
                visible.contains("<b>literal fallback</b>"),
                "{content}: {visible}"
            );
        }
    }

    /// An entry outside the day window still MAPS — it is a good observation belonging to
    /// another day. The caller decides the window, so a quiet feed can never be mistaken
    /// for one whose entries cannot be read at all.
    #[test]
    fn an_out_of_window_entry_still_maps() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>Old</title><id>a</id><updated>2026-01-01T00:00:00Z</updated></entry>
</feed>"#;
        let (min, max) = window();
        let item = map_entry("v", &None, parse_one(xml)).expect("a dated, titled entry maps");
        assert!(
            item.timestamp < min || item.timestamp >= max,
            "and the caller is the one that finds it out of window"
        );
    }

    #[test]
    fn skips_undated_entry_without_defaulting_to_now() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>No date</title><id>a</id></entry>
</feed>"#;
        assert!(map_entry("v", &None, parse_one(xml)).is_none());
    }

    #[test]
    fn skips_titleless_entry() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><id>a</id><updated>2026-05-21T00:00:00Z</updated></entry>
</feed>"#;
        assert!(map_entry("v", &None, parse_one(xml)).is_none());
    }

    #[test]
    fn falls_back_to_feed_id_for_provenance() {
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>T</title><id>a</id><updated>2026-05-21T00:00:00Z</updated></entry>
</feed>"#;
        let item = map_entry("feedx", &None, parse_one(xml)).unwrap();
        assert_eq!(item.author.as_deref(), Some("feedx"));
    }
}
