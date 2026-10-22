mod atlassian;
mod confluence;
pub mod content;
pub mod credentials;
pub mod fetch;
mod google;
mod jira;
mod manual;
mod manual_bundle;
pub(crate) mod markdown;
mod nodex;
pub use nodex::repoint_document_links;
pub(crate) mod paging;
pub(crate) mod remote_text;
pub(crate) mod retry;
mod rss;
mod slack;
mod tasks;

/// Move consumed manual-inbox files into `<inbox>/archived/{date}/`. The CLI calls
/// this only once a source's vault writes and the queue flush have succeeded, so a
/// write/flush failure leaves the inbox intact for retry.
pub use manual::{
    ManualAdmission, ManualExtraction, ManualPublishedInput, UnprocessedInput,
    archive_consumed_files, archive_manual_originals, capture_manual,
    inbox_dir as manual_inbox_dir, validate_processing as validate_manual_processing,
};

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
    #[error(transparent)]
    Input(#[from] lk_core::input::InputError),
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth: {0}")]
    Auth(String),
    #[error("source collection identity changed; retry with the intended account and dataset")]
    CollectionIdentityChanged,
    #[error(
        "source collection identity is unavailable; check the intended account and identity-read permissions"
    )]
    CollectionIdentityUnavailable,
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error(
        "source collection exceeded its {seconds}-second acquisition budget; observation is incomplete"
    )]
    CollectionTimeout { seconds: u64 },
    #[error("response exceeds the {max_bytes}-byte acquisition limit")]
    ResponseTooLarge { max_bytes: usize },
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
/// Reaching one item permits a partial result; `Source::observe` reports its reach
/// separately. `attempted == 0` is a genuinely
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
    /// Managed output namespaces, kept separate from raw input directories.
    pub vault_dirs: lk_core::config::VaultDirs,
    pub documents: Option<lk_core::content::InputProcessingConfig>,
    pub projection_cache: Option<crate::content::ProjectionCache>,
    pub originals: lk_core::manual_original::OriginalCaptureConfig,
    /// Cumulative compact-JSON observation bytes admitted for this collection.
    /// The CLI may lower this to the run's remaining admission budget.
    pub max_input_bytes: usize,
}

/// Admit each projected observation before retaining it. Capacity exhaustion is a
/// source error, never an item-level failure or permission to return a prefix.
pub(crate) struct SourceItems {
    items: Vec<RawItem>,
    budget: lk_core::input::InputBudget,
}

impl SourceItems {
    pub(crate) fn new(ctx: &ExtractContext) -> Self {
        Self {
            items: Vec::new(),
            budget: lk_core::input::InputBudget::new(ctx.max_input_bytes),
        }
    }

    pub(crate) fn push(&mut self, item: RawItem) -> Result<(), SourceError> {
        self.budget.admit(&item)?;
        self.items.push(item);
        Ok(())
    }

