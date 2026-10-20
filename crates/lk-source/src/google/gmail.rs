use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use lk_core::event::RawItem;

use super::GoogleAuth;
use crate::{ExtractContext, Source, SourceError};

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_MIME_PARTS: usize = 1024;
const MAX_MIME_DEPTH: usize = 32;

pub struct GmailSource {
    http: reqwest::Client,
    auth: Arc<GoogleAuth>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GmailParams {
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    /// At least one Gmail search query, OR-combined. Required: an empty list has no stable
    /// filter, so the window would fall to mailbox read-state and the daily page would change
    /// between runs — see `validate_params`. Use durable filters (labels, senders), never
    /// read-state.
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
    crate::parse_validated::<GmailParams>(params).map(|_| ())
}

impl crate::ValidatedParams for GmailParams {
    fn validate(&self) -> Result<(), SourceError> {
        // Enforced at consumption (via `parse_validated` in `extract`), not just at the
        // config boundary: an empty `include_queries` would assemble `() after:…`, a
        // silently malformed Gmail query, and a read-state-dependent window besides.
        if self.include_queries.is_empty() {
            return Err(SourceError::InvalidParams(
                "gmail `include_queries` must list at least one query — with no stable filter the \
                 daily page would depend on mailbox read-state and change between runs, breaking the \
                 complete-refetch guarantee. Use a durable filter such as a label \
                 (`label:newsletters`) or sender (`from:@example.com`)."
                    .into(),
            ));
        }
        if self.include_queries.iter().any(|q| q.trim().is_empty()) {
            return Err(SourceError::InvalidParams(
                "gmail `include_queries` entries must each be a non-blank query — a blank term \
                 assembles an empty `()` clause and silently malforms the Gmail search."
                    .into(),
            ));
        }
        if self.max_messages == 0 {
            return Err(SourceError::InvalidParams(
                "gmail `max_messages` must be > 0".into(),
            ));
        }
        // A blank exclude pattern is the mirror image of a blank include term: `str::contains`
        // matches the empty string against EVERY subject/sender, so one stray `""` entry would
        // silently drop the entire day's mail with no signal. Reject it at the same boundary.
        if let Some(exclude) = &self.exclude
            && exclude
                .subjects
                .iter()
                .chain(exclude.senders.iter())
                .any(|p| p.trim().is_empty())
        {
            return Err(SourceError::InvalidParams(
                "gmail `exclude.subjects`/`exclude.senders` entries must each be non-blank — a blank \
                 pattern matches every message and would silently drop the entire day's mail."
                    .into(),
            ));
        }
        Ok(())
    }
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
    messages: Option<Vec<MessageReference>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct MessageReference {
    id: String,
}

#[derive(Deserialize)]
struct Message {
    id: String,
    payload: Option<MimePart>,
    #[serde(rename = "internalDate")]
    internal_date: Option<String>,
    #[serde(rename = "labelIds")]
    label_ids: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct MimePart {
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(default)]
    filename: String,
    headers: Option<Vec<Header>>,
    body: Option<PartBody>,
    parts: Option<Vec<MimePart>>,
}

#[derive(Deserialize)]
struct PartBody {
    data: Option<String>,
    #[serde(rename = "attachmentId")]
    attachment_id: Option<String>,
    size: Option<u64>,
}

#[derive(Deserialize)]
struct Header {
    name: String,
    value: String,
}

#[derive(Deserialize, serde::Serialize)]
struct ApiErrorResponse {
    error: ApiError,
}

#[derive(Deserialize, serde::Serialize)]
struct ApiError {
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    errors: Vec<ApiErrorReason>,
}

#[derive(Deserialize, serde::Serialize)]
struct ApiErrorReason {
    #[serde(skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
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

    fn is_excluded(msg: &Message, exclude: &ExcludeParams) -> bool {
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
        Ok(self.observe(params, ctx).await?.items)
    }

    async fn observe(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<crate::SourceObservation, SourceError> {
        let p: GmailParams = crate::parse_validated(params)?;
        let token = self.auth.access_token().await?;
        GmailApi {
            http: &self.http,
            token: &token,
            base: url::Url::parse(BASE).expect("static Gmail URL"),
        }
        .observe(&p, ctx)
        .await
    }
}

struct GmailApi<'a> {
    http: &'a reqwest::Client,
    token: &'a str,
    base: url::Url,
}

impl GmailApi<'_> {
    async fn get<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<T, SourceError> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("Gmail API URL has path segments")
            .pop_if_empty()
            .extend(segments);
        let response = crate::retry::send_with_retry(|| {
            self.http
                .get(url.clone())
                .bearer_auth(self.token)
                .query(query)
                .send()
        })
        .await?;
        read_json(response).await
    }

