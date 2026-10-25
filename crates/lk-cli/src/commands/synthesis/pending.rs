//! Durable closed-period work. A calendar trigger records intent; readiness is
//! checked again against collection responses and the exact semantic inputs.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use jiff::civil::Date;
use lk_core::config::{Config, SourceType};
use lk_core::frontmatter::field;
use lk_core::vault_path::{VaultPath, work_log_dir};
use lk_pipeline::{PipelineContext, RenderResult, Synthesizer};
use lk_queue::TargetKind;
use lk_vault::{FsVault, MutationPlan, MutationSession, VaultPage, VaultStore};
use serde::{Deserialize, Serialize};

const STATE_PATH: &str = ".lorekeeper/periods/state.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ResumeStage {
    Base,
    Quarter,
    Annual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    WeeklyThemes,
    WeeklyReview,
    Monthly,
    Quarterly,
    Annual,
}

impl Kind {
    fn stage(self) -> ResumeStage {
        match self {
            Self::WeeklyThemes | Self::WeeklyReview | Self::Monthly => ResumeStage::Base,
            Self::Quarterly => ResumeStage::Quarter,
            Self::Annual => ResumeStage::Annual,
        }
    }

    fn target_kind(self) -> TargetKind {
        match self {
            Self::WeeklyThemes => TargetKind::WeeklySynthesisThemes,
            Self::WeeklyReview => TargetKind::WeeklyReviewNarrative,
            Self::Monthly => TargetKind::MonthlyReviewNarrative,
            Self::Quarterly => TargetKind::QuarterlyReviewNarrative,
            Self::Annual => TargetKind::AnnualReviewNarrative,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosedPeriod {
    pub kind: Kind,
    pub start: Date,
    pub end: Date,
}

impl ClosedPeriod {
    pub fn containing(kind: Kind, date: Date) -> miette::Result<Self> {
        let range = match kind {
            Kind::WeeklyThemes | Kind::WeeklyReview => {
                let week = date.iso_week_date();
                lk_pipeline::iso_week_range(week.year(), week.week() as u8)
            }
            Kind::Monthly => lk_pipeline::month_range(date.year(), date.month() as u8),
            Kind::Quarterly => lk_pipeline::quarter_range(
                date.year(),
                lk_core::vault_path::quarter_of_month(date.month() as u8),
            ),
            Kind::Annual => {
                let (start, _) = lk_pipeline::month_range(date.year(), 1)
                    .map_err(|e| miette::miette!("period: {e}"))?;
                let (_, end) = lk_pipeline::month_range(date.year(), 12)
                    .map_err(|e| miette::miette!("period: {e}"))?;
                return Ok(Self { kind, start, end });
            }
        }
        .map_err(|e| miette::miette!("period: {e}"))?;
        Ok(Self {
            kind,
            start: range.0,
            end: range.1,
        })
    }

    fn key(&self) -> String {
        let kind = match self.kind {
            Kind::WeeklyThemes => "weekly-themes",
            Kind::WeeklyReview => "weekly-review",
            Kind::Monthly => "monthly",
            Kind::Quarterly => "quarterly",
            Kind::Annual => "annual",
        };
        format!("{kind}:{}", self.start)
    }

    fn validate(&self) -> miette::Result<()> {
        if Self::containing(self.kind, self.start)? != *self {
            return Err(miette::miette!(
                "invalid stored closed period: {}",
                self.key()
            ));
        }
        Ok(())
    }

    fn matches_page(&self, page: &VaultPage) -> bool {
        let (format, period) = match self.kind {
            Kind::WeeklyThemes | Kind::WeeklyReview => {
                let iso = self.start.iso_week_date();
                (
                    if self.kind == Kind::WeeklyThemes {
                        "weekly-synthesis"
                    } else {
                        "weekly-review"
                    },
                    format!("{}-W{:02}", iso.year(), iso.week()),
                )
            }
            Kind::Monthly => (
                "monthly-review",
                format!("{}-{:02}", self.start.year(), self.start.month()),
            ),
            Kind::Quarterly => (
                "quarterly-review",
                format!(
                    "{}-Q{}",
                    self.start.year(),
                    lk_core::vault_path::quarter_of_month(self.start.month() as u8)
                ),
            ),
            Kind::Annual => ("annual-review", self.start.year().to_string()),
        };
        page.frontmatter
            .get("type")
            .and_then(serde_json::Value::as_str)
            == Some(format)
            && page
                .frontmatter
                .get("period")
                .and_then(serde_json::Value::as_str)
                == Some(period.as_str())
    }

    fn path(&self, config: &Config) -> VaultPath {
        let dirs = &config.vault.dirs;
        let year = self.start.year();
        match self.kind {
            Kind::WeeklyThemes | Kind::WeeklyReview => {
                let iso = self.start.iso_week_date();
                if self.kind == Kind::WeeklyThemes {
                    VaultPath::weekly_synthesis(dirs, iso.year(), iso.week() as u8)
                } else {
                    VaultPath::weekly_review(dirs, iso.year(), iso.week() as u8)
                }
            }
            Kind::Monthly => VaultPath::monthly_review(dirs, year, self.start.month() as u8),
            Kind::Quarterly => VaultPath::quarterly_review(
                dirs,
                year,
                lk_core::vault_path::quarter_of_month(self.start.month() as u8),
            ),
            Kind::Annual => VaultPath::annual_review(dirs, year),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResponseOutcome {
    Observed,
    Partial,
    Unverified,
    Failed,
}

/// Evidence supplied by the adapter's actual typed observation path. No window
/// is reconstructed from a JSON parameter or inferred from the event count.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedSource {
    pub source_id: String,
    pub scope: ObservedScope,
    pub outcome: ResponseOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_identity: Option<lk_source::CollectionIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personal_inputs: Option<lk_pipeline::WorkLogInputs>,
}

impl ObservedSource {
    pub fn from_observation(source_id: &str, observation: &lk_source::SourceObservation) -> Self {
        Self {
            source_id: source_id.into(),
            personal_inputs: None,
            collection_identity: observation.collection_identity.clone(),
            scope: match &observation.scope {
                lk_source::ObservationScope::Window { start, end } => ObservedScope::Window {
                    start: *start,
                    end: *end,
                },
                lk_source::ObservationScope::ConfiguredQuery => ObservedScope::ConfiguredQuery,
                lk_source::ObservationScope::Unverified => ObservedScope::Unverified,
            },
            outcome: match observation.outcome {
                lk_source::ObservationOutcome::Observed => ResponseOutcome::Observed,
                lk_source::ObservationOutcome::Partial => ResponseOutcome::Partial,
                lk_source::ObservationOutcome::Unverified => ResponseOutcome::Unverified,
            },
        }
    }

    pub fn failed(source_id: &str) -> Self {
        Self {
            source_id: source_id.into(),
            personal_inputs: None,
            collection_identity: None,
            scope: ObservedScope::Unverified,
            outcome: ResponseOutcome::Failed,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ObservedScope {
    Window {
        start: jiff::Timestamp,
        end: jiff::Timestamp,
    },
    ConfiguredQuery,
    Unverified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionBatch {
    at: jiff::Timestamp,
    #[serde(default)]
    started_at: Option<jiff::Timestamp>,
    #[serde(default)]
    targeted: bool,
    requested_day: Date,
    full_ingest: bool,
    sources: Vec<ObservedSource>,
    source_scopes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    work_logs: BTreeMap<Date, String>,
}

pub struct CollectionPublication {
    pub requested_day: Date,
    pub full_ingest: bool,
    pub started_at: jiff::Timestamp,
    pub targeted: bool,
    pub work_logs: BTreeMap<Date, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Status {
    Pending,
    Blocked,
    AwaitingSemantics,
    Complete,
    RetainedComplete,
    NoData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputPage {
    path: String,
    revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    period: ClosedPeriod,
    timezone: String,
    status: Status,
    reason: Option<String>,
    scope: String,
    inputs: Vec<InputPage>,
    input_scope: String,
    target_input: Option<String>,
    retained: bool,
}

impl Intent {
    fn new(period: ClosedPeriod, config: &Config) -> miette::Result<Self> {
        Ok(Self {
            period,
            timezone: timezone_identity(config)?,
            status: Status::Pending,
            reason: None,
            scope: "Retained pages in the exact interval; earlier history coverage is unverified. Closing-day readiness checks configured adapter responses, not worldwide provider completeness.".into(),
            inputs: vec![],
            input_scope: String::new(),
            target_input: None,
            retained: false,
        })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    intents: BTreeMap<String, Intent>,
    batches: Vec<CollectionBatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery: Option<RecoveryCursor>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryCursor {
    start_date: Date,
    timezone: String,
    next: BTreeMap<Kind, Date>,
    next_kind: Option<Kind>,
}

#[derive(Debug, Default, Serialize)]
struct RecoveryResult {
    examined: usize,
    requested: Vec<String>,
    remaining: bool,
}

fn enabled_kinds(config: &Config) -> Vec<Kind> {
    let mut kinds = Vec::new();
    if config.synthesis.weekly.enabled {
        if !config.synthesis.weekly.include_sources.is_empty() {
            kinds.push(Kind::WeeklyThemes);
        }
        if config.personal.is_some() {
            kinds.push(Kind::WeeklyReview);
        }
    }
    if let Some(personal) = &config.personal {
        if personal.monthly.enabled {
            kinds.push(Kind::Monthly);
        }
        if personal.quarterly.enabled {
            kinds.push(Kind::Quarterly);
        }
        if personal.annual.enabled {
            kinds.push(Kind::Annual);
        }
    }
    kinds
}

fn next_period(period: &ClosedPeriod) -> miette::Result<Date> {
    period
        .end
        .checked_add(jiff::Span::new().days(1))
        .map_err(|error| miette::miette!("next closed period: {error}"))
}

fn reconcile_periods(
    state: &mut State,
    config: &Config,
    today: Date,
) -> miette::Result<RecoveryResult> {
    let recovery = config.synthesis.recovery.as_ref().ok_or_else(|| {
        miette::miette!("configure synthesis.recovery.start_date before reconciling closed periods")
    })?;
    let timezone = timezone_identity(config)?;
    if state.recovery.as_ref().is_none_or(|cursor| {
        cursor.start_date != recovery.start_date || cursor.timezone != timezone
    }) {
        state.recovery = Some(RecoveryCursor {
            start_date: recovery.start_date,
            timezone,
            next: BTreeMap::new(),
            next_kind: None,
        });
    }
    let kinds = enabled_kinds(config);
    let cursor = state
        .recovery
        .as_mut()
        .expect("recovery cursor initialized");
    for kind in &kinds {
        if !cursor.next.contains_key(kind) {
            let period = ClosedPeriod::containing(*kind, recovery.start_date)?;
            let first = if period.start < recovery.start_date {
                next_period(&period)?
            } else {
                period.start
            };
            cursor.next.insert(*kind, first);
        }
    }
    let mut result = RecoveryResult::default();
    while result.examined < recovery.max_periods_per_run.get() {
        let cursor = state
            .recovery
            .as_ref()
            .expect("recovery cursor initialized");
        let start = cursor
            .next_kind
            .and_then(|next| kinds.iter().position(|kind| *kind >= next))
            .unwrap_or(0);
        let mut selected = None;
        for kind in kinds.iter().cycle().skip(start).take(kinds.len()) {
            let period = ClosedPeriod::containing(*kind, cursor.next[kind])?;
            if period.end < today {
                selected = Some(period);
                break;
            }
        }
        let Some(period) = selected else {
            break;
        };
        let cursor = state
            .recovery
            .as_mut()
            .expect("recovery cursor initialized");
        cursor.next.insert(period.kind, next_period(&period)?);
        let position = kinds
            .iter()
            .position(|kind| *kind == period.kind)
            .expect("selected enabled kind");
        cursor.next_kind = Some(kinds[(position + 1) % kinds.len()]);
        result.examined += 1;
        let key = period.key();
        if !state.intents.contains_key(&key) {
            request(state, period, config, today)?;
            result.requested.push(key);
        }
    }
    let cursor = state
        .recovery
        .as_ref()
        .expect("recovery cursor initialized");
    for kind in kinds {
        if ClosedPeriod::containing(kind, cursor.next[&kind])?.end < today {
            result.remaining = true;
            break;
        }
    }
    Ok(result)
}

pub fn reconcile(
    config: &Config,
    session: &MutationSession,
    today: Date,
    json: bool,
) -> miette::Result<()> {
    let root = session.root().to_path_buf();
    let (mut state, before) = load(&root)?;
    let result = reconcile_periods(&mut state, config, today)?;
    save(&root, session, &state, before)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result)
                .map_err(|error| miette::miette!("period recovery report: {error}"))?
        );
    } else {
        eprintln!(
            "Closed-period recovery: examined {}, requested {}, remaining={}; source coverage is checked separately before processing",
            result.examined,
            result.requested.len(),
            result.remaining
        );
    }
    Ok(())
}

fn load(root: &Path) -> miette::Result<(State, Option<String>)> {
    let path = lk_vault::page_path(root, Path::new(STATE_PATH))
        .map_err(|e| miette::miette!("period state path: {e}"))?;
    let original = match std::fs::read_to_string(path) {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                State {
                    version: 2,
                    ..Default::default()
                },
                None,
            ));
        }
        Err(e) => return Err(miette::miette!("read period state: {e}")),
    };
    let mut state: State =
        serde_json::from_str(&original).map_err(|e| miette::miette!("decode period state: {e}"))?;
    if !matches!(state.version, 1 | 2) {
        return Err(miette::miette!("unsupported period state version"));
    }
    state.version = 2;
    for (key, intent) in &state.intents {
        intent.period.validate()?;
        if *key != intent.period.key() {
            return Err(miette::miette!("period state key mismatch"));
        }
    }
    if let Some(cursor) = &state.recovery {
        for (kind, next) in &cursor.next {
            if *next < cursor.start_date || ClosedPeriod::containing(*kind, *next)?.start != *next {
                return Err(miette::miette!("invalid stored period recovery cursor"));
            }
        }
    }
    Ok((state, Some(original)))
}

fn save(
    root: &Path,
    session: &MutationSession,
    state: &State,
    before: Option<String>,
) -> miette::Result<()> {
    let body = serde_json::to_string_pretty(state)
        .map_err(|e| miette::miette!("encode period state: {e}"))?
        + "\n";
    let mut plan = MutationPlan::default();
    plan.publish_expected(root, STATE_PATH, before, body)
        .and_then(|()| session.apply(plan))
        .map_err(|e| miette::miette!("publish period state: {e}"))
}

fn timezone_identity(config: &Config) -> miette::Result<String> {
    #[derive(Serialize)]
    struct Zone {
        #[serde(with = "jiff::fmt::serde::tz::required")]
        zone: jiff::tz::TimeZone,
    }
    serde_json::to_string(&Zone {
        zone: config.vault.timezone(),
    })
    .map_err(|error| {
        miette::miette!(
            "durable period timezone cannot be serialized; configure a named IANA zone: {error}"
        )
    })
}

fn digest(value: serde_json::Value) -> String {
    blake3::hash(value.to_string().as_bytes())
        .to_hex()
        .to_string()
}

fn input_scope(config: &Config, period: &ClosedPeriod) -> miette::Result<String> {
    let sources = required_sources(config, period.kind)
        .iter()
        .map(|id| Ok((id.clone(), source_scope(config, id)?)))
        .collect::<miette::Result<BTreeMap<_, _>>>()?;
    let personal = config.personal.as_ref().map(|personal| {
        serde_json::json!({
            "tracked_sources": personal.tracked_sources,
            "performance_categories": personal.performance_categories,
            "source_category_map": personal.source_category_map,
            "source_type_category_map": personal.source_type_category_map,
            "uncategorized_label": personal.uncategorized_label,
            "monthly": personal.monthly.enabled,
            "quarterly": personal.quarterly.enabled,
            "annual": personal.annual.enabled,
        })
    });
    let dirs = &config.vault.dirs;
    Ok(digest(serde_json::json!({
        "format": 1, "period": period, "sources": sources, "personal": personal,
        "weekly_enabled": config.synthesis.weekly.enabled,
        "include_sources": config.synthesis.weekly.include_sources,
        "locale": config.vault.locale().tag(),
        "dirs": { "daily": dirs.daily, "weekly": dirs.weekly, "monthly": dirs.monthly,
            "quarterly": dirs.quarterly, "annual": dirs.annual, "personal": dirs.personal,
            "synthesis": dirs.synthesis, "wiki": dirs.wiki },
    })))
}

fn source_scope(config: &Config, id: &str) -> miette::Result<String> {
    // Hash only typed selection values; no identity/configuration payload is persisted.
    let source = config.sources.get(id).map(|source| {
        serde_json::json!({
            "type": source.source_type, "instance": source.instance,
            "enabled": source.enabled, "params": source.params, "classify": source.classify,
            "labels": source.labels, "concept_policy": source.concept_policy,
            "focus": source.normalized_focus(),
            "highlights": source.highlights.iter().map(|highlight| serde_json::json!({
                "category": highlight.category, "label": highlight.label,
            })).collect::<Vec<_>>(),
        })
    });
    Ok(digest(serde_json::json!({
        "format": 1, "source": source, "timezone": timezone_identity(config)?,
        "identity": { "name": config.identity.name, "email": config.identity.email,
            "slack_id": config.identity.slack_id },
    })))
}

/// Called only after page publication and queue flush succeeded, under ingest's
/// existing mutation session. A failed source is an outcome, never empty data.
pub fn record_collection(
    config: &Config,
    session: &MutationSession,
    publication: CollectionPublication,
    sources: Vec<ObservedSource>,
) -> miette::Result<()> {
    let CollectionPublication {
        requested_day,
        full_ingest,
        started_at,
        targeted,
        work_logs,
    } = publication;
    let root = session.root().to_path_buf();
    let (mut state, before) = load(&root)?;
    let batch = CollectionBatch {
        at: jiff::Timestamp::now(),
        started_at: Some(started_at),
        targeted,
        requested_day,
        full_ingest,
        work_logs,
        source_scopes: sources
            .iter()
            .map(|s| Ok((s.source_id.clone(), source_scope(config, &s.source_id)?)))
            .collect::<miette::Result<BTreeMap<_, _>>>()?,
        sources,
    };
    let scopes = current_source_scopes(config)?;
    let identities = collection_identities(&batch.sources);
    let published = published_windows(&batch, config, &scopes)?;
    let mut retained = Vec::with_capacity(state.batches.len() + 1);
    for old in state.batches {
        let same_selection = old.requested_day == batch.requested_day
            && old.full_ingest == batch.full_ingest
            && old.source_scopes.keys().eq(batch.source_scopes.keys());
        let old_identities = collection_identities(&old.sources);
        let old_published = published_windows(&old, config, &scopes)?;
        let unpublished_work_logs = old
            .work_logs
            .keys()
            .any(|day| !batch.work_logs.contains_key(day));
        let retained_personal_days = old.sources.iter().any(|source| {
            source.personal_inputs.as_ref().is_some_and(|inputs| {
                inputs.days.keys().any(|day| {
                    !batch.sources.iter().any(|new_source| {
                        new_source.source_id == source.source_id
                            && new_source.outcome == ResponseOutcome::Observed
                            && new_source.personal_inputs.is_some()
                            && (new_source
                                .personal_inputs
                                .as_ref()
                                .is_some_and(|inputs| inputs.days.contains_key(day))
                                || published.iter().any(|(id, window)| {
                                    id == &source.source_id
                                        && old_identities.get(id) == identities.get(id)
                                        && window.start <= *day
                                        && window.end > *day
                                }))
                    })
                })
            })
        });
        if !same_selection
            || unpublished_work_logs
            || retained_personal_days
            || old_published.iter().any(|(id, old_window)| {
                !published.iter().any(|(new_id, new_window)| {
                    id == new_id
                        && old_identities.get(id) == identities.get(new_id)
                        && new_window.start <= old_window.start
                        && new_window.end >= old_window.end
                })
            })
        {
            retained.push(old);
        }
    }
    state.batches = retained;
    state.batches.push(batch);
    save(&root, session, &state, before)
}

/// Complete civil days selected by an observed provider query, end-exclusive.
/// This establishes query reach, not historical provider revision completeness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectionWindow {
    pub start: Date,
    pub end: Date,
}

fn current_source_scopes(config: &Config) -> miette::Result<BTreeMap<String, String>> {
    config
        .enabled_sources()
        .map(|(id, _)| Ok((id.to_owned(), source_scope(config, id)?)))
        .collect()
}

fn collection_identities(
    sources: &[ObservedSource],
) -> BTreeMap<String, lk_source::CollectionIdentity> {
    sources
        .iter()
        .filter_map(|source| {
            source
                .collection_identity
                .as_ref()
                .map(|identity| (source.source_id.clone(), identity.clone()))
        })
        .collect()
}

fn accepted_windows(
    batch: &CollectionBatch,
    config: &Config,
    scopes: &BTreeMap<String, String>,
    identities: &BTreeMap<String, lk_source::CollectionIdentity>,
) -> miette::Result<Vec<(String, CollectionWindow)>> {
    let recorded = collection_identities(&batch.sources);
    Ok(published_windows(batch, config, scopes)?
        .into_iter()
        .filter(|(id, _)| {
            recorded
                .get(id)
                .is_some_and(|identity| identities.get(id) == Some(identity))
        })
        .collect())
}

/// Post-close retained publications, without asserting the current provider account.
/// Period synthesis reads these retained inputs; automatic replay also requires binding.
fn published_windows(
    batch: &CollectionBatch,
    config: &Config,
    scopes: &BTreeMap<String, String>,
) -> miette::Result<Vec<(String, CollectionWindow)>> {
    let Some(started_at) = batch.started_at.filter(|started| *started <= batch.at) else {
        return Ok(vec![]);
    };
    let tz = config.vault.timezone();
    let closed_end = started_at
        .to_zoned(tz.clone())
        .date()
        .to_zoned(tz.clone())
        .map_err(|e| miette::miette!("collection coverage timezone: {e}"))?
        .timestamp();
    let mut accepted = Vec::new();
    for source in &batch.sources {
        if source.outcome != ResponseOutcome::Observed
            || scopes.get(&source.source_id).is_none()
            || batch.source_scopes.get(&source.source_id) != scopes.get(&source.source_id)
        {
            continue;
        }
        let ObservedScope::Window { start, end } = source.scope else {
            continue;
        };
        let end = end.min(closed_end);
        if start >= end {
            continue;
        }
        let mut first = start.to_zoned(tz.clone()).date();
        let midnight = first
            .to_zoned(tz.clone())
            .map_err(|e| miette::miette!("collection coverage start: {e}"))?
            .timestamp();
        if start > midnight {
            first = first
                .tomorrow()
                .map_err(|e| miette::miette!("collection coverage date: {e}"))?;
        }
        let mut last = end.to_zoned(tz.clone()).date();
        if batch.targeted {
            first = first.max(batch.requested_day);
            last = last.min(
                batch
                    .requested_day
                    .tomorrow()
                    .map_err(|e| miette::miette!("collection target date: {e}"))?,
            );
        }
        if first < last {
            accepted.push((
                source.source_id.clone(),
                CollectionWindow {
                    start: first,
                    end: last,
                },
            ));
        }
    }
    Ok(accepted)
}

/// Load once per recovery run; accepted intervals use the exact current source
/// selection and were already closed when acquisition began. Legacy batches
/// without acquisition time cannot establish this coverage.
pub fn source_coverage(
    config: &Config,
    identities: &BTreeMap<String, lk_source::CollectionIdentity>,
) -> miette::Result<BTreeMap<String, Vec<CollectionWindow>>> {
    let (state, _) = load(&config.vault.root_path())?;
    let scopes = current_source_scopes(config)?;
    let mut windows: BTreeMap<String, Vec<CollectionWindow>> = BTreeMap::new();
    for batch in &state.batches {
        for (id, window) in accepted_windows(batch, config, &scopes, identities)? {
            windows.entry(id).or_default().push(window);
        }
    }
    for values in windows.values_mut() {
        values.sort_by_key(|window| (window.start, window.end));
        let mut merged: Vec<CollectionWindow> = Vec::with_capacity(values.len());
        for window in values.drain(..) {
            if let Some(last) = merged.last_mut()
                && window.start <= last.end
            {
                last.end = last.end.max(window.end);
            } else {
                merged.push(window);
            }
        }
        *values = merged;
    }
    Ok(windows)
}

#[cfg(test)]
mod collection_coverage_tests {
    use super::*;
    use jiff::civil::date;

    fn identities() -> BTreeMap<String, lk_source::CollectionIdentity> {
        ["mail", "other"]
            .into_iter()
            .map(|id| {
                (
                    id.into(),
                    lk_source::CollectionIdentity::new("mail.example.invalid", id).unwrap(),
                )
            })
            .collect()
    }

    fn config() -> (tempfile::TempDir, Config) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, "vault: {root: vault, timezone: UTC}\nidentity: {name: Test, email: test@example.invalid}\nsources:\n  mail: {type: gmail}\n  other: {type: gmail}\n").unwrap();
        (temp, Config::load(&path).unwrap())
    }

    fn observed(id: &str, start: &str, end: &str) -> ObservedSource {
        ObservedSource {
            source_id: id.into(),
            personal_inputs: None,
            collection_identity: identities().remove(id),
            scope: ObservedScope::Window {
                start: start.parse().unwrap(),
                end: end.parse().unwrap(),
            },
            outcome: ResponseOutcome::Observed,
        }
    }

    fn receipt(config: &Config, started_at: &str) -> CollectionBatch {
        CollectionBatch {
            work_logs: BTreeMap::new(),
            at: "2026-10-20T09:00:00Z".parse().unwrap(),
            started_at: Some(started_at.parse().unwrap()),
            targeted: false,
            requested_day: date(2026, 10, 19),
            full_ingest: true,
            sources: vec![observed(
                "mail",
                "2026-10-17T00:00:00Z",
                "2026-10-21T00:00:00Z",
            )],
            source_scopes: current_source_scopes(config).unwrap(),
        }
    }

    #[test]
    fn coverage_excludes_open_days_and_fetched_but_unpublished_target_days() {
        let (_temp, config) = config();
        let scopes = current_source_scopes(&config).unwrap();
        let mut batch = receipt(&config, "2026-10-20T07:00:00Z");
        assert_eq!(
            accepted_windows(&batch, &config, &scopes, &identities()).unwrap(),
            vec![(
                "mail".into(),
                CollectionWindow {
                    start: date(2026, 10, 17),
                    end: date(2026, 10, 20),
                }
            )]
        );
        batch.targeted = true;
        assert_eq!(
            accepted_windows(&batch, &config, &scopes, &identities()).unwrap(),
            vec![(
                "mail".into(),
                CollectionWindow {
                    start: date(2026, 10, 19),
                    end: date(2026, 10, 20),
                }
            )]
        );
        batch.started_at = Some("2026-10-19T23:59:59Z".parse().unwrap());
        assert!(
            accepted_windows(&batch, &config, &scopes, &identities())
                .unwrap()
                .is_empty()
        );
        batch.started_at = None;
        assert!(
            accepted_windows(&batch, &config, &scopes, &identities())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn coverage_requires_observed_window_and_exact_selection_configuration() {
        let (_temp, config) = config();
        let scopes = current_source_scopes(&config).unwrap();
        let mut batch = receipt(&config, "2026-10-20T07:00:00Z");
        for outcome in [
            ResponseOutcome::Partial,
            ResponseOutcome::Failed,
            ResponseOutcome::Unverified,
        ] {
            batch.sources[0].outcome = outcome;
            assert!(
                accepted_windows(&batch, &config, &scopes, &identities())
                    .unwrap()
                    .is_empty()
            );
        }
        batch.sources[0].outcome = ResponseOutcome::Observed;
        batch.sources[0].scope = ObservedScope::ConfiguredQuery;
        assert!(
            accepted_windows(&batch, &config, &scopes, &identities())
                .unwrap()
                .is_empty()
        );
        batch.sources[0] = observed("mail", "2026-10-17T00:00:00Z", "2026-10-20T00:00:00Z");
        batch.source_scopes.insert("mail".into(), "changed".into());
        assert!(
            accepted_windows(&batch, &config, &scopes, &identities())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn coverage_requires_the_same_observed_dataset_and_principal() {
        let (_temp, config) = config();
        let scopes = current_source_scopes(&config).unwrap();
        let mut batch = receipt(&config, "2026-10-20T07:00:00Z");
        let mut current = identities();
        assert_eq!(
            accepted_windows(&batch, &config, &scopes, &current)
                .unwrap()
                .len(),
            1
        );

        for identity in [
            lk_source::CollectionIdentity::new("other.example.invalid", "mail").unwrap(),
            lk_source::CollectionIdentity::new("mail.example.invalid", "other-account").unwrap(),
        ] {
            current.insert("mail".into(), identity);
            assert!(
                accepted_windows(&batch, &config, &scopes, &current)
                    .unwrap()
                    .is_empty()
            );
        }
        current = identities();
        current.remove("mail");
        assert!(
            accepted_windows(&batch, &config, &scopes, &current)
                .unwrap()
                .is_empty()
        );

        batch.sources[0].collection_identity = None;
        assert!(
            accepted_windows(&batch, &config, &scopes, &identities())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn legacy_receipts_remain_readable_without_inventing_collection_identity() {
        let (_temp, config) = config();
        let root = config.vault.root_path();
        let mut batch = receipt(&config, "2026-10-20T07:00:00Z");
        batch.sources[0].collection_identity = None;
        let raw = serde_json::to_string(&State {
            version: 1,
            batches: vec![batch],
            ..Default::default()
        })
        .unwrap();
        let path = root.join(STATE_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &raw).unwrap();
        assert!(source_coverage(&config, &identities()).unwrap().is_empty());
        let (state, before) = load(&root).unwrap();
        assert_eq!(state.version, 2);
        assert!(state.batches[0].sources[0].collection_identity.is_none());
        assert_eq!(before.as_deref(), Some(raw.as_str()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
    }

    #[test]
    fn coverage_tracks_complete_civil_days_across_daylight_saving_time() {
        let (_temp, mut config) = config();
        config.vault.timezone = Some("America/New_York".into());
        let scopes = current_source_scopes(&config).unwrap();
        let mut batch = receipt(&config, "2026-11-02T12:00:00Z");
        batch.at = "2026-11-02T13:00:00Z".parse().unwrap();
        batch.sources[0] = observed("mail", "2026-11-01T04:00:00Z", "2026-11-02T05:00:00Z");
        assert_eq!(
            accepted_windows(&batch, &config, &scopes, &identities()).unwrap(),
            vec![(
                "mail".into(),
                CollectionWindow {
                    start: date(2026, 11, 1),
                    end: date(2026, 11, 2),
                }
            )]
        );
    }

    #[test]
    fn filtered_receipts_and_accepted_coverage_survive_independent_and_partial_retries() {
        let (_temp, mut config) = config();
        let root = config.vault.root_path();
        std::fs::create_dir_all(&root).unwrap();
        let session = MutationSession::acquire(&root).unwrap();
        let started = "2020-01-02T07:00:00Z".parse().unwrap();
        for id in ["mail", "other"] {
            record_collection(
                &config,
                &session,
                CollectionPublication {
                    requested_day: date(2020, 1, 1),
                    full_ingest: false,
                    started_at: started,
                    targeted: true,
                    work_logs: BTreeMap::new(),
                },
                vec![observed(id, "2020-01-01T00:00:00Z", "2020-01-02T00:00:00Z")],
            )
            .unwrap();
        }
        let mut partial = observed("mail", "2020-01-01T00:00:00Z", "2020-01-02T00:00:00Z");
        partial.outcome = ResponseOutcome::Partial;
        record_collection(
            &config,
            &session,
            CollectionPublication {
                requested_day: date(2020, 1, 1),
                full_ingest: false,
                started_at: started,
                targeted: true,
                work_logs: BTreeMap::new(),
            },
            vec![partial],
        )
        .unwrap();
        let coverage = source_coverage(&config, &identities()).unwrap();
        assert_eq!(coverage.len(), 2);
        assert_eq!(
            coverage["mail"],
            vec![CollectionWindow {
                start: date(2020, 1, 1),
                end: date(2020, 1, 2)
            }]
        );
        config.sources.get_mut("mail").unwrap().params = serde_json::json!({"queries":["changed"]});
        let coverage = source_coverage(&config, &identities()).unwrap();
        assert!(!coverage.contains_key("mail"));
        assert!(coverage.contains_key("other"));
    }

    #[test]
    fn retries_preserve_bound_coverage_and_unbound_retained_period_inputs() {
        for (bound, replacement) in [
            (true, "failed"),
            (true, "unbound"),
            (true, "other-dataset"),
            (false, "failed"),
            (false, "other-dataset"),
        ] {
            let (_temp, mut config) = config();
            config.synthesis.weekly.include_sources = vec!["mail".into()];
            let root = config.vault.root_path();
            std::fs::create_dir_all(&root).unwrap();
            let session = MutationSession::acquire(&root).unwrap();
            let publish = |source| {
                record_collection(
                    &config,
                    &session,
                    CollectionPublication {
                        requested_day: date(2020, 1, 5),
                        full_ingest: false,
                        started_at: "2020-01-06T07:00:00Z".parse().unwrap(),
                        targeted: true,
                        work_logs: BTreeMap::new(),
                    },
                    vec![source],
                )
                .unwrap();
            };
            let mut original = observed("mail", "2020-01-05T00:00:00Z", "2020-01-06T00:00:00Z");
            if !bound {
                original.collection_identity = None;
            }
            publish(original.clone());
            let mut retry = original;
            match replacement {
                "failed" => retry = ObservedSource::failed("mail"),
                "unbound" => retry.collection_identity = None,
                "other-dataset" => {
                    retry.collection_identity = Some(
                        lk_source::CollectionIdentity::new("another.example.invalid", "mail")
                            .unwrap(),
                    )
                }
                _ => unreachable!(),
            }
            publish(retry);
            let coverage = source_coverage(&config, &identities()).unwrap();
            assert_eq!(
                coverage.get("mail").map_or(0, Vec::len),
                usize::from(bound),
                "{bound} {replacement}"
            );
            let state = load(&root).unwrap().0;
            assert_eq!(state.batches.len(), 2, "{bound} {replacement}");
            let period = ClosedPeriod::containing(Kind::WeeklyThemes, date(2020, 1, 5)).unwrap();
            assert!(
                closing_day_ready(&state, &config, &period).is_ok(),
                "{bound} {replacement}"
            );
        }
    }

    #[test]
    fn incomplete_configured_query_cannot_compact_away_known_personal_contributions() {
        let (_temp, config) = config();
        let root = config.vault.root_path();
        std::fs::create_dir_all(&root).unwrap();
        let session = MutationSession::acquire(&root).unwrap();
        let day = date(2020, 1, 1);
        let mut source = observed("mail", "2020-01-01T00:00:00Z", "2020-01-02T00:00:00Z");
        source.scope = ObservedScope::ConfiguredQuery;
        source.personal_inputs = Some(lk_pipeline::WorkLogInputs {
            scope: "personal-scope".into(),
            days: BTreeMap::from([(day, "known-positive".into())]),
        });
        for outcome in [ResponseOutcome::Observed, ResponseOutcome::Partial] {
            source.outcome = outcome;
            if outcome == ResponseOutcome::Partial {
                source.personal_inputs.as_mut().unwrap().days.clear();
            }
            record_collection(
                &config,
                &session,
                CollectionPublication {
                    requested_day: day,
                    full_ingest: false,
                    started_at: "2020-01-02T07:00:00Z".parse().unwrap(),
                    targeted: true,
                    work_logs: BTreeMap::new(),
                },
                vec![source.clone()],
            )
            .unwrap();
        }
        let (state, _) = load(&root).unwrap();
        assert_eq!(state.batches.len(), 2);
        assert_eq!(
            state.batches[0].sources[0]
                .personal_inputs
                .as_ref()
                .unwrap()
                .days[&day],
            "known-positive"
        );
    }
}

fn request(
    state: &mut State,
    period: ClosedPeriod,
    config: &Config,
    today: Date,
) -> miette::Result<()> {
    period.validate()?;
    if period.end >= today {
        return Err(miette::miette!(
            "deferred synthesis requires a closed period ending before {today}"
        ));
    }
    let key = period.key();
    state.intents.insert(key, Intent::new(period, config)?);
    Ok(())
}

pub fn defer(
    config: &Config,
    session: &MutationSession,
    periods: Vec<ClosedPeriod>,
    today: Date,
) -> miette::Result<()> {
    let root = session.root().to_path_buf();
    let (mut state, before) = load(&root)?;
    for period in periods {
        request(&mut state, period, config, today)?;
    }
    save(&root, session, &state, before)
}

fn personal_sources(config: &Config) -> Vec<String> {
    let Some(personal) = &config.personal else {
        return vec![];
    };
    personal
        .tracked_sources
        .iter()
        .filter(|id| {
            config.sources.get(*id).is_some_and(|sc| {
                sc.enabled
                    && matches!(
                        sc.source_type,
                        SourceType::Gmail
                            | SourceType::SlackChannel
                            | SourceType::SlackSearch
                            | SourceType::Jira
                            | SourceType::Confluence
                            | SourceType::GoogleCalendar
                            | SourceType::Tasks
                    )
            })
        })
        .cloned()
        .collect()
}

fn required_sources(config: &Config, kind: Kind) -> Vec<String> {
    if kind == Kind::WeeklyThemes {
        config.synthesis.weekly.include_sources.clone()
    } else {
        personal_sources(config)
    }
}

fn closing_day_ready(
    state: &State,
    config: &Config,
    period: &ClosedPeriod,
) -> Result<String, String> {
    let mut sources = required_sources(config, period.kind);
    sources.sort();
    sources.dedup();
    if sources.is_empty() {
        return Ok("No configured contributing sources".into());
    }
    for id in &sources {
        if !config.sources.get(id).is_some_and(|sc| sc.enabled) {
            return Err(format!("configured dependency {id} is absent or disabled"));
        }
    }
    let tz = config.vault.timezone();
    let start = period
        .end
        .to_zoned(tz.clone())
        .map_err(|e| e.to_string())?
        .timestamp();
    let end = period
        .end
        .checked_add(jiff::Span::new().days(1))
        .map_err(|e| e.to_string())?
        .to_zoned(tz)
        .map_err(|e| e.to_string())?
        .timestamp();
    let scopes = current_source_scopes(config).map_err(|error| error.to_string())?;
    let check = |batch: &CollectionBatch,
                 accepted: &[(String, CollectionWindow)],
                 id: &str|
     -> Result<(), String> {
        let source = batch
            .sources
            .iter()
            .find(|source| source.source_id == id)
            .ok_or_else(|| format!("{id}: unobserved in the current collection batch"))?;
        if batch.source_scopes.get(id) != scopes.get(id) {
            return Err(format!("{id}: collection configuration changed"));
        }
        if source.outcome != ResponseOutcome::Observed {
            return Err(format!(
                "{id}: {:?} configured-source response",
                source.outcome
            ));
        }
        if batch.started_at.is_none_or(|at| at < end || at > batch.at)
            || (batch.targeted && batch.requested_day != period.end)
        {
            return Err(format!(
                "{id}: closing day was not published from a known post-close acquisition"
            ));
        }
        match source.scope {
            ObservedScope::Window { .. }
                if accepted.iter().any(|(source_id, window)| {
                    source_id == id && window.start <= period.end && window.end > period.end
                }) =>
            {
                Ok(())
            }
            ObservedScope::ConfiguredQuery => Ok(()),
            _ => Err(format!(
                "{id}: closing-day response window unknown or outside published scope"
            )),
        }
    };
    let scope_limit = "earlier-history coverage unverified; configured-query scopes do not establish date coverage";
    if period.kind != Kind::WeeklyThemes {
        // A later partial full ingest can replace the work-log with its available
        // sources. An earlier successful batch must not certify that newer view.
        let candidate = state.batches.iter().rev().find(|batch| {
            batch.full_ingest && batch.at >= end
                && (!batch.targeted || batch.requested_day == period.end)
                && batch.sources.iter().any(|source| sources.contains(&source.source_id)
                    && match source.scope {
                        ObservedScope::Window { start: min, end: max } => min <= start && max >= end,
                        ObservedScope::ConfiguredQuery | ObservedScope::Unverified => true,
                    })
        }).ok_or_else(|| format!("unobserved closing day {}; collect its actual window with `lore ingest --date {}` (full-source) and finish semantic processing before resuming", period.end, period.end))?;
        let accepted =
            published_windows(candidate, config, &scopes).map_err(|error| error.to_string())?;
        for id in &sources {
            check(candidate, &accepted, id)?;
        }
        return Ok(format!(
            "Configured response batch at {}; closing day {}; {scope_limit}",
            candidate.at, period.end
        ));
    }
    let mut ready = BTreeMap::new();
    let mut reasons = BTreeMap::new();
    for batch in state.batches.iter().rev().filter(|batch| batch.at >= end) {
        let accepted =
            published_windows(batch, config, &scopes).map_err(|error| error.to_string())?;
        for id in &sources {
            if ready.contains_key(id) || !batch.sources.iter().any(|source| source.source_id == *id)
            {
                continue;
            }
            match check(batch, &accepted, id) {
                Ok(()) => {
                    ready.insert(id.clone(), batch.at);
                }
                Err(reason) => {
                    reasons.entry(id.clone()).or_insert(reason);
                }
            }
        }
        if ready.len() == sources.len() {
            return Ok(format!(
                "Configured per-source response receipts; closing day {}; earliest accepted receipt {}; {scope_limit}",
                period.end,
                ready.values().min().expect("required sources are nonempty")
            ));
        }
    }
    let missing = sources
        .iter()
        .find(|id| !ready.contains_key(id.as_str()))
        .expect("incomplete sources");
    Err(reasons.remove(missing).unwrap_or_else(|| format!(
        "{missing}: unobserved closing day {}; collect its actual window and finish semantic processing before resuming", period.end
    )))
}

fn children(period: &ClosedPeriod, config: &Config) -> Result<Vec<ClosedPeriod>, String> {
    if !matches!(period.kind, Kind::Quarterly | Kind::Annual) {
        return Ok(vec![]);
    }
    let Some(personal) = &config.personal else {
        return Err("personal module disabled".into());
    };
    let child_kind = if period.kind == Kind::Annual && personal.quarterly.enabled {
        Kind::Quarterly
    } else if personal.monthly.enabled {
        Kind::Monthly
    } else if config.synthesis.weekly.enabled {
        Kind::WeeklyReview
    } else {
        return Err("no enabled child-review path; existing previews remain available through direct synthesis".into());
    };
    let mut result = Vec::new();
    let mut date = period.start;
    while date <= period.end {
        let child = ClosedPeriod::containing(child_kind, date).map_err(|e| e.to_string())?;
        date = child
            .end
            .checked_add(jiff::Span::new().days(1))
            .map_err(|e| e.to_string())?;
        result.push(child);
    }
    Ok(result)
}

async fn page(reader: &FsVault, path: &Path) -> miette::Result<Option<VaultPage>> {
    reader
        .read_page(path)
        .await
        .map_err(|e| miette::miette!("read {}: {e}", path.display()))
}

fn page_semantics_ready(page: &VaultPage) -> bool {
    super::super::queue::readiness::unfinished_inputs(page).is_ok_and(|keys| keys.is_empty())
}

struct CapturedInputs {
    pages: Vec<InputPage>,
    has_contributing_page: bool,
    guards: MutationPlan,
    work_logs: BTreeMap<Date, WorkLogInput>,
}

fn work_log_day(config: &Config, path: &str) -> Option<Date> {
    let day: Date = Path::new(path).file_stem()?.to_str()?.parse().ok()?;
    (VaultPath::work_log(&config.vault.dirs, day).to_string() == path).then_some(day)
}

fn work_log_input(page: &VaultPage) -> Option<String> {
    page.frontmatter
        .get(field::LLM_INPUTS)?
        .get(TargetKind::WorkLogSynthesis.llm_inputs_key())?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[derive(Debug, Clone)]
enum WorkLogInput {
    Absent,
    Unbound,
    Bound(String),
}

fn latest_personal_receipt<'a>(
    batches: &'a [CollectionBatch],
    id: &str,
    day: Date,
    start: jiff::Timestamp,
    end: jiff::Timestamp,
) -> Option<(&'a CollectionBatch, &'a ObservedSource)> {
    batches.iter().rev().find_map(|batch| {
        let source = batch.sources.iter().find(|source| source.source_id == id)?;
        if source.outcome == ResponseOutcome::Failed
            || (source.outcome == ResponseOutcome::Partial && source.personal_inputs.is_none()) {
            return None;
        }
        let applies = (!batch.targeted || batch.requested_day == day)
            && (source.personal_inputs.as_ref().is_some_and(|inputs| inputs.days.contains_key(&day))
                || (batch.started_at.is_some_and(|at| at >= end && at <= batch.at)
                    && matches!(source.scope, ObservedScope::Window { start: min, end: max } if min <= start && max >= end)));
        applies.then_some((batch, source))
    })
}

fn personal_work_logs_ready(
    batches: &[CollectionBatch],
    config: &Config,
    work_logs: &BTreeMap<Date, WorkLogInput>,
) -> Result<usize, String> {
    if work_logs.is_empty() {
        return Ok(0);
    }
    let Some(personal) = &config.personal else {
        return Err("personal module disabled".into());
    };
    let scope = lk_pipeline::work_log_scope(personal, config.vault.locale());
    let scopes = current_source_scopes(config).map_err(|error| error.to_string())?;
    let mut sources = personal_sources(config);
    sources.sort();
    sources.dedup();
    let mut unverified_absences = std::collections::BTreeSet::new();
    let remedy = |day: Date, reason: &str| {
        format!(
            "work-log {day}: {reason}; full-source `lore ingest --date {day}` can refresh the binding only when it actually publishes that day's work-log; empty/unavailable historical input does not authorize erasing or adopting the retained log"
        )
    };
    for (day, input) in work_logs {
        let tz = config.vault.timezone();
        let start = day
            .to_zoned(tz.clone())
            .map_err(|error| error.to_string())?
            .timestamp();
        let end = day
            .tomorrow()
            .map_err(|error| error.to_string())?
            .to_zoned(tz)
            .map_err(|error| error.to_string())?
            .timestamp();
        if matches!(input, WorkLogInput::Absent) {
            for id in &sources {
                let latest = latest_personal_receipt(batches, id, *day, start, end);
                let known = latest.and_then(|(batch, source)| {
                    source
                        .personal_inputs
                        .as_ref()
                        .filter(|inputs| inputs.scope == scope)
                        .filter(|_| batch.source_scopes.get(id) == scopes.get(id))
                });
                if known.is_some_and(|inputs| {
                    inputs
                        .days
                        .get(day)
                        .is_some_and(|hash| *hash != lk_pipeline::empty_work_log_input(id, *day))
                }) {
                    return Err(remedy(
                        *day,
                        &format!("{id}: known personal contributions have no published work-log"),
                    ));
                }
                if known.is_some()
                    && latest.is_some_and(|(_, source)| source.outcome != ResponseOutcome::Observed)
                {
                    return Err(remedy(
                        *day,
                        &format!(
                            "{id}: incomplete personal contribution observation; retry source collection"
                        ),
                    ));
                }
                let known = known.filter(|_| {
                    latest.is_some_and(|(_, source)| source.outcome == ResponseOutcome::Observed)
                });
                match known {
                    Some(inputs) if inputs.days.contains_key(day) => {}
                    Some(_) if latest.is_some_and(|(batch, source)|
                        batch.started_at.is_some_and(|at| at >= end && at <= batch.at)
                            && matches!(source.scope, ObservedScope::Window { start: min, end: max } if min <= start && max >= end)) => {}
                    _ => { unverified_absences.insert(*day); }
                }
            }
            continue;
        }
        let WorkLogInput::Bound(input) = input else {
            return Err(remedy(*day, "unbound synthesis input"));
        };
        let baseline = batches
            .iter()
            .rev()
            .find(|batch| batch.full_ingest && batch.work_logs.get(day) == Some(input))
            .ok_or_else(|| remedy(*day, "no bound full-source publication"))?;
        let mut has_publication_policy = false;
        for inputs in baseline
            .sources
            .iter()
            .filter_map(|source| source.personal_inputs.as_ref())
        {
            has_publication_policy = true;
            if inputs.scope != scope {
                return Err(remedy(*day, "personal publication policy changed"));
            }
        }
        if !has_publication_policy {
            return Err(remedy(*day, "personal publication policy is unbound"));
        }
        for id in &sources {
            let original = baseline
                .sources
                .iter()
                .find(|source| source.source_id == *id)
                .filter(|source| source.outcome == ResponseOutcome::Observed)
                .and_then(|source| source.personal_inputs.as_ref())
                .filter(|inputs| inputs.scope == scope)
                .filter(|_| baseline.source_scopes.get(id) == scopes.get(id))
                .ok_or_else(|| {
                    remedy(
                        *day,
                        &format!("{id}: unbound personal contribution selection"),
                    )
                })?;
            let expected = original
                .days
                .get(day)
                .cloned()
                .unwrap_or_else(|| lk_pipeline::empty_work_log_input(id, *day));
            // A full publication binds the inputs it actually used. An out-of-window
            // omission is not evidence of historical emptiness and cannot supersede it.
            let Some((latest_batch, latest)) =
                latest_personal_receipt(batches, id, *day, start, end)
            else {
                continue;
            };
            let current = latest
                .personal_inputs
                .as_ref()
                .filter(|_| latest.outcome == ResponseOutcome::Observed)
                .filter(|inputs| inputs.scope == scope)
                .filter(|_| latest_batch.source_scopes.get(id) == scopes.get(id))
                .ok_or_else(|| {
                    remedy(
                        *day,
                        &format!("{id}: latest personal contribution input is unbound"),
                    )
                })?;
            let actual = current
                .days
                .get(day)
                .cloned()
                .unwrap_or_else(|| lk_pipeline::empty_work_log_input(id, *day));
            if actual != expected {
                return Err(remedy(
                    *day,
                    &format!("{id}: personal contributions changed after this work-log input"),
                ));
            }
        }
    }
    Ok(unverified_absences.len())
}

async fn input_pages(
    reader: &FsVault,
    config: &Config,
    intent: &Intent,
    child_periods: &[ClosedPeriod],
) -> miette::Result<Result<CapturedInputs, String>> {
    let mut paths = BTreeMap::new();
    if child_periods.is_empty() {
        let mut date = intent.period.start;
        while date <= intent.period.end {
            if intent.period.kind == Kind::WeeklyThemes {
                for id in required_sources(config, intent.period.kind) {
                    paths.insert(
                        VaultPath::daily(&config.vault.dirs, &id, date).to_string(),
                        true,
                    );
                }
            } else {
                paths.insert(
                    VaultPath::work_log(&config.vault.dirs, date).to_string(),
                    true,
                );
                for id in required_sources(config, intent.period.kind) {
                    // This source page gates semantic readiness, but is not personal review material.
                    paths.insert(
                        VaultPath::daily(&config.vault.dirs, &id, date).to_string(),
                        false,
                    );
                }
            }
            date = date
                .checked_add(jiff::Span::new().days(1))
                .map_err(|e| miette::miette!("date: {e}"))?;
        }
    } else {
        for child in child_periods {
            paths.insert(child.path(config).to_string(), true);
        }
        // Parent category totals read work-log independently of child prose.
        let mut date = intent.period.start;
        while date <= intent.period.end {
            paths.insert(
                work_log_dir(&config.vault.dirs)
                    .join(format!("{date}.md"))
                    .display()
                    .to_string(),
                true,
            );
            date = date
                .checked_add(jiff::Span::new().days(1))
                .map_err(|e| miette::miette!("date: {e}"))?;
        }
    }
    let mut result = Vec::new();
    let mut has_contributing_page = false;
    let mut guards = MutationPlan::default();
    let mut work_logs = BTreeMap::new();
    for (path, contributes) in paths {
        if let Some(source) = page(reader, Path::new(&path)).await? {
            if !page_semantics_ready(&source) {
                return Ok(Err(format!("semantic input pending: {path}")));
            }
            has_contributing_page |= contributes;
            if let Some(day) = work_log_day(config, &path) {
                work_logs.insert(
                    day,
                    work_log_input(&source)
                        .map(WorkLogInput::Bound)
                        .unwrap_or(WorkLogInput::Unbound),
                );
            }
            guards.require_unchanged(
                &path,
                source
                    .original
                    .clone()
                    .ok_or_else(|| miette::miette!("period input snapshot missing"))?,
            );
            result.push(InputPage {
                path,
                revision: Some(lk_core::concept::source_revision(&source)),
            });
        } else {
            if let Some(day) = work_log_day(config, &path) {
                work_logs.insert(day, WorkLogInput::Absent);
            }
            guards.require_absent(&path);
            result.push(InputPage {
                path,
                revision: None,
            });
        }
    }
    Ok(Ok(CapturedInputs {
        pages: result,
        has_contributing_page,
        guards,
        work_logs,
    }))
}

async fn plan(
    synth: &Synthesizer,
    period: &ClosedPeriod,
    children: &[(std::path::PathBuf, TargetKind)],
) -> miette::Result<Option<RenderResult>> {
    let result = match period.kind {
        Kind::WeeklyThemes => synth.try_weekly_synthesis(period.start).await,
        Kind::WeeklyReview => synth.try_weekly_review(period.start).await,
        Kind::Monthly => {
            synth
                .try_monthly_review(period.start.year(), period.start.month() as u8)
                .await
        }
        Kind::Quarterly => {
            synth
                .try_closed_review_from_children(
                    period.start.year(),
                    Some(lk_core::vault_path::quarter_of_month(
                        period.start.month() as u8
                    )),
                    children,
                )
                .await
        }
        Kind::Annual => {
            synth
                .try_closed_review_from_children(period.start.year(), None, children)
                .await
        }
    };
    result.map_err(|e| miette::miette!("period planning: {e}"))
}

pub async fn resume(
    config: &Config,
    opts: &super::super::GlobalOptions,
    session: &MutationSession,
    stage: Option<ResumeStage>,
    json: bool,
    today: Date,
) -> miette::Result<()> {
    let root = session.root().to_path_buf();
    let (mut state, before) = load(&root)?;
    let reader = FsVault::new(&root);
    // Materialize child obligations before the base stage, including those whose
    // parent's scheduled trigger arrived while collection was still pending.
    loop {
        let mut added = false;
        for intent in state.intents.values().cloned().collect::<Vec<_>>() {
            if matches!(
                intent.status,
                Status::Complete | Status::RetainedComplete | Status::NoData
            ) {
                continue;
            }
            if let Ok(children) = children(&intent.period, config) {
                for child in children {
                    if child.end < today && !state.intents.contains_key(&child.key()) {
                        request(&mut state, child.clone(), config, today)?;
                        if let Some(existing) = page(&reader, child.path(config).as_ref()).await?
                            && child.matches_page(&existing)
                            && let Some(input) =
                                lk_pipeline::completed_input(&existing, child.kind.target_kind())
                        {
                            let retained = state
                                .intents
                                .get_mut(&child.key())
                                .expect("child requested");
                            retained.status = Status::RetainedComplete;
                            retained.retained = true;
                            retained.target_input = Some(input.to_owned());
                            retained.scope = "Retained exact-period semantic input is marked complete; prior collection coverage is unverified.".into();
                        }
                        added = true;
                    }
                }
            }
        }
        if !added {
            break;
        }
    }
    let llm = super::super::build_llm_client(config, &root)?;
    let ctx = Arc::new(
        PipelineContext::build(opts.template_dir.as_deref(), llm.clone(), config)
            .map_err(|e| miette::miette!("period context: {e}"))?,
    );
    let synth = Synthesizer::new(&root, ctx, config, today);
    refresh_completed(&mut state, config, &reader).await?;
    let mut publication = super::super::publication::Publication::new(session, llm.clone());
    let mut failed = None;
    let mut keys: Vec<_> = state.intents.keys().cloned().collect();
    keys.sort_by_key(|key| {
        let intent = &state.intents[key];
        (intent.period.kind.stage() as u8, intent.period.start)
    });
    for key in keys {
        let mut intent = state.intents[&key].clone();
        if stage.is_some_and(|s| s != intent.period.kind.stage())
            || matches!(
                intent.status,
                Status::Complete | Status::RetainedComplete | Status::NoData
            )
        {
            continue;
        }
        let result = process(
            &mut intent,
            &state,
            config,
            &reader,
            &synth,
            &mut publication,
            today,
        )
        .await;
        if let Err(error) = result {
            intent.status = Status::Blocked;
            intent.reason = Some(error.to_string());
            failed = Some(error);
        }
        state.intents.insert(key, intent);
        if failed.is_some() {
            break;
        }
    }
    if failed.is_none()
        && let Err(error) = publication.finish().await
    {
        failed = Some(error);
    }
    save(&root, session, &state, before)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&state.intents)
                .map_err(|e| miette::miette!("period report: {e}"))?
        );
    } else {
        for (key, intent) in &state.intents {
            eprintln!(
                "{key}: {:?}{}",
                intent.status,
                intent
                    .reason
                    .as_ref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default()
            );
        }
    }
    if let Some(error) = failed {
        return Err(error);
    }
    Ok(())
}

async fn process(
    intent: &mut Intent,
    state: &State,
    config: &Config,
    reader: &FsVault,
    synth: &Synthesizer,
    publisher: &mut super::super::publication::Publication<'_>,
    today: Date,
) -> miette::Result<()> {
    let blocked = |intent: &mut Intent, reason: String| {
        intent.status = Status::Blocked;
        intent.reason = Some(reason);
    };
    if intent.retained {
        blocked(
            intent,
            "retained child semantic input pending; finish its existing request before using it"
                .into(),
        );
        return Ok(());
    }
    if intent.period.end >= today {
        blocked(intent, "period is not closed".into());
        return Ok(());
    }
    if intent.timezone != timezone_identity(config)? {
        blocked(
            intent,
            "vault timezone changed; request the intended period explicitly".into(),
        );
        return Ok(());
    }
    if intent.period.kind != Kind::WeeklyThemes && config.personal.is_none() {
        blocked(intent, "personal module disabled".into());
        return Ok(());
    }
    let enabled = match intent.period.kind {
        Kind::WeeklyThemes | Kind::WeeklyReview => config.synthesis.weekly.enabled,
        Kind::Monthly => config
            .personal
            .as_ref()
            .is_some_and(|personal| personal.monthly.enabled),
        Kind::Quarterly => config
            .personal
            .as_ref()
            .is_some_and(|personal| personal.quarterly.enabled),
        Kind::Annual => config
            .personal
            .as_ref()
            .is_some_and(|personal| personal.annual.enabled),
    };
    if !enabled {
        blocked(
            intent,
            "configured period is disabled; intent retained for explicit re-enabling".into(),
        );
        return Ok(());
    }
    let scope = match closing_day_ready(state, config, &intent.period) {
        Ok(scope) => scope,
        Err(reason) => {
            blocked(intent, reason);
            return Ok(());
        }
    };
    let child_periods = match children(&intent.period, config) {
        Ok(children) => children,
        Err(reason) => {
            blocked(intent, reason);
            return Ok(());
        }
    };
    for child in &child_periods {
        let Some(child_intent) = state.intents.get(&child.key()) else {
            blocked(intent, format!("child period pending: {}", child.key()));
            return Ok(());
        };
        if child_intent.status == Status::NoData {
            continue;
        }
        if !matches!(
            child_intent.status,
            Status::Complete | Status::RetainedComplete
        ) {
            blocked(
                intent,
                format!("child period {:?}: {}", child_intent.status, child.key()),
            );
            return Ok(());
        }
        let Some(child_page) = page(reader, child.path(config).as_ref()).await? else {
            blocked(intent, format!("child page missing: {}", child.key()));
            return Ok(());
        };
        if lk_pipeline::completed_input(&child_page, child.kind.target_kind())
            != child_intent.target_input.as_deref()
        {
            blocked(
                intent,
                format!("child captured input no longer complete: {}", child.key()),
            );
            return Ok(());
        }
    }
    let CapturedInputs {
        pages: inputs,
        has_contributing_page,
        mut guards,
        work_logs,
    } = match input_pages(reader, config, intent, &child_periods).await? {
        Ok(inputs) => inputs,
        Err(reason) => {
            blocked(intent, reason);
            return Ok(());
        }
    };
    let unverified_absences = match personal_work_logs_ready(&state.batches, config, &work_logs) {
        Ok(count) => count,
        Err(reason) => {
            blocked(intent, reason);
            return Ok(());
        }
    };
    let scope = if unverified_absences > 0 {
        format!(
            "{scope}; {unverified_absences} absent work-log dates have unverified contribution history, not observed-empty history"
        )
    } else {
        scope
    };
    let scope_hash = input_scope(config, &intent.period)?;
    let target = intent.period.path(config);
    if intent.inputs == inputs
        && intent.input_scope == scope_hash
        && let Some(target_page) = page(reader, target.as_ref()).await?
        && let Some(input) = intent.target_input.as_deref()
        && lk_pipeline::completed_input(&target_page, intent.period.kind.target_kind())
            == Some(input)
    {
        guards.require_unchanged(
            target.as_ref(),
            target_page
                .original
                .ok_or_else(|| miette::miette!("period target snapshot missing"))?,
        );
        publisher
            .publish_with_plan(std::iter::empty::<&RenderResult>(), guards)
            .await?;
        intent.status = Status::Complete;
        intent.reason = None;
        intent.scope = scope;
        return Ok(());
    }
    let target_before = page(reader, target.as_ref()).await?;
    let child_inputs = child_periods
        .iter()
        .filter(|child| {
            state
                .intents
                .get(&child.key())
                .is_some_and(|i| matches!(i.status, Status::Complete | Status::RetainedComplete))
        })
        .map(|child| {
            (
                child.path(config).as_ref().to_path_buf(),
                child.kind.target_kind(),
            )
        })
        .collect::<Vec<_>>();
    let scoped_synth = synth.clone().with_evidence_scope(format!(
        "This closed period is {} through {}. Sources are retained authored pages under the current configured selections. Closing-day configured adapter responses were observed after the day closed; configured-query scopes do not establish date coverage. Earlier-history coverage and provider-wide completeness are unverified. Preserve these limitations where they affect conclusions. Input selection fingerprint (bookkeeping only; do not reproduce): {scope_hash}.",
        intent.period.start, intent.period.end));
    let output = plan(&scoped_synth, &intent.period, &child_inputs).await?;
    intent.inputs = inputs;
    intent.input_scope = scope_hash;
    intent.scope = scope;
    intent.reason = None;
    match output {
        Some(output) => {
            let output = output
                .with_original(target_before.as_ref())
                .map_err(|e| miette::miette!("period target guard: {e}"))?;
            let parsed = lk_core::frontmatter::parse_page(&output.content)
                .map_err(|e| miette::miette!("planned period: {e}"))?;
            let input = parsed
                .frontmatter
                .get(field::LLM_INPUTS)
                .and_then(|v| v.get(intent.period.kind.target_kind().llm_inputs_key()))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| miette::miette!("planned period has no input"))?
                .to_owned();
            publisher.publish_with_plan([&output], guards).await?;
            intent.target_input = Some(input.clone());
            intent.status =
                if lk_pipeline::completed_input(&parsed, intent.period.kind.target_kind())
                    == Some(input.as_str())
                {
                    Status::Complete
                } else {
                    Status::AwaitingSemantics
                };
        }
        None => {
            // Readiness-only source pages are not personal narrative input. The
            // post-close write/flush receipt was checked before inspecting this
            // inventory; missing actual contributors can establish retained NoData.
            // None with a contributor remains ambiguous (for example render drift).
            if config.llm.provider != lk_core::config::LlmProvider::Queue || has_contributing_page {
                blocked(intent, "no output with retained input or without an answering queue provider; no-data is not established".into());
            } else {
                publisher
                    .publish_with_plan(std::iter::empty::<&RenderResult>(), guards)
                    .await?;
                intent.status = Status::NoData;
                intent.target_input = None;
                intent.scope.push_str("; no retained contributing period input; this does not establish zero historical remote activity");
            }
        }
    }
    Ok(())
}

async fn refresh_completed(
    state: &mut State,
    config: &Config,
    reader: &FsVault,
) -> miette::Result<()> {
    let batches = &state.batches;
    for intent in state.intents.values_mut() {
        if intent.retained {
            match page(reader, intent.period.path(config).as_ref()).await? {
                Some(source) if intent.period.matches_page(&source) => {
                    if let Some(input) =
                        lk_pipeline::completed_input(&source, intent.period.kind.target_kind())
                    {
                        intent.status = Status::RetainedComplete;
                        intent.target_input = Some(input.to_owned());
                        intent.reason = None;
                    } else {
                        intent.status = Status::Blocked;
                        intent.reason = Some("retained child semantic input pending".into());
                    }
                }
                _ => {
                    intent.status = Status::Blocked;
                    intent.reason =
                        Some("retained child exact-period page missing or changed".into());
                }
            }
            continue;
        }
        if !matches!(intent.status, Status::Complete | Status::NoData) {
            continue;
        }
        let mut work_logs = BTreeMap::new();
        for input in &intent.inputs {
            let current = page(reader, Path::new(&input.path)).await?;
            if let Some(day) = work_log_day(config, &input.path) {
                work_logs.insert(
                    day,
                    current
                        .as_ref()
                        .map(|page| {
                            work_log_input(page)
                                .map(WorkLogInput::Bound)
                                .unwrap_or(WorkLogInput::Unbound)
                        })
                        .unwrap_or(WorkLogInput::Absent),
                );
            }
            if current.as_ref().map(lk_core::concept::source_revision) != input.revision
                || current
                    .as_ref()
                    .is_some_and(|page| !page_semantics_ready(page))
            {
                intent.status = Status::Pending;
                break;
            }
        }
        if intent.input_scope != input_scope(config, &intent.period)? {
            intent.status = Status::Pending;
        }
        if matches!(intent.status, Status::Complete | Status::NoData)
            && let Err(reason) = personal_work_logs_ready(batches, config, &work_logs)
        {
            intent.status = Status::Pending;
            intent.reason = Some(reason);
        }
        if intent.status == Status::Complete {
            let target = page(reader, intent.period.path(config).as_ref()).await?;
            if target.as_ref().and_then(|page| {
                lk_pipeline::completed_input(page, intent.period.kind.target_kind())
            }) != intent.target_input.as_deref()
            {
                intent.status = Status::Pending;
            }
        }
    }
    Ok(())
}

pub fn report(root: &Path, json: bool) -> miette::Result<()> {
    let (state, _) = load(root)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&state.intents)
                .map_err(|e| miette::miette!("period report: {e}"))?
        );
    } else {
        for (key, intent) in &state.intents {
            eprintln!(
                "{key}: {:?}{}",
                intent.status,
                intent
                    .reason
                    .as_ref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn config() -> (tempfile::TempDir, Config) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, "vault:\n  root: vault\n  timezone: UTC\nidentity:\n  name: Test\n  email: test@example.invalid\nsources:\n  mail:\n    type: gmail\n  news:\n    type: rss\npersonal:\n  tracked_sources: [mail]\n").unwrap();
        let config = Config::load(&path).unwrap();
        (temp, config)
    }

    fn recovery(config: &mut Config, start_date: Date, budget: usize) {
        config.synthesis.recovery = Some(lk_core::config::PeriodRecoveryConfig {
            start_date,
            max_periods_per_run: std::num::NonZeroUsize::new(budget).unwrap(),
        });
    }

    #[tokio::test]
    async fn completed_period_reopens_for_marker_only_pending_input_and_reuses_restored_authored_state()
     {
        let (_temp, config) = config();
        let root = config.vault.root_path();
        let source = VaultPath::daily(&config.vault.dirs, "mail", date(2026, 9, 30));
        let path = root.join(source.as_ref());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let heading = config.vault.locale().strings().summary;
        let ready = format!(
            "---\ntype: daily\nllm_inputs: {{summary: captured, summary_done: captured}}\n---\n## {heading}\nRead interpretation.\n"
        );
        let pending = ready.replace(", summary_done: captured", "");
        std::fs::write(&path, &ready).unwrap();
        let mut intent = Intent::new(period(), &config).unwrap();
        intent.status = Status::NoData;
        intent.input_scope = input_scope(&config, &intent.period).unwrap();
        intent.inputs = vec![InputPage {
            path: source.to_string(),
            revision: Some(lk_core::concept::source_revision(
                &lk_vault::parse_page(&ready).unwrap(),
            )),
        }];
        let mut state = State {
            intents: BTreeMap::from([(intent.period.key(), intent)]),
            ..Default::default()
        };
        std::fs::write(&path, &pending).unwrap();
        assert_eq!(
            lk_core::concept::source_revision(&lk_vault::parse_page(&ready).unwrap()),
            lk_core::concept::source_revision(&lk_vault::parse_page(&pending).unwrap())
        );
        refresh_completed(&mut state, &config, &FsVault::new(&root))
            .await
            .unwrap();
        assert_eq!(state.intents[&period().key()].status, Status::Pending);
        std::fs::write(&path, &ready).unwrap();
        assert!(page_semantics_ready(
            &page(&FsVault::new(&root), source.as_ref())
                .await
                .unwrap()
                .unwrap()
        ));
    }

    #[tokio::test]
    async fn period_input_guards_refuse_a_previously_missing_page_appearing_before_publication() {
        let (_temp, config) = config();
        let root = config.vault.root_path();
        std::fs::create_dir_all(&root).unwrap();
        let intent = Intent::new(period(), &config).unwrap();
        let reader = FsVault::new(config.vault.root_path());
        let captured = input_pages(&reader, &config, &intent, &[])
            .await
            .unwrap()
            .unwrap();
        let absent = captured
            .pages
            .iter()
            .find(|input| input.revision.is_none())
            .unwrap();
        let new_source = root.join(&absent.path);
        std::fs::create_dir_all(new_source.parent().unwrap()).unwrap();
        std::fs::write(
            &new_source,
            "---\ntype: work-log\n---\nNew historical observation.\n",
        )
        .unwrap();
        let mut guards = captured.guards;
        guards.create(
            "must-not-publish.md",
            "A stale period interpretation".into(),
        );
        let session = MutationSession::acquire(&root).unwrap();
        assert!(session.apply(guards).is_err());
        assert!(!root.join("must-not-publish.md").exists());
    }

    #[test]
    fn period_recovery_is_opt_in_bounded_resumable_and_does_not_invent_collection_coverage() {
        let (_temp, mut config) = config();
        let mut state = State {
            version: 1,
            ..Default::default()
        };
        assert!(reconcile_periods(&mut state, &config, date(2026, 5, 20)).is_err());
        config.synthesis.weekly.enabled = false;
        let personal = config.personal.as_mut().unwrap();
        personal.quarterly.enabled = false;
        personal.annual.enabled = false;
        recovery(&mut config, date(2026, 1, 15), 2);
        let result = reconcile_periods(&mut state, &config, date(2026, 5, 20)).unwrap();
        assert_eq!(result.examined, 2);
        assert_eq!(
            result.requested,
            ["monthly:2026-02-01", "monthly:2026-03-01"]
        );
        assert!(result.remaining);
        assert!(state.batches.is_empty());
        assert!(
            closing_day_ready(&state, &config, &state.intents["monthly:2026-02-01"].period)
                .is_err()
        );
        state.intents.get_mut("monthly:2026-02-01").unwrap().status = Status::Complete;
        state = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        let result = reconcile_periods(&mut state, &config, date(2026, 5, 20)).unwrap();
        assert_eq!(result.requested, ["monthly:2026-04-01"]);
        assert!(!result.remaining);
        assert_eq!(state.intents["monthly:2026-02-01"].status, Status::Complete);
        assert_eq!(
            reconcile_periods(&mut state, &config, date(2026, 5, 20))
                .unwrap()
                .examined,
            0
        );
    }

    #[test]
    fn period_recovery_rotates_modes_and_pauses_disabled_modes_without_losing_their_cursor() {
        let (_temp, mut config) = config();
        config.synthesis.weekly.include_sources = vec!["news".into()];
        recovery(&mut config, date(2026, 1, 1), 2);
        let mut state = State::default();
        let first = reconcile_periods(&mut state, &config, date(2026, 10, 20)).unwrap();
        assert_eq!(
            first.requested,
            ["weekly-themes:2026-01-05", "weekly-review:2026-01-05"]
        );
        let second = reconcile_periods(&mut state, &config, date(2026, 10, 20)).unwrap();
        assert_eq!(
            second.requested,
            ["monthly:2026-01-01", "quarterly:2026-01-01"]
        );
        let paused = state.recovery.as_ref().unwrap().next[&Kind::Monthly];
        config.personal.as_mut().unwrap().monthly.enabled = false;
        reconcile_periods(&mut state, &config, date(2026, 10, 20)).unwrap();
        assert_eq!(
            state.recovery.as_ref().unwrap().next[&Kind::Monthly],
            paused
        );
        config.personal.as_mut().unwrap().monthly.enabled = true;
        for _ in 0..3 {
            reconcile_periods(&mut state, &config, date(2026, 10, 20)).unwrap();
        }
        assert!(state.intents.contains_key("monthly:2026-02-01"));
    }

    #[test]
    fn period_recovery_first_install_and_timezone_changes_preserve_prior_intent_authority() {
        let (_temp, mut config) = config();
        recovery(&mut config, date(2026, 10, 20), 8);
        let mut state = State::default();
        let first = reconcile_periods(&mut state, &config, date(2026, 10, 20)).unwrap();
        assert_eq!(first.examined, 0);
        assert!(state.intents.is_empty());
        let explicit = ClosedPeriod::containing(Kind::Monthly, date(2026, 9, 1)).unwrap();
        request(&mut state, explicit, &config, date(2026, 10, 20)).unwrap();
        let old_timezone = state.intents["monthly:2026-09-01"].timezone.clone();
        config.vault.timezone = Some("Asia/Seoul".into());
        recovery(&mut config, date(2026, 1, 1), 1);
        reconcile_periods(&mut state, &config, date(2026, 10, 20)).unwrap();
        assert_eq!(state.intents["monthly:2026-09-01"].timezone, old_timezone);
        assert_ne!(state.recovery.as_ref().unwrap().timezone, old_timezone);
        assert!(state.batches.is_empty());
    }

    fn period() -> ClosedPeriod {
        ClosedPeriod::containing(Kind::Monthly, date(2026, 9, 15)).unwrap()
    }

    fn batch(config: &Config, outcome: ResponseOutcome, scope: ObservedScope) -> CollectionBatch {
        CollectionBatch {
            work_logs: BTreeMap::new(),
            at: "2026-10-01T08:00:00Z".parse().unwrap(),
            started_at: Some("2026-10-01T07:00:00Z".parse().unwrap()),
            targeted: false,
            requested_day: date(2026, 9, 30),
            full_ingest: true,
            sources: vec![ObservedSource {
                source_id: "mail".into(),
                personal_inputs: None,
                collection_identity: None,
                scope,
                outcome,
            }],
            source_scopes: BTreeMap::from([("mail".into(), source_scope(config, "mail").unwrap())]),
        }
    }

    fn window() -> ObservedScope {
        ObservedScope::Window {
            start: "2026-09-30T00:00:00Z".parse().unwrap(),
            end: "2026-10-01T00:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn personal_receipts_preserve_positive_configured_query_days_after_empty_targeted_requery() {
        let (_temp, mut config) = config();
        config.sources.get_mut("mail").unwrap().source_type = SourceType::Jira;
        let root = config.vault.root_path();
        std::fs::create_dir_all(&root).unwrap();
        let session = MutationSession::acquire(&root).unwrap();
        let day = date(2026, 9, 30);
        let mut positive = batch(
            &config,
            ResponseOutcome::Observed,
            ObservedScope::ConfiguredQuery,
        );
        positive.full_ingest = false;
        positive.targeted = true;
        positive.sources[0].personal_inputs = Some(lk_pipeline::WorkLogInputs {
            scope: lk_pipeline::work_log_scope(
                config.personal.as_ref().unwrap(),
                config.vault.locale(),
            ),
            days: BTreeMap::from([(day, "known-positive".into())]),
        });
        let mut empty = positive.clone();
        empty.sources[0]
            .personal_inputs
            .as_mut()
            .unwrap()
            .days
            .clear();
        let absent = BTreeMap::from([(day, WorkLogInput::Absent)]);
        let before = personal_work_logs_ready(&[positive.clone(), empty.clone()], &config, &absent)
            .is_err_and(|reason| reason.contains("no published work-log"));
        for receipt in [positive, empty] {
            record_collection(
                &config,
                &session,
                CollectionPublication {
                    requested_day: day,
                    full_ingest: false,
                    started_at: receipt.started_at.unwrap(),
                    targeted: true,
                    work_logs: BTreeMap::new(),
                },
                receipt.sources,
            )
            .unwrap();
        }
        let (state, _) = load(&root).unwrap();
        let after = personal_work_logs_ready(&state.batches, &config, &absent)
            .is_err_and(|reason| reason.contains("no published work-log"));
        assert_eq!((before, state.batches.len(), after), (true, 2, true));
    }

    #[test]
    fn personal_receipts_check_publication_policy_after_all_contributing_sources_are_removed() {
        let (_temp, config) = config();
        let day = date(2026, 9, 30);
        let mut publication = batch(&config, ResponseOutcome::Observed, window());
        publication.work_logs.insert(day, "published-input".into());
        publication.sources[0].personal_inputs = Some(lk_pipeline::WorkLogInputs {
            scope: lk_pipeline::work_log_scope(
                config.personal.as_ref().unwrap(),
                config.vault.locale(),
            ),
            days: BTreeMap::from([(day, "personal-input".into())]),
        });
        let logs = BTreeMap::from([(day, WorkLogInput::Bound("published-input".into()))]);
        for clear_tracking in [true, false] {
            let mut changed = config.clone();
            if clear_tracking {
                changed.personal.as_mut().unwrap().tracked_sources.clear();
            } else {
                changed.sources.get_mut("mail").unwrap().enabled = false;
                changed.personal.as_mut().unwrap().uncategorized_label =
                    Some("Changed category".into());
            }
            assert!(personal_sources(&changed).is_empty());
            assert!(
                personal_work_logs_ready(&[publication.clone()], &changed, &logs)
                    .unwrap_err()
                    .contains("publication policy")
            );
        }
    }

    #[test]
    fn personal_binding_distinguishes_published_inputs_empty_observations_and_unbound_history() {
        let (_temp, config) = config();
        let day = date(2026, 9, 30);
        let scope =
            lk_pipeline::work_log_scope(config.personal.as_ref().unwrap(), config.vault.locale());
        let mut full = batch(&config, ResponseOutcome::Observed, window());
        full.work_logs.insert(day, "published-input".into());
        full.sources[0].personal_inputs = Some(lk_pipeline::WorkLogInputs {
            scope: scope.clone(),
            days: BTreeMap::from([(day, "personal-input".into())]),
        });
        let logs = BTreeMap::from([(day, WorkLogInput::Bound("published-input".into()))]);
        assert!(personal_work_logs_ready(&[full.clone()], &config, &logs).is_ok());
        let mut filtered = full.clone();
        filtered.full_ingest = false;
        filtered.work_logs.clear();
        assert!(
            personal_work_logs_ready(&[full.clone(), filtered.clone()], &config, &logs).is_ok()
        );
        filtered.sources[0]
            .personal_inputs
            .as_mut()
            .unwrap()
            .days
            .clear();
        assert!(
            personal_work_logs_ready(&[full.clone(), filtered.clone()], &config, &logs)
                .unwrap_err()
                .contains("contributions changed")
        );
        for outcome in [ResponseOutcome::Partial, ResponseOutcome::Unverified] {
            filtered.sources[0].outcome = outcome;
            assert!(
                personal_work_logs_ready(&[full.clone(), filtered.clone()], &config, &logs)
                    .is_err()
            );
        }
        let absent = BTreeMap::from([(day, WorkLogInput::Absent)]);
        assert!(
            personal_work_logs_ready(&[full.clone()], &config, &absent)
                .unwrap_err()
                .contains("no published work-log")
        );
        for outcome in [ResponseOutcome::Partial, ResponseOutcome::Unverified] {
            let mut positive = full.clone();
            positive.sources[0].outcome = outcome;
            assert!(personal_work_logs_ready(&[positive], &config, &absent).is_err());
            let mut incomplete = full.clone();
            incomplete.sources[0].outcome = outcome;
            incomplete.sources[0]
                .personal_inputs
                .as_mut()
                .unwrap()
                .days
                .clear();
            assert!(
                personal_work_logs_ready(&[full.clone(), incomplete], &config, &absent).is_err()
            );
        }
        full.sources[0]
            .personal_inputs
            .as_mut()
            .unwrap()
            .days
            .clear();
        assert_eq!(
            personal_work_logs_ready(&[full.clone()], &config, &absent).unwrap(),
            0
        );
        full.sources[0].personal_inputs = None;
        assert_eq!(
            personal_work_logs_ready(&[full.clone()], &config, &absent).unwrap(),
            1
        );
        assert!(personal_work_logs_ready(&[full], &config, &logs).is_err());
    }

    #[test]
    fn different_lookbacks_bind_selected_contributions_without_inventing_empty_day_coverage() {
        let (_temp, mut config) = config();
        config
            .sources
            .insert("other".into(), config.sources["mail"].clone());
        config
            .personal
            .as_mut()
            .unwrap()
            .tracked_sources
            .push("other".into());
        let day = date(2026, 9, 30);
        let scope =
            lk_pipeline::work_log_scope(config.personal.as_ref().unwrap(), config.vault.locale());
        let mut full = batch(
            &config,
            ResponseOutcome::Observed,
            ObservedScope::Window {
                start: "2026-09-30T00:00:00Z".parse().unwrap(),
                end: "2026-10-21T00:00:00Z".parse().unwrap(),
            },
        );
        full.at = "2026-10-21T08:00:00Z".parse().unwrap();
        full.started_at = Some("2026-10-21T07:00:00Z".parse().unwrap());
        full.sources[0].personal_inputs = Some(lk_pipeline::WorkLogInputs {
            scope: scope.clone(),
            days: BTreeMap::from([(day, "selected-personal-input".into())]),
        });
        full.sources.push(ObservedSource {
            source_id: "other".into(),
            collection_identity: None,
            outcome: ResponseOutcome::Observed,
            scope: ObservedScope::Window {
                start: "2026-10-20T00:00:00Z".parse().unwrap(),
                end: "2026-10-21T00:00:00Z".parse().unwrap(),
            },
            personal_inputs: Some(lk_pipeline::WorkLogInputs {
                scope,
                days: BTreeMap::new(),
            }),
        });
        full.source_scopes
            .insert("other".into(), source_scope(&config, "other").unwrap());
        full.work_logs.insert(day, "selected-publication".into());
        let start = "2026-09-30T00:00:00Z".parse().unwrap();
        let end = "2026-10-01T00:00:00Z".parse().unwrap();
        assert!(latest_personal_receipt(&[full.clone()], "other", day, start, end).is_none());
        assert!(
            personal_work_logs_ready(
                &[full],
                &config,
                &BTreeMap::from([(day, WorkLogInput::Bound("selected-publication".into()))])
            )
            .is_ok()
        );
    }

    #[test]
    fn intent_retains_exact_period_and_refuses_open_period() {
        let (_temp, config) = config();
        let mut state = State::default();
        request(&mut state, period(), &config, date(2026, 10, 1)).unwrap();
        request(&mut state, period(), &config, date(2027, 1, 1)).unwrap();
        assert_eq!(state.intents.len(), 1);
        assert_eq!(
            state.intents.values().next().unwrap().period.end,
            date(2026, 9, 30)
        );
        assert!(
            request(
                &mut state,
                ClosedPeriod::containing(Kind::Monthly, date(2026, 10, 3)).unwrap(),
                &config,
                date(2026, 10, 4)
            )
            .is_err()
        );
        let mut invalid = period();
        invalid.end = date(2026, 9, 29);
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn partial_failed_and_unknown_responses_never_establish_readiness() {
        let (_temp, config) = config();
        for outcome in [
            ResponseOutcome::Partial,
            ResponseOutcome::Failed,
            ResponseOutcome::Unverified,
        ] {
            let state = State {
                batches: vec![
                    batch(&config, ResponseOutcome::Observed, window()),
                    batch(&config, outcome, window()),
                ],
                ..Default::default()
            };
            assert!(closing_day_ready(&state, &config, &period()).is_err());
        }
        let state = State {
            batches: vec![batch(
                &config,
                ResponseOutcome::Observed,
                ObservedScope::Unverified,
            )],
            ..Default::default()
        };
        assert!(closing_day_ready(&state, &config, &period()).is_err());
    }

    #[test]
    fn configured_query_has_explicit_date_limitation_and_unrelated_rss_is_not_a_dependency() {
        let (_temp, config) = config();
        assert_eq!(required_sources(&config, Kind::Monthly), vec!["mail"]);
        let state = State {
            batches: vec![batch(
                &config,
                ResponseOutcome::Observed,
                ObservedScope::ConfiguredQuery,
            )],
            ..Default::default()
        };
        let scope = closing_day_ready(&state, &config, &period()).unwrap();
        assert!(scope.contains("do not establish date coverage"));
    }

    #[test]
    fn preclose_and_filtered_batches_do_not_certify_cross_source_worklog() {
        let (_temp, config) = config();
        let mut receipt = batch(&config, ResponseOutcome::Observed, window());
        receipt.full_ingest = false;
        let mut state = State {
            batches: vec![receipt],
            ..Default::default()
        };
        assert!(closing_day_ready(&state, &config, &period()).is_err());
        state.batches[0].full_ingest = true;
        state.batches[0].at = "2026-09-30T23:00:00Z".parse().unwrap();
        assert!(closing_day_ready(&state, &config, &period()).is_err());
        state.batches[0].at = "2026-10-01T08:00:00Z".parse().unwrap();
        assert!(closing_day_ready(&state, &config, &period()).is_ok());
        state.batches[0]
            .source_scopes
            .insert("mail".into(), "old-config".into());
        assert!(closing_day_ready(&state, &config, &period()).is_err());
    }

    #[test]
    fn filtered_recovery_receipts_jointly_ready_weekly_themes_without_certifying_worklog() {
        let (_temp, mut config) = config();
        config
            .sources
            .insert("other".into(), config.sources["mail"].clone());
        config.synthesis.weekly.include_sources = vec!["mail".into(), "other".into()];
        config.personal.as_mut().unwrap().tracked_sources = vec!["mail".into(), "other".into()];
        let themes = ClosedPeriod::containing(Kind::WeeklyThemes, date(2026, 9, 30)).unwrap();
        let mut first = batch(
            &config,
            ResponseOutcome::Observed,
            ObservedScope::Window {
                start: "2026-10-04T00:00:00Z".parse().unwrap(),
                end: "2026-10-05T00:00:00Z".parse().unwrap(),
            },
        );
        first.at = "2026-10-05T08:00:00Z".parse().unwrap();
        first.started_at = Some("2026-10-05T07:00:00Z".parse().unwrap());
        first.requested_day = date(2026, 10, 4);
        first.targeted = true;
        first.full_ingest = false;
        let mut second = first.clone();
        second.sources[0].source_id = "other".into();
        second.source_scopes =
            BTreeMap::from([("other".into(), source_scope(&config, "other").unwrap())]);
        let state = State {
            batches: vec![first, second],
            ..Default::default()
        };
        assert!(closing_day_ready(&state, &config, &themes).is_ok());
        let review = ClosedPeriod::containing(Kind::WeeklyReview, date(2026, 9, 30)).unwrap();
        assert!(closing_day_ready(&state, &config, &review).is_err());
    }

    #[test]
    fn period_readiness_refuses_fetched_but_unpublished_closing_day() {
        let (_temp, config) = config();
        let mut receipt = batch(&config, ResponseOutcome::Observed, window());
        receipt.targeted = true;
        receipt.requested_day = date(2026, 10, 1);
        let state = State {
            batches: vec![receipt],
            ..Default::default()
        };
        assert!(closing_day_ready(&state, &config, &period()).is_err());
    }

    #[test]
    fn period_readiness_refuses_acquisition_before_close_even_if_publication_finishes_afterward() {
        let (_temp, config) = config();
        let mut receipt = batch(&config, ResponseOutcome::Observed, window());
        receipt.started_at = Some("2026-09-30T23:59:59Z".parse().unwrap());
        let mut state = State {
            batches: vec![receipt],
            ..Default::default()
        };
        assert!(closing_day_ready(&state, &config, &period()).is_err());
        state.batches[0].started_at = None;
        assert!(closing_day_ready(&state, &config, &period()).is_err());
    }

    #[test]
    fn filled_semantic_body_is_pending_and_empty_done_is_ready() {
        let key = TargetKind::WorkLogSynthesis.llm_inputs_key();
        let done = TargetKind::WorkLogSynthesis.completion_key();
        let heading = lk_core::i18n::Locale::default().strings().topic_summary;
        let pending = lk_core::frontmatter::parse_page(&format!(
            "---\ntype: work-log\nllm_inputs:\n  {key}: abc\n---\n## {heading}\n\nAlready written.\n"
        ))
        .unwrap();
        assert!(!page_semantics_ready(&pending));
        let completed = lk_core::frontmatter::parse_page(&format!(
            "---\ntype: work-log\nllm_inputs:\n  {key}: abc\n  {done}: abc\n---\n## {heading}\n"
        ))
        .unwrap();
        assert!(page_semantics_ready(&completed));
        assert_eq!(
            lk_pipeline::completed_input(&completed, TargetKind::WorkLogSynthesis),
            Some("abc")
        );
        let missing = lk_core::frontmatter::parse_page(&format!(
            "---\ntype: work-log\nllm_inputs:\n  {key}: abc\n  {done}: abc\n---\n"
        ))
        .unwrap();
        assert!(!page_semantics_ready(&missing));
    }

    #[tokio::test]
    async fn quiet_personal_period_keeps_source_semantic_guard_without_becoming_blocked_debt() {
        let (_temp, config) = config();
        let root = config.vault.root_path();
        std::fs::create_dir_all(&root).unwrap();
        let daily = VaultPath::daily(&config.vault.dirs, "mail", date(2026, 9, 30));
        let path = root.join(daily.as_ref());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let kind = TargetKind::DailySummary;
        let key = kind.llm_inputs_key();
        let done = kind.completion_key();
        let heading = lk_core::i18n::Locale::default().strings().summary;
        std::fs::write(&path,format!("---\ntype: daily\nllm_inputs:\n  {key}: captured\n---\n## {heading}\n\nTeam activity; no own events.\n")).unwrap();
        let mut intent = Intent::new(period(), &config).unwrap();
        let reader = FsVault::new(config.vault.root_path());
        assert!(
            input_pages(&reader, &config, &intent, &[])
                .await
                .unwrap()
                .is_err()
        );
        std::fs::write(&path,format!("---\ntype: daily\nllm_inputs:\n  {key}: captured\n  {done}: captured\n---\n## {heading}\n\nTeam activity; no own events.\n")).unwrap();
        let captured = input_pages(&reader, &config, &intent, &[])
            .await
            .unwrap()
            .ok()
            .unwrap();
        assert!(captured.pages.iter().any(|input| input.revision.is_some()));
        assert!(!captured.has_contributing_page);
        let mut state = State {
            batches: vec![batch(&config, ResponseOutcome::Observed, window())],
            ..Default::default()
        };
        let session = MutationSession::acquire(&root).unwrap();
        let llm = super::super::super::build_llm_client(&config, &root).unwrap();
        let ctx = Arc::new(PipelineContext::build(None, llm.clone(), &config).unwrap());
        let mut publisher = super::super::super::publication::Publication::new(&session, llm);
        let reader = FsVault::new(&root);
        let synth = Synthesizer::new(&root, ctx, &config, date(2026, 10, 1));
        process(
            &mut intent,
            &state,
            &config,
            &reader,
            &synth,
            &mut publisher,
            date(2026, 10, 1),
        )
        .await
        .unwrap();
        assert_eq!(intent.status, Status::NoData);
        // Identical missing work-log with a failed full-run response is not quiet.
        intent.status = Status::Pending;
        state.batches[0].sources[0].outcome = ResponseOutcome::Failed;
        process(
            &mut intent,
            &state,
            &config,
            &reader,
            &synth,
            &mut publisher,
            date(2026, 10, 1),
        )
        .await
        .unwrap();
        assert_eq!(intent.status, Status::Blocked);
    }

    #[tokio::test]
    async fn receipt_persists_outcomes_without_upgrading_partial_or_exposing_configuration() {
        let (_temp, config) = config();
        let root = config.vault.root_path();
        std::fs::create_dir_all(&root).unwrap();
        let session = MutationSession::acquire(&root).unwrap();
        record_collection(
            &config,
            &session,
            CollectionPublication {
                requested_day: date(2026, 9, 30),
                full_ingest: true,
                started_at: "2026-10-01T07:00:00Z".parse().unwrap(),
                targeted: false,
                work_logs: BTreeMap::new(),
            },
            vec![ObservedSource {
                source_id: "mail".into(),
                personal_inputs: None,
                collection_identity: None,
                scope: window(),
                outcome: ResponseOutcome::Partial,
            }],
        )
        .unwrap();
        let (state, original) = load(&root).unwrap();
        assert_eq!(
            state.batches[0].sources[0].outcome,
            ResponseOutcome::Partial
        );
        assert!(!original.unwrap().contains("test@example.invalid"));
        assert!(closing_day_ready(&state, &config, &period()).is_err());
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    use jiff::civil::date;

    #[test]
    fn explicit_rerequest_rebinds_timezone_without_preserving_completion() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, "vault:\n  root: vault\n  timezone: UTC\nidentity:\n  name: Test\n  email: test@example.invalid\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\npersonal: {}\n").unwrap();
        let mut config = Config::load(&path).unwrap();
        let period = ClosedPeriod::containing(Kind::Monthly, date(2026, 9, 1)).unwrap();
        let mut state = State::default();
        request(&mut state, period.clone(), &config, date(2026, 10, 1)).unwrap();
        let key = period.key();
        state.intents.get_mut(&key).unwrap().status = Status::Complete;
        config.vault.timezone = Some("Asia/Seoul".into());
        request(&mut state, period, &config, date(2026, 10, 1)).unwrap();
        assert_eq!(state.intents[&key].status, Status::Pending);
        assert_eq!(
            state.intents[&key].timezone,
            timezone_identity(&config).unwrap()
        );
        assert!(state.intents[&key].target_input.is_none());
    }

    #[test]
    fn output_configuration_changes_invalidate_input_without_changing_collection_scope() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.yaml");
        std::fs::write(&path, "vault:\n  root: vault\n  timezone: UTC\nidentity:\n  name: Test\n  email: test@example.invalid\nsources:\n  mail:\n    type: gmail\npersonal:\n  tracked_sources: [mail]\n").unwrap();
        let mut config = Config::load(&path).unwrap();
        let period = ClosedPeriod::containing(Kind::Annual, date(2026, 1, 1)).unwrap();
        let collection = source_scope(&config, "mail").unwrap();
        let mut output = input_scope(&config, &period).unwrap();
        config
            .personal
            .as_mut()
            .unwrap()
            .performance_categories
            .push("new-category".into());
        assert_ne!(output, input_scope(&config, &period).unwrap());
        assert_eq!(collection, source_scope(&config, "mail").unwrap());
        output = input_scope(&config, &period).unwrap();
        config.vault.locale = Some("en".into());
        assert_ne!(output, input_scope(&config, &period).unwrap());
        output = input_scope(&config, &period).unwrap();
        config.personal.as_mut().unwrap().quarterly.enabled = false;
        assert_ne!(output, input_scope(&config, &period).unwrap());
        assert_eq!(collection, source_scope(&config, "mail").unwrap());
    }
}
