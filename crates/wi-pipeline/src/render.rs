use wi_core::config::{SourceType, VaultDirs};
use wi_core::event::Event;
use wi_core::vault_path::VaultPath;
use wi_vault::TemplateEngine;

use crate::PipelineError;

pub struct RenderOutput {
    pub path: VaultPath,
    pub content: String,
}

pub struct RenderContext<'a> {
    pub source_id: &'a str,
    pub source_type: SourceType,
    pub date: jiff::civil::Date,
    pub events: &'a [Event],
    pub labels: &'a [String],
    pub summary: &'a str,
    pub concepts: &'a [String],
}

pub fn render_daily_page(
    ctx: &RenderContext<'_>,
    engine: &TemplateEngine,
    dirs: &VaultDirs,
) -> Result<RenderOutput, PipelineError> {
    let RenderContext {
        source_id,
        source_type,
        date,
        events,
        labels,
        summary,
        concepts,
    } = *ctx;

    let path = VaultPath::daily(dirs, source_id, date);

    let events_json: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "title": e.title,
                "body": e.body,
                "author": e.author,
                "url": e.url,
                "classification": e.classification,
                "labels": e.labels,
                "is_personal": e.is_personal,
                "subject": e.title,
                "sender": e.author,
                "summary": truncate(&e.body, 200),
            })
        })
        .collect();

    let action_items = filter_by_class(events, "action_required");
    let decision_items = filter_by_class(events, "decisions");
    let project_items = filter_by_class(events, "project_updates");
    let knowledge_items = filter_by_class(events, "knowledge_sharing");
    let meeting_items = filter_by_class(events, "meeting_followup");

    let context = serde_json::json!({
        "date": date.to_string(),
        "source_id": source_id,
        "labels": labels,
        "events": events_json,
        "summary": summary,
        "concepts": concepts,
        "events_count": events.len(),
        "total": events.len(),
        "filtered": events.len(),
        "filter_rate": 0,
        "action_count": action_items.len(),
        "action_required_items": action_items,
        "decision_items": decision_items,
        "project_items": project_items,
        "knowledge_items": knowledge_items,
        "meeting_items": meeting_items,
        "members": [],
        "clusters": [],
    });

    let source_template = format!("{source_id}.md.jinja");
    let type_template = default_template(source_type);

    let content = if engine.available(&source_template) {
        engine
            .render(&source_template, &context)
            .map_err(|e| PipelineError::Render(e.to_string()))?
    } else if engine.available(type_template) {
        engine
            .render(type_template, &context)
            .map_err(|e| PipelineError::Render(e.to_string()))?
    } else {
        render_fallback(source_id, date, events, labels, summary, concepts)
    };

    Ok(RenderOutput { path, content })
}

fn default_template(source_type: SourceType) -> &'static str {
    match source_type {
        SourceType::Gmail => "gmail.md.jinja",
        SourceType::GoogleDrive => "google-drive.md.jinja",
        SourceType::GoogleCalendar => "google-calendar.md.jinja",
        SourceType::SlackChannel => "slack-channel.md.jinja",
        SourceType::SlackSearch => "slack-search.md.jinja",
        SourceType::Jira => "jira.md.jinja",
    }
}

fn filter_by_class(events: &[Event], class: &str) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter(|e| e.classification.as_deref() == Some(class))
        .map(|e| {
            serde_json::json!({
                "subject": e.title,
                "sender": e.author,
                "summary": truncate(&e.body, 200),
            })
        })
        .collect()
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}

fn render_fallback(
    source_id: &str,
    date: jiff::civil::Date,
    events: &[Event],
    labels: &[String],
    summary: &str,
    concepts: &[String],
) -> String {
    let labels_json = serde_json::to_string(labels).unwrap_or_else(|_| "[]".into());
    let mut out = format!(
        "---\nid: {source_id}-{date}\ntitle: \"{source_id} {date}\"\ncreated: {date}\nlabels: {labels_json}\nevents_count: {}\n---\n\n# {source_id} {date}\n\n",
        events.len()
    );

    // Section anchors are always emitted (even with empty bodies) so the queue-mode
    // consumer `/wi-process` has a stable insertion point regardless of when the LLM
    // result arrives.
    out.push_str(&format!("## 요약\n\n{summary}\n\n"));

    for event in events {
        out.push_str(&format!("## {}\n\n{}\n\n", event.title, event.body));
    }

    out.push_str("## 관련 개념\n\n");
    for c in concepts {
        out.push_str(&format!("- [[{c}]]\n"));
    }

    out
}
