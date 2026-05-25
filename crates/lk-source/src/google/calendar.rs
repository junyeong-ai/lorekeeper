use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use super::{GoogleAuth, check_response};
use crate::{ExtractContext, Source, SourceError};

const BASE: &str = "https://www.googleapis.com/calendar/v3";

/// Maximum number of Drive files to fetch meeting notes from per calendar event.
const MAX_DRIVE_FETCHES_PER_EVENT: usize = 3;

pub struct CalendarSource {
    http: reqwest::Client,
    auth: Arc<GoogleAuth>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CalendarParams {
    #[serde(default = "default_calendar")]
    calendar_id: String,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    #[serde(default = "default_lookahead")]
    lookahead_hours: u32,
    #[serde(default)]
    fetch_meeting_notes: bool,
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    serde_json::from_value::<CalendarParams>(params.clone())
        .map(|_| ())
        .map_err(|e| SourceError::InvalidParams(e.to_string()))
}

fn default_calendar() -> String {
    "primary".into()
}
fn default_lookback() -> u32 {
    24
}
fn default_lookahead() -> u32 {
    24
}

#[derive(Deserialize)]
struct EventList {
    items: Option<Vec<CalEvent>>,
}

#[derive(Deserialize)]
struct CalEvent {
    id: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    status: Option<String>,
    #[serde(rename = "htmlLink")]
    html_link: Option<String>,
    start: Option<EventTime>,
    organizer: Option<Person>,
    attendees: Option<Vec<Person>>,
    #[serde(default)]
    attachments: Vec<Attachment>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Attachment {
    #[serde(rename = "fileUrl")]
    file_url: Option<String>,
    title: Option<String>,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(rename = "fileId")]
    file_id: Option<String>,
}

#[derive(Deserialize)]
struct EventTime {
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
    date: Option<String>,
}

#[derive(Deserialize)]
struct Person {
    email: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

/// Extract Google Drive / Docs file IDs from text containing Drive URLs.
///
/// Matches patterns like:
/// - `https://docs.google.com/document/d/{id}/edit`
/// - `https://drive.google.com/file/d/{id}/view`
/// - `https://docs.google.com/spreadsheets/d/{id}/edit`
/// - `https://docs.google.com/presentation/d/{id}/edit`
fn extract_drive_file_ids(text: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"https?://(?:docs|drive)\.google\.com/(?:document|spreadsheets?|presentation|file)/d/([a-zA-Z0-9_-]+)",
        )
        .unwrap()
    });
    re.captures_iter(text)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

/// Fetch a Google Drive file's content as plain text.
///
/// First attempts an export as `text/plain` (works for Google-native Docs/Sheets/Slides).
/// Falls back to direct download (`alt=media`) for non-native files (uploaded PDFs, txt, etc.).
async fn fetch_drive_content(
    http: &reqwest::Client,
    token: &str,
    file_id: &str,
) -> Result<String, SourceError> {
    // Try export as text/plain (for Google Docs / Sheets / Slides).
    let export_resp = http
        .get(format!(
            "https://www.googleapis.com/drive/v3/files/{file_id}/export"
        ))
        .bearer_auth(token)
        .query(&[("mimeType", "text/plain")])
        .send()
        .await?;

    if export_resp.status().is_success() {
        return export_resp.text().await.map_err(SourceError::Http);
    }

    // Fallback: direct download (for non-Google-native files).
    let resp = http
        .get(format!(
            "https://www.googleapis.com/drive/v3/files/{file_id}"
        ))
        .bearer_auth(token)
        .query(&[("alt", "media")])
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(SourceError::Api {
            status: resp.status().as_u16(),
            message: format!("drive file {file_id}"),
        });
    }
    resp.text().await.map_err(SourceError::Http)
}

impl CalendarSource {
    pub fn new(http: reqwest::Client, auth: Arc<GoogleAuth>) -> Self {
        Self { http, auth }
    }
}

