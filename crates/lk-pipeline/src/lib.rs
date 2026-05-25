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

use lk_core::concept::ExtractedConcept;
use lk_core::config::{Config, DedupConfig, SourceConfig, SourceType};
use lk_core::event::{Event, RawItem};
use lk_vault::VaultReader;

use concepts::ConceptDrafts;

/// Helper for the `lore maintenance` CLI command: opens the dedup cache standalone.
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
    Vault(#[from] lk_vault::VaultError),
    #[error(transparent)]
    Llm(#[from] lk_llm::LlmError),
}

pub struct IngestResult {
    pub source_id: String,
    pub events: Vec<Event>,
    pub concepts: Vec<ExtractedConcept>,
    pub daily_pages: Vec<RenderOutput>,
    pub document_pages: Vec<RenderOutput>,
}

impl IngestResult {
    pub fn is_empty(&self) -> bool {
        self.daily_pages.is_empty() && self.document_pages.is_empty()
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
    /// Concept accumulation spans the WHOLE run, not a single `plan` call: a concept
    /// page is a cross-source aggregate, so two sources mentioning the same slug must
    /// merge into one page. Rendered once via `render_concept_pages` after all sources
    /// are planned.
    concept_drafts: tokio::sync::Mutex<ConceptDrafts>,
}

impl Pipeline {
    pub fn new(
        vault_root: &Path,
        ctx: Arc<PipelineContext>,
        config: &Config,
    ) -> Result<Self, PipelineError> {
        let dedup_path = vault_root.join(".lorekeeper").join("dedup.redb");
        let dedup = DedupCache::open(&dedup_path, config.dedup.title_threshold)?;
        Ok(Self::with_dedup(ctx, dedup, config, vault_root))
    }

    /// Pipeline for `--dry-run`: dedup is opened read-only and never creates the cache
    /// file, so a preview run leaves the vault untouched while still reflecting real
    /// dedup state when a cache already exists.
    pub fn new_dry_run(
        vault_root: &Path,
        ctx: Arc<PipelineContext>,
        config: &Config,
    ) -> Result<Self, PipelineError> {
        let dedup_path = vault_root.join(".lorekeeper").join("dedup.redb");
        let dedup = DedupCache::open_read_only(&dedup_path, config.dedup.title_threshold)?;
        Ok(Self::with_dedup(ctx, dedup, config, vault_root))
    }

    fn with_dedup(
        ctx: Arc<PipelineContext>,
        dedup: DedupCache,
        config: &Config,
        vault_root: &Path,
    ) -> Self {
        Self {
            ctx,
            dedup,
            reader: VaultReader::new(vault_root),
            dedup_config: config.dedup.clone(),
            run_lock: tokio::sync::Mutex::new(()),
            concept_drafts: tokio::sync::Mutex::new(ConceptDrafts::new()),
        }
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
        if config.classify_with_llm && !options.dry_run {
            classify::classify_with_llm(&mut events, &config.classify, &self.ctx.llm).await;
        }

        if config.source_type == SourceType::Manual {
            return self
                .plan_documents(source_id, config, events, options)
                .await;
        }

        let mut by_date: BTreeMap<jiff::civil::Date, Vec<usize>> = BTreeMap::new();
        for (i, event) in events.iter().enumerate() {
            by_date.entry(event.date).or_default().push(i);
        }

        let mut daily_pages: Vec<RenderOutput> = Vec::new();
        let mut all_concepts: Vec<ExtractedConcept> = Vec::new();

        // Normalize once: blank focus = no filter, identical across every provider path.
        let focus = config.normalized_focus();

        let existing_concepts = if config.extract_concepts {
            self.load_existing_concept_refs().await
        } else {
            vec![]
        };

        for (date, day_indices) in &by_date {
            let day_events: Vec<&Event> = day_indices.iter().map(|&i| &events[i]).collect();
            let combined: String = day_events
                .iter()
                .map(|e| format!("{}\n{}", e.title, e.body))
                .collect::<Vec<_>>()
                .join("\n---\n");

            let daily_path =
                lk_core::vault_path::VaultPath::daily(&self.ctx.dirs, source_id, *date).to_string();

            let summary = match self
                .ctx
                .llm
                .summarize(lk_llm::SummarizeRequest {
                    text: combined.clone(),
                    max_sentences: 5,
                    focus: focus.clone(),
                    locale: self.ctx.locale.tag().to_string(),
                    target: lk_llm::TaskTarget {
                        vault_path: daily_path.clone(),
                        kind: lk_llm::TargetKind::DailySummary,
                        anchor: format!("## {}", self.ctx.locale.strings().summary),
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
                    .extract_concepts(lk_llm::ExtractConceptsRequest {
                        text: combined.clone(),
                        source_id: source_id.to_string(),
                        date: *date,
                        focus: focus.clone(),
                        target: lk_llm::TaskTarget {
                            vault_path: daily_path,
                            kind: lk_llm::TargetKind::DailyConcepts,
                            anchor: format!("## {}", self.ctx.locale.strings().related_concepts),
                        },
                        existing_concepts: existing_concepts.clone(),
                        categories: self.ctx.concept_categories.clone(),
                    })
                    .await
                {
                    Ok(c) => {
                        let valid_cat_ids: Vec<&str> = self
                            .ctx
                            .concept_categories
                            .iter()
                            .map(|c| c.id.as_str())
                            .collect();
                        c.into_iter()
                            .filter(concepts::is_valid)
                            .map(|mut ec| {
                                if let Some(ref cat) = ec.category
                                    && !valid_cat_ids.contains(&cat.as_str())
                                {
                                    ec.category = None;
                                }
                                ec
                            })
                            .collect()
                    }
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
                for e in &day_events {
                    set.extend(e.labels.iter().cloned());
                }
                set.into_iter().collect()
            };

            let output = render::render_daily_page(
                &render::RenderContext {
                    source_id,
                    source_type: config.source_type,
                    date: *date,
                    events: &day_events,
                    labels: &labels,
                    summary: &summary,
                    concepts: &concept_names,
                    extract_concepts: config.extract_concepts,
                    locale: self.ctx.locale,
                },
                &self.ctx.engine,
                &self.ctx.dirs,
            )?;
            daily_pages.push(output);

            // Merge into the run-level accumulator (shared across all sources) so a
            // concept mentioned by multiple sources aggregates into one page.
            {
                let source = concepts::ConceptSource {
                    ref_path: concepts::strip_md_extension(
                        &lk_core::vault_path::VaultPath::daily(&self.ctx.dirs, source_id, *date)
                            .to_string(),
                    ),
                    date: *date,
                };
                let mut drafts = self.concept_drafts.lock().await;
                for concept in &day_concepts {
                    drafts
                        .merge(concept, &source, &self.reader, &self.ctx.dirs)
                        .await?;
                }
            }
            all_concepts.extend(day_concepts);
        }

        Ok(IngestResult {
            source_id: source_id.into(),
            events,
            concepts: all_concepts,
            daily_pages,
            document_pages: vec![],
        })
    }

    /// Render the concept pages accumulated across every `plan` call in this run.
    /// Call once after all sources are planned and before committing dedup.
    pub async fn render_concept_pages(&self) -> Result<Vec<RenderOutput>, PipelineError> {
        let drafts = self.concept_drafts.lock().await;
        drafts.render_pages(&self.ctx.engine, &self.ctx.dirs, self.ctx.locale)
    }

    /// Mark events as processed. Call AFTER vault writes succeed to avoid losing pages
    /// when a write fails midway.
    pub fn commit(&self, events: &[Event]) -> Result<(), PipelineError> {
        self.dedup.record(events)
    }

    pub async fn render_work_log(
        &self,
        personal_events: &[Event],
    ) -> Result<Vec<RenderOutput>, PipelineError> {
        worklog::render_work_log(
            personal_events,
            &self.ctx.perf,
            &self.ctx.engine,
            &self.ctx.dirs,
            self.ctx.locale,
            &self.ctx.llm,
        )
        .await
    }

    async fn plan_documents(
        &self,
        source_id: &str,
        config: &SourceConfig,
        events: Vec<Event>,
        _options: &IngestOptions,
    ) -> Result<IngestResult, PipelineError> {
        let focus = config.normalized_focus();

        let existing_concepts = if config.extract_concepts {
            self.load_existing_concept_refs().await
        } else {
            vec![]
        };

        let mut document_pages: Vec<RenderOutput> = Vec::new();
        let mut all_concepts: Vec<lk_core::concept::ExtractedConcept> = Vec::new();

        for event in &events {
            // Derive slug from title; fall back to source_file metadata.
            let slug = match lk_core::concept::slugify(&event.title) {
                Some(s) => s,
                None => {
                    let source_file = event
                        .metadata
                        .get("source_file")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match lk_core::concept::slugify(source_file) {
                        Some(s) => s,
                        None => {
                            tracing::warn!(
                                source = source_id,
                                title = %event.title,
                                "skipping document with empty slug"
                            );
                            continue;
                        }
                    }
                }
            };

            let vault_path =
                lk_core::vault_path::VaultPath::document(&self.ctx.dirs, &slug).to_string();

            let summary = match self
                .ctx
                .llm
                .summarize(lk_llm::SummarizeRequest {
                    text: format!("{}\n{}", event.title, event.body),
                    max_sentences: 5,
                    focus: focus.clone(),
                    locale: self.ctx.locale.tag().to_string(),
                    target: lk_llm::TaskTarget {
                        vault_path: vault_path.clone(),
                        kind: lk_llm::TargetKind::DocumentSummary,
                        anchor: format!("## {}", self.ctx.locale.strings().summary),
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
                        slug = %slug,
                        "document summarize failed; continuing without summary"
                    );
                    String::new()
                }
            };

            let doc_concepts: Vec<lk_core::concept::ExtractedConcept> = if config.extract_concepts {
                match self
                    .ctx
                    .llm
                    .extract_concepts(lk_llm::ExtractConceptsRequest {
                        text: format!("{}\n{}", event.title, event.body),
                        source_id: source_id.to_string(),
                        date: event.date,
                        focus: focus.clone(),
                        target: lk_llm::TaskTarget {
                            vault_path: vault_path.clone(),
                            kind: lk_llm::TargetKind::DocumentConcepts,
                            anchor: format!("## {}", self.ctx.locale.strings().related_concepts),
                        },
                        existing_concepts: existing_concepts.clone(),
                        categories: self.ctx.concept_categories.clone(),
                    })
                    .await
                {
                    Ok(c) => {
                        let valid_cat_ids: Vec<&str> = self
                            .ctx
                            .concept_categories
                            .iter()
                            .map(|c| c.id.as_str())
                            .collect();
                        c.into_iter()
                            .filter(concepts::is_valid)
                            .map(|mut ec| {
                                if let Some(ref cat) = ec.category
                                    && !valid_cat_ids.contains(&cat.as_str())
                                {
                                    ec.category = None;
                                }
                                ec
                            })
                            .collect()
                    }
                    Err(e) if e.is_fatal() => return Err(PipelineError::Llm(e)),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            source = source_id,
                            slug = %slug,
                            "document concept extraction failed; continuing without concepts"
                        );
                        vec![]
                    }
                }
            } else {
                vec![]
            };

            let concept_names: Vec<String> = doc_concepts.iter().map(|c| c.name.clone()).collect();

            match render::render_document_page(
                &render::DocumentRenderContext {
                    slug: &slug,
                    event,
                    summary: &summary,
                    concepts: &concept_names,
                    extract_concepts: config.extract_concepts,
                    locale: self.ctx.locale,
                },
                &self.ctx.engine,
                &self.ctx.dirs,
            ) {
                Ok(output) => document_pages.push(output),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        source = source_id,
                        slug = %slug,
                        "document render failed; skipping"
                    );
                    continue;
                }
            }

            // Merge concepts into run-level accumulator.
            {
                let source = concepts::ConceptSource {
                    ref_path: concepts::strip_md_extension(
                        &lk_core::vault_path::VaultPath::document(&self.ctx.dirs, &slug)
                            .to_string(),
                    ),
                    date: event.date,
                };
                let mut drafts = self.concept_drafts.lock().await;
                for concept in &doc_concepts {
                    drafts
                        .merge(concept, &source, &self.reader, &self.ctx.dirs)
                        .await?;
                }
            }
            all_concepts.extend(doc_concepts);
        }

        Ok(IngestResult {
            source_id: source_id.into(),
            events,
            concepts: all_concepts,
            daily_pages: vec![],
            document_pages,
        })
    }

    async fn load_existing_concept_refs(&self) -> Vec<lk_llm::ExistingConceptRef> {
        let concept_dir = std::path::Path::new(&self.ctx.dirs.wiki).join("concepts");
        let files = match self.reader.list_markdown(&concept_dir).await {
            Ok(f) => f,
            Err(_) => return vec![],
        };

        let mut refs = Vec::with_capacity(files.len());
        for file in &files {
            if let Ok(Some(page)) = self.reader.read_page(file).await {
                let slug = page
                    .frontmatter
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let name = page
                    .frontmatter
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !slug.is_empty() && !name.is_empty() {
                    refs.push(lk_llm::ExistingConceptRef {
                        slug: slug.to_string(),
                        name: name.to_string(),
                    });
                }
            }
        }

        let drafts = self.concept_drafts.lock().await;
        for (slug, name) in drafts.known_slugs_and_names() {
            if !refs.iter().any(|r| r.slug == slug) {
                refs.push(lk_llm::ExistingConceptRef { slug, name });
            }
        }

        refs
    }
}

fn empty_result(source_id: &str) -> IngestResult {
    IngestResult {
        source_id: source_id.into(),
        events: vec![],
        concepts: vec![],
        daily_pages: vec![],
        document_pages: vec![],
    }
}
