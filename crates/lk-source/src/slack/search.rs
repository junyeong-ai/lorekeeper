use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use super::{resolve_users, slack_post};
use crate::markdown::slack_to_markdown;
use crate::{ExtractContext, Source, SourceError};

pub struct SlackSearchSource {
    http: reqwest::Client,
    token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    queries: Vec<QuerySpec>,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
}

fn default_lookback() -> u32 {
    24
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuerySpec {
    channel: String,
    keywords: Vec<String>,
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    serde_json::from_value::<SearchParams>(params.clone())
        .map(|_| ())
        .map_err(|e| SourceError::InvalidParams(e.to_string()))
}

#[derive(Deserialize)]
struct SearchData {
    messages: Option<SearchMessages>,
}

#[derive(Deserialize)]
struct SearchMessages {
    matches: Option<Vec<SearchMatch>>,
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
        let p: SearchParams = serde_json::from_value(params.clone())
            .map_err(|e| SourceError::InvalidParams(e.to_string()))?;

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

            let data: SearchData = slack_post(
                &self.http,
                &self.token,
                "search.messages",
                &[
                    ("query", query.as_str()),
                    ("sort", "timestamp"),
                    ("count", "50"),
                ],
            )
            .await?;

            let matches = data.messages.and_then(|m| m.matches).unwrap_or_default();

            tracing::info!(
                count = matches.len(),
                channel = channel_name,
                "slack-search: matches"
            );

            for m in matches {
                let body = slack_to_markdown(&m.text, &users);
                let title = body.lines().next().unwrap_or_default().to_string();
                // Strip the first line from body so it doesn't duplicate the heading.
                let body = body
                    .split_once('\n')
                    .map(|x| x.1)
                    .unwrap_or("")
                    .trim_start()
                    .to_string();

                let secs: f64 = m.ts.parse().unwrap_or(0.0);
                let ts = jiff::Timestamp::from_second(secs as i64)
                    .unwrap_or_else(|_| jiff::Timestamp::now());

                let ch = m
                    .channel
                    .as_ref()
                    .and_then(|c| c.name.as_deref())
                    .unwrap_or(channel_name);

                // Preserve raw user id for identity matching (flag_personal
                // inspects metadata), then resolve to display name for the page.
                let author_id = m.user.clone();
                let author = m
                    .user
                    .as_ref()
                    .and_then(|uid| users.get(uid).cloned())
                    .or(m.user);

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
