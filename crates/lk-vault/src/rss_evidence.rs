//! Explicit RSS concept selections, independently guarded from daily interpretation.
use std::{collections::BTreeMap, path::Path};

use crate::{VaultError, original_io::read_bytes};
use lk_core::{
    concept::Citation,
    rss_evidence::{
        CONCEPT_EVIDENCE_FIELD, ConceptEvidence, RSS_EVIDENCE_SCHEMA, RssEventSelection,
        RssEventSelector, bindings,
    },
    rss_original::{
        MAX_RSS_EVENT_BYTES, RSS_ORIGINAL_FIELD, RSS_ORIGINAL_STORE, RssEventPayload,
        RssOriginalManifest, RssOriginalReference, verify_payload,
    },
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RssEvidencePreview {
    pub events: Vec<RssEventSelection>,
    pub total: usize,
    pub truncated: bool,
}

/// A bounded projection on the generated link itself, not every co-occurring link.
pub fn rss_link_previews(
    root: &Path,
    origin: &Path,
    raw: &str,
) -> Result<BTreeMap<usize, RssEvidencePreview>, VaultError> {
    let page = crate::parse_page(raw).map_err(err)?;
    let declarations = bindings(&page).map_err(err)?;
    let mut result = BTreeMap::new();
    if declarations.is_empty() {
        return Ok(result);
    }
    let section = crate::resolve_section(raw, |s| s.related_concepts)
        .ok_or_else(|| err("missing evidence section"))?;
    let start = section.body.as_ptr() as usize - raw.as_ptr() as usize;
    let mut selections: BTreeMap<String, Vec<RssEventSelection>> = BTreeMap::new();
    for binding in declarations {
        selections
            .entry(binding.concept)
            .or_default()
            .extend(binding.events);
    }
    for events in selections.values_mut() {
        events.sort();
        events.dedup();
    }
    let mut offsets = Vec::new();
    let mut offset = 0;
    for line in lk_core::markdown::lines_with_endings(raw) {
        offsets.push((offset, line.trim_end_matches(['\r', '\n'])));
        offset += line.len();
    }
    for occurrence in page_link_occurrences(raw)? {
        let (offset, line) = offsets[occurrence.line - 1];
        if offset < start
            || offset + line.len() > start + section.body.len()
            || !is_generated_link(line, &occurrence.link)
        {
            continue;
        }
        let Some(target) = lk_core::link::resolve_dest(origin, &occurrence.link.dest) else {
            continue;
        };
        let mut events = Vec::new();
        for (declared, selected) in &selections {
            if is_same_concept_address(root, declared, &target)? {
                events.extend(selected.iter().cloned());
            }
        }
        if events.is_empty() {
            continue;
        }
        events.sort();
        events.dedup();
        let total = events.len();
        result.insert(
            occurrence.line,
            RssEvidencePreview {
                events: events.iter().take(20).cloned().collect(),
                total,
                truncated: total > 20,
            },
        );
    }
    Ok(result)
}

/// Preserve physical page lines while treating only the original Markdown body as links.
fn page_link_occurrences(
    raw: &str,
) -> Result<Vec<lk_core::link::PageLinkOccurrence<'_>>, VaultError> {
    let offset = lk_core::frontmatter::body_offset(raw)
        .ok_or_else(|| err("RSS evidence page has unclosed frontmatter"))?;
    let lines = lk_core::markdown::lines_with_endings(&raw[..offset]).count();
    let mut occurrences = lk_core::link::extract_page_link_occurrences(&raw[offset..]);
    for occurrence in &mut occurrences {
        occurrence.line += lines;
    }
    Ok(occurrences)
}

fn is_generated_link(line: &str, link: &lk_core::link::PageLink) -> bool {
    line == format!(
        "- {}",
        lk_core::link::md_link(&link.text, &lk_core::link::encode_dest(&link.dest))
    )
}

fn is_same_concept_address(
    root: &Path,
    declared: &str,
    concept: &Path,
) -> Result<bool, VaultError> {
    Ok(Path::new(declared) == concept
        || crate::same_existing_entry(root, Path::new(declared), concept)?)
}