#[async_trait]
impl Source for CalendarSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: CalendarParams = serde_json::from_value(params.clone())
            .map_err(|e| SourceError::InvalidParams(e.to_string()))?;

        let token = self.auth.access_token().await?;

        let (time_min, time_max) = ctx.day_window(p.lookback_hours, p.lookahead_hours)?;

        // Build the URL via path-segment pushing so the calendar id is percent-encoded;
        // ids can contain characters (`#`, spaces) that would otherwise corrupt the path.
        let mut url = reqwest::Url::parse(&format!("{BASE}/calendars"))
            .map_err(|e| SourceError::Parse(format!("calendar url: {e}")))?;
        url.path_segments_mut()
            .map_err(|_| SourceError::Parse("calendar url cannot be a base".into()))?
            .push(&p.calendar_id)
            .push("events");

        let resp = check_response(
            self.http
                .get(url)
                .bearer_auth(&token)
                .query(&[
                    ("timeMin", &time_min.to_string()),
                    ("timeMax", &time_max.to_string()),
                    ("singleEvents", &"true".to_string()),
                    ("orderBy", &"startTime".to_string()),
                    ("maxResults", &"50".to_string()),
                ])
                .send()
                .await?,
        )
        .await?;

        let list: EventList = resp.json().await?;
        let events = list.items.unwrap_or_default();

        tracing::info!(count = events.len(), "calendar: events found");

        let mut items = Vec::new();
        for ev in events {
            let Some(id) = ev.id else {
                continue;
            };
            let summary = ev.summary.unwrap_or_else(|| "(no title)".into());

            // Timed events carry an RFC3339 `dateTime`; all-day events carry only a
            // `date` (YYYY-MM-DD), parsed as a civil date anchored to the configured
            // timezone so the event lands on the correct vault day.
            let ts = match ev.start.as_ref().and_then(|s| {
                if let Some(dt) = s.date_time.as_deref() {
                    dt.parse::<jiff::Timestamp>().ok()
                } else if let Some(d) = s.date.as_deref() {
                    d.parse::<jiff::civil::Date>()
                        .ok()
                        .and_then(|date| date.to_zoned(ctx.timezone.clone()).ok())
                        .map(|z| z.timestamp())
                } else {
                    None
                }
            }) {
                Some(t) => t,
                None => {
                    tracing::warn!(event_id = %id, "calendar: skipping event with unparseable timestamp");
                    continue;
                }
            };

            let attendee_names: Vec<String> = ev
                .attendees
                .as_ref()
                .map(|a| {
                    a.iter()
                        .filter_map(|p| {
                            p.display_name
                                .as_deref()
                                .or(p.email.as_deref())
                                .map(String::from)
                        })
                        .collect()
                })
                .unwrap_or_default();

            // Google Calendar descriptions are HTML (`<ul>`, `<a>`, `<br>`); convert to
            // Markdown so the daily page and LLM see clean text, not raw tags.
            let description = ev
                .description
                .as_deref()
                .map(crate::markdown::html_to_markdown)
                .unwrap_or_default();
            let s = ctx.locale.strings();
            let mut body_parts: Vec<String> = Vec::new();
            if !description.is_empty() {
                body_parts.push(description.clone());
            }
            if let Some(loc) = ev.location.as_deref().filter(|l| !l.is_empty()) {
                body_parts.push(format!("{}: {loc}", s.location));
            }
            if !attendee_names.is_empty() {
                body_parts.push(format!("{}: {}", s.attendees, attendee_names.join(", ")));
            }

            // Optionally fetch meeting notes from Drive links in description + attachments.
            if p.fetch_meeting_notes {
                let mut file_ids = extract_drive_file_ids(&description);
                for att in &ev.attachments {
                    if let Some(ref fid) = att.file_id
                        && !file_ids.contains(fid)
                    {
                        file_ids.push(fid.clone());
                    }
                }
                for file_id in file_ids.into_iter().take(MAX_DRIVE_FETCHES_PER_EVENT) {
                    match fetch_drive_content(&self.http, &token, &file_id).await {
                        Ok(content) if !content.trim().is_empty() => {
                            body_parts.push(format!(
                                "**{}:**\n\n{}",
                                s.meeting_notes,
                                content.trim()
                            ));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                event_id = %id,
                                file_id = %file_id,
                                error = %e,
                                "calendar: failed to fetch meeting notes from Drive, skipping"
                            );
                        }
                    }
                }
            }

            let body = body_parts.join("\n\n");

            items.push(RawItem {
                external_id: Some(id),
                title: summary,
                body,
                url: ev.html_link,
                author: ev.organizer.and_then(|o| o.display_name.or(o.email)),
                timestamp: ts,
                metadata: serde_json::json!({
                    "status": ev.status,
                    "attendees": attendee_names,
                }),
            });
        }

        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_google_doc_ids() {
        let text = "See notes: https://docs.google.com/document/d/abc123_-X/edit and also \
                    https://drive.google.com/file/d/def456/view";
        let ids = extract_drive_file_ids(text);
        assert_eq!(ids, vec!["abc123_-X", "def456"]);
    }

    #[test]
    fn extracts_spreadsheet_and_presentation_ids() {
        let text = "Sheet: https://docs.google.com/spreadsheets/d/sheet1/edit \
                    Slides: https://docs.google.com/presentation/d/slides2/edit";
        let ids = extract_drive_file_ids(text);
        assert_eq!(ids, vec!["sheet1", "slides2"]);
    }

    #[test]
    fn no_ids_from_plain_text() {
        let ids = extract_drive_file_ids("no links here");
        assert!(ids.is_empty());
    }

    #[test]
    fn fetch_meeting_notes_defaults_to_false() {
        let params: CalendarParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!params.fetch_meeting_notes);
    }

    #[test]
    fn fetch_meeting_notes_can_be_enabled() {
        let params: CalendarParams =
            serde_json::from_value(serde_json::json!({"fetch_meeting_notes": true})).unwrap();
        assert!(params.fetch_meeting_notes);
    }
}
