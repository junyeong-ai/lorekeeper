use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use lk_core::config::{PerformanceConfig, VaultDirs};
use lk_core::event::Event;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_queue::{LlmClient, SummarizeRequest, TargetKind, TaskTarget};
use lk_vault::{TemplateEngine, VaultStore};

use crate::PipelineError;
use crate::llm_cache::{self, SectionDecision};
use crate::render::{RenderResult, llm_inputs_map, splice_preserved_sections};

pub async fn render_work_log(
    events: &[Event],
    perf: &PerformanceConfig,
    engine: &TemplateEngine,
    dirs: &VaultDirs,
    locale: Locale,
    llm: &Arc<dyn LlmClient>,
    reader: &dyn VaultStore,
) -> Result<Vec<RenderResult>, PipelineError> {
    // The work-log is the performance subsystem; `performance.enabled` gates it at the
    // mechanism boundary so no caller can produce one while the subsystem is off.
    if !perf.enabled || events.is_empty() {
        return Ok(vec![]);
    }

    let mut by_date: BTreeMap<jiff::civil::Date, Vec<Event>> = BTreeMap::new();
    for event in events {
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

        let path = VaultPath::work_log(dirs, date);
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
            match llm.summarize(req).await {
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
            "daily_dir": dirs.daily,
            "i18n": locale.strings(),
            "llm_inputs": llm_inputs_map(&[(kind, Some(&hash))]),
        });

        // The work-log template is embedded, so it always resolves.
        let fresh = engine
            .render("work-log.md.jinja", &context)
            .map_err(|e| PipelineError::Render(e.to_string()))?;

        let content = splice_preserved_sections(fresh, std::iter::once((topic_heading, &decision)));

        outputs.push(RenderResult { path, content });
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
        .work_categories
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
            Some(cat) => perf.work_categories.iter().position(|c| c == &cat),
            None => None,
        };

        groups[idx.unwrap_or(other_idx)].count += 1;
    }

    groups.retain(|g| g.count > 0);
    groups
}
