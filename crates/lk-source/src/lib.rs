mod atlassian;
mod confluence;
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

/// Mint an Atlassian OAuth 2.0 (3LO) grant — refresh token + tenant — covering Jira and
/// Confluence (used by `lore init credentials`).
pub use atlassian::oauth::{
    AtlassianGrant, AtlassianSite, DEFAULT_REDIRECT_PORT as ATLASSIAN_REDIRECT_PORT, Products,
    build_grant as build_atlassian_grant,
};

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
    #[error("nothing observed: no {unit} could be read (of {attempted} tried); see warnings above")]
    NothingObserved {
        unit: &'static str,
        attempted: usize,
    },
}

/// Fail a source that reached NONE of the things it tried to read.
///
/// Adapters that fetch many independent things — RSS feeds, Drive files — isolate a
/// per-item failure so one broken item does not cost the others their day. That isolation
/// must stop short of the case where every item failed: the source's window was never
/// observed, yet an empty success is indistinguishable from a quiet day. The ingest log
/// records only that one bit, and `lore health` reads the log as its sole evidence a source
/// is alive — so a source whose every URL has moved would report fresh indefinitely, with
/// nothing else in the system able to notice.
///
/// One item reaching means the source WAS observed, however partially, and stays a success;
/// the failures are already `tracing::warn`-ed individually. `attempted == 0` is a genuinely
/// empty listing, not a failure.
///
/// It takes what was OBSERVED, never what failed. A caller computing the failures itself
/// would subtract, and a subtraction is what must not appear here: the identity it would
/// rest on is "one attempt yields at most one observation", and the day that stops holding
/// — a listing entry expanding into several items — the difference underflows a `usize`.
/// Release builds carry no overflow checks, so it would wrap to near `usize::MAX`, never
/// equal `attempted`, and this guard would silently stop firing FOREVER — reinstating the
/// exact blindness it exists to remove, while debug builds panicked about something else.
/// Counting observations upward cannot express that.
pub(crate) fn require_any_observation(
    unit: &'static str,
    observed: usize,
    attempted: usize,
) -> Result<(), SourceError> {
    if attempted > 0 && observed == 0 {
        return Err(SourceError::NothingObserved { unit, attempted });
    }
    Ok(())
}

#[cfg(test)]
mod observation_tests {
    use super::require_any_observation;

    #[test]
    fn one_observation_among_failures_is_still_an_observation() {
        assert!(require_any_observation("feed", 1, 3).is_ok());
        assert!(require_any_observation("feed", 3, 3).is_ok());
    }

    #[test]
    fn observing_none_of_what_was_attempted_is_not_an_observation() {
        let err = require_any_observation("feed", 0, 3)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no feed could be read (of 3 tried)"), "{err}");
    }

    /// Nothing to read is not the same as reading nothing — an empty folder or an empty
    /// feed list must never be reported as a failure.
    #[test]
    fn attempting_nothing_is_not_a_failure() {
        assert!(require_any_observation("listed file", 0, 0).is_ok());
    }

