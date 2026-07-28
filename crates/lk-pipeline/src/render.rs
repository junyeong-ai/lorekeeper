use std::path::Path;

use lk_core::config::{HighlightSection, SourceType, VaultDirs};
use lk_core::event::Event;
use lk_core::frontmatter::field;
use lk_core::i18n::Locale;
use lk_core::link;
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

/// Relative path from a target page to the concepts directory — the base of every concept
/// link that page renders, computed here so no caller does relative-path arithmetic.
pub fn concepts_dir_dest(vault_path: &str, dirs: &VaultDirs) -> String {
    link::relative_dest(
        Path::new(vault_path),
        &lk_core::vault_path::concepts_dir(dirs),
    )
}

/// Render resolved concept identities into `[Name](<concepts_dir>/<slug>.md)` links for a
/// page at `vault_path`.
///
/// The slug is the one `ConceptDrafts` resolved, never re-derived from the display name: a
/// page's slug is not always `slugify(title)` — an alias or a renamed page resolves
/// elsewhere — so deriving it here again would point the citation at a page that was never
/// written.
pub(crate) fn concept_links(
    concepts: &[crate::concept_draft::ConceptIdentity],
    vault_path: &str,
    dirs: &VaultDirs,
) -> Vec<String> {
    let base = concepts_dir_dest(vault_path, dirs);
    concepts
        .iter()
        .map(|c| {
            link::md_link(
                &c.name,
                &format!("{base}/{}.md", link::encode_dest(&c.slug)),
            )
        })
        .collect()
}

/// Per-page LLM input hashes stamped into the rendered frontmatter. Subsequent
/// re-ingests read these back to decide whether each LLM task is still necessary.
/// `concepts` is optional because not every source extracts concepts.
pub struct DailyLlmInputHashes<'a> {
    /// Current-input hash for each LLM section — pre-stamped every render, so it is
    /// always the stale-task reference point.
    pub summary: &'a str,
    pub refine_events: &'a str,
    pub concepts: Option<&'a str>,
    /// Completion stamp for each section, owned by `/lore-process`. `Some` ONLY when the
    /// task is a cache hit (the on-disk marker equals the current input hash); `None` on
    /// a miss, so a stale marker is dropped rather than riding a changed-input render
    /// forward. Completion is uniformly marker-tracked — a non-empty body never signals
    /// done — so a legitimately-empty result (focus-filtered summary, empty extraction)
    /// stays cached instead of re-enqueueing forever.
    pub summary_done: Option<&'a str>,
    pub refine_events_done: Option<&'a str>,
    pub concepts_done: Option<&'a str>,
}

pub struct DailyRenderContext<'a> {
    pub source_id: &'a str,
    pub source_type: SourceType,
    pub date: jiff::civil::Date,
    pub events: &'a [&'a Event],
    pub labels: &'a [String],
    pub summary: &'a str,
    pub concepts: &'a [crate::concept_draft::ConceptIdentity],
    /// Whether this source extracts concepts. Templates render the `## Related Concepts`
    /// section only when true, so a source that opts out (`extract_concepts: false`,
    /// e.g. a personal work-log feed) doesn't carry a permanently-empty section.
    pub extract_concepts: bool,
    /// Config-declared highlight sections for this source: each surfaces events whose
    /// `Event::category` matches, under its own label, above the full event list.
    pub highlights: &'a [HighlightSection],
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
        highlights,
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

    // Exactly the fields the per-type templates consume — no speculative aliases.
    let events_json: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            serde_json::json!({
                "title": e.title,
                "body": e.body,
                "author": e.author,
                "url": e.url,
            })
        })
        .collect();

    // Pre-stamp the current-input hashes; then splice in each section's `*_done`
    // completion stamp (owned by the skill), keyed off the single-source
    // `completion_key`. Absent on a first ingest (skill hasn't run yet).
    let mut llm_inputs_json = llm_inputs_map(&[
        (TargetKind::DailySummary, Some(llm_inputs.summary)),
        (
            TargetKind::DailyRefineEvents,
            Some(llm_inputs.refine_events),
        ),
        (TargetKind::DailyConcepts, llm_inputs.concepts),
    ]);
    for (kind, done) in [
        (TargetKind::DailySummary, llm_inputs.summary_done),
        (TargetKind::DailyRefineEvents, llm_inputs.refine_events_done),
        (TargetKind::DailyConcepts, llm_inputs.concepts_done),
    ] {
        if let Some(stamp) = done {
            llm_inputs_json.insert(kind.completion_key(), stamp.into());
        }
    }

    let mut context = serde_json::json!({
        "date": date.to_string(),
        "source_id": source_id,
        "labels": labels,
        "events": events_json,
        "summary": summary,
        "concepts": concept_links(concepts, &path.to_string(), dirs),
        "extract_concepts": extract_concepts,
        "i18n": strings,
        (field::LLM_INPUTS): llm_inputs_json,
    });

    // Highlight sections: each configured highlight surfaces the events whose category
    // matches, under its own label, ABOVE the full event list (which still renders every
    // event, so a highlight never hides anything). Config-driven and generic — the core
    // branches on no source type. A source that declares none adds nothing.
    let highlight_sections: Vec<serde_json::Value> = highlights
        .iter()
        .map(|h| {
            serde_json::json!({
                "label": h.label,
                "items": filter_by_category(events, &h.category),
            })
        })
        .collect();
    context
        .as_object_mut()
        .expect("daily render context is a json object")
        .insert(
            "highlights".into(),
            serde_json::Value::Array(highlight_sections),
        );

    let source_template = format!("{source_id}.md.jinja");
    let type_template = source_type.descriptor().default_template;

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
    /// Completion stamps, owned by `/lore-process` — see [`DailyLlmInputHashes`].
    pub summary_done: Option<&'a str>,
    pub concepts_done: Option<&'a str>,
}

