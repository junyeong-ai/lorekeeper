use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lk_core::config::Config;
use lk_core::frontmatter::field;
use lk_core::vault_path::{VaultPath, work_log_dir};
use lk_queue::TargetKind;
use lk_vault::{FsVault, VaultPage, VaultStore, section_body};

use crate::PipelineError;
use crate::context::PipelineContext;
use crate::llm_cache::{self, SectionDecision};
use crate::render::{RenderResult, llm_inputs_map, splice_preserved_sections};

pub struct Synthesizer {
    ctx: Arc<PipelineContext>,
    reader: Arc<dyn VaultStore>,
    sources: Vec<String>,
    /// Realized/forecast boundary (wall-clock today, vault tz). Synthesis never reads a
    /// page dated after today, so a review or theme run mid-period reflects only the days
    /// that have actually happened — a forecast schedule-preview page can never inflate a
    /// performance review or a weekly digest.
    today: jiff::civil::Date,
}

/// Outcome of resolving a synthesis page's LLM-owned section.
struct SynthesisSection {
    /// Cache decision — carries the hash to stamp into frontmatter and the
    /// preserved body to splice on a cache hit.
    decision: SectionDecision,
    /// The narrative text to feed the template. Empty on a cache hit (the body is
    /// spliced in afterward, not re-rendered) and in queue mode (the skill fills it
    /// later). `None` only when a transient LLM failure means the page should be
    /// skipped entirely.
    narrative: Option<String>,
}

impl SynthesisSection {
    fn cached(decision: SectionDecision) -> Self {
        Self {
            decision,
            narrative: Some(String::new()),
        }
    }

    fn fresh(decision: SectionDecision, narrative: String) -> Self {
        Self {
            decision,
            narrative: Some(narrative),
        }
    }

    fn failed() -> Self {
        Self {
            decision: SectionDecision {
                hash: String::new(),
                cached: false,
                preserved_body: None,
                discarding: None,
            },
            narrative: None,
        }
    }
}

impl Synthesizer {
    pub fn new(
        vault_root: &Path,
        ctx: Arc<PipelineContext>,
        config: &Config,
        today: jiff::civil::Date,
    ) -> Self {
        let reader: Arc<dyn VaultStore> = Arc::new(FsVault::new(vault_root));
        // Cross-source weekly themes are opt-in: only the sources explicitly listed in
        // `synthesis.weekly.include_sources` are rolled up. Knowledge feeds (news, RSS)
        // deliberately stay out — their value is the continuously-accumulated concept
        // graph, so a weekly digest of them would be redundant. An empty list yields no
        // themes page; the weekly review narrative (work-log) is produced regardless.
        let sources = config.synthesis.weekly.include_sources.clone();
        Self {
            ctx,
            reader,
            sources,
            today,
        }
    }

    /// The review narratives (weekly/monthly/quarterly/annual) are the personal module;
    /// an absent `config.personal` → no review pages, even if work-log entries exist. The
    /// cross-source weekly themes page is independent and not gated here.
    fn is_personal_enabled(&self) -> bool {
        self.ctx.personal.is_some()
    }

    /// A synthesis page is a materialized view exactly like a daily page: its one
    /// LLM-owned section (narrative or themes) is preserved across re-renders and
    /// re-computed only when the source input changes. This is the shared lookup:
    /// build the request, hash its `cache_identity()`, and compare against the existing
    /// page's `*_done` completion marker (`completion_key`). On a hit the LLM call is
    /// skipped and the existing body is spliced back; on a miss the task runs (or
    /// enqueues, in queue mode).
    async fn summarize_section(
        &self,
        text: String,
        max_sentences: usize,
        path: &VaultPath,
        kind: TargetKind,
        heading: &str,
        what: &str,
    ) -> Result<SynthesisSection, PipelineError> {
        let req = lk_queue::SummarizeRequest {
            text,
            max_sentences,
            focus: None,
            locale: self.ctx.locale.tag().to_string(),
            // Period synthesis rolls up pre-summarized pages across sources/time;
            // no single source_type applies.
            source_type: None,
            target: lk_queue::TaskTarget {
                vault_path: path.to_string(),
                kind,
                anchor: format!("## {heading}"),
            },
        };
        let decision = self.lookup(path, kind, heading, req.cache_hash()).await?;
        if !decision.enqueue() {
            return Ok(SynthesisSection::cached(decision));
        }
        match self.ctx.llm.summarize(req).await {
            Ok(narrative) => Ok(SynthesisSection::fresh(decision, narrative)),
            Err(e) if e.is_fatal() => Err(PipelineError::Queue(e)),
            Err(e) => {
                tracing::warn!(error = %e, what, "synthesis summarize failed; skipping page");
                Ok(SynthesisSection::failed())
            }
        }
    }

