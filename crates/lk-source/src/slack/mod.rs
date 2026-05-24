pub mod channel;
pub mod search;

use std::collections::HashMap;

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

/// POST to a Slack Web API method with `application/x-www-form-urlencoded` params. Every
/// method accepts form encoding, whereas JSON bodies are rejected by read methods such as
/// `search.messages` — so form is the one encoding that works across all of them.
async fn slack_post<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    token: &str,
    method: &str,
    params: &[(&str, &str)],
) -> Result<T, SourceError> {
    let resp = http
        .post(format!("{API}/{method}"))
        .bearer_auth(token)
        .form(params)
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
    name: String,
}

/// A bare Slack channel id (`C…`/`G…`/`D…` + uppercase alphanumerics) — config may give
/// either an id or a `#name`. Ids are used directly; only names need a lookup.
fn looks_like_channel_id(s: &str) -> bool {
    matches!(s.chars().next(), Some('C' | 'G' | 'D'))
        && s.len() >= 9
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Resolve all workspace user IDs to display names via `users.list`. Returns an empty map
/// on any error so callers degrade gracefully to raw IDs.
pub(crate) async fn resolve_users(http: &reqwest::Client, token: &str) -> HashMap<String, String> {
    #[derive(Deserialize)]
    struct Profile {
        display_name: Option<String>,
        real_name: Option<String>,
    }

    #[derive(Deserialize)]
    struct Member {
        id: String,
        name: Option<String>,
        profile: Option<Profile>,
    }

    #[derive(Deserialize)]
    struct ResponseMetadata {
        #[serde(default)]
        next_cursor: String,
    }

    #[derive(Deserialize)]
    struct MembersPage {
        #[serde(default)]
        members: Vec<Member>,
        #[serde(default)]
        response_metadata: Option<ResponseMetadata>,
    }

    fn display_name(m: &Member) -> String {
        m.profile
            .as_ref()
            .and_then(|p| {
                p.display_name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .or(p.real_name.as_deref().filter(|s| !s.is_empty()))
            })
            .or(m.name.as_deref())
            .unwrap_or_default()
            .to_string()
    }

    // Paginate through the full member list (large workspaces exceed 1000).
    const MAX_PAGES: usize = 50; // 50 × 200 = 10,000 users
    let mut map = HashMap::new();
    let mut cursor = String::new();
    for _ in 0..MAX_PAGES {
        let mut params: Vec<(&str, &str)> = vec![("limit", "200")];
        if !cursor.is_empty() {
            params.push(("cursor", cursor.as_str()));
        }
        let page: Result<MembersPage, _> = slack_post(http, token, "users.list", &params).await;
        match page {
            Ok(data) => {
                if data.members.is_empty() {
                    break;
                }
                for m in &data.members {
                    map.insert(m.id.clone(), display_name(m));
                }
                cursor = data
                    .response_metadata
                    .map(|r| r.next_cursor)
                    .unwrap_or_default();
                if cursor.is_empty() {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to resolve Slack users; falling back to raw IDs");
                break;
            }
        }
    }
    map
}

async fn resolve_channel_id(
    http: &reqwest::Client,
    token: &str,
    channel_ref: &str,
) -> Result<String, SourceError> {
    if looks_like_channel_id(channel_ref) {
        return Ok(channel_ref.to_string());
    }
    let name = channel_ref.strip_prefix('#').unwrap_or(channel_ref);

    #[derive(Deserialize)]
    struct ResponseMetadata {
        #[serde(default)]
        next_cursor: String,
    }

    #[derive(Deserialize)]
    struct ChannelsPage {
        #[serde(default)]
        channels: Vec<ChannelInfo>,
        #[serde(default)]
        response_metadata: Option<ResponseMetadata>,
    }

    // Paginate through the full channel list (large workspaces exceed 1000).
    const MAX_PAGES: usize = 25; // 25 × 200 = 5,000 channels
    let mut cursor = String::new();
    for _ in 0..MAX_PAGES {
        let mut params: Vec<(&str, &str)> = vec![
            ("types", "public_channel,private_channel"),
            ("limit", "200"),
            ("exclude_archived", "true"),
        ];
        if !cursor.is_empty() {
            params.push(("cursor", cursor.as_str()));
        }
        let page: ChannelsPage =
            slack_post(http, token, "conversations.list", &params).await?;

        if let Some(ch) = page.channels.into_iter().find(|c| c.name == name) {
            return Ok(ch.id);
        }

        cursor = page
            .response_metadata
            .map(|r| r.next_cursor)
            .unwrap_or_default();
        if cursor.is_empty() {
            break;
        }
    }

    Err(SourceError::Parse(format!(
        "channel not found: {channel_ref}"
    )))
}
