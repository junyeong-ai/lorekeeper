pub mod credentials;
mod google;
mod jira;
mod slack;

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use wi_core::config::SourceType;
use wi_core::event::RawItem;

use credentials::Credentials;
use google::GoogleAuth;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth: {0}")]
    Auth(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("API {status}: {message}")]
    Api { status: u16, message: String },
    #[error("parse: {0}")]
    Parse(String),
}

#[derive(Debug, Clone)]
pub struct ExtractContext {
    pub target_date: jiff::civil::Date,
    pub timezone: jiff::tz::TimeZone,
}

impl ExtractContext {
    /// Time window an adapter should query so that everything landing on
    /// `target_date` (the civil day the pipeline keeps) is covered, with optional
    /// padding. Anchored to the target day's bounds in `timezone` — NOT to "now" —
    /// so historical backfill (`wi ingest --date YYYY-MM-DD`) fetches the right day
    /// instead of the last N hours from the wall clock.
    ///
    /// Returns `[day_start - lookback, day_end + lookahead]`.
    pub fn day_window(
        &self,
        lookback_hours: u32,
        lookahead_hours: u32,
    ) -> Result<(jiff::Timestamp, jiff::Timestamp), SourceError> {
        let day_start = self
            .target_date
            .to_zoned(self.timezone.clone())
            .map_err(|e| SourceError::Parse(format!("day start: {e}")))?;
        let day_end = self
            .target_date
            .tomorrow()
            .and_then(|d| d.to_zoned(self.timezone.clone()))
            .map_err(|e| SourceError::Parse(format!("day end: {e}")))?;
        let min = day_start
            .timestamp()
            .checked_sub(jiff::SignedDuration::from_hours(lookback_hours.into()))
            .map_err(|e| SourceError::Parse(format!("window min: {e}")))?;
        let max = day_end
            .timestamp()
            .checked_add(jiff::SignedDuration::from_hours(lookahead_hours.into()))
            .map_err(|e| SourceError::Parse(format!("window max: {e}")))?;
        Ok((min, max))
    }
}

#[async_trait]
pub trait Source: Send + Sync {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError>;
}

/// Validate a source's `params` against its adapter's typed schema without any
/// network access or credentials. Used by `wi validate` to surface config errors
/// (missing required keys, wrong types, unknown/typo'd keys) before runtime.
pub fn validate_params(
    source_type: SourceType,
    params: &serde_json::Value,
) -> Result<(), SourceError> {
    match source_type {
        SourceType::Gmail => google::gmail::validate_params(params),
        SourceType::GoogleDrive => google::drive::validate_params(params),
        SourceType::GoogleCalendar => google::calendar::validate_params(params),
        SourceType::SlackChannel => slack::channel::validate_params(params),
        SourceType::SlackSearch => slack::search::validate_params(params),
        SourceType::Jira => jira::validate_params(params),
    }
}

