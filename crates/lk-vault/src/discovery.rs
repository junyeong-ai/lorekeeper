use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use lk_core::concept::{Citation, file_revision, identity_key, source_revision};
use lk_core::config::VaultDirs;
pub use lk_core::discovery::DiscoveryQuery;

use crate::search::QueryPlan;
use crate::{SearchOptions, VaultError, search};

pub const DISCOVERY_CAPTURE_POLICY: &str = "discovery-candidates-v1";
/// The bound covers the serialized card, excluding exact candidate identities.
pub const DISCOVERY_CARD_MAX_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct DiscoveryReadMetrics {
    pub files: usize,
    /// Logical bytes consumed: the header/probe for vocabulary, complete UTF-8
    /// text for knowledge and matched rereads. Excludes buffered read-ahead.
    pub bytes: usize,
    pub enumeration_duration_us: u64,
    pub address_duration_us: u64,
    pub read_duration_us: u64,
    pub max_read_duration_us: u64,
    pub parse_duration_us: u64,
}

impl DiscoveryReadMetrics {
    pub(crate) fn record_read(&mut self, bytes: usize, elapsed: std::time::Duration) {
        self.files += 1;
        self.bytes += bytes;
        let duration = elapsed.as_micros() as u64;
        self.read_duration_us += duration;
        self.max_read_duration_us = self.max_read_duration_us.max(duration);
    }

    fn accumulate(&mut self, other: Self) {
        self.files += other.files;
        self.bytes += other.bytes;
        self.enumeration_duration_us += other.enumeration_duration_us;
        self.address_duration_us += other.address_duration_us;
        self.read_duration_us += other.read_duration_us;
        self.max_read_duration_us = self.max_read_duration_us.max(other.max_read_duration_us);
        self.parse_duration_us += other.parse_duration_us;
    }
}

/// Existing reads from successful fresh captures. Durations are summed microseconds,
/// not filesystem latency attribution or a frozen-corpus guarantee.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct DiscoveryCaptureMetrics {
    pub captures: usize,
    pub vocabulary: DiscoveryReadMetrics,
    pub knowledge: DiscoveryReadMetrics,
    pub matched_rereads: DiscoveryReadMetrics,
    pub projection_match_duration_us: u64,
}

impl DiscoveryCaptureMetrics {
    pub fn accumulate(&mut self, other: Self) {
        self.captures += other.captures;
        self.vocabulary.accumulate(other.vocabulary);
        self.knowledge.accumulate(other.knowledge);
        self.matched_rereads.accumulate(other.matched_rereads);
        self.projection_match_duration_us += other.projection_match_duration_us;
    }
}

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