    pub(crate) fn extend(
        &mut self,
        items: impl IntoIterator<Item = RawItem>,
    ) -> Result<(), SourceError> {
        for item in items {
            self.push(item)?;
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        self.items.truncate(len);
    }

    pub(crate) fn into_vec(self) -> Vec<RawItem> {
        self.items
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    DateWindow,
    DocumentSnapshot,
}

/// Temporal reach declared by the adapter that performed the query.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ObservationScope {
    /// The actual half-open query window in provider-independent timestamps.
    Window {
        start: jiff::Timestamp,
        end: jiff::Timestamp,
    },
    /// The configured query owns its temporal conditions; no date coverage is inferred.
    ConfiguredQuery,
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationOutcome {
    Observed,
    Partial,
    Unverified,
}

/// Nonsecret provider namespace and account/resource key observed for collection.
/// This is query-coverage identity, separate from an individual document's identity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "serde_json::Map<String, serde_json::Value>")]
pub struct CollectionIdentity {
    authority: String,
    principal: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionIdentityFields {
    authority: String,
    principal: String,
}

impl TryFrom<serde_json::Map<String, serde_json::Value>> for CollectionIdentity {
    type Error = SourceError;

    fn try_from(value: serde_json::Map<String, serde_json::Value>) -> Result<Self, Self::Error> {
        let fields: CollectionIdentityFields =
            serde_json::from_value(serde_json::Value::Object(value))
                .map_err(|error| SourceError::Parse(error.to_string()))?;
        Self::new(fields.authority, fields.principal)
    }
}

impl CollectionIdentity {
    pub fn new(
        authority: impl Into<String>,
        principal: impl Into<String>,
    ) -> Result<Self, SourceError> {
        let authority = authority.into();
        let principal = principal.into();
        if authority.trim().is_empty() || principal.trim().is_empty() {
            return Err(SourceError::Parse(
                "collection identity requires nonblank authority and principal".into(),
            ));
        }
        Ok(Self {
            authority,
            principal,
        })
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn principal(&self) -> &str {
        &self.principal
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SourceObservation {
    pub items: Vec<RawItem>,
    pub scope: ObservationScope,
    pub outcome: ObservationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_identity: Option<CollectionIdentity>,
}

#[async_trait]
pub trait Source: Send + Sync {
    fn selection(&self, _params: &serde_json::Value) -> Result<Selection, SourceError> {
        Ok(Selection::DateWindow)
    }

    /// Resolve the current provider namespace/principal without collecting content.
    /// `None` declares unavailable identity and cannot establish recovery coverage.
    /// Errors refuse collection, including uncertain authentication refreshes.
    async fn collection_identity(&self) -> Result<Option<CollectionIdentity>, SourceError> {
        Ok(None)
    }

    /// Bind query reach only when fresh identity checks agree around acquisition.
    /// Limited-scope credentials may still collect without a coverage binding.
    async fn observe_bound(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<SourceObservation, SourceError> {
        let before = self.collection_identity().await?;
        let mut observation = self.observe(params, ctx).await?;
        if let Some(before) = before {
            let after = self
                .collection_identity()
                .await?
                .ok_or(SourceError::CollectionIdentityUnavailable)?;
            if after != before {
                return Err(SourceError::CollectionIdentityChanged);
            }
            observation.collection_identity = Some(before);
        } else {
            observation.collection_identity = None;
        }
        Ok(observation)
    }

    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError>;

    /// An adapter opts into explicit reach reporting; legacy extraction alone does
    /// not certify a time window or complete pagination.
    async fn observe(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<SourceObservation, SourceError> {
        Ok(SourceObservation {
            items: self.extract(params, ctx).await?,
            scope: ObservationScope::Unverified,
            outcome: ObservationOutcome::Unverified,
            collection_identity: None,
        })
    }
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
        SourceType::Nodex => nodex::validate_params(params),
        SourceType::Tasks => tasks::validate_params(params),
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
/// Sharing reuses each instance's access-token cache. The credential store serializes
/// refresh and successor persistence across cooperating providers and processes.
pub type AtlassianRegistry = std::collections::BTreeMap<String, Arc<atlassian::AtlassianAuth>>;

pub fn build_atlassian_registry(
    creds: &Credentials,
    http: &reqwest::Client,
    vault_root: &std::path::Path,
) -> AtlassianRegistry {
    build_atlassian_registry_at_path(creds, http, &credentials::Credentials::path(vault_root))
}

pub fn build_atlassian_registry_with_store(
    creds: &Credentials,
    http: &reqwest::Client,
    store: &credentials::CredentialStore,
) -> AtlassianRegistry {
    build_atlassian_registry_at_path(creds, http, store.path())
}

fn build_atlassian_registry_at_path(
    creds: &Credentials,
    http: &reqwest::Client,
    path: &std::path::Path,
) -> AtlassianRegistry {
    // Existing constructors retain their per-vault behavior; configured callers supply
    // one canonical store for both initial credentials and later rotation.
    creds
        .atlassian
        .iter()
        .map(|(name, ac)| {
            let mut auth =
                atlassian::AtlassianAuth::new(http.clone(), name, ac, std::path::Path::new(""));
            auth.set_credential_path(path);
            (name.clone(), Arc::new(auth))
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

/// Whether this adapter depends on configured provider credentials.
/// Public HTTP and local sources do not use the shared credential store.
pub const fn is_credential_dependent(source_type: SourceType) -> bool {
    match source_type {
        SourceType::Gmail
        | SourceType::GoogleDrive
        | SourceType::GoogleCalendar
        | SourceType::SlackChannel
        | SourceType::SlackSearch
        | SourceType::Jira
        | SourceType::Confluence => true,
        SourceType::Rss | SourceType::Manual | SourceType::Nodex | SourceType::Tasks => false,
    }
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
        // A local repository read through its own CLI — no credentials, no network.
        SourceType::Nodex => Ok(Box::new(nodex::NodexSource)),
        SourceType::Tasks => Ok(Box::new(tasks::TasksSource)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_dependency_matches_empty_credential_construction() {
        use strum::IntoEnumIterator;

        let http = build_http_client().unwrap();
        let credentials = Credentials::default();
        let registry = AtlassianRegistry::default();
        for source_type in SourceType::iter() {
            let result = build_source(source_type, http.clone(), &credentials, &registry, None);
            assert_eq!(
                is_credential_dependent(source_type),
                result.is_err(),
                "{source_type}"
            );
        }
    }

    #[test]
    fn collection_identity_requires_named_nonblank_fields() {
        let valid = CollectionIdentity::new("provider:tenant", "principal").unwrap();
        let encoded = serde_json::to_value(&valid).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({"authority": "provider:tenant", "principal": "principal"})
        );
        assert_eq!(
            serde_json::from_value::<CollectionIdentity>(encoded).unwrap(),
            valid
        );
        assert_eq!(valid.authority(), "provider:tenant");
        assert_eq!(valid.principal(), "principal");
        for invalid in [
            serde_json::json!({"authority": "", "principal": "principal"}),
            serde_json::json!({"authority": "provider", "principal": " \t"}),
            serde_json::json!({"authority": "provider"}),
            serde_json::json!({"authority": "provider", "principal": "principal", "extra": true}),
            serde_json::json!(["provider", "principal"]),
        ] {
            assert!(
                serde_json::from_value::<CollectionIdentity>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }

    struct BoundSource {
        identities: std::sync::Mutex<
            std::collections::VecDeque<Result<Option<CollectionIdentity>, SourceError>>,
        >,
        observations: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Source for BoundSource {
        async fn collection_identity(&self) -> Result<Option<CollectionIdentity>, SourceError> {
            self.identities
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected identity lookup")
        }

        async fn extract(
            &self,
            _: &serde_json::Value,
            _: &ExtractContext,
        ) -> Result<Vec<RawItem>, SourceError> {
            unreachable!("test source supplies an observation")
        }

        async fn observe(
            &self,
            _: &serde_json::Value,
            ctx: &ExtractContext,
        ) -> Result<SourceObservation, SourceError> {
            self.observations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (start, end) = ctx.day_window(0, 0)?;
            Ok(SourceObservation {
                items: Vec::new(),
                scope: ObservationScope::Window { start, end },
                outcome: ObservationOutcome::Observed,
                collection_identity: Some(CollectionIdentity::new("untrusted", "adapter-value")?),
            })
        }
    }

    fn bound_source(
        identities: Vec<Result<Option<CollectionIdentity>, SourceError>>,
    ) -> BoundSource {
        BoundSource {
            identities: std::sync::Mutex::new(identities.into()),
            observations: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn identity(principal: &str) -> Result<Option<CollectionIdentity>, SourceError> {
        CollectionIdentity::new("provider:tenant", principal).map(Some)
    }

    #[tokio::test]
    async fn bound_observation_requires_fresh_matching_identity() {
        let source = bound_source(vec![identity("A"), identity("A")]);
        let observation = source
            .observe_bound(&serde_json::json!({}), &ctx("2026-05-01", "UTC"))
            .await
            .unwrap();
        assert_eq!(observation.collection_identity, identity("A").unwrap());
        assert_eq!(observation.outcome, ObservationOutcome::Observed);
        assert_eq!(
            source
                .observations
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(source.identities.lock().unwrap().is_empty());

        for (after, unavailable) in [(identity("B"), false), (Ok(None), true)] {
            let source = bound_source(vec![identity("A"), after]);
            let error = source
                .observe_bound(&serde_json::json!({}), &ctx("2026-05-01", "UTC"))
                .await
                .err()
                .expect("changed or unavailable identity must refuse collection");
            assert!(if unavailable {
                matches!(error, SourceError::CollectionIdentityUnavailable)
            } else {
                matches!(error, SourceError::CollectionIdentityChanged)
            });
            assert_eq!(
                source
                    .observations
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
        }
        let source = bound_source(vec![
            identity("A"),
            Err(SourceError::Auth("identity unavailable".into())),
        ]);
        assert!(matches!(
            source
                .observe_bound(&serde_json::json!({}), &ctx("2026-05-01", "UTC"))
                .await,
            Err(SourceError::Auth(_))
        ));
        assert_eq!(
            source
                .observations
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn declared_unavailable_identity_preserves_unbound_collection() {
        let source = bound_source(vec![Ok(None)]);
        let observation = source
            .observe_bound(&serde_json::json!({}), &ctx("2026-05-01", "UTC"))
            .await
            .unwrap();
        assert_eq!(observation.collection_identity, None);
        assert_eq!(observation.outcome, ObservationOutcome::Observed);
        assert!(matches!(observation.scope, ObservationScope::Window { .. }));
        assert_eq!(
            source
                .observations
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn initial_identity_error_does_not_start_collection_or_retry_authentication() {
        let source = bound_source(vec![Err(SourceError::Auth(
            "uncertain token refresh".into(),
        ))]);
        assert!(
            source
                .observe_bound(&serde_json::json!({}), &ctx("2026-05-01", "UTC"))
                .await
                .is_err()
        );
        assert_eq!(
            source
                .observations
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(source.identities.lock().unwrap().is_empty());
    }

    #[test]
    fn legacy_observation_remains_unbound() {
        let observation: SourceObservation = serde_json::from_value(serde_json::json!({"items": [], "scope": {"kind": "unverified"}, "outcome": "unverified"})).unwrap();
        assert!(observation.collection_identity.is_none());
        assert!(
            serde_json::to_value(observation)
                .unwrap()
                .get("collection_identity")
                .is_none()
        );
    }

    fn ctx(date: &str, tz: &str) -> ExtractContext {
        ExtractContext {
            target_date: date.parse().unwrap(),
            timezone: jiff::tz::TimeZone::get(tz).unwrap(),
            locale: lk_core::i18n::Locale::default(),
            identity: lk_core::config::Identity::default(),
            vault_root: std::path::PathBuf::new(),
            vault_dirs: Default::default(),
            documents: None,
            projection_cache: None,
            originals: Default::default(),
            max_input_bytes: lk_core::config::IngestInputLimits::default()
                .max_source_bytes
                .get(),
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
                serde_json::json!({"folder_id": "f"}),
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
                SourceType::Nodex,
                serde_json::json!({"repo": "/srv/projects/platform", "kinds": ["learning"]}),
            ),
            (SourceType::Tasks, serde_json::json!({})),
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
