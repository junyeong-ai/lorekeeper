use std::collections::BTreeMap;
use std::sync::Arc;

use lk_core::config::{PerformanceConfig, VaultDirs};
use lk_core::event::Event;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_llm::LlmClient;
use lk_vault::TemplateEngine;

use crate::PipelineError;
use crate::render::RenderOutput;

pub async fn aggregate_and_render(
    events: &[Event],
    perf: &PerformanceConfig,
    engine: &TemplateEngine,
    dirs: &VaultDirs,
    locale: Locale,
    llm: &Arc<dyn LlmClient>,
) -> Result<Vec<RenderOutput>, PipelineError> {
    if events.is_empty() {
        return Ok(vec![]);
    }

    let mut by_date: BTreeMap<jiff::civil::Date, Vec<Event>> = BTreeMap::new();
    for event in events {
        by_date.entry(event.date).or_default().push(event.clone());
    }

    let mut outputs = Vec::new();
    for (date, day_events) in by_date {
        let groups = group_by_category(&day_events, perf);
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

        let path = VaultPath::work_log(dirs, date);

        let groups_json: Vec<serde_json::Value> = groups
            .iter()
            .map(|g| {
                serde_json::json!({
                    "category": g.category,
                    "items": g.items.iter().map(|i| serde_json::json!({
                        "summary": i.summary,
                        "source_id": i.source_id,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();

        let context = serde_json::json!({
            "date": date.to_string(),
            "categories": categories,
            "sources": sources,
            "groups": groups_json,
            "i18n": locale.strings(),
        });

        // The work-log template is embedded, so it always resolves.
        let content = engine
            .render("work-log.md.jinja", &context)
            .map_err(|e| PipelineError::Render(e.to_string()))?;

        outputs.push(RenderOutput {
            path: path.clone(),
            content,
        });

        // Emit a queue task for the LLM to fill the topic summary section with
        // cross-source correlation. The input concatenates every personal event's
        // title, body, and source_id so the LLM can group by topic across sources.
        let synthesis_input: String = day_events
            .iter()
            .map(|e| format!("[{}] {}\n{}", e.source_id, e.title, e.body))
            .collect::<Vec<_>>()
            .join("\n---\n");

        let anchor = format!("## {}", locale.strings().topic_summary);
        let vault_path = path.to_string();

        match llm
            .summarize(lk_llm::SummarizeRequest {
                text: synthesis_input,
                max_sentences: 10,
                target: lk_llm::TaskTarget {
                    vault_path,
                    kind: lk_llm::TargetKind::WorkLogSynthesis,
                    anchor,
                },
            })
            .await
        {
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

    Ok(outputs)
}

struct WorkLogGroup {
    category: String,
    items: Vec<WorkLogItem>,
}

struct WorkLogItem {
    summary: String,
    source_id: String,
}

fn group_by_category(events: &[Event], perf: &PerformanceConfig) -> Vec<WorkLogGroup> {
    let mut groups: Vec<WorkLogGroup> = perf
        .work_categories
        .iter()
        .map(|c| WorkLogGroup {
            category: c.clone(),
            items: vec![],
        })
        .collect();

    groups.push(WorkLogGroup {
        category: perf.uncategorized_label.clone(),
        items: vec![],
    });
    let other_idx = groups.len() - 1;

    for event in events {
        let item = WorkLogItem {
            summary: event.title.clone(),
            source_id: event.source_id.clone(),
        };

        let category = perf.resolve_category(
            &event.source_id,
            event.source_type,
            event.classification.as_deref(),
        );

        let idx = match category {
            Some(cat) => perf.work_categories.iter().position(|c| c == &cat),
            None => None,
        };

        groups[idx.unwrap_or(other_idx)].items.push(item);
    }

    groups.retain(|g| !g.items.is_empty());
    groups
}
