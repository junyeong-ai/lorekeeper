use lk_core::config::{SourceType, VaultDirs};
use lk_core::event::Event;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_queue::{CacheShape, TargetKind};
use lk_vault::{TemplateEngine, replace_section};

use crate::PipelineError;
use crate::llm_cache::SectionDecision;

/// Build an `llm_inputs` frontmatter map keyed by `TargetKind::llm_inputs_key()`,
/// the single source of truth for kind→key. Every page type (daily, document,
/// work-log, synthesis) stamps through this so the key the page records always
/// matches the key the cache lookup reads. Entries with a `None` hash are skipped
/// (e.g. a source that doesn't extract concepts).
pub fn llm_inputs_map(
    entries: &[(TargetKind, Option<&str>)],
) -> serde_json::Map<String, serde_json::Value> {
    entries
        .iter()
        .filter_map(|(kind, hash)| hash.map(|h| (kind.llm_inputs_key().to_string(), h.into())))
        .collect()
}

pub struct RenderOutput {
    pub path: VaultPath,
    pub content: String,
}

/// Per-page LLM input hashes stamped into the rendered frontmatter. Subsequent
/// re-ingests read these back to decide whether each LLM task is still necessary.
/// `concepts` is optional because not every source extracts concepts.
pub struct LlmInputHashes<'a> {
    pub summary: &'a str,
    /// Current-input hash for the event list — pre-stamped every render, exactly like
    /// `summary`, so it is always the stale-task reference point.
    pub refine_events: &'a str,
    /// Completion stamp for the in-place event rewrite, owned by `/lore-process` and
    /// passed through unchanged here. `None` until the skill has refined for the
    /// current input; equal to `refine_events` once it has. The render→splice cycle
    /// preserves it (and the refined body) on a cache hit.
    pub refine_events_done: Option<&'a str>,
    pub concepts: Option<&'a str>,
}

pub struct RenderContext<'a> {
    pub source_id: &'a str,
    pub source_type: SourceType,
    pub date: jiff::civil::Date,
    pub events: &'a [&'a Event],
    pub labels: &'a [String],
    pub summary: &'a str,
    pub concepts: &'a [String],
    /// Whether this source extracts concepts. Templates render the `## Related Concepts`
    /// section only when true, so a source that opts out (`extract_concepts: false`,
    /// e.g. a personal work-log feed) doesn't carry a permanently-empty section.
    pub extract_concepts: bool,
    pub locale: Locale,
    pub llm_inputs: LlmInputHashes<'a>,
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
        llm_inputs,
    } = ctx;
    let source_id = *source_id;
    let source_type = *source_type;
    let date = *date;
    let extract_concepts = *extract_concepts;
    let locale = *locale;
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
                "classification": e.classification,
                "labels": e.labels,
                "is_personal": e.is_personal,
                "subject": e.title,
                "sender": e.author,
                "summary": e.body,
            })
        })
        .collect();

    // Pre-stamp the current-input hashes; then splice in the in-place completion
    // stamp (`refine_events_done`) the skill owns, keyed off the single-source
    // `cache_shape`. Absent on a first ingest (skill hasn't refined yet).
    let mut llm_inputs_json = llm_inputs_map(&[
        (TargetKind::DailySummary, Some(llm_inputs.summary)),
        (
            TargetKind::DailyRefineEvents,
            Some(llm_inputs.refine_events),
        ),
        (TargetKind::DailyConcepts, llm_inputs.concepts),
    ]);
    if let (CacheShape::InPlace { completion_key }, Some(done)) = (
        TargetKind::DailyRefineEvents.cache_shape(),
        llm_inputs.refine_events_done,
    ) {
        llm_inputs_json.insert(completion_key.to_string(), done.into());
    }

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
        "llm_inputs": llm_inputs_json,
    });

    let source_template = format!("{source_id}.md.jinja");
    let type_template = source_type.default_template_name();

    // A per-source override is honored only when the user actually placed a
    // `{source_id}.md.jinja` in their template dir; otherwise render the embedded
    // per-type template. (Embedded basenames are never treated as overrides.)
    let use_override = engine
        .has_user_override(&source_template)
        .map_err(|e| PipelineError::Render(e.to_string()))?;
    let chosen = if use_override {
        source_template.as_str()
    } else {
        type_template
    };
    let content = engine
        .render(chosen, &context)
        .map_err(|e| PipelineError::Render(e.to_string()))?;

    Ok(RenderOutput { path, content })
}

pub struct DocumentLlmInputHashes<'a> {
    pub summary: &'a str,
    pub concepts: Option<&'a str>,
}

pub struct DocumentRenderContext<'a> {
    pub slug: &'a str,
    pub event: &'a Event,
    pub summary: &'a str,
    pub concepts: &'a [String],
    pub extract_concepts: bool,
    pub locale: Locale,
    pub llm_inputs: DocumentLlmInputHashes<'a>,
}

pub fn render_document_page(
    ctx: &DocumentRenderContext<'_>,
    engine: &TemplateEngine,
    dirs: &VaultDirs,
) -> Result<RenderOutput, PipelineError> {
    let DocumentRenderContext {
        slug,
        event,
        summary,
        concepts,
        extract_concepts,
        locale,
        llm_inputs,
    } = ctx;
    let strings = locale.strings();

    let path = VaultPath::document(dirs, slug);

    // Derive document_type from source_file extension in metadata.
    let source_file = event
        .metadata
        .get("source_file")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let document_type = lk_core::document::document_type_for_extension(
        source_file.rsplit('.').next().unwrap_or(""),
    );

    let date = event.date;

    let mut tags = vec!["document".to_string()];
    tags.extend(event.labels.iter().cloned());

    let llm_inputs_json = llm_inputs_map(&[
        (TargetKind::DocumentSummary, Some(llm_inputs.summary)),
        (TargetKind::DocumentConcepts, llm_inputs.concepts),
    ]);

    let context = serde_json::json!({
        "slug": slug,
        "title": event.title,
        "created": date.to_string(),
        "updated": date.to_string(),
        "document_type": document_type,
        "source_url": event.url,
        "source_file": source_file,
        "tags": tags,
        "summary": summary,
        "content": event.body,
        "concepts": concepts,
        "extract_concepts": extract_concepts,
        "i18n": strings,
        "llm_inputs": llm_inputs_json,
    });

    let content = engine
        .render("document.md.jinja", &context)
        .map_err(|e| PipelineError::Render(e.to_string()))?;

    Ok(RenderOutput { path, content })
}

/// Splice cached LLM-filled bodies into a freshly-rendered page. Each
/// `(heading, decision)` pair represents an LLM-owned section: if the decision is
/// `cached` the existing body is written back over the empty section the template
/// just rendered. Bodies for non-cached decisions are left untouched so the queue
/// processor can fill them.
pub fn splice_preserved_sections<'a, I>(content: String, sections: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a SectionDecision)>,
{
    let mut out = content;
    for (heading, decision) in sections {
        if let Some(body) = &decision.preserved_body {
            out = replace_section(&out, heading, body);
        }
    }
    out
}

fn filter_by_class(events: &[&Event], class: &str) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter(|e| e.classification.as_deref() == Some(class))
        .map(|e| {
            serde_json::json!({
                "subject": e.title,
                "sender": e.author,
                "body": e.body,
            })
        })
        .collect()
}
