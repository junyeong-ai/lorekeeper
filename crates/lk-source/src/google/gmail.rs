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
    exclude: Option<ExcludeParams>,
    /// Cap on messages fetched per day window. A busy day or a `--date` backfill can
    /// exceed it; the newest `max_messages` are kept and a truncation warning is logged
    /// so the operator can raise it — the same observable-cap contract as slack-channel.
    #[serde(default = "default_max_messages")]
    max_messages: usize,
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    let p: GmailParams = crate::parse_params(params)?;
    if p.max_messages == 0 {
        return Err(SourceError::InvalidParams(
            "gmail `max_messages` must be > 0".into(),
        ));
    }
    Ok(())
}

fn default_lookback() -> u32 {
    24
}

fn default_max_messages() -> usize {
    200
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExcludeParams {
    #[serde(default)]
    subjects: Vec<String>,
    #[serde(default)]
    senders: Vec<String>,
    #[serde(default = "default_true")]
    calendar_invites: bool,
}

fn default_true() -> bool {
    true
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

    fn should_exclude(msg: &Message, exclude: &ExcludeParams) -> bool {
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
        let p: GmailParams = crate::parse_params(params)?;

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

        // Requested page size; the server may return fewer (or zero) per page.
        // Termination is `paging::page_step` — the rule all listing adapters share.
        const PAGE_SIZE: usize = 50;

        let mut refs: Vec<MessageRef> = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages_fetched = 0usize;

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

            refs.extend(list.messages.unwrap_or_default());
            pages_fetched += 1;

            match crate::paging::page_step(
                refs.len(),
                p.max_messages,
                list.next_page_token.is_some(),
                pages_fetched,
            ) {
                crate::paging::PageStep::Continue => page_token = list.next_page_token,
                crate::paging::PageStep::Stop { dropped } => {
                    refs.truncate(p.max_messages);
                    if dropped {
                        tracing::warn!(
                            max = p.max_messages,
                            "gmail: message cap hit, some messages may have been dropped; raise max_messages"
                        );
                    }
                    break;
                }
                crate::paging::PageStep::Exhausted => {
                    tracing::warn!(
                        pages = crate::paging::MAX_PAGES,
                        "gmail: page budget exhausted before the listing completed; results may be incomplete"
                    );
                    break;
                }
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

            if let Some(item) = map_message(
                msg,
                ctx.identity.email.trim(),
                p.exclude.as_ref(),
                ctx.locale.strings(),
            ) {
                items.push(item);
            }
        }

        tracing::info!(
            total = refs.len(),
            kept = items.len(),
            "gmail: extraction complete"
        );
        Ok(items)
    }
}

/// Map one fetched Gmail message to a `RawItem`, or `None` if it is filtered
/// (matched an `exclude` rule, is a calendar invite) or has no parseable timestamp.
/// Pure — no I/O — so the exclude logic, snippet body-fallback, ownership match
/// (`From` vs `identity_email`), and metadata projection are unit-testable against
/// fixtures. `identity_email` must already be trimmed.
fn map_message(
    msg: Message,
    identity_email: &str,
    exclude: Option<&ExcludeParams>,
    strings: &lk_core::i18n::Strings,
) -> Option<RawItem> {
    if let Some(exc) = exclude
        && GmailSource::should_exclude(&msg, exc)
    {
        return None;
    }
    if exclude.is_none_or(|e| e.calendar_invites) && has_calendar_attachment(&msg) {
        tracing::debug!(message_id = %msg.id, "gmail: skipping calendar invite");
        return None;
    }

    // An absent or whitespace-only Subject gets the same `untitled` placeholder
    // every adapter uses (Calendar summary, Jira summary).
    let subject = GmailSource::header(&msg, "Subject")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(strings.untitled);
    let from = GmailSource::header(&msg, "From").unwrap_or_default();
    let snippet = msg.snippet.as_deref().unwrap_or_default();

    let body = {
        let extracted = msg.payload.as_ref().map(extract_body).unwrap_or_default();
        if extracted.is_empty() {
            // The MIME walk found no text body — fall back to Gmail's short snippet so
            // the page isn't empty, but make the degradation observable: a silently
            // truncated body would be summarized by the LLM as if it were complete.
            tracing::warn!(
                message_id = %msg.id,
                "gmail: no text body extracted, falling back to snippet (truncated)"
            );
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
        return None;
    };

    let is_self =
        !identity_email.is_empty() && header_email(from).eq_ignore_ascii_case(identity_email);

    Some(RawItem {
        external_id: Some(msg.id.clone()),
        title: subject.to_string(),
        body,
        url: Some(format!(
            "https://mail.google.com/mail/u/0/#inbox/{}",
            msg.id
        )),
        author: Some(from.to_string()),
        timestamp: ts,
        is_self,
        metadata: serde_json::json!({
            "to": GmailSource::header(&msg, "To"),
            "labels": msg.label_ids,
        }),
    })
}

/// The bare address from a `From`/`To` header: `"Name" <addr@x>` → `addr@x`,
/// a bare `addr@x` → itself. Compared case-insensitively against the identity.
/// Trims surrounding whitespace inside the angle brackets too (`< addr >`).
fn header_email(raw: &str) -> &str {
    match raw.rsplit_once('<') {
        Some((_, rest)) => rest.trim().trim_end_matches('>').trim(),
        None => raw.trim(),
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

fn has_calendar_attachment(msg: &Message) -> bool {
    fn has_calendar_part(parts: Option<&[MimePart]>) -> bool {
        let Some(parts) = parts else { return false };
        for part in parts {
            if part
                .mime_type
                .as_deref()
                .is_some_and(|m| m.starts_with("text/calendar"))
            {
                return true;
            }
            if has_calendar_part(part.parts.as_deref()) {
                return true;
            }
        }
        false
    }
    msg.payload.as_ref().is_some_and(|p| {
        p.mime_type
            .as_deref()
            .is_some_and(|m| m.starts_with("text/calendar"))
            || has_calendar_part(p.parts.as_deref())
    })
}

fn decode_base64url(data: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(data)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{Message, header_email, map_message as map_message_inner};

    fn msg_from(json: serde_json::Value) -> Message {
        serde_json::from_value(json).expect("message fixture parses")
    }

    fn map_message(
        msg: Message,
        identity_email: &str,
        exclude: Option<&super::ExcludeParams>,
    ) -> Option<lk_core::event::RawItem> {
        map_message_inner(
            msg,
            identity_email,
            exclude,
            lk_core::i18n::Locale::default().strings(),
        )
    }

    // base64url(no-pad) of "Hello body".
    const PLAIN_BODY_B64: &str = "SGVsbG8gYm9keQ";

    #[test]
    fn map_message_extracts_body_and_self_ownership() {
        let msg = msg_from(serde_json::json!({
            "id": "m1",
            "snippet": "snippet text",
            "internalDate": "1769158800000",
            "labelIds": ["INBOX"],
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    {"name": "Subject", "value": "Weekly update"},
                    {"name": "From", "value": "\"Me\" <ME@x.com>"}
                ],
                "body": {"data": PLAIN_BODY_B64}
            }
        }));
        let item = map_message(msg, "me@x.com", None).expect("maps");
        assert_eq!(item.title, "Weekly update");
        assert_eq!(item.body, "Hello body");
        assert!(item.is_self, "From matches identity case-insensitively");
        assert_eq!(
            item.url.as_deref(),
            Some("https://mail.google.com/mail/u/0/#inbox/m1")
        );
    }

    #[test]
    fn map_message_falls_back_to_snippet_when_no_body() {
        let msg = msg_from(serde_json::json!({
            "id": "m2",
            "snippet": "just the snippet",
            "internalDate": "1769158800000",
            "payload": {"headers": [{"name": "From", "value": "other@x.com"}]}
        }));
        let item = map_message(msg, "me@x.com", None).expect("maps");
        assert_eq!(item.body, "just the snippet");
        assert!(!item.is_self);
    }

    #[test]
    fn map_message_blank_subject_gets_untitled_placeholder() {
        // A whitespace-only Subject header is the same as no Subject at all —
        // both land on the shared i18n `untitled` placeholder, never an empty
        // or whitespace-led title.
        let msg = msg_from(serde_json::json!({
            "id": "m4",
            "internalDate": "1769158800000",
            "payload": {"headers": [{"name": "Subject", "value": "   "}]}
        }));
        let item = map_message(msg, "me@x.com", None).expect("maps");
        assert_eq!(
            item.title,
            lk_core::i18n::Locale::default().strings().untitled
        );
    }

    #[test]
    fn map_message_skips_unparseable_timestamp() {
        let msg = msg_from(serde_json::json!({
            "id": "m3",
            "internalDate": "not-a-number",
            "payload": {"headers": []}
        }));
        assert!(map_message(msg, "me@x.com", None).is_none());
    }

    #[test]
    fn header_email_extracts_bare_address() {
        assert_eq!(header_email("\"Gildong Hong\" <me@x.com>"), "me@x.com");
        assert_eq!(header_email("me@x.com"), "me@x.com");
        assert_eq!(header_email("Me <me@x.com>"), "me@x.com");
        // Whitespace inside the brackets and around the header is trimmed.
        assert_eq!(header_email("Me < me@x.com > "), "me@x.com");
    }

    #[test]
    fn ownership_match_is_exact_sender_not_substring() {
        // The From sender matches the identity (case-insensitive) → self-authored.
        let me = "me@x.com";
        assert!(header_email("\"Me\" <ME@X.com>").eq_ignore_ascii_case(me));
        // A different sender does NOT match, even if the identity address appears
        // elsewhere in the raw header text (e.g. the user is in a reply-to display).
        assert!(!header_email("\"me@x.com via list\" <list@x.com>").eq_ignore_ascii_case(me));
        // A look-alike longer domain must not match.
        assert!(!header_email("<me@x.com.evil.com>").eq_ignore_ascii_case(me));
    }
}