    async fn observe(
        &self,
        p: &GmailParams,
        ctx: &ExtractContext,
    ) -> Result<crate::SourceObservation, SourceError> {
        let mut partial = false;
        // Constrain the query to the target day's window anchored to ctx.target_date so
        // --date backfill fetches the right day instead of "newer_than N hours from now".
        // Gmail's after:/before: accept epoch seconds, which (unlike YYYY/MM/DD) are
        // timezone-exact — the bounds come from day_window in the vault's timezone. The OR
        // group is parenthesized so the bounds apply to every include_query, not just the
        // last term. `parse_validated` ran `GmailParams::validate`, so `include_queries` is
        // non-empty here and the window always carries a stable filter — never read-state,
        // which would make the complete-refetch window drift between runs.
        let (min, max) = ctx.day_window(p.lookback_hours, 0)?;
        let query = format!(
            "({}) after:{} before:{}",
            p.include_queries.join(" OR "),
            min.as_second(),
            max.as_second()
        );

        // Requested page size; the server may return fewer (or zero) per page.
        // Termination is `paging::page_step` — the rule all listing adapters share.
        const PAGE_SIZE: usize = 50;

        let mut refs: Vec<MessageReference> = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages_fetched = 0usize;

        loop {
            // The listing GET is idempotent — retry transient 429/5xx so one provider hiccup
            // doesn't abort an unattended ingest (the same guard Jira's search uses). The
            // per-message fetch below stays warn-and-skip (one bad message ≠ whole-run failure).
            let mut query_params =
                vec![("q", query.clone()), ("maxResults", PAGE_SIZE.to_string())];
            if let Some(token) = &page_token {
                query_params.push(("pageToken", token.clone()));
            }
            let list: ListResponse = self.get(&["messages"], &query_params).await?;

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
                        partial = true;
                        tracing::warn!(
                            max = p.max_messages,
                            "gmail: message cap hit, some messages may have been dropped; raise max_messages"
                        );
                    }
                    break;
                }
                crate::paging::PageStep::Exhausted => {
                    partial = true;
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
        let mut observed = 0;
        for r in &refs {
            let result = async {
                let msg: Message = self
                    .get(&["messages", &r.id], &[("format", "full".into())])
                    .await?;
                if is_filtered(&msg, p.exclude.as_ref()) {
                    return Ok(None);
                }
                let body = load_body(msg.payload.as_ref(), |attachment_id| {
                    let message_id = msg.id.clone();
                    async move {
                        self.get::<PartBody>(
                            &["messages", &message_id, "attachments", &attachment_id],
                            &[],
                        )
                        .await
                    }
                })
                .await?;
                map_message_observed(msg, body, ctx.identity.email.trim(), ctx.locale.strings())
                    .map(Some)
            }
            .await;
            match result {
                Ok(item) => {
                    observed += 1;
                    items.extend(item);
                }
                Err(error) => {
                    partial = true;
                    tracing::warn!(message_id = %r.id, error = %error, "gmail: skipping unreadable message");
                }
            }
        }

        crate::require_any_observation("gmail message", observed, refs.len())?;
        tracing::info!(
            total = refs.len(),
            kept = items.len(),
            "gmail: extraction complete"
        );
        Ok(crate::SourceObservation {
            items,
            scope: crate::ObservationScope::Window {
                start: min,
                end: max,
            },
            outcome: if partial {
                crate::ObservationOutcome::Partial
            } else {
                crate::ObservationOutcome::Observed
            },
        })
    }
}

