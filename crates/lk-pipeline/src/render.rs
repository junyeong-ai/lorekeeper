use lk_core::config::{SourceType, VaultDirs};
use lk_core::event::Event;
use lk_core::frontmatter::field;
use lk_core::i18n::Locale;
use lk_core::vault_path::VaultPath;
use lk_queue::TargetKind;
use lk_vault::{TemplateEngine, replace_section, section_body};

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

pub struct RenderResult {
    pub path: VaultPath,
    pub content: String,
}

/// Per-page LLM input hashes stamped into the rendered frontmatter. Subsequent
/// re-ingests read these back to decide whether each LLM task is still necessary.
/// `concepts` is optional because not every source extracts concepts.
pub struct DailyLlmInputHashes<'a> {
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
    /// Completion stamp for concept extraction, owned by `/lore-process`. Mirrors
    /// `refine_events_done`: concept extraction can legitimately find nothing, so an
    /// empty `## Related Concepts` body can't signal completion — this marker does.
    pub concepts_done: Option<&'a str>,
}

pub struct DailyRenderContext<'a> {
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
    pub llm_inputs: DailyLlmInputHashes<'a>,
}

pub fn render_daily_page(
    ctx: &DailyRenderContext<'_>,
    engine: &TemplateEngine,
    dirs: &VaultDirs,
) -> Result<RenderResult, PipelineError> {
    let DailyRenderContext {
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
                "category": e.category,
                "labels": e.labels,
                "is_personal": e.is_personal,
                "subject": e.title,
                "sender": e.author,
                "summary": e.body,
            })
        })
        .collect();

    // Pre-stamp the current-input hashes; then splice in the marker-completion
    // stamps the skill owns (`refine_events_done`, `concepts_done`), each keyed off
    // the single-source `cache_shape`. Absent on a first ingest (skill hasn't run yet).
    let mut llm_inputs_json = llm_inputs_map(&[
        (TargetKind::DailySummary, Some(llm_inputs.summary)),
        (
            TargetKind::DailyRefineEvents,
            Some(llm_inputs.refine_events),
        ),
        (TargetKind::DailyConcepts, llm_inputs.concepts),
    ]);
    for (kind, done) in [
        (TargetKind::DailyRefineEvents, llm_inputs.refine_events_done),
        (TargetKind::DailyConcepts, llm_inputs.concepts_done),
    ] {
        if let (Some(key), Some(stamp)) = (kind.cache_shape().completion_key(), done) {
            llm_inputs_json.insert(key.to_string(), stamp.into());
        }
    }

    let mut context = serde_json::json!({
        "date": date.to_string(),
        "source_id": source_id,
        "labels": labels,
        "events": events_json,
        "summary": summary,
        "concepts": concepts,
        "extract_concepts": extract_concepts,
        "i18n": strings,
        (field::LLM_INPUTS): llm_inputs_json,
    });

    // Highlight buckets: for each category the source declares, expose `{category}_items`
    // and `{category}_count` so its template can surface dedicated sections above the full
    // event list. A source that declares none adds nothing and pays for no filtering.
    let obj = context
        .as_object_mut()
        .expect("daily render context is a json object");
    for category in source_type.highlight_categories() {
        let items = filter_by_category(events, category);
        obj.insert(
            format!("{category}_count"),
            serde_json::Value::from(items.len()),
        );
        obj.insert(format!("{category}_items"), serde_json::Value::Array(items));
    }

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

    Ok(RenderResult { path, content })
}

pub struct DocumentLlmInputHashes<'a> {
    pub summary: &'a str,
    pub concepts: Option<&'a str>,
    /// Completion stamp for concept extraction, owned by `/lore-process` — see
    /// [`DailyLlmInputHashes::concepts_done`].
    pub concepts_done: Option<&'a str>,
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
) -> Result<RenderResult, PipelineError> {
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

    let mut llm_inputs_json = llm_inputs_map(&[
        (TargetKind::DocumentSummary, Some(llm_inputs.summary)),
        (TargetKind::DocumentConcepts, llm_inputs.concepts),
    ]);
    if let (Some(key), Some(stamp)) = (
        TargetKind::DocumentConcepts.cache_shape().completion_key(),
        llm_inputs.concepts_done,
    ) {
        llm_inputs_json.insert(key.to_string(), stamp.into());
    }

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
        (field::LLM_INPUTS): llm_inputs_json,
    });

    let content = engine
        .render("document.md.jinja", &context)
        .map_err(|e| PipelineError::Render(e.to_string()))?;

    Ok(RenderResult { path, content })
}

/// Splice cached LLM-filled bodies into a freshly-rendered page. Each
/// `(heading, decision)` pair represents an LLM-owned section: if the decision is
/// `cached`, its preserved body is written back over the empty section the template
/// just rendered. Bodies for non-cached decisions are left untouched so the queue
/// processor can fill them.
///
/// Returns `None` when a cached body cannot be placed because its heading is absent
/// from the fresh render — a custom `--template-dir` or a locale switch renamed the
/// section. Emitting the blanked render would drop the preserved LLM body, so the
/// caller keeps the previous on-disk page; a later run re-enqueues the section under
/// the new heading.
pub fn splice_preserved_sections<'a, I>(content: String, sections: I) -> Option<String>
where
    I: IntoIterator<Item = (&'a str, &'a SectionDecision)>,
{
    let mut out = content;
    for (heading, decision) in sections {
        if let Some(body) = &decision.preserved_body {
            if section_body(&out, heading).is_none() {
                tracing::warn!(
                    heading,
                    "preserved LLM section not spliceable: heading missing in rendered \
                     template — keeping previous page"
                );
                return None;
            }
            out = replace_section(&out, heading, body);
        }
    }
    Some(out)
}

fn filter_by_category(events: &[&Event], category: &str) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter(|e| e.category.as_deref() == Some(category))
        .map(|e| {
            serde_json::json!({
                "subject": e.title,
                "sender": e.author,
                "body": e.body,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(body: &str) -> SectionDecision {
        SectionDecision {
            hash: "h".into(),
            cached: true,
            preserved_body: Some(body.into()),
        }
    }

    fn uncached() -> SectionDecision {
        SectionDecision {
            hash: "h".into(),
            cached: false,
            preserved_body: None,
        }
    }

    #[test]
    fn splice_writes_cached_bodies_and_leaves_uncached_sections_for_the_queue() {
        let fresh = "# Page\n\n## Summary\n\n## Concepts\n\n".to_string();
        let summary = cached("A preserved summary body.");
        let concepts = uncached();
        let out =
            splice_preserved_sections(fresh, [("Summary", &summary), ("Concepts", &concepts)])
                .expect("every cached heading exists in the fresh render");
        assert!(
            out.contains("A preserved summary body."),
            "cached body must be written over the empty section:\n{out}"
        );
        assert_eq!(
            section_body(&out, "Concepts").map(str::trim),
            Some(""),
            "an uncached section must stay empty for the queue processor to fill:\n{out}"
        );
    }

    #[test]
    fn splice_refuses_when_a_cached_heading_is_missing_from_the_render() {
        // A custom `--template-dir` or a locale switch renamed `## Summary` in the
        // fresh render. Emitting it would drop the preserved LLM body, so the splice
        // must return `None` — the caller keeps the previous on-disk page and a later
        // run re-enqueues the section under the new heading.
        let fresh = "# Page\n\n## Daily Overview\n\n".to_string();
        let summary = cached("Body that must not be lost.");
        assert!(
            splice_preserved_sections(fresh, [("Summary", &summary)]).is_none(),
            "a missing heading must refuse the splice rather than drop the body"
        );
    }
}