    /// Cache lookup for one synthesis page's LLM-owned section. Completion is uniformly
    /// marker-signalled (`completion_key`): a narrative — or weekly themes — can be empty
    /// (an empty period, no cross-source theme), so an empty section must never be
    /// mistaken for "not done" and re-enqueued forever.
    async fn lookup(
        &self,
        path: &VaultPath,
        kind: TargetKind,
        heading: &str,
        hash: String,
    ) -> Result<SectionDecision, PipelineError> {
        let existing = self.reader.read_page(path.as_ref()).await?;
        Ok(llm_cache::lookup(
            existing.as_ref(),
            &kind.completion_key(),
            heading,
            hash,
        ))
    }

    /// Render a synthesis/personal template, injecting localized labels as `i18n.*`
    /// and the `llm_inputs.<key>` cache hash, then splice the preserved LLM-owned
    /// section body back when the lookup was a cache hit. Every period template is
    /// embedded, so this always resolves; a user template dir merely overrides.
    fn render_section(
        &self,
        template: &str,
        kind: TargetKind,
        heading: &str,
        decision: &SectionDecision,
        context: serde_json::Value,
    ) -> Result<Option<String>, PipelineError> {
        let mut ctx = context;
        if let Some(map) = ctx.as_object_mut() {
            let i18n = serde_json::to_value(self.ctx.locale.strings())
                .map_err(|e| PipelineError::Render(e.to_string()))?;
            map.insert("i18n".to_string(), i18n);
            let mut llm_inputs = llm_inputs_map(&[(kind, Some(&decision.hash))]);
            // Re-emit the completion marker so it round-trips. On a cache hit `lookup`
            // proved the on-disk marker equals this hash, so re-stamping `decision.hash`
            // preserves it; on a miss there is no valid marker yet and the skill writes
            // it after processing.
            if decision.cached {
                llm_inputs.insert(kind.completion_key(), decision.hash.clone().into());
            }
            map.insert(
                field::LLM_INPUTS.to_string(),
                serde_json::Value::Object(llm_inputs),
            );
        }
        let rendered = self
            .ctx
            .engine
            .render(template, &ctx)
            .map_err(|e| PipelineError::Render(e.to_string()))?;
        // `None` (a cached body whose heading drifted out of the template) leaves the
        // on-disk page untouched — the caller skips the write and a later run re-fills.
        Ok(splice_preserved_sections(
            rendered,
            std::iter::once((heading, decision)),
        ))
    }

