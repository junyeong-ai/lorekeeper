use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use wi_core::config::Config;
use wi_core::vault_path::VaultPath;
use wi_vault::{Page, VaultReader};

use crate::PipelineError;
use crate::context::PipelineContext;
use crate::render::RenderOutput;

pub struct Synthesizer {
    ctx: Arc<PipelineContext>,
    reader: VaultReader,
    sources: Vec<String>,
}

impl Synthesizer {
    pub fn new(vault_root: &Path, ctx: Arc<PipelineContext>, config: &Config) -> Self {
        let reader = VaultReader::new(vault_root);
        let sources = if config.synthesis.weekly.include_sources.is_empty() {
            config
                .enabled_sources()
                .map(|(id, _)| id.to_string())
                .collect()
        } else {
            config.synthesis.weekly.include_sources.clone()
        };
        Self {
            ctx,
            reader,
            sources,
        }
    }

    /// Run a summarize task, propagating only fatal (persistence) errors. A transient
    /// LLM failure degrades to an empty narrative with a warning, so synthesis still
    /// produces a page. Centralizes the fatal/non-fatal split every period shares.
    async fn summarize_or_warn(
        &self,
        text: String,
        max_sentences: usize,
        vault_path: String,
        kind: wi_llm::TargetKind,
        what: &str,
    ) -> Result<String, PipelineError> {
        match self
            .ctx
            .llm
            .summarize(wi_llm::SummarizeRequest {
                text,
                max_sentences,
                target: wi_llm::TaskTarget { vault_path, kind },
            })
            .await
        {
            Ok(s) => Ok(s),
            Err(e) if e.is_fatal() => Err(PipelineError::Llm(e)),
            Err(e) => {
                tracing::warn!(error = %e, what, "synthesis summarize failed");
                Ok(String::new())
            }
        }
    }

    /// Render a synthesis/personal template, injecting localized labels as `i18n.*`.
    /// Every period template is embedded, so this always resolves; a user template dir
    /// merely overrides.
    fn render(&self, template: &str, context: &serde_json::Value) -> Result<String, PipelineError> {
        let mut ctx = context.clone();
        if let Some(map) = ctx.as_object_mut() {
            let i18n = serde_json::to_value(self.ctx.locale.strings())
                .map_err(|e| PipelineError::Render(e.to_string()))?;
            map.insert("i18n".to_string(), i18n);
        }
        self.ctx
            .engine
            .render(template, &ctx)
            .map_err(|e| PipelineError::Render(e.to_string()))
    }

    pub async fn weekly_synthesis(
        &self,
        date: jiff::civil::Date,
    ) -> Result<Option<RenderOutput>, PipelineError> {
        let (year, week) = iso_year_week(date);
        let (start, end) = iso_week_range(year, week)?;

        let mut source_summaries: Vec<serde_json::Value> = Vec::new();
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

            // Per-source summaries reuse the same target as the cross-source theme
            // summary below; queuing both would emit two tasks pointing at the same
            // narrative section. Skip the per-source LLM call — the page count serves
            // as a deterministic placeholder. The cross-source themes call below is
            // the one that drives the actual narrative content.
            source_summaries.push(serde_json::json!({
                "source_id": source_id,
                "summary": format!("{} pages this week", pages.len()),
            }));

            combined.push_str(&format!("=== {source_id} ===\n{combined_source}\n\n"));
            covered_sources.push(source_id.clone());
        }

        if source_summaries.is_empty() {
            return Ok(None);
        }

        let path = VaultPath::weekly_synthesis(&self.ctx.dirs, year, week);
        let themes_text = self
            .summarize_or_warn(
                format!("Identify the top 3-5 themes across all sources this week:\n\n{combined}"),
                8,
                path.to_string(),
                wi_llm::TargetKind::WeeklySynthesisNarrative,
                "weekly synthesis",
            )
            .await?;

        // Weekly synthesis observes concepts already materialized during daily ingest;
        // it does not extract new ones here (which would not be persisted to wiki/concepts).
        let context = serde_json::json!({
            "year": year,
            "week": week,
            "date": end.to_string(),
            "labels": ["synthesis"],
            "sources": covered_sources,
            "themes": split_into_themes(&themes_text),
            "source_summaries": source_summaries,
            "new_concepts": Vec::<String>::new(),
            "narrative": themes_text,
        });

