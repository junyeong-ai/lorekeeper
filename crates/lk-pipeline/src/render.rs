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
    /// Section headings on the existing page whose body this write replaces with an empty one.
    ///
    /// Travels with the page rather than beside it, so a caller cannot report a loss for a
    /// write that did not happen: no page, no entry. The pipeline's own flow re-enqueues each
    /// one and a drain fills it again, so this is routine — it is carried because the same
    /// state describes a section somebody answered WITHOUT recording it, and rewriting the
    /// page is not reversible.
    pub discarded: Vec<String>,
}

impl RenderResult {
    /// A freshly rendered page, before any preserved body is spliced back into it. Nothing is
    /// discarded yet — [`splice_preserved_sections`] decides that, and only for a page that
    /// will be written.
    pub fn fresh(path: VaultPath, content: String) -> Self {
        Self {
            path,
            content,
            discarded: Vec::new(),
        }
    }

    /// A page whose preserved bodies have been spliced back in, ready to write.
    pub fn spliced(path: VaultPath, spliced: Spliced) -> Self {
        Self {
            path,
            content: spliced.content,
            discarded: spliced.discarded,
        }
    }
}

/// Relative path from a target page to the concepts directory — the base of every concept
/// link that page renders, computed here so no caller does relative-path arithmetic.
pub fn concepts_dir_dest(vault_path: &str, dirs: &VaultDirs) -> String {
    link::relative_dest(
        Path::new(vault_path),
        &lk_core::vault_path::concepts_dir(dirs),
    )
}

/// Render resolved concept identities into the LINES of a page's related-concepts section:
/// `- [Name](<concepts_dir>/<slug>.md)`, one per concept.
///
/// The bullet belongs here and not in the templates because this section has TWO writers —
/// the plan render, which goes through a template, and `lore queue apply`, which writes the
/// body directly — and whichever of them ran last decided the markup. They disagreed: the
/// templates bulleted the list and apply did not, so an ingest and a drain flipped a page
/// between a list and a run of paragraphs. Returning the finished line leaves one place that
/// says what a citation looks like.
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
            format!(
                "- {}",
                link::md_link(
                    &c.name,
                    &format!("{base}/{}.md", link::encode_dest(&c.slug))
                )
            )
        })
        .collect()
}

