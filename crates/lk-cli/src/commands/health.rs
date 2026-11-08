use std::path::Path;

use lk_core::config::{Config, SourceType};

use super::{find_config, load_config};

/// A source is stale once TWO ingest fires have come due since its last success (one missed run
/// of grace) — anchored at the last success, NOT at `now`, so the window follows the real
/// schedule sequence including weekend and off-day gaps. For `0 9 * * 1-5` a Friday-morning
/// success is not stale over the weekend (the next fire is Monday); it goes stale once
/// Tuesday's fire has also passed unfilled. Ingestion is a single all-source run
/// (`ingest.schedule`), so every source shares one cadence; with none set, a flat window
/// applies.
const DEFAULT_STALE_AFTER_SECS: i64 = 48 * 3600;

/// When each enabled source was last collected, and whether that is recent enough.
pub(crate) struct Freshness {
    pub sources: Vec<SourceFreshness>,
}

pub(crate) struct SourceFreshness {
    pub id: String,
    pub source_type: SourceType,
    /// `None` for a source that has never been collected — which a first install has, and a
    /// permanently broken one is indistinguishable from without `--strict`.
    pub last: Option<jiff::Timestamp>,
    pub latest_attempt: Option<lk_vault::LogEntry>,
    pub stale: bool,
}

impl SourceFreshness {
    pub fn hours_ago(&self, now: jiff::Timestamp) -> Option<i64> {
        self.last
            .map(|last| (now.as_second() - last.as_second()) / 3600)
    }
}

impl Freshness {
    pub fn fresh(&self) -> usize {
        self.sources
            .iter()
            .filter(|s| s.last.is_some() && !s.stale)
            .count()
    }

    pub fn stale(&self) -> usize {
        self.sources.iter().filter(|s| s.stale).count()
    }

    pub fn never(&self) -> usize {
        self.sources.iter().filter(|s| s.last.is_none()).count()
    }
}

/// Read the ingest log — the sole evidence a source is alive — and decide each source's
/// currency against the configured cadence.
pub(crate) async fn freshness(
    config: &Config,
    vault_root: &Path,
    now: jiff::Timestamp,
) -> miette::Result<Freshness> {
    let tz = config.vault.timezone();
    let log = lk_vault::IngestLog::new(vault_root.join(".lorekeeper").join("ingest.jsonl"));
    let schedule = config.ingest.schedule.as_deref();

    let is_stale = |last: jiff::Timestamp| -> bool {
        match schedule
            .and_then(|expr| lk_core::cron::next_fire_after(expr, last, &tz))
            .and_then(|first| {
                schedule.and_then(|expr| lk_core::cron::next_fire_after(expr, first, &tz))
            }) {
            Some(second_due) => now > second_due,
            // No schedule, or one that will not parse post-validation: the flat window.
            None => now.as_second() - last.as_second() > DEFAULT_STALE_AFTER_SECS,
        }
    };

    let mut states = log
        .load_source_states()
        .await
        .map_err(|e| miette::miette!("read ingest log: {e}"))?;
    let mut sources = Vec::new();
    for (id, sc) in config.enabled_sources() {
        let state = states.remove(id).unwrap_or_default();
        let last = state.last_collection.map(|entry| entry.timestamp);
        sources.push(SourceFreshness {
            id: id.to_string(),
            source_type: sc.source_type,
            stale: last.is_some_and(is_stale),
            last,
            latest_attempt: state.latest_attempt,
        });
    }
    Ok(Freshness { sources })
}

impl Freshness {
    /// The contract a skill reads, as `lore health --json`.
    ///
    /// `hours_ago` is resolved here rather than left as a timestamp to subtract, for the reason
    /// every other view resolves what it reports: the reader is deciding whether to say
    /// something, not doing arithmetic.
    /// The exit code answers for the subsystem whichever form was asked for: a caller that
    /// reads the JSON still learns the verdict from the code, and one that reads neither still
    /// fails its pipeline.
    fn verdict(&self, strict: bool) -> miette::Result<()> {
        if self.stale() > 0 || (strict && self.never() > 0) {
            std::process::exit(1);
        }
        Ok(())
    }

    fn as_json(&self, now: jiff::Timestamp) -> serde_json::Value {
        serde_json::json!({
            "fresh": self.fresh(),
            "stale": self.stale(),
            "never": self.never(),
            "sources": self.sources.iter().map(|source| serde_json::json!({
                "id": source.id,
                "type": source.source_type.to_string(),
                "last": source.last.map(|at| at.to_string()),
                "hours_ago": source.hours_ago(now),
                "stale": source.stale,
                "latest_attempt": source.latest_attempt.as_ref().map(|entry| serde_json::json!({
                    "timestamp": entry.timestamp.to_string(),
                    "hours_ago": (now.as_second() - entry.timestamp.as_second()) / 3600,
                    "status": entry.status,
                    "observation": entry.observation,
                    "event_count": entry.event_count,
                })),
            })).collect::<Vec<_>>(),
        })
    }
}

pub async fn run(opts: &super::GlobalOptions, strict: bool, json: bool) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let now = jiff::Timestamp::now();
    let report = freshness(&config, &config.vault.root_path(), now).await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report.as_json(now))
                .map_err(|e| miette::miette!("{e}"))?
        );
        return report.verdict(strict);
    }

    for source in &report.sources {
        match source.hours_ago(now) {
            Some(hours) if source.stale => {
                eprintln!(
                    "⚠ {} ({}) — {hours}h ago, STALE",
                    source.id, source.source_type
                );
            }
            Some(hours) => eprintln!("✓ {} ({}) — {hours}h ago", source.id, source.source_type),
            None => eprintln!("✗ {} ({}) — never ingested", source.id, source.source_type),
        }
        if let Some(attempt) = &source.latest_attempt {
            let observation = match attempt.observation {
                Some(lk_core::event::ObservationOutcome::Observed) => "observed",
                Some(lk_core::event::ObservationOutcome::Partial) => "partial",
                Some(lk_core::event::ObservationOutcome::Unverified) => "unverified",
                None => "unknown",
            };
            let status = match attempt.status {
                lk_vault::LogStatus::Success => "success",
                lk_vault::LogStatus::Failed => "failed",
                lk_vault::LogStatus::Skipped => "skipped",
            };
            eprintln!(
                "  latest attempt: {} · {status} · observation {observation} · {} event(s)",
                attempt.timestamp, attempt.event_count
            );
        }
    }

    let (fresh, stale, never) = (report.fresh(), report.stale(), report.never());
    eprintln!("\n{fresh} fresh, {stale} stale, {never} never");
    report.verdict(strict)
}
