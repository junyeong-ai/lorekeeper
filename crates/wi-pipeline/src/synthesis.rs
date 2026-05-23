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

            let weekly_path = VaultPath::weekly_synthesis(&self.ctx.dirs, year, week).to_string();
            let summary = match self
                .ctx
                .llm
                .summarize(wi_llm::SummarizeRequest {
                    text: combined_source.clone(),
                    max_sentences: 5,
                    target: wi_llm::TaskTarget {
                        vault_path: weekly_path.clone(),
                        kind: wi_llm::TargetKind::WeeklySynthesisNarrative,
                    },
                })
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, source = %source_id, "synthesis summarize failed");
                    String::new()
                }
            };

            let fallback = if summary.is_empty() {
                format!("{} pages this week", pages.len())
            } else {
                summary.clone()
            };

            source_summaries.push(serde_json::json!({
                "source_id": source_id,
                "summary": fallback,
            }));

            combined.push_str(&format!("=== {source_id} ===\n{combined_source}\n\n"));
            covered_sources.push(source_id.clone());
        }

        if source_summaries.is_empty() {
            return Ok(None);
        }

        let weekly_path = VaultPath::weekly_synthesis(&self.ctx.dirs, year, week);
        let themes_text = match self
            .ctx
            .llm
            .summarize(wi_llm::SummarizeRequest {
                text: format!(
                    "Identify the top 3-5 themes across all sources this week:\n\n{combined}"
                ),
                max_sentences: 8,
                target: wi_llm::TaskTarget {
                    vault_path: weekly_path.to_string(),
                    kind: wi_llm::TargetKind::WeeklySynthesisNarrative,
                },
            })
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "weekly theme synthesis failed");
                String::new()
            }
        };

        // Weekly synthesis observes concepts already materialized during daily ingest;
        // it does not extract new ones here (which would not be persisted to wiki/concepts).
        let path = VaultPath::weekly_synthesis(&self.ctx.dirs, year, week);
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

        let content = if self.ctx.engine.available("weekly-synthesis.md.jinja") {
            self.ctx
                .engine
                .render("weekly-synthesis.md.jinja", &context)
                .map_err(|e| PipelineError::Render(e.to_string()))?
        } else {
            fallback_weekly_synthesis(year, week, &context)
        };

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
        let narrative = match self
            .ctx
            .llm
            .summarize(wi_llm::SummarizeRequest {
                text: format!(
                    "Summarize this week's personal work into key accomplishments by category:\n\n{combined}"
                ),
                max_sentences: 10,
                target: wi_llm::TaskTarget {
                    vault_path: path.to_string(),
                    kind: wi_llm::TargetKind::WeeklyPersonalNarrative,
                },
            })
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "weekly personal narrative failed");
                String::new()
            }
        };
        let context = serde_json::json!({
            "year": year,
            "week": week,
            "start_date": start.to_string(),
            "end_date": end.to_string(),
            "narrative": narrative,
            "days_logged": pages.len(),
            "categories": self.ctx.perf.work_categories,
        });

        let content = if self.ctx.engine.available("weekly-personal.md.jinja") {
            self.ctx
                .engine
                .render("weekly-personal.md.jinja", &context)
                .map_err(|e| PipelineError::Render(e.to_string()))?
        } else {
            fallback_personal(year, week, &context)
        };

        Ok(Some(RenderOutput { path, content }))
    }

    pub async fn monthly(
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
        let narrative = match self
            .ctx
            .llm
            .summarize(wi_llm::SummarizeRequest {
                text: format!(
                    "Generate a monthly work summary with key achievements and category distribution:\n\n{combined}"
                ),
                max_sentences: 15,
                target: wi_llm::TaskTarget {
                    vault_path: path.to_string(),
                    kind: wi_llm::TargetKind::MonthlyNarrative,
                },
            })
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "monthly narrative failed");
                String::new()
            }
        };
        let context = serde_json::json!({
            "year": year,
            "month": month,
            "start_date": start.to_string(),
            "end_date": end.to_string(),
            "narrative": narrative,
            "days_logged": pages.len(),
            "categories": self.ctx.perf.work_categories,
        });

        let content = if self.ctx.engine.available("monthly-summary.md.jinja") {
            self.ctx
                .engine
                .render("monthly-summary.md.jinja", &context)
                .map_err(|e| PipelineError::Render(e.to_string()))?
        } else {
            fallback_monthly(year, month, &context)
        };

        Ok(Some(RenderOutput { path, content }))
    }

    pub async fn quarterly(
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
        let narrative = match self
            .ctx
            .llm
            .summarize(wi_llm::SummarizeRequest {
                text: format!(
                    "Generate a quarterly performance review: top 5 achievements, category breakdown, growth areas, next direction:\n\n{combined}"
                ),
                max_sentences: 20,
                target: wi_llm::TaskTarget {
                    vault_path: path.to_string(),
                    kind: wi_llm::TargetKind::QuarterlyNarrative,
                },
            })
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "quarterly narrative failed");
                String::new()
            }
        };

        let category_stats = self.aggregate_category_stats(start, end).await?;

        let context = serde_json::json!({
            "year": year,
            "quarter": quarter,
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

        let content = if self.ctx.engine.available("quarterly-review.md.jinja") {
            self.ctx
                .engine
                .render("quarterly-review.md.jinja", &context)
                .map_err(|e| PipelineError::Render(e.to_string()))?
        } else {
            fallback_quarterly(year, quarter, &context)
        };

        Ok(Some(RenderOutput { path, content }))
    }

    pub async fn annual(&self, year: i16) -> Result<Option<RenderOutput>, PipelineError> {
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
        let narrative = match self
            .ctx
            .llm
            .summarize(wi_llm::SummarizeRequest {
                text: format!(
                    "Generate a comprehensive annual performance review based on quarterly summaries:\n\n{combined}"
                ),
                max_sentences: 25,
                target: wi_llm::TaskTarget {
                    vault_path: path.to_string(),
                    kind: wi_llm::TargetKind::AnnualNarrative,
                },
            })
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "annual narrative failed");
                String::new()
            }
        };

        let context = serde_json::json!({
            "year": year,
            "narrative": narrative,
            "quarter_summaries": quarter_summaries,
            "categories": self.ctx.perf.work_categories,
        });

        let content = if self.ctx.engine.available("annual-review.md.jinja") {
            self.ctx
                .engine
                .render("annual-review.md.jinja", &context)
                .map_err(|e| PipelineError::Render(e.to_string()))?
        } else {
            fallback_annual(year, &context)
        };

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

