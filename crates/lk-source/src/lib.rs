pub mod credentials;
mod google;
mod jira;
mod manual;
pub(crate) mod markdown;
pub(crate) mod paging;
pub(crate) mod retry;
mod rss;
mod slack;

/// Move consumed manual-inbox files into `<inbox>/archived/{date}/`. The CLI calls
/// this only once a source's vault writes and the queue flush have succeeded, so a
/// write/flush failure leaves the inbox intact for retry.
pub use manual::archive_consumed_files;

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use lk_core::config::SourceType;
use lk_core::event::RawItem;

use credentials::Credentials;
use google::GoogleAuth;

/// Mint a Google refresh token via an interactive OAuth loopback flow (used by
/// `lore init credentials`).
pub use google::oauth::build_refresh_token as build_google_refresh_token;

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
    /// Output language for labels adapters add (status/period, thread marker, …).
    pub locale: lk_core::i18n::Locale,
    /// The configured user. Adapters compare their structured authorship fields
    /// against it to set `RawItem::is_self`.
    pub identity: lk_core::config::Identity,
    /// Anchor for user-supplied relative filesystem paths in source params (the
    /// manual source's `inbox_dir`). Resolving against it — never the process CWD —
    /// keeps scheduled runs and interactive runs reading the same directories.
    pub vault_root: std::path::PathBuf,
}

impl ExtractContext {
    /// Time window an adapter should query so that everything landing on
    /// `target_date` (the civil day the pipeline keeps) is covered, with optional
    /// padding. Anchored to the target day's bounds in `timezone` — NOT to "now" —
    /// so historical backfill (`lore ingest --date YYYY-MM-DD`) fetches the right day
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

/// Deserialize a source's untyped `params` into its typed schema. Every params
/// struct is `#[serde(deny_unknown_fields)]`, so this maps a missing key, wrong
/// type, or unknown/typo'd key to `InvalidParams` with a uniform error shape. The
/// single place JSON params become a typed struct; both `validate_params` and each
/// adapter's `extract` reach it through [`parse_validated`].
pub(crate) fn parse_params<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
) -> Result<T, SourceError> {
    serde_json::from_value(params.clone()).map_err(|e| SourceError::InvalidParams(e.to_string()))
}

/// Semantic validation for an adapter's params, beyond what `#[serde(deny_unknown_fields)]`
/// deserialization already enforces: caps `> 0`, required non-empty fields, value formats.
/// Pure — no network, credentials, or filesystem — so `lore validate` runs it offline.
pub(crate) trait ValidatedParams: serde::de::DeserializeOwned {
    fn validate(&self) -> Result<(), SourceError>;
}

/// Deserialize AND semantically validate a source's params in one step. Every adapter's
/// `validate_params` (the offline config check) and its `extract` (the runtime consumer)
/// route through this, so "params are validated before use" holds BY CONSTRUCTION — an
/// invariant can never be enforced in the config check yet skipped at consumption, and a
/// new adapter inherits the guarantee the moment it implements [`ValidatedParams`].
pub(crate) fn parse_validated<T: ValidatedParams>(
    params: &serde_json::Value,
) -> Result<T, SourceError> {
    let typed: T = parse_params(params)?;
    typed.validate()?;
    Ok(typed)
}

/// Validate a source's `params` against its adapter's typed schema without any
/// network access or credentials. Used by `lore validate` to surface config errors
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
        SourceType::Rss => rss::validate_params(params),
        SourceType::Manual => manual::validate_params(params),
    }
}

/// Build the shared Google auth helper, or a configuration error naming the
/// credentials to set. Shared by the three Google-backed source types.
fn google_auth(
    creds: &Credentials,
    http: &reqwest::Client,
) -> Result<Arc<GoogleAuth>, SourceError> {
    let gc = creds.google.as_ref().ok_or_else(|| {
        SourceError::Auth(
            "Google credentials not configured. \
             Set LORE_GOOGLE_CLIENT_ID, LORE_GOOGLE_CLIENT_SECRET, \
             LORE_GOOGLE_REFRESH_TOKEN or add to .lorekeeper/credentials.json"
                .into(),
        )
    })?;
    Ok(Arc::new(GoogleAuth::new(http.clone(), gc.clone())))
}

