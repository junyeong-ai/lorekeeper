//! Lexical discovery of authoritative Markdown: which pages answer to a name,
//! and which merely mention it. Prepared SQLite snapshots share these semantics.
//!
//! There is no score. A hit is ordered by WHERE it matched — the page's own names, then the
//! line it opens with, then anywhere in its prose — and within that by the number of citing
//! pages, which does not measure independent evidence or confidence. The ordering is explainable and
//! reproducible across runs, which a weighted blend of the two would not be.
//!
//! A query naming one concept matches its declared names as phrases. Other queries require
//! every term. Names and aliases belong to the vault's vocabulary; the reader does not infer
//! equivalent spellings or combine scattered words into a declared concept name.

use std::borrow::Cow;
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::ops::Range;
use std::path::{Path, PathBuf};

use lk_core::concept::identity_key;
use lk_core::config::{SourceType, VaultDirs};
use lk_core::frontmatter::KnowledgeStatus;
use lk_core::vault_path::{concepts_dir, documents_dir, explorations_dir};
use pulldown_cmark::{Event, Options, Tag, TagEnd};
use unicode_normalization::UnicodeNormalization;

use crate::VaultError;

pub(super) const REVIEW_PREVIEW_LIMIT: usize = 3;

/// Where a query matched a page, from the strongest claim to the weakest. The derived
/// ordering is the search's ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchField {
    /// Metadata-only browsing; no text relevance is claimed.
    Catalog,
    /// The query IS one of the page's names, by the same exact fold `lore resolve` uses. The
    /// page does not merely discuss the subject; it is the page the name addresses.
    Identity,
    /// A declared name with different whitespace; this does not establish name ownership.
    NameVariant,
    /// The subject is named in the page's declared title or aliases.
    Name,
    /// The subject is named in the page's opening statement — the first line of its first
    /// section, which is what every format leads with.
    Summary,
    /// The subject is named somewhere in the page's prose. Link destinations are not prose: a
    /// concept page carries one per citation, so matching them would make every page in the
    /// vault answer to `daily` or to any source id.
    Text,
}

/// What the excerpt's source block establishes. A bounded preview may clip that block;
/// `excerpt_section` locates its complete content under the hit's file revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExcerptKind {
    Opening,
    MatchedBlock,
    PartialMatch,
    Unavailable,
}

/// One page a query reached.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub id: String,
    pub path: String,
    pub title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Declared creation date, not ingestion time; invalid strings remain visible (bounded).
    pub created: Option<String>,
    pub created_status: CreatedStatus,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub created_truncated: bool,
    pub source_count: u64,
    pub synthesis: SynthesisFreshness,
    pub knowledge_status: KnowledgeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knowledge_review: Option<String>,
    pub tags: Vec<String>,
    pub phrase_match: bool,
    pub spacing_match: bool,
    pub reviews: crate::ContextReviews,
    pub matched: MatchField,
    pub excerpt: String,
    pub excerpt_kind: ExcerptKind,
    /// Section id accepted by `wiki read --section`, pinned by `file_revision`.
    /// None when a source position is unproven, including the HTML DOM projection.
    pub excerpt_section: Option<usize>,
    pub file_revision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_record: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_identity: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub metadata_issues: Vec<String>,
}

/// Whether a concept's completed synthesis answers the captured graph evidence.
/// Input currency does not establish semantic correctness or independent corroboration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SynthesisFreshness {
    pub status: SynthesisInputStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_input: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SynthesisInputStatus {
    NotApplicable,
    NotCaptured,
    EmptyEvidence,
    Untracked,
    Pending,
    CurrentInput,
    InvalidMetadata,
}

impl SynthesisFreshness {
    pub(super) fn from_frontmatter(
        frontmatter: &lk_core::frontmatter::Frontmatter,
        concept: bool,
    ) -> Self {
        let mut value = Self {
            status: if concept {
                SynthesisInputStatus::NotCaptured
            } else {
                SynthesisInputStatus::NotApplicable
            },
            evidence_digest: None,
            recorded_input: None,
            completed_input: None,
            issues: Vec::new(),
        };
        if !concept {
            return value;
        }
        let inputs = frontmatter.get(lk_core::frontmatter::field::LLM_INPUTS);
        if inputs.is_some_and(|inputs| !inputs.is_null() && !inputs.is_object()) {
            value
                .issues
                .push("llm_inputs must be an object or null".into());
        }
        let mut marker = |key: &str| match inputs.and_then(|inputs| inputs.get(key)) {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
            Some(_) => {
                value.issues.push(format!(
                    "llm_inputs.{key} must be a nonblank string or null"
                ));
                None
            }
        };
        value.recorded_input = marker(lk_core::frontmatter::field::SYNTHESIS);
        value.completed_input = marker(&lk_core::frontmatter::field::completion(
            lk_core::frontmatter::field::SYNTHESIS,
        ));
        if !value.issues.is_empty() {
            value.status = SynthesisInputStatus::InvalidMetadata;
        }
        value
    }

    pub(super) fn with_evidence(mut self, citations: usize, digest: &str) -> Self {
        self.evidence_digest = Some(digest.to_owned());
        self.status = if !self.issues.is_empty() {
            SynthesisInputStatus::InvalidMetadata
        } else if citations == 0 {
            SynthesisInputStatus::EmptyEvidence
        } else if self.completed_input.as_deref() == Some(digest) {
            SynthesisInputStatus::CurrentInput
        } else if self.recorded_input.is_none() && self.completed_input.is_none() {
            SynthesisInputStatus::Untracked
        } else {
            SynthesisInputStatus::Pending
        };
        self
    }
}

/// Metadata browsing order. Search relevance ordering is unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowseSort {
    #[default]
    Title,
    /// Valid declared dates newest first, then exact path; unknown dates last.
    Created,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CreatedStatus {
    Valid,
    Missing,
    Invalid,
}

/// Only canonical civil dates participate in chronological discovery.
pub fn parse_created_date(value: &str) -> Option<jiff::civil::Date> {
    let date = value.parse::<jiff::civil::Date>().ok()?;
    (value.len() == 10 && date.to_string() == value).then_some(date)
}

/// Search knowledge pages, optionally including managed source and review pages.
/// Source inclusion is explicit so callers can retrieve evidence without requiring
/// a successful concept extraction first.
pub struct SearchOptions<'a> {
    pub query: Option<&'a str>,
    pub sort: BrowseSort,
    pub created_from: Option<jiff::civil::Date>,
    pub created_to: Option<jiff::civil::Date>,
    pub snapshot: Option<&'a str>,
    /// Maximum returned hits, between 1 and 100 for public search and browse.
    pub limit: usize,
    pub offset: usize,
    /// Maximum distinct values per metadata facet, ordered by their exact value.
    pub facet_limit: usize,
    pub facet_offset: usize,
    pub include_sources: bool,
    pub category: Option<&'a str>,
    pub source_status: Option<&'a str>,
    /// Exact declared repository ownership, never inferred concept evidence scope.
    pub repository: Option<&'a str>,
    pub provider: Option<SourceType>,
    pub namespace: Option<&'a str>,
    /// Exact lexical vault-relative address prefix, matched on path components.
    pub path_prefix: Option<&'a Path>,
    pub format: Option<&'a str>,
    pub tags: &'a [String],
    pub knowledge_status: Option<KnowledgeStatus>,
    pub phrase: bool,
    pub spacing: bool,
}

impl Default for SearchOptions<'_> {
    fn default() -> Self {
        Self {
            query: None,
            sort: BrowseSort::Title,
            created_from: None,
            created_to: None,
            snapshot: None,
            limit: 20,
            offset: 0,
            facet_limit: 20,
            facet_offset: 0,
            include_sources: false,
            category: None,
            source_status: None,
            repository: None,
            provider: None,
            namespace: None,
            path_prefix: None,
            format: None,
            tags: &[],
            knowledge_status: Some(KnowledgeStatus::Active),
            phrase: false,
            spacing: false,
        }
    }
}

impl SearchOptions<'_> {
    pub(crate) fn validate_browse(&self) -> Result<(), VaultError> {
        let message = if !(1..=100).contains(&self.facet_limit) {
            Some("facet limit must be between 1 and 100")
        } else if self.query.is_none() && (self.phrase || self.spacing) {
            Some("phrase and spacing matching require a query")
        } else if self.query.is_some() && self.sort != BrowseSort::Title {
            Some("created sorting is only available when browsing without a query")
        } else if self.repository.is_some() && (self.provider.is_some() || self.namespace.is_some())
        {
            Some("repository scope cannot be combined with provider or namespace")
        } else if self.provider.is_some() != self.namespace.is_some() {
            Some("provider and namespace scope must be supplied together")
        } else if [self.repository, self.namespace]
            .into_iter()
            .flatten()
            .any(|value| value.trim().is_empty())
        {
            Some("repository and namespace must be nonblank exact identifiers")
        } else if self.path_prefix.is_some_and(|prefix| {
            prefix
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
                || prefix.to_str().is_none_or(|text| {
                    text.is_empty()
                        || text.contains(['\\', '\0'])
                        || text
                            .split('/')
                            .any(|part| part.is_empty() || matches!(part, "." | ".."))
                })
        }) {
            Some("path prefix must be a nonempty exact vault-relative path without dot components")
        } else if self
            .created_from
            .zip(self.created_to)
            .is_some_and(|(from, to)| from > to)
        {
            Some("created date range requires from <= to")
        } else {
            None
        };
        if let Some(message) = message {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, message).into());
        }
        Ok(())
    }
}