    pub async fn try_weekly_synthesis(
        &self,
        date: jiff::civil::Date,
    ) -> Result<Option<RenderResult>, PipelineError> {
        let (year, week) = iso_year_week(date);
        let (start, end) = iso_week_range(year, week)?;

        let mut combined = String::new();
        let mut covered_sources: Vec<String> = Vec::new();

        for source_id in &self.sources {
            let dir = PathBuf::from(&self.ctx.dirs.daily).join(source_id);
            let pages = self.read_date_range(&dir, start, end).await?;
            if pages.is_empty() {
                continue;
            }

            let combined_source = pages
                .iter()
                .map(|p| p.body.as_str())
                .collect::<Vec<_>>()
                .join("\n\n---\n\n");

            combined.push_str(&format!("=== {source_id} ===\n{combined_source}\n\n"));
            covered_sources.push(source_id.clone());
        }

        if combined.is_empty() {
            return Ok(None);
        }

        let path = VaultPath::weekly_synthesis(&self.ctx.dirs, year, week);
        let kind = TargetKind::WeeklySynthesisThemes;
        let heading = self.ctx.locale.strings().key_themes_this_week;

        let req = lk_queue::ThemeRequest {
            text: combined,
            max_themes: 5,
            locale: self.ctx.locale.tag().to_string(),
            target: lk_queue::TaskTarget {
                vault_path: path.to_string(),
                kind,
                anchor: format!("## {heading}"),
            },
        };
        let decision = self.lookup(&path, kind, heading, req.cache_hash()).await?;
        let themes = if decision.enqueue() {
            match self.ctx.llm.identify_themes(req).await {
                Ok(t) => t,
                Err(e) if e.is_fatal() => return Err(PipelineError::Queue(e)),
                Err(e) => {
                    tracing::warn!(error = %e, "weekly theme extraction failed; skipping page");
                    return Ok(None);
                }
            }
        } else {
            Vec::new()
        };

        // Weekly synthesis observes concepts already materialized during daily ingest;
        // it does not extract new ones here (which would not be persisted to wiki/concepts).
        let context = serde_json::json!({
            "year": year,
            "week": week,
            "date": end.to_string(),
            "labels": ["synthesis"],
            "sources": covered_sources,
            "themes": themes,
        });

        let content = self.render_section(
            "weekly-synthesis.md.jinja",
            kind,
            heading,
            &decision,
            context,
        )?;

        Ok(content.map(|content| RenderResult { path, content }))
    }