fn map_message_observed(
    msg: Message,
    body: String,
    identity_email: &str,
    strings: &lk_core::i18n::Strings,
) -> Result<RawItem, SourceError> {
    let subject = GmailSource::header(&msg, "Subject")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(strings.untitled);
    let from = GmailSource::header(&msg, "From").unwrap_or_default();
    let timestamp = msg
        .internal_date
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .ok_or_else(|| SourceError::Parse("Gmail message has no valid internalDate".into()))?;
    let is_self =
        !identity_email.is_empty() && header_email(from).eq_ignore_ascii_case(identity_email);
    Ok(RawItem {
        labels: Vec::new(),
        external_id: Some(msg.id.clone()),
        title: subject.to_string(),
        body,
        // #all links survive archiving and label changes.
        url: Some(format!("https://mail.google.com/mail/u/0/#all/{}", msg.id)),
        author: Some(from.to_string()),
        timestamp,
        is_self,
        work: None,
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

async fn load_body<F, Fut>(
    payload: Option<&MimePart>,
    mut load_attachment: F,
) -> Result<String, SourceError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<PartBody, SourceError>>,
{
    let payload =
        payload.ok_or_else(|| SourceError::Parse("Gmail message has no payload".into()))?;
    let mut visited = 0;
    validate_mime_structure(payload, 0, &mut visited)?;
    let mut parts = Vec::new();
    collect_parts(payload, &mut parts)?;
    if parts.is_empty() {
        return Err(SourceError::Parse(
            "Gmail message has no supported text body".into(),
        ));
    }
    let mut result = String::new();
    let mut acquired_bytes = 0;
    for (part, media) in parts {
        let body = part
            .body
            .as_ref()
            .ok_or_else(|| SourceError::Parse("Gmail text part has no body".into()))?;
        let expected_size = body.size;
        let loaded;
        let body = if let Some(id) = &body.attachment_id {
            if body.size.is_some_and(|size| size > MAX_BODY_BYTES as u64) {
                return Err(SourceError::ResponseTooLarge {
                    max_bytes: MAX_BODY_BYTES,
                });
            }
            loaded = load_attachment(id.clone()).await?;
            if body
                .size
                .zip(loaded.size)
                .is_some_and(|(expected, actual)| expected != actual)
            {
                return Err(SourceError::Parse(
                    "Gmail body size changed during acquisition".into(),
                ));
            }
            &loaded
        } else {
            body
        };
        let bytes = decode_base64url(body)?;
        if expected_size.is_some_and(|size| size != bytes.len() as u64) {
            return Err(SourceError::Parse(
                "Gmail body does not match its declared size".into(),
            ));
        }
        if bytes.len() > MAX_BODY_BYTES - acquired_bytes {
            return Err(SourceError::ResponseTooLarge {
                max_bytes: MAX_BODY_BYTES,
            });
        }
        acquired_bytes += bytes.len();
        let encoding = match media.get_param(mime::CHARSET) {
            Some(charset) => encoding_rs::Encoding::for_label(charset.as_str().as_bytes())
                .ok_or_else(|| SourceError::Parse("unsupported Gmail body charset".into()))?,
            None => encoding_rs::UTF_8,
        };
        let (text, malformed) = encoding.decode_with_bom_removal(&bytes);
        if malformed {
            return Err(SourceError::Parse(
                "Gmail body cannot be decoded without replacement characters".into(),
            ));
        }
        let text = if media.essence_str() == "text/html" {
            crate::markdown::html_to_markdown(&text)
        } else {
            text.into_owned()
        };
        let separator = if result.is_empty() || text.is_empty() {
            ""
        } else {
            "\n\n"
        };
        if separator.len() + text.len() > MAX_BODY_BYTES - result.len() {
            return Err(SourceError::ResponseTooLarge {
                max_bytes: MAX_BODY_BYTES,
            });
        }
        result.push_str(separator);
        result.push_str(&text);
    }
    Ok(result)
}

fn validate_mime_structure(
    part: &MimePart,
    depth: usize,
    visited: &mut usize,
) -> Result<(), SourceError> {
    *visited += 1;
    if depth > MAX_MIME_DEPTH || *visited > MAX_MIME_PARTS {
        return Err(SourceError::Parse(
            "Gmail MIME structure exceeds its part or depth limit".into(),
        ));
    }
    if let Some(children) = &part.parts {
        for child in children {
            validate_mime_structure(child, depth + 1, visited)?;
        }
    }
    Ok(())
}

fn collect_parts<'a>(
    part: &'a MimePart,
    selected: &mut Vec<(&'a MimePart, mime::Mime)>,
) -> Result<(), SourceError> {
    let declared = header_value(part, "Content-Type")
        .or(part.mime_type.as_deref())
        .ok_or_else(|| SourceError::Parse("Gmail MIME part has no media type".into()))?;
    let media: mime::Mime = declared
        .parse()
        .map_err(|_| SourceError::Parse("Gmail MIME part has an invalid Content-Type".into()))?;
    if let Some(kind) = &part.mime_type {
        let kind: mime::Mime = kind
            .parse()
            .map_err(|_| SourceError::Parse("Gmail MIME part has an invalid mimeType".into()))?;
        if kind.essence_str() != media.essence_str() {
            return Err(SourceError::Parse(
                "Gmail MIME type disagrees with Content-Type".into(),
            ));
        }
    }
    if media.type_() == mime::MULTIPART {
        let children = part
            .parts
            .as_deref()
            .filter(|parts| !parts.is_empty())
            .ok_or_else(|| SourceError::Parse("Gmail multipart body has no parts".into()))?;
        match media.subtype().as_str() {
            "alternative" => {
                // RFC 2046 alternatives are ordered by increasing faithfulness.
                for child in children.iter().rev().filter(|child| !is_attachment(child)) {
                    let before = selected.len();
                    collect_parts(child, selected)?;
                    if selected.len() != before {
                        break;
                    }
                }
            }
            "related" => {
                let root = if let Some(start) = media.get_param("start") {
                    children
                        .iter()
                        .find(|child| {
                            header_value(child, "Content-ID")
                                .is_some_and(|id| id.trim() == start.as_str())
                        })
                        .ok_or_else(|| {
                            SourceError::Parse("Gmail multipart/related root is absent".into())
                        })?
                } else {
                    &children[0]
                };
                collect_parts(root, selected)?;
            }
            _ => {
                for child in children.iter().filter(|child| !is_attachment(child)) {
                    collect_parts(child, selected)?;
                }
            }
        }
    } else if matches!(media.essence_str(), "text/plain" | "text/html") {
        if part.parts.as_ref().is_some_and(|parts| !parts.is_empty()) {
            return Err(SourceError::Parse(
                "Gmail text body unexpectedly contains MIME parts".into(),
            ));
        }
        selected.push((part, media));
    }
    Ok(())
}

fn is_attachment(part: &MimePart) -> bool {
    !part.filename.is_empty()
        || header_value(part, "Content-Disposition").is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("attachment"))
        })
}

