use std::collections::BTreeMap;
use std::path::Path;

use lk_core::config::PerformanceConfig;
use lk_core::event::Event;
use lk_core::frontmatter::field;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_queue::{SummarizeRequest, TargetKind, TaskTarget};
use lk_vault::VaultStore;

use crate::PipelineContext;
use crate::PipelineError;
use crate::llm_cache::{self, SectionDecision};
use crate::render::{RenderResult, llm_inputs_map, splice_preserved_sections};

pub async fn render_work_log(
    events: &[Event],
    today: jiff::civil::Date,
    ctx: &PipelineContext,
    reader: &dyn VaultStore,
) -> Result<Vec<RenderResult>, PipelineError> {
    let perf = &ctx.perf;
    let locale = ctx.locale;

    // The work-log is the performance subsystem; `performance.enabled` gates it at the
    // mechanism boundary so no caller can produce one while the subsystem is off.
    if !perf.enabled || events.is_empty() {
        return Ok(vec![]);
    }

    // The work-log records PERFORMED contribution, so it is strictly backward-looking:
    // an event dated after `today` is a not-yet-occurred commitment (a calendar
    // look-ahead event, say), never work done. The work-log is event-driven (it never
    // goes through the daily-page render that already drops forecast dates), so it owns
    // this temporal gate itself — no caller can leak a future contribution into a
    // work-log page or a performance review.
    let mut by_date: BTreeMap<jiff::civil::Date, Vec<Event>> = BTreeMap::new();
    for event in events {
        if event.date > today {
            continue;
        }
        by_date.entry(event.date).or_default().push(event.clone());
    }

    let topic_heading = locale.strings().topic_summary;

    let mut outputs = Vec::new();
    for (date, day_events) in by_date {
        let groups = group_by_category(&day_events, perf, locale);
        if groups.is_empty() {
            continue;
        }

        let sources: Vec<String> = {
            let mut s: Vec<String> = day_events.iter().map(|e| e.source_id.clone()).collect();
            s.sort();
            s.dedup();
            s
        };
        let categories: Vec<String> = groups.iter().map(|g| g.category.clone()).collect();

        let path = VaultPath::work_log(&ctx.dirs, date);
        let vault_path = path.to_string();

        let synthesis_input: String = day_events
            .iter()
            .map(|e| format!("[{}] {}\n{}", e.source_id, e.title, e.body))
            .collect::<Vec<_>>()
            .join("\n---\n");

        let req = SummarizeRequest {
            text: synthesis_input,
            max_sentences: 10,
            focus: None,
            locale: locale.tag().to_string(),
            // Work-log topic synthesis is cross-source by design (it groups personal
            // events from many sources), so no single source_type applies.
            source_type: None,
            target: TaskTarget {
                vault_path: vault_path.clone(),
                kind: TargetKind::WorkLogSynthesis,
                anchor: format!("## {topic_heading}"),
            },
        };
        let hash = req.cache_hash();
        let kind = req.target.kind;

        let existing = reader.read_page(Path::new(&vault_path)).await?;
        let decision: SectionDecision = llm_cache::lookup(
            existing.as_ref(),
            kind.llm_inputs_key(),
            topic_heading,
            hash.clone(),
        );

        if decision.enqueue() {
            match ctx.llm.summarize(req).await {
                Ok(_) => {}
                Err(e) if e.is_fatal() => return Err(PipelineError::Llm(e)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        date = %date,
                        "work-log synthesis task failed; continuing without topic summary"
                    );
                }
            }
        }

        let context = serde_json::json!({
            "date": date.to_string(),
            "categories": categories,
            "sources": sources,
            "daily_dir": ctx.dirs.daily,
            "i18n": locale.strings(),
            (field::LLM_INPUTS): llm_inputs_map(&[(kind, Some(&hash))]),
        });

        // The work-log template is embedded, so it always resolves.
        let fresh = ctx
            .engine
            .render("work-log.md.jinja", &context)
            .map_err(|e| PipelineError::Render(e.to_string()))?;

        if let Some(content) =
            splice_preserved_sections(fresh, std::iter::once((topic_heading, &decision)))
        {
            outputs.push(RenderResult { path, content });
        }
    }

    Ok(outputs)
}

struct WorkLogGroup {
    category: String,
    count: usize,
}

fn group_by_category(
    events: &[Event],
    perf: &PerformanceConfig,
    locale: Locale,
) -> Vec<WorkLogGroup> {
    let mut groups: Vec<WorkLogGroup> = perf
        .performance_categories
        .iter()
        .map(|c| WorkLogGroup {
            category: c.clone(),
            count: 0,
        })
        .collect();

    groups.push(WorkLogGroup {
        category: perf.uncategorized_label(locale).to_owned(),
        count: 0,
    });
    let other_idx = groups.len() - 1;

    for event in events {
        let category = perf.resolve_category(
            &event.source_id,
            event.source_type,
            event.performance_category.as_deref(),
        );

        let idx = match category {
            Some(cat) => perf.performance_categories.iter().position(|c| c == &cat),
            None => None,
        };

        groups[idx.unwrap_or(other_idx)].count += 1;
    }

    groups.retain(|g| g.count > 0);
    groups
}