pub struct DocumentRenderContext<'a> {
    pub slug: &'a str,
    pub event: &'a Event,
    pub summary: &'a str,
    pub concepts: &'a [crate::concept_draft::ConceptIdentity],
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
    for (kind, done) in [
        (TargetKind::DocumentSummary, llm_inputs.summary_done),
        (TargetKind::DocumentConcepts, llm_inputs.concepts_done),
    ] {
        if let Some(stamp) = done {
            llm_inputs_json.insert(kind.completion_key(), stamp.into());
        }
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
        "concepts": concept_links(concepts, &path.to_string(), dirs),
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

/// A highlight bucket item is a POINTER — subject + sender only, never the body.
/// Buckets are STRUCTURAL (re-rendered every ingest, never LLM-refined), so any
/// body text here would pin the raw source (quoted thread, signature PII) into
/// the page permanently; the event's content lives once, in the Key Events list,
/// where the LLM refines it. The body key is absent from the item entirely (not
/// blanked), so no bucket rendering path can carry it — the raw body reaches a
/// page only through the `events` context, which every template routes through
/// the LLM-refined Key Events section.
fn filter_by_category(events: &[&Event], category: &str) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter(|e| e.category.as_deref() == Some(category))
        .map(|e| {
            serde_json::json!({
                "subject": e.title,
                "sender": e.author,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::config::SourceType;
    use lk_core::event::{Event, EventId};

    /// A highlight bucket item must be a POINTER — subject + sender only — so a
    /// structural (never-LLM-refined) section can never pin a raw source body into
    /// the page. Locked here because the leak it prevents is invisible until a real
    /// multi-paragraph body lands in a bucket.
    #[test]
    fn highlight_bucket_items_never_carry_a_body() {
        let ev = Event {
            id: EventId::new("s", jiff::civil::date(2026, 6, 7), "x"),
            source_id: "s".into(),
            source_type: SourceType::Gmail,
            timestamp: jiff::Timestamp::UNIX_EPOCH,
            date: jiff::civil::date(2026, 6, 7),
            title: "Subject".into(),
            body: "A quoted thread\nwith a signature and PII.".into(),
            url: None,
            author: Some("a@x.com".into()),
            labels: vec![],
            category: Some("action_required".into()),
            performance_category: None,
            is_self: false,
            is_personal: false,
            metadata: serde_json::Value::Null,
        };
        let items = filter_by_category(&[&ev], "action_required");
        let obj = items[0].as_object().expect("bucket item is an object");
        for forbidden in ["body", "summary", "content"] {
            assert!(
                !obj.contains_key(forbidden),
                "bucket item must not expose `{forbidden}` (raw-body leak vector): {obj:?}"
            );
        }
        assert_eq!(obj.get("subject").and_then(|v| v.as_str()), Some("Subject"));
        assert_eq!(obj.get("sender").and_then(|v| v.as_str()), Some("a@x.com"));
    }

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
