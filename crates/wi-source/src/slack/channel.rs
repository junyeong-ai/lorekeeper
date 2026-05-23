use async_trait::async_trait;
use serde::Deserialize;

use wi_core::event::RawItem;

use super::{resolve_channel_id, slack_post};
use crate::{ExtractContext, Source, SourceError};

pub struct SlackChannelSource {
    http: reqwest::Client,
    token: String,
}

#[derive(Debug, Deserialize)]
struct ChannelParams {
    channel: String,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    serde_json::from_value::<ChannelParams>(params.clone())
        .map(|_| ())
        .map_err(|e| SourceError::InvalidParams(e.to_string()))
}

fn default_lookback() -> u32 {
    24
}

#[derive(Deserialize)]
struct HistoryData {
    messages: Option<Vec<SlackMessage>>,
}

#[derive(Debug, Deserialize)]
struct SlackMessage {
    ts: String,
    user: Option<String>,
    text: Option<String>,
}

impl SlackChannelSource {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self { http, token }
    }
}

#[async_trait]
impl Source for SlackChannelSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        _ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: ChannelParams = serde_json::from_value(params.clone())
            .map_err(|e| SourceError::InvalidParams(e.to_string()))?;

        let channel_id = resolve_channel_id(&self.http, &self.token, &p.channel).await?;
        let channel_name = p.channel.strip_prefix('#').unwrap_or(&p.channel);

        let oldest = jiff::Timestamp::now()
            .checked_sub(jiff::SignedDuration::from_hours(p.lookback_hours.into()))
            .unwrap_or_else(|_| jiff::Timestamp::now());
        let oldest_ts = format!("{}.000000", oldest.as_second());

        let data: HistoryData = slack_post(
            &self.http,
            &self.token,
            "conversations.history",
            &serde_json::json!({
                "channel": channel_id,
                "oldest": oldest_ts,
                "limit": 100,
            }),
        )
        .await?;

        let messages = data.messages.unwrap_or_default();
        tracing::info!(
            count = messages.len(),
            channel = channel_name,
            "slack-channel: messages read"
        );

        let items = messages
            .into_iter()
            .filter_map(|msg| {
                let text = msg.text.unwrap_or_default();
                if text.is_empty() {
                    return None;
                }

                let title = text.lines().next().unwrap_or_default().to_string();
                let secs: f64 = msg.ts.parse().unwrap_or(0.0);
                let ts = jiff::Timestamp::from_second(secs as i64)
                    .unwrap_or_else(|_| jiff::Timestamp::now());

                Some(RawItem {
                    external_id: Some(format!("{channel_id}/{}", msg.ts)),
                    title,
                    body: text,
                    url: None,
                    author: msg.user,
                    timestamp: ts,
                    metadata: serde_json::json!({ "channel": channel_name }),
                })
            })
            .collect();

        Ok(items)
    }
}
