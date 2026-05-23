use std::collections::BTreeMap;

use wi_core::config::{PerformanceConfig, VaultDirs};
use wi_core::event::Event;
use wi_core::vault_path::VaultPath;
use wi_vault::TemplateEngine;

use crate::PipelineError;
use crate::render::RenderOutput;

pub fn aggregate_and_render(
    events: &[Event],
    perf: &PerformanceConfig,
    engine: &TemplateEngine,
    dirs: &VaultDirs,
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
        });

        let content = if engine.available("work-log.md.jinja") {
            engine
                .render("work-log.md.jinja", &context)
                .map_err(|e| PipelineError::Render(e.to_string()))?
        } else {
            format!(
                "---\nid: work-log-{date}\ntitle: \"업무 기록 {date}\"\ncreated: {date}\nlabels: [\"personal\"]\n---\n\n# 업무 기록 {date}\n\n"
            )
        };

        outputs.push(RenderOutput { path, content });
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