#[derive(serde::Serialize)]
pub struct SearchResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    pub hits: Vec<SearchHit>,
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    /// Counts for the query and source scope, before metadata filters.
    pub categories: FacetPage,
    pub source_statuses: FacetPage,
    pub formats: FacetPage,
    pub tags: FacetPage,
    pub knowledge_statuses: FacetPage,
}

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct FacetPage {
    pub values: BTreeMap<String, usize>,
    /// Number of distinct values, not the number of matching pages.
    pub total: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

impl FacetPage {
    fn from_counts(counts: BTreeMap<String, usize>, options: &SearchOptions<'_>) -> Self {
        let total = counts.len();
        let values = counts
            .into_iter()
            .skip(options.facet_offset)
            .take(options.facet_limit)
            .collect::<BTreeMap<_, _>>();
        let end = options.facet_offset.saturating_add(values.len());
        Self {
            values,
            total,
            offset: options.facet_offset,
            next_offset: (end < total).then_some(end),
        }
    }
}

#[derive(Default)]
struct FacetCounts {
    categories: BTreeMap<String, usize>,
    source_statuses: BTreeMap<String, usize>,
    formats: BTreeMap<String, usize>,
    tags: BTreeMap<String, usize>,
    knowledge_statuses: BTreeMap<String, usize>,
}

/// Discovery exposes recorded navigation fields; full source metadata remains in the page.
fn source_record_preview(
    page: &crate::VaultPage,
    issues: &mut Vec<String>,
) -> Option<serde_json::Value> {
    let value = page.frontmatter.get("source_record")?;
    if value.is_null() {
        return Some(serde_json::Value::Null);
    }
    let Some(record) = value.as_object() else {
        issues.push("source_record must be an object or null".into());
        return None;
    };
    let mut selected = serde_json::Map::new();
    for key in [
        "id",
        "kind",
        "status",
        "superseded_by",
        "created",
        "updated",
        "reviewed",
        "timestamp_basis",
    ] {
        if let Some(value) = record.get(key) {
            if value.is_string() || value.is_null() {
                selected.insert(key.into(), value.clone());
            } else {
                issues.push(format!("source_record.{key} must be a string or null"));
            }
        }
    }
    Some(serde_json::Value::Object(selected))
}

fn source_identity_preview(
    page: &crate::VaultPage,
    issues: &mut Vec<String>,
) -> Option<serde_json::Value> {
    let value = page.frontmatter.get("source_identity")?;
    if value.is_null() {
        return Some(serde_json::Value::Null);
    }
    match lk_core::source_identity::SourceIdentity::from_value(value.clone()) {
        Ok(_) => Some(value.clone()),
        Err(error) => {
            issues.push(error);
            None
        }
    }
}

fn knowledge_review_preview(page: &crate::VaultPage, issues: &mut Vec<String>) -> Option<String> {
    let value = page.frontmatter.get("knowledge_review")?;
    let Some(address) = value.as_str() else {
        issues.push("knowledge_review must be a vault-relative Markdown address".into());
        return None;
    };
    if address.chars().count() > 400
        || !lk_core::link::resolve_dest(Path::new(""), address).is_some_and(|path| {
            path.to_string_lossy() == address
                && path.extension().is_some_and(|extension| extension == "md")
        })
    {
        issues.push(
            "knowledge_review must be an exact Markdown address of at most 400 characters".into(),
        );
        return None;
    }
    Some(address.to_owned())
}

fn source_file_preview(
    page: &crate::VaultPage,
    issues: &mut Vec<String>,
) -> Option<serde_json::Value> {
    match page.frontmatter.source_file() {
        Ok(value) => {
            value.map(|value| serde_json::to_value(value).expect("source file serializes"))
        }
        Err(issue) => {
            issues.push(issue);
            None
        }
    }
}

fn source_url_preview(
    page: &crate::VaultPage,
    issues: &mut Vec<String>,
) -> Option<serde_json::Value> {
    let value = page.frontmatter.get("source_url")?;
    if value.is_string() || value.is_null() {
        Some(value.clone())
    } else {
        issues.push("source_url must be a string or null".into());
        None
    }
}

pub fn search(
    vault_root: &Path,
    dirs: &VaultDirs,
    options: &SearchOptions<'_>,
) -> Result<SearchResult, VaultError> {
    options.validate_browse()?;
    if !(1..=100).contains(&options.limit) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "search limit must be between 1 and 100",
        )
        .into());
    }
    let query = options.query.unwrap_or_default();
    let terms = query_terms(query, options.phrase);
    if options.query.is_some_and(|query| query.trim().is_empty()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "search requires a nonempty query and positive limit; omit query to browse",
        )
        .into());
    }
    let query_identity = identity_key(query);

    if let Some(snapshot) = options.snapshot {
        return super::search_index::search(vault_root, dirs, options, snapshot);
    }
    let mut pages = load_pages(vault_root, dirs, options.include_sources)?;
    attach_reviews(vault_root, dirs, &mut pages)?;
    let spellings = spellings_of(query_identity.as_deref(), &pages);

    Ok(search_pages(
        options,
        &pages,
        &spellings,
        &terms,
        query_identity.as_deref(),
    ))
}

pub(super) fn load_pages(
    vault_root: &Path,
    dirs: &VaultDirs,
    include_sources: bool,
) -> Result<Vec<Page>, VaultError> {
    let mut pages = Vec::new();
    visit_pages(vault_root, dirs, include_sources, |page, _| {
        pages.push(page);
        Ok(())
    })?;
    Ok(pages)
}

/// The shared live scope and reader. Body evidence can inspect one page at a time;
/// ranked discovery retains pages because alias ownership requires the vocabulary.
/// The second argument is the number of physical file lines before the parsed body.
pub(super) fn visit_pages(
    vault_root: &Path,
    dirs: &VaultDirs,
    include_sources: bool,
    visit: impl FnMut(Page, usize) -> Result<(), VaultError>,
) -> Result<(), VaultError> {
    let mut directories = vec![
        concepts_dir(dirs),
        documents_dir(dirs),
        explorations_dir(dirs),
    ];
    if include_sources {
        directories.extend([
            PathBuf::from(&dirs.daily),
            PathBuf::from(&dirs.personal),
            PathBuf::from(&dirs.synthesis),
        ]);
    }
    visit_collections(vault_root, dirs, directories, visit)
}

pub(super) fn visit_collections(
    vault_root: &Path,
    dirs: &VaultDirs,
    directories: Vec<PathBuf>,
    mut visit: impl FnMut(Page, usize) -> Result<(), VaultError>,
) -> Result<(), VaultError> {
    let reserved = crate::navigation::ReservedNavigationPaths::load(vault_root, dirs)?;
    let mut visited = std::collections::BTreeSet::new();
    for rel_dir in directories {
        let concept_collection = rel_dir == concepts_dir(dirs);
        for path in markdown_files(vault_root, &vault_root.join(&rel_dir))? {
            if !visited.insert(path.clone()) {
                continue;
            }
            if reserved.contains(&path)? {
                continue;
            }
            let raw = std::fs::read_to_string(crate::page_path(
                vault_root,
                path.strip_prefix(vault_root)
                    .map_err(std::io::Error::other)?,
            )?)?;
            let mut page = crate::parse_page(&raw).map_err(VaultError::Frontmatter)?;
            page.original = None;
            let knowledge_status = page
                .frontmatter
                .knowledge_status()
                .map_err(VaultError::Frontmatter)?;
            if page.frontmatter.get("type").and_then(|v| v.as_str()) == Some("task-board") {
                continue;
            }
            let concept = concept_collection && page.frontmatter.is_concept_or_legacy();
            let id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let title = page
                .frontmatter
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            let body_line_offset = lk_core::markdown::lines_with_endings(&raw).count()
                - lk_core::markdown::lines_with_endings(&page.body).count();
            visit(
                Page {
                    file_revision: lk_core::concept::file_revision(&raw),
                    body_line_offset,
                    // Taken from where the file actually sits rather than assembled from the
                    // directory and the name: the walk is recursive, so a page filed in a
                    // subdirectory would otherwise be reported at an address resolving nowhere.
                    rel_path: path.strip_prefix(vault_root).unwrap_or(&path).to_path_buf(),
                    aliases: string_array(&page, "aliases"),
                    id,
                    title,
                    concept,
                    citations_captured: false,
                    page,
                    knowledge_status,
                    reviews: Vec::new(),
                    review_states: crate::ReviewCounts::default(),
                    projection: None,
                },
                body_line_offset,
            )?;
        }
    }

    Ok(())
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SearchRank {
    Title(String, PathBuf),
    Created(Reverse<Option<jiff::civil::Date>>, PathBuf),
    Relevance(u8, MatchField, Reverse<u64>, String, PathBuf),
}

impl SearchRank {
    fn build(
        page: &Page,
        options: &SearchOptions<'_>,
        matched: MatchField,
        phrase: bool,
        spacing: bool,
    ) -> Self {
        if options.query.is_none() {
            return match options.sort {
                BrowseSort::Title => Self::Title(page.title.clone(), page.rel_path.clone()),
                BrowseSort::Created => {
                    Self::Created(Reverse(page.created_date()), page.rel_path.clone())
                }
            };
        }
        let priority = match matched {
            MatchField::Identity => 0,
            MatchField::NameVariant => 1,
            _ if phrase => 2,
            _ if !spacing => 3,
            _ => 4,
        };
        Self::Relevance(
            priority,
            matched,
            Reverse(page.page.frontmatter.source_count().unwrap_or(0)),
            page.id.clone(),
            page.rel_path.clone(),
        )
    }
}

struct SearchCandidate<'a> {
    rank: SearchRank,
    page: Cow<'a, Page>,
    matched: MatchField,
    phrase_match: bool,
    spacing_match: bool,
}

impl PartialEq for SearchCandidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.rank == other.rank
    }
}
impl Eq for SearchCandidate<'_> {}
impl PartialOrd for SearchCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SearchCandidate<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank.cmp(&other.rank)
    }
}

pub(super) struct SearchCollector<'a> {
    window: usize,
    candidates: BinaryHeap<SearchCandidate<'a>>,
    result: SearchResult,
    facets: FacetCounts,
}

impl<'a> SearchCollector<'a> {
    pub(super) fn new(options: &SearchOptions<'_>) -> Self {
        Self {
            window: if options.limit == 0 {
                0
            } else {
                options.offset.saturating_add(options.limit)
            },
            candidates: BinaryHeap::new(),
            facets: FacetCounts::default(),
            result: SearchResult {
                snapshot: options.snapshot.map(String::from),
                hits: Vec::new(),
                total: 0,
                offset: options.offset,
                next_offset: None,
                categories: FacetPage::default(),
                source_statuses: FacetPage::default(),
                formats: FacetPage::default(),
                tags: FacetPage::default(),
                knowledge_statuses: FacetPage::default(),
            },
        }
    }

    pub(super) fn push(
        &mut self,
        mut entry: Cow<'a, Page>,
        options: &SearchOptions<'_>,
        spellings: &[String],
        terms: &[String],
        query_identity: Option<&str>,
    ) {
        let matched = match_query(&entry, options, spellings, terms, query_identity);
        let Some((matched, phrase_match, spacing_match)) = matched else {
            return;
        };
        let metadata = |key| {
            entry
                .page
                .frontmatter
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
        };
        *self
            .facets
            .categories
            .entry(metadata("category").to_owned())
            .or_insert(0) += 1;
        *self
            .facets
            .formats
            .entry(metadata("type").to_owned())
            .or_insert(0) += 1;
        for tag in search_tags(&entry.page) {
            *self.facets.tags.entry(tag).or_insert(0) += 1;
        }
        *self
            .facets
            .knowledge_statuses
            .entry(entry.knowledge_status().as_str().to_owned())
            .or_insert(0) += 1;
        let source_status = entry
            .page
            .frontmatter
            .get("source_record")
            .and_then(|record| record.get("status"))
            .and_then(|value| value.as_str())
            .unwrap_or("");
        *self
            .facets
            .source_statuses
            .entry(source_status.to_owned())
            .or_insert(0) += 1;
        if !entry.matches_filters(options) {
            return;
        }
        self.result.total += 1;
        if self.window == 0 {
            return;
        }
        let rank = SearchRank::build(&entry, options, matched, phrase_match, spacing_match);
        if self.candidates.len() == self.window
            && self
                .candidates
                .peek()
                .is_some_and(|candidate| candidate.rank <= rank)
        {
            return;
        }
        // Captured projections are needed only while matching; excerpts load selected bodies.
        if let Cow::Owned(page) = &mut entry {
            page.projection = None;
        }
        self.candidates.push(SearchCandidate {
            rank,
            page: entry,
            matched,
            phrase_match,
            spacing_match,
        });
        if self.candidates.len() > self.window {
            self.candidates.pop();
        }
    }

