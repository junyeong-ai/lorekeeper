use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use super::{resolve_channel_id, slack_post};
use crate::markdown::slack_to_markdown;
use crate::{ExtractContext, Source, SourceError};

pub struct SlackChannelSource {
    http: reqwest::Client,
    token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelParams {
    /// A single channel (`#name` or id). Kept alongside `channels` so existing configs
    /// and the common single-channel case stay terse.
    #[serde(default)]
    channel: Option<String>,
    /// Multiple channels read in one source.
    #[serde(default)]
    channels: Vec<String>,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    /// User ids whose messages/mentions are the focus. Empty = read the whole channel
    /// (team-observation mode); non-empty = keep only threads where one of these users
    /// authored or was mentioned (personal/teammate activity mode).
    #[serde(default)]
    watch_users: Vec<String>,
    /// Pull each matching message's full thread (`conversations.replies`) for context —
    /// most channel discussion happens in threads, which `conversations.history` omits.
    #[serde(default)]
    include_threads: bool,
    /// Drop bot/integration messages (CI, deploy bots, app notifications). On by default
    /// since they're noise for work analysis; set false to keep them.
    #[serde(default = "default_exclude_bots")]
    exclude_bots: bool,
}

impl ChannelParams {
    /// All channel references (single + list), in config order.
    fn channel_refs(&self) -> Vec<&str> {
        self.channel
            .as_deref()
            .into_iter()
            .chain(self.channels.iter().map(String::as_str))
            .collect()
    }
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    let p: ChannelParams = serde_json::from_value(params.clone())
        .map_err(|e| SourceError::InvalidParams(e.to_string()))?;
    if p.channel_refs().is_empty() {
        return Err(SourceError::InvalidParams(
            "slack-channel requires `channel` or `channels`".into(),
        ));
    }
    Ok(())
}

fn default_lookback() -> u32 {
    24
}

fn default_exclude_bots() -> bool {
    true
}

#[derive(Deserialize)]
struct HistoryData {
    messages: Option<Vec<SlackMessage>>,
}

#[derive(Debug, Clone, Deserialize)]
struct SlackMessage {
    ts: String,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    reply_count: Option<u32>,
    user: Option<String>,
    text: Option<String>,
    /// Present on bot/integration messages; absent on human posts.
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

impl SlackMessage {
    /// A bot/integration post (CI, deploy, app notification) rather than a human message.
    fn is_bot(&self) -> bool {
        self.bot_id.is_some() || self.subtype.as_deref() == Some("bot_message")
    }

    /// True if this message was authored by, or mentions, one of `watch_users`.
    fn matches(&self, watch_users: &[String]) -> bool {
        if let Some(u) = &self.user
            && watch_users.iter().any(|w| w == u)
        {
            return true;
        }
        if let Some(t) = &self.text
            && watch_users.iter().any(|w| t.contains(&format!("<@{w}>")))
        {
            return true;
        }
        false
    }
}

impl SlackChannelSource {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self { http, token }
    }

    /// Fetch the full thread for a root message (`conversations.replies` returns the root
    /// plus every reply).
    async fn fetch_thread(
        &self,
        channel_id: &str,
        root_ts: &str,
    ) -> Result<Vec<SlackMessage>, SourceError> {
        let data: HistoryData = slack_post(
            &self.http,
            &self.token,
            "conversations.replies",
            &[("channel", channel_id), ("ts", root_ts), ("limit", "100")],
        )
        .await?;
        Ok(data.messages.unwrap_or_default())
    }
}

#[async_trait]
impl Source for SlackChannelSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: ChannelParams = serde_json::from_value(params.clone())
            .map_err(|e| SourceError::InvalidParams(e.to_string()))?;

        let (oldest, latest) = ctx.day_window(p.lookback_hours, 0)?;
        let oldest_ts = format!("{}.000000", oldest.as_second());
        let latest_ts = format!("{}.000000", latest.as_second());

