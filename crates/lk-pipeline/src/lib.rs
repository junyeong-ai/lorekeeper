mod classify;
mod concept_draft;
mod context;
mod dedup;
mod event_log;
mod llm_cache;
mod normalize;
pub mod render;
mod synthesis;
mod work_log;

pub use context::PipelineContext;
pub use render::RenderResult;
pub use synthesis::Synthesizer;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use thiserror::Error;

use lk_core::concept::ExtractedConcept;
use lk_core::config::{SourceConfig, SourceType};
use lk_core::event::{Event, RawItem};
use lk_vault::{FsVault, VaultStore};

use concept_draft::ConceptDrafts;

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("event-log: {0}")]
    EventLog(String),
    #[error("render: {0}")]
    Render(String),
    #[error(transparent)]
    Vault(#[from] lk_vault::VaultError),
    #[error(transparent)]
    Queue(#[from] lk_queue::QueueError),
}

pub struct IngestResult {
    pub source_id: String,
    pub events: Vec<Event>,
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
    /// Anchor the fetch window and the kept-date filter to a specific day instead of
    /// today, for `lore ingest --date <past>` backfill / repair.
    pub target_date: Option<jiff::civil::Date>,
    /// Wall-clock today in the vault timezone — the realized/forecast boundary, independent
    /// of `target_date`. An event dated after `today` is a FORECAST (a calendar look-ahead
    /// event that hasn't happened): the vault never materializes a page for it (`Pipeline::plan`
    /// skips the date entirely), so a forecast becomes knowledge only once its date arrives.
    /// The work-log gate (`render_work_log`) and the synthesis read cap
    /// (`Synthesizer::read_date_range`) enforce the same boundary on the event- and
    /// read-driven paths as defense in depth.
    pub today: jiff::civil::Date,
    /// Preview only: plan normally (reading the event log to reflect what WOULD be
    /// written) but never mutate it, so a dry-run leaves the vault untouched.
    pub dry_run: bool,
}

pub struct Pipeline {
    ctx: Arc<PipelineContext>,
    reader: Arc<dyn VaultStore>,
    /// Durable per-date record of observed events. A daily page is a projection of it,
    /// so a streaming source (RSS) never loses an item that has scrolled out of its feed.
    event_log: event_log::EventLog,
    /// Concept accumulation spans the WHOLE run, not a single `plan` call: a concept
    /// page is a cross-source aggregate, so two sources mentioning the same slug merge
    /// into one page, rendered once via `render_concept_pages` after all sources are
    /// planned. Exclusive `&mut self` access keeps this run-level state coherent.
    concept_drafts: ConceptDrafts,
    /// Document slugs claimed so far THIS run, across every source. Document pages share one
    /// flat `<wiki>/documents/` namespace (unlike daily pages, which are keyed by
    /// source+date), so two manual sources with a same-titled file would otherwise both
    /// claim the bare slug — and since pages are written after all sources are planned,
    /// neither sees the other's page on disk. Run-level reservation makes the later one
    /// disambiguate by its own identity instead of silently overwriting the earlier page.
    document_slugs: std::collections::HashSet<String>,
}

impl Pipeline {
    pub fn new(vault_root: &Path, ctx: Arc<PipelineContext>) -> Self {
        Self {
            ctx,
            reader: Arc::new(FsVault::new(vault_root)),
            event_log: event_log::EventLog::new(vault_root),
            concept_drafts: ConceptDrafts::new(),
            document_slugs: std::collections::HashSet::new(),
        }
    }

    /// Build a source's daily (or document) pages and accumulate its concepts.
    ///
    /// A daily page is re-rendered IN FULL each run, so a re-run reproduces it
    /// byte-identically. A complete-refetch source renders from the fetch; a STREAMING
    /// source (RSS) renders from the union of the fetch and its per-date event log, so a
    /// scrolled-out item is never lost and a deleted page self-heals from the log. The log
    /// is the source of truth, never a suppression cache — it only ever adds to what a page
    /// can show.
    ///
    /// `plan` takes `&mut self`: one `Pipeline` drives one ingest run with exclusive
    /// access. The caller plans every source, then writes all pages — distinct phases
    /// over the whole source list — while concept drafts accumulate run-wide across the
    /// `plan` calls and render once in `render_concept_pages`.
    pub async fn plan(
        &mut self,
        source_id: &str,
        config: &SourceConfig,
        items: Vec<RawItem>,
        options: &IngestOptions,
    ) -> Result<IngestResult, PipelineError> {
        let mut events =
            normalize::normalize_events(source_id, config.source_type, items, &self.ctx.timezone);
        tracing::info!(source = source_id, raw = events.len(), "normalized");

        if let Some(target) = options.target_date {
            events.retain(|e| e.date == target);
            tracing::info!(target = %target, kept = events.len(), "filtered by --date");
        }

        // Collapse repeats of the SAME item within this single fetch (same event-id —
        // exact identity, e.g. paginated history overlap).
        let before = events.len();
        events = dedup::deduplicate(events);
        if events.len() != before {
            tracing::info!(source = source_id, kept = events.len(), "intra-batch dedup");
        }

        // Manual is a document source (one page per inbox file, archived after ingest), not
        // a daily aggregation — it doesn't project from the per-date event log.
        if config.source_type == SourceType::Manual {
            if events.is_empty() {
                return Ok(empty_result(source_id));
            }
            classify::assign_labels(&mut events, &config.labels);
            classify::assign_personal(
                &mut events,
                self.ctx
                    .personal
                    .as_ref()
                    .is_some_and(|p| p.is_tracked(source_id)),
            );
            classify::classify_by_keywords(&mut events, &config.classify);
            return self.plan_documents(source_id, config, events).await;
        }

        // A STREAMING source (RSS) fetches a rolling, capped window that can't reproduce a
        // past day, so its daily page is a projection of the per-date event log: union this
        // fetch with the stored events (fresh wins on id) so a scrolled-out item is never
        // lost and a deleted page self-heals from the log. `--date` is folded in so a repair
        // run renders from the log even when the feed returns nothing for that day. Only
        // dates present in the fetch (or `--date`) are re-rendered — a date that lives only
        // in the log keeps its already-correct frozen page rather than being churned.
        // Complete-refetch sources reproduce their whole window on demand, so they render
        // directly from the fetch and keep no log (nothing to accumulate).
        if config.source_type.descriptor().streaming {
            events =
                self.accumulate_with_log(source_id, events, options.target_date, options.dry_run)?;
        }

        if events.is_empty() {
            return Ok(empty_result(source_id));
        }

        // Canonical order BEFORE any bucketing, hashing, or rendering, so a complete-refetch
        // source whose API returns the same set in a different order still produces the same
        // page bytes and the same LLM-input hash (zero spurious re-enqueue). The streaming
        // path already arrives sorted from `merge_by_id`; sorting again here is idempotent and
        // single-sources the order for every source type through one comparator.
        events.sort_by(Event::canonical_cmp);

        // Classification is a render-time derivation of current config — re-applied to the
        // whole set so a config change reaches preserved events too. A streaming source's
        // log stores pre-classification, pre-refine events: the source of truth, untouched
        // by the LLM, so a re-render always feeds refine raw text.
        classify::assign_labels(&mut events, &config.labels);
        classify::assign_personal(
            &mut events,
            self.ctx
                .personal
                .as_ref()
                .is_some_and(|p| p.is_tracked(source_id)),
        );
        classify::classify_by_keywords(&mut events, &config.classify);

        let mut by_date: BTreeMap<jiff::civil::Date, Vec<usize>> = BTreeMap::new();
        for (i, event) in events.iter().enumerate() {
            by_date.entry(event.date).or_default().push(i);
        }

        let mut daily_pages: Vec<RenderResult> = Vec::new();
        // Concepts are staged per date and merged into the run-level accumulator only after
        // the whole source plan succeeds, so a mid-source render failure (`?`) contributes
        // nothing to the cross-source concept pages — the same commit-on-success contract the
        // queue gives buffered tasks.
        let mut staged_concepts: Vec<(ExtractedConcept, jiff::civil::Date)> = Vec::new();

        // Normalize once: blank focus = no filter, identical across every provider path.
        let focus = config.normalized_focus();

        let strings = self.ctx.locale.strings();
        let summary_heading = strings.summary;
        let events_heading = config.source_type.descriptor().item_kind.heading(strings);
        let concepts_heading = strings.related_concepts;

        for (date, day_indices) in &by_date {
            // A date after today is a FORECAST (a calendar look-ahead event that hasn't
            // happened): it isn't knowledge yet, so the vault never materializes a page for
            // it. The events already served their timezone-boundary purpose during fetch;
            // skipping the date here means no page, no concepts, and therefore nothing any
            // downstream consumer (work-log, synthesis, backlinks, orphans) can leak from a
            // not-yet-real day. The normal ingest writes the page once the date becomes today.
            if *date > options.today {
                continue;
            }
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
                    concepts_dir: render::concepts_dir_dest(&daily_path, &self.ctx.dirs),
                },
            };
            let summary_decision = llm_cache::lookup(
                existing.as_ref(),
                &summary_req.target.kind.completion_key(),
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
                    concepts_dir: render::concepts_dir_dest(&daily_path, &self.ctx.dirs),
                },
            };
            let refine_decision = llm_cache::lookup(
                existing.as_ref(),
                &refine_req.target.kind.completion_key(),
                events_heading,
                refine_req.cache_hash(),
            );

            let summary = if summary_decision.enqueue() {
                match self.ctx.llm.summarize(summary_req).await {
                    Ok(s) => s,
                    Err(e) if e.is_fatal() => return Err(PipelineError::Queue(e)),
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
                    return Err(PipelineError::Queue(e));
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
                        concepts_dir: render::concepts_dir_dest(&daily_path, &self.ctx.dirs),
                    },
                    categories: self.ctx.concept_categories.clone(),
                };
                let decision = llm_cache::lookup(
                    existing.as_ref(),
                    &concepts_req.target.kind.completion_key(),
                    concepts_heading,
                    concepts_req.cache_hash(),
                );
                let extracted = if decision.enqueue() {
                    match self.ctx.llm.extract_concepts(concepts_req).await {
                        Ok(c) => filter_valid_concepts(c, &self.ctx.concept_categories),
                        Err(e) if e.is_fatal() => return Err(PipelineError::Queue(e)),
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

            // Resolve before rendering so the link carries the slug the merge will use.
            // Resolution is a pure lookup; staging still happens after every page renders.
            let mut concept_names = Vec::with_capacity(day_concepts.len());
            for c in &day_concepts {
                concept_names.push(
                    self.concept_drafts
                        .resolve_identity(&c.name, self.reader.as_ref(), &self.ctx.dirs)
                        .await?,
                );
            }

            let labels: Vec<String> = {
                let mut set = std::collections::BTreeSet::new();
                for e in &day_events {
                    set.extend(e.labels.iter().cloned());
                }
                set.into_iter().collect()
            };

            // The `summary`/`refine_events`/`concepts` input keys are pre-stamped with the
            // current-input hash (the stale-task reference). A `*_done` completion marker is
            // valid ONLY for the input it was stamped against, so emit it only on a cache
            // hit — where `lookup` proved it equals the current hash. A miss drops it (the
            // skill re-stamps after processing), so a stale marker can never ride a
            // changed-input render forward and later false-hit on a revert to the earlier
            // input.
            let summary_done = summary_decision
                .cached
                .then_some(summary_decision.hash.as_str());
            let refine_events_done = refine_decision
                .cached
                .then_some(refine_decision.hash.as_str());
            let concepts_done = concepts_decision
                .as_ref()
                .filter(|d| d.cached)
                .map(|d| d.hash.as_str());

            let llm_inputs = render::DailyLlmInputHashes {
                summary: &summary_decision.hash,
                summary_done,
                refine_events: &refine_decision.hash,
                refine_events_done,
                concepts: concepts_decision.as_ref().map(|d| d.hash.as_str()),
                concepts_done,
            };

            let fresh = render::render_daily_page(
                &render::DailyRenderContext {
                    source_id,
                    source_type: config.source_type,
                    date: *date,
                    events: &day_events,
                    labels: &labels,
                    summary: &summary,
                    concepts: &concept_names,
                    extract_concepts: config.extract_concepts,
                    highlights: &config.highlights,
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
            // A cached body that can't be spliced (a custom template heading diverged
            // from the configured one) yields `None`; skip the write so the previous
            // on-disk page — and its LLM body — is left intact.
            match render::splice_preserved_sections(fresh.content, splices) {
                Some(content) => daily_pages.push(render::RenderResult {
                    path: fresh.path,
                    content,
                }),
                // A cached section heading is absent from the freshly rendered template
                // (almost always a custom template that renamed it). The previous on-disk
                // page is kept intact; warn so the divergence is observable instead of a
                // silently frozen page. Any task enqueued above targets that existing page
                // (a preserved body implies a prior page), so no orphan-to-missing results.
                None => tracing::warn!(
                    source = source_id,
                    date = %date,
                    "daily page write skipped: a cached section heading is missing from the \
                     rendered template; keeping the previous page"
                ),
            }

            staged_concepts.extend(day_concepts.into_iter().map(|c| (c, *date)));
        }

        // Commit staged concepts into the run-level accumulator (shared across all sources,
        // so a concept mentioned by several sources aggregates into one page) now that every
        // page rendered.
        let mut all_concepts = Vec::with_capacity(staged_concepts.len());
        for (concept, date) in staged_concepts {
            self.concept_drafts
                .merge(&concept, date, self.reader.as_ref(), &self.ctx.dirs)
                .await?;
            all_concepts.push(concept);
        }

        Ok(IngestResult {
            source_id: source_id.into(),
            events,
            concepts: all_concepts,
            daily_pages,
            document_pages: vec![],
        })
    }

    /// Union this run's freshly-fetched events with the durable per-date event log, per
    /// date, and return the full merged set the page will project. Persists the merged log
    /// (unless `dry_run`). `target_date` is folded in so a `--date` repair run reads the
    /// log for that day even when the fetch returned nothing for it.
    fn accumulate_with_log(
        &self,
        source_id: &str,
        fresh: Vec<Event>,
        target_date: Option<jiff::civil::Date>,
        dry_run: bool,
    ) -> Result<Vec<Event>, PipelineError> {
        let mut fresh_by_date: BTreeMap<jiff::civil::Date, Vec<Event>> = BTreeMap::new();
        for event in fresh {
            fresh_by_date.entry(event.date).or_default().push(event);
        }
        let mut dates: std::collections::BTreeSet<jiff::civil::Date> =
            fresh_by_date.keys().copied().collect();
        dates.extend(target_date);

        let mut merged_all = Vec::new();
        for date in dates {
            let stored = self.event_log.read(source_id, date)?;
            let fresh_for_date = fresh_by_date.remove(&date).unwrap_or_default();
            let merged = event_log::merge_by_id(stored, fresh_for_date);
            if !dry_run && !merged.is_empty() {
                self.event_log.write(source_id, date, &merged)?;
            }
            merged_all.extend(merged);
        }
        Ok(merged_all)
    }

    /// Materialize the concepts a drained queue task produced.
    ///
    /// Extraction is the LLM's half of the work; deciding where each concept lands is this
    /// crate's, and it is the same decision `plan` makes when an LLM answers synchronously:
    /// invalid categories dropped, slugs normalized, the origin page's related-concepts
    /// section rendered from `concept_links`, and each concept page created-or-merged with
    /// its `## Synthesis`, aliases and citation count preserved. Routing the queue's results
    /// back through it is what keeps that logic in one place rather than restated as prose
    /// the drain has to follow.
    ///
    /// Returns the rewritten origin page. Concept pages accumulate and are emitted together
    /// by [`Self::render_concept_pages`], so a concept named by several pages produces one
    /// page rather than a last-writer-wins race.
    pub async fn apply_concept_result(
        &mut self,
        result: &lk_queue::TaskResult,
        page: &str,
    ) -> Result<String, PipelineError> {
        let reported: Vec<&lk_queue::ReportedConcept> = result
            .concepts
            .iter()
            .filter(|r| {
                !filter_valid_concepts(vec![r.concept.clone()], &self.ctx.concept_categories)
                    .is_empty()
            })
            .collect();
        let mut identities = Vec::with_capacity(reported.len());
        for reported in reported {
            let concept =
                filter_valid_concepts(vec![reported.concept.clone()], &self.ctx.concept_categories)
                    .remove(0);
            identities.push(
                self.concept_drafts
                    .merge_with_synthesis(
                        &concept,
                        reported.synthesis.as_deref(),
                        result.date,
                        self.reader.as_ref(),
                        &self.ctx.dirs,
                    )
                    .await?,
            );
        }

        // The heading comes from the task's own anchor, which is the value the queue
        // contract carries for exactly this purpose. Deriving it from the CURRENT locale
        // instead would break the moment `vault.locale` changed between the drain and the
        // apply: the page still carries the old heading, the task still classifies current,
        // and every later run would abort on a section it could not find.
        let heading = result.target.anchor.strip_prefix("## ").ok_or_else(|| {
            PipelineError::Render(format!(
                "{}: task anchor `{}` is not an H2 heading",
                result.target.vault_path, result.target.anchor
            ))
        })?;
        // A missing section is an error, not a quiet pass: `replace_section` returns the page
        // untouched when the heading is absent, so the links would vanish while the result
        // was consumed and reported as applied.
        if lk_vault::section_body(page, heading).is_none() {
            return Err(PipelineError::Render(format!(
                "{}: no `{}` section to receive concept links",
                result.target.vault_path, result.target.anchor
            )));
        }

        let links = render::concept_links(&identities, &result.target.vault_path, &self.ctx.dirs);
        let filled = lk_vault::replace_section(page, heading, &links.join("\n"));
        // Stamp the completion marker in the SAME edit that fills the section. `llm_cache`
        // decides a section's fate purely on this marker, never on whether the body looks
        // filled — so a section written without it is erased by the next render and its task
        // re-enqueued, forever.
        Ok(lk_vault::set_llm_input(
            &filled,
            &result.target.kind.completion_key(),
            &result.cache_hash,
        ))
    }

    /// Render the concept pages accumulated across every `plan` call in this run.
    /// Call once after all sources are planned.
    pub async fn render_concept_pages(&self) -> Result<Vec<RenderResult>, PipelineError> {
        self.concept_drafts
            .render_pages(&self.ctx.engine, &self.ctx.dirs, self.ctx.locale)
    }

    pub async fn render_work_log(
        &self,
        personal_events: &[Event],
        today: jiff::civil::Date,
    ) -> Result<Vec<RenderResult>, PipelineError> {
        work_log::render_work_log(personal_events, today, &self.ctx, self.reader.as_ref()).await
    }

    async fn plan_documents(
        &mut self,
        source_id: &str,
        config: &SourceConfig,
        events: Vec<Event>,
    ) -> Result<IngestResult, PipelineError> {
        // Canonical order so slug-collision disambiguation (first occurrence keeps the clean
        // slug) is stable across runs — the same comparator the daily path and event log use.
        let mut events = events;
        events.sort_by(Event::canonical_cmp);

        let focus = config.normalized_focus();

        let strings = self.ctx.locale.strings();
        let summary_heading = strings.summary;
        let concepts_heading = strings.related_concepts;

        let mut document_pages: Vec<RenderResult> = Vec::new();
        // Run-level state (slug claims, concept drafts) is staged locally and committed to
        // `self` only after the whole source plan succeeds, so a mid-source render failure
        // (which returns `Err` and is rolled back by the CLI) never poisons the shared slug
        // namespace nor leaks half a source's concepts — the same commit-on-success contract
        // the queue's begin_source/rollback_source gives buffered tasks.
        let mut claimed_slugs: Vec<String> = Vec::new();
        let mut staged_concepts: Vec<(lk_core::concept::ExtractedConcept, jiff::civil::Date)> =
            Vec::new();
        // Two documents can share a title — and therefore a base slug — whether in one batch
        // or across two sources in the same run. Disambiguate a collision by the document's
        // OWN stable identity (the trailing hash of its `EventId`), so a same-titled later
        // document gets its own page instead of silently overwriting the first. First
        // occurrence keeps the clean slug; the suffix is content-derived (stable across runs),
        // never a positional counter (which would shift if order or membership changed).
        // `self.document_slugs` is run-level so a second source sees the first's claims even
        // though pages are not written to disk until every source is planned.

        for event in &events {
            // Derive slug from title; fall back to source_file metadata.
            let base_slug = match lk_core::concept::slugify(&event.title) {
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
            // This document's stable identity — the manual file path the template records as
            // `source_file` (`source_url` is also accepted, as forward-compat for any
            // URL-sourced document). Used to decide whether an existing page at a candidate
            // slug is THIS document (reuse it idempotently) or a DIFFERENT one (disambiguate)
            // — so a later same-titled document never overwrites an earlier one's page,
            // ACROSS runs too: the in-memory slug claims only cover this run, so the on-disk
            // owner check is what catches a prior run's page.
            let identity = event
                .metadata
                .get("source_file")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| event.url.clone());

            // Resolve a collision-free slug. The bare title slug is tried first (readable);
            // on any collision — a slug already claimed in this batch, OR an on-disk page
            // owned by a DIFFERENT document — the document's own `EventId` hash is appended,
            // lengthened, and finally given a numeric tail. The candidate ALWAYS keeps
            // changing, so termination never depends on hash uniqueness, and a candidate is
            // claimed ONLY when it is free or already this document's own page — a different
            // document's page is never overwritten.
            let hash = event.id.content_hash();
            let mut suffix_len: Option<usize> = None;
            let (slug, existing) = loop {
                let candidate = match suffix_len {
                    None => base_slug.clone(),
                    Some(n) if n <= hash.len() => format!("{base_slug}-{}", &hash[..n]),
                    // Past the full hash (astronomically unlikely): a numeric tail guarantees
                    // the candidate never saturates, so the loop always reaches a free slug.
                    Some(n) => format!("{base_slug}-{hash}-{}", n - hash.len()),
                };
                if !self.document_slugs.contains(&candidate) && !claimed_slugs.contains(&candidate)
                {
                    let path = lk_core::vault_path::VaultPath::document(&self.ctx.dirs, &candidate)
                        .to_string();
                    let existing = self.reader.read_page(Path::new(&path)).await?;
                    let owner = existing.as_ref().and_then(|pg| {
                        pg.frontmatter
                            .get("source_file")
                            .or_else(|| pg.frontmatter.get("source_url"))
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    });
                    // Claim only a free slug OR a page provably THIS document (owner present
                    // and equal). A page with NO owner can't be proven to be ours, so it is
                    // never taken over — we disambiguate around it rather than overwrite an
                    // unidentifiable page.
                    if existing.is_none() || (owner.is_some() && owner == identity) {
                        break (candidate, existing);
                    }
                }
                suffix_len = Some(suffix_len.map_or(6, |n| n + 2));
            };
            claimed_slugs.push(slug.clone());

            let vault_path =
                lk_core::vault_path::VaultPath::document(&self.ctx.dirs, &slug).to_string();

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
                    concepts_dir: render::concepts_dir_dest(&vault_path, &self.ctx.dirs),
                },
            };
            let summary_decision = llm_cache::lookup(
                existing.as_ref(),
                &summary_req.target.kind.completion_key(),
                summary_heading,
                summary_req.cache_hash(),
            );

            let summary = if summary_decision.enqueue() {
                match self.ctx.llm.summarize(summary_req).await {
                    Ok(s) => s,
                    Err(e) if e.is_fatal() => return Err(PipelineError::Queue(e)),
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
                        concepts_dir: render::concepts_dir_dest(&vault_path, &self.ctx.dirs),
                    },
                    categories: self.ctx.concept_categories.clone(),
                };
                let decision = llm_cache::lookup(
                    existing.as_ref(),
                    &concepts_req.target.kind.completion_key(),
                    concepts_heading,
                    concepts_req.cache_hash(),
                );
                let extracted = if decision.enqueue() {
                    match self.ctx.llm.extract_concepts(concepts_req).await {
                        Ok(c) => filter_valid_concepts(c, &self.ctx.concept_categories),
                        Err(e) if e.is_fatal() => return Err(PipelineError::Queue(e)),
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

            // Resolve before rendering so the link carries the slug the merge will use.
            let mut concept_names = Vec::with_capacity(doc_concepts.len());
            for c in &doc_concepts {
                concept_names.push(
                    self.concept_drafts
                        .resolve_identity(&c.name, self.reader.as_ref(), &self.ctx.dirs)
                        .await?,
                );
            }

            // Completion markers are valid only on a cache hit — see the daily path.
            let summary_done = summary_decision
                .cached
                .then_some(summary_decision.hash.as_str());
            let concepts_done = concepts_decision
                .as_ref()
                .filter(|d| d.cached)
                .map(|d| d.hash.as_str());
            let llm_inputs = render::DocumentLlmInputHashes {
                summary: &summary_decision.hash,
                summary_done,
                concepts: concepts_decision.as_ref().map(|d| d.hash.as_str()),
                concepts_done,
            };

            let fresh = render::render_document_page(
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
            )?;

            let mut splices: Vec<(&str, &llm_cache::SectionDecision)> =
                vec![(summary_heading, &summary_decision)];
            if let Some(d) = concepts_decision.as_ref() {
                splices.push((concepts_heading, d));
            }
            match render::splice_preserved_sections(fresh.content, splices) {
                Some(content) => document_pages.push(render::RenderResult {
                    path: fresh.path,
                    content,
                }),
                None => tracing::warn!(
                    source = source_id,
                    slug = %slug,
                    "document page write skipped: a cached section heading is missing from the \
                     rendered template; keeping the previous page"
                ),
            }

            staged_concepts.extend(doc_concepts.into_iter().map(|c| (c, event.date)));
        }

        // Commit staged run-level state now that every page rendered. Concept merges
        // (fallible — they read existing pages) run before the infallible slug commit, so a
        // merge error leaves neither half-committed.
        let mut all_concepts = Vec::with_capacity(staged_concepts.len());
        for (concept, date) in staged_concepts {
            self.concept_drafts
                .merge(&concept, date, self.reader.as_ref(), &self.ctx.dirs)
                .await?;
            all_concepts.push(concept);
        }
        self.document_slugs.extend(claimed_slugs);

        Ok(IngestResult {
            source_id: source_id.into(),
            events,
            concepts: all_concepts,
            daily_pages: vec![],
            document_pages,
        })
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

/// Drop concepts the model invented outside the configured slate. The pipeline
/// already accepts free-text categories from the skill side; here we strip any
/// `category` field that names an unknown id, so the rest of the system never
/// sees a non-existent category.
fn filter_valid_concepts(
    raw: Vec<ExtractedConcept>,
    categories: &[lk_queue::CategoryReference],
) -> Vec<ExtractedConcept> {
    let valid_cat_ids: Vec<&str> = categories.iter().map(|c| c.id.as_str()).collect();
    raw.into_iter()
        .filter(concept_draft::has_valid_slug)
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
