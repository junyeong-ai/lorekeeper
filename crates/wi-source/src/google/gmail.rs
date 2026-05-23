use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use wi_core::event::RawItem;

use super::{GoogleAuth, check_response};
use crate::{ExtractContext, Source, SourceError};

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

pub struct GmailSource {
    http: reqwest::Client,
    auth: Arc<GoogleAuth>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GmailParams {
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    #[serde(default)]
    include_queries: Vec<String>,
    #[serde(default)]
    exclude: Option<ExcludeConfig>,
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    serde_json::from_value::<GmailParams>(params.clone())
        .map(|_| ())
        .map_err(|e| SourceError::InvalidParams(e.to_string()))
}

fn default_lookback() -> u32 {
    24
}

#[derive(Debug, Deserialize)]
struct ExcludeConfig {
    #[serde(default)]
    subjects: Vec<String>,
    #[serde(default)]
    senders: Vec<String>,
}

#[derive(Deserialize)]
struct ListResponse {
    messages: Option<Vec<MessageRef>>,
}

#[derive(Deserialize)]
struct MessageRef {
    id: String,
}

#[derive(Deserialize)]
struct Message {
    id: String,
    snippet: Option<String>,
    payload: Option<Payload>,
    #[serde(rename = "internalDate")]
    internal_date: Option<String>,
    #[serde(rename = "labelIds")]
    label_ids: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Payload {
    headers: Option<Vec<Header>>,
}

#[derive(Deserialize)]
struct Header {
    name: String,
    value: String,
}

impl GmailSource {
    pub fn new(http: reqwest::Client, auth: Arc<GoogleAuth>) -> Self {
        Self { http, auth }
    }

    fn header<'a>(msg: &'a Message, name: &str) -> Option<&'a str> {
        msg.payload
            .as_ref()?
            .headers
            .as_ref()?
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    fn should_exclude(msg: &Message, exclude: &ExcludeConfig) -> bool {
        let subject = Self::header(msg, "Subject")
            .unwrap_or_default()
            .to_lowercase();
        let from = Self::header(msg, "From").unwrap_or_default().to_lowercase();

        exclude
            .subjects
            .iter()
            .any(|pat| subject.contains(&pat.to_lowercase()))
            || exclude
                .senders
                .iter()
                .any(|pat| from.contains(&pat.to_lowercase()))
    }
}

#[async_trait]
impl Source for GmailSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: GmailParams = serde_json::from_value(params.clone())
            .map_err(|e| SourceError::InvalidParams(e.to_string()))?;

        let token = self.auth.access_token().await?;

        // Constrain the query to the target day (Gmail date operators are day-granular,
        // `before:` exclusive) anchored to ctx.target_date so --date backfill fetches the
        // right day instead of "newer_than N hours from now". The OR group is parenthesized
        // so the date bounds apply to every include_query, not just the last term.
        let lookback_days = (i64::from(p.lookback_hours) / 24).max(1);
        let after = ctx
            .target_date
            .checked_sub(jiff::Span::new().days(lookback_days))
            .map_err(|e| SourceError::Parse(e.to_string()))?
            .strftime("%Y/%m/%d");
        let before = ctx
            .target_date
            .tomorrow()
            .map_err(|e| SourceError::Parse(e.to_string()))?
            .strftime("%Y/%m/%d");
        let base = if p.include_queries.is_empty() {
            "is:unread".to_string()
        } else {
            format!("({})", p.include_queries.join(" OR "))
        };
        let query = format!("{base} after:{after} before:{before}");

        let resp = check_response(
            self.http
                .get(format!("{BASE}/messages"))
                .bearer_auth(&token)
                .query(&[("q", &query), ("maxResults", &"50".to_string())])
                .send()
                .await?,
        )
        .await?;

        let list: ListResponse = resp.json().await?;
        let refs = list.messages.unwrap_or_default();

        tracing::info!(count = refs.len(), "gmail: listed messages");

        let mut items = Vec::new();
        for r in &refs {
            let resp = check_response(
                self.http
                    .get(format!("{BASE}/messages/{}", r.id))
                    .bearer_auth(&token)
                    .query(&[
                        ("format", "metadata"),
                        ("metadataHeaders", "From"),
                        ("metadataHeaders", "To"),
                        ("metadataHeaders", "Subject"),
                        ("metadataHeaders", "Date"),
                    ])
                    .send()
                    .await?,
            )
            .await?;

            let msg: Message = resp.json().await?;

            if let Some(ref exc) = p.exclude
                && Self::should_exclude(&msg, exc)
            {
                continue;
            }

            let subject = Self::header(&msg, "Subject").unwrap_or("(no subject)");
            let from = Self::header(&msg, "From").unwrap_or_default();
            let snippet = msg.snippet.as_deref().unwrap_or_default();

            let ts = msg
                .internal_date
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
                .unwrap_or_else(jiff::Timestamp::now);

            items.push(RawItem {
                external_id: Some(msg.id.clone()),
                title: subject.to_string(),
                body: snippet.to_string(),
                url: Some(format!(
                    "https://mail.google.com/mail/u/0/#inbox/{}",
                    msg.id
                )),
                author: Some(from.to_string()),
                timestamp: ts,
                metadata: serde_json::json!({
                    "to": Self::header(&msg, "To"),
                    "labels": msg.label_ids,
                }),
            });
        }

        tracing::info!(
            total = refs.len(),
            kept = items.len(),
            "gmail: extraction complete"
        );
        Ok(items)
    }
}