    pub(super) fn finish(
        mut self,
        options: &SearchOptions<'_>,
        spellings: &[String],
        terms: &[String],
    ) -> SearchResult {
        self.result.hits = self
            .candidates
            .into_sorted_vec()
            .into_iter()
            .skip(options.offset)
            .take(options.limit)
            .map(|candidate| {
                let SearchCandidate {
                    page: entry,
                    matched,
                    phrase_match,
                    spacing_match,
                    ..
                } = candidate;
                let mut metadata_issues = Vec::new();
                let source_record = source_record_preview(&entry.page, &mut metadata_issues);
                let source_identity = source_identity_preview(&entry.page, &mut metadata_issues);
                let source_file = source_file_preview(&entry.page, &mut metadata_issues);
                let source_url = source_url_preview(&entry.page, &mut metadata_issues);
                let knowledge_review = knowledge_review_preview(&entry.page, &mut metadata_issues);
                let excerpt =
                    excerpt_details(&entry.page.body, matched, terms, spellings, spacing_match);
                SearchHit {
                    excerpt: excerpt.text,
                    excerpt_kind: excerpt.kind,
                    excerpt_section: excerpt.section,
                    file_revision: entry.file_revision.clone(),
                    source_record,
                    source_identity,
                    source_file,
                    source_url,
                    metadata_issues,
                    id: entry.id.clone(),
                    path: entry.rel_path.to_string_lossy().into_owned(),
                    title: entry.title.clone(),
                    aliases: entry.aliases.clone(),
                    format: entry
                        .page
                        .frontmatter
                        .get("type")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    category: entry
                        .page
                        .frontmatter
                        .get("category")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from),
                    created: entry
                        .page
                        .frontmatter
                        .get("created")
                        .and_then(|v| v.as_str())
                        .map(|value| value.chars().take(160).collect()),
                    created_status: match entry.page.frontmatter.get("created") {
                        None | Some(serde_json::Value::Null) => CreatedStatus::Missing,
                        Some(_) if entry.created_date().is_some() => CreatedStatus::Valid,
                        Some(_) => CreatedStatus::Invalid,
                    },
                    created_truncated: entry
                        .page
                        .frontmatter
                        .get("created")
                        .and_then(|v| v.as_str())
                        .is_some_and(|value| value.chars().count() > 160),
                    source_count: entry.page.frontmatter.source_count().unwrap_or(0),
                    synthesis: SynthesisFreshness::from_frontmatter(
                        &entry.page.frontmatter,
                        entry.concept,
                    ),
                    knowledge_status: entry.knowledge_status(),
                    knowledge_review,
                    tags: search_tags(&entry.page),
                    phrase_match,
                    spacing_match,
                    reviews: crate::ContextReviews {
                        reviews: entry
                            .reviews
                            .iter()
                            .take(REVIEW_PREVIEW_LIMIT)
                            .cloned()
                            .collect(),
                        total: entry.review_states.total(),
                        offset: 0,
                        next_offset: (entry.review_states.total() > REVIEW_PREVIEW_LIMIT)
                            .then_some(REVIEW_PREVIEW_LIMIT),
                        states: entry.review_states.clone(),
                    },
                    matched,
                }
            })
            .collect();
        let end = options.offset.saturating_add(self.result.hits.len());
        self.result.next_offset = (end < self.result.total).then_some(end);
        self.result.categories = FacetPage::from_counts(self.facets.categories, options);
        self.result.source_statuses = FacetPage::from_counts(self.facets.source_statuses, options);
        self.result.formats = FacetPage::from_counts(self.facets.formats, options);
        self.result.tags = FacetPage::from_counts(self.facets.tags, options);
        self.result.knowledge_statuses =
            FacetPage::from_counts(self.facets.knowledge_statuses, options);
        self.result
    }
}

pub(super) fn match_query(
    page: &Page,
    options: &SearchOptions<'_>,
    spellings: &[String],
    terms: &[String],
    identity: Option<&str>,
) -> Option<(MatchField, bool, bool)> {
    if options.query.is_none() {
        return Some((MatchField::Catalog, false, false));
    }
    let names = page.names().collect::<Vec<_>>();
    let projection = page.match_projection();
    let exact = match_page(
        terms,
        identity,
        spellings,
        &names,
        &page.page.body,
        Some(&projection),
    );
    let matched = exact.or_else(|| {
        options
            .spacing
            .then(|| match_spacing(terms, spellings, &names, &projection))
            .flatten()
    })?;
    let phrase = options.query.is_some_and(|query| {
        let phrase = fold_name(query);
        page.names()
            .any(|name| contains_term(&fold_name(name), &phrase))
            || contains_term(&projection.text, &phrase)
    });
    Some((
        matched,
        phrase,
        exact.is_none() || matched == MatchField::NameVariant,
    ))
}

pub(super) fn search_pages(
    options: &SearchOptions<'_>,
    pages: &[Page],
    spellings: &[String],
    terms: &[String],
    query_identity: Option<&str>,
) -> SearchResult {
    let mut collector = SearchCollector::new(options);
    for page in pages {
        collector.push(
            Cow::Borrowed(page),
            options,
            spellings,
            terms,
            query_identity,
        );
    }
    collector.finish(options, spellings, terms)
}

/// One knowledge page, read and parsed once.
///
/// Buffered rather than matched as it is read, because the query has to be read against the
/// vault's vocabulary before any page is judged, and that vocabulary is on the pages.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct Page {
    pub(super) file_revision: String,
    pub(super) body_line_offset: usize,
    pub(super) id: String,
    pub(super) title: String,
    pub(super) aliases: Vec<String>,
    pub(super) rel_path: PathBuf,
    /// Whether this page is a concept. Concepts are the vault's vocabulary — the pages a name
    /// addresses, and the only ones [`spellings_of`] reads a query's other spellings from.
    pub(super) concept: bool,
    /// Exact graph projection membership, independent of whether its evidence row remains intact.
    pub(super) citations_captured: bool,
    pub(super) page: lk_core::frontmatter::VaultPage,
    pub(super) knowledge_status: KnowledgeStatus,
    #[serde(skip)]
    pub(super) reviews: Vec<crate::review::ReviewPreview>,
    pub(super) review_states: crate::ReviewCounts,
    #[serde(skip)]
    pub(super) projection: Option<MatchProjection>,
}

pub(super) fn attach_reviews(
    root: &Path,
    dirs: &VaultDirs,
    pages: &mut [Page],
) -> Result<(), VaultError> {
    let reviews = crate::review::review_previews(root, dirs)?;
    for page in pages {
        let owner = crate::resolve_page_owner(root, &page.rel_path)?;
        page.reviews = reviews
            .get(owner.to_string_lossy().as_ref())
            .cloned()
            .unwrap_or_default();
        page.review_states = crate::ReviewCounts::default();
        for review in &page.reviews {
            page.review_states.record(review.effective_state, 1);
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct MatchProjection {
    pub summary: String,
    pub text: String,
    pub spacing_text: String,
}

impl Page {
    pub(super) fn knowledge_status(&self) -> KnowledgeStatus {
        self.knowledge_status
    }

    pub(super) fn match_projection(&self) -> std::borrow::Cow<'_, MatchProjection> {
        match &self.projection {
            Some(projection) => std::borrow::Cow::Borrowed(projection),
            None => std::borrow::Cow::Owned(MatchProjection::from_body(&self.page.body)),
        }
    }
    pub(super) fn created_date(&self) -> Option<jiff::civil::Date> {
        self.page
            .frontmatter
            .get("created")
            .and_then(|v| v.as_str())
            .and_then(parse_created_date)
    }

    pub(super) fn matches_filters(&self, options: &SearchOptions<'_>) -> bool {
        let metadata = |key| {
            self.page
                .frontmatter
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
        };
        let status = self
            .page
            .frontmatter
            .get("source_record")
            .and_then(|record| record.get("status"))
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let identity_matches = if options.repository.is_some() || options.provider.is_some() {
            let identity = self
                .page
                .frontmatter
                .get("source_identity")
                .and_then(|value| {
                    lk_core::source_identity::SourceIdentity::from_value(value.clone()).ok()
                });
            match identity {
                Some(lk_core::source_identity::SourceIdentity::Repository {
                    repository, ..
                }) => options.repository == Some(repository.as_str()),
                Some(lk_core::source_identity::SourceIdentity::Provider {
                    provider,
                    namespace,
                    ..
                }) => {
                    options.provider == Some(provider)
                        && options.namespace == Some(namespace.as_str())
                }
                None => false,
            }
        } else {
            true
        };
        identity_matches
            && options
                .path_prefix
                .is_none_or(|prefix| self.rel_path.starts_with(prefix))
            && options
                .category
                .is_none_or(|value| metadata("category") == value)
            && options.format.is_none_or(|value| metadata("type") == value)
            && options.source_status.is_none_or(|value| status == value)
            && options
                .knowledge_status
                .is_none_or(|value| self.knowledge_status() == value)
            && options
                .tags
                .iter()
                .all(|tag| search_tags(&self.page).contains(&tag_key(tag)))
            && (options.created_from.is_none() && options.created_to.is_none()
                || self.created_date().is_some_and(|date| {
                    options.created_from.is_none_or(|from| date >= from)
                        && options.created_to.is_none_or(|to| date <= to)
                }))
    }

    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.title.as_str()).chain(self.aliases.iter().map(String::as_str))
    }
}

/// Every spelling the vault declares for the concept the QUERY names, folded for matching.
///
/// A query that IS a concept's name asks what the vault knows about that concept, and what
/// that concept is CALLED is already written down: the page's title and its aliases. So the
/// prose search runs under those spellings too, and a page discussing the subject under
/// another of its names is reached rather than missed. Nothing here folds a space, stems a
/// word or matches a pattern — every spelling is one a person or an extraction recorded on
/// the page, so a hit reached this way was reached by a declared name and the line shown
/// contains it.
///
/// Declared aliases can bridge Korean spacing variants such as `지식그래프` and
/// `지식 그래프`, or an English abbreviation such as `RAG` and its full name.
/// These remain distinct identity keys: expansion happens only when the page records
/// the alternative spelling, in either direction.
///
/// Empty when the query names nothing, which leaves the term-by-term match as the only path
/// and is the answer for a query that is a phrase rather than a name.
///
/// Exact identity ownership takes precedence over retrieval folding and spacing.
/// Ambiguous candidates do not borrow aliases. A declared alias may still occur in another sense or within a Korean
/// compound; such matches are retrieval candidates, not semantic identity decisions.
/// Excerpts expose the matched prose for the caller to assess.
pub(super) fn spellings_of(query_identity: Option<&str>, pages: &[Page]) -> Vec<String> {
    let Some(key) = query_identity else {
        return Vec::new();
    };
    let mut owners: Vec<_> = pages
        .iter()
        .filter(|p| p.concept)
        .filter(|p| {
            p.names()
                .any(|name| identity_key(name).as_deref() == Some(key))
        })
        .collect();
    if owners.is_empty() {
        let folded = fold_name(key);
        owners = pages
            .iter()
            .filter(|p| p.concept)
            .filter(|p| p.names().any(|name| fold_name(name) == folded))
            .collect();
    }
    if owners.is_empty() {
        let key = spacing_key(key);
        owners = pages
            .iter()
            .filter(|p| p.concept)
            .filter(|p| p.names().any(|name| spacing_key(name) == key))
            .collect();
    }
    if owners.len() != 1 {
        return Vec::new();
    }
    owners[0]
        .names()
        .map(fold_name)
        .filter(|name| !name.is_empty())
        .collect()
}

