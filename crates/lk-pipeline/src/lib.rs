mod classify;
mod concept_draft;
mod context;
mod dedup;
mod llm_cache;
mod normalize;
pub mod render;
mod synthesis;
mod work_log;

pub use context::PipelineContext;
pub use dedup::DedupCache;
pub use render::RenderResult;
pub use synthesis::Synthesizer;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use thiserror::Error;

use lk_core::concept::ExtractedConcept;
use lk_core::config::{Config, DedupConfig, SourceConfig, SourceType};
use lk_core::event::{Event, RawItem};
use lk_vault::VaultReader;

use concept_draft::ConceptDrafts;

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("dedup: {0}")]
    Dedup(String),
    #[error("render: {0}")]
    Render(String),
    #[error(transparent)]
    Vault(#[from] lk_vault::VaultError),
    #[error(transparent)]
    Llm(#[from] lk_queue::LlmError),
}

pub struct IngestResult {
    pub source_id: String,
    pub events: Vec<Event>,
    /// Events recognized as duplicates this run. Carried so `commit` can refresh
    /// their dedup timestamps (not rendered).
    pub duplicates: Vec<Event>,
    pub concepts: Vec<ExtractedConcept>,
    pub daily_pages: Vec<RenderResult>,
    pub document_pages: Vec<RenderResult>,
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
        let dedup = DedupCache::open(&dedup_path, config.dedup.extra_tracking_params.clone())?;
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
        let dedup =
            DedupCache::open_read_only(&dedup_path, config.dedup.extra_tracking_params.clone())?;
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
            return Ok(empty_result(source_id, vec![]));
        }

        // Duplicates are retained (not just dropped) so `commit` can refresh their
        // `seen_at` — a recurring item recognized as a duplicate must not age out of
        // the retention window and re-emit as new.
        //
        // Mutable source types (Jira/Calendar) re-render with the latest upstream
        // state every run: their items change after first sight (status, assignee,
        // scheduled→actual), so dedup-by-event-id would freeze the first snapshot.
        // They bypass dedup ENTIRELY for themselves — every strategy, not just
        // event-id — because a re-render is the intended behaviour: matching a prior
        // snapshot by content-hash/url would suppress the very update we want, and a
        // cross-source content collision (a Jira issue whose text happens to equal a
        // mail body) must NOT drop the distinct issue from its own daily page. This is
        // the per-source equivalent of `--force`, which is why the daily job needs no
        // blanket flag. Records still land in the cache via `commit` below (as novel),
        // so cache growth/retention is unaffected; unchanged content still skips LLM
        // work via the materialized-view cache, so the re-render is cheap.
        // Append-only types keep full dedup: re-seeing an item is a true duplicate.
        let bypass_dedup = options.force || config.source_type.is_mutable();
        let duplicates = if !bypass_dedup {
            let result = self.dedup.dedup(events, &self.dedup_config.cascade)?;
            events = result.novel;
            tracing::info!(source = source_id, novel = events.len(), "dedup");
            result.duplicates
        } else {
            Vec::new()
        };

        if events.is_empty() {
            return Ok(empty_result(source_id, duplicates));
        }

        classify::assign_labels(&mut events, &config.labels);
        classify::mark_personal(&mut events, config.track_personal);
        classify::classify_by_keywords(&mut events, &config.classify);

        if config.source_type == SourceType::Manual {
            return self
                .plan_documents(source_id, config, events, duplicates, options)
                .await;
        }

        let mut by_date: BTreeMap<jiff::civil::Date, Vec<usize>> = BTreeMap::new();
        for (i, event) in events.iter().enumerate() {
            by_date.entry(event.date).or_default().push(i);
        }

        let mut daily_pages: Vec<RenderResult> = Vec::new();
        let mut all_concepts: Vec<ExtractedConcept> = Vec::new();

        // Normalize once: blank focus = no filter, identical across every provider path.
        let focus = config.normalized_focus();

        let existing_concepts = if config.extract_concepts {
            self.load_existing_concept_refs().await?
        } else {
            vec![]
        };

        let strings = self.ctx.locale.strings();
        let summary_heading = strings.summary;
        let events_heading = config.source_type.events_heading(strings);
        let concepts_heading = strings.related_concepts;

        for (date, day_indices) in &by_date {
            let day_events: Vec<&Event> = day_indices.iter().map(|&i| &events[i]).collect();
            let combined: String = day_events
                .iter()
                .map(|e| format!("{}\n{}", e.title, e.body))
                .collect::<Vec<_>>()
                .join("\n---\n");

            let daily_path =
                lk_core::vault_path::VaultPath::daily(&self.ctx.dirs, source_id, *date).to_string();

            // Read the existing page ONCE per date so all three section decisions
            // share the same view; the new render plus splice produces the final bytes.
            // `read_page` already maps NotFound to Ok(None); only real I/O or YAML
            // errors propagate. Silently degrading those to "cache miss" would let
            // a transient failure overwrite LLM-owned bodies — abort instead.
            let existing = self.reader.read_page(Path::new(&daily_path)).await?;

            let summary_req = lk_queue::SummarizeRequest {
                text: combined.clone(),
                max_sentences: 5,
                focus: focus.clone(),
                locale: self.ctx.locale.tag().to_string(),
                source_type: Some(config.source_type),
                target: lk_queue::TaskTarget {
                    vault_path: daily_path.clone(),
                    kind: lk_queue::TargetKind::DailySummary,
                    anchor: format!("## {summary_heading}"),
                },
            };
            let summary_decision = llm_cache::lookup(
                existing.as_ref(),
                summary_req.target.kind.llm_inputs_key(),
                summary_heading,
                summary_req.cache_hash(),
            );

            let refine_req = lk_queue::SummarizeRequest {
                text: combined.clone(),
                // Refine rewrites each event body in place (2–5 sentences per event,
                // per the skill); it does not emit a flat N-sentence summary. This is a
                // loose upper bound, NOT coupled to the event count — coupling it would
                // churn the cache hash every time a same-day source gains an event.
                max_sentences: 20,
                focus: focus.clone(),
                locale: self.ctx.locale.tag().to_string(),
                source_type: Some(config.source_type),
                target: lk_queue::TaskTarget {
                    vault_path: daily_path.clone(),
                    kind: lk_queue::TargetKind::DailyRefineEvents,
                    anchor: format!("## {events_heading}"),
                },
            };
            // The event list is an in-place rewrite: the render populates it, so
            // completion is tracked by a separate frontmatter key the skill stamps,
            // not by the section being non-empty.
            let refine_completion_key = match refine_req.target.kind.cache_shape() {
                lk_queue::CacheShape::InPlace { completion_key } => completion_key,
                lk_queue::CacheShape::FillEmpty => {
                    unreachable!("DailyRefineEvents is an in-place rewrite")
                }
            };
            let refine_decision = llm_cache::lookup_in_place(
                existing.as_ref(),
                refine_completion_key,
                events_heading,
                refine_req.cache_hash(),
            );

            let summary = if summary_decision.enqueue() {
                match self.ctx.llm.summarize(summary_req).await {
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
                }
            } else {
                tracing::debug!(source = source_id, date = %date, "summary cached; skipping enqueue");
                String::new()
            };

            if refine_decision.enqueue()
                && let Err(e) = self.ctx.llm.summarize(refine_req).await
            {
                if e.is_fatal() {
                    return Err(PipelineError::Llm(e));
                }
                tracing::warn!(error = %e, "refine-events task failed; events stay as raw");
            } else if !refine_decision.enqueue() {
                tracing::debug!(source = source_id, date = %date, "refine-events cached; skipping enqueue");
            }

            let (concepts_decision, day_concepts) = if config.extract_concepts {
                let concepts_req = lk_queue::ExtractConceptsRequest {
                    text: combined.clone(),
                    source_id: source_id.to_string(),
                    source_type: config.source_type,
                    date: *date,
                    focus: focus.clone(),
                    target: lk_queue::TaskTarget {
                        vault_path: daily_path.clone(),
                        kind: lk_queue::TargetKind::DailyConcepts,
                        anchor: format!("## {concepts_heading}"),
                    },
                    existing_concepts: existing_concepts.clone(),
                    categories: self.ctx.concept_categories.clone(),
                };
                let decision = llm_cache::lookup(
                    existing.as_ref(),
                    concepts_req.target.kind.llm_inputs_key(),
                    concepts_heading,
                    concepts_req.cache_hash(),
                );
                let extracted = if decision.enqueue() {
                    match self.ctx.llm.extract_concepts(concepts_req).await {
                        Ok(c) => filter_valid_concepts(c, &self.ctx.concept_categories),
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
                    tracing::debug!(source = source_id, date = %date, "concepts cached; skipping enqueue");
                    vec![]
                };
                (Some(decision), extracted)
            } else {
                (None, vec![])
            };

            let concept_names: Vec<String> = day_concepts.iter().map(|c| c.name.clone()).collect();

            let labels: Vec<String> = {
                let mut set = std::collections::BTreeSet::new();
                for e in &day_events {
                    set.extend(e.labels.iter().cloned());
                }
                set.into_iter().collect()
            };

            // `refine_events` is pre-stamped with the current-input hash (the
            // stale-task reference point, like summary); `refine_events_done` is the
            // skill-owned completion stamp, passed through from disk.
            let refine_done_stamp =
                llm_cache::stored_hash(existing.as_ref(), refine_completion_key);

            let llm_inputs = render::LlmInputHashes {
                summary: &summary_decision.hash,
                refine_events: &refine_decision.hash,
                refine_events_done: refine_done_stamp,
                concepts: concepts_decision.as_ref().map(|d| d.hash.as_str()),
            };

            let fresh = render::render_daily_page(
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
                    llm_inputs,
                },
                &self.ctx.engine,
                &self.ctx.dirs,
            )?;

            let mut splices: Vec<(&str, &llm_cache::SectionDecision)> = vec![
                (summary_heading, &summary_decision),
                (events_heading, &refine_decision),
            ];
            if let Some(d) = concepts_decision.as_ref() {
                splices.push((concepts_heading, d));
            }
            let content = render::splice_preserved_sections(fresh.content, splices);

            daily_pages.push(render::RenderResult {
                path: fresh.path,
                content,
            });

            // Merge into the run-level accumulator (shared across all sources) so a
            // concept mentioned by multiple sources aggregates into one page.
            {
                let mut drafts = self.concept_drafts.lock().await;
                for concept in &day_concepts {
                    drafts
                        .merge(concept, *date, &self.reader, &self.ctx.dirs)
                        .await?;
                }
            }
            all_concepts.extend(day_concepts);
        }

        Ok(IngestResult {
            source_id: source_id.into(),
            events,
            duplicates,
            concepts: all_concepts,
            daily_pages,
            document_pages: vec![],
        })
    }

    /// Render the concept pages accumulated across every `plan` call in this run.
    /// Call once after all sources are planned and before committing dedup.
    pub async fn render_concept_pages(&self) -> Result<Vec<RenderResult>, PipelineError> {
        let drafts = self.concept_drafts.lock().await;
        drafts.render_pages(&self.ctx.engine, &self.ctx.dirs, self.ctx.locale)
    }

    /// Mark this run's events as processed. Call AFTER vault writes succeed to avoid
    /// losing pages when a write fails midway. Records the `novel` events (first sight)
    /// AND refreshes the `duplicates`' timestamps (re-seen) so steady-state re-arrivals
    /// never age out of the retention window and re-emit as new.
    pub fn commit(&self, novel: &[Event], duplicates: &[Event]) -> Result<(), PipelineError> {
        self.dedup.record(novel.iter().chain(duplicates))
    }

    pub async fn render_work_log(
        &self,
        personal_events: &[Event],
    ) -> Result<Vec<RenderResult>, PipelineError> {
        work_log::render_work_log(
            personal_events,
            &self.ctx.perf,
            &self.ctx.engine,
            &self.ctx.dirs,
            self.ctx.locale,
            &self.ctx.llm,
            &self.reader,
        )
        .await
    }

    async fn plan_documents(
        &self,
        source_id: &str,
        config: &SourceConfig,
        events: Vec<Event>,
        duplicates: Vec<Event>,
        _options: &IngestOptions,
    ) -> Result<IngestResult, PipelineError> {
        let focus = config.normalized_focus();

        let existing_concepts = if config.extract_concepts {
            self.load_existing_concept_refs().await?
        } else {
            vec![]
        };

        let strings = self.ctx.locale.strings();
        let summary_heading = strings.summary;
        let concepts_heading = strings.related_concepts;

        let mut document_pages: Vec<RenderResult> = Vec::new();
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

            let existing = self.reader.read_page(Path::new(&vault_path)).await?;

            let combined = format!("{}\n{}", event.title, event.body);

            let summary_req = lk_queue::SummarizeRequest {
                text: combined.clone(),
                max_sentences: 5,
                focus: focus.clone(),
                locale: self.ctx.locale.tag().to_string(),
                source_type: Some(config.source_type),
                target: lk_queue::TaskTarget {
                    vault_path: vault_path.clone(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: format!("## {summary_heading}"),
                },
            };
            let summary_decision = llm_cache::lookup(
                existing.as_ref(),
                summary_req.target.kind.llm_inputs_key(),
                summary_heading,
                summary_req.cache_hash(),
            );

            let summary = if summary_decision.enqueue() {
                match self.ctx.llm.summarize(summary_req).await {
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
                }
            } else {
                tracing::debug!(source = source_id, slug = %slug, "document summary cached; skipping enqueue");
                String::new()
            };

            let (concepts_decision, doc_concepts) = if config.extract_concepts {
                let concepts_req = lk_queue::ExtractConceptsRequest {
                    text: combined.clone(),
                    source_id: source_id.to_string(),
                    source_type: config.source_type,
                    date: event.date,
                    focus: focus.clone(),
                    target: lk_queue::TaskTarget {
                        vault_path: vault_path.clone(),
                        kind: lk_queue::TargetKind::DocumentConcepts,
                        anchor: format!("## {concepts_heading}"),
                    },
                    existing_concepts: existing_concepts.clone(),
                    categories: self.ctx.concept_categories.clone(),
                };
                let decision = llm_cache::lookup(
                    existing.as_ref(),
                    concepts_req.target.kind.llm_inputs_key(),
                    concepts_heading,
                    concepts_req.cache_hash(),
                );
                let extracted = if decision.enqueue() {
                    match self.ctx.llm.extract_concepts(concepts_req).await {
                        Ok(c) => filter_valid_concepts(c, &self.ctx.concept_categories),
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
                    tracing::debug!(source = source_id, slug = %slug, "document concepts cached; skipping enqueue");
                    vec![]
                };
                (Some(decision), extracted)
            } else {
                (None, vec![])
            };

            let concept_names: Vec<String> = doc_concepts.iter().map(|c| c.name.clone()).collect();

            let llm_inputs = render::DocumentLlmInputHashes {
                summary: &summary_decision.hash,
                concepts: concepts_decision.as_ref().map(|d| d.hash.as_str()),
            };

            let fresh = match render::render_document_page(
                &render::DocumentRenderContext {
                    slug: &slug,
                    event,
                    summary: &summary,
                    concepts: &concept_names,
                    extract_concepts: config.extract_concepts,
                    locale: self.ctx.locale,
                    llm_inputs,
                },
                &self.ctx.engine,
                &self.ctx.dirs,
            ) {
                Ok(output) => output,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        source = source_id,
                        slug = %slug,
                        "document render failed; skipping"
                    );
                    continue;
                }
            };

            let mut splices: Vec<(&str, &llm_cache::SectionDecision)> =
                vec![(summary_heading, &summary_decision)];
            if let Some(d) = concepts_decision.as_ref() {
                splices.push((concepts_heading, d));
            }
            let content = render::splice_preserved_sections(fresh.content, splices);

            document_pages.push(render::RenderResult {
                path: fresh.path,
                content,
            });

            // Merge concepts into run-level accumulator.
            {
                let mut drafts = self.concept_drafts.lock().await;
                for concept in &doc_concepts {
                    drafts
                        .merge(concept, event.date, &self.reader, &self.ctx.dirs)
                        .await?;
                }
            }
            all_concepts.extend(doc_concepts);
        }

        Ok(IngestResult {
            source_id: source_id.into(),
            events,
            duplicates,
            concepts: all_concepts,
            daily_pages: vec![],
            document_pages,
        })
    }

    async fn load_existing_concept_refs(
        &self,
    ) -> Result<Vec<lk_queue::ExistingConceptRef>, PipelineError> {
        let concept_dir = lk_core::vault_path::concepts_dir(&self.ctx.dirs);
        // Missing concepts directory is the legitimate "no concepts yet" state, not
        // an error. `list_markdown` already returns Ok(vec![]) in that case, so any
        // error returned here is a real I/O or permission failure worth surfacing.
        let files = self.reader.list_markdown(&concept_dir).await?;

        let mut refs = Vec::with_capacity(files.len());
        for file in &files {
            let Some(page) = self.reader.read_page(file).await? else {
                // Race: file appeared in listing but vanished before we could read it.
                // Skip rather than fail the whole ingest.
                continue;
            };
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
                refs.push(lk_queue::ExistingConceptRef {
                    slug: slug.to_string(),
                    name: name.to_string(),
                });
            }
        }

        let drafts = self.concept_drafts.lock().await;
        for (slug, name) in drafts.known_slugs_and_names() {
            if !refs.iter().any(|r| r.slug == slug) {
                refs.push(lk_queue::ExistingConceptRef { slug, name });
            }
        }

        Ok(refs)
    }
}