impl DiscoveryCandidate {
    pub fn preferred_path<'a>(owner: &str, left: &'a str, right: &'a str) -> &'a str {
        if left == owner || right != owner && left < right {
            left
        } else {
            right
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DiscoveryCapture {
    pub policy: String,
    pub revision: String,
    pub queries: Vec<DiscoveryQuery>,
    pub candidates: Vec<DiscoveryCandidate>,
    #[serde(skip)]
    pub query_revisions: BTreeMap<DiscoveryQuery, String>,
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

struct CapturePlan {
    queries: Vec<DiscoveryQuery>,
    plans: Vec<QueryPlan>,
    guarded_pages: BTreeMap<PathBuf, Arc<String>>,
    roster: BTreeMap<String, DiscoveryCandidate>,
    reviews: BTreeMap<String, Vec<crate::review::ReviewPreview>>,
}

enum ReviewCapture<'a> {
    Reports(&'a [crate::ReviewReport]),
    Inventory(&'a mut crate::ReviewInventory),
}

enum ReviewPreviews<'a> {
    Empty,
    Reports(BTreeMap<String, Vec<crate::review::ReviewPreview>>),
    Inventory(&'a mut crate::ReviewInventory),
}

impl ReviewPreviews<'_> {
    fn for_owner(
        &mut self,
        owner: &str,
        read: &crate::ReadControl,
    ) -> Result<Vec<crate::review::ReviewPreview>, VaultError> {
        read.check()?;
        match self {
            Self::Empty => Ok(Vec::new()),
            Self::Reports(reports) => Ok(reports.get(owner).cloned().unwrap_or_default()),
            Self::Inventory(inventory) => inventory.previews_for_owner(owner, read),
        }
    }
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
    Ok(capture_discovery_batch_inner(
        root,
        dirs,
        &[queries],
        reports.map(ReviewCapture::Reports),
        None,
        &crate::ReadControl::default(),
    )?
    .remove(0))
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
    capture_discovery_batch_inner(
        root,
        dirs,
        &query_sets,
        reviews.map(ReviewCapture::Reports),
        None,
        &crate::ReadControl::default(),
    )
}

pub fn capture_discovery_batch_measured(
    root: &Path,
    dirs: &VaultDirs,
    query_sets: &[Vec<DiscoveryQuery>],
    reviews: Option<&[crate::ReviewReport]>,
) -> Result<(Vec<DiscoveryCapture>, DiscoveryCaptureMetrics), VaultError> {
    capture_discovery_batch_controlled(
        root,
        dirs,
        query_sets,
        reviews,
        &crate::ReadControl::default(),
    )
}

/// A complete fresh capture or cancellation, never a partial candidate inventory.
pub fn capture_discovery_batch_controlled(
    root: &Path,
    dirs: &VaultDirs,
    query_sets: &[Vec<DiscoveryQuery>],
    reviews: Option<&[crate::ReviewReport]>,
    read: &crate::ReadControl,
) -> Result<(Vec<DiscoveryCapture>, DiscoveryCaptureMetrics), VaultError> {
    let mut metrics = DiscoveryCaptureMetrics::default();
    let query_sets = query_sets.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let captures = capture_discovery_batch_inner(
        root,
        dirs,
        &query_sets,
        reviews.map(ReviewCapture::Reports),
        Some(&mut metrics),
        read,
    )?;
    metrics.captures = usize::from(query_sets.iter().any(|queries| !queries.is_empty()));
    Ok((captures, metrics))
}

/// Share one fresh review inventory with dependency selection in this checkpoint.
pub fn capture_discovery_batch_with_inventory(
    root: &Path,
    dirs: &VaultDirs,
    query_sets: &[Vec<DiscoveryQuery>],
    reviews: &mut crate::ReviewInventory,
    read: &crate::ReadControl,
) -> Result<(Vec<DiscoveryCapture>, DiscoveryCaptureMetrics), VaultError> {
    let mut metrics = DiscoveryCaptureMetrics::default();
    let query_sets = query_sets.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let captures = capture_discovery_batch_inner(
        root,
        dirs,
        &query_sets,
        Some(ReviewCapture::Inventory(reviews)),
        Some(&mut metrics),
        read,
    )?;
    metrics.captures = usize::from(query_sets.iter().any(|queries| !queries.is_empty()));
    Ok((captures, metrics))
}

fn capture_discovery_batch_inner(
    root: &Path,
    dirs: &VaultDirs,
    query_sets: &[&[DiscoveryQuery]],
    reviews: Option<ReviewCapture<'_>>,
    mut metrics: Option<&mut DiscoveryCaptureMetrics>,
    read: &crate::ReadControl,
) -> Result<Vec<DiscoveryCapture>, VaultError> {
    read.check()?;
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
    let vocabulary = if has_queries {
        search::ConceptVocabulary::load_controlled(
            &root,
            dirs,
            metrics.as_mut().map(|metrics| &mut metrics.vocabulary),
            read,
        )?
    } else {
        search::ConceptVocabulary::default()
    };
    let mut captures = query_sets
        .into_iter()
        .map(|queries| {
            let plans = queries
                .iter()
                .map(|query| {
                    let identity = identity_key(&query.query);
                    QueryPlan {
                        spellings: vocabulary.spellings(identity.as_deref()),
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
                reviews: BTreeMap::new(),
            }
        })
        .collect::<Vec<_>>();
    drop(vocabulary);
    let mut owned_reviews;
    let mut reviews = match reviews {
        Some(ReviewCapture::Inventory(inventory)) => {
            inventory.validate_root(&root)?;
            if has_queries {
                inventory.prepare_previews(read)?;
                ReviewPreviews::Inventory(inventory)
            } else {
                ReviewPreviews::Empty
            }
        }
        Some(ReviewCapture::Reports(reports)) if has_queries => ReviewPreviews::Reports(
            crate::review::review_previews_from_reports_controlled(&root, reports, read)?,
        ),
        None if has_queries => {
            owned_reviews = crate::ReviewInventory::load_controlled(&root, dirs, read)?;
            owned_reviews.prepare_previews(read)?;
            ReviewPreviews::Inventory(&mut owned_reviews)
        }
        _ => ReviewPreviews::Empty,
    };
    let mut guarded_pages = BTreeMap::<PathBuf, Arc<String>>::new();
    let mut matched_rereads = DiscoveryReadMetrics::default();
    let mut projection_match_duration_us = 0;
    if has_queries {
        let measured = metrics.is_some();
        search::visit_pages_controlled(
            &root,
            dirs,
            false,
            metrics.as_mut().map(|metrics| &mut metrics.knowledge),
            read,
            |mut page, _| {
                read.check()?;
                let started_at = measured.then(std::time::Instant::now);
                let document = search::VisibleDocument::from_body(&page.page.body);
                page.projection = Some(search::MatchProjection::from_document(&document));
                let matches = {
                    let matcher = search::PageMatcher::from_page(&page);
                    captures
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
                                    matcher
                                        .match_query(
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
                        .collect::<Vec<_>>()
                };
                if let Some(started_at) = started_at {
                    projection_match_duration_us += started_at.elapsed().as_micros() as u64;
                }
                page.projection = None;
                read.check()?;
                if matches.is_empty() {
                    return Ok(());
                }
                let full = crate::page_path(&root, &page.rel_path)?;
                let owner = full
                    .canonicalize()?
                    .strip_prefix(&root)
                    .map_err(std::io::Error::other)?
                    .to_path_buf();
                let started_at = measured.then(std::time::Instant::now);
                let raw = read.read_to_string(&full)?;
                if let Some(started_at) = started_at {
                    matched_rereads.record_read(raw.len(), started_at.elapsed());
                }
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
                let previews = reviews.for_owner(&owner, read)?;
                for review in &previews {
                    counts.record(review.effective_state, 1);
                }
                let outline = crate::document::outline(&page.page.body);
                for (capture_index, mut query_indexes) in matches {
                    let capture = &mut captures[capture_index];
                    capture.reviews.insert(owner.clone(), previews.clone());
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
                        path = DiscoveryCandidate::preferred_path(&owner, &previous.path, &path)
                            .to_owned();
                        query_indexes.extend(previous.query_indexes.iter().copied());
                        if path == previous.path
                            && query_indexes.len() == previous.query_indexes.len()
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
            },
        )?;
    }
    if let Some(metrics) = metrics {
        metrics.matched_rereads = matched_rereads;
        metrics.projection_match_duration_us = projection_match_duration_us;
    }
    captures
        .into_iter()
        .map(|capture| {
            read.check()?;
            let review_identity = capture.reviews;
            let candidates = capture.roster.into_values().collect::<Vec<_>>();
            let revision = file_revision(&serialized(&(
                DISCOVERY_CAPTURE_POLICY,
                &root,
                dirs,
                &capture.queries,
                &candidates,
                &review_identity,
            ))?);
            let query_revisions = capture
                .queries
                .iter()
                .zip(&capture.plans)
                .map(|(query, plan)| {
                    Ok((
                        query.clone(),
                        query_revision(query, plan.identity.as_deref(), &plan.spellings)?,
                    ))
                })
                .collect::<Result<_, VaultError>>()?;
            Ok(DiscoveryCapture {
                policy: DISCOVERY_CAPTURE_POLICY.into(),
                revision,
                queries: capture.queries,
                candidates,
                query_revisions,
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
    discover_queries_controlled(root, dirs, queries, &crate::ReadControl::default())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryQueryMatches {
    pub matches: BTreeMap<DiscoveryQuery, Vec<Citation>>,
    pub query_revisions: BTreeMap<DiscoveryQuery, String>,
}

fn query_revision(
    query: &DiscoveryQuery,
    identity: Option<&str>,
    spellings: &[String],
) -> Result<String, VaultError> {
    Ok(file_revision(&serialized(&(
        DISCOVERY_CAPTURE_POLICY,
        query,
        identity,
        spellings.iter().collect::<BTreeSet<_>>(),
    ))?))
}

pub fn discover_queries_controlled(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
    read: &crate::ReadControl,
) -> Result<BTreeMap<DiscoveryQuery, Vec<Citation>>, VaultError> {
    Ok(discover_query_matches_controlled(root, dirs, queries, read)?.matches)
}

pub fn discover_query_matches_controlled(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
    read: &crate::ReadControl,
) -> Result<DiscoveryQueryMatches, VaultError> {
    read.check()?;
    for query in queries {
        query.validate().map_err(std::io::Error::other)?;
    }
    if queries.is_empty() {
        return Ok(DiscoveryQueryMatches {
            matches: BTreeMap::new(),
            query_revisions: BTreeMap::new(),
        });
    }
    if let Some(matches) = indexed_queries(root, dirs, queries, read)? {
        read.check()?;
        return Ok(matches);
    }
    let vocabulary = search::ConceptVocabulary::load_controlled(root, dirs, None, read)?;
    let plans = queries
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|query| {
            let identity = identity_key(&query.query);
            let spellings = vocabulary.spellings(identity.as_deref());
            let terms = search::query_terms(&query.query, query.phrase);
            (query, identity, spellings, terms)
        })
        .collect::<Vec<_>>();
    let mut result = BTreeMap::from_iter(
        plans
            .iter()
            .map(|(query, ..)| ((*query).clone(), Vec::new())),
    );
    search::visit_pages_controlled(root, dirs, false, None, read, |mut page, _| {
        page.projection = Some(page.match_projection().into_owned());
        let matcher = search::PageMatcher::from_page(&page);
        let mut revision = None;
        for (query, identity, spellings, terms) in &plans {
            read.check()?;
            let options = SearchOptions {
                query: Some(&query.query),
                phrase: query.phrase,
                spacing: query.spacing,
                knowledge_status: None,
                ..Default::default()
            };
            if matcher
                .match_query(&options, spellings, terms, identity.as_deref())
                .is_some()
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
    let query_revisions = plans
        .iter()
        .map(|(query, identity, spellings, _)| {
            Ok((
                (*query).clone(),
                query_revision(query, identity.as_deref(), spellings)?,
            ))
        })
        .collect::<Result<_, VaultError>>()?;
    Ok(DiscoveryQueryMatches {
        matches: result,
        query_revisions,
    })
}

fn indexed_queries(
    root: &Path,
    dirs: &VaultDirs,
    queries: &[DiscoveryQuery],
    read: &crate::ReadControl,
) -> Result<Option<DiscoveryQueryMatches>, VaultError> {
    read.check()?;
    let snapshot = crate::search_index::inspect_search_snapshot(root, dirs);
    read.check()?;
    let snapshot = match snapshot {
        Ok(snapshot) => snapshot,
        Err(VaultError::Cancelled) => return Err(VaultError::Cancelled),
        Err(_) => return Ok(None),
    };
    let path = crate::page_path(root, Path::new(crate::search_index::INDEX_PATH))?;
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let transaction = connection.transaction()?;
    crate::search_index::verify_snapshot(&transaction, root, dirs, &snapshot.snapshot)?;
    read.check()?;
    let mut statement = transaction.prepare(
        "SELECT path, json_extract(metadata, '$.file_revision') FROM pages WHERE knowledge = 1",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            PathBuf::from(row.get::<_, String>(0)?),
            row.get::<_, String>(1)?,
        ))
    })?;
    let mut captured = BTreeMap::new();
    for row in rows {
        read.check()?;
        let (path, revision) = row?;
        captured.insert(path, revision);
    }
    let queries = queries
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut candidates = (|| -> Result<_, VaultError> {
        let mut plans = Vec::new();
        let mut paths = BTreeMap::<PathBuf, Vec<usize>>::new();
        for (index, query) in queries.iter().enumerate() {
            read.check()?;
            let options = SearchOptions {
                query: Some(&query.query),
                phrase: query.phrase,
                spacing: query.spacing,
                knowledge_status: None,
                ..Default::default()
            };
            let (plan, candidates) =
                crate::search_index::query_candidate_paths(&transaction, &options)?;
            plans.push(plan);
            for path in candidates {
                read.check()?;
                paths.entry(path).or_default().push(index);
            }
        }
        Ok((plans, paths))
    })();
    read.check()?;
    if matches!(candidates, Err(VaultError::Cancelled)) {
        return Err(VaultError::Cancelled);
    }
    let mut result =
        BTreeMap::from_iter(queries.iter().map(|query| ((*query).clone(), Vec::new())));
    let mut unchanged = true;
    search::visit_pages_controlled(root, dirs, false, None, read, |page, _| {
        unchanged &= captured.remove(&page.rel_path).as_deref() == Some(&page.file_revision);
        if let Ok((plans, paths)) = &mut candidates
            && let Some(indexes) = paths.remove(&page.rel_path)
            && unchanged
        {
            let matcher = search::PageMatcher::from_page(&page);
            let mut revision = None;
            for index in indexes {
                read.check()?;
                let query = queries[index];
                let plan = &plans[index];
                let options = SearchOptions {
                    query: Some(&query.query),
                    phrase: query.phrase,
                    spacing: query.spacing,
                    knowledge_status: None,
                    ..Default::default()
                };
                if matcher
                    .match_query(
                        &options,
                        &plan.spellings,
                        &plan.terms,
                        plan.identity.as_deref(),
                    )
                    .is_some()
                {
                    let revision = revision.get_or_insert_with(|| source_revision(&page.page));
                    result
                        .get_mut(query)
                        .expect("selected query")
                        .push(Citation::at_revision(&page.rel_path, revision.clone()));
                }
            }
        }
        Ok(())
    })?;
    if !unchanged || !captured.is_empty() {
        return Ok(None);
    }
    let (plans, paths) = candidates?;
    if !paths.is_empty() {
        return Err(std::io::Error::other(
            "indexed discovery candidate is outside the verified scope",
        )
        .into());
    }
    for matches in result.values_mut() {
        read.check()?;
        matches.sort();
        matches.dedup();
    }
    let query_revisions = queries
        .iter()
        .zip(plans)
        .map(|(query, plan)| {
            Ok((
                (*query).clone(),
                query_revision(query, plan.identity.as_deref(), &plan.spellings)?,
            ))
        })
        .collect::<Result<_, VaultError>>()?;
    Ok(Some(DiscoveryQueryMatches {
        matches: result,
        query_revisions,
    }))
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
        let mut inventory = crate::ReviewInventory::load(root, dirs).unwrap();
        let (borrowed, _) = capture_discovery_batch_with_inventory(
            root,
            dirs,
            query_sets,
            &mut inventory,
            &crate::ReadControl::default(),
        )
        .unwrap();
        assert_eq!(serialized(&batch).unwrap(), serialized(&borrowed).unwrap());
        for (actual, expected) in borrowed.iter().zip(&batch) {
            assert_eq!(actual.guarded_pages(), expected.guarded_pages());
        }
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
    fn shared_review_inventory_refuses_a_different_physical_vault() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let mut inventory = crate::ReviewInventory::load(first.path(), &dirs).unwrap();
        assert!(
            capture_discovery_batch_with_inventory(
                second.path(),
                &dirs,
                &[],
                &mut inventory,
                &crate::ReadControl::default()
            )
            .is_err()
        );
    }

    #[test]
    fn measured_batch_preserves_capture_bytes_and_counts_shared_reads_once() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = "---\ntype: concept\ntitle: Helios\naliases: [Sun]\n---\n## Summary\nHelios observation.\n";
        let document = "---\ntype: document\n---\n## Content\nHelios observation.🙂\n";
        write(root.path(), "wiki/concepts/helios.md", concept);
        write(root.path(), "wiki/documents/observation.md", document);
        let queries = vec![
            vec![DiscoveryQuery {
                query: "Helios".into(),
                phrase: true,
                spacing: false
            }];
            2
        ];
        let expected = capture_discovery_batch(root.path(), &dirs, &queries, None).unwrap();
        let (measured, metrics) =
            capture_discovery_batch_measured(root.path(), &dirs, &queries, None).unwrap();
        assert_eq!(
            serialized(&expected).unwrap(),
            serialized(&measured).unwrap()
        );
        assert_eq!(metrics.captures, 1);
        assert_eq!(metrics.vocabulary.files, 1);
        assert_eq!(
            metrics.vocabulary.bytes,
            lk_core::frontmatter::body_offset(concept).unwrap()
        );
        assert_eq!(metrics.knowledge.files, 2);
        assert_eq!(metrics.knowledge.bytes, concept.len() + document.len());
        assert_eq!(metrics.matched_rereads.files, 2);
        assert_eq!(
            metrics.matched_rereads.bytes,
            concept.len() + document.len()
        );
        let (_, empty) =
            capture_discovery_batch_measured(root.path(), &dirs, &[vec![], vec![]], None).unwrap();
        assert_eq!(empty.captures, 0);
        assert_eq!(empty.knowledge.files, 0);
        assert_eq!(empty.vocabulary.files, 0);
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
    fn overlapping_indexed_queries_preserve_live_citation_bytes() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        write(
            root.path(),
            "wiki/concepts/budget.md",
            "---\ntype: concept\ntitle: Retry budget\naliases: [재시도 정책]\n---\nRetries follow policies and use tokens.\n",
        );
        write(
            root.path(),
            "wiki/documents/current.md",
            &format!(
                "---\ntype: document\n---\n{}\n재시도 정책으로 처리한다.\n",
                "Retries policies tokens. ".repeat(40)
            ),
        );
        write(
            root.path(),
            "wiki/documents/retired.md",
            "---\ntype: document\nknowledge_status: retired\n---\nTokens covered policies for retries.\n",
        );
        let numeric = "wiki/documents/numeric.md";
        write(
            root.path(),
            numeric,
            "---\ntype: document\n---\nretry2 policy2 token2\n",
        );
        let queries = [
            ("retry", false, false),
            ("policy", false, false),
            ("token", false, false),
            ("retry policy", false, false),
            ("retry policy", true, false),
            ("재시도정책", false, true),
            ("retry", false, false),
        ]
        .map(|(query, phrase, spacing)| DiscoveryQuery {
            query: query.into(),
            phrase,
            spacing,
        });
        let live = discover_queries(root.path(), &dirs, &queries).unwrap();
        let live_capture = discover_query_matches_controlled(
            root.path(),
            &dirs,
            &queries,
            &crate::ReadControl::default(),
        )
        .unwrap();
        assert_eq!(live_capture.matches, live);
        assert_eq!(
            capture_discovery(root.path(), &dirs, &queries)
                .unwrap()
                .query_revisions,
            live_capture.query_revisions
        );

        assert_eq!(live[&queries[0]].len(), 3);
        let cancelled = crate::ReadControl::new(std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(true),
        ));
        assert!(matches!(
            indexed_queries(root.path(), &dirs, &queries, &cancelled),
            Err(VaultError::Cancelled)
        ));
        assert!(
            live.values()
                .flatten()
                .all(|citation| citation.path != numeric)
        );
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        crate::prepare_search(&session, &dirs).unwrap();
        let database = root.path().join(crate::search_index::INDEX_PATH);
        let before = std::fs::read(&database).unwrap();
        let indexed_capture =
            indexed_queries(root.path(), &dirs, &queries, &crate::ReadControl::default())
                .unwrap()
                .unwrap();
        assert_eq!(indexed_capture, live_capture);

        assert!(matches!(
            indexed_queries(root.path(), &dirs, &queries, &cancelled),
            Err(VaultError::Cancelled)
        ));
        assert_eq!(
            indexed_queries(root.path(), &dirs, &queries, &crate::ReadControl::default())
                .unwrap()
                .map(|capture| capture.matches),
            Some(live.clone())
        );
        assert_eq!(
            discover_queries(root.path(), &dirs, &queries).unwrap(),
            live
        );
        assert_eq!(std::fs::read(database).unwrap(), before);
    }

    #[test]
    fn stale_capture_precedes_candidate_query_failure() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let subject = "wiki/documents/retry.md";
        write(
            root.path(),
            subject,
            "---\ntype: document\n---\nRetries follow policy.\n",
        );
        let query = DiscoveryQuery {
            query: "retry".into(),
            phrase: false,
            spacing: false,
        };
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        crate::prepare_search(&session, &dirs).unwrap();
        let database = Connection::open(root.path().join(crate::search_index::INDEX_PATH)).unwrap();
        database.execute_batch("DROP TABLE terms").unwrap();
        drop(database);
        assert!(
            indexed_queries(
                root.path(),
                &dirs,
                std::slice::from_ref(&query),
                &crate::ReadControl::default()
            )
            .is_err()
        );
        write(
            root.path(),
            subject,
            "---\ntype: document\n---\nRetries follow the new policy.\n",
        );
        assert!(
            indexed_queries(
                root.path(),
                &dirs,
                std::slice::from_ref(&query),
                &crate::ReadControl::default()
            )
            .unwrap()
            .is_none()
        );
        let live = discover_queries(root.path(), &dirs, std::slice::from_ref(&query)).unwrap();
        assert_eq!(live[&query].len(), 1);
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
            indexed_queries(root.path(), &dirs, &queries, &crate::ReadControl::default())
                .unwrap()
                .map(|capture| capture.matches),
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
            indexed_queries(root.path(), &dirs, &queries, &crate::ReadControl::default())
                .unwrap()
                .is_none(),
            "same-size, same-mtime changes invalidate capture reuse"
        );
        let changed = discover_queries(root.path(), &dirs, &queries).unwrap();
        assert_ne!(changed[&queries[1]], live[&queries[1]]);
        crate::prepare_search(&session, &dirs).unwrap();
        assert_eq!(
            indexed_queries(root.path(), &dirs, &queries, &crate::ReadControl::default())
                .unwrap()
                .map(|capture| capture.matches),
            Some(changed)
        );

        let added = "wiki/documents/new.md";
        write(root.path(), added, body);
        assert!(
            indexed_queries(root.path(), &dirs, &queries, &crate::ReadControl::default())
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
            indexed_queries(root.path(), &dirs, &queries, &crate::ReadControl::default())
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
            indexed_queries(root.path(), &dirs, &queries, &crate::ReadControl::default())
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