/// The strongest claim this page has on the query, or `None` when some term is nowhere on it.
///
/// Every term must appear, and the page is ranked by the WEAKEST field it needed — a query
/// whose second word only turns up deep in the prose is a prose match however well the first
/// one matched the title. Ranking by the strongest field instead would let one common word in
/// a title outrank a page that actually discusses the whole phrase.
///
/// The summary field is what keeps a long page from answering everything. A page states its
/// subject in the line it OPENS with, so a page whose subject is the query outranks one that
/// merely grew long enough to contain the words — which, ordered by evidence, is otherwise
/// every hub page in the vault. It has to be that line rather than the whole section: a
/// concept cited a hundred times has a synthesis long enough to hold any three common words.
fn match_page(
    terms: &[String],
    query_identity: Option<&str>,
    spellings: &[String],
    names: &[&str],
    body: &str,
    projection: Option<&MatchProjection>,
) -> Option<MatchField> {
    if let Some(key) = query_identity
        && names
            .iter()
            .any(|name| identity_key(name).as_deref() == Some(key))
    {
        return Some(MatchField::Identity);
    }
    if let Some(key) = query_identity {
        let compact = spacing_key(key);
        if names
            .iter()
            .any(|name| fold_name(name) != fold_name(key) && spacing_key(name) == compact)
        {
            return Some(MatchField::NameVariant);
        }
    }
    let computed;
    let projection = match projection {
        Some(projection) => projection,
        None => {
            computed = MatchProjection::from_body(body);
            &computed
        }
    };
    let name_text = names
        .iter()
        .map(|name| fold_name(name))
        .collect::<Vec<_>>()
        .join("\n");
    let fields = [
        (MatchField::Name, name_text.as_str()),
        (MatchField::Summary, projection.summary.as_str()),
        (MatchField::Text, projection.text.as_str()),
    ];
    match_fields(terms, spellings, &fields)
}

impl MatchProjection {
    fn from_body(body: &str) -> Self {
        let document = VisibleDocument::from_body(body);
        let text = fold(&document.text());
        Self {
            summary: fold(document.opening.as_deref().unwrap_or_default()),
            spacing_text: compact_blocks(&text),
            text,
        }
    }
}

fn match_spacing(
    terms: &[String],
    spellings: &[String],
    names: &[&str],
    projection: &MatchProjection,
) -> Option<MatchField> {
    let names = names
        .iter()
        .map(|name| fold_name(name))
        .collect::<Vec<_>>()
        .join("\n");
    match_fields_by(
        terms,
        spellings,
        &[
            (MatchField::Name, &names),
            (MatchField::Summary, &projection.summary),
            (MatchField::Text, &projection.text),
        ],
        contains_spacing_term,
    )
}

fn match_fields(
    terms: &[String],
    spellings: &[String],
    fields: &[(MatchField, &str)],
) -> Option<MatchField> {
    match_fields_by(terms, spellings, fields, contains_term)
}

fn match_fields_by(
    terms: &[String],
    spellings: &[String],
    fields: &[(MatchField, &str)],
    contains: impl Fn(&str, &str) -> bool,
) -> Option<MatchField> {
    let field_of = |needle: &str| {
        fields
            .iter()
            .find(|(_, text)| contains(text, needle))
            .map(|(field, _)| *field)
    };
    if !spellings.is_empty() {
        return spellings.iter().filter_map(|name| field_of(name)).min();
    }
    terms
        .iter()
        .map(|term| field_of(term))
        .try_fold(MatchField::Name, |worst, field| Some(worst.max(field?)))
}

/// Does `text` hold `term` as a word of its own, rather than inside a longer one?
///
/// Plain containment is what a Korean query needs: a noun carries its particle inside the same
/// word (`에이전트를`, `게이트가`), so requiring a boundary there would make the language's
/// ordinary spelling unsearchable. A Latin-script query needs the opposite, because spaces do
/// separate words there — `ai` matched `daily`, `chain` and `guardrail`, and did so at the
/// second-strongest rank, since those spellings are page NAMES.
///
/// So a boundary is required per EDGE and decided by the TERM: where a term begins or ends
/// with an ASCII letter or digit, the character beside the match may not be one. Nothing here
/// inspects the page's language, and a query mixing the two scripts is judged edge by edge.
/// The limit is the other scripts that DO separate words — Cyrillic, Greek, an accented Latin
/// word — which keep plain containment: unconstrained rather than wrongly constrained, which
/// is the state this started from.
fn contains_term(text: &str, term: &str) -> bool {
    text.match_indices(term)
        .any(|(at, _)| has_term_boundaries(text, term, at, at + term.len()))
}

fn has_term_boundaries(text: &str, term: &str, start: usize, end: usize) -> bool {
    let (Some(first), Some(last)) = (term.chars().next(), term.chars().last()) else {
        return false;
    };
    let open_anywhere = !first.is_ascii_alphanumeric();
    let close_anywhere = !last.is_ascii_alphanumeric();
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    (open_anywhere || !before.is_some_and(|c| c.is_ascii_alphanumeric()))
        && (close_anywhere || !after.is_some_and(|c| c.is_ascii_alphanumeric()))
}

fn contains_spacing_term(text: &str, term: &str) -> bool {
    let needle = spacing_key(term);
    if needle.is_empty() {
        return false;
    }
    text.lines().any(|line| {
        let mut compact = String::new();
        let mut original_bytes = Vec::new();
        for (start, character) in line.char_indices() {
            if !character.is_whitespace() {
                compact.push(character);
                original_bytes.extend(start..start + character.len_utf8());
            }
        }
        compact.match_indices(&needle).any(|(start, _)| {
            let end = start + needle.len();
            has_term_boundaries(
                line,
                &needle,
                original_bytes[start],
                original_bytes[end - 1] + 1,
            )
        })
    })
}

/// What to show beside the hit. A page matched by name is introduced by what it says first;
/// one matched in its prose is shown the line that matched, because the title already failed
/// to explain why it is here.
pub(super) fn excerpt(
    body: &str,
    matched: MatchField,
    terms: &[String],
    spellings: &[String],
) -> String {
    excerpt_details(body, matched, terms, spellings, false).text
}

pub(super) struct SearchExcerpt {
    pub text: String,
    pub kind: ExcerptKind,
    pub section: Option<usize>,
}

pub(super) fn excerpt_details(
    body: &str,
    matched: MatchField,
    terms: &[String],
    spellings: &[String],
    spacing: bool,
) -> SearchExcerpt {
    let document = VisibleDocument::from_body(body);
    if matched != MatchField::Text {
        let text = document
            .opening
            .map(|text| crate::summary::truncate_summary(&text))
            .filter(|text| !text.is_empty());
        return SearchExcerpt {
            kind: if text.is_some() {
                ExcerptKind::Opening
            } else {
                ExcerptKind::Unavailable
            },
            text: text.unwrap_or_default(),
            section: None,
        };
    }
    let contains = |text: &str, term: &str| {
        contains_term(text, term) || spacing && contains_spacing_term(text, term)
    };
    let matches_all = |line: &str| {
        let text = fold(line);
        if spellings.is_empty() {
            terms.iter().all(|term| contains(&text, term))
        } else {
            spellings.iter().any(|name| contains(&text, name))
        }
    };
    let selected = document
        .blocks
        .iter()
        .find(|block| matches_all(&block.text))
        .map(|block| (block, ExcerptKind::MatchedBlock))
        .or_else(|| {
            document
                .blocks
                .iter()
                .find(|block| {
                    let text = fold(&block.text);
                    terms.iter().any(|term| contains(&text, term))
                })
                .map(|block| (block, ExcerptKind::PartialMatch))
        });
    let Some((block, kind)) = selected else {
        return SearchExcerpt {
            text: String::new(),
            kind: ExcerptKind::Unavailable,
            section: None,
        };
    };
    // Positions come from the same full-document parse that produced the selected text.
    // Section parsing alone would lose reference definitions and occurrence identity.
    let section = block.source.as_ref().and_then(|source| {
        super::document::outline(body)
            .iter()
            .rev()
            .find(|section| section.start <= source.start && source.end <= section.end)
            .map(|section| section.id)
    });
    SearchExcerpt {
        text: crate::summary::truncate_summary(&block.text),
        kind,
        section,
    }
}

#[cfg(test)]
fn search_text(body: &str) -> String {
    VisibleDocument::from_body(body).text()
}

struct VisibleBlock {
    text: String,
    source: Option<Range<usize>>,
}

/// One complete parse supplies visible blocks and the opening statement. Inline
/// formatting preserves phrases; blocks and code lines remain separate. HTML text
/// comes from the complete DOM, excluding non-content containers and attributes.
struct VisibleDocument {
    blocks: Vec<VisibleBlock>,
    opening: Option<String>,
}

impl VisibleDocument {
    fn from_body(body: &str) -> Self {
        let options =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        let markdown = lk_core::markdown::MarkdownInput::from_text(body);
        let events: Vec<_> = markdown.offset_events(options).collect();
        let section = crate::section::first_section_range(body);
        let has_html = events
            .iter()
            .any(|(event, _)| matches!(event, Event::Html(_) | Event::InlineHtml(_)));
        if !has_html {
            let blocks = markdown_search_blocks(events);
            let opening = section.and_then(|section| {
                blocks
                    .iter()
                    .find(|block| {
                        block.source.as_ref().is_some_and(|source| {
                            section.start <= source.start && source.end <= section.end
                        })
                    })
                    .map(|block| block.text.clone())
            });
            return Self { blocks, opening };
        }
        let opening = section.and_then(|section| html_opening(body, &events, section));
        let blocks = html_search_blocks(events.into_iter().map(|(event, _)| event), body.len());
        Self { blocks, opening }
    }