        let content = self.render("weekly-synthesis.md.jinja", &context)?;

        Ok(Some(RenderOutput { path, content }))
    }

    pub async fn weekly_personal(
        &self,
        date: jiff::civil::Date,
    ) -> Result<Option<RenderOutput>, PipelineError> {
        let (year, week) = iso_year_week(date);
        let (start, end) = iso_week_range(year, week)?;

        let dir = PathBuf::from(&self.ctx.dirs.personal).join("work-log");
        let pages = self.read_date_range(&dir, start, end).await?;

        if pages.is_empty() {
            return Ok(None);
        }

        let combined = pages
            .iter()
            .map(|p| p.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let path = VaultPath::weekly_personal(&self.ctx.dirs, year, week);
        let narrative = self
            .summarize_or_warn(
                format!(
                    "Summarize this week's personal work into key accomplishments by category:\n\n{combined}"
                ),
                10,
                path.to_string(),
                wi_llm::TargetKind::WeeklyPersonalNarrative,
                "weekly personal",
            )
            .await?;
        let context = serde_json::json!({
            "year": year,
            "week": week,
            "start_date": start.to_string(),
            "end_date": end.to_string(),
            "narrative": narrative,
            "days_logged": pages.len(),
            "categories": self.ctx.perf.work_categories,
        });

        let content = self.render("weekly-personal.md.jinja", &context)?;

        Ok(Some(RenderOutput { path, content }))
    }

    pub async fn monthly_personal(
        &self,
        year: i16,
        month: u8,
    ) -> Result<Option<RenderOutput>, PipelineError> {
        let (start, end) = month_range(year, month)?;
        let dir = PathBuf::from(&self.ctx.dirs.personal).join("work-log");
        let pages = self.read_date_range(&dir, start, end).await?;

        if pages.is_empty() {
            return Ok(None);
        }

        let combined = pages
            .iter()
            .map(|p| p.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let path = VaultPath::monthly_personal(&self.ctx.dirs, year, month);
        let narrative = self
            .summarize_or_warn(
                format!(
                    "Generate a monthly work summary with key achievements and category distribution:\n\n{combined}"
                ),
                15,
                path.to_string(),
                wi_llm::TargetKind::MonthlyPersonalNarrative,
                "monthly",
            )
            .await?;
        let context = serde_json::json!({
            "year": year,
            "month": month,
            "title": self.ctx.locale.monthly_title(year, month as i8),
            "start_date": start.to_string(),
            "end_date": end.to_string(),
            "narrative": narrative,
            "days_logged": pages.len(),
            "categories": self.ctx.perf.work_categories,
        });

        let content = self.render("monthly-summary.md.jinja", &context)?;

        Ok(Some(RenderOutput { path, content }))
    }

    pub async fn quarterly_personal(
        &self,
        year: i16,
        quarter: u8,
    ) -> Result<Option<RenderOutput>, PipelineError> {
        let (start, end) = quarter_range(year, quarter)?;
        let months: Vec<u8> = ((quarter - 1) * 3 + 1..=quarter * 3).collect();

        let monthly_dir = PathBuf::from(&self.ctx.dirs.monthly).join(&self.ctx.dirs.personal);
        let mut monthly_summaries: Vec<serde_json::Value> = Vec::new();
        let mut combined = String::new();

        for m in &months {
            let file = monthly_dir.join(format!("{year}-{:02}.md", m));
            if let Some(page) = self.reader.read_page(&file).await? {
                monthly_summaries.push(serde_json::json!({
                    "month": format!("{year}-{:02}", m),
                    "summary": &page.body,
                }));
                combined.push_str(&format!("=== {year}-{:02} ===\n", m));
                combined.push_str(&page.body);
                combined.push_str("\n\n");
            }
        }

        if monthly_summaries.is_empty() {
            let dir = PathBuf::from(&self.ctx.dirs.personal).join("work-log");
            let pages = self.read_date_range(&dir, start, end).await?;
            if pages.is_empty() {
                return Ok(None);
            }
            for page in &pages {
                combined.push_str(&page.body);
                combined.push_str("\n\n");
            }
        }

        let path = VaultPath::quarterly_personal(&self.ctx.dirs, year, quarter);
        let narrative = self
            .summarize_or_warn(
                format!(
                    "Generate a quarterly performance review: top 5 achievements, category breakdown, growth areas, next direction:\n\n{combined}"
                ),
                20,
                path.to_string(),
                wi_llm::TargetKind::QuarterlyPersonalNarrative,
                "quarterly",
            )
            .await?;

        let category_stats = self.aggregate_category_stats(start, end).await?;

        let context = serde_json::json!({
            "year": year,
            "quarter": quarter,
            "title": self.ctx.locale.quarterly_title(year, quarter),
            "date": end.to_string(),
            "start_date": start.to_string(),
            "end_date": end.to_string(),
            "category_stats": category_stats,
            "achievements": split_into_lines(&narrative)
                .into_iter()
                .take(5)
                .collect::<Vec<_>>(),
            "monthly_summaries": monthly_summaries,
            "narrative": narrative,
            "team_contribution": "",
            "growth_areas": "",
            "next_direction": "",
        });

        let content = self.render("quarterly-review.md.jinja", &context)?;

        Ok(Some(RenderOutput { path, content }))
    }

    pub async fn annual_personal(&self, year: i16) -> Result<Option<RenderOutput>, PipelineError> {
        let quarterly_dir = PathBuf::from(&self.ctx.dirs.quarterly).join(&self.ctx.dirs.personal);
        let mut quarter_summaries: Vec<serde_json::Value> = Vec::new();
        let mut combined = String::new();

        for q in 1..=4u8 {
            let file = quarterly_dir.join(format!("{year}-Q{q}.md"));
            if let Some(page) = self.reader.read_page(&file).await? {
                quarter_summaries.push(serde_json::json!({
                    "quarter": format!("Q{q}"),
                    "summary": &page.body,
                }));
                combined.push_str(&format!("=== {year}-Q{q} ===\n"));
                combined.push_str(&page.body);
                combined.push_str("\n\n");
            }
        }

        if quarter_summaries.is_empty() {
            return Ok(None);
        }

        let path = VaultPath::annual_personal(&self.ctx.dirs, year);
        let narrative = self
            .summarize_or_warn(
                format!(
                    "Generate a comprehensive annual performance review based on quarterly summaries:\n\n{combined}"
                ),
                25,
                path.to_string(),
                wi_llm::TargetKind::AnnualPersonalNarrative,
                "annual",
            )
            .await?;

        let context = serde_json::json!({
            "year": year,
            "title": self.ctx.locale.annual_title(year),
            "narrative": narrative,
            "quarter_summaries": quarter_summaries,
            "categories": self.ctx.perf.work_categories,
        });

        let content = self.render("annual-review.md.jinja", &context)?;

        Ok(Some(RenderOutput { path, content }))
    }

    async fn aggregate_category_stats(
        &self,
        start: jiff::civil::Date,
        end: jiff::civil::Date,
    ) -> Result<Vec<serde_json::Value>, PipelineError> {
        let dir = PathBuf::from(&self.ctx.dirs.personal).join("work-log");
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

        if total == 0 {
            // No work-log data → emit empty stats so the template omits the table
            // rather than rendering all-zero rows.
            return Ok(vec![]);
        }

        let stats = self
            .ctx
            .perf
            .work_categories
            .iter()
            .filter_map(|cat| {
                let count = counts.get(cat).copied().unwrap_or(0);
                if count == 0 {
                    return None;
                }
                let percent = (count as f64 / total as f64 * 100.0).round() as u32;
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
    ) -> Result<Vec<Page>, PipelineError> {
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

fn iso_year_week(date: jiff::civil::Date) -> (i16, u8) {
    let iwd = date.iso_week_date();
    (iwd.year(), iwd.week() as u8)
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

fn split_into_themes(text: &str) -> Vec<serde_json::Value> {
    split_into_lines(text)
        .into_iter()
        .map(|line| {
            let (title, rest) = line.split_once(':').unwrap_or((line.as_str(), ""));
            serde_json::json!({
                "title": title.trim(),
                "description": rest.trim(),
            })
        })
        .collect()
}

fn split_into_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let trimmed = l.trim();
            if trimmed.is_empty() {
                return None;
            }
            let cleaned = trimmed
                .trim_start_matches(['-', '*', '•'])
                .trim_start_matches(char::is_numeric)
                .trim_start_matches('.')
                .trim();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned.to_string())
            }
        })
        .collect()
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