/// The concepts a page cites once an extraction has been applied to it: everything the
/// page already cited, plus everything the extraction reported that it didn't.
///
/// Concept extraction is the one LLM-owned section whose result creates durable pages
/// OUTSIDE the page it is written on, and `lore graph backlinks-sync` re-derives each of
/// those pages' sources section and `source_count` from exactly these forward links. So a
/// section that merely restated the newest extraction would not just drop a link: it would
/// strip a page that still asserts knowledge of the evidence justifying it, permanently,
/// because nothing records what the superseded extraction had cited. Every other section
/// states something about its own input and is replaced wholesale.
///
/// A page's concept links therefore only ever accumulate. What that rests on is that a
/// page's set of OBSERVATIONS only grows: a streaming source's date unions each fetch into
/// its event log, and a complete-refetch source's date is a closed window.
///
/// It does NOT rest on any one observation's text being fixed, and for most adapters it
/// isn't. Only `confluence` and `manual` key an item's identity to its content (page
/// version, file fingerprint); the rest key it to the item's id, so an in-place edit
/// re-reads under the same `EventId` and re-renders the day. A concept extracted before
/// such an edit keeps its citation afterwards. That is the intended reading: the citation
/// records that this page's material named the concept when it was observed, which stays
/// true, and the alternative — letting a later extraction retract it — is the loss above.
///
/// A link is carried only when it addresses a concept page at the address
/// [`concept_links`] writes — resolved relative to the citing page, `.md`, a DIRECT child
/// of the concepts directory. That is narrower than the prefix match `lore graph
/// backlinks-sync` counts an edge by, and deliberately: this set is re-RENDERED through
/// `concept_links`, which flattens any slug to one path segment, so carrying a nested
/// destination would rewrite it into a link that resolves nowhere. Nothing the pipeline
/// writes is ever nested (`slugify` maps `/` to `-`), so the two agree on every link this
/// system produces.
///
/// The section is rebuilt from the returned set, so anything else living in it does not
/// survive — as was already true when each extraction replaced the section outright. It is
/// LLM-owned; hand-authored prose belongs in a section that is not.
pub(crate) fn accumulate_concepts(
    cited: Option<&str>,
    extracted: Vec<crate::concept_draft::ConceptIdentity>,
    vault_path: &str,
    dirs: &VaultDirs,
) -> Vec<crate::concept_draft::ConceptIdentity> {
    let page = Path::new(vault_path);
    let concepts_dir = lk_core::vault_path::concepts_dir(dirs);
    let mut seen = std::collections::HashSet::new();
    let mut concepts = Vec::new();

    // Two citations are the same citation when they address the same PAGE, and the vault
    // decides that at the id level — the graph resolves a destination to `slugify` of its
    // stem, so `Bad_Name.md` and `bad-name.md` are one page to every consumer that counts
    // edges. Deduping on the raw stem instead would let a non-canonical spelling ride
    // alongside the canonical one, and since these links only ever accumulate, the double
    // citation would be permanent.
    let mut claim = |stem: &str| match lk_core::concept::slugify(stem) {
        Some(id) => seen.insert(id),
        // Not a name this vault can address, so it cannot be reconciled with anything —
        // said out loud rather than folded into the dedup, where a dropped citation would
        // be indistinguishable from a deduplicated one. Only a carried destination can
        // reach this: an extracted concept's slug is already `slugify` output.
        None => {
            tracing::warn!(stem, "carried citation has no addressable id; dropping it");
            false
        }
    };

    let carried = cited.map(link::extract_page_links).unwrap_or_default();
    for cite in carried {
        let Some(resolved) = link::resolve_dest(page, &cite.dest) else {
            continue;
        };
        if resolved.parent() != Some(concepts_dir.as_path())
            || resolved.extension() != Some(std::ffi::OsStr::new("md"))
        {
            continue;
        }
        let Some(slug) = resolved.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // The destination is kept exactly as written. Rewriting it to the canonical
        // spelling here would repoint a citation on the strength of a name, which is
        // `lore graph normalize`'s job — it renames the page in the same pass.
        if claim(slug) {
            concepts.push(crate::concept_draft::ConceptIdentity {
                name: cite.text,
                slug: slug.to_string(),
            });
        }
    }

    for concept in extracted {
        if claim(&concept.slug) {
            concepts.push(concept);
        }
    }

    concepts
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

    Ok(RenderResult::fresh(path, content))
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

    Ok(RenderResult::fresh(path, content))
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
///
/// The headings whose existing body this render replaces with an empty one come back with
/// the content, so they are reported by the same thing that decides a page is written at
/// all — a run that returns `None` writes nothing and so discards nothing.
///
/// Emptiness is read off the FINISHED page, never inferred from the decision. A cache miss
/// means only that the section was not answered for this input; whether the render leaves it
/// empty is the template's business, and they differ. `{{ summary }}` renders `""` on a miss —
/// genuinely emptied. Related-concepts renders the ACCUMULATED links, which
/// [`accumulate_concepts`] carries forward precisely so a citation is never retracted. Reporting
/// the second as a loss would fire on the ordinary state of every page between an ingest and a
/// drain, teaching a reader to skip the one line that reports an irreversible one.
pub fn splice_preserved_sections<'a, I>(content: String, sections: I) -> Option<Spliced>
where
    I: IntoIterator<Item = (&'a str, &'a SectionDecision)>,
{
    let mut out = content;
    let mut candidates = Vec::new();
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
        } else if decision.discarding.is_some() {
            candidates.push(heading);
        }
    }
    // After every splice, so a section is judged on the page as it will be written. A heading
    // the render does not carry at all is a loss too: the body goes with the section.
    let discarded = candidates
        .into_iter()
        .filter(|heading| section_body(&out, heading).is_none_or(|body| body.trim().is_empty()))
        .map(str::to_owned)
        .collect();
    Some(Spliced {
        content: out,
        discarded,
    })
}