fn header_value<'a>(part: &'a MimePart, name: &str) -> Option<&'a str> {
    part.headers
        .as_deref()?
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

fn is_filtered(msg: &Message, exclude: Option<&ExcludeParams>) -> bool {
    exclude.is_some_and(|exclude| GmailSource::is_excluded(msg, exclude))
        || (exclude.is_none_or(|exclude| exclude.calendar_invites) && has_calendar_attachment(msg))
}

async fn read_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, SourceError> {
    let status = response.status();
    let max_bytes = if status.is_success() {
        MAX_RESPONSE_BYTES
    } else {
        MAX_ERROR_BYTES
    };
    let exceeded_limit = || {
        if status.is_success() {
            SourceError::ResponseTooLarge { max_bytes }
        } else {
            SourceError::Api {
                status: status.as_u16(),
                message: "Gmail request failed; error diagnostics exceed their acquisition limit"
                    .into(),
            }
        }
    };
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(exceeded_limit());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > max_bytes - bytes.len() {
            return Err(exceeded_limit());
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let diagnostic = serde_json::from_slice::<ApiErrorResponse>(&bytes)
            .ok()
            .filter(|response| {
                response.error.status.is_some()
                    || response
                        .error
                        .errors
                        .iter()
                        .any(|error| error.domain.is_some() || error.reason.is_some())
            })
            .and_then(|response| serde_json::to_string(&response.error).ok());
        return Err(SourceError::Api {
            status: status.as_u16(),
            message: diagnostic
                .map(|diagnostic| format!("Gmail request failed: {diagnostic}"))
                .unwrap_or_else(|| "Gmail request failed; no structured error diagnostics".into()),
        });
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| SourceError::Parse("invalid Gmail JSON response".into()))
}

fn has_calendar_attachment(msg: &Message) -> bool {
    fn contains_calendar(part: &MimePart) -> bool {
        part.mime_type
            .as_deref()
            .and_then(|value| value.parse::<mime::Mime>().ok())
            .is_some_and(|media| media.essence_str() == "text/calendar")
            || part
                .parts
                .as_deref()
                .is_some_and(|parts| parts.iter().any(contains_calendar))
    }
    msg.payload.as_ref().is_some_and(contains_calendar)
}

