use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use lk_core::concept::{Citation, file_revision, identity_key, source_revision};
use lk_core::config::VaultDirs;
pub use lk_core::discovery::DiscoveryQuery;

use crate::{SearchOptions, VaultError, search};

pub const DISCOVERY_CAPTURE_POLICY: &str = "discovery-candidates-v1";
/// The bound covers the serialized card, excluding exact candidate identities.
pub const DISCOVERY_CARD_MAX_BYTES: usize = 2048;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryText {
    pub text: String,
    pub truncated: bool,
}

impl DiscoveryText {
    fn build(value: &str, budget: usize) -> Self {
        let mut bytes = 0;
        let mut end = 0;
        for (at, character) in value.char_indices() {
            let width = serde_json::to_string(&character)
                .expect("a Unicode scalar serializes")
                .len()
                - 2;
            if bytes + width > budget {
                break;
            }
            bytes += width;
            end = at + character.len_utf8();
        }
        Self {
            text: value[..end].into(),
            truncated: end < value.len(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryPreview {
    pub text: DiscoveryText,
    pub kind: crate::ExcerptKind,
    /// Accepted by a read pinned to the candidate's file_revision; HTML has no locator.
    pub section: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryMatch {
    pub query_index: usize,
    pub preview: DiscoveryPreview,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryHeading {
    pub section: usize,
    pub level: u8,
    pub title: DiscoveryText,
}

/// A bounded discovery projection, never a full-body read or a semantic definition.
/// Each match previews one visible block for its recipe, not all occurrences.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryCandidateCard {
    pub title: DiscoveryText,
    pub aliases: Vec<DiscoveryText>,
    pub aliases_total: usize,
    pub aliases_truncated: bool,
    pub format: Option<DiscoveryText>,
    pub knowledge_status: lk_core::frontmatter::KnowledgeStatus,
    pub opening: DiscoveryPreview,
    pub matches: Vec<DiscoveryMatch>,
    pub matches_total: usize,
    pub matches_truncated: bool,
    pub outline: Vec<DiscoveryHeading>,
    pub outline_total: usize,
    pub outline_truncated: bool,
    pub reviews: crate::ReviewCounts,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryCandidate {
    pub path: String,
    /// Exact canonical vault-relative owner, not a name or content-equivalence key.
    pub owner: String,
    pub file_revision: String,
    pub source_revision: String,
    pub query_indexes: Vec<usize>,
    pub card: DiscoveryCandidateCard,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiscoveryCapture {
    pub policy: String,
    pub revision: String,
    pub queries: Vec<DiscoveryQuery>,
    pub candidates: Vec<DiscoveryCandidate>,
    #[serde(skip)]
    guarded_pages: BTreeMap<PathBuf, Arc<String>>,
}

impl DiscoveryCapture {
    /// Full candidate bytes stay local for guards; cards do not replace those guards.
    /// Graph-owned review/evidence dependencies must be captured by the caller.
    pub fn guarded_pages(&self) -> &BTreeMap<PathBuf, Arc<String>> {
        &self.guarded_pages
    }
}

fn serialized<T: Serialize>(value: &T) -> Result<String, VaultError> {
    serde_json::to_string(value).map_err(|error| VaultError::Serialization(error.to_string()))
}

fn invalid(message: &str) -> VaultError {
    std::io::Error::other(message).into()
}

struct QueryPlan {
    identity: Option<String>,
    spellings: Vec<String>,
    terms: Vec<String>,
}

struct CapturePlan {
    queries: Vec<DiscoveryQuery>,
    plans: Vec<QueryPlan>,
    guarded_pages: BTreeMap<PathBuf, Arc<String>>,
    roster: BTreeMap<String, DiscoveryCandidate>,
}

/// Capture every lexical candidate, including retired knowledge, without ranking or
/// pagination. Each physical symlink owner occurs once; hard-link addresses stay distinct.
/// This observes current files rather than freezing external editors. Recompute the capture
/// before recording decisions/publication; callers own cooperating workflow/mutation leases.
pub fn capture_discovery(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
) -> Result<DiscoveryCapture, VaultError> {
    capture_discovery_inner(root, dirs, queries, None)
}

/// Share freshly read review reports within one discovery/dependency checkpoint.
/// Callers must still reread the complete scope after external execution.
pub fn capture_discovery_with_reviews(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
    reviews: &[crate::ReviewReport],
) -> Result<DiscoveryCapture, VaultError> {
    capture_discovery_inner(root, dirs, queries, Some(reviews))
}

fn capture_discovery_inner(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
    reports: Option<&[crate::ReviewReport]>,
) -> Result<DiscoveryCapture, VaultError> {
    Ok(capture_discovery_batch_inner(root, dirs, &[queries], reports)?.remove(0))
}

/// Capture task-local recipe sets in one fresh traversal. Output order matches the
/// input sets; queries, cards, revisions and guards remain independent per set.
/// Matched page bytes are shared immutably across captures. Callers own the fresh
/// pre-execution checkpoint, including supplied review reports, and recapture
/// before recording decisions or publication.
pub fn capture_discovery_batch(
    root: &Path,
    dirs: &VaultDirs,
    query_sets: &[Vec<DiscoveryQuery>],
    reviews: Option<&[crate::ReviewReport]>,
) -> Result<Vec<DiscoveryCapture>, VaultError> {
    let query_sets = query_sets.iter().map(Vec::as_slice).collect::<Vec<_>>();
    capture_discovery_batch_inner(root, dirs, &query_sets, reviews)
}

fn capture_discovery_batch_inner(
    root: &Path,
    dirs: &VaultDirs,
    query_sets: &[&[DiscoveryQuery]],
    reports: Option<&[crate::ReviewReport]>,
) -> Result<Vec<DiscoveryCapture>, VaultError> {
    let root = root.canonicalize()?;
    for query in query_sets.iter().flat_map(|queries| queries.iter()) {
        query.validate().map_err(std::io::Error::other)?;
    }
    let query_sets = query_sets
        .iter()
        .map(|queries| {
            queries
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let has_queries = query_sets.iter().any(|queries| !queries.is_empty());
    let mut vocabulary = Vec::new();
    if has_queries {
        search::visit_collections(
            &root,
            dirs,
            vec![lk_core::vault_path::concepts_dir(dirs)],
            |mut page, _| {
                page.page.body.clear();
                vocabulary.push(page);
                Ok(())
            },
        )?;
    }
    let mut captures = query_sets
        .into_iter()
        .map(|queries| {
            let plans = queries
                .iter()
                .map(|query| {
                    let identity = identity_key(&query.query);
                    QueryPlan {
                        spellings: search::spellings_of(identity.as_deref(), &vocabulary),
                        terms: search::query_terms(&query.query, query.phrase),
                        identity,
                    }
                })
                .collect();
            CapturePlan {
                queries,
                plans,
                guarded_pages: BTreeMap::new(),
                roster: BTreeMap::new(),
            }
        })
        .collect::<Vec<_>>();
    drop(vocabulary);
    let reviews = if !has_queries {
        BTreeMap::new()
    } else if let Some(reports) = reports {
        crate::review::review_previews_from_reports(&root, reports)?
    } else {
        crate::review::review_previews(&root, dirs)?
    };
    let mut guarded_pages = BTreeMap::<PathBuf, Arc<String>>::new();
    if has_queries {
        search::visit_pages(&root, dirs, false, |mut page, _| {
            let document = search::VisibleDocument::from_body(&page.page.body);
            page.projection = Some(search::MatchProjection::from_document(&document));
            let matches = captures
                .iter()
                .enumerate()
                .filter_map(|(capture_index, capture)| {
                    let query_indexes = capture
                        .queries
                        .iter()
                        .zip(&capture.plans)
                        .enumerate()
                        .filter_map(|(index, (query, plan))| {
                            let options = SearchOptions {
                                query: Some(&query.query),
                                phrase: query.phrase,
                                spacing: query.spacing,
                                knowledge_status: None,
                                ..Default::default()
                            };
                            search::match_query(
                                &page,
                                &options,
                                &plan.spellings,
                                &plan.terms,
                                plan.identity.as_deref(),
                            )
                            .is_some()
                            .then_some(index)
                        })
                        .collect::<BTreeSet<_>>();
                    (!query_indexes.is_empty()).then_some((capture_index, query_indexes))
                })
                .collect::<Vec<_>>();
            page.projection = None;
            if matches.is_empty() {
                return Ok(());
            }
            let full = crate::page_path(&root, &page.rel_path)?;
            let owner = full
                .canonicalize()?
                .strip_prefix(&root)
                .map_err(std::io::Error::other)?
                .to_path_buf();
            let raw = std::fs::read_to_string(&full)?;
            if file_revision(&raw) != page.file_revision {
                return Err(invalid("discovery candidate changed during capture"));
            }
            let raw = match guarded_pages.entry(owner.clone()) {
                std::collections::btree_map::Entry::Occupied(entry) => {
                    if entry.get().as_str() != raw {
                        return Err(invalid("discovery aliases changed during capture"));
                    }
                    Arc::clone(entry.get())
                }
                std::collections::btree_map::Entry::Vacant(entry) => {
                    Arc::clone(entry.insert(Arc::new(raw)))
                }
            };
            let owner_path = owner;
            let owner = owner_path
                .to_str()
                .ok_or_else(|| invalid("discovery owner address is not UTF-8"))?
                .to_owned();
            let path = page
                .rel_path
                .to_str()
                .ok_or_else(|| invalid("discovery candidate address is not UTF-8"))?
                .to_owned();
            let authored = source_revision(&page.page);
            let mut counts = crate::ReviewCounts::default();
            if let Some(previews) = reviews.get(&owner) {
                for review in previews {
                    counts.record(review.effective_state, 1);
                }
            }
            let outline = crate::document::outline(&page.page.body);
            for (capture_index, mut query_indexes) in matches {
                let capture = &mut captures[capture_index];
                capture
                    .guarded_pages
                    .insert(owner_path.clone(), Arc::clone(&raw));
                let mut path = path.clone();
                if let Some(previous) = capture.roster.get(&owner) {
                    if previous.file_revision != page.file_revision
                        || previous.source_revision != authored
                    {
                        return Err(invalid("discovery owner changed during capture"));
                    }
                    if path != owner && (previous.path == owner || previous.path < path) {
                        path.clone_from(&previous.path);
                    }
                    query_indexes.extend(previous.query_indexes.iter().copied());
                    if path == previous.path && query_indexes.len() == previous.query_indexes.len()
                    {
                        continue;
                    }
                }
                let query_indexes = query_indexes.into_iter().collect::<Vec<_>>();
                let card = build_card(
                    &page.page,
                    &document,
                    &outline,
                    &path,
                    &query_indexes,
                    capture,
                    counts.clone(),
                )?;
                capture.roster.insert(
                    owner.clone(),
                    DiscoveryCandidate {
                        path,
                        owner: owner.clone(),
                        file_revision: page.file_revision.clone(),
                        source_revision: authored.clone(),
                        query_indexes,
                        card,
                    },
                );
            }
            Ok(())
        })?;
    }
    captures
        .into_iter()
        .map(|capture| {
            let review_identity = capture
                .roster
                .keys()
                .map(|owner| {
                    (
                        owner.clone(),
                        reviews.get(owner).cloned().unwrap_or_default(),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let candidates = capture.roster.into_values().collect::<Vec<_>>();
            let revision = file_revision(&serialized(&(
                DISCOVERY_CAPTURE_POLICY,
                &root,
                dirs,
                &capture.queries,
                &candidates,
                &review_identity,
            ))?);
            Ok(DiscoveryCapture {
                policy: DISCOVERY_CAPTURE_POLICY.into(),
                revision,
                queries: capture.queries,
                candidates,
                guarded_pages: capture.guarded_pages,
            })
        })
        .collect()
}

fn build_card(
    page: &crate::VaultPage,
    document: &search::VisibleDocument,
    outline: &[crate::DocumentSection],
    path: &str,
    query_indexes: &[usize],
    capture: &CapturePlan,
    reviews: crate::ReviewCounts,
) -> Result<DiscoveryCandidateCard, VaultError> {
    let queries = &capture.queries;
    let plans = &capture.plans;
    let opening = document.opening.as_deref();
    let aliases = page
        .frontmatter
        .get("aliases")
        .and_then(serde_json::Value::as_array);
    let aliases = aliases
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    let matches = query_indexes
        .iter()
        .take(2)
        .map(|&query_index| {
            let plan = &plans[query_index];
            let selected =
                document.matching_block(&plan.terms, &plan.spellings, queries[query_index].spacing);
            let (text, kind, section) = match selected {
                Some((block, kind)) => {
                    let section = block.source.as_ref().and_then(|source| {
                        outline
                            .iter()
                            .rev()
                            .find(|section| {
                                section.start <= source.start && source.end <= section.end
                            })
                            .map(|section| section.id)
                    });
                    (block.text.as_str(), kind, section)
                }
                None => ("", crate::ExcerptKind::Unavailable, None),
            };
            DiscoveryMatch {
                query_index,
                preview: DiscoveryPreview {
                    text: DiscoveryText::build(text, 256),
                    kind,
                    section,
                },
            }
        })
        .collect::<Vec<_>>();
    let outline_total = outline.len().saturating_sub(1);
    let mut card = DiscoveryCandidateCard {
        title: DiscoveryText::build(
            page.frontmatter
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| {
                    Path::new(path)
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .unwrap_or("")
                }),
            128,
        ),
        aliases: aliases
            .iter()
            .take(2)
            .map(|value| DiscoveryText::build(value, 64))
            .collect(),
        aliases_total: aliases.len(),
        aliases_truncated: aliases.len() > 2,
        format: page
            .frontmatter
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|value| DiscoveryText::build(value, 32)),
        knowledge_status: page
            .frontmatter
            .knowledge_status()
            .map_err(VaultError::Frontmatter)?,
        opening: DiscoveryPreview {
            text: DiscoveryText::build(opening.unwrap_or_default(), 192),
            kind: if opening.is_some() {
                crate::ExcerptKind::Opening
            } else {
                crate::ExcerptKind::Unavailable
            },
            section: None,
        },
        matches_total: query_indexes.len(),
        matches_truncated: query_indexes.len() > matches.len(),
        matches,
        outline: outline
            .iter()
            .skip(1)
            .take(3)
            .map(|section| DiscoveryHeading {
                section: section.id,
                level: section.level,
                title: DiscoveryText::build(&section.title, 48),
            })
            .collect(),
        outline_total,
        outline_truncated: outline_total > 3,
        reviews,
    };
    while serialized(&card)?.len() > DISCOVERY_CARD_MAX_BYTES {
        if card.outline.pop().is_some() {
            card.outline_truncated = true;
        } else if card.aliases.pop().is_some() {
            card.aliases_truncated = true;
        } else if card.matches.len() > 1 {
            card.matches.pop();
            card.matches_truncated = true;
        } else {
            let mut texts = vec![&mut card.title, &mut card.opening.text];
            texts.extend(card.format.iter_mut());
            texts.extend(
                card.matches
                    .iter_mut()
                    .map(|preview| &mut preview.preview.text),
            );
            let longest = texts
                .into_iter()
                .max_by_key(|text| text.text.len())
                .expect("title and opening are present");
            if longest.text.is_empty() {
                return Err(invalid(
                    "discovery card metadata exceeds its serialized bound",
                ));
            }
            let budget = serialized(&longest.text)?.len().saturating_sub(2) / 2;
            *longest = DiscoveryText::build(&longest.text, budget);
            longest.truncated = true;
        }
    }
    Ok(card)
}

/// Complete lexical candidates, including retired knowledge. Matches are context,
/// never inferred citations, equivalence or contradiction judgments.
pub fn discover_queries(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
) -> Result<BTreeMap<DiscoveryQuery, Vec<Citation>>, VaultError> {
    for query in queries {
        query.validate().map_err(std::io::Error::other)?;
    }
    if queries.is_empty() {
        return Ok(BTreeMap::new());
    }
    if let Some(matches) = indexed_queries(root, dirs, queries)? {
        return Ok(matches);
    }
    let mut vocabulary = Vec::new();
    search::visit_collections(
        root,
        dirs,
        vec![lk_core::vault_path::concepts_dir(dirs)],
        |mut page, _| {
            page.page.body = String::new();
            page.page.original = None;
            vocabulary.push(page);
            Ok(())
        },
    )?;
    let plans = queries
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|query| {
            let identity = identity_key(&query.query);
            let spellings = search::spellings_of(identity.as_deref(), &vocabulary);
            let terms = search::query_terms(&query.query, query.phrase);
            (query, identity, spellings, terms)
        })
        .collect::<Vec<_>>();
    let mut result = BTreeMap::from_iter(
        plans
            .iter()
            .map(|(query, ..)| ((*query).clone(), Vec::new())),
    );
    search::visit_pages(root, dirs, false, |mut page, _| {
        page.projection = Some(page.match_projection().into_owned());
        let mut revision = None;
        for (query, identity, spellings, terms) in &plans {
            let options = SearchOptions {
                query: Some(&query.query),
                phrase: query.phrase,
                spacing: query.spacing,
                knowledge_status: None,
                ..Default::default()
            };
            if search::match_query(&page, &options, spellings, terms, identity.as_deref()).is_some()
            {
                let revision = revision.get_or_insert_with(|| source_revision(&page.page));
                result
                    .get_mut(*query)
                    .expect("selected query")
                    .push(Citation::at_revision(&page.rel_path, revision.clone()));
            }
        }
        Ok(())
    })?;
    for matches in result.values_mut() {
        matches.sort();
        matches.dedup();
    }
    Ok(result)
}

fn indexed_queries(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
) -> Result<Option<BTreeMap<DiscoveryQuery, Vec<Citation>>>, VaultError> {
    let snapshot = match crate::search_index::inspect_search_snapshot(root, dirs) {
        Ok(snapshot) => snapshot,
        Err(_) => return Ok(None),
    };
    let path = crate::page_path(root, Path::new(crate::search_index::INDEX_PATH))?;
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    crate::search_index::verify_snapshot(&transaction, root, dirs, &snapshot.snapshot)?;
    let mut statement = transaction.prepare(
        "SELECT path, json_extract(metadata, '$.file_revision') FROM pages WHERE knowledge = 1",
    )?;
    let mut captured = statement
        .query_map([], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                row.get::<_, String>(1)?,
            ))
        })?
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mut revisions = BTreeMap::new();
    let mut unchanged = true;
    search::visit_pages(root, dirs, false, |page, _| {
        unchanged &= captured.remove(&page.rel_path).as_deref() == Some(&page.file_revision);
        revisions.insert(page.rel_path, source_revision(&page.page));
        Ok(())
    })?;
    if !unchanged || !captured.is_empty() {
        return Ok(None);
    }
    let mut result = BTreeMap::new();
    for query in queries.iter().collect::<BTreeSet<_>>() {
        let options = SearchOptions {
            query: Some(&query.query),
            phrase: query.phrase,
            spacing: query.spacing,
            knowledge_status: None,
            ..Default::default()
        };
        let mut matches = Vec::new();
        crate::search_index::visit_query_candidates(
            &transaction,
            &options,
            |page, spellings, terms, identity| {
                if search::match_query(&page, &options, spellings, terms, identity).is_some() {
                    let revision = revisions.get(&page.rel_path).ok_or_else(|| {
                        std::io::Error::other(
                            "indexed discovery candidate is outside the verified scope",
                        )
                    })?;
                    matches.push(Citation::at_revision(&page.rel_path, revision.clone()));
                }
                Ok(())
            },
        )?;
        matches.sort();
        matches.dedup();
        result.insert(query.clone(), matches);
    }
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, body: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn assert_batch_matches_individual(
        root: &Path,
        dirs: &VaultDirs,
        query_sets: &[Vec<DiscoveryQuery>],
    ) -> Vec<DiscoveryCapture> {
        let reports = crate::list_reviews(root, dirs).unwrap();
        let batch = capture_discovery_batch(root, dirs, query_sets, None).unwrap();
        let shared = capture_discovery_batch(root, dirs, query_sets, Some(&reports)).unwrap();
        assert_eq!(batch.len(), query_sets.len());
        assert_eq!(shared.len(), query_sets.len());
        for ((queries, capture), shared) in query_sets.iter().zip(&batch).zip(&shared) {
            let individual = capture_discovery(root, dirs, queries).unwrap();
            assert_eq!(
                serialized(capture).unwrap(),
                serialized(&individual).unwrap()
            );
            assert_eq!(serialized(capture).unwrap(), serialized(shared).unwrap());
            assert_eq!(capture.guarded_pages(), individual.guarded_pages());
            assert_eq!(capture.guarded_pages(), shared.guarded_pages());
        }
        batch
    }

    #[test]
    fn batch_captures_preserve_local_queries_reviews_and_shared_exact_bytes() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let subject = "wiki/documents/retired.md";
        let raw = "---\ntype: document\nknowledge_status: retired\n---\n## Summary\n보존 정책은 기존 관찰이다.🙂\n\n## Observation\n[Helios][reference] remains attributed.\n\n## Definitions\n[reference]: https://destination.invalid\n";
        write(root.path(), subject, raw);
        write(
            root.path(),
            "wiki/concepts/policy.md",
            "---\ntype: concept\ntitle: Retention policy\naliases: [보존 정책]\n---\n## Synthesis\nA scoped definition.\n",
        );
        write(
            root.path(),
            "wiki/documents/html.md",
            "---\ntype: document\n---\n## Summary\nUnrelated.\n\n## Observation\n<div>Orbit <strong>관찰🙂</strong>.</div>\n<script>ScriptOnly</script>\n",
        );
        let revision = source_revision(&crate::parse_page(raw).unwrap());
        write(
            root.path(),
            "wiki/explorations/assessment.md",
            &format!(
                "---\ntype: exploration\nreview: {{status: open, subjects: [{subject}], evidence: [{{path: {subject}, revision: {revision}}}]}}\n---\nThe original scope requires assessment.\n"
            ),
        );
        let query = |query: &str, phrase, spacing| DiscoveryQuery {
            query: query.into(),
            phrase,
            spacing,
        };
        let helios = query("Helios", true, false);
        let orbit = query("Orbit", true, false);
        let query_sets = vec![
            vec![],
            vec![orbit.clone(), helios.clone(), helios.clone()],
            vec![query("보존정책", false, true)],
            vec![helios.clone()],
            vec![helios],
            vec![query("ScriptOnly", true, false)],
            vec![orbit],
        ];
        let batch = assert_batch_matches_individual(root.path(), &dirs, &query_sets);
        assert!(batch[0].queries.is_empty());
        assert!(batch[0].candidates.is_empty());
        assert!(batch[0].guarded_pages().is_empty());
        assert_eq!(batch[1].queries.len(), 2);
        assert_eq!(batch[1].candidates.len(), 2);
        let retired = batch[1]
            .candidates
            .iter()
            .find(|candidate| candidate.path == subject)
            .unwrap();
        assert_eq!(retired.query_indexes, [0]);
        assert_eq!(retired.card.reviews.open, 1);
        assert_eq!(retired.card.matches[0].preview.section, Some(2));
        assert_eq!(batch[2].candidates.len(), 2);
        assert_eq!(batch[3].candidates.len(), 1);
        assert!(batch[5].candidates.is_empty());
        assert_eq!(batch[6].candidates.len(), 1);
        assert_eq!(batch[6].candidates[0].card.reviews.open, 0);
        assert!(
            batch[6].candidates[0].card.matches[0]
                .preview
                .section
                .is_none()
        );
        let shared = &batch[1].guarded_pages()[Path::new(subject)];
        assert_eq!(shared.as_str(), raw);
        for index in [2, 3, 4] {
            assert!(Arc::ptr_eq(
                shared,
                &batch[index].guarded_pages()[Path::new(subject)]
            ));
        }

        write(
            root.path(),
            "wiki/documents/later.md",
            "---\ntype: document\n---\nHelios is a later observation.\n",
        );
        let refreshed = assert_batch_matches_individual(root.path(), &dirs, &query_sets);
        assert_ne!(refreshed[3].revision, batch[3].revision);
        assert_eq!(refreshed[3].candidates.len(), 2);
        assert_eq!(refreshed[6].revision, batch[6].revision);
    }

    #[test]
    fn empty_batch_recipes_skip_unneeded_corpus_reads_and_validate_other_recipes() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        write(
            root.path(),
            "wiki/documents/invalid.md",
            "---\ntype: [\n---\n",
        );
        assert!(
            capture_discovery_batch(root.path(), &dirs, &[], None)
                .unwrap()
                .is_empty()
        );
        let captures =
            capture_discovery_batch(root.path(), &dirs, &[vec![], vec![]], None).unwrap();
        assert_eq!(captures.len(), 2);
        let individual = capture_discovery(root.path(), &dirs, &[]).unwrap();
        for capture in captures {
            assert_eq!(
                serialized(&capture).unwrap(),
                serialized(&individual).unwrap()
            );
            assert!(capture.guarded_pages().is_empty());
        }
        for query in ["Helios", " "] {
            assert!(
                capture_discovery_batch(
                    root.path(),
                    &dirs,
                    &[
                        vec![],
                        vec![DiscoveryQuery {
                            query: query.into(),
                            phrase: true,
                            spacing: false,
                        }],
                    ],
                    None,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn candidate_capture_is_complete_guarded_and_invalidates_changed_membership() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let queries = [DiscoveryQuery {
            query: "Helios".into(),
            phrase: true,
            spacing: false,
        }];
        for index in 0..127 {
            write(
                root.path(),
                &format!("wiki/documents/{index:03}.md"),
                "---\ntype: document\nknowledge_status: retired\n---\n## Summary\nHelios observation.\n",
            );
        }
        let capture = capture_discovery(root.path(), &dirs, &queries).unwrap();
        assert_eq!(capture.candidates.len(), 127);
        assert_eq!(capture.guarded_pages().len(), 127);
        let legacy = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert_eq!(
            capture
                .candidates
                .iter()
                .map(|candidate| candidate.path.clone())
                .collect::<Vec<_>>(),
            legacy[&queries[0]]
                .iter()
                .map(|citation| citation.path.clone())
                .collect::<Vec<_>>()
        );
        for candidate in &capture.candidates {
            assert_eq!(candidate.query_indexes, [0]);
            let raw = &capture.guarded_pages()[Path::new(&candidate.owner)];
            assert_eq!(candidate.file_revision, file_revision(raw));
            assert_eq!(
                candidate.source_revision,
                source_revision(&crate::parse_page(raw).unwrap())
            );
            assert!(serialized(&candidate.card).unwrap().len() <= DISCOVERY_CARD_MAX_BYTES);
        }
        assert_eq!(
            capture_discovery(root.path(), &dirs, &queries)
                .unwrap()
                .revision,
            capture.revision
        );
        write(
            root.path(),
            "wiki/documents/126.md",
            "---\ntype: document\nknowledge_status: retired\n---\n## Summary\nHelios observation.\n\n## Later\nChanged beyond the opening preview.\n",
        );
        let edited = capture_discovery(root.path(), &dirs, &queries).unwrap();
        assert_ne!(edited.revision, capture.revision);
        write(
            root.path(),
            "wiki/documents/new.md",
            "---\ntype: document\n---\nHelios arrives.\n",
        );
        let added = capture_discovery(root.path(), &dirs, &queries).unwrap();
        assert_eq!(added.candidates.len(), 128);
        assert_ne!(added.revision, edited.revision);
        std::fs::remove_file(root.path().join("wiki/documents/new.md")).unwrap();
        assert_eq!(
            capture_discovery(root.path(), &dirs, &queries)
                .unwrap()
                .revision,
            edited.revision
        );
        assert!(!serialized(&capture).unwrap().contains("guarded_pages"));
    }

    #[test]
    fn candidate_cards_use_full_document_projection_and_truthful_original_locations() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let query = DiscoveryQuery {
            query: "Helios".into(),
            phrase: true,
            spacing: false,
        };
        let fragment = crate::render_markdown_fragment(
            "Note[^private-marker].\n\n[^private-marker]: [Helios](https://destination.invalid)\n",
        )
        .unwrap();
        let cases = [
            ("## Summary\nUnrelated.\n\n## Match\n[Helios][ref]\n\n## Definitions\n[ref]: https://destination.invalid\n".to_owned(), Some(2)),
            (format!("## Summary\nUnrelated.\n\n## Match\n{fragment}"), Some(2)),
            ((1..=7).map(|index| format!("## Section {index}\n{}\n\n", if index == 7 { "Helios" } else { "Unrelated." })).collect::<String>(), Some(7)),
            ("## Summary\nUnrelated.\n\n## Match\n<div>Helios <strong>scope</strong>.</div>\n".into(), None),
        ];
        for (body, section) in cases {
            for ending in ["\n", "\r\n", "\r"] {
                let body = body.replace('\n', ending);
                let raw = format!("---\ntype: document\ntitle: Source scope\n---\n{body}");
                let path = "wiki/documents/source.md";
                write(root.path(), path, &raw);
                let capture =
                    capture_discovery(root.path(), &dirs, std::slice::from_ref(&query)).unwrap();
                assert_eq!(capture.candidates.len(), 1);
                let candidate = &capture.candidates[0];
                let preview = &candidate.card.matches[0].preview;
                assert!(preview.text.text.contains("Helios"));
                assert_eq!(preview.kind, crate::ExcerptKind::MatchedBlock);
                assert_eq!(preview.section, section, "{body}");
                if let Some(section) = section {
                    let view = crate::inspect_document(
                        root.path(),
                        Path::new(path),
                        &crate::DocumentOptions {
                            section: Some(section),
                            offset: 0,
                            limit: 1000,
                            revision: Some(&candidate.file_revision),
                        },
                    )
                    .unwrap();
                    assert!(view.content.unwrap().text.contains("Helios"));
                }
                if section == Some(7) {
                    assert_eq!(candidate.card.outline_total, 7);
                    assert!(candidate.card.outline_truncated);
                }
                assert_eq!(
                    std::fs::read_to_string(root.path().join(path)).unwrap(),
                    raw
                );
            }
        }
    }

    #[test]
    fn candidate_capture_matches_alias_expansion_without_conflating_identity() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        write(
            root.path(),
            "wiki/concepts/topic.md",
            "---\ntype: concept\ntitle: Retention policy\naliases: [보존 정책]\n---\n## Synthesis\nA scoped definition.\n",
        );
        write(
            root.path(),
            "wiki/documents/source.md",
            "---\ntype: document\ntitle: Source\n---\nThe Retention policy is scoped.\n",
        );
        let queries = [DiscoveryQuery {
            query: "보존정책".into(),
            phrase: false,
            spacing: true,
        }];
        let capture = capture_discovery(root.path(), &dirs, &queries).unwrap();
        assert_eq!(capture.candidates.len(), 2);
        let legacy = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert_eq!(
            capture
                .candidates
                .iter()
                .map(|candidate| candidate.path.clone())
                .collect::<Vec<_>>(),
            legacy[&queries[0]]
                .iter()
                .map(|citation| citation.path.clone())
                .collect::<Vec<_>>()
        );
        write(
            root.path(),
            "wiki/concepts/other.md",
            "---\ntype: concept\ntitle: 보존 정책\n---\nAnother declared owner.\n",
        );
        let ambiguous = capture_discovery(root.path(), &dirs, &queries).unwrap();
        assert_ne!(ambiguous.revision, capture.revision);
        assert!(
            !ambiguous
                .candidates
                .iter()
                .any(|candidate| candidate.path.ends_with("source.md"))
        );
    }

    #[test]
    fn candidate_cards_bound_unicode_and_json_escaping_with_explicit_truncation() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let title = "가🙂\\\"\u{0001}".repeat(200);
        let aliases = vec![title.clone(); 20];
        let mut body = format!("## Summary\n{}\n\n", "가🙂 ".repeat(300));
        for index in 0..12 {
            body.push_str(&format!(
                "## Section {index}\n{} Helios\n\n",
                "긴 문장🙂 ".repeat(100)
            ));
        }
        write(
            root.path(),
            "wiki/documents/source.md",
            &format!(
                "---\ntype: document\ntitle: {}\naliases: {}\n---\n{body}",
                serialized(&title).unwrap(),
                serialized(&aliases).unwrap()
            ),
        );
        let capture = capture_discovery(
            root.path(),
            &dirs,
            &[DiscoveryQuery {
                query: "Helios".into(),
                phrase: true,
                spacing: false,
            }],
        )
        .unwrap();
        let card = &capture.candidates[0].card;
        assert!(serialized(card).unwrap().len() <= DISCOVERY_CARD_MAX_BYTES);
        assert!(card.title.truncated);
        assert!(card.aliases_truncated);
        assert!(card.opening.text.truncated);
        assert!(card.matches[0].preview.text.truncated);
        assert!(card.outline_truncated);
        assert_eq!(card.aliases_total, 20);
        assert_eq!(card.outline_total, 13);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_capture_deduplicates_verified_owners_and_detects_alias_retargeting() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let raw = "---\ntype: concept\ntitle: Helios\n---\nA source-backed definition.\n";
        write(root.path(), "backing/one.md", raw);
        write(root.path(), "backing/two.md", raw);
        for directory in ["wiki/concepts", "wiki/documents"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        std::os::unix::fs::symlink(
            "../../backing/one.md",
            root.path().join("wiki/concepts/topic.md"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../../backing/one.md",
            root.path().join("wiki/documents/topic.md"),
        )
        .unwrap();
        std::fs::hard_link(
            root.path().join("backing/one.md"),
            root.path().join("wiki/documents/hard.md"),
        )
        .unwrap();
        let queries = [DiscoveryQuery {
            query: "Helios".into(),
            phrase: true,
            spacing: false,
        }];
        let capture = capture_discovery(root.path(), &dirs, &queries).unwrap();
        assert_eq!(capture.candidates.len(), 2);
        assert_eq!(
            capture
                .candidates
                .iter()
                .filter(|candidate| candidate.owner == "backing/one.md")
                .count(),
            1
        );
        std::fs::remove_file(root.path().join("wiki/documents/topic.md")).unwrap();
        std::os::unix::fs::symlink(
            "../../backing/two.md",
            root.path().join("wiki/documents/topic.md"),
        )
        .unwrap();
        let changed = capture_discovery(root.path(), &dirs, &queries).unwrap();
        assert_eq!(changed.candidates.len(), 3);
        assert_ne!(changed.revision, capture.revision);
    }

    #[cfg(unix)]
    #[test]
    fn candidate_cards_merge_alias_queries_with_the_preferred_address() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let raw = "---\ntype: document\n---\n## Summary\rUnrelated opening.\r\r## Match\r[Orbit][reference] is bright.\r\r## Definitions\r[reference]: https://destination.invalid\r";
        write(root.path(), "backing/owner.md", raw);
        for (path, target) in [
            ("wiki/concepts/zeta.md", "../../backing/owner.md"),
            ("wiki/documents/alpha.md", "../../backing/owner.md"),
        ] {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, path).unwrap();
        }
        let queries = ["zeta", "Orbit", "alpha"].map(|query| DiscoveryQuery {
            query: query.into(),
            phrase: true,
            spacing: false,
        });
        let capture = capture_discovery(root.path(), &dirs, &queries).unwrap();
        assert_eq!(capture.candidates.len(), 1);
        let query_sets = vec![
            queries.to_vec(),
            vec![queries[1].clone()],
            vec![queries[2].clone(), queries[0].clone()],
            vec![],
        ];
        assert_batch_matches_individual(root.path(), &dirs, &query_sets);
        assert_eq!(
            capture
                .queries
                .iter()
                .map(|query| query.query.as_str())
                .collect::<Vec<_>>(),
            ["Orbit", "alpha", "zeta"]
        );
        let candidate = &capture.candidates[0];
        assert_eq!(candidate.path, "wiki/concepts/zeta.md");
        assert_eq!(candidate.owner, "backing/owner.md");
        assert_eq!(candidate.query_indexes, [0, 1, 2]);
        assert_eq!(candidate.card.title.text, "zeta");
        assert_eq!(candidate.card.opening.text.text, "Unrelated opening.");
        assert_eq!(candidate.card.matches_total, 3);
        assert!(candidate.card.matches_truncated);
        let matched = &candidate.card.matches[0];
        assert_eq!(capture.queries[matched.query_index].query, "Orbit");
        assert_eq!(matched.preview.text.text, "Orbit is bright.");
        assert_eq!(matched.preview.section, Some(2));
        assert_eq!(matched.preview.kind, crate::ExcerptKind::MatchedBlock);
        assert_eq!(
            capture.guarded_pages()[Path::new(&candidate.owner)].as_str(),
            raw
        );
        assert_eq!(candidate.file_revision, file_revision(raw));
        assert_eq!(
            serialized(&capture).unwrap(),
            serialized(&capture_discovery(root.path(), &dirs, &queries).unwrap()).unwrap()
        );

        std::fs::rename(
            root.path().join("backing/owner.md"),
            root.path().join("wiki/documents/owner.md"),
        )
        .unwrap();
        for path in ["wiki/concepts/zeta.md", "wiki/documents/alpha.md"] {
            let path = root.path().join(path);
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink("../documents/owner.md", path).unwrap();
        }
        let direct = capture_discovery(root.path(), &dirs, &queries).unwrap();
        assert_batch_matches_individual(root.path(), &dirs, &query_sets);
        assert_eq!(direct.candidates.len(), 1);
        assert_eq!(direct.candidates[0].path, "wiki/documents/owner.md");
        assert_eq!(direct.candidates[0].owner, "wiki/documents/owner.md");
        assert_eq!(direct.candidates[0].card.title.text, "owner");
        assert_eq!(direct.candidates[0].query_indexes, [0, 2]);
        assert_eq!(
            direct.guarded_pages()[Path::new("wiki/documents/owner.md")].as_str(),
            raw
        );
    }

    #[test]
    fn indexed_discovery_is_complete_and_requires_exact_current_knowledge_inputs() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = "wiki/concepts/policy.md";
        let subject = "wiki/documents/a.md";
        let body =
            "---\ntype: document\n---\nProduction order retention is 30 days. AI is in scope.\n";
        write(
            root.path(),
            concept,
            "---\ntype: concept\ntitle: Retention policy\naliases: [보존 정책]\n---\nA production order retention policy.\n",
        );
        write(root.path(), subject, body);
        for index in 0..120 {
            write(
                root.path(),
                &format!("wiki/documents/retired-{index:02}.md"),
                "---\ntype: document\nknowledge_status: retired\n---\n보존 정책: production order retention was 7 days.\n",
            );
        }
        let queries = [
            DiscoveryQuery {
                query: "Retention policy".into(),
                phrase: false,
                spacing: false,
            },
            DiscoveryQuery {
                query: "production order retention".into(),
                phrase: true,
                spacing: false,
            },
            DiscoveryQuery {
                query: "보존정책".into(),
                phrase: false,
                spacing: true,
            },
            DiscoveryQuery {
                query: "AI".into(),
                phrase: false,
                spacing: false,
            },
        ];
        let live = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert!(
            live[&queries[0]].len() > 100,
            "discovery is not ranked top-K retrieval"
        );
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        crate::prepare_search(&session, &dirs).unwrap();
        let database = root.path().join(crate::search_index::INDEX_PATH);
        let before = std::fs::read(&database).unwrap();
        assert_eq!(
            indexed_queries(root.path(), &dirs, &queries).unwrap(),
            Some(live.clone())
        );
        assert_eq!(
            discover_queries(root.path(), &dirs, &queries).unwrap(),
            live
        );
        assert_eq!(
            std::fs::read(&database).unwrap(),
            before,
            "read-only discovery must not prepare or update a capture"
        );

        let modified = std::fs::metadata(root.path().join(subject))
            .unwrap()
            .modified()
            .unwrap();
        write(root.path(), subject, &body.replace("30 days", "70 days"));
        std::fs::File::options()
            .write(true)
            .open(root.path().join(subject))
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert!(
            indexed_queries(root.path(), &dirs, &queries)
                .unwrap()
                .is_none(),
            "same-size, same-mtime changes invalidate capture reuse"
        );
        let changed = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert_ne!(changed[&queries[1]], live[&queries[1]]);
        crate::prepare_search(&session, &dirs).unwrap();
        assert_eq!(
            indexed_queries(root.path(), &dirs, &queries).unwrap(),
            Some(changed)
        );

        let added = "wiki/documents/new.md";
        write(root.path(), added, body);
        assert!(
            indexed_queries(root.path(), &dirs, &queries)
                .unwrap()
                .is_none()
        );
        let expanded = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert!(
            expanded[&queries[1]]
                .iter()
                .any(|citation| citation.path == added)
        );
        crate::prepare_search(&session, &dirs).unwrap();
        std::fs::remove_file(root.path().join(added)).unwrap();
        assert!(
            indexed_queries(root.path(), &dirs, &queries)
                .unwrap()
                .is_none()
        );
        assert!(
            !discover_queries(root.path(), &dirs, &queries).unwrap()[&queries[1]]
                .iter()
                .any(|citation| citation.path == added)
        );
        crate::prepare_search(&session, &dirs).unwrap();
        write(
            root.path(),
            concept,
            "---\ntype: concept\ntitle: Retention policy\naliases: [A different name]\n---\nA production order retention policy.\n",
        );
        assert!(
            indexed_queries(root.path(), &dirs, &queries)
                .unwrap()
                .is_none()
        );
        let renamed = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert!(
            renamed[&queries[0]].len() < live[&queries[0]].len(),
            "alias changes must alter lexical discovery immediately"
        );
    }

    #[test]
    fn discovery_uses_declared_aliases_and_preserves_unlinked_retired_candidates() {
        let root = tempfile::tempdir().unwrap();
        let files = [
            (
                "concepts/policy.md",
                "---\ntype: concept\ntitle: Retention policy\naliases: [보존 정책]\n---\n## Core\nA scoped policy.\n",
            ),
            (
                "documents/a.md",
                "---\ntype: document\n---\n보존 정책은 운영에서 30일이다.\n",
            ),
            (
                "documents/b.md",
                "---\ntype: document\nknowledge_status: retired\n---\n보존 정책은 운영에서 7일이다.\n",
            ),
            (
                "documents/unrelated.md",
                "---\ntype: document\n---\nDifferent material.\n",
            ),
        ];
        for (path, text) in files {
            let path = root.path().join("wiki").join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let query = DiscoveryQuery {
            query: "Retention policy".into(),
            phrase: false,
            spacing: false,
        };
        let matches = discover_queries(
            root.path(),
            &VaultDirs::default(),
            std::slice::from_ref(&query),
        )
        .unwrap();
        assert_eq!(
            matches[&query]
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            [
                "wiki/concepts/policy.md",
                "wiki/documents/a.md",
                "wiki/documents/b.md"
            ]
        );
        let invalid = DiscoveryQuery {
            query: " ".into(),
            ..query
        };
        assert!(discover_queries(root.path(), &VaultDirs::default(), &[invalid]).is_err());
    }
}
