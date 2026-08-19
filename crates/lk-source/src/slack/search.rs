use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use super::{resolve_users, slack_post, split_first_line};
use crate::markdown::slack_to_markdown;
use crate::{ExtractContext, Source, SourceError};

pub struct SlackSearchSource {
    http: reqwest::Client,
    token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlackSearchParams {
    queries: Vec<QueryParams>,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    #[serde(default = "default_max_matches")]
    max_matches_per_query: usize,
}

fn default_lookback() -> u32 {
    24
}

fn default_max_matches() -> usize {
    200
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryParams {
    channel: String,
    keywords: Vec<String>,
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    crate::parse_validated::<SlackSearchParams>(params).map(|_| ())
}

impl crate::ValidatedParams for SlackSearchParams {
    fn validate(&self) -> Result<(), SourceError> {
        if self.max_matches_per_query == 0 {
            return Err(SourceError::InvalidParams(
                "slack-search `max_matches_per_query` must be > 0".into(),
            ));
        }
        if self.queries.is_empty() {
            return Err(SourceError::InvalidParams(
                "slack-search `queries` must list at least one query".into(),
            ));
        }
        for q in &self.queries {
            if q.channel.trim().is_empty() {
                return Err(SourceError::InvalidParams(
                    "slack-search query `channel` must not be blank".into(),
                ));
            }
            // The query is assembled as `in:#<channel> after:… before:… <keywords joined by OR>`.
            // Empty or blank keywords collapse the keyword clause to nothing, turning a targeted
            // keyword-trend search into an unbounded whole-channel scrape — the opposite of this
            // source's purpose, and a quota/noise hazard. A keyword search needs keywords.
            if q.keywords.is_empty() || q.keywords.iter().any(|k| k.trim().is_empty()) {
                return Err(SourceError::InvalidParams(
                    "slack-search query `keywords` must list at least one non-blank keyword — \
                     a blank keyword clause would search the whole channel instead of a trend."
                        .into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct SearchData {
    messages: Option<SearchMessages>,
}

#[derive(Deserialize)]
struct SearchMessages {
    matches: Option<Vec<SearchMatch>>,
    paging: Option<SearchPaging>,
}

#[derive(Deserialize)]
struct SearchPaging {
    page: Option<u32>,
    pages: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SearchMatch {
    ts: String,
    text: String,
    user: Option<String>,
    channel: Option<MatchChannel>,
    permalink: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MatchChannel {
    id: Option<String>,
    name: Option<String>,
}

impl SlackSearchSource {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self { http, token }
    }
}

#[async_trait]
impl Source for SlackSearchSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: SlackSearchParams = crate::parse_validated(params)?;

        let users = resolve_users(&self.http, &self.token).await;

        // Slack search date operators are day-granular and exclusive. Bound the query
        // to the target day (`after` the prior day, `before` the next) anchored to
        // ctx.target_date — not "now" — so --date backfill searches the right day. The
        // pipeline still date-filters precisely afterward.
        // Slack search date operators are day-granular, so round the hour lookback UP to
        // whole days — flooring would drop the boundary day for e.g. a 36h window.
        let lookback_days = ((i64::from(p.lookback_hours) + 23) / 24).max(1);
        let after = ctx
            .target_date
            .checked_sub(jiff::Span::new().days(lookback_days))
            .map_err(|e| SourceError::Parse(e.to_string()))?;
        let before = ctx
            .target_date
            .tomorrow()
            .map_err(|e| SourceError::Parse(e.to_string()))?;
        let after_str = after.to_string();
        let before_str = before.to_string();

        let mut all_items = Vec::new();

        for spec in &p.queries {
            let channel_name = spec.channel.strip_prefix('#').unwrap_or(&spec.channel);
            let kw = spec.keywords.join(" OR ");
            let query = format!("in:#{channel_name} after:{after_str} before:{before_str} {kw}");

            // Paginate the search to the end of the day window (complete-refetch
            // contract: the daily page is re-rendered from this fetch, so a match
            // beyond one page is silently lost knowledge). `search.messages`
            // paginates by page number (`messages.paging.page` of `.pages`), not a
            // cursor, so it loops here rather than through the cursor-flavored
            // `paginate` helper — termination is still the shared
            // `paging::page_step` rule.
            // Requested page size; the server may return fewer (or zero) per page.
            const PAGE_SIZE: usize = 100;
            let page_size = PAGE_SIZE.to_string();

            let mut matches: Vec<SearchMatch> = Vec::new();
            // The page number doubles as the fetched-page count: it starts at 1
            // and increments only after a fetch, so at each decision point
            // exactly `page` pages have been fetched.
            let mut page = 1u32;

            loop {
                let page_str = page.to_string();
                let data: SearchData = slack_post(
                    &self.http,
                    &self.token,
                    "search.messages",
                    &[
                        ("query", query.as_str()),
                        ("sort", "timestamp"),
                        ("count", page_size.as_str()),
                        ("page", page_str.as_str()),
                    ],
                )
                .await?;

                let (page_matches, paging) = match data.messages {
                    Some(m) => (m.matches.unwrap_or_default(), m.paging),
                    None => (Vec::new(), None),
                };
                matches.extend(page_matches);

                // A missing paging envelope reads as "no further pages" — Slack
                // always includes it on multi-page results, so this never
                // under-fetches; it only ends cleanly on the single-page case.
                let has_next = paging
                    .map(|pg| pg.page.unwrap_or(page) < pg.pages.unwrap_or(page))
                    .unwrap_or(false);

                match crate::paging::page_step(
                    matches.len(),
                    p.max_matches_per_query,
                    has_next,
                    page as usize,
                ) {
                    crate::paging::PageStep::Continue => page += 1,
                    crate::paging::PageStep::Stop { dropped } => {
                        matches.truncate(p.max_matches_per_query);
                        if dropped {
                            tracing::warn!(
                                max = p.max_matches_per_query,
                                "slack-search: match cap hit, some matches may have been dropped; raise max_matches_per_query"
                            );
                        }
                        break;
                    }
                    crate::paging::PageStep::Exhausted => {
                        tracing::warn!(
                            pages = crate::paging::MAX_PAGES,
                            "slack-search: page budget exhausted before the search completed; results may be incomplete"
                        );
                        break;
                    }
                }
            }

            tracing::info!(
                count = matches.len(),
                channel = channel_name,
                "slack-search: matches"
            );

            for m in matches {
                let rendered = slack_to_markdown(&m.text, &users);
                let (title, body) = split_first_line(&rendered);

                let Some(ts) =
                    m.ts.parse::<f64>()
                        .ok()
                        .and_then(|secs| jiff::Timestamp::from_second(secs as i64).ok())
                else {
                    tracing::warn!(
                        ts = m.ts.as_str(),
                        "slack-search: skipping message with unparseable timestamp"
                    );
                    continue;
                };

                let ch = m
                    .channel
                    .as_ref()
                    .and_then(|c| c.name.as_deref())
                    .unwrap_or(channel_name);

                // Keep the raw user id (for the page metadata) before resolving it
                // to a display name.
                let author_id = m.user.clone();
                let author = m
                    .user
                    .as_ref()
                    .and_then(|uid| users.get(uid).cloned())
                    .or(m.user);

                let is_self = ctx
                    .identity
                    .slack_id
                    .as_deref()
                    .filter(|me| !me.trim().is_empty())
                    .is_some_and(|me| author_id.as_deref() == Some(me));

                // Use the API permalink if available, otherwise construct one.
                let url = m.permalink.or_else(|| {
                    m.channel.as_ref().and_then(|c| c.id.as_ref()).map(|cid| {
                        format!(
                            "https://slack.com/archives/{}/p{}",
                            cid,
                            m.ts.replace('.', "")
                        )
                    })
                });

                all_items.push(RawItem {
                    external_id: Some(format!("search:{ch}/{}", m.ts)),
                    title,
                    body,
                    url,
                    author,
                    timestamp: ts,
                    is_self,
                    open_work: None,
                    metadata: serde_json::json!({
                        "channel": ch,
                        "keywords": spec.keywords,
                        "author_id": author_id,
                    }),
                });
            }
        }

        Ok(all_items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty_or_blank_keyword_clause() {
        // No queries at all.
        assert!(validate_params(&serde_json::json!({"queries": []})).is_err());
        // Blank channel.
        assert!(
            validate_params(&serde_json::json!({
                "queries": [{"channel": "  ", "keywords": ["a"]}]
            }))
            .is_err()
        );
        // Empty keyword list → the query would scrape the whole channel.
        assert!(
            validate_params(&serde_json::json!({
                "queries": [{"channel": "#x", "keywords": []}]
            }))
            .is_err()
        );
        // Blank keyword entry → same collapsed clause.
        assert!(
            validate_params(&serde_json::json!({
                "queries": [{"channel": "#x", "keywords": [" "]}]
            }))
            .is_err()
        );
        // A real keyword search is accepted.
        assert!(
            validate_params(&serde_json::json!({
                "queries": [{"channel": "#x", "keywords": ["release"]}]
            }))
            .is_ok()
        );
    }

    #[test]
    fn max_matches_defaults_and_rejects_zero() {
        let base = serde_json::json!({
            "queries": [{"channel": "#x", "keywords": ["a"]}]
        });
        let params: SlackSearchParams = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(params.max_matches_per_query, 200);
        // A zero cap would silently fetch nothing; validation must refuse it up front.
        let mut zero = base.clone();
        zero["max_matches_per_query"] = serde_json::json!(0);
        assert!(validate_params(&zero).is_err());
        let mut one = base;
        one["max_matches_per_query"] = serde_json::json!(1);
        assert!(validate_params(&one).is_ok());
    }
}
