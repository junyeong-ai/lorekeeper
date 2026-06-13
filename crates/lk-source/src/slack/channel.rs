use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use super::{ResponseMetadata, paginate, resolve_channel_id, resolve_users, split_first_line};
use crate::markdown::slack_to_markdown;
use crate::{ExtractContext, Source, SourceError};

pub struct SlackChannelSource {
    http: reqwest::Client,
    token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlackChannelParams {
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
    /// Safety cap on messages fetched per channel in one run (newest-first). Raise it for
    /// a high-volume channel where the window holds more than this; a cap hit is logged so
    /// truncation is never silent.
    #[serde(default = "default_max_messages_per_channel")]
    max_messages_per_channel: usize,
    /// Safety cap on messages fetched per thread. `conversations.replies` returns the
    /// root message plus its replies, and both count toward this cap — it bounds the
    /// fetch, not a precise reply count.
    #[serde(default = "default_max_thread_messages")]
    max_thread_messages: usize,
}

impl SlackChannelParams {
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
    crate::parse_validated::<SlackChannelParams>(params).map(|_| ())
}

impl crate::ValidatedParams for SlackChannelParams {
    fn validate(&self) -> Result<(), SourceError> {
        if self.channel_refs().is_empty() {
            return Err(SourceError::InvalidParams(
                "slack-channel requires `channel` or `channels`".into(),
            ));
        }
        if self.max_messages_per_channel == 0 || self.max_thread_messages == 0 {
            return Err(SourceError::InvalidParams(
                "slack-channel `max_messages_per_channel` and `max_thread_messages` must be > 0"
                    .into(),
            ));
        }
        Ok(())
    }
}

fn default_lookback() -> u32 {
    24
}

fn default_exclude_bots() -> bool {
    true
}

fn default_max_messages_per_channel() -> usize {
    500
}

fn default_max_thread_messages() -> usize {
    200
}

#[derive(Deserialize)]
struct HistoryData {
    messages: Option<Vec<SlackMessage>>,
    #[serde(default)]
    response_metadata: Option<ResponseMetadata>,
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
    /// plus every reply). Paginates with `next_cursor`, capped at `max_messages` to prevent
    /// runaway on huge threads.
    async fn fetch_thread(
        &self,
        channel_id: &str,
        root_ts: &str,
        max_messages: usize,
    ) -> Result<Vec<SlackMessage>, SourceError> {
        paginate::<HistoryData, SlackMessage, _>(
            &self.http,
            &self.token,
            "conversations.replies",
            &[("channel", channel_id), ("ts", root_ts), ("limit", "100")],
            max_messages,
            "max_thread_messages",
            |data| {
                (
                    data.messages.unwrap_or_default(),
                    ResponseMetadata::cursor(data.response_metadata),
                )
            },
        )
        .await
    }
}

#[async_trait]
impl Source for SlackChannelSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: SlackChannelParams = crate::parse_validated(params)?;

        let users = resolve_users(&self.http, &self.token).await;

        let (oldest, latest) = ctx.day_window(p.lookback_hours, 0)?;
        let oldest_ts = format!("{}.000000", oldest.as_second());
        let latest_ts = format!("{}.000000", latest.as_second());