fn fallback_weekly_synthesis(year: i16, week: u8, ctx: &serde_json::Value) -> String {
    format!(
        "---\nid: synthesis-{year}-W{week:02}\ntitle: \"주간 종합 {year}-W{week:02}\"\ncreated: {}\nlabels: [\"synthesis\"]\n---\n\n# 주간 종합 {year}-W{week:02}\n\n{}\n",
        ctx["date"].as_str().unwrap_or(""),
        ctx["narrative"].as_str().unwrap_or("(no narrative)"),
    )
}

fn fallback_personal(year: i16, week: u8, ctx: &serde_json::Value) -> String {
    format!(
        "---\nid: me-{year}-W{week:02}\ntitle: \"내 주간 업무 {year}-W{week:02}\"\ncreated: {}\nlabels: [\"personal\"]\n---\n\n# 내 주간 업무 {year}-W{week:02}\n\n{}\n",
        ctx["end_date"].as_str().unwrap_or(""),
        ctx["narrative"].as_str().unwrap_or("(no narrative)"),
    )
}

fn fallback_monthly(year: i16, month: u8, ctx: &serde_json::Value) -> String {
    format!(
        "---\nid: me-{year}-{month:02}\ntitle: \"{year}년 {month}월 업무 요약\"\ncreated: {}\nlabels: [\"personal\"]\n---\n\n# {year}년 {month}월 업무 요약\n\n{}\n",
        ctx["end_date"].as_str().unwrap_or(""),
        ctx["narrative"].as_str().unwrap_or("(no narrative)"),
    )
}

fn fallback_quarterly(year: i16, quarter: u8, ctx: &serde_json::Value) -> String {
    format!(
        "---\nid: performance-{year}-Q{quarter}\ntitle: \"{year}년 {quarter}분기 성과 리뷰\"\ncreated: {}\nlabels: [\"personal\", \"strategy\"]\n---\n\n# {year}년 {quarter}분기 성과 리뷰\n\n{}\n",
        ctx["date"].as_str().unwrap_or(""),
        ctx["narrative"].as_str().unwrap_or("(no narrative)"),
    )
}

fn fallback_annual(year: i16, ctx: &serde_json::Value) -> String {
    format!(
        "---\nid: annual-{year}\ntitle: \"{year}년 연간 성과 리뷰\"\ncreated: {year}-12-31\nlabels: [\"personal\", \"strategy\"]\n---\n\n# {year}년 연간 성과 리뷰\n\n{}\n",
        ctx["narrative"].as_str().unwrap_or("(no narrative)"),
    )
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