pub fn create_source(
    source_type: SourceType,
    http: reqwest::Client,
    creds: &Credentials,
) -> Result<Box<dyn Source>, SourceError> {
    match source_type {
        SourceType::Gmail | SourceType::GoogleDrive | SourceType::GoogleCalendar => {
            let gc = creds.google.as_ref().ok_or_else(|| {
                SourceError::Auth(
                    "Google credentials not configured. \
                     Set WI_GOOGLE_CLIENT_ID, WI_GOOGLE_CLIENT_SECRET, \
                     WI_GOOGLE_REFRESH_TOKEN or add to .wiki-ingest/credentials.json"
                        .into(),
                )
            })?;
            let auth = Arc::new(GoogleAuth::new(http.clone(), gc.clone()));
            match source_type {
                SourceType::Gmail => Ok(Box::new(google::gmail::GmailSource::new(http, auth))),
                SourceType::GoogleDrive => {
                    Ok(Box::new(google::drive::DriveSource::new(http, auth)))
                }
                SourceType::GoogleCalendar => {
                    Ok(Box::new(google::calendar::CalendarSource::new(http, auth)))
                }
                _ => unreachable!(),
            }
        }
        SourceType::SlackChannel | SourceType::SlackSearch => {
            let sc = creds.slack.as_ref().ok_or_else(|| {
                SourceError::Auth(
                    "Slack credentials not configured. Set WI_SLACK_TOKEN / \
                     WI_SLACK_USER_TOKEN or add a slack block to credentials.json"
                        .into(),
                )
            })?;
            match source_type {
                SourceType::SlackChannel => {
                    let token = sc.history_token().ok_or_else(|| {
                        SourceError::Auth("slack-channel needs a bot_token or user_token".into())
                    })?;
                    Ok(Box::new(slack::channel::SlackChannelSource::new(
                        http,
                        token.to_string(),
                    )))
                }
                SourceType::SlackSearch => {
                    let token = sc.search_token().ok_or_else(|| {
                        SourceError::Auth(
                            "slack-search requires a user_token (xoxp-); bot tokens \
                             cannot call search.messages"
                                .into(),
                        )
                    })?;
                    Ok(Box::new(slack::search::SlackSearchSource::new(
                        http,
                        token.to_string(),
                    )))
                }
                _ => unreachable!(),
            }
        }
        SourceType::Jira => {
            let jc = creds.jira.as_ref().ok_or_else(|| {
                SourceError::Auth(
                    "Jira credentials not configured. \
                     Set WI_JIRA_URL, WI_JIRA_EMAIL, WI_JIRA_TOKEN \
                     or add to .wiki-ingest/credentials.json"
                        .into(),
                )
            })?;
            Ok(Box::new(jira::JiraSource::new(http, jc.clone())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(date: &str, tz: &str) -> ExtractContext {
        ExtractContext {
            target_date: date.parse().unwrap(),
            timezone: jiff::tz::TimeZone::get(tz).unwrap(),
        }
    }

    #[test]
    fn day_window_anchors_to_target_date_not_now() {
        // 2026-05-01 in Asia/Seoul (UTC+9): day starts 2026-04-30T15:00:00Z.
        let (min, max) = ctx("2026-05-01", "Asia/Seoul").day_window(0, 0).unwrap();
        assert_eq!(min.to_string(), "2026-04-30T15:00:00Z");
        assert_eq!(max.to_string(), "2026-05-01T15:00:00Z");
    }

    #[test]
    fn day_window_applies_padding() {
        let (min, max) = ctx("2026-05-01", "UTC").day_window(24, 12).unwrap();
        // day [05-01T00:00, 05-02T00:00) padded by -24h / +12h.
        assert_eq!(min.to_string(), "2026-04-30T00:00:00Z");
        assert_eq!(max.to_string(), "2026-05-02T12:00:00Z");
    }

    #[test]
    fn validate_params_dispatch_accepts_valid_per_adapter() {
        let cases = [
            (
                SourceType::GoogleDrive,
                serde_json::json!({"folder": "f", "file_pattern": "p-{date}.md"}),
            ),
            (SourceType::GoogleCalendar, serde_json::json!({})),
            (SourceType::Gmail, serde_json::json!({"lookback_hours": 24})),
            (
                SourceType::SlackChannel,
                serde_json::json!({"channel": "#x"}),
            ),
            (
                SourceType::SlackSearch,
                serde_json::json!({"queries": [{"channel": "#x", "keywords": ["a"]}]}),
            ),
            (SourceType::Jira, serde_json::json!({"jql": "x"})),
        ];
        for (st, params) in cases {
            assert!(validate_params(st, &params).is_ok(), "valid {st} params");
        }
    }

    #[test]
    fn validate_params_dispatch_rejects_bad_per_adapter() {
        // Missing required field, wrong type, and unknown key respectively.
        assert!(
            validate_params(SourceType::GoogleDrive, &serde_json::json!({"folder": "f"})).is_err()
        );
        assert!(
            validate_params(SourceType::SlackChannel, &serde_json::json!({"channel": 7})).is_err()
        );
        assert!(
            validate_params(
                SourceType::Jira,
                &serde_json::json!({"jql": "x", "typo": 1})
            )
            .is_err()
        );
    }
}