    /// Taking OBSERVATIONS means the input a subtraction would have produced — more
    /// results than attempts — is a plain success, not a `usize` that wrapped past
    /// `attempted` and silently disarmed the guard in release builds.
    #[test]
    fn more_observations_than_attempts_cannot_disarm_the_guard() {
        assert!(require_any_observation("listed file", 5, 3).is_ok());
    }
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
    /// Time window an adapter should query so that everything landing on `target_date`
    /// (the civil day the pipeline keeps) is covered, with optional padding.
    ///
    /// Anchored to the target day in `timezone`, never to "now", so historical backfill
    /// (`lore ingest --date YYYY-MM-DD`) fetches the right day.
    ///
    /// Both bounds land on CIVIL MIDNIGHT, with the padding rounded outward to whole days.
    /// That is not tidiness: the pipeline renders a daily page for every date a batch
    /// touches, and a complete-refetch source renders that page from the fetch alone — so a
    /// window ending mid-day would hand the pipeline a PARTIAL day and overwrite that date's
    /// existing page with the fragment. Subtracting absolute hours cannot guarantee this: on
    /// a DST fall-back day the local day is 25 hours long, so `day_start - 24h` lands an hour
    /// AFTER the previous midnight. Rounding outward keeps the window a superset while making
    /// whole days true by construction, for every adapter and every `lookback_hours` value.
    pub fn day_window(
        &self,
        lookback_hours: u32,
        lookahead_hours: u32,
    ) -> Result<(jiff::Timestamp, jiff::Timestamp), SourceError> {
        let days_back = lookback_hours.div_ceil(24) as i32;
        let days_forward = lookahead_hours.div_ceil(24) as i32;

        let midnight = |date: jiff::civil::Date| -> Result<jiff::Timestamp, SourceError> {
            date.to_zoned(self.timezone.clone())
                .map(|z| z.timestamp())
                .map_err(|e| SourceError::Parse(format!("day boundary {date}: {e}")))
        };

        let first = self
            .target_date
            .checked_sub(jiff::Span::new().days(days_back))
            .map_err(|e| SourceError::Parse(format!("window start: {e}")))?;
        // `+1` because the target day itself must be fully inside the window.
        let last = self
            .target_date
            .checked_add(jiff::Span::new().days(days_forward + 1))
            .map_err(|e| SourceError::Parse(format!("window end: {e}")))?;

        Ok((midnight(first)?, midnight(last)?))
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
        SourceType::Confluence => confluence::validate_params(params),
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

/// Build the shared HTTP client injected into every adapter (`build_source`) and the
/// OAuth token flow. Connect and read timeouts bound a connection that stops making
/// progress — reqwest has no default timeouts, so a provider that accepts the
/// connection and then stalls would otherwise hang an unattended (cron) ingest
/// indefinitely, and `retry`'s timeout branch would never fire. A total request
/// timeout is deliberately not set: a transfer that is still delivering bytes is
/// alive, and capping it would misclassify large-but-slow responses as dead.
pub fn build_http_client() -> Result<reqwest::Client, SourceError> {
    Ok(reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()?)
}

/// One shared auth provider per Atlassian instance, built once for a whole run.
///
/// Sharing is a correctness requirement, not an optimization. Atlassian rotates OAuth
/// refresh tokens: the first refresh invalidates the token it used. Two providers over one
/// instance would each hold the same starting token, so once one refreshed, the other's copy
/// would be dead and its next refresh would fail with `invalid_grant` — the Jira source
/// would silently break the Confluence source in the same run. One provider per instance
/// means one rotation chain per instance.
pub type AtlassianRegistry = std::collections::BTreeMap<String, Arc<atlassian::AtlassianAuth>>;

pub fn build_atlassian_registry(
    creds: &Credentials,
    http: &reqwest::Client,
    vault_root: &std::path::Path,
) -> AtlassianRegistry {
    creds
        .atlassian
        .iter()
        .map(|(name, ac)| {
            (
                name.clone(),
                Arc::new(atlassian::AtlassianAuth::new(
                    http.clone(),
                    name,
                    ac,
                    vault_root,
                )),
            )
        })
        .collect()
}

/// Resolve the shared provider for the instance a source asked for. Name resolution lives
/// in `Credentials` so the "one instance is unambiguous, several need naming" rule — and
/// its error text — is stated once.
fn resolve_atlassian(
    creds: &Credentials,
    registry: &AtlassianRegistry,
    instance: Option<&str>,
) -> Result<Arc<atlassian::AtlassianAuth>, SourceError> {
    let (name, _) = creds.resolve_atlassian_instance(instance)?;
    registry
        .get(name)
        .cloned()
        .ok_or_else(|| SourceError::Auth(format!("Atlassian instance `{name}` was not built")))
}

pub fn build_source(
    source_type: SourceType,
    http: reqwest::Client,
    creds: &Credentials,
    registry: &AtlassianRegistry,
    instance: Option<&str>,
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
            let auth = resolve_atlassian(creds, registry, instance)?;
            Ok(Box::new(jira::JiraSource::new(http, auth)))
        }
        SourceType::Confluence => {
            let auth = resolve_atlassian(creds, registry, instance)?;
            Ok(Box::new(confluence::ConfluenceSource::new(http, auth)))
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
    fn day_window_applies_padding_rounded_out_to_whole_days() {
        // Padding is rounded outward: a partial day in the batch would make the pipeline
        // re-render that date's page from a fragment.
        let (min, max) = ctx("2026-05-01", "UTC").day_window(24, 12).unwrap();
        assert_eq!(min.to_string(), "2026-04-30T00:00:00Z");
        assert_eq!(max.to_string(), "2026-05-03T00:00:00Z");

        // A lookback that is not a multiple of 24 still yields whole days, as a superset.
        let (min, max) = ctx("2026-05-01", "UTC").day_window(30, 0).unwrap();
        assert_eq!(min.to_string(), "2026-04-29T00:00:00Z");
        assert_eq!(max.to_string(), "2026-05-02T00:00:00Z");
    }

    #[test]
    fn day_window_bounds_are_true_midnights_across_a_dst_transition() {
        // 2026-11-01 is a 25-hour local day in America/New_York. Subtracting 24 absolute
        // hours from 11-02's midnight lands an hour INSIDE 11-01, which would hand the
        // pipeline a partial day and overwrite that date's page with the fragment.
        let tz = jiff::tz::TimeZone::get("America/New_York").unwrap();
        let (min, max) = ctx("2026-11-02", "America/New_York")
            .day_window(24, 0)
            .unwrap();

        let expect_midnight =
            |date: jiff::civil::Date| date.to_zoned(tz.clone()).unwrap().timestamp();
        assert_eq!(min, expect_midnight(jiff::civil::date(2026, 11, 1)));
        assert_eq!(max, expect_midnight(jiff::civil::date(2026, 11, 3)));
        // The hour-arithmetic result this replaces would have been 05:00Z, an hour late.
        assert_eq!(min.to_string(), "2026-11-01T04:00:00Z");
    }

    /// Every adapter, not a subset that reads like every adapter. The list omitted `Confluence`
    /// while claiming per-adapter coverage, so the one dispatch arm nothing exercised was
    /// indistinguishable from the eight that were. Driven by `SourceType::iter()`, so adding a
    /// source type forces a case rather than silently narrowing what "per adapter" means.
    #[test]
    fn validate_params_dispatch_accepts_valid_per_adapter() {
        use strum::IntoEnumIterator;

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
            (
                SourceType::Confluence,
                serde_json::json!({"cql": "space = ENG"}),
            ),
        ];
        for st in SourceType::iter() {
            let params = cases
                .iter()
                .find(|(candidate, _)| *candidate == st)
                .map(|(_, params)| params)
                .unwrap_or_else(|| {
                    panic!("{st} has no valid-params case, so its dispatch arm is unexercised")
                });
            assert!(validate_params(st, params).is_ok(), "valid {st} params");
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
