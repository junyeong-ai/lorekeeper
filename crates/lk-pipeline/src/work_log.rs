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
    for (date, mut day_events) in by_date {
        // Canonical order, like every other event-materializing page: the work-log groups
        // personal events from many sources, so without this its grouping AND its
        // `synthesis_input` (hashed for the cache) would depend on source-collection order.
        // Sorting through the one comparator makes the page bytes and the cache hash
        // independent of it — zero spurious re-enqueue on an unchanged day.
        day_events.sort_by(Event::canonical_cmp);
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

        // The topic synthesis groups events and skips trivial ones, so a day of only
        // trivial activity yields an empty topic summary — a valid finished result, not
        // "not done". Completion is marker-signalled (`topic_summary_done`), like every
        // LLM section, never inferred from an empty body.
        let completion_key = kind.completion_key();
        let existing = reader.read_page(Path::new(&vault_path)).await?;
        let decision: SectionDecision = llm_cache::lookup(
            existing.as_ref(),
            &completion_key,
            topic_heading,
            hash.clone(),
        );

        if decision.enqueue() {
            match ctx.llm.summarize(req).await {
                Ok(_) => {}
                Err(e) if e.is_fatal() => return Err(PipelineError::Queue(e)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        date = %date,
                        "work-log synthesis task failed; continuing without topic summary"
                    );
                }
            }
        }

        // Re-emit the completion marker only on a cache hit (where `lookup` proved it
        // equals the current hash); a miss drops a stale marker rather than riding a
        // changed-input render forward.
        let mut llm_inputs = llm_inputs_map(&[(kind, Some(&hash))]);
        if decision.cached {
            llm_inputs.insert(completion_key, hash.clone().into());
        }

        let context = serde_json::json!({
            "date": date.to_string(),
            "categories": categories,
            "sources": sources,
            "daily_dir": ctx.dirs.daily,
            "i18n": locale.strings(),
            (field::LLM_INPUTS): llm_inputs,
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

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::config::SourceType;
    use lk_core::event::{Event, EventId};

    fn event(performance_category: Option<&str>) -> Event {
        let date = jiff::civil::date(2026, 6, 1);
        Event {
            id: EventId::new("my-tasks", date, "x"),
            source_id: "my-tasks".into(),
            source_type: SourceType::Jira,
            timestamp: jiff::Timestamp::UNIX_EPOCH,
            date,
            title: "t".into(),
            body: "b".into(),
            url: None,
            author: None,
            labels: vec![],
            category: None,
            performance_category: performance_category.map(Into::into),
            is_self: true,
            is_personal: true,
            metadata: serde_json::Value::Null,
        }
    }

    fn perf(categories: &[&str]) -> PerformanceConfig {
        PerformanceConfig {
            enabled: true,
            performance_categories: categories.iter().map(|c| (*c).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn group_by_category_buckets_in_config_order_and_drops_empty_groups() {
        let perf = perf(&["project-delivery", "innovation", "team-contribution"]);
        let events = vec![
            event(Some("project-delivery")),
            event(Some("project-delivery")),
            event(Some("innovation")),
        ];
        let groups = group_by_category(&events, &perf, Locale::En);
        let view: Vec<(&str, usize)> = groups
            .iter()
            .map(|g| (g.category.as_str(), g.count))
            .collect();
        // Configured order is preserved; `team-contribution` (count 0) and the
        // uncategorized bucket (count 0) are dropped, never rendered as empty sections.
        assert_eq!(view, [("project-delivery", 2), ("innovation", 1)]);
    }

    #[test]
    fn group_by_category_routes_unresolved_events_to_the_uncategorized_bucket() {
        // No explicit performance_category and no source/type mapping → the event
        // still counts, under the locale's uncategorized label — work is never
        // silently dropped from the work-log because classification didn't fire.
        let perf = perf(&["project-delivery"]);
        let groups = group_by_category(&[event(None)], &perf, Locale::En);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].category, Locale::En.strings().uncategorized);
        assert_eq!(groups[0].count, 1);
    }

    #[test]
    fn group_by_category_never_invents_a_bucket_for_an_unconfigured_category() {
        // A classify rule carrying a performance_category outside the configured
        // list must not mint a new bucket — the event lands in uncategorized, so the
        // work-log's section vocabulary stays exactly `performance.performance_categories`.
        let perf = perf(&["project-delivery"]);
        let groups = group_by_category(&[event(Some("not-configured"))], &perf, Locale::En);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].category, Locale::En.strings().uncategorized);
        assert_eq!(groups[0].count, 1);
    }
}