    fn text(&self) -> String {
        self.blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn html_opening(
    body: &str,
    events: &[(Event<'_>, Range<usize>)],
    section: Range<usize>,
) -> Option<String> {
    let heading_end = events.iter().position(|(event, range)| {
        matches!(
            event,
            Event::End(TagEnd::Heading(pulldown_cmark::HeadingLevel::H2))
        ) && range.end == section.start
    })?;
    let heading_start = events[..=heading_end].iter().rposition(|(event, _)| {
        matches!(
            event,
            Event::Start(Tag::Heading {
                level: pulldown_cmark::HeadingLevel::H2,
                ..
            })
        )
    })?;
    if events[heading_start..heading_end]
        .iter()
        .any(|(event, _)| matches!(event, Event::Html(_) | Event::InlineHtml(_)))
    {
        return None;
    }
    if events[..heading_start]
        .iter()
        .any(|(event, _)| matches!(event, Event::Html(_) | Event::InlineHtml(_)))
        && !has_html_opening_boundary(body, &events[..=heading_end], heading_start)
    {
        return None;
    }
    // Complete top-level blocks after a verified independent heading can be rendered
    // separately. HTML inside an open block has no source map, so its opening is unknown.
    let start = heading_end + 1;
    let mut prefix = start;
    let mut depth = 0;
    for (index, (event, range)) in events.iter().enumerate().skip(start) {
        if range.start >= section.end || matches!(event, Event::Html(_) | Event::InlineHtml(_)) {
            break;
        }
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            prefix = index + 1;
        }
    }
    html_search_blocks(
        events[start..prefix].iter().map(|(event, _)| event.clone()),
        section.len(),
    )
    .into_iter()
    .next()
    .map(|block| block.text)
}

fn has_html_opening_boundary(
    body: &str,
    events: &[(Event<'_>, Range<usize>)],
    heading: usize,
) -> bool {
    use markup5ever_rcdom::NodeData;

    let marker = format!(
        "data-lorekeeper-opening-{}",
        blake3::hash(body.as_bytes()).to_hex()
    );
    // HTML attribute names only fold ASCII case; character references cannot spell one.
    if body
        .as_bytes()
        .windows(marker.len())
        .any(|bytes| bytes.eq_ignore_ascii_case(marker.as_bytes()))
    {
        return false;
    }
    let mut prefix: Vec<_> = events.iter().map(|(event, _)| event.clone()).collect();
    let Event::Start(Tag::Heading { attrs, .. }) = &mut prefix[heading] else {
        return false;
    };
    attrs.push((marker.clone().into(), None));
    let document = parse_search_html(
        prefix.into_iter(),
        events.last().map_or(0, |(_, range)| range.end),
    );
    let find_child = |node: &markup5ever_rcdom::Handle, tag: &str| {
        node.children
            .borrow()
            .iter()
            .find(|child| {
                matches!(&child.data, NodeData::Element { name, .. }
                    if name.ns.as_ref() == "http://www.w3.org/1999/xhtml"
                    && name.local.as_ref() == tag)
            })
            .cloned()
    };
    let Some(html) = find_child(&document.document, "html") else {
        return false;
    };
    let Some(body) = find_child(&html, "body") else {
        return false;
    };
    let children = body.children.borrow();
    let Some(last) = children.iter().rev().find(|child| {
        !matches!(&child.data, NodeData::Text { contents } if contents.borrow().trim().is_empty())
    }) else { return false };
    matches!(&last.data, NodeData::Element { name, attrs, .. }
        if name.ns.as_ref() == "http://www.w3.org/1999/xhtml"
        && name.local.as_ref() == "h2" && attrs.borrow().iter().any(|attribute|
            attribute.name.ns.as_ref().is_empty() && attribute.name.local.as_ref() == marker))
}

fn html_search_blocks<'a>(
    events: impl Iterator<Item = Event<'a>>,
    capacity: usize,
) -> Vec<VisibleBlock> {
    let document = parse_search_html(events, capacity);
    let mut text = String::with_capacity(capacity);
    append_html_search_text(&document.document, &mut text, false);
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        // DOM repair and whole-document HTML rendering do not preserve an exact source
        // position. Keep their visible-text semantics without guessing a location.
        .map(|text| VisibleBlock { text, source: None })
        .collect::<Vec<_>>()
}

fn parse_search_html<'a>(
    events: impl Iterator<Item = Event<'a>>,
    capacity: usize,
) -> markup5ever_rcdom::RcDom {
    use html5ever::tendril::TendrilSink;
    let mut html = String::with_capacity(capacity);
    pulldown_cmark::html::push_html(&mut html, events);
    html5ever::parse_document(markup5ever_rcdom::RcDom::default(), Default::default()).one(html)
}

fn finish_search_block(
    blocks: &mut Vec<VisibleBlock>,
    text: &mut String,
    source: &mut Option<Range<usize>>,
) {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let source = source.take();
    if !normalized.is_empty() {
        blocks.push(VisibleBlock {
            text: normalized,
            source,
        });
    }
    text.clear();
}

fn markdown_search_blocks(events: Vec<(Event<'_>, Range<usize>)>) -> Vec<VisibleBlock> {
    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut source: Option<Range<usize>> = None;
    let mut code_block = false;
    for (event, range) in events {
        let value = match event {
            Event::Start(Tag::CodeBlock(_)) => {
                code_block = true;
                finish_search_block(&mut blocks, &mut text, &mut source);
                continue;
            }
            Event::End(TagEnd::CodeBlock) => {
                code_block = false;
                finish_search_block(&mut blocks, &mut text, &mut source);
                continue;
            }
            Event::Text(value) | Event::Code(value) => value,
            Event::SoftBreak | Event::HardBreak => " ".into(),
            Event::Start(
                Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough
                | Tag::Link { .. }
                | Tag::Image { .. },
            )
            | Event::End(
                TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Link
                | TagEnd::Image,
            )
            | Event::TaskListMarker(_) => continue,
            _ => {
                finish_search_block(&mut blocks, &mut text, &mut source);
                continue;
            }
        };
        for part in lk_core::markdown::lines_with_endings(&value) {
            text.push_str(part);
            if let Some(source) = &mut source {
                source.start = source.start.min(range.start);
                source.end = source.end.max(range.end);
            } else {
                source = Some(range.clone());
            }
            if code_block && part.ends_with(['\r', '\n']) {
                finish_search_block(&mut blocks, &mut text, &mut source);
            }
        }
    }
    finish_search_block(&mut blocks, &mut text, &mut source);
    blocks
}

fn append_html_search_text(node: &markup5ever_rcdom::Node, text: &mut String, preformatted: bool) {
    use markup5ever_rcdom::NodeData;
    match &node.data {
        NodeData::Text { contents } => {
            if preformatted {
                text.push_str(&contents.borrow());
            } else {
                text.extend(
                    contents
                        .borrow()
                        .chars()
                        .map(|ch| if ch == '\n' { ' ' } else { ch }),
                );
            }
        }
        NodeData::Element { name, attrs, .. } => {
            let tag = name.local.as_ref();
            if matches!(
                tag,
                "head"
                    | "script"
                    | "style"
                    | "template"
                    | "noscript"
                    | "iframe"
                    | "noembed"
                    | "noframes"
            ) {
                return;
            }
            if tag == "br" {
                text.push(' ');
                return;
            }
            let boundary = matches!(
                tag,
                "address"
                    | "article"
                    | "aside"
                    | "blockquote"
                    | "caption"
                    | "dd"
                    | "details"
                    | "div"
                    | "dl"
                    | "dt"
                    | "fieldset"
                    | "figcaption"
                    | "figure"
                    | "footer"
                    | "form"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "header"
                    | "hgroup"
                    | "hr"
                    | "li"
                    | "legend"
                    | "main"
                    | "nav"
                    | "ol"
                    | "optgroup"
                    | "option"
                    | "p"
                    | "pre"
                    | "section"
                    | "select"
                    | "summary"
                    | "table"
                    | "tbody"
                    | "td"
                    | "tfoot"
                    | "th"
                    | "thead"
                    | "tr"
                    | "ul"
            );
            if boundary {
                text.push('\n');
            }
            if tag == "img"
                && let Some(alt) = attrs
                    .borrow()
                    .iter()
                    .find(|attr| attr.name.local.as_ref() == "alt")
            {
                text.push_str(&alt.value);
            }
            for child in node.children.borrow().iter() {
                append_html_search_text(child, text, preformatted || tag == "pre");
            }
            if boundary {
                text.push('\n');
            }
        }
        NodeData::Document => {
            for child in node.children.borrow().iter() {
                append_html_search_text(child, text, preformatted);
            }
        }
        _ => {}
    }
}

pub(super) fn fold_name(text: &str) -> String {
    fold(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub(super) fn spacing_key(text: &str) -> String {
    fold(text)
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

pub(super) fn compact_blocks(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn query_terms(query: &str, phrase: bool) -> Vec<String> {
    if phrase {
        vec![fold_name(query)]
    } else {
        query.split_whitespace().map(fold).collect()
    }
}

pub(super) fn tag_key(value: &str) -> String {
    value.nfc().flat_map(char::to_lowercase).collect()
}

pub(super) fn search_tags(page: &crate::VaultPage) -> Vec<String> {
    let manual = page
        .frontmatter
        .get("source_record")
        .and_then(|record| record.get("manual_frontmatter"))
        .and_then(|record| record.get("tags"));
    [page.frontmatter.get("tags"), manual]
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|tag| !tag.trim().is_empty())
        .map(tag_key)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// What the page says it is: the body of its first root-level H2 section.
///
/// Read by POSITION, which is type- and locale-independent on purpose. A concept leads with
/// its synthesis, a document with its summary, an exploration with its question — the
/// section's name differs with both the format and `vault.locale`, its position with neither,
/// so nothing here has a table to keep in step with either.
fn first_section(body: &str) -> Option<&str> {
    crate::section::first_section(body)
}

/// The page's opening statement: the first paragraph of [`first_section`].
///
/// A paragraph rather than a line, because a hard-wrapped page would otherwise have its
/// opening sentence split across two of them — the terms of a query landing on either side of
/// a break that carries no meaning.
pub(crate) fn opening_statement(body: &str) -> Option<String> {
    let mut paragraph: Vec<&str> = Vec::new();
    for line in lk_core::markdown::lines_with_endings(first_section(body)?).map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        paragraph.push(line);
    }
    (!paragraph.is_empty()).then(|| paragraph.join(" "))
}

/// Case- and width-insensitive comparison text. NFKC first, so a full-width or decomposed
/// spelling matches the composed one a page was written with — the same normalization every
/// slug in the vault goes through.
pub(super) fn fold(text: &str) -> String {
    text.nfkc().flat_map(char::to_lowercase).collect()
}

fn string_array(page: &lk_core::frontmatter::VaultPage, key: &str) -> Vec<String> {
    page.frontmatter
        .get(key)
        .and_then(|v| v.as_array())
        .map(|seq| {
            seq.iter()
                .filter_map(|x| x.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn markdown_files(root: &Path, dir: &Path) -> Result<Vec<PathBuf>, VaultError> {
    let relative = dir.strip_prefix(root).map_err(std::io::Error::other)?;
    Ok(crate::load_markdown_inventory(root, relative)?
        .owners
        .into_iter()
        .map(|path| root.join(path))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_search_keeps_only_the_requested_rank_window_and_complete_facets() {
        let options = SearchOptions {
            query: Some("topic"),
            category: Some("selected"),
            offset: 1,
            limit: 2,
            ..Default::default()
        };
        let terms = vec!["topic".into()];
        let mut collector = SearchCollector::new(&options);
        for count in 0..1000 {
            let category = if count % 2 == 0 { "selected" } else { "other" };
            let page = Page {
                file_revision: "revision".into(),
                body_line_offset: 0,
                id: format!("item-{count:04}"),
                title: format!("Item {count}"),
                aliases: vec![],
                rel_path: PathBuf::from(format!("wiki/documents/{count}.md")),
                concept: false,
                citations_captured: false,
                page: crate::parse_page(&format!(
                    "---\ntype: document\ncategory: {category}\nsource_count: {count}\n---\n"
                ))
                .unwrap(),
                knowledge_status: KnowledgeStatus::Active,
                reviews: vec![],
                review_states: crate::ReviewCounts::default(),
                projection: Some(MatchProjection {
                    summary: "topic".into(),
                    text: format!("topic {}", "detail ".repeat(1000)),
                    spacing_text: "topic".into(),
                }),
            };
            collector.push(Cow::Owned(page), &options, &[], &terms, None);
            assert!(collector.candidates.len() <= 3);
            assert!(
                collector
                    .candidates
                    .iter()
                    .all(|candidate| candidate.page.projection.is_none())
            );
        }
        let result = collector.finish(&options, &[], &terms);
        assert_eq!(result.total, 500);
        assert_eq!(result.next_offset, Some(3));
        assert_eq!(result.formats.values["document"], 1000);
        assert_eq!(result.categories.values["selected"], 500);
        assert_eq!(result.categories.values["other"], 500);
        assert_eq!(
            result
                .hits
                .iter()
                .map(|hit| hit.source_count)
                .collect::<Vec<_>>(),
            [996, 994]
        );
    }

    #[test]
    fn visible_html_tables_preserve_text_and_cell_boundaries() {
        let body = "<table><tr><td rowspan=\"2\">검색 <strong>품질</strong></td><td>230 kB</td></tr><tr><td>다음</td></tr></table>";
        assert_eq!(search_text(body), "검색 품질\n230 kB\n다음");
    }

    #[test]
    fn visible_html_keeps_inline_phrases_and_excludes_non_content() {
        let body = "before <span data-note=\"attribute-secret\">soft\nbreak</span> and<br>hard break <a href=\"https://destination-secret.invalid\">link</a><img src=\"image-secret\" alt=\"description\"> after\n\n<script>script-secret</script>\n<style>style-secret</style>\n<template>template-secret</template>\n<!-- comment-secret -->";
        assert_eq!(
            search_text(body),
            "before soft break and hard break linkdescription after"
        );
        assert_eq!(
            search_text("one<script>script-secret</script>two"),
            "onetwo"
        );
        assert_eq!(
            search_text("<div>one &amp; two</div><div>next</div>"),
            "one & two\nnext"
        );
    }

    #[test]
    fn visible_html_preserves_markdown_block_and_code_boundaries() {
        let markdown = "## Heading\n\nsoft\nbreak and  \nhard break\n\n- first\n- second\n\n```html\n<p>literal</p>\nnext\n```\n\n| Key | Value |\n| --- | --- |\n| one | two |\n";
        let with_html = format!("{markdown}\n<span></span>");
        assert_eq!(search_text(markdown), search_text(&with_html));
    }

    #[test]
    fn visible_html_excludes_embedded_fallbacks_and_separates_choices() {
        for tag in ["noscript", "iframe", "noembed", "noframes"] {
            let body = format!("<div>before<{tag}><div>fallback value</div></{tag}>after</div>");
            assert_eq!(search_text(&body), "beforeafter", "{tag}");
        }
        for body in [
            "<fieldset><legend>alpha</legend>beta</fieldset>",
            "<select><optgroup label=\"Group\"><option>alpha</option><option>beta</option></optgroup></select>",
        ] {
            assert_eq!(search_text(body), "alpha\nbeta");
        }
    }

    #[test]
    fn public_hit_limit_is_bounded_for_search_and_browse_in_live_and_snapshot_modes() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let docs = root.path().join(documents_dir(&dirs));
        for index in 0..101 {
            write(
                &docs,
                &format!("needle-{index:03}.md"),
                "---\ntype: document\ntitle: Needle\n---\nRecorded evidence.\n",
            );
        }
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let snapshot = crate::prepare_search(&session, &dirs).unwrap();
        drop(session);
        for captured in [None, Some(snapshot.snapshot.as_str())] {
            for query in [None, Some("Needle")] {
                let mut options = SearchOptions {
                    query,
                    snapshot: captured,
                    limit: 100,
                    ..Default::default()
                };
                let result = super::search(root.path(), &dirs, &options).unwrap();
                assert_eq!(result.hits.len(), 100);
                assert_eq!(result.total, 101);
                assert_eq!(result.next_offset, Some(100));
                for limit in [0, 101] {
                    options.limit = limit;
                    let error = super::search(root.path(), &dirs, &options)
                        .err()
                        .unwrap()
                        .to_string();
                    assert!(error.contains("limit must be between 1 and 100"), "{error}");
                }
            }
        }
    }

    #[test]
    fn created_browse_preserves_date_scope_facets_and_snapshot_pagination() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let docs = root.path().join(documents_dir(&dirs));
        for (name, created, category) in [
            ("a", "2026-02-03", "ops"),
            ("b", "2026-02-03", "ops"),
            ("old", "2025-01-01", "other"),
            ("missing", "null", "ops"),
            ("invalid", "2026-02-30", "ops"),
            ("object", "{unexpected: value}", "ops"),
        ] {
            write(
                &docs,
                &format!("{name}.md"),
                &format!(
                    "---\ntype: document\ntitle: Same\ncategory: {category}\ncreated: {created}\n---\n## Summary\nBody."
                ),
            );
        }
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let snapshot = crate::prepare_search(&session, &dirs).unwrap();
        drop(session);
        for captured in [None, Some(snapshot.snapshot.as_str())] {
            let mut options = SearchOptions {
                snapshot: captured,
                sort: BrowseSort::Created,
                limit: 1,
                ..Default::default()
            };
            let mut paths = Vec::new();
            loop {
                let result = super::search(root.path(), &dirs, &options).unwrap();
                assert_eq!(result.total, 6);
                paths.extend(result.hits.into_iter().map(|hit| hit.path));
                if let Some(offset) = result.next_offset {
                    options.offset = offset;
                } else {
                    break;
                }
            }
            assert_eq!(
                paths
                    .iter()
                    .map(|p| p.rsplit('/').next().unwrap())
                    .collect::<Vec<_>>(),
                [
                    "a.md",
                    "b.md",
                    "old.md",
                    "invalid.md",
                    "missing.md",
                    "object.md"
                ]
            );
            options.offset = 0;
            options.limit = 20;
            options.category = Some("ops");
            options.created_from = parse_created_date("2026-02-03");
            options.created_to = parse_created_date("2026-02-03");
            let result = super::search(root.path(), &dirs, &options).unwrap();
            assert_eq!(result.total, 2);
            assert_eq!(result.categories.values["other"], 1);
            assert_eq!(result.categories.values["ops"], 5);
            assert_eq!(result.formats.values["document"], 6);
            assert!(
                result
                    .hits
                    .iter()
                    .all(|hit| matches!(hit.created_status, CreatedStatus::Valid))
            );
        }
        let result = super::search(
            root.path(),
            &dirs,
            &SearchOptions {
                limit: 20,
                ..Default::default()
            },
        )
        .unwrap();
        let invalid = result
            .hits
            .iter()
            .find(|hit| hit.path.ends_with("invalid.md"))
            .unwrap();
        assert!(matches!(invalid.created_status, CreatedStatus::Invalid));
        assert_eq!(invalid.created.as_deref(), Some("2026-02-30"));
        let object = result
            .hits
            .iter()
            .find(|hit| hit.path.ends_with("object.md"))
            .unwrap();
        assert!(matches!(object.created_status, CreatedStatus::Invalid));
        assert_eq!(object.created, None);
        let missing = result
            .hits
            .iter()
            .find(|hit| hit.path.ends_with("missing.md"))
            .unwrap();
        assert!(matches!(missing.created_status, CreatedStatus::Missing));
    }

    #[test]
    fn created_browse_nested_paths_preserve_component_order() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let docs = root.path().join(documents_dir(&dirs));
        for name in ["a/z.md", "a-x.md", "가.md"] {
            let path = docs.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                "---\ntype: document\ntitle: Same\ncreated: 2026-02-03\n---\nBody",
            )
            .unwrap();
        }
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let snapshot = crate::prepare_search(&session, &dirs).unwrap();
        drop(session);
        for sort in [BrowseSort::Title, BrowseSort::Created] {
            for offset in 0..3 {
                let options = SearchOptions {
                    sort,
                    offset,
                    limit: 1,
                    ..Default::default()
                };
                let live = super::search(root.path(), &dirs, &options).unwrap();
                let indexed = super::search(
                    root.path(),
                    &dirs,
                    &SearchOptions {
                        snapshot: Some(&snapshot.snapshot),
                        ..options
                    },
                )
                .unwrap();
                assert_eq!(
                    serde_json::to_value(&live.hits).unwrap(),
                    serde_json::to_value(&indexed.hits).unwrap()
                );
                assert_eq!(live.next_offset, indexed.next_offset);
            }
        }
    }

    #[test]
    fn created_browse_rejects_query_options_and_reversed_ranges_before_io() {
        let absent = Path::new("/nonexistent-created-browse-fixture");
        let dirs = VaultDirs::default();
        for options in [
            SearchOptions {
                query: Some("topic"),
                sort: BrowseSort::Created,
                ..Default::default()
            },
            SearchOptions {
                created_from: parse_created_date("2026-02-01"),
                created_to: parse_created_date("2026-01-01"),
                ..Default::default()
            },
        ] {
            let error = super::search(absent, &dirs, &options)
                .err()
                .unwrap()
                .to_string();
            assert!(
                error.contains("only available") || error.contains("from <= to"),
                "{error}"
            );
        }
        assert!(parse_created_date("2026-02-30").is_none());
        assert!(parse_created_date("2026-01-01T00:00:00Z").is_none());
    }

    #[test]
    fn discovery_filters_after_alias_resolution_and_paginates_without_losing_pages() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        write(
            &tmp.path().join(concepts_dir(&dirs)),
            "topic.md",
            "---\ntype: concept\ntitle: Topic\naliases: [Subject]\ncategory: theory\n---\n## Summary\nDefinition.",
        );
        let documents = tmp.path().join(documents_dir(&dirs));
        for name in ["a", "b", "nested/a"] {
            write(
                &documents,
                &format!("{name}.md"),
                "---\ntype: document\ntitle: Same title\ncategory: practice\n---\n## Summary\nSubject application.",
            );
        }
        let mut options = SearchOptions {
            query: Some("Topic"),
            limit: 1,
            format: Some("document"),
            category: Some("practice"),
            ..Default::default()
        };
        let mut paths = Vec::new();
        loop {
            let result = super::search(tmp.path(), &dirs, &options).unwrap();
            assert_eq!(result.total, 3);
            assert_eq!(result.categories.values["theory"], 1);
            assert_eq!(result.categories.values["practice"], 3);
            assert_eq!(result.formats.values["concept"], 1);
            paths.extend(result.hits.into_iter().map(|hit| hit.path));
            let Some(offset) = result.next_offset else {
                break;
            };
            options.offset = offset;
        }
        assert_eq!(
            paths,
            [
                "wiki/documents/a.md",
                "wiki/documents/nested/a.md",
                "wiki/documents/b.md"
            ]
        );
        options.offset = usize::MAX;
        let exhausted = super::search(tmp.path(), &dirs, &options).unwrap();
        assert!(exhausted.hits.is_empty());
        assert_eq!(exhausted.total, 3);
        assert_eq!(exhausted.next_offset, None);
    }

    #[test]
    fn metadata_previews_preserve_typed_provider_identity_and_exact_review_addresses() {
        let page = crate::parse_page(
            "---\ntype: document\ntitle: Drive source\nsource_identity: {provider: google-drive, namespace: drive-team, document: opaque-file-id}\nknowledge_review: wiki/explorations/drive-scope.md\n---\nSource content.\n",
        )
        .unwrap();
        let mut issues = Vec::new();
        assert_eq!(
            source_identity_preview(&page, &mut issues),
            Some(
                serde_json::json!({"provider":"google-drive","namespace":"drive-team","document":"opaque-file-id"})
            )
        );
        assert_eq!(
            knowledge_review_preview(&page, &mut issues).as_deref(),
            Some("wiki/explorations/drive-scope.md")
        );
        assert!(issues.is_empty());
        for invalid in [
            serde_json::json!("https://example.test/review.md"),
            serde_json::json!("../outside.md"),
            serde_json::json!("wiki/explorations/review.md#scope"),
            serde_json::json!("x".repeat(401)),
            serde_json::json!(["wiki/explorations/review.md"]),
        ] {
            let mut page = page.clone();
            page.frontmatter
                .fields
                .insert("knowledge_review".into(), invalid);
            let mut issues = Vec::new();
            assert!(knowledge_review_preview(&page, &mut issues).is_none());
            assert_eq!(issues.len(), 1);
        }
    }

    #[test]
    fn discovery_metadata_stays_compact_without_losing_source_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let documents = tmp.path().join(documents_dir(&dirs));
        let record = serde_json::json!({
            "id": "record", "kind": "adr", "status": "superseded",
            "superseded_by": "successor", "created": "2026-06-01",
            "updated": "2026-10-18", "reviewed": null, "timestamp_basis": "authored",
            "relations": (0..100).map(|i| serde_json::json!({
                "relation": "related", "target": format!("target-{i}"),
                "reference": {"location": "L20", "detail": "x".repeat(300)}
            })).collect::<Vec<_>>(),
            "attributes": {"prompt": "Read the complete original. ".repeat(2000)},
            "manual_original": {"manifest": "original-manifest", "members": ["entry.md"]}
        });
        let raw = format!(
            "---\ntype: document\ntitle: Needle\nsource_identity: {{repository: example/repo, document: record}}\nsource_file: [docs/record.md]\nsource_url: https://example.test/pinned/record.md\nsource_record: {record}\n---\n## Summary\nNeedle evidence.\n"
        );
        write(&documents, "record.md", &raw);
        let options = SearchOptions {
            query: Some("Needle"),
            ..Default::default()
        };
        let live = super::search(tmp.path(), &dirs, &options).unwrap();
        let hit = &live.hits[0];
        let serialized = serde_json::to_string(hit).unwrap();
        assert!(raw.len() > 50_000);
        assert!(serialized.len() < 2_000);
        assert_eq!(hit.source_record.as_ref().unwrap()["status"], "superseded");
        assert_eq!(
            hit.source_record.as_ref().unwrap()["superseded_by"],
            "successor"
        );
        assert!(
            hit.source_record
                .as_ref()
                .unwrap()
                .get("relations")
                .is_none()
        );
        assert!(
            hit.source_record
                .as_ref()
                .unwrap()
                .get("attributes")
                .is_none()
        );
        assert!(
            hit.source_record
                .as_ref()
                .unwrap()
                .get("manual_original")
                .is_none()
        );
        assert_eq!(hit.source_identity.as_ref().unwrap()["document"], "record");
        assert_eq!(hit.source_file, Some(serde_json::json!(["docs/record.md"])));
        assert_eq!(
            hit.source_url,
            Some(serde_json::json!("https://example.test/pinned/record.md"))
        );
        assert_eq!(hit.file_revision, lk_core::concept::file_revision(&raw));
        assert!(hit.metadata_issues.is_empty());
        let session = crate::MutationSession::acquire(tmp.path()).unwrap();
        let snapshot = crate::prepare_search(&session, &dirs).unwrap();
        let indexed = super::search(
            tmp.path(),
            &dirs,
            &SearchOptions {
                snapshot: Some(&snapshot.snapshot),
                ..options
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&live.hits).unwrap(),
            serde_json::to_value(&indexed.hits).unwrap()
        );
        let connection = rusqlite::Connection::open_with_flags(
            tmp.path().join(super::super::search_index::INDEX_PATH),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let payload: String = connection
            .query_row(
                "SELECT metadata FROM pages WHERE path = 'wiki/documents/record.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let preserved: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(preserved["page"]["frontmatter"]["source_record"], record);
        assert_eq!(
            std::fs::read_to_string(documents.join("record.md")).unwrap(),
            raw
        );
    }

    #[test]
    fn discovery_metadata_marks_invalid_values_without_inventing_source_state() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let documents = tmp.path().join(documents_dir(&dirs));
        for (name, fields) in [
            ("absent", ""),
            ("null", "source_record: null\n"),
            (
                "invalid",
                "source_record: [active]\nsource_identity: broken\nsource_file: {path: wrong}\nsource_url: 123\n",
            ),
            (
                "invalid-status",
                "source_record: {id: record, status: {active: true}}\nsource_identity: {repository: example/repo}\n",
            ),
        ] {
            write(
                &documents,
                &format!("{name}.md"),
                &format!(
                    "---\ntype: document\ntitle: {name}\n{fields}---\n## Summary\nEvidence.\n"
                ),
            );
        }
        let result = super::search(tmp.path(), &dirs, &SearchOptions::default()).unwrap();
        let hit = |name: &str| result.hits.iter().find(|hit| hit.title == name).unwrap();
        let absent = serde_json::to_value(hit("absent")).unwrap();
        assert!(absent.get("source_record").is_none());
        assert!(absent.get("metadata_issues").is_none());
        assert_eq!(hit("null").source_record, Some(serde_json::Value::Null));
        assert!(hit("null").metadata_issues.is_empty());
        assert!(hit("invalid").source_record.is_none());
        assert_eq!(hit("invalid").metadata_issues.len(), 4);
        assert!(
            hit("invalid-status")
                .source_record
                .as_ref()
                .unwrap()
                .get("status")
                .is_none()
        );
        assert_eq!(hit("invalid-status").metadata_issues.len(), 2);
        assert_eq!(result.source_statuses.values[""], 4);
    }

    #[test]
    fn browsing_uses_metadata_and_title_without_claiming_text_relevance() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "z.md",
            "---\ntitle: Zebra\nsource_count: 100\n---\n## Summary\nZ.",
        );
        write(
            &concepts,
            "a.md",
            "---\ntitle: Alpha\ncategory: ''\n---\n## Summary\nA.",
        );
        write(
            &concepts,
            "b.md",
            "---\ntitle: Beta\ncategory: theory\n---\n## Summary\nB.",
        );
        let options = SearchOptions {
            limit: 10,
            category: Some(""),
            ..Default::default()
        };
        let result = super::search(tmp.path(), &dirs, &options).unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(
            result
                .hits
                .iter()
                .map(|hit| hit.title.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "Zebra"]
        );
        assert!(
            result
                .hits
                .iter()
                .all(|hit| hit.matched == MatchField::Catalog)
        );
        assert_eq!(result.categories.values[""], 2);
        assert_eq!(result.categories.values["theory"], 1);
        assert!(
            super::search(
                tmp.path(),
                &dirs,
                &SearchOptions {
                    query: Some(" "),
                    ..options
                }
            )
            .is_err()
        );
        assert!(
            super::search(
                tmp.path(),
                &dirs,
                &SearchOptions {
                    limit: 0,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    fn search(
        root: &Path,
        dirs: &VaultDirs,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, VaultError> {
        super::search(
            root,
            dirs,
            &SearchOptions {
                query: Some(query),
                limit,
                ..Default::default()
            },
        )
        .map(|result| result.hits)
    }

    #[test]
    fn opening_search_uses_the_first_real_section_in_every_page_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        for prefix in [
            "",
            "\n# Topic\n\n",
            "```markdown\n## Example\nquoted\n```\n",
            "<!--\n```\n-->\n\n",
            "<div>\n## Fake\n</div>\n\n",
        ] {
            write(
                &concepts,
                "topic.md",
                &format!(
                    "---\ntype: concept\ntitle: Topic\n---\n{prefix}## 핵심\n\nA quasar emits radiation.\n\n## 출처\n\n- Observation\n"
                ),
            );
            let identity = search(tmp.path(), &dirs, "Topic", 10).unwrap();
            assert_eq!(identity[0].excerpt, "A quasar emits radiation.");
            let summary = search(tmp.path(), &dirs, "quasar", 10).unwrap();
            assert_eq!(summary[0].matched, MatchField::Summary);
            assert_eq!(summary[0].excerpt, "A quasar emits radiation.");
            let citation = search(tmp.path(), &dirs, "Observation", 10).unwrap();
            assert_eq!(citation[0].matched, MatchField::Text);
            let session = crate::MutationSession::acquire(tmp.path()).unwrap();
            let snapshot = crate::prepare_search(&session, &dirs).unwrap();
            for query in [None, Some("quasar")] {
                let captured = super::search(
                    tmp.path(),
                    &dirs,
                    &SearchOptions {
                        query,
                        snapshot: Some(&snapshot.snapshot),
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(captured.hits[0].excerpt, "A quasar emits radiation.");
                if query.is_some() {
                    assert_eq!(captured.hits[0].matched, MatchField::Summary);
                }
            }
        }
        assert_eq!(
            first_section(
                "## Summary\n\n```markdown\n## Quoted\n```\n\nActual.\n\n## Sources\nCitation"
            ),
            Some("\n```markdown\n## Quoted\n```\n\nActual.\n\n")
        );
    }

    #[test]
    fn declared_phrases_follow_markdown_blocks_and_independent_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "owner.md",
            "---\ntitle: Knowledge Graph\naliases: [지식 그래프]\n---\n## Summary\nDefinition.",
        );
        let documents = tmp.path().join(documents_dir(&dirs));
        for (name, body) in [
            ("wrapped", "Knowledge\nGraph applications."),
            ("emphasis", "Knowledge **Graph** applications."),
            ("spaces", "Knowledge\t  Graph applications."),
            ("paragraphs", "Knowledge\n\nGraph applications."),
            ("items", "- Knowledge\n- Graph applications."),
            ("cells", "| A | B |\n|---|---|\n| Knowledge | Graph |"),
            ("destination", "[reference](Knowledge%20Graph.md)"),
        ] {
            write(
                &documents,
                &format!("{name}.md"),
                &format!("---\ntitle: Example\n---\n## Summary\nIntroduction.\n\n{body}"),
            );
        }
        write(
            &documents,
            "names.md",
            "---\ntitle: Knowledge\naliases: [Graph]\n---\n## Summary\nUnrelated.",
        );
        let hits = search(tmp.path(), &dirs, "Knowledge Graph", 30).unwrap();
        let mut ids = hits.iter().map(|hit| hit.id.as_str()).collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, ["emphasis", "owner", "spaces", "wrapped"]);
        for hit in hits.iter().filter(|hit| hit.id != "owner") {
            assert_eq!(hit.matched, MatchField::Text);
            assert_eq!(hit.excerpt, "Knowledge Graph applications.");
        }
    }

    #[test]
    fn phrase_summary_and_aliases_preserve_field_boundaries() {
        let terms = vec!["knowledge".into(), "graph".into()];
        let spellings = vec!["knowledge graph".into()];
        for (body, expected) in [
            ("Knowledge\nGraph applications.", Some(MatchField::Summary)),
            (
                "Knowledge **Graph** applications.",
                Some(MatchField::Summary),
            ),
            ("Knowledge\n\nGraph applications.", None),
            ("- Knowledge\n- Graph applications.", None),
            ("| Knowledge | Graph |\n|---|---|", None),
            (
                "[link][ref]\n\n[ref]: https://example.org/Knowledge-Graph",
                None,
            ),
        ] {
            assert_eq!(
                match_page(
                    &terms,
                    None,
                    &spellings,
                    &["Other"],
                    &format!("## Summary\n{body}"),
                    None,
                ),
                expected,
                "{body}"
            );
        }
        assert_eq!(
            match_page(&terms, None, &spellings, &["Knowledge", "Graph"], "", None),
            None
        );
        assert_eq!(
            match_page(&terms, None, &spellings, &["Knowledge\t  Graph"], "", None),
            Some(MatchField::Name)
        );
    }

    #[test]
    fn ambiguous_search_names_do_not_expand_across_senses() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "gas.md",
            "---\ntype: concept\ntitle: CO\naliases: [carbon monoxide]\n---\nGas.\n",
        );
        write(
            &concepts,
            "metal.md",
            "---\ntype: concept\ntitle: Co\naliases: [cobalt]\n---\nMetal.\n",
        );
        write(
            &concepts,
            "sensor.md",
            "---\ntype: concept\ntitle: Sensor\n---\nMeasures carbon monoxide.\n",
        );
        let hits = search(tmp.path(), &dirs, "co", 10).unwrap();
        assert!(!hits.iter().any(|h| h.id == "sensor"));
        let exact = search(tmp.path(), &dirs, "CO", 10).unwrap();
        assert_eq!(exact[0].id, "gas");
        assert_eq!(exact[0].matched, MatchField::Identity);
    }

    #[test]
    fn source_search_reaches_unextracted_observations_and_excludes_intent() {
        let tmp = vault();
        let dirs = VaultDirs {
            daily: "observations".into(),
            ..VaultDirs::default()
        };
        write(
            &tmp.path().join(&dirs.daily),
            "event.md",
            "---\ntype: daily\ntitle: Observation\n---\nA rare failure before extraction: quasar.\n",
        );
        write(
            &tmp.path().join(&dirs.personal),
            "tasks.md",
            "---\ntype: task-board\n---\nInvestigate quasar.\n",
        );
        assert!(search(tmp.path(), &dirs, "quasar", 10).unwrap().is_empty());
        let hits = super::search(
            tmp.path(),
            &dirs,
            &SearchOptions {
                query: Some("quasar"),
                limit: 10,
                include_sources: true,
                ..Default::default()
            },
        )
        .unwrap()
        .hits;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "observations/event.md");
    }

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::create_dir_all(dir.join(name).parent().unwrap()).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }

    fn vault() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "agent-capability.md",
            "---\ntype: concept\ntitle: \"Agent Capability\"\naliases: [\"Agent Capability\", \"에이전트 호출 가능 애플리케이션 단위\"]\ncategory: agent-architecture\nsource_count: 3\n---\n\n# Agent Capability\n\n## 핵심\n\n애플리케이션의 일부를 에이전트가 직접 호출할 수 있게 노출한 단위.\n",
        );
        write(
            &concepts,
            "tool-exposure.md",
            "---\ntype: concept\ntitle: \"Tool Exposure\"\nsource_count: 9\n---\n\n# Tool Exposure\n\n## 핵심\n\n외부 기능을 호출 가능한 단위로 여는 방식이다.\n\n호출자는 대체로 에이전트이고, 도구를 여는 쪽은 권한을 함께 정한다.\n",
        );
        tmp
    }

    /// The ordering the whole command rests on: a page the query NAMES outranks one that
    /// merely carries the words, whatever the evidence behind either.
    #[test]
    fn a_name_outranks_prose_and_evidence_orders_the_rest() {
        let tmp = vault();
        let hits = search(tmp.path(), &VaultDirs::default(), "에이전트", 10).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["agent-capability", "tool-exposure"],
            "the alias carries the term; the other page only says it in prose"
        );
        assert_eq!(hits[0].matched, MatchField::Name);
        assert_eq!(hits[1].matched, MatchField::Text);
        assert!(
            hits[1].excerpt.contains("호출자는 대체로"),
            "a prose hit is shown the line that matched, not the page's opening"
        );
    }

    /// The rank that keeps a well-cited page from answering every query. A concept with a
    /// hundred citations accumulates a synthesis long enough to contain any three common
    /// words, and ordering by evidence then puts it above the page the words actually name.
    #[test]
    fn a_page_that_opens_with_the_query_outranks_a_longer_one_that_merely_holds_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "fail-open-fail-closed.md",
            "---\ntype: concept\ntitle: \"Fail-Open / Fail-Closed\"\nsource_count: 8\n---\n\n## 핵심\n\n품질 게이트의 실패 정책을 에러 성격에 따라 이분하는 설계 원칙.\n",
        );
        write(
            &concepts,
            "md-wisely.md",
            "---\ntype: concept\ntitle: \"MD Wisely\"\nsource_count: 146\n---\n\n## 핵심\n\nMD AX 프로젝트의 사내 제품이다.\n\n배포 게이트를 도입했고, 실패 시 롤백한다. 운영 정책은 별도로 관리한다.\n",
        );
        let hits = search(tmp.path(), &dirs, "게이트 실패 정책", 10).unwrap();
        assert_eq!(hits[0].id, "fail-open-fail-closed");
        assert_eq!(hits[0].matched, MatchField::Summary);
        assert_eq!(
            hits[1].matched,
            MatchField::Text,
            "the longer page still answers, as the weaker kind of hit it is"
        );
    }

    /// The question `lore resolve` answers, asked of a reader who does not know the name yet:
    /// an exact name reaches its page ahead of every page that discusses it.
    #[test]
    fn an_exact_name_is_the_strongest_match_however_it_is_spelled() {
        let tmp = vault();
        let hits = search(tmp.path(), &VaultDirs::default(), "Agent Capability", 10).unwrap();
        assert_eq!(hits[0].id, "agent-capability");
        assert_eq!(hits[0].matched, MatchField::Identity);
    }

    /// A declared alias bridges a Korean spacing variant. Identity normalization alone
    /// keeps these names distinct; the page explicitly records both spellings.
    #[test]
    fn a_query_spelled_solid_reaches_the_prose_spelled_with_a_space() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "knowledge-graph.md",
            "---\ntype: concept\ntitle: \"지식 그래프\"\naliases: [\"Knowledge Graph\", \"지식그래프\"]\nsource_count: 9\n---\n\n## 핵심\n\n개체와 관계를 그래프로 구조화한 지식 표현.\n",
        );
        write(
            &concepts,
            "graphrag.md",
            "---\ntype: concept\ntitle: \"GraphRAG\"\nsource_count: 5\n---\n\n## 핵심\n\n검색 단계에서 지식 그래프의 구조를 활용하는 RAG 계열 접근이다.\n",
        );
        write(
            &concepts,
            "unrelated.md",
            "---\ntype: concept\ntitle: Unrelated\n---\n## 핵심\n지식은 여러 관점으로 정리된다. 관계없는 그래프 예제.\n",
        );
        let ids = |q: &str| {
            search(tmp.path(), &dirs, q, 10)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("Knowledge Graph"), ["knowledge-graph", "graphrag"]);
        assert_eq!(ids("지식 그래프"), ["knowledge-graph", "graphrag"]);
        assert_eq!(
            ids("지식그래프"),
            ["knowledge-graph", "graphrag"],
            "the solid spelling names the same concept, so it reads the same prose"
        );
        assert!(
            search(tmp.path(), &dirs, "지식그래프", 10).unwrap()[1]
                .excerpt
                .contains("지식 그래프"),
            "the line shown carries the declared name that reached the page"
        );
    }

    /// The same mechanism in the other direction, which is what makes it not a Korean feature:
    /// an English query reaches prose written in Korean, and a full name reaches the prose that
    /// only ever writes the acronym. On the reference vault `Model Context Protocol` reached 9
    /// pages where `MCP` reached 202.
    #[test]
    fn a_query_reaches_the_prose_written_under_the_concepts_other_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concepts = tmp.path().join(concepts_dir(&dirs));
        write(
            &concepts,
            "prompt-injection.md",
            "---\ntype: concept\ntitle: \"Prompt Injection\"\naliases: [\"프롬프트 인젝션\"]\nsource_count: 31\n---\n\n## 핵심\n\n외부 입력이 모델의 지시로 읽히는 취약점.\n",
        );
        write(
            &concepts,
            "agent-sandbox.md",
            "---\ntype: concept\ntitle: \"Agent Sandbox\"\nsource_count: 4\n---\n\n## 핵심\n\n에이전트 실행을 격리하는 방식이다.\n\n프롬프트 인젝션이 성립해도 피해를 가둔다.\n",
        );
        let hits = search(tmp.path(), &dirs, "prompt injection", 10).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["prompt-injection", "agent-sandbox"],
            "the English query reaches the page that only writes the Korean name"
        );
        assert_eq!(hits[1].matched, MatchField::Text);
    }

    /// A query that names nothing is a phrase, and a phrase has no other spellings to read —
    /// so it is matched term by term exactly as before. Without this the expansion would be
    /// unbounded: every query would look for a concept to borrow names from.
    #[test]
    fn a_query_that_names_no_concept_is_matched_term_by_term() {
        let tmp = vault();
        let hits = search(
            tmp.path(),
            &VaultDirs::default(),
            "호출 가능한 단위로 여는 권한",
            10,
        )
        .unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["tool-exposure"]
        );
        assert_eq!(hits[0].matched, MatchField::Text);
    }

    /// The acronym is the commonest shape a technical query takes, and plain containment made
    /// it the noisiest: on the reference vault `ai` reached `md-wisely-daily-scrum`,
    /// `supply-chain-attack` and `pure-code-guardrail-chain` — all at the NAME rank, above
    /// every page that actually discusses the subject.
    #[test]
    fn a_latin_term_matches_a_word_and_not_the_middle_of_one() {
        assert!(contains_term("ai-slop", "ai"));
        assert!(contains_term("amazon sagemaker ai", "ai"));
        assert!(
            contains_term("ai기반 파이프라인", "ai"),
            "no space is needed where the neighbouring script separates words on its own"
        );
        assert!(!contains_term("md-wisely-daily-scrum", "ai"));
        assert!(!contains_term("supply-chain-attack", "ai"));
        assert!(
            !contains_term("openai-codex", "ai"),
            "a longer name is a different name"
        );
        assert!(
            !contains_term("claude-35", "5"),
            "a numeral inside a number is not the number"
        );
    }

    /// The other half of the same rule: Korean writes a noun and its particle as one word, so
    /// a boundary requirement would leave the vault's own language unsearchable.
    #[test]
    fn a_korean_term_still_matches_inside_an_inflected_word() {
        assert!(contains_term("호출자는 대체로 에이전트이고", "에이전트"));
        assert!(contains_term("게이트가 실패하면", "게이트"));
    }

    /// Every term has to land somewhere, so a query is narrowed by adding words rather than
    /// widened — the property that makes a two-word query useful at all.
    #[test]
    fn a_term_matching_nothing_excludes_the_page() {
        let tmp = vault();
        let hits = search(tmp.path(), &VaultDirs::default(), "에이전트 없는단어", 10).unwrap();
        assert!(hits.is_empty());
    }
}