fn empty_result(source_id: &str, duplicates: Vec<Event>) -> IngestResult {
    IngestResult {
        source_id: source_id.into(),
        events: vec![],
        duplicates,
        concepts: vec![],
        daily_pages: vec![],
        document_pages: vec![],
    }
}

/// Drop concepts the model invented outside the configured slate. The pipeline
/// already accepts free-text categories from the skill side; here we strip any
/// `category` field that names an unknown id, so the rest of the system never
/// sees a non-existent category.
fn filter_valid_concepts(
    raw: Vec<ExtractedConcept>,
    categories: &[lk_queue::CategoryRef],
) -> Vec<ExtractedConcept> {
    let valid_cat_ids: Vec<&str> = categories.iter().map(|c| c.id.as_str()).collect();
    raw.into_iter()
        .filter(concept_draft::is_valid)
        .map(|mut ec| {
            if let Some(ref cat) = ec.category
                && !valid_cat_ids.contains(&cat.as_str())
            {
                // Observable parity with the queue-path `graph lint`: a category the
                // LLM invented (or a config id that was renamed) is dropped, but never
                // silently — otherwise ingest-path drift is invisible.
                tracing::warn!(
                    concept = %ec.name,
                    category = %cat,
                    "dropping concept category not in configured slate"
                );
                ec.category = None;
            }
            ec
        })
        .collect()
}
