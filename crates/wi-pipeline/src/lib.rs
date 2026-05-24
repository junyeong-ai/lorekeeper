mod classify;
mod concepts;
mod context;
mod dedup;
mod normalize;
pub mod render;
mod synthesis;
mod worklog;

pub use context::PipelineContext;
pub use dedup::DedupCache;
pub use render::RenderOutput;
pub use synthesis::Synthesizer;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use thiserror::Error;

use wi_core::concept::ExtractedConcept;
use wi_core::config::{Config, DedupConfig, SourceConfig};
use wi_core::event::{Event, RawItem};
use wi_vault::VaultReader;

use concepts::ConceptDrafts;

/// Helper for the `wi maintenance` CLI command: opens the dedup cache standalone.
pub fn dedup_cache_for_maintenance(
    path: &Path,
    config: &Config,
) -> Result<DedupCache, PipelineError> {
    DedupCache::open(path, config.dedup.title_threshold)
}

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("dedup: {0}")]
    Dedup(String),
    #[error("render: {0}")]
    Render(String),
    #[error(transparent)]
    Vault(#[from] wi_vault::VaultError),
    #[error(transparent)]
    Llm(#[from] wi_llm::LlmError),
}

pub struct IngestResult {
    pub source_id: String,
    pub events: Vec<Event>,
    pub concepts: Vec<ExtractedConcept>,
    pub daily_pages: Vec<RenderOutput>,
    pub concept_pages: Vec<RenderOutput>,
}

impl IngestResult {
    pub fn is_empty(&self) -> bool {
        self.daily_pages.is_empty() && self.concept_pages.is_empty()
    }

    pub fn page_count(&self) -> usize {
        self.daily_pages.len() + self.concept_pages.len()
    }
}

pub struct IngestOptions {
    pub dry_run: bool,
    pub force: bool,
    pub target_date: Option<jiff::civil::Date>,
}

pub struct Pipeline {
    ctx: Arc<PipelineContext>,
    dedup: DedupCache,
    reader: VaultReader,
    dedup_config: DedupConfig,
    run_lock: tokio::sync::Mutex<()>,
}

impl Pipeline {
    pub fn new(
        vault_root: &Path,
        ctx: Arc<PipelineContext>,
        config: &Config,
    ) -> Result<Self, PipelineError> {
        let dedup_path = vault_root.join(".wiki-ingest").join("dedup.redb");
        let dedup = DedupCache::open(&dedup_path, config.dedup.title_threshold)?;
        let reader = VaultReader::new(vault_root);

        Ok(Self {
            ctx,
            dedup,
            reader,
            dedup_config: config.dedup.clone(),
            run_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Build daily and concept pages without recording dedup. The caller writes pages
    /// to the vault, then calls `commit` to mark events as seen.
    ///
    /// Concurrency: `plan` serializes via an internal mutex, so a second concurrent
    /// `plan` blocks until the first returns. The mutex is NOT held across the
    /// subsequent `commit` call, so callers running multiple pipelines against the
    /// same dedup cache must externally serialize the plan→write→commit sequence
    /// to avoid race-induced duplicates.
    pub async fn plan(
        &self,
        source_id: &str,
        config: &SourceConfig,
        items: Vec<RawItem>,
        options: &IngestOptions,
    ) -> Result<IngestResult, PipelineError> {
        let _guard = self.run_lock.lock().await;

        let mut events =
            normalize::normalize(source_id, config.source_type, items, &self.ctx.timezone);
        tracing::info!(source = source_id, raw = events.len(), "normalized");

        if let Some(target) = options.target_date {
            events.retain(|e| e.date == target);
            tracing::info!(target = %target, kept = events.len(), "filtered by --date");
        }

        if events.is_empty() {
            return Ok(empty_result(source_id));
        }

        if !options.force {
            events = self.dedup.deduplicate(events, &self.dedup_config.cascade)?;
            tracing::info!(source = source_id, novel = events.len(), "dedup");
        }

        if events.is_empty() {
            return Ok(empty_result(source_id));
        }

        classify::assign_static_labels(&mut events, &config.labels);
        if config.track_personal {
            classify::flag_personal(&mut events, &self.ctx.identity);
        }
        classify::classify_by_keywords(&mut events, &config.classify);

        let mut by_date: BTreeMap<jiff::civil::Date, Vec<Event>> = BTreeMap::new();
        for event in events.clone() {
            by_date.entry(event.date).or_default().push(event);
        }

        let mut daily_pages: Vec<RenderOutput> = Vec::new();
        let mut all_concepts: Vec<ExtractedConcept> = Vec::new();
        let mut concept_drafts = ConceptDrafts::new();

        for (date, day_events) in &by_date {
            let combined: String = day_events
                .iter()
                .map(|e| format!("{}\n{}", e.title, e.body))
                .collect::<Vec<_>>()
                .join("\n---\n");

            let daily_path =
                wi_core::vault_path::VaultPath::daily(&self.ctx.dirs, source_id, *date).to_string();

            let summary = match self
                .ctx
                .llm
                .summarize(wi_llm::SummarizeRequest {
                    text: combined.clone(),
                    max_sentences: 5,
                    target: wi_llm::TaskTarget {
                        vault_path: daily_path.clone(),
                        kind: wi_llm::TargetKind::DailySummary,
                    },
                })
                .await
            {
                Ok(s) => s,
                Err(e) if e.is_fatal() => return Err(PipelineError::Llm(e)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        source = source_id,
                        date = %date,
                        "summarize failed; continuing without summary"
                    );
                    String::new()
                }
            };

            let day_concepts: Vec<ExtractedConcept> = if config.extract_concepts {
                match self
                    .ctx
                    .llm
                    .extract_concepts(wi_llm::ExtractConceptsRequest {
                        text: combined.clone(),
                        source_id: source_id.to_string(),
                        date: *date,
                        target: wi_llm::TaskTarget {
                            vault_path: daily_path,
                            kind: wi_llm::TargetKind::DailyConcepts,
                        },
                    })
                    .await
                {
                    Ok(c) => c.into_iter().filter(concepts::is_valid).collect(),
                    Err(e) if e.is_fatal() => return Err(PipelineError::Llm(e)),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            source = source_id,
                            date = %date,
                            "concept extraction failed; continuing without concepts"
                        );
                        vec![]
                    }
                }
            } else {
                vec![]
            };

            let concept_names: Vec<String> = day_concepts.iter().map(|c| c.name.clone()).collect();

            let labels: Vec<String> = {
                let mut set = std::collections::BTreeSet::new();
                for e in day_events {
                    set.extend(e.labels.iter().cloned());
                }
                set.into_iter().collect()
            };

            let output = render::render_daily_page(
                &render::RenderContext {
                    source_id,
                    source_type: config.source_type,
                    date: *date,
                    events: day_events,
                    labels: &labels,
                    summary: &summary,
                    concepts: &concept_names,
                },
                &self.ctx.engine,
                &self.ctx.dirs,
            )?;
            daily_pages.push(output);

            for concept in &day_concepts {
                concept_drafts
                    .merge(concept, source_id, *date, &self.reader, &self.ctx.dirs)
                    .await?;
            }
            all_concepts.extend(day_concepts);
        }

        let concept_pages = concept_drafts.render(&self.ctx.engine, &self.ctx.dirs)?;

        Ok(IngestResult {
            source_id: source_id.into(),
            events,
            concepts: all_concepts,
            daily_pages,
            concept_pages,
        })
    }

    /// Mark events as processed. Call AFTER vault writes succeed to avoid losing pages
    /// when a write fails midway.
    pub fn commit(&self, events: &[Event]) -> Result<(), PipelineError> {
        self.dedup.record(events)
    }

    pub fn aggregate_work_log(
        &self,
        personal_events: &[Event],
    ) -> Result<Vec<RenderOutput>, PipelineError> {
        worklog::aggregate_and_render(
            personal_events,
            &self.ctx.perf,
            &self.ctx.engine,
            &self.ctx.dirs,
        )
    }
}

fn empty_result(source_id: &str) -> IngestResult {
    IngestResult {
        source_id: source_id.into(),
        events: vec![],
        concepts: vec![],
        daily_pages: vec![],
        concept_pages: vec![],
    }
}