/// Borrow the configured Slack credentials, or a configuration error. Shared by
/// the two Slack-backed source types (each then selects the token it needs).
fn slack_creds(creds: &Credentials) -> Result<&credentials::SlackCredentials, SourceError> {
    creds.slack.as_ref().ok_or_else(|| {
        SourceError::Auth(
            "Slack credentials not configured. Set LORE_SLACK_TOKEN / \
             LORE_SLACK_USER_TOKEN or add a slack block to credentials.json"
                .into(),
        )
    })
}

pub fn build_source(
    source_type: SourceType,
    http: reqwest::Client,
    creds: &Credentials,
) -> Result<Box<dyn Source>, SourceError> {
    match source_type {
        SourceType::Gmail => {
            let auth = google_auth(creds, &http)?;
            Ok(Box::new(google::gmail::GmailSource::new(http, auth)))
        }
        SourceType::GoogleDrive => {
            let auth = google_auth(creds, &http)?;
            Ok(Box::new(google::drive::GoogleDriveSource::new(http, auth)))
        }
        SourceType::GoogleCalendar => {
            let auth = google_auth(creds, &http)?;
            Ok(Box::new(google::calendar::GoogleCalendarSource::new(
                http, auth,
            )))
        }
        SourceType::SlackChannel => {
            let token = slack_creds(creds)?
                .history_token()
                .ok_or_else(|| {
                    SourceError::Auth("slack-channel needs a bot_token or user_token".into())
                })?
                .to_string();
            Ok(Box::new(slack::channel::SlackChannelSource::new(
                http, token,
            )))
        }
        SourceType::SlackSearch => {
            let token = slack_creds(creds)?
                .search_token()
                .ok_or_else(|| {
                    SourceError::Auth(
                        "slack-search requires a user_token (xoxp-); bot tokens \
                         cannot call search.messages"
                            .into(),
                    )
                })?
                .to_string();
            Ok(Box::new(slack::search::SlackSearchSource::new(http, token)))
        }
        SourceType::Jira => {
            let jc = creds.jira.as_ref().ok_or_else(|| {
                SourceError::Auth(
                    "Jira credentials not configured. \
                     Set LORE_JIRA_URL, LORE_JIRA_EMAIL, LORE_JIRA_TOKEN \
                     or add to .lorekeeper/credentials.json"
                        .into(),
                )
            })?;
            Ok(Box::new(jira::JiraSource::new(http, jc.clone())))
        }
        // RSS feeds are public HTTP — no credentials.
        SourceType::Rss => Ok(Box::new(rss::RssSource::new(http))),
        SourceType::Manual => Ok(Box::new(manual::ManualSource::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(date: &str, tz: &str) -> ExtractContext {
        ExtractContext {
            target_date: date.parse().unwrap(),
            timezone: jiff::tz::TimeZone::get(tz).unwrap(),
            locale: lk_core::i18n::Locale::default(),
            identity: lk_core::config::Identity::default(),
            vault_root: std::path::PathBuf::new(),
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
            (
                SourceType::Gmail,
                serde_json::json!({"lookback_hours": 24, "include_queries": ["label:newsletters"]}),
            ),
            (
                SourceType::SlackChannel,
                serde_json::json!({"channel": "#x"}),
            ),
            (
                SourceType::SlackSearch,
                serde_json::json!({"queries": [{"channel": "#x", "keywords": ["a"]}]}),
            ),
            (SourceType::Jira, serde_json::json!({"jql": "x"})),
            (
                SourceType::Rss,
                serde_json::json!({"feeds": [{"id": "openai", "url": "https://openai.com/news/rss.xml"}]}),
            ),
            (
                SourceType::Manual,
                serde_json::json!({"inbox_dir": "inbox", "extensions": ["md"]}),
            ),
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
        // Gmail with no include_queries: a read-state-dependent window is rejected so the
        // complete-refetch guarantee can't be silently broken.
        assert!(validate_params(SourceType::Gmail, &serde_json::json!({})).is_err());
        // Gmail with a blank include_queries entry: it would assemble an empty `()` clause.
        assert!(
            validate_params(
                SourceType::Gmail,
                &serde_json::json!({"include_queries": [""]})
            )
            .is_err()
        );
        assert!(
            validate_params(
                SourceType::Gmail,
                &serde_json::json!({"include_queries": ["label:x", "  "]})
            )
            .is_err()
        );
    }
}