fn has_selected_membership(
    root: &Path,
    map: &BTreeMap<String, Vec<Citation>>,
    concept: &Path,
    citation: &Citation,
) -> Result<bool, VaultError> {
    for (declared, set) in map {
        if set.contains(citation) && is_same_concept_address(root, declared, concept)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn find_binding(
    root: &Path,
    page: &crate::VaultPage,
    concept: &Path,
    selection: &RssEventSelection,
) -> Result<ConceptEvidence, VaultError> {
    for binding in bindings(page).map_err(err)? {
        if binding.events.contains(selection)
            && is_same_concept_address(root, &binding.concept, concept)?
        {
            return Ok(binding);
        }
    }
    Err(err("RSS selection membership changed"))
}

fn err(message: impl std::fmt::Display) -> VaultError {
    VaultError::Original(message.to_string())
}

pub fn load_rss_manifest(
    root: &Path,
    reference: &RssOriginalReference,
) -> Result<RssOriginalManifest, VaultError> {
    crate::rss_original::read_manifest(root, reference)
}

fn scope(root: &Path, raw: &str) -> Result<(crate::VaultPage, RssOriginalManifest), VaultError> {
    let page = crate::parse_page(raw).map_err(err)?;
    if page.frontmatter.get("type").and_then(|v| v.as_str()) != Some("daily") {
        return Err(err("RSS evidence requires a daily page"));
    }
    let reference: RssOriginalReference = serde_json::from_value(
        page.frontmatter
            .get("source_record")
            .and_then(|v| v.get(RSS_ORIGINAL_FIELD))
            .cloned()
            .ok_or_else(|| err("RSS evidence requires a bound original"))?,
    )
    .map_err(err)?;
    let manifest = load_rss_manifest(root, &reference)?;
    if page.frontmatter.get("source").and_then(|v| v.as_str()) != Some(manifest.source_id.as_str())
        || page.frontmatter.get("created").and_then(|v| v.as_str())
            != Some(manifest.date.to_string().as_str())
    {
        return Err(err("RSS evidence source/date differs from bound original"));
    }
    Ok((page, manifest))
}

pub fn load_rss_selected_payload(
    root: &Path,
    manifest: &RssOriginalManifest,
    selection: &RssEventSelection,
) -> Result<RssEventPayload, VaultError> {
    manifest.validate().map_err(err)?;
    let entry = manifest
        .events
        .iter()
        .find(|e| e.event_id == selection.event_id && e.revision == selection.revision)
        .ok_or_else(|| err("RSS selection not in provided manifest"))?;
    let relative = format!("{RSS_ORIGINAL_STORE}/events/{}.json", entry.revision);
    let path =
        crate::original_io::store_path(root, Path::new(&relative), RSS_ORIGINAL_STORE, false)?;
    let bytes = read_bytes(&path, MAX_RSS_EVENT_BYTES)?;
    verify_payload(manifest, entry, &bytes).map_err(err)
}

/// All bindings must have real standalone generated links. Other occurrences keep page evidence.
pub fn rss_concept_citations(
    root: &Path,
    origin: &Path,
    raw: &str,
) -> Result<BTreeMap<String, Vec<Citation>>, VaultError> {
    rss_concept_citations_mode(root, origin, raw, true)
}

fn rss_concept_citations_mode(
    root: &Path,
    origin: &Path,
    raw: &str,
    payloads: bool,
) -> Result<BTreeMap<String, Vec<Citation>>, VaultError> {
    let parsed = crate::parse_page(raw).map_err(err)?;
    let declarations = bindings(&parsed).map_err(err)?;
    if declarations.is_empty() {
        return Ok(BTreeMap::new());
    }
    let (_, current) = scope(root, raw)?;
    let section = crate::resolve_section(raw, |s| s.related_concepts)
        .ok_or_else(|| err("RSS concept evidence lacks generated links section"))?;
    let start = section.body.as_ptr() as usize - raw.as_ptr() as usize;
    let end = start + section.body.len();
    let mut lines = Vec::new();
    let mut offset = 0;
    for line in lk_core::markdown::lines_with_endings(raw) {
        lines.push((offset, line.trim_end_matches(['\r', '\n'])));
        offset += line.len();
    }
    let mut occurrences: BTreeMap<std::path::PathBuf, (usize, bool)> = BTreeMap::new();
    for occurrence in page_link_occurrences(raw)? {
        let Some(target) = lk_core::link::resolve_dest(origin, &occurrence.link.dest) else {
            continue;
        };
        let (offset, line) = lines[occurrence.line - 1];
        let (generated, other) = occurrences.entry(target).or_default();
        if offset >= start
            && offset + line.len() <= end
            && is_generated_link(line, &occurrence.link)
        {
            *generated += 1;
        } else {
            *other = true;
        }
    }
    let mut manifests = BTreeMap::new();
    let mut verified: BTreeMap<
        String,
        (
            String,
            jiff::civil::Date,
            lk_core::rss_original::RssEventEntry,
        ),
    > = BTreeMap::new();
    let mut result: BTreeMap<String, Vec<Citation>> = BTreeMap::new();
    for binding in declarations {
        if !manifests.contains_key(&binding.manifest.revision) {
            manifests.insert(
                binding.manifest.revision.clone(),
                load_rss_manifest(root, &binding.manifest)?,
            );
        }
        let historical = &manifests[&binding.manifest.revision];
        if historical.source_id != current.source_id || historical.date != current.date {
            return Err(err("RSS evidence original source/date changed"));
        }
        let mut generated = 0;
        let mut other = false;
        for (target, (count, extra)) in &occurrences {
            if is_same_concept_address(root, &binding.concept, target)? {
                generated += *count;
                other |= *extra;
            }
        }
        if generated == 0 {
            return Err(err(
                "RSS evidence requires a standalone generated concept link",
            ));
        }
        let citations = result.entry(binding.concept.clone()).or_default();
        for selection in &binding.events {
            let entry = historical
                .events
                .iter()
                .find(|e| e.event_id == selection.event_id && e.revision == selection.revision)
                .ok_or_else(|| err("RSS selection absent from declared original"))?;
            if payloads {
                let proof = (historical.source_id.clone(), historical.date, entry.clone());
                if verified.get(&selection.revision) != Some(&proof) {
                    load_rss_selected_payload(root, historical, selection)?;
                    verified.insert(selection.revision.clone(), proof);
                }
            }
            citations.push(Citation {
                path: origin.to_string_lossy().replace('\\', "/"),
                revision: selection.revision.clone(),
                rss_event: Some(RssEventSelector {
                    schema: RSS_EVIDENCE_SCHEMA.into(),
                    event_id: selection.event_id.clone(),
                }),
            });
        }
        if other {
            citations.push(Citation::new(origin, &parsed));
        }
    }
    for citations in result.values_mut() {
        citations.sort();
        citations.dedup();
    }
    Ok(result)
}

/// Verify the exact selected membership, not the origin's unrelated prose revision.
pub fn validate_rss_citation(
    root: &Path,
    concept: &Path,
    citation: &Citation,
) -> Result<(), VaultError> {
    if !has_rss_citation(root, concept, citation)? {
        return Err(err("RSS concept evidence membership changed"));
    }
    Ok(())
}

pub fn has_rss_citation(
    root: &Path,
    concept: &Path,
    citation: &Citation,
) -> Result<bool, VaultError> {
    let selector = citation
        .rss_event
        .as_ref()
        .ok_or_else(|| err("expected RSS citation"))?;
    selector.validate(&citation.revision).map_err(err)?;
    let origin = Path::new(&citation.path);
    let raw = std::fs::read_to_string(crate::page_path(root, origin)?)?;
    let map = rss_concept_citations_mode(root, origin, &raw, false)?;
    let found = has_selected_membership(root, &map, concept, citation)?;
    if found {
        let page = crate::parse_page(&raw).map_err(err)?;
        let selection = RssEventSelection {
            event_id: selector.event_id.clone(),
            revision: citation.revision.clone(),
        };
        let declaration = find_binding(root, &page, concept, &selection)?;
        load_rss_selected_payload(
            root,
            &load_rss_manifest(root, &declaration.manifest)?,
            &selection,
        )?;
    }
    Ok(found)
}

/// Bounded read of a declared historical selection, even after the current manifest moves.
pub fn inspect_rss_citation(
    root: &Path,
    concept: &Path,
    citation: &Citation,
    offset: usize,
    limit: usize,
) -> Result<crate::rss_original::RssOriginalText, VaultError> {
    if !(1..=65536).contains(&limit) {
        return Err(err("RSS text-limit must be 1..65536"));
    }
    validate_rss_citation(root, concept, citation)?;
    let raw = std::fs::read_to_string(crate::page_path(root, Path::new(&citation.path))?)?;
    let map = rss_concept_citations_mode(root, Path::new(&citation.path), &raw, false)?;
    if !has_selected_membership(root, &map, concept, citation)? {
        return Err(err("RSS membership changed during read"));
    }
    let page = crate::parse_page(&raw).map_err(err)?;
    let event = citation.rss_event.as_ref().expect("validated selector");
    let selection = RssEventSelection {
        event_id: event.event_id.clone(),
        revision: citation.revision.clone(),
    };
    let binding = find_binding(root, &page, concept, &selection)?;
    let manifest = load_rss_manifest(root, &binding.manifest)?;
    let payload = load_rss_selected_payload(root, &manifest, &selection)?;
    let normalized = format!("{}\n\n{}", payload.title, payload.body);
    let total = normalized.chars().count();
    let text: String = normalized.chars().skip(offset).take(limit).collect();
    let end = offset.saturating_add(text.chars().count());
    if std::fs::read_to_string(crate::page_path(root, Path::new(&citation.path))?)? != raw {
        return Err(err("RSS membership changed during read"));
    }
    Ok(crate::rss_original::RssOriginalText {
        text_format: "title_then_blank_line_then_body",
        payload_path: Some(format!(
            "{RSS_ORIGINAL_STORE}/events/{}.json",
            selection.revision
        )),
        event_id: selection.event_id,
        revision: selection.revision,
        event_timestamp: payload.event_timestamp,
        captured_at: payload.captured_at,
        offset,
        limit,
        total_characters: total,
        text,
        next_offset: (end < total).then_some(end),
    })
}

/// New selections must belong to the task's provided manifest; old declarations remain pinned.
pub fn accumulate_rss_evidence(
    root: &Path,
    origin: &Path,
    raw: &str,
    manifest: Option<&RssOriginalManifest>,
    selections: &[(String, Vec<RssEventSelection>)],
) -> Result<String, VaultError> {
    let page = crate::parse_page(raw).map_err(err)?;
    let mut declarations = bindings(&page).map_err(err)?;
    for (concept, events) in selections.iter().filter(|(_, events)| !events.is_empty()) {
        let provided =
            manifest.ok_or_else(|| err("RSS selections require versioned task manifest"))?;
        let (_, current) = scope(root, raw)?;
        if current.reference().map_err(err)? != provided.reference().map_err(err)? {
            return Err(err("RSS extraction manifest changed"));
        }
        for event in events {
            load_rss_selected_payload(root, provided, event)?;
        }
        let mut events = events.clone();
        events.sort();
        events.dedup();
        for existing in &declarations {
            if is_same_concept_address(root, &existing.concept, Path::new(concept))? {
                events.retain(|e| !existing.events.contains(e));
            }
        }
        if !events.is_empty() {
            declarations.push(ConceptEvidence {
                schema: RSS_EVIDENCE_SCHEMA.into(),
                concept: concept.clone(),
                manifest: provided.reference().map_err(err)?,
                events,
            });
        }
    }
    if declarations.is_empty() {
        return Ok(raw.into());
    }
    let updated = write_bindings(raw, &declarations)?;
    rss_concept_citations(root, origin, &updated)?;
    Ok(updated)
}

pub fn write_bindings(raw: &str, declarations: &[ConceptEvidence]) -> Result<String, VaultError> {
    let mut declarations = declarations.to_vec();
    declarations.sort_by(|a, b| {
        (&a.concept, &a.manifest.revision).cmp(&(&b.concept, &b.manifest.revision))
    });
    let mut seen = std::collections::BTreeSet::new();
    for binding in &mut declarations {
        binding.events.sort();
        binding.events.dedup();
        binding
            .events
            .retain(|event| seen.insert((binding.concept.clone(), event.clone())));
    }
    declarations.retain(|b| !b.events.is_empty());
    crate::set_frontmatter_field(
        raw,
        CONCEPT_EVIDENCE_FIELD,
        &serde_json::to_string(&declarations).map_err(err)?,
    )
    .ok_or_else(|| err("missing concept evidence frontmatter"))
}

/// The same filesystem identity as Markdown link rewrites, never concept-name inference.
pub fn repoint_rss_evidence(
    root: &Path,
    raw: &str,
    replacements: &[(std::path::PathBuf, std::path::PathBuf)],
) -> Result<String, VaultError> {
    let page = crate::parse_page(raw).map_err(err)?;
    let mut declarations = bindings(&page).map_err(err)?;
    if declarations.is_empty() {
        return Ok(raw.into());
    }
    for binding in &mut declarations {
        for (from, into) in replacements {
            if crate::same_existing_entry(root, Path::new(&binding.concept), from)? {
                binding.concept = into.to_string_lossy().replace('\\', "/");
                break;
            }
        }
    }
    write_bindings(raw, &declarations)
}

pub fn guard_rss_evidence(
    plan: &mut crate::MutationPlan,
    root: &Path,
    origin: &Path,
    raw: &str,
) -> Result<(), VaultError> {
    rss_concept_citations(root, origin, raw)?;
    let page = crate::parse_page(raw).map_err(err)?;
    let declarations = bindings(&page).map_err(err)?;
    if declarations.is_empty() {
        return Ok(());
    }
    let (_, current) = scope(root, raw)?;
    let mut dependencies = BTreeMap::new();
    let reference = current.reference().map_err(err)?;
    dependencies.insert(
        reference.manifest_path,
        lk_core::rss_original::canonical_bytes(&current).map_err(err)?,
    );
    for binding in declarations {
        if !dependencies.contains_key(&binding.manifest.manifest_path) {
            let manifest = load_rss_manifest(root, &binding.manifest)?;
            dependencies.insert(
                binding.manifest.manifest_path.clone(),
                lk_core::rss_original::canonical_bytes(&manifest).map_err(err)?,
            );
        }
        let manifest: RssOriginalManifest =
            serde_json::from_slice(&dependencies[&binding.manifest.manifest_path]).map_err(err)?;
        for selection in binding.events {
            let relative = format!("{RSS_ORIGINAL_STORE}/events/{}.json", selection.revision);
            if let std::collections::btree_map::Entry::Vacant(entry) = dependencies.entry(relative)
            {
                let payload = load_rss_selected_payload(root, &manifest, &selection)?;
                entry.insert(lk_core::rss_original::canonical_bytes(&payload).map_err(err)?);
            }
        }
    }
    for (relative, bytes) in dependencies {
        plan.require_unchanged(relative, String::from_utf8(bytes).map_err(err)?);
    }
    Ok(())
}

/// Pin the current semantic membership and immutable dependencies in a publication journal.
pub fn guard_rss_citation(
    plan: &mut crate::MutationPlan,
    root: &Path,
    concept: &Path,
    citation: &Citation,
) -> Result<(), VaultError> {
    validate_rss_citation(root, concept, citation)?;
    let origin = Path::new(&citation.path);
    let raw = std::fs::read_to_string(crate::page_path(root, origin)?)?;
    let map = rss_concept_citations_mode(root, origin, &raw, false)?;
    if !has_selected_membership(root, &map, concept, citation)? {
        return Err(err("RSS membership changed before publication"));
    }
    let page = crate::parse_page(&raw).map_err(err)?;
    let selector = citation.rss_event.as_ref().expect("validated selector");
    let selection = RssEventSelection {
        event_id: selector.event_id.clone(),
        revision: citation.revision.clone(),
    };
    let binding = find_binding(root, &page, concept, &selection)?;
    let manifest = load_rss_manifest(root, &binding.manifest)?;
    let payload = load_rss_selected_payload(root, &manifest, &selection)?;
    let current: RssOriginalReference = serde_json::from_value(
        page.frontmatter
            .get("source_record")
            .and_then(|v| v.get(RSS_ORIGINAL_FIELD))
            .cloned()
            .ok_or_else(|| err("missing current manifest"))?,
    )
    .map_err(err)?;
    for reference in [&current, &binding.manifest] {
        let manifest = load_rss_manifest(root, reference)?;
        plan.require_unchanged(
            &reference.manifest_path,
            String::from_utf8(lk_core::rss_original::canonical_bytes(&manifest).map_err(err)?)
                .map_err(err)?,
        );
    }
    let relative = format!("{RSS_ORIGINAL_STORE}/events/{}.json", selection.revision);
    plan.require_unchanged(
        relative,
        String::from_utf8(lk_core::rss_original::canonical_bytes(&payload).map_err(err)?)
            .map_err(err)?,
    );
    plan.require_unchanged(origin, raw);
    Ok(())
}

#[derive(serde::Serialize)]
pub struct RssEvidenceInventory {
    pub origin: String,
    pub concept: String,
    pub revision: String,
    pub events: Vec<RssEventSelection>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
}

pub fn inspect_rss_evidence_inventory(
    root: &Path,
    origin: &Path,
    concept: &Path,
    revision: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<RssEvidenceInventory, VaultError> {
    if !(1..=100).contains(&limit) {
        return Err(err("RSS evidence limit must be 1..100"));
    }
    let raw = std::fs::read_to_string(crate::page_path(root, origin)?)?;
    let map = rss_concept_citations_mode(root, origin, &raw, false)?;
    let mut citations = Vec::new();
    for (declared, set) in &map {
        if is_same_concept_address(root, declared, concept)? {
            citations.extend(set);
        }
    }
    if citations.is_empty() {
        return Err(err("no explicit RSS evidence for concept"));
    }
    let mut selections = citations
        .into_iter()
        .filter_map(|c| {
            c.rss_event.as_ref().map(|e| RssEventSelection {
                event_id: e.event_id.clone(),
                revision: c.revision.clone(),
            })
        })
        .collect::<Vec<_>>();
    selections.sort();
    selections.dedup();
    let identity = lk_core::rss_original::digest(
        &lk_core::rss_original::canonical_bytes(&(origin, concept, &selections)).map_err(err)?,
    );
    if revision.is_some_and(|r| r != identity) {
        return Err(err("RSS evidence membership changed"));
    }
    let total = selections.len();
    let events = selections
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let end = offset.saturating_add(events.len());
    if std::fs::read_to_string(crate::page_path(root, origin)?)? != raw {
        return Err(err("RSS evidence changed during discovery"));
    }
    Ok(RssEvidenceInventory {
        origin: origin.to_string_lossy().into(),
        concept: concept.to_string_lossy().into(),
        revision: identity,
        events,
        total,
        offset,
        limit,
        next_offset: (end < total).then_some(end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_body_links_preserve_selected_evidence_and_physical_lines_after_yaml_fences() {
        let root = tempfile::tempdir().unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let event: lk_core::event::Event = serde_json::from_value(serde_json::json!({
            "id":"rss:2026-10-18:0000000000000001", "source_id":"rss", "source_type":"rss",
            "timestamp":"2026-10-18T00:00:00Z", "date":"2026-10-18", "title":"Title", "body":"Original evidence.",
            "url":"https://example.com", "author":null, "labels":[], "category":null,
            "performance_category":null, "is_self":false, "is_personal":false, "metadata":{}
        })).unwrap();
        let capture = lk_core::rss_original::RssOriginalCapture::from_events(&[&event]).unwrap();
        let reference = crate::publish_rss_original(&session, &capture).unwrap();
        let selected = &capture.manifest.events[0];
        let bindings = serde_json::json!([{
            "schema":RSS_EVIDENCE_SCHEMA, "concept":"wiki/concepts/topic.md", "manifest":reference,
            "events":[{"event_id":selected.event_id,"revision":selected.revision}]
        }]);
        let prefix = format!(
            "\u{feff}---\r\ntype: daily\r\nsource: rss\r\ncreated: 2026-10-18\r\nsource_record: {{rss_original: {}}}\r\nconcept_evidence: {bindings}\r\nliteral: |\r\n  ```\r\n  [Metadata](../wiki/concepts/topic.md)\r\n---\r\n\r\n",
            serde_json::to_string(&reference).unwrap()
        );
        let origin = Path::new("daily/rss.md");
        for ending in ["\n", "\r\n", "\r"] {
            let raw = format!(
                "{prefix}{}",
                "<div>\n[HTML](../wiki/concepts/topic.md)\n</div>\n\n<!--\n```\n-->\n\n## Related Concepts\n- [Topic](../wiki/concepts/topic.md)\n".replace('\n', ending)
            );
            for authored in [false, true] {
                let raw = if authored {
                    format!(
                        "{raw}{ending}An authored [Topic](../wiki/concepts/topic.md) reference.{ending}"
                    )
                } else {
                    raw.clone()
                };
                let previews = rss_link_previews(root.path(), origin, &raw).unwrap();
                assert_eq!(previews.len(), 1);
                assert_eq!(previews[&21].total, 1);
                assert_eq!(previews[&21].events[0].revision, selected.revision);
                let citations = rss_concept_citations(root.path(), origin, &raw).unwrap();
                let citations = &citations["wiki/concepts/topic.md"];
                assert_eq!(citations.len(), 1 + usize::from(authored));
                assert_eq!(
                    citations
                        .iter()
                        .filter(|citation| citation.rss_event.is_some())
                        .count(),
                    1
                );
                let mut plan = crate::MutationPlan::default();
                guard_rss_evidence(&mut plan, root.path(), origin, &raw).unwrap();
                session.apply(plan).unwrap();
            }
        }
    }
}