    pub async fn try_weekly_review(
        &self,
        date: jiff::civil::Date,
    ) -> Result<Option<RenderResult>, PipelineError> {
        if !self.is_personal_enabled() {
            return Ok(None);
        }
        let (year, week) = iso_year_week(date);
        let (start, end) = iso_week_range(year, week)?;

        let dir = work_log_dir(&self.ctx.dirs);
        let pages = self.read_date_range(&dir, start, end).await?;

        if pages.is_empty() {
            return Ok(None);
        }

        let combined = pages
            .iter()
            .map(|p| p.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let path = VaultPath::weekly_review(&self.ctx.dirs, year, week);
        let kind = TargetKind::WeeklyReviewNarrative;
        let heading = self.ctx.locale.strings().key_summary;
        let section = self
            .summarize_section(
                format!(
                    "Summarize this week's personal work into key accomplishments by category:\n\n{combined}"
                ),
                10,
                &path,
                kind,
                heading,
                "weekly review",
            )
            .await?;
        let Some(narrative) = section.narrative else {
            return Ok(None);
        };
        let category_stats = self.aggregate_category_stats(start, end).await?;
        let context = serde_json::json!({
            "title": self.ctx.locale.weekly_title(year, week),
            "year": year,
            "week": week,
            "start_date": start.to_string(),
            "end_date": end.to_string(),
            "narrative": narrative,
            "days_logged": pages.len(),
            "category_stats": category_stats,
        });

        let content = self.render_section(
            "weekly-review.md.jinja",
            kind,
            heading,
            &section.decision,
            context,
        )?;

        Ok(content.map(|content| RenderResult { path, content }))
    }

    pub async fn try_monthly_review(
        &self,
        year: i16,
        month: u8,
    ) -> Result<Option<RenderResult>, PipelineError> {
        if !self.is_personal_enabled() {
            return Ok(None);
        }
        let (start, end) = month_range(year, month)?;
        let dir = work_log_dir(&self.ctx.dirs);
        let pages = self.read_date_range(&dir, start, end).await?;

        if pages.is_empty() {
            return Ok(None);
        }

        let combined = pages
            .iter()
            .map(|p| p.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let path = VaultPath::monthly_review(&self.ctx.dirs, year, month);
        let kind = TargetKind::MonthlyReviewNarrative;
        let heading = self.ctx.locale.strings().key_summary;
        let section = self
            .summarize_section(
                format!(
                    "Generate a monthly performance review with key achievements and category distribution:\n\n{combined}"
                ),
                15,
                &path,
                kind,
                heading,
                "monthly",
            )
            .await?;
        let Some(narrative) = section.narrative else {
            return Ok(None);
        };
        let category_stats = self.aggregate_category_stats(start, end).await?;
        let context = serde_json::json!({
            "year": year,
            "month": month,
            "title": self.ctx.locale.monthly_title(year, month as i8),
            "start_date": start.to_string(),
            "end_date": end.to_string(),
            "narrative": narrative,
            "days_logged": pages.len(),
            "category_stats": category_stats,
        });

        let content = self.render_section(
            "monthly-review.md.jinja",
            kind,
            heading,
            &section.decision,
            context,
        )?;

        Ok(content.map(|content| RenderResult { path, content }))
    }

    pub async fn try_quarterly_review(
        &self,
        year: i16,
        quarter: u8,
    ) -> Result<Option<RenderResult>, PipelineError> {
        if !self.is_personal_enabled() {
            return Ok(None);
        }
        let (start, end) = quarter_range(year, quarter)?;
        let months: Vec<u8> = ((quarter - 1) * 3 + 1..=quarter * 3).collect();

        // Each month's narrative section only (via `child_narrative`) — never its whole
        // body, which would drag the child's own distribution table and metadata headings
        // into the parent, producing duplicate tables and nested headings.
        let mut monthly_summaries: Vec<serde_json::Value> = Vec::new();
        let mut combined = String::new();

        for m in &months {
            let Some(narrative) = self.month_child_narrative(year, *m).await? else {
                continue;
            };
            let label = format!("{year}-{m:02}");
            combined.push_str(&format!("=== {label} ===\n"));
            combined.push_str(&narrative);
            combined.push_str("\n\n");
            monthly_summaries.push(serde_json::json!({
                "month": label,
                "summary": narrative,
            }));
        }

        if combined.is_empty() {
            return Ok(None);
        }

        let path = VaultPath::quarterly_review(&self.ctx.dirs, year, quarter);
        let kind = TargetKind::QuarterlyReviewNarrative;
        let heading = self.ctx.locale.strings().key_summary;
        let section = self
            .summarize_section(
                format!(
                    "Generate a quarterly performance review: top 5 achievements, category breakdown, growth areas, next direction:\n\n{combined}"
                ),
                20,
                &path,
                kind,
                heading,
                "quarterly",
            )
            .await?;
        let Some(narrative) = section.narrative else {
            return Ok(None);
        };

        let category_stats = self.aggregate_category_stats(start, end).await?;

        let context = serde_json::json!({
            "year": year,
            "quarter": quarter,
            "title": self.ctx.locale.quarterly_title(year, quarter),
            "date": end.to_string(),
            "start_date": start.to_string(),
            "end_date": end.to_string(),
            "category_stats": category_stats,
            "monthly_summaries": monthly_summaries,
            "narrative": narrative,
        });

        let content = self.render_section(
            "quarterly-review.md.jinja",
            kind,
            heading,
            &section.decision,
            context,
        )?;

        Ok(content.map(|content| RenderResult { path, content }))
    }

    pub async fn try_annual_review(
        &self,
        year: i16,
    ) -> Result<Option<RenderResult>, PipelineError> {
        if !self.is_personal_enabled() {
            return Ok(None);
        }
        let mut period_summaries: Vec<serde_json::Value> = Vec::new();
        let mut combined = String::new();
        let strings = self.ctx.locale.strings();

        for q in 1..=4u8 {
            let Some(narrative) = self.quarter_child_narrative(year, q).await? else {
                continue;
            };
            combined.push_str(&format!("=== {year}-Q{q} ===\n"));
            combined.push_str(&narrative);
            combined.push_str("\n\n");
            period_summaries.push(serde_json::json!({
                "label": format!("Q{q}"),
                "summary": narrative,
            }));
        }

        if combined.is_empty() {
            return Ok(None);
        }

        let breakdown_heading = strings.quarterly_breakdown;

        let path = VaultPath::annual_review(&self.ctx.dirs, year);
        let kind = TargetKind::AnnualReviewNarrative;
        let heading = strings.overall_summary;
        let section = self
            .summarize_section(
                format!(
                    "Generate a comprehensive annual performance review based on quarterly summaries:\n\n{combined}"
                ),
                25,
                &path,
                kind,
                heading,
                "annual",
            )
            .await?;
        let Some(narrative) = section.narrative else {
            return Ok(None);
        };

        let category_stats = self
            .aggregate_category_stats(
                jiff::civil::date(year, 1, 1),
                jiff::civil::date(year, 12, 31),
            )
            .await?;
        let context = serde_json::json!({
            "year": year,
            "title": self.ctx.locale.annual_title(year),
            "narrative": narrative,
            "breakdown_heading": breakdown_heading,
            "period_summaries": period_summaries,
            "category_stats": category_stats,
        });

        let content = self.render_section(
            "annual-review.md.jinja",
            kind,
            heading,
            &section.decision,
            context,
        )?;

        Ok(content.map(|content| RenderResult { path, content }))
    }

    /// Narrative standing in for one month of a quarterly/annual rollup: the monthly
    /// review if it exists, otherwise the month's weekly reviews concatenated. Each
    /// level reads only pre-summarized child pages — never raw work-log. An ISO week
    /// straddling a month/quarter boundary is read by both adjacent fallback periods;
    /// for a narrative rollup that mild redundancy is harmless (the parent summary folds
    /// it), and dropping it instead would lose real activity from the later period. The
    /// numeric category table is computed separately from raw work-log over the exact
    /// date range, so counts are never double-tallied regardless.
    async fn month_child_narrative(
        &self,
        year: i16,
        month: u8,
    ) -> Result<Option<String>, PipelineError> {
        let monthly_dir = PathBuf::from(&self.ctx.dirs.personal).join(&self.ctx.dirs.monthly);
        let file = monthly_dir.join(format!("{year}-{month:02}.md"));
        if let Some(page) = self.reader.read_page(&file).await? {
            // An existing monthly whose narrative is still queue-pending (empty) yields no
            // rollup contribution — drop it rather than fall back to its raw body.
            let narrative = child_narrative(&page);
            return Ok((!narrative.is_empty()).then(|| narrative.to_string()));
        }

        let (start, end) = month_range(year, month)?;
        let weekly_dir = PathBuf::from(&self.ctx.dirs.personal).join(&self.ctx.dirs.weekly);
        let mut parts = Vec::new();
        for (wy, ww) in iso_weeks_in_range(start, end)? {
            let file = weekly_dir.join(format!("{wy}-W{ww:02}.md"));
            if let Some(page) = self.reader.read_page(&file).await? {
                let narrative = child_narrative(&page);
                if !narrative.is_empty() {
                    parts.push(narrative.to_string());
                }
            }
        }
        Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
    }

    /// Narrative standing in for one quarter of the annual rollup: the quarterly
    /// review if it exists, otherwise its three months (each falling back to weeks).
    async fn quarter_child_narrative(
        &self,
        year: i16,
        quarter: u8,
    ) -> Result<Option<String>, PipelineError> {
        let quarterly_dir = PathBuf::from(&self.ctx.dirs.personal).join(&self.ctx.dirs.quarterly);
        let file = quarterly_dir.join(format!("{year}-Q{quarter}.md"));
        if let Some(page) = self.reader.read_page(&file).await? {
            // Queue-pending quarterly (empty narrative) → no contribution, not its raw body.
            let narrative = child_narrative(&page);
            return Ok((!narrative.is_empty()).then(|| narrative.to_string()));
        }

        let mut parts = Vec::new();
        for month in (quarter - 1) * 3 + 1..=quarter * 3 {
            if let Some(narrative) = self.month_child_narrative(year, month).await? {
                parts.push(narrative);
            }
        }
        Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
    }

    async fn aggregate_category_stats(
        &self,
        start: jiff::civil::Date,
        end: jiff::civil::Date,
    ) -> Result<Vec<serde_json::Value>, PipelineError> {
        let dir = work_log_dir(&self.ctx.dirs);
        let pages = self.read_date_range(&dir, start, end).await?;

        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut total = 0usize;

        for page in &pages {
            if let Some(cats) = page
                .frontmatter
                .get("categories")
                .and_then(|v| v.as_array())
            {
                for cat in cats {
                    if let Some(s) = cat.as_str() {
                        *counts.entry(s.to_string()).or_insert(0) += 1;
                        total += 1;
                    }
                }
            }
        }

        // Reached only from the review methods, which are gated on the personal module;
        // bind it defensively so a stray call without it yields no table rather than panics.
        let Some(personal) = self.ctx.personal.as_ref() else {
            return Ok(vec![]);
        };

        if total == 0 {
            // No work-log data → emit empty stats so the template omits the table
            // rather than rendering all-zero rows.
            return Ok(vec![]);
        }

        let stats = personal
            .performance_categories
            .iter()
            .filter_map(|cat| {
                let count = counts.get(cat).copied().unwrap_or(0);
                if count == 0 {
                    return None;
                }
                let percent = lk_core::math::round_percent(count, total);
                Some(serde_json::json!({
                    "name": cat,
                    "count": count,
                    "percent": percent,
                }))
            })
            .collect();

        Ok(stats)
    }

    async fn read_date_range(
        &self,
        dir: &Path,
        start: jiff::civil::Date,
        end: jiff::civil::Date,
    ) -> Result<Vec<VaultPage>, PipelineError> {
        // Synthesis reflects realized time only: never read a page dated after today, so a
        // review or theme run mid-period excludes forecast days that haven't happened. This
        // is the single read boundary for every synthesis path (themes and all reviews).
        let end = end.min(self.today);
        let mut pages = Vec::new();
        let mut date = start;
        while date <= end {
            let file = dir.join(format!("{date}.md"));
            if let Some(page) = self.reader.read_page(&file).await? {
                pages.push(page);
            }
            date = date
                .checked_add(jiff::Span::new().days(1))
                .map_err(|e| PipelineError::Render(format!("date arithmetic: {e}")))?;
        }
        Ok(pages)
    }
}

/// A child review page's narrative section (`## key_summary`) ONLY — never its whole body,
/// so a quarterly/annual rollup never inherits the child's own distribution table or metadata
/// headings (which would render as duplicated tables and nested `##` on the parent).
///
/// Searches EVERY locale's heading (like `capture_section`/`backlinks`/`audit`) so a child
/// authored before a `vault.locale` switch is still found by its old-language heading. A
/// missing OR empty section yields `""` — there is NO whole-body fallback: an absent heading
/// (a custom template, or a not-yet-summarized queue-pending child) returns empty and the
/// caller drops it, rather than leaking the child's raw body into the parent.
fn child_narrative(page: &VaultPage) -> &str {
    lk_core::i18n::Locale::ALL
        .iter()
        .find_map(|l| section_body(&page.body, l.strings().key_summary))
        .map_or("", str::trim)
}

fn iso_year_week(date: jiff::civil::Date) -> (i16, u8) {
    let iwd = date.iso_week_date();
    (iwd.year(), iwd.week() as u8)
}

/// The distinct ISO year-weeks that any day in `[start, end]` falls into, in order.
/// Used to locate the weekly review narratives overlapping a quarter.
fn iso_weeks_in_range(
    start: jiff::civil::Date,
    end: jiff::civil::Date,
) -> Result<Vec<(i16, u8)>, PipelineError> {
    let mut weeks = Vec::new();
    let mut date = start;
    while date <= end {
        let yw = iso_year_week(date);
        if !weeks.contains(&yw) {
            weeks.push(yw);
        }
        date = date
            .checked_add(jiff::Span::new().days(1))
            .map_err(|e| PipelineError::Render(format!("date arithmetic: {e}")))?;
    }
    Ok(weeks)
}

fn iso_week_range(
    year: i16,
    week: u8,
) -> Result<(jiff::civil::Date, jiff::civil::Date), PipelineError> {
    let monday = jiff::civil::ISOWeekDate::new(year, week as i8, jiff::civil::Weekday::Monday)
        .map_err(|e| PipelineError::Render(format!("invalid ISO week: {e}")))?
        .date();
    let sunday = monday
        .checked_add(jiff::Span::new().days(6))
        .map_err(|e| PipelineError::Render(format!("date arithmetic: {e}")))?;
    Ok((monday, sunday))
}

fn month_range(
    year: i16,
    month: u8,
) -> Result<(jiff::civil::Date, jiff::civil::Date), PipelineError> {
    let start = jiff::civil::Date::new(year, month as i8, 1)
        .map_err(|e| PipelineError::Render(format!("invalid month: {e}")))?;
    let next_month_start = if month == 12 {
        jiff::civil::Date::new(year + 1, 1, 1)
    } else {
        jiff::civil::Date::new(year, (month + 1) as i8, 1)
    }
    .map_err(|e| PipelineError::Render(format!("invalid next month: {e}")))?;
    let end = next_month_start
        .checked_sub(jiff::Span::new().days(1))
        .map_err(|e| PipelineError::Render(format!("date arithmetic: {e}")))?;
    Ok((start, end))
}

fn quarter_range(
    year: i16,
    quarter: u8,
) -> Result<(jiff::civil::Date, jiff::civil::Date), PipelineError> {
    if !(1..=4).contains(&quarter) {
        return Err(PipelineError::Render(format!("invalid quarter: {quarter}")));
    }
    let start_month = (quarter - 1) * 3 + 1;
    let end_month = quarter * 3;
    let (start, _) = month_range(year, start_month)?;
    let (_, end) = month_range(year, end_month)?;
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_week_basic() {
        let date = jiff::civil::date(2026, 5, 23);
        let (year, week) = iso_year_week(date);
        assert_eq!(year, 2026);
        assert!((1..=53).contains(&week));
    }

    #[test]
    fn month_range_may_2026() {
        let (start, end) = month_range(2026, 5).unwrap();
        assert_eq!(start, jiff::civil::date(2026, 5, 1));
        assert_eq!(end, jiff::civil::date(2026, 5, 31));
    }

    #[test]
    fn month_range_december() {
        let (start, end) = month_range(2026, 12).unwrap();
        assert_eq!(start, jiff::civil::date(2026, 12, 1));
        assert_eq!(end, jiff::civil::date(2026, 12, 31));
    }

    #[test]
    fn month_range_february_leap() {
        let (_, end) = month_range(2024, 2).unwrap();
        assert_eq!(end, jiff::civil::date(2024, 2, 29));
    }

    #[test]
    fn quarter_range_q2() {
        let (start, end) = quarter_range(2026, 2).unwrap();
        assert_eq!(start, jiff::civil::date(2026, 4, 1));
        assert_eq!(end, jiff::civil::date(2026, 6, 30));
    }

    #[test]
    fn quarter_range_q4() {
        let (start, end) = quarter_range(2026, 4).unwrap();
        assert_eq!(start, jiff::civil::date(2026, 10, 1));
        assert_eq!(end, jiff::civil::date(2026, 12, 31));
    }

    #[test]
    fn reject_invalid_quarter() {
        assert!(quarter_range(2026, 5).is_err());
        assert!(quarter_range(2026, 0).is_err());
    }
}
