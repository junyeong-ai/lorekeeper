use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use super::{GoogleAuth, check_response};
use crate::{ExtractContext, Source, SourceError};

const BASE: &str = "https://www.googleapis.com/calendar/v3";

/// Per-event Drive fetch budget: a calendar event normally carries one notes doc plus a
/// couple of attachments, so three covers the common case while bounding fan-out against
/// an event that links many files (each is a separate Drive round-trip). Unlike the
/// listing caps this is not a config knob — it guards per-event I/O, not window coverage —
/// but an event that overshoots it is `tracing::warn!`ed so the truncation is observable.
const MAX_DRIVE_FETCHES_PER_EVENT: usize = 3;

pub struct GoogleCalendarSource {
    http: reqwest::Client,
    auth: Arc<GoogleAuth>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoogleCalendarParams {
    #[serde(default = "default_calendar")]
    calendar_id: String,
    #[serde(default = "default_lookback")]
    lookback_hours: u32,
    #[serde(default = "default_lookahead")]
    lookahead_hours: u32,
    #[serde(default)]
    fetch_meeting_notes: bool,
    #[serde(default = "default_max_events")]
    max_events: usize,
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    crate::parse_validated::<GoogleCalendarParams>(params).map(|_| ())
}

impl crate::ValidatedParams for GoogleCalendarParams {
    fn validate(&self) -> Result<(), SourceError> {
        if self.max_events == 0 {
            return Err(SourceError::InvalidParams(
                "calendar `max_events` must be > 0".into(),
            ));
        }
        Ok(())
    }
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
fn default_max_events() -> usize {
    500
}

#[derive(Deserialize)]
struct EventList {
    items: Option<Vec<CalEvent>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
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
struct Attachment {
    title: Option<String>,
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

impl GoogleCalendarSource {
    pub fn new(http: reqwest::Client, auth: Arc<GoogleAuth>) -> Self {
        Self { http, auth }
    }
}

#[async_trait]
impl Source for GoogleCalendarSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: GoogleCalendarParams = crate::parse_validated(params)?;

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

        // Paginate to the end of the window (complete-refetch contract: the daily page is
        // re-rendered from this fetch, so a dropped event is silently lost knowledge).
        // Requested page size; the server may return fewer (or zero) per page.
        // Termination is `paging::page_step` — the rule all listing adapters share.
        const PAGE_SIZE: usize = 250;
        let page_size = PAGE_SIZE.to_string();

        let mut events: Vec<CalEvent> = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages_fetched = 0usize;

        loop {
            let mut req = self.http.get(url.clone()).bearer_auth(&token).query(&[
                ("timeMin", &time_min.to_string()),
                ("timeMax", &time_max.to_string()),
                ("singleEvents", &"true".to_string()),
                ("orderBy", &"startTime".to_string()),
                ("maxResults", &page_size),
            ]);
            if let Some(ref pt) = page_token {
                req = req.query(&[("pageToken", pt.as_str())]);
            }

            let resp = check_response(req.send().await?).await?;
            let list: EventList = resp.json().await?;

            events.extend(list.items.unwrap_or_default());
            pages_fetched += 1;

            match crate::paging::page_step(
                events.len(),
                p.max_events,
                list.next_page_token.is_some(),
                pages_fetched,
            ) {
                crate::paging::PageStep::Continue => page_token = list.next_page_token,
                crate::paging::PageStep::Stop { dropped } => {
                    events.truncate(p.max_events);
                    if dropped {
                        tracing::warn!(
                            max = p.max_events,
                            "calendar: event cap hit, some events may have been dropped; raise max_events"
                        );
                    }
                    break;
                }
                crate::paging::PageStep::Exhausted => {
                    tracing::warn!(
                        pages = crate::paging::MAX_PAGES,
                        "calendar: page budget exhausted before the window completed; results may be incomplete"
                    );
                    break;
                }
            }
        }

        tracing::info!(count = events.len(), "calendar: events found");

        let s = ctx.locale.strings();
        let me = ctx.identity.email.trim();
        let mut items = Vec::new();
        for ev in events {
            let Some(mut mapped) = map_event(ev, me, &ctx.timezone, s) else {
                continue;
            };

            // Optionally enrich the body with meeting notes fetched from the Drive files
            // referenced in the event (description links + attachments). The candidate
            // list was computed purely in `map_event`; only the fetch is I/O here.
            if p.fetch_meeting_notes {
                let linked = mapped.drive_files.len();
                if linked > MAX_DRIVE_FETCHES_PER_EVENT {
                    tracing::warn!(
                        event_id = ?mapped.item.external_id,
                        linked,
                        cap = MAX_DRIVE_FETCHES_PER_EVENT,
                        "calendar: event links more Drive files than the per-event fetch budget; the remainder are not expanded into meeting notes"
                    );
                }
                for (file_id, att_title) in mapped
                    .drive_files
                    .into_iter()
                    .take(MAX_DRIVE_FETCHES_PER_EVENT)
                {
                    match fetch_drive_content(&self.http, &token, &file_id).await {
                        Ok(content) if !content.trim().is_empty() => {
                            let header = att_title.as_deref().unwrap_or(s.meeting_notes);
                            let note = format!("**{header}:**\n\n{}", content.trim());
                            if mapped.item.body.is_empty() {
                                mapped.item.body = note;
                            } else {
                                mapped.item.body.push_str("\n\n");
                                mapped.item.body.push_str(&note);
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                event_id = ?mapped.item.external_id,
                                file_id = %file_id,
                                error = %e,
                                "calendar: failed to fetch meeting notes from Drive, skipping"
                            );
                        }
                    }
                }
            }

            items.push(mapped.item);
        }