/// A page ready to write, and what writing it costs.
pub struct Spliced {
    pub content: String,
    /// Headings whose existing body this render replaces with an empty one.
    pub discarded: Vec<String>,
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
            discarding: None,
        }
    }

    fn uncached() -> SectionDecision {
        SectionDecision {
            hash: "h".into(),
            cached: false,
            preserved_body: None,
            discarding: None,
        }
    }

    fn identity(name: &str, slug: &str) -> crate::concept_draft::ConceptIdentity {
        crate::concept_draft::ConceptIdentity {
            name: name.into(),
            slug: slug.into(),
        }
    }

    fn accumulated(
        cited: Option<&str>,
        extracted: Vec<crate::concept_draft::ConceptIdentity>,
    ) -> Vec<(String, String)> {
        accumulate_concepts(
            cited,
            extracted,
            "daily/ai-news/2026-07-15.md",
            &VaultDirs::default(),
        )
        .into_iter()
        .map(|c| (c.name, c.slug))
        .collect()
    }

    /// The defect this exists to prevent: an extraction reporting a different set than the
    /// one before it silently un-cites every concept page the earlier one created, leaving
    /// pages that assert knowledge with no evidence and no record of what they lost.
    #[test]
    fn a_later_extraction_never_un_cites_what_an_earlier_one_created() {
        let cited = "\
- [Lethal Trifecta](../../wiki/concepts/lethal-trifecta.md)
- [AB-MCTS](../../wiki/concepts/ab-mcts.md)
";
        assert_eq!(
            accumulated(Some(cited), vec![identity("Wero", "wero")]),
            vec![
                ("Lethal Trifecta".to_string(), "lethal-trifecta".to_string()),
                ("AB-MCTS".to_string(), "ab-mcts".to_string()),
                ("Wero".to_string(), "wero".to_string()),
            ],
            "the newest extraction adds to the page's citations; it never replaces them"
        );
    }

    /// A citation whose destination is not the canonical spelling still addresses the same
    /// PAGE — that is how the graph counts it — so re-reporting it must not add a second
    /// link beside it. Accumulation makes any such double permanent.
    #[test]
    fn a_non_canonical_destination_is_the_same_citation_as_its_canonical_form() {
        let cited = "- [Bad Name](../../wiki/concepts/Bad_Name.md)\n";
        assert_eq!(
            accumulated(Some(cited), vec![identity("Bad Name", "bad-name")]),
            vec![("Bad Name".to_string(), "Bad_Name".to_string())],
            "one citation, kept at the address the page already carries"
        );
    }

    /// A destination whose stem has no addressable id cannot be reconciled with anything, so
    /// it is dropped — and said out loud, because a silently dropped citation and a
    /// deduplicated one are the same absence in the result and would be the same silence in
    /// the log.
    #[test]
    fn a_citation_with_no_addressable_id_is_dropped_and_the_rest_survive() {
        let cited = "\
- [Punctuation](../../wiki/concepts/---.md)
- [Real](../../wiki/concepts/real.md)
";
        assert_eq!(
            accumulated(Some(cited), vec![]),
            vec![("Real".to_string(), "real".to_string())]
        );
    }

    /// Re-reporting what a page already cites must reproduce the page, not double it — the
    /// common case, since a re-extraction of grown input names most of the same concepts.
    #[test]
    fn re_reporting_a_cited_concept_leaves_the_page_unchanged() {
        let cited = "- [AB-MCTS](../../wiki/concepts/ab-mcts.md)\n";
        assert_eq!(
            accumulated(Some(cited), vec![identity("AB-MCTS", "ab-mcts")]),
            vec![("AB-MCTS".to_string(), "ab-mcts".to_string())]
        );
        // The carried spelling wins over the newest one: the citation already on the page
        // is the one `backlinks-sync` has counted, so re-rendering must not churn it.
        assert_eq!(
            accumulated(Some(cited), vec![identity("AB MCTS", "ab-mcts")]),
            vec![("AB-MCTS".to_string(), "ab-mcts".to_string())]
        );
    }

    /// Only what the graph counts as a citation is carried. Anything else in the section is
    /// not this function's to re-render — carrying it as a concept would rewrite its
    /// destination into the concepts directory and invent a citation that never existed.
    #[test]
    fn only_a_link_the_graph_counts_as_a_citation_is_carried() {
        let cited = "\
A note with [a daily page](../team-slack/2026-07-15.md) and
[an attachment](../../wiki/concepts/a.pdf) and [the wiki index](../../wiki/index.md)
and [an external](https://example.com/wiki/concepts/x.md) and `[code](../../wiki/concepts/c.md)`
and [a real one](../../wiki/concepts/nl2sql.md)
";
        assert_eq!(
            accumulated(Some(cited), vec![]),
            vec![("a real one".to_string(), "nl2sql".to_string())]
        );
    }

    #[test]
    fn a_page_with_no_section_yet_carries_only_the_extraction() {
        assert_eq!(
            accumulated(None, vec![identity("Wero", "wero")]),
            vec![("Wero".to_string(), "wero".to_string())]
        );
        assert_eq!(accumulated(Some(""), vec![]), vec![]);
    }

    /// `concept_links` writes the citation and this reads it back, so whatever address form
    /// it produces must recover the same identity — otherwise the second render of a page
    /// carries a citation the first one wrote as a stranger, and duplicates it.
    #[test]
    fn every_address_concept_links_writes_reads_back_as_the_same_identity() {
        for (name, slug) in [
            ("에이전트 하니스", "에이전트-하니스"),
            ("RAG [retrieval]", "rag-retrieval"),
            ("Claude 3.5", "claude-3-5"),
        ] {
            let extracted = vec![identity(name, slug)];
            let cited = concept_links(
                &extracted,
                "daily/ai-news/2026-07-15.md",
                &VaultDirs::default(),
            )
            .join("\n");
            assert_eq!(
                accumulated(Some(&cited), vec![identity(name, slug)]),
                vec![(name.to_string(), slug.to_string())],
                "reading back {cited}"
            );
        }
    }

    /// A citation is a finished LINE, bullet included, because two writers put it on a page:
    /// the plan render through a template and `lore queue apply` writing the body directly.
    /// When the bullet lived in the templates instead, apply wrote bare links — so a page
    /// flipped between a list and a run of paragraphs every time an ingest and a drain took
    /// turns, and neither writer was wrong on its own.
    #[test]
    fn a_citation_is_a_finished_line_so_both_writers_produce_the_same_markup() {
        let lines = concept_links(
            &[identity("Wero", "wero"), identity("uv", "uv")],
            "daily/ai-news/2026-07-15.md",
            &VaultDirs::default(),
        );
        assert_eq!(
            lines,
            vec![
                "- [Wero](../../wiki/concepts/wero.md)",
                "- [uv](../../wiki/concepts/uv.md)",
            ]
        );
        // And the finished line still reads back as the identity it names, so accumulating
        // over a page written by either writer recovers the same citation.
        assert_eq!(
            accumulated(Some(&lines.join("\n")), vec![]),
            vec![
                ("Wero".to_string(), "wero".to_string()),
                ("uv".to_string(), "uv".to_string()),
            ]
        );
    }

    #[test]
    fn splice_writes_cached_bodies_and_leaves_uncached_sections_for_the_queue() {
        let fresh = "# Page\n\n## Summary\n\n## Concepts\n\n".to_string();
        let summary = cached("A preserved summary body.");
        let concepts = uncached();
        let out =
            splice_preserved_sections(fresh, [("Summary", &summary), ("Concepts", &concepts)])
                .expect("every cached heading exists in the fresh render");
        let content = &out.content;
        assert!(
            content.contains("A preserved summary body."),
            "cached body must be written over the empty section:\n{content}"
        );
        assert_eq!(
            section_body(content, "Concepts").map(str::trim),
            Some(""),
            "an uncached section must stay empty for the queue processor to fill:\n{content}"
        );
        assert!(
            out.discarded.is_empty(),
            "neither section held a body this render replaces"
        );
    }

    /// Only a section that HELD something is reported, and only alongside the content that
    /// replaces it — the report and the write are one return value, so a splice that refuses
    /// cannot leave a loss announced for a page nobody writes.
    #[test]
    fn splice_names_the_sections_whose_body_it_replaces_with_an_empty_one() {
        let fresh = "# Page\n\n## Summary\n\n## Concepts\n\n".to_string();
        let mut summary = uncached();
        summary.discarding = Some("An answer nothing recorded.".into());
        let concepts = uncached();
        let out =
            splice_preserved_sections(fresh, [("Summary", &summary), ("Concepts", &concepts)])
                .expect("nothing cached, so nothing can fail to splice");
        assert_eq!(out.discarded, vec!["Summary".to_string()]);
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