        let mut items = Vec::new();
        for ch_ref in p.channel_refs() {
            let channel_id = resolve_channel_id(&self.http, &self.token, ch_ref).await?;
            let channel_name = ch_ref.strip_prefix('#').unwrap_or(ch_ref);

            let data: HistoryData = slack_post(
                &self.http,
                &self.token,
                "conversations.history",
                &[
                    ("channel", channel_id.as_str()),
                    ("oldest", oldest_ts.as_str()),
                    ("latest", latest_ts.as_str()),
                    // Include a message whose ts lands exactly on a boundary; the pipeline's
                    // date filter still trims anything outside the target day.
                    ("inclusive", "true"),
                    ("limit", "100"),
                ],
            )
            .await?;

            let messages = data.messages.unwrap_or_default();

            for root in messages {
                let text = root.text.clone().unwrap_or_default();
                if text.is_empty() || (p.exclude_bots && root.is_bot()) {
                    continue;
                }

                // Pull the thread when asked and the message actually has replies.
                let has_replies = root.reply_count.unwrap_or(0) > 0
                    || root.thread_ts.as_deref() == Some(root.ts.as_str());
                let mut thread = if p.include_threads && has_replies {
                    self.fetch_thread(&channel_id, &root.ts).await?
                } else {
                    vec![root.clone()]
                };

                // Strip bot replies from the thread too, so context stays human-only.
                if p.exclude_bots {
                    thread.retain(|m| !m.is_bot());
                }
                if thread.is_empty() {
                    continue;
                }

                // In watch mode keep the thread only if the focus users are involved
                // anywhere in it (root or any reply); otherwise it's noise.
                if !p.watch_users.is_empty() && !thread.iter().any(|m| m.matches(&p.watch_users)) {
                    continue;
                }

                let body = render_thread(&thread, ctx.locale.strings().thread_replies);
                // Title from the converted body so it shares the Markdown normalization
                // (mentions/entities resolved) rather than showing raw Slack tokens.
                let title = body.lines().next().unwrap_or_default().to_string();
                let secs: f64 = root.ts.parse().unwrap_or(0.0);
                let ts = jiff::Timestamp::from_second(secs as i64)
                    .unwrap_or_else(|_| jiff::Timestamp::now());

                items.push(RawItem {
                    external_id: Some(format!("{channel_id}/{}", root.ts)),
                    title,
                    body,
                    url: None,
                    author: root.user.clone(),
                    timestamp: ts,
                    metadata: serde_json::json!({
                        "channel": channel_name,
                        "reply_count": root.reply_count,
                    }),
                });
            }

            tracing::info!(
                channel = channel_name,
                kept = items.len(),
                "slack-channel: messages"
            );
        }

        Ok(items)
    }
}

/// Render a message (and its thread, if present) as Markdown. The root becomes the body;
/// replies are listed under a marker so the LLM sees the full discussion in order.
fn render_thread(thread: &[SlackMessage], replies_label: &str) -> String {
    let render = |m: &SlackMessage| slack_to_markdown(m.text.as_deref().unwrap_or(""));
    let Some((root, replies)) = thread.split_first() else {
        return String::new();
    };
    let root_md = render(root);
    if replies.is_empty() {
        return root_md;
    }
    let mut out = format!("{root_md}\n\n--- {replies_label} {} ---", replies.len());
    for r in replies {
        let user = r.user.as_deref().unwrap_or("unknown");
        out.push_str(&format!("\n@{user}: {}", render(r)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(ts: &str, user: &str, text: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.into(),
            thread_ts: None,
            reply_count: None,
            user: Some(user.into()),
            text: Some(text.into()),
            bot_id: None,
            subtype: None,
        }
    }

    #[test]
    fn channel_refs_merge_single_and_list() {
        let p = ChannelParams {
            channel: Some("#a".into()),
            channels: vec!["C1".into(), "C2".into()],
            lookback_hours: 24,
            watch_users: vec![],
            include_threads: false,
            exclude_bots: true,
        };
        assert_eq!(p.channel_refs(), vec!["#a", "C1", "C2"]);
    }

    #[test]
    fn is_bot_detects_bot_id_and_subtype() {
        let mut m = msg("1", "U1", "human");
        assert!(!m.is_bot());
        m.bot_id = Some("B123".into());
        assert!(m.is_bot());
        let mut m2 = msg("2", "U2", "via app");
        m2.subtype = Some("bot_message".into());
        assert!(m2.is_bot());
    }

    #[test]
    fn exclude_bots_defaults_on() {
        let p: ChannelParams = serde_json::from_value(serde_json::json!({
            "channel": "#x"
        }))
        .unwrap();
        assert!(p.exclude_bots);
    }

    #[test]
    fn matches_author_or_mention() {
        let watch = vec!["U1".to_string()];
        assert!(msg("1", "U1", "hi").matches(&watch)); // author
        assert!(msg("1", "U9", "cc <@U1>").matches(&watch)); // mention
        assert!(!msg("1", "U9", "unrelated").matches(&watch));
    }

    #[test]
    fn validate_requires_a_channel() {
        assert!(validate_params(&serde_json::json!({ "lookback_hours": 24 })).is_err());
        assert!(validate_params(&serde_json::json!({ "channel": "#x" })).is_ok());
        assert!(validate_params(&serde_json::json!({ "channels": ["C1"] })).is_ok());
    }

    #[test]
    fn render_thread_lists_replies() {
        let thread = vec![
            msg("1", "U1", "root question"),
            msg("2", "U2", "first answer"),
            msg("3", "U1", "thanks <@U2>"),
        ];
        let body = render_thread(&thread, "쓰레드 답글");
        assert!(body.starts_with("root question"));
        assert!(body.contains("--- 쓰레드 답글 2 ---"));
        assert!(body.contains("@U2: first answer"));
        assert!(body.contains("@U1: thanks @U2")); // slack mention normalized
    }

    #[test]
    fn render_single_message_is_just_body() {
        let thread = vec![msg("1", "U1", "solo <@U2> ping")];
        assert_eq!(render_thread(&thread, "쓰레드 답글"), "solo @U2 ping");
    }
}
