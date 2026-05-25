use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

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
#[serde(deny_unknown_fields)]
struct ExcludeConfig {
    #[serde(default)]
    subjects: Vec<String>,
    #[serde(default)]
    senders: Vec<String>,
}

#[derive(Deserialize)]
struct ListResponse {
    messages: Option<Vec<MessageRef>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
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
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    headers: Option<Vec<Header>>,
    body: Option<PartBody>,
    parts: Option<Vec<MimePart>>,
}

#[derive(Deserialize)]
struct MimePart {
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    body: Option<PartBody>,
    parts: Option<Vec<MimePart>>,
}

#[derive(Deserialize)]
struct PartBody {
    data: Option<String>,
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

        // Constrain the query to the target day's window anchored to ctx.target_date so
        // --date backfill fetches the right day instead of "newer_than N hours from now".
        // Gmail's after:/before: accept epoch seconds, which (unlike YYYY/MM/DD) are
        // timezone-exact — the bounds come from day_window in the vault's timezone. The OR
        // group is parenthesized so the bounds apply to every include_query, not just the
        // last term.
        let (min, max) = ctx.day_window(p.lookback_hours, 0)?;
        let base = if p.include_queries.is_empty() {
            "is:unread".to_string()
        } else {
            format!("({})", p.include_queries.join(" OR "))
        };
        let query = format!(
            "{base} after:{} before:{}",
            min.as_second(),
            max.as_second()
        );

        const PAGE_SIZE: usize = 50;
        const MAX_MESSAGES: usize = 200;

        let mut refs: Vec<MessageRef> = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut req = self
                .http
                .get(format!("{BASE}/messages"))
                .bearer_auth(&token)
                .query(&[
                    ("q", query.as_str()),
                    ("maxResults", &PAGE_SIZE.to_string()),
                ]);
            if let Some(ref pt) = page_token {
                req = req.query(&[("pageToken", pt.as_str())]);
            }

            let resp = check_response(req.send().await?).await?;
            let list: ListResponse = resp.json().await?;

            let page = list.messages.unwrap_or_default();
            let page_empty = page.is_empty();
            refs.extend(page);

            if page_empty || refs.len() >= MAX_MESSAGES {
                refs.truncate(MAX_MESSAGES);
                break;
            }

            match list.next_page_token {
                Some(pt) => page_token = Some(pt),
                None => break,
            }
        }

        tracing::info!(count = refs.len(), "gmail: listed messages");

        let mut items = Vec::new();
        for r in &refs {
            let fetch = async {
                let resp = check_response(
                    self.http
                        .get(format!("{BASE}/messages/{}", r.id))
                        .bearer_auth(&token)
                        .query(&[("format", "full")])
                        .send()
                        .await?,
                )
                .await?;
                resp.json::<Message>().await.map_err(SourceError::Http)
            };

            let msg = match fetch.await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        message_id = %r.id,
                        error = %e,
                        "gmail: skipping message (fetch failed)"
                    );
                    continue;
                }
            };

            if let Some(ref exc) = p.exclude
                && Self::should_exclude(&msg, exc)
            {
                continue;
            }

            let subject = Self::header(&msg, "Subject").unwrap_or("(no subject)");
            let from = Self::header(&msg, "From").unwrap_or_default();
            let snippet = msg.snippet.as_deref().unwrap_or_default();

            let body = {
                let extracted = msg.payload.as_ref().map(extract_body).unwrap_or_default();
                if extracted.is_empty() {
                    snippet.to_string()
                } else {
                    extracted
                }
            };

            let Some(ts) = msg
                .internal_date
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
            else {
                tracing::warn!(message_id = %msg.id, "gmail: skipping message with unparseable timestamp");
                continue;
            };

            items.push(RawItem {
                external_id: Some(msg.id.clone()),
                title: subject.to_string(),
                body,
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

fn extract_body(payload: &Payload) -> String {
    let mut plain = None;
    let mut html = None;
    collect_parts(
        payload.mime_type.as_deref(),
        payload.body.as_ref(),
        payload.parts.as_deref(),
        &mut plain,
        &mut html,
    );
    if let Some(text) = plain {
        return text;
    }
    if let Some(h) = html {
        return crate::markdown::html_to_markdown(&h);
    }
    String::new()
}

fn collect_parts(
    mime: Option<&str>,
    body: Option<&PartBody>,
    parts: Option<&[MimePart]>,
    plain: &mut Option<String>,
    html: &mut Option<String>,
) {
    if let Some(parts) = parts {
        for part in parts {
            collect_parts(
                part.mime_type.as_deref(),
                part.body.as_ref(),
                part.parts.as_deref(),
                plain,
                html,
            );
        }
    } else if let Some(data) = body.and_then(|b| b.data.as_deref()) {
        let decoded = decode_base64url(data);
        match mime {
            Some("text/plain") if plain.is_none() => *plain = Some(decoded),
            Some("text/html") if html.is_none() => *html = Some(decoded),
            _ => {}
        }
    }
}

fn decode_base64url(data: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(data)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}