        Ok(items)
    }
}

/// A mapped calendar event plus the Drive files it references (`(file_id, title)`),
/// extracted purely so the caller can decide whether to fetch their contents.
struct MappedEvent {
    item: RawItem,
    drive_files: Vec<(String, Option<String>)>,
}

/// Map one Calendar event to a `RawItem` (and its Drive-file candidates), or `None`
/// if it has no id or no parseable start. Pure — no I/O — so timed/all-day timestamp
/// resolution, organizer/attendee ownership, HTML→Markdown body assembly, and
/// Drive-link candidate extraction are unit-testable against fixtures. `identity_email`
/// must already be trimmed.
fn map_event(
    ev: CalEvent,
    identity_email: &str,
    timezone: &jiff::tz::TimeZone,
    s: &lk_core::i18n::Strings,
) -> Option<MappedEvent> {
    let id = ev.id?;
    let summary = ev.summary.unwrap_or_else(|| s.untitled.to_string());

    // Timed events carry an RFC3339 `dateTime`; all-day events carry only a `date`
    // (YYYY-MM-DD), parsed as a civil date anchored to the configured timezone so the
    // event lands on the correct vault day.
    let Some(ts) = ev.start.as_ref().and_then(|st| {
        if let Some(dt) = st.date_time.as_deref() {
            dt.parse::<jiff::Timestamp>().ok()
        } else if let Some(d) = st.date.as_deref() {
            d.parse::<jiff::civil::Date>()
                .ok()
                .and_then(|date| date.to_zoned(timezone.clone()).ok())
                .map(|z| z.timestamp())
        } else {
            None
        }
    }) else {
        tracing::warn!(event_id = %id, "calendar: skipping event with unparseable timestamp");
        return None;
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

    // Google Calendar descriptions are HTML (`<ul>`, `<a>`, `<br>`); convert to Markdown
    // so the daily page and LLM see clean text, not raw tags.
    let description = ev
        .description
        .as_deref()
        .map(crate::markdown::html_to_markdown)
        .unwrap_or_default();
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

    // Drive-file candidates: links in the description, then attachments (deduped).
    let mut drive_files: Vec<(String, Option<String>)> = extract_drive_file_ids(&description)
        .into_iter()
        .map(|id| (id, None))
        .collect();
    for att in &ev.attachments {
        if let Some(ref fid) = att.file_id
            && !drive_files.iter().any(|(id, _)| id == fid)
        {
            drive_files.push((fid.clone(), att.title.clone()));
        }
    }

    let is_self = !identity_email.is_empty()
        && (ev
            .organizer
            .as_ref()
            .and_then(|p| p.email.as_deref())
            .is_some_and(|e| e.eq_ignore_ascii_case(identity_email))
            || ev.attendees.as_ref().is_some_and(|a| {
                a.iter()
                    .filter_map(|p| p.email.as_deref())
                    .any(|e| e.eq_ignore_ascii_case(identity_email))
            }));

    let item = RawItem {
        external_id: Some(id),
        title: summary,
        body: body_parts.join("\n\n"),
        url: ev.html_link,
        author: ev.organizer.and_then(|o| o.display_name.or(o.email)),
        timestamp: ts,
        is_self,
        metadata: serde_json::json!({
            "status": ev.status,
            "attendees": attendee_names,
        }),
    };
    Some(MappedEvent { item, drive_files })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_from(json: serde_json::Value) -> CalEvent {
        serde_json::from_value(json).expect("event fixture parses")
    }

    #[test]
    fn map_event_timed_with_self_attendee() {
        let s = lk_core::i18n::Locale::En.strings();
        let tz = jiff::tz::TimeZone::UTC;
        let ev = event_from(serde_json::json!({
            "id": "ev1",
            "summary": "Sprint planning",
            "start": {"dateTime": "2026-05-23T09:00:00Z"},
            "description": "<p>Agenda</p>",
            "location": "Room A",
            "organizer": {"email": "boss@x.com", "displayName": "Boss"},
            "attendees": [{"email": "ME@x.com"}, {"email": "boss@x.com"}],
            "htmlLink": "https://cal/ev1"
        }));
        let m = map_event(ev, "me@x.com", &tz, s).expect("maps");
        assert!(m.item.is_self, "self is an attendee (case-insensitive)");
        assert_eq!(m.item.title, "Sprint planning");
        assert!(m.item.body.contains("Agenda"));
        assert!(m.item.body.contains("Room A"));
        assert_eq!(m.item.author.as_deref(), Some("Boss"));
    }

    #[test]
    fn map_event_ownership_is_organizer_or_attendee_exact_match() {
        let s = lk_core::i18n::Locale::En.strings();
        let tz = jiff::tz::TimeZone::UTC;

        // Organizer match alone confers ownership (no attendee list at all).
        let organized = event_from(serde_json::json!({
            "id": "ev-org",
            "summary": "1:1",
            "start": {"dateTime": "2026-05-23T09:00:00Z"},
            "organizer": {"email": "Me@x.com"}
        }));
        let m = map_event(organized, "me@x.com", &tz, s).expect("maps");
        assert!(m.item.is_self, "organizer match is ownership");

        // Neither organizer nor attendee → not self, even when the identity appears
        // in free-form text. Ownership is structured-field exact match only.
        let unrelated = event_from(serde_json::json!({
            "id": "ev-other",
            "summary": "Mentions me@x.com in the title",
            "start": {"dateTime": "2026-05-23T09:00:00Z"},
            "organizer": {"email": "boss@x.com"},
            "attendees": [{"email": "peer@x.com"}]
        }));
        let m = map_event(unrelated, "me@x.com", &tz, s).expect("maps");
        assert!(!m.item.is_self, "text mention must never confer ownership");

        // Empty configured identity never matches anything.
        let any = event_from(serde_json::json!({
            "id": "ev-any",
            "summary": "Meeting",
            "start": {"dateTime": "2026-05-23T09:00:00Z"},
            "organizer": {"email": ""}
        }));
        let m = map_event(any, "", &tz, s).expect("maps");
        assert!(!m.item.is_self, "empty identity must be a safe non-match");
    }

    #[test]
    fn map_event_all_day_resolves_to_tz_midnight() {
        let s = lk_core::i18n::Locale::En.strings();
        let tz = jiff::tz::TimeZone::get("Asia/Seoul").unwrap();
        let ev = event_from(serde_json::json!({
            "id": "ev2",
            "summary": "Holiday",
            "start": {"date": "2026-05-23"}
        }));
        let m = map_event(ev, "me@x.com", &tz, s).expect("maps");
        // 2026-05-23 00:00 KST == 2026-05-22T15:00:00Z
        assert_eq!(m.item.timestamp, "2026-05-22T15:00:00Z".parse().unwrap());
        assert!(!m.item.is_self, "no attendees/organizer → not self");
    }

    #[test]
    fn map_event_collects_drive_candidates_from_description_and_attachments() {
        let s = lk_core::i18n::Locale::En.strings();
        let tz = jiff::tz::TimeZone::UTC;
        let ev = event_from(serde_json::json!({
            "id": "ev3",
            "summary": "Review",
            "start": {"dateTime": "2026-05-23T09:00:00Z"},
            "description": "Notes: https://docs.google.com/document/d/doc123/edit",
            "attachments": [{"fileId": "att456", "title": "Deck"}]
        }));
        let m = map_event(ev, "me@x.com", &tz, s).expect("maps");
        assert_eq!(
            m.drive_files,
            vec![
                ("doc123".to_string(), None),
                ("att456".to_string(), Some("Deck".to_string()))
            ]
        );
    }

    #[test]
    fn map_event_skips_unparseable_and_idless() {
        let s = lk_core::i18n::Locale::En.strings();
        let tz = jiff::tz::TimeZone::UTC;
        // No start at all.
        let ev = event_from(serde_json::json!({"id": "ev4", "summary": "x"}));
        assert!(map_event(ev, "me@x.com", &tz, s).is_none());
        // No id.
        let ev = event_from(
            serde_json::json!({"summary": "x", "start": {"dateTime": "2026-05-23T09:00:00Z"}}),
        );
        assert!(map_event(ev, "me@x.com", &tz, s).is_none());
    }

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
        let params: GoogleCalendarParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!params.fetch_meeting_notes);
    }

    #[test]
    fn fetch_meeting_notes_can_be_enabled() {
        let params: GoogleCalendarParams =
            serde_json::from_value(serde_json::json!({"fetch_meeting_notes": true})).unwrap();
        assert!(params.fetch_meeting_notes);
    }

    #[test]
    fn max_events_defaults_and_rejects_zero() {
        let params: GoogleCalendarParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(params.max_events, 500);
        // A zero cap would silently fetch nothing; validation must refuse it up front.
        assert!(validate_params(&serde_json::json!({"max_events": 0})).is_err());
        assert!(validate_params(&serde_json::json!({"max_events": 1})).is_ok());
    }
}