fn decode_base64url(body: &PartBody) -> Result<Vec<u8>, SourceError> {
    if body.size.is_some_and(|size| size > MAX_BODY_BYTES as u64) {
        return Err(SourceError::ResponseTooLarge {
            max_bytes: MAX_BODY_BYTES,
        });
    }
    let Some(data) = &body.data else {
        return if body.size == Some(0) {
            Ok(Vec::new())
        } else {
            Err(SourceError::Parse("Gmail text body is unavailable".into()))
        };
    };
    if data.len() > MAX_BODY_BYTES.div_ceil(3) * 4 {
        return Err(SourceError::ResponseTooLarge {
            max_bytes: MAX_BODY_BYTES,
        });
    }
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    let bytes = engine
        .decode(data)
        .map_err(|_| SourceError::Parse("invalid Gmail base64url body".into()))?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(SourceError::ResponseTooLarge {
            max_bytes: MAX_BODY_BYTES,
        });
    }
    if body.size.is_some_and(|size| size != bytes.len() as u64) {
        return Err(SourceError::Parse(
            "Gmail body does not match its declared size".into(),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn msg_from(json: serde_json::Value) -> Message {
        serde_json::from_value(json).expect("message fixture parses")
    }

    async fn map_message(
        msg: Message,
        identity_email: &str,
        exclude: Option<&ExcludeParams>,
    ) -> Result<Option<lk_core::event::RawItem>, SourceError> {
        if is_filtered(&msg, exclude) {
            return Ok(None);
        }
        let body = load_body(msg.payload.as_ref(), |_| async {
            panic!("inline fixture must not request an attachment")
        })
        .await?;
        map_message_observed(
            msg,
            body,
            identity_email,
            lk_core::i18n::Locale::default().strings(),
        )
        .map(Some)
    }

    #[tokio::test]
    async fn invalid_timestamp_is_an_error_but_explicit_exclusion_is_observed() {
        let bad = || {
            msg_from(serde_json::json!({
                "id": "bad-date", "internalDate": "not-a-date",
                "payload": {"mimeType": "text/plain", "body": {"size": 0},
                    "headers": [{"name": "Subject", "value": "skip this"}]}
            }))
        };
        assert!(map_message(bad(), "me@x.com", None).await.is_err());
        let exclude =
            serde_json::from_value(serde_json::json!({"subjects": ["skip this"]})).unwrap();
        assert!(
            map_message(bad(), "me@x.com", Some(&exclude))
                .await
                .unwrap()
                .is_none()
        );
    }

    // base64url(no-pad) of "Hello body".
    const PLAIN_BODY_B64: &str = "SGVsbG8gYm9keQ";

    #[tokio::test]
    async fn map_message_extracts_body_and_self_ownership() {
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
        let item = map_message(msg, "me@x.com", None)
            .await
            .unwrap()
            .expect("maps");
        assert_eq!(item.title, "Weekly update");
        assert_eq!(item.body, "Hello body");
        assert!(item.is_self, "From matches identity case-insensitively");
        assert_eq!(
            item.url.as_deref(),
            Some("https://mail.google.com/mail/u/0/#all/m1")
        );
    }

    #[tokio::test]
    async fn missing_body_never_substitutes_a_snippet() {
        let msg = msg_from(serde_json::json!({
            "id": "m2", "snippet": "incomplete preview", "internalDate": "1769158800000",
            "payload": {"mimeType": "text/plain", "body": {"size": 42}}
        }));
        assert!(map_message(msg, "me@x.com", None).await.is_err());
    }

    #[tokio::test]
    async fn map_message_blank_subject_gets_untitled_placeholder() {
        // A whitespace-only Subject header is the same as no Subject at all —
        // both land on the shared i18n `untitled` placeholder, never an empty
        // or whitespace-led title.
        let msg = msg_from(serde_json::json!({
            "id": "m4",
            "internalDate": "1769158800000",
            "payload": {"mimeType": "text/plain", "body": {"size": 0}, "headers": [{"name": "Subject", "value": "   "}]}
        }));
        let item = map_message(msg, "me@x.com", None)
            .await
            .unwrap()
            .expect("maps");
        assert_eq!(
            item.title,
            lk_core::i18n::Locale::default().strings().untitled
        );
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

    #[test]
    fn validate_rejects_blank_exclude_pattern() {
        use super::validate_params;
        let with_exclude = |exclude: serde_json::Value| {
            validate_params(&serde_json::json!({
                "include_queries": ["label:newsletters"],
                "exclude": exclude,
            }))
        };
        // A blank subject/sender pattern `contains`-matches every message → silent total drop.
        assert!(with_exclude(serde_json::json!({"subjects": [""]})).is_err());
        assert!(with_exclude(serde_json::json!({"senders": ["  "]})).is_err());
        // Real patterns are fine, and an absent exclude block is fine.
        assert!(
            with_exclude(serde_json::json!({"subjects": ["[ad]"], "senders": ["noreply@x.com"]}))
                .is_ok()
        );
        assert!(validate_params(&serde_json::json!({"include_queries": ["label:x"]})).is_ok());
    }
    fn text_part(text: &str) -> serde_json::Value {
        serde_json::json!({
            "mimeType": "text/plain",
            "body": {"data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text), "size": text.len()},
        })
    }

    async fn body(value: serde_json::Value) -> Result<String, SourceError> {
        let part = serde_json::from_value(value).unwrap();
        load_body(Some(&part), |_| async {
            panic!("unexpected attachment request")
        })
        .await
    }

    #[tokio::test]
    async fn mime_structure_selects_alternatives_and_preserves_independent_siblings() {
        let html = "<p>Faithful body</p>";
        let payload = serde_json::json!({
            "mimeType": "multipart/mixed",
            "parts": [
                {"mimeType": "multipart/alternative", "parts": [text_part("Alternative"), {
                    "mimeType": "text/html", "body": {
                        "data": base64::engine::general_purpose::URL_SAFE.encode(html), "size": html.len()
                    }
                }]},
                text_part("Independent sibling"),
                {"mimeType": "text/plain", "filename": "attachment.txt", "body": {"attachmentId": "skip-file"}},
                {"mimeType": "text/plain", "headers": [{"name": "Content-Disposition", "value": "attachment"}], "body": {"attachmentId": "skip-disposition"}}
            ]
        });
        assert_eq!(
            body(payload).await.unwrap(),
            "Faithful body\n\nIndependent sibling"
        );
        assert_eq!(
            body(serde_json::json!({
                "mimeType": "multipart/alternative",
                "parts": [text_part("Supported"), {"mimeType": "application/octet-stream"}]
            }))
            .await
            .unwrap(),
            "Supported"
        );
        let root = serde_json::json!({
            "mimeType": "text/plain", "filename": "root.txt",
            "headers": [{"name": "Content-ID", "value": "<body>"}],
            "body": text_part("Related root")["body"]
        });
        assert_eq!(body(serde_json::json!({
            "mimeType": "multipart/related",
            "headers": [{"name": "Content-Type", "value": "multipart/related; start=\"<body>\"; type=\"text/plain\""}],
            "parts": [text_part("Resource, not an independent body"), root]
        })).await.unwrap(), "Related root");
    }

    #[tokio::test]
    async fn text_decoding_is_strict_and_legitimate_empty_bodies_remain_valid() {
        assert_eq!(
            body(serde_json::json!({"mimeType": "text/plain", "body": {"size": 0}}))
                .await
                .unwrap(),
            ""
        );
        let (bytes, _, _) = encoding_rs::EUC_KR.encode("한글 근거");
        assert_eq!(body(serde_json::json!({
            "mimeType": "text/plain", "headers": [{"name": "Content-Type", "value": "text/plain; charset=\"EUC-KR\""}],
            "body": {"data": base64::engine::general_purpose::URL_SAFE.encode(bytes.as_ref()), "size": bytes.len()}
        })).await.unwrap(), "한글 근거");
        for invalid in [
            serde_json::json!({"mimeType": "text/plain", "body": {"data": "%%%"}}),
            serde_json::json!({"mimeType": "text/plain", "body": {"data": "_w"}}),
            serde_json::json!({"mimeType": "text/plain", "body": {"size": 1}}),
            serde_json::json!({"mimeType": "text/plain", "body": {"data": "YQ", "size": 2}}),
            serde_json::json!({"mimeType": "text/plain", "headers": [{"name": "Content-Type", "value": "text/plain; charset=unknown"}], "body": {"size": 0}}),
            serde_json::json!({"mimeType": "text/plain", "headers": [{"name": "Content-Type", "value": "text/html"}], "body": {"size": 0}}),
            serde_json::json!({"mimeType": "multipart/related", "headers": [{"name": "Content-Type", "value": "multipart/related; start=\"<absent>\""}], "parts": [text_part("Body")]}),
            serde_json::json!({"mimeType": "multipart/alternative", "parts": [text_part("Earlier alternative"), {"mimeType": "text/plain", "body": {"data": "%%%"}}]}),
            serde_json::json!({"mimeType": "multipart/mixed", "parts": [text_part("Readable sibling"), {"mimeType": "text/plain", "body": {"size": 3}}]}),
        ] {
            assert!(
                body(invalid.clone()).await.is_err(),
                "must reject {invalid}"
            );
        }
    }

    #[test]
    fn calendar_filter_uses_the_exact_media_type() {
        for (media, expected) in [
            ("text/calendar; method=REQUEST", true),
            ("text/calendar-extra", false),
        ] {
            let msg = msg_from(message(
                "calendar",
                serde_json::json!({"mimeType": "multipart/mixed", "parts": [{"mimeType": media}]}),
            ));
            assert_eq!(is_filtered(&msg, None), expected);
        }
    }

    #[tokio::test]
    async fn mime_and_body_budgets_refuse_overflow_before_fetching() {
        let oversized = serde_json::json!({"mimeType": "text/plain", "body": {"size": MAX_BODY_BYTES + 1, "attachmentId": "too-large"}});
        assert!(matches!(
            body(oversized).await,
            Err(SourceError::ResponseTooLarge { .. })
        ));
        let mut nested = text_part("Body");
        for _ in 0..=MAX_MIME_DEPTH {
            nested = serde_json::json!({"mimeType": "multipart/mixed", "parts": [nested]});
        }
        assert!(body(nested).await.is_err());
        assert!(body(serde_json::json!({"mimeType": "multipart/mixed", "parts": vec![text_part(""); MAX_MIME_PARTS]})).await.is_err());
    }

    struct TestServer {
        base: url::Url,
        requests: Arc<Mutex<Vec<url::Url>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn server(respond: impl Fn(&url::Url) -> Vec<u8> + Send + Sync + 'static) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base: url::Url = format!(
            "http://{}/gmail/v1/users/me",
            listener.local_addr().unwrap()
        )
        .parse()
        .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let server_base = base.clone();
        let task = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read(&mut byte).await.unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                    assert!(request.len() < 16 * 1024);
                }
                let target = std::str::from_utf8(&request)
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let url = server_base.join(target).unwrap();
                captured.lock().unwrap().push(url.clone());
                let _ = stream.write_all(&respond(&url)).await;
                let _ = stream.shutdown().await;
            }
        });
        TestServer {
            base,
            requests,
            task,
        }
    }

    fn json_response(value: serde_json::Value) -> Vec<u8> {
        let body = serde_json::to_vec(&value).unwrap();
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(&body);
        response
    }

    fn context() -> ExtractContext {
        ExtractContext {
            target_date: jiff::civil::date(2026, 10, 19),
            timezone: jiff::tz::TimeZone::get("Asia/Seoul").unwrap(),
            locale: lk_core::i18n::Locale::En,
            identity: lk_core::config::Identity {
                name: "Test".into(),
                email: "test@example.com".into(),
                slack_id: None,
            },
            vault_root: ".".into(),
            vault_dirs: Default::default(),
            documents: None,
            originals: Default::default(),
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap()
    }

    fn message(id: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"id": id, "internalDate": "1792378800000", "snippet": "Incomplete preview", "payload": payload})
    }

    #[tokio::test]
    async fn external_text_is_fetched_with_safe_paths_and_failures_are_not_observed_as_complete() {
        let server = server(|url| match url.path() {
            "/gmail/v1/users/me/messages" => json_response(serde_json::json!({"messages": [{"id": "external"}, {"id": "broken"}, {"id": "empty"}]})),
            "/gmail/v1/users/me/messages/external" => json_response(message("external", serde_json::json!({"mimeType": "text/plain", "body": {"attachmentId": "part/id", "size": 10}}))),
            "/gmail/v1/users/me/messages/external/attachments/part%2Fid" => json_response(serde_json::json!({"data": PLAIN_BODY_B64, "size": 10})),
            "/gmail/v1/users/me/messages/broken" => json_response(message("broken", serde_json::json!({"mimeType": "text/plain", "body": {"attachmentId": "missing", "size": 10}}))),
            "/gmail/v1/users/me/messages/broken/attachments/missing" => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            "/gmail/v1/users/me/messages/empty" => json_response(message("empty", serde_json::json!({"mimeType": "text/plain", "body": {"size": 0}}))),
            other => panic!("unexpected request {other}"),
        }).await;
        let http = client();
        let api = GmailApi {
            http: &http,
            token: "fixture",
            base: server.base.clone(),
        };
        let params = crate::parse_validated::<GmailParams>(
            &serde_json::json!({"include_queries": ["label:kept"], "max_messages": 10}),
        )
        .unwrap();
        let result = api.observe(&params, &context()).await.unwrap();
        assert_eq!(result.outcome, crate::ObservationOutcome::Partial);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].body, "Hello body");
        assert_eq!(result.items[1].body, "");
        assert_eq!(server.requests.lock().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn all_unreadable_is_an_error_but_filtered_and_empty_listings_are_valid() {
        for mode in ["unreadable", "excluded", "calendar", "empty"] {
            let server = server(move |url| match url.path() {
                "/gmail/v1/users/me/messages" => json_response(if mode == "empty" {
                    serde_json::json!({})
                } else {
                    serde_json::json!({"messages": [{"id": "one"}]})
                }),
                "/gmail/v1/users/me/messages/one" => json_response(message(
                    "one",
                    serde_json::json!({
                        "mimeType": if mode == "calendar" {"text/calendar"} else {"text/plain"},
                        "headers": [{"name": "Subject", "value": "skip"}],
                        "body": {"attachmentId": "missing", "size": 1}
                    }),
                )),
                "/gmail/v1/users/me/messages/one/attachments/missing" if mode == "unreadable" => {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                }
                other => panic!("unexpected request {other}"),
            })
            .await;
            let http = client();
            let api = GmailApi {
                http: &http,
                token: "fixture",
                base: server.base.clone(),
            };
            let params = crate::parse_validated::<GmailParams>(&serde_json::json!({"include_queries": ["label:kept"], "exclude": {"subjects": if mode == "excluded" { vec!["skip"] } else { vec![] }}})).unwrap();
            let result = api.observe(&params, &context()).await;
            if mode == "unreadable" {
                assert!(matches!(
                    result,
                    Err(SourceError::NothingObserved { attempted: 1, .. })
                ));
            } else {
                let result = result.unwrap();
                assert_eq!(result.outcome, crate::ObservationOutcome::Observed);
                assert!(result.items.is_empty());
                assert_eq!(
                    server.requests.lock().unwrap().len(),
                    if mode == "empty" { 1 } else { 2 }
                );
            }
        }
    }

    #[tokio::test]
    async fn api_errors_preserve_defined_identifiers_without_provider_text() {
        for (domain, reason) in [
            ("usageLimits", "rateLimitExceeded"),
            ("global", "insufficientPermissions"),
        ] {
            let server = server(move |_| {
                let body = serde_json::to_vec(&serde_json::json!({
                    "error": {"status": "PERMISSION_DENIED", "message": "private provider text",
                        "errors": [{"domain": domain, "reason": reason, "message": "private provider text"}]}
                })).unwrap();
                let mut response = format!("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
                response.extend_from_slice(&body);
                response
            }).await;
            let http = client();
            let api = GmailApi {
                http: &http,
                token: "fixture",
                base: server.base.clone(),
            };
            let Err(SourceError::Api { status, message }) =
                api.get::<Message>(&["messages", "one"], &[]).await
            else {
                panic!("expected the provider's HTTP error")
            };
            assert_eq!(status, 403);
            assert!(message.contains("PERMISSION_DENIED"));
            assert!(message.contains(domain));
            assert!(message.contains(reason));
            assert!(!message.contains("private provider text"));
        }
    }

    #[tokio::test]
    async fn api_refuses_oversized_json_before_reading_it() {
        let server = server(|_| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_RESPONSE_BYTES + 1
            )
            .into_bytes()
        })
        .await;
        let http = client();
        let api = GmailApi {
            http: &http,
            token: "fixture",
            base: server.base.clone(),
        };
        assert!(matches!(
            api.get::<Message>(&["messages", "one"], &[]).await,
            Err(SourceError::ResponseTooLarge { .. })
        ));
    }
}
