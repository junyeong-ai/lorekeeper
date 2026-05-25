use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use super::{GoogleAuth, check_response};
use crate::{ExtractContext, Source, SourceError};

const BASE: &str = "https://www.googleapis.com/calendar/v3";

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

        let items = events
            .into_iter()
            .filter_map(|ev| {
                let id = ev.id?;
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
                        return None;
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
                    body_parts.push(description);
                }
                if let Some(loc) = ev.location.as_deref().filter(|l| !l.is_empty()) {
                    body_parts.push(format!("{}: {loc}", s.location));
                }
                if !attendee_names.is_empty() {
                    body_parts.push(format!("{}: {}", s.attendees, attendee_names.join(", ")));
                }
                let body = body_parts.join("\n\n");

                Some(RawItem {
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
                })
            })
            .collect();

        Ok(items)
    }
}