        let mut items = Vec::new();
        for ch_ref in p.channel_refs() {
            let channel_id = resolve_channel_id(&self.http, &self.token, ch_ref).await?;
            let channel_name = ch_ref.strip_prefix('#').unwrap_or(ch_ref);

            // Paginate through conversations.history, capped per the source config.
            let messages = paginate::<HistoryData, SlackMessage, _>(
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
                p.max_messages_per_channel,
                "max_messages_per_channel",
                |data| {
                    (
                        data.messages.unwrap_or_default(),
                        ResponseMetadata::cursor(data.response_metadata),
                    )
                },
            )
            .await?;

            for root in messages {
                let text = root.text.clone().unwrap_or_default();
                if text.is_empty() || (p.exclude_bots && root.is_bot()) {
                    continue;
                }

                // Pull the thread when asked and the message actually has replies.
                let has_replies = root.reply_count.unwrap_or(0) > 0
                    || root.thread_ts.as_deref() == Some(root.ts.as_str());
                let mut thread = if p.include_threads && has_replies {
                    match self
                        .fetch_thread(&channel_id, &root.ts, p.max_thread_messages)
                        .await
                    {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(
                                channel = channel_name,
                                ts = root.ts.as_str(),
                                error = %e,
                                "slack-channel: thread fetch failed, using root message only"
                            );
                            vec![root.clone()]
                        }
                    }
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

                let rendered = render_thread(&thread, ctx.locale.strings().thread_replies, &users);
                // Title from the converted body so it shares the Markdown normalization
                // (mentions/entities resolved) rather than showing raw Slack tokens.
                let (title, body) = split_first_line(&rendered);

                let Some(ts) = root
                    .ts
                    .parse::<f64>()
                    .ok()
                    .and_then(|secs| jiff::Timestamp::from_second(secs as i64).ok())
                else {
                    tracing::warn!(
                        channel = channel_name,
                        ts = root.ts.as_str(),
                        "slack-channel: skipping message with unparseable timestamp"
                    );
                    continue;
                };

                // Standard Slack permalink: /archives/{channel}/p{ts_no_dot}
                let permalink = format!(
                    "https://slack.com/archives/{}/p{}",
                    channel_id,
                    root.ts.replace('.', "")
                );

                // Resolve author user id to display name.
                let author = root
                    .user
                    .as_ref()
                    .and_then(|uid| users.get(uid).cloned())
                    .or(root.user.clone());

                let is_self = ctx
                    .identity
                    .slack_id
                    .as_deref()
                    .filter(|me| !me.trim().is_empty())
                    .is_some_and(|me| root.user.as_deref() == Some(me));

                items.push(RawItem {
                    external_id: Some(format!("{channel_id}/{}", root.ts)),
                    title,
                    body,
                    url: Some(permalink),
                    author,
                    timestamp: ts,
                    is_self,
                    metadata: serde_json::json!({
                        "channel": channel_name,
                        "reply_count": root.reply_count,
                        "author_id": root.user,
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
fn render_thread(
    thread: &[SlackMessage],
    replies_label: &str,
    users: &HashMap<String, String>,
) -> String {
    let render = |m: &SlackMessage| slack_to_markdown(m.text.as_deref().unwrap_or(""), users);
    let Some((root, replies)) = thread.split_first() else {
        return String::new();
    };
    let root_md = render(root);
    if replies.is_empty() {
        return root_md;
    }
    let mut out = format!("{root_md}\n\n--- {replies_label} {} ---", replies.len());
    for r in replies {
        let raw_uid = r.user.as_deref().unwrap_or("unknown");
        let display = users.get(raw_uid).map(String::as_str).unwrap_or(raw_uid);
        out.push_str(&format!("\n@{display}: {}", render(r)));
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

    fn no_users() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn channel_refs_merge_single_and_list() {
        let p = SlackChannelParams {
            channel: Some("#a".into()),
            channels: vec!["C1".into(), "C2".into()],
            lookback_hours: 24,
            watch_users: vec![],
            include_threads: false,
            exclude_bots: true,
            max_messages_per_channel: 500,
            max_thread_messages: 200,
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
        let p: SlackChannelParams = serde_json::from_value(serde_json::json!({
            "channel": "#x"
        }))
        .unwrap();
        assert!(p.exclude_bots);
    }

    #[test]
    fn caps_default_and_reject_zero() {
        let p: SlackChannelParams =
            serde_json::from_value(serde_json::json!({ "channel": "#x" })).unwrap();
        assert_eq!(p.max_messages_per_channel, 500);
        assert_eq!(p.max_thread_messages, 200);
        assert!(
            validate_params(&serde_json::json!({ "channel": "#x", "max_messages_per_channel": 0 }))
                .is_err()
        );
        assert!(
            validate_params(&serde_json::json!({ "channel": "#x", "max_thread_messages": 0 }))
                .is_err()
        );
        assert!(
            validate_params(
                &serde_json::json!({ "channel": "#x", "max_messages_per_channel": 2000 })
            )
            .is_ok()
        );
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
        let mut users = HashMap::new();
        users.insert("U1".to_string(), "Alice".to_string());
        users.insert("U2".to_string(), "Bob".to_string());
        let thread = vec![
            msg("1", "U1", "root question"),
            msg("2", "U2", "first answer"),
            msg("3", "U1", "thanks <@U2>"),
        ];
        let body = render_thread(&thread, "쓰레드 답글", &users);
        assert!(body.starts_with("root question"));
        assert!(body.contains("--- 쓰레드 답글 2 ---"));
        assert!(body.contains("@Bob: first answer"));
        assert!(body.contains("@Alice: thanks @Bob")); // slack mention + user resolved
    }

    #[test]
    fn render_single_message_is_just_body() {
        let thread = vec![msg("1", "U1", "solo <@U2> ping")];
        assert_eq!(
            render_thread(&thread, "쓰레드 답글", &no_users()),
            "solo @U2 ping"
        );
    }
}
