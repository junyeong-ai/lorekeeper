use lk_core::config::{SourceType, VaultDirs};
use lk_core::event::Event;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_vault::TemplateEngine;

use crate::PipelineError;

pub struct RenderOutput {
    pub path: VaultPath,
    pub content: String,
}

pub struct RenderContext<'a> {
    pub source_id: &'a str,
    pub source_type: SourceType,
    pub date: jiff::civil::Date,
    pub events: &'a [&'a Event],
    pub labels: &'a [String],
    pub summary: &'a str,
    pub concepts: &'a [String],
    /// Whether this source extracts concepts. Templates render the `## 관련 개념`
    /// section only when true, so a source that opts out (`extract_concepts: false`,
    /// e.g. a personal work-log feed) doesn't carry a permanently-empty section.
    pub extract_concepts: bool,
    pub locale: Locale,
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
        extract_concepts,
        locale,
    } = *ctx;
    let strings = locale.strings();

    let path = VaultPath::daily(dirs, source_id, date);

    let events_json: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "title": e.title,
                "body": e.body,
                "author": e.author,
                "url": e.url,
                "work_category": e.work_category,
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
        "extract_concepts": extract_concepts,
        "i18n": strings,
        "action_count": action_items.len(),
        "action_required_items": action_items,
        "decision_items": decision_items,
        "project_items": project_items,
        "knowledge_items": knowledge_items,
        "meeting_items": meeting_items,
    });

    let source_template = format!("{source_id}.md.jinja");
    let type_template = source_type.default_template_name();

    let avail = |name: &str| {
        engine
            .available(name)
            .map_err(|e| PipelineError::Render(e.to_string()))
    };
    let content = if avail(&source_template)? {
        engine
            .render(&source_template, &context)
            .map_err(|e| PipelineError::Render(e.to_string()))?
    } else {
        // The per-type template is always embedded, so this never falls through.
        engine
            .render(type_template, &context)
            .map_err(|e| PipelineError::Render(e.to_string()))?
    };

    Ok(RenderOutput { path, content })
}

fn filter_by_class(events: &[&Event], class: &str) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter(|e| e.work_category.as_deref() == Some(class))
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
