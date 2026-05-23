pub mod channel;
pub mod search;

use serde::Deserialize;

use crate::SourceError;

const API: &str = "https://slack.com/api";

#[derive(Deserialize)]
struct SlackResponse<T> {
    ok: bool,
    error: Option<String>,
    #[serde(flatten)]
    data: T,
}

async fn slack_post<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    token: &str,
    method: &str,
    body: &serde_json::Value,
) -> Result<T, SourceError> {
    let resp = http
        .post(format!("{API}/{method}"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(SourceError::Api {
            status,
            message: text,
        });
    }

    let wrapper: SlackResponse<T> = resp
        .json()
        .await
        .map_err(|e| SourceError::Parse(e.to_string()))?;

    if !wrapper.ok {
        return Err(SourceError::Api {
            status: 200,
            message: wrapper
                .error
                .unwrap_or_else(|| "unknown Slack error".into()),
        });
    }

    Ok(wrapper.data)
}

#[derive(Debug, Deserialize)]
struct ChannelInfo {
    id: String,
    #[allow(dead_code)]
    name: String,
}

async fn resolve_channel_id(
    http: &reqwest::Client,
    token: &str,
    channel_name: &str,
) -> Result<String, SourceError> {
    let name = channel_name.strip_prefix('#').unwrap_or(channel_name);

    #[derive(Deserialize)]
    struct Channels {
        channels: Vec<ChannelInfo>,
    }

    let data: Channels = slack_post(
        http,
        token,
        "conversations.list",
        &serde_json::json!({
            "types": "public_channel,private_channel",
            "limit": 1000,
            "exclude_archived": true,
        }),
    )
    .await?;

    data.channels
        .into_iter()
        .find(|c| c.name == name)
        .map(|c| c.id)
        .ok_or_else(|| SourceError::Parse(format!("channel not found: {channel_name}")))
}
