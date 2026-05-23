use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use wi_core::event::RawItem;

use super::{GoogleAuth, check_response};
use crate::{ExtractContext, Source, SourceError};

const BASE: &str = "https://www.googleapis.com/calendar/v3";

pub struct CalendarSource {
    http: reqwest::Client,
    auth: Arc<GoogleAuth>,
}

#[derive(Debug, Deserialize)]
struct CalendarParams {
    #[serde(default = "default_calendar")]
    calendar_id: String,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    #[serde(default = "default_lookahead")]
    lookahead_hours: u32,
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
        _ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: CalendarParams = serde_json::from_value(params.clone())
            .map_err(|e| SourceError::InvalidParams(e.to_string()))?;

        let token = self.auth.access_token().await?;

        let now = jiff::Timestamp::now();
        let time_min = now
            .checked_sub(jiff::SignedDuration::from_hours(p.lookback_hours.into()))
            .unwrap_or(now);
        let time_max = now
            .checked_add(jiff::SignedDuration::from_hours(p.lookahead_hours.into()))
            .unwrap_or(now);

        let resp = check_response(
            self.http
                .get(format!("{BASE}/calendars/{}/events", p.calendar_id))
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

                let ts_str = ev
                    .start
                    .as_ref()
                    .and_then(|s| s.date_time.as_deref().or(s.date.as_deref()))
                    .unwrap_or_default();
                let ts = ts_str
                    .parse::<jiff::Timestamp>()
                    .unwrap_or_else(|_| jiff::Timestamp::now());

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

                let body = format!(
                    "{}\n\nLocation: {}\nAttendees: {}",
                    ev.description.as_deref().unwrap_or_default(),
                    ev.location.as_deref().unwrap_or("N/A"),
                    if attendee_names.is_empty() {
                        "N/A".into()
                    } else {
                        attendee_names.join(", ")
                    }
                );

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
