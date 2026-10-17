//! Vault search — the lookup a reader does before writing, and the one an agent does
//! instead of reading the catalog.
//!
//! `index.md` is a catalog of every page and grows with the vault; past a few thousand pages
//! reading it to locate one concept costs more than the answer is worth, and an agent has no
//! way to read part of it. This answers the same question by query instead: which pages
//! ANSWER to this, and which merely mention it.
//!
//! There is no score. A hit is ordered by WHERE it matched — the page's own names, then the
//! line it opens with, then anywhere in its prose — and within that by the number of citing
//! pages, which does not measure independent evidence or confidence. The ordering is explainable and
//! reproducible across runs, which a weighted blend of the two would not be.
//!
//! A query naming one concept matches its declared names as phrases. Other queries require
//! every term. Names and aliases belong to the vault's vocabulary; the reader does not infer
//! equivalent spellings or combine scattered words into a declared concept name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lk_core::concept::identity_key;
use lk_core::config::VaultDirs;
use lk_core::vault_path::{concepts_dir, documents_dir, explorations_dir};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use crate::VaultError;

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
    /// The subject is named in the page's declared title or aliases.
    Name,
    /// The subject is named in the page's opening statement — the first line of its first
    /// section, which is what every format leads with and what the catalog shows.
    Summary,
    /// The subject is named somewhere in the page's prose. Link destinations are not prose: a
    /// concept page carries one per citation, so matching them would make every page in the
    /// vault answer to `daily` or to any source id.
    Text,
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
    pub source_count: u64,
    pub matched: MatchField,
    pub excerpt: String,
    pub file_revision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_record: Option<serde_json::Value>,
}

/// Search knowledge pages, optionally including managed source and review pages.
/// Source inclusion is explicit so callers can retrieve evidence without requiring
/// a successful concept extraction first.
pub struct SearchOptions<'a> {
    pub query: Option<&'a str>,
    pub snapshot: Option<&'a str>,
    pub limit: usize,
    pub offset: usize,
    pub include_sources: bool,
    pub category: Option<&'a str>,
    pub source_status: Option<&'a str>,
    pub format: Option<&'a str>,
}

impl Default for SearchOptions<'_> {
    fn default() -> Self {
        Self {
            query: None,
            snapshot: None,
            limit: 20,
            offset: 0,
            include_sources: false,
            category: None,
            source_status: None,
            format: None,
        }
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
    pub categories: BTreeMap<String, usize>,
    pub source_statuses: BTreeMap<String, usize>,
    pub formats: BTreeMap<String, usize>,
}

pub fn search(
    vault_root: &Path,
    dirs: &VaultDirs,
    options: &SearchOptions<'_>,
) -> Result<SearchResult, VaultError> {
    let query = options.query.unwrap_or_default();
    let terms: Vec<String> = query.split_whitespace().map(fold).collect();
    if options.limit == 0 || (options.query.is_some() && terms.is_empty()) {
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
    let pages = load_pages(vault_root, dirs, options.include_sources)?;
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
    mut visit: impl FnMut(Page, usize) -> Result<(), VaultError>,
) -> Result<(), VaultError> {
    // Configured source roots can overlap the wiki (including filesystem aliases).
    // Navigation is not evidence merely because a source walk also reaches it.
    let reserved = crate::navigation::ReservedNavigationPaths::load(vault_root, dirs)?;
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
    let mut visited = std::collections::BTreeSet::new();
    for rel_dir in directories {
        let concept = rel_dir == concepts_dir(dirs);
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
            let page = crate::parse_page(&raw).map_err(VaultError::Frontmatter)?;
            if page.frontmatter.get("type").and_then(|v| v.as_str()) == Some("task-board") {
                continue;
            }
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
            let body_line_offset = raw.lines().count() - page.body.lines().count();
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
                    page,
                },
                body_line_offset,
            )?;
        }
    }

    Ok(())
}

pub(super) fn search_pages(
    options: &SearchOptions<'_>,
    pages: &[Page],
    spellings: &[String],
    terms: &[String],
    query_identity: Option<&str>,
) -> SearchResult {
    let mut candidates = Vec::new();
    let mut categories = BTreeMap::new();
    let mut source_statuses = BTreeMap::new();
    let mut formats = BTreeMap::new();
    for entry in pages {
        let names: Vec<&str> = entry.names().collect();
        let matched = if options.query.is_none() {
            Some(MatchField::Catalog)
        } else {
            match_page(terms, query_identity, spellings, &names, &entry.page.body)
        };
        let Some(matched) = matched else {
            continue;
        };
        let metadata = |key| {
            entry
                .page
                .frontmatter
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
        };
        *categories
            .entry(metadata("category").to_owned())
            .or_insert(0) += 1;
        *formats.entry(metadata("type").to_owned()).or_insert(0) += 1;
        let source_status = entry
            .page
            .frontmatter
            .get("source_record")
            .and_then(|record| record.get("status"))
            .and_then(|value| value.as_str())
            .unwrap_or("");
        *source_statuses.entry(source_status.to_owned()).or_insert(0) += 1;
        if !entry.matches_filters(options) {
            continue;
        }
        candidates.push((entry, matched));
    }

    candidates.sort_by(|(a, a_match), (b, b_match)| {
        if options.query.is_none() {
            return a.title.cmp(&b.title).then(a.rel_path.cmp(&b.rel_path));
        }
        a_match
            .cmp(b_match)
            .then(
                b.page
                    .frontmatter
                    .source_count()
                    .unwrap_or(0)
                    .cmp(&a.page.frontmatter.source_count().unwrap_or(0)),
            )
            .then(a.id.cmp(&b.id))
            .then(a.rel_path.cmp(&b.rel_path))
    });
    let total = candidates.len();
    // Excerpts strip and search the body again. Only the selected hits need one.
    let hits = candidates
        .into_iter()
        .skip(options.offset)
        .take(options.limit)
        .map(|(entry, matched)| SearchHit {
            excerpt: excerpt(&entry.page.body, matched, terms, spellings),
            file_revision: entry.file_revision.clone(),
            source_record: entry.page.frontmatter.get("source_record").cloned(),
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
            source_count: entry.page.frontmatter.source_count().unwrap_or(0),
            matched,
        })
        .collect::<Vec<_>>();
    let end = options.offset.saturating_add(hits.len());
    SearchResult {
        snapshot: options.snapshot.map(String::from),
        hits,
        total,
        offset: options.offset,
        next_offset: (end < total).then_some(end),
        categories,
        source_statuses,
        formats,
    }
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
    pub(super) page: lk_core::frontmatter::VaultPage,
}

impl Page {
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
        options
            .category
            .is_none_or(|value| metadata("category") == value)
            && options.format.is_none_or(|value| metadata("type") == value)
            && options.source_status.is_none_or(|value| status == value)
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
/// Expansion requires one owner under the search fold. Ambiguous folded names do not
/// borrow aliases. A declared alias may still occur in another sense or within a Korean
/// compound; such matches are retrieval candidates, not semantic identity decisions.
/// Excerpts expose the matched prose for the caller to assess.
pub(super) fn spellings_of(query_identity: Option<&str>, pages: &[Page]) -> Vec<String> {
    let Some(key) = query_identity else {
        return Vec::new();
    };
    let mut owners = pages.iter().filter(|p| p.concept).filter(|p| {
        p.names()
            .any(|name| identity_key(name).is_some_and(|name| fold(&name) == fold(key)))
    });
    let Some(owner) = owners.next() else {
        return Vec::new();
    };
    if owners.next().is_some() {
        return Vec::new();
    }
    owner
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
) -> Option<MatchField> {
    if let Some(key) = query_identity
        && names
            .iter()
            .any(|name| identity_key(name).as_deref() == Some(key))
    {
        return Some(MatchField::Identity);
    }
    let prose = search_text(body);
    let fields = [
        (
            MatchField::Name,
            names
                .iter()
                .map(|name| fold_name(name))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            MatchField::Summary,
            fold(&search_summary(body).unwrap_or_default()),
        ),
        (MatchField::Text, fold(&prose)),
    ];
    let field_of = |needle: &str| {
        fields
            .iter()
            .find(|(_, text)| contains_term(text, needle))
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
    let (Some(first), Some(last)) = (term.chars().next(), term.chars().last()) else {
        return false;
    };
    let open_anywhere = !first.is_ascii_alphanumeric();
    let close_anywhere = !last.is_ascii_alphanumeric();
    text.match_indices(term).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + term.len()..].chars().next();
        (open_anywhere || !before.is_some_and(|c| c.is_ascii_alphanumeric()))
            && (close_anywhere || !after.is_some_and(|c| c.is_ascii_alphanumeric()))
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
    // The same prose the match was taken over, so the line shown is the line that matched —
    // under whichever of the two routes reached the page, so a page found by another of the
    // concept's names shows the line carrying that name rather than the page's opening.
    let prose = search_text(body);
    let text = match matched {
        MatchField::Catalog | MatchField::Identity | MatchField::Name | MatchField::Summary => {
            search_summary(body)
        }
        MatchField::Text => prose
            .lines()
            .map(str::trim)
            .find(|line| {
                let folded = fold(line);
                (spellings.is_empty() && terms.iter().all(|t| contains_term(&folded, t)))
                    || spellings.iter().any(|n| contains_term(&folded, n))
            })
            .map(str::to_owned)
            .or_else(|| search_summary(body)),
    };
    text.map(|t| crate::index::truncate_summary(&t))
        .unwrap_or_default()
}

/// Visible text with one line per Markdown block. Inline formatting and soft breaks
/// do not split a phrase; paragraphs, list items and table cells do. Link addresses
/// and HTML markup are not prose. Code remains searchable as literal content.
pub(super) fn search_text(body: &str) -> String {
    let mut text = String::with_capacity(body.len());
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(body, options) {
        match event {
            Event::Text(value) | Event::Code(value) => text.push_str(&value),
            Event::SoftBreak | Event::HardBreak => text.push(' '),
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
            | Event::TaskListMarker(_) => {}
            _ => text.push('\n'),
        }
    }
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn search_summary(body: &str) -> Option<String> {
    search_text(first_section(body)?)
        .lines()
        .next()
        .map(str::to_owned)
}

pub(super) fn fold_name(text: &str) -> String {
    fold(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// What the page says it is: the body of its first `## ` section.
///
/// Read by POSITION, which is type- and locale-independent on purpose. A concept leads with
/// its synthesis, a document with its summary, an exploration with its question — the
/// section's name differs with both the format and `vault.locale`, its position with neither,
/// so nothing here has a table to keep in step with either.
fn first_section(body: &str) -> Option<&str> {
    let mut fence = lk_core::markdown::FenceState::new();
    for line in body.lines() {
        if fence.apply(line) || !fence.is_closed() {
            continue;
        }
        if let Some(heading) = line.trim_end().strip_prefix("## ") {
            return crate::section_body(body, heading);
        }
    }
    None
}

/// The page's opening statement: the first paragraph of [`first_section`].
///
/// A paragraph rather than a line, because a hard-wrapped page would otherwise have its
/// opening sentence split across two of them — the terms of a query landing on either side of
/// a break that carries no meaning.
pub(crate) fn opening_statement(body: &str) -> Option<String> {
    let mut paragraph: Vec<&str> = Vec::new();
    for line in first_section(body)?.lines().map(str::trim) {
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
    crate::page_path(root, dir.strip_prefix(root).map_err(std::io::Error::other)?)?;
    match std::fs::read_dir(dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    let mut files = Vec::new();
    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry.map_err(|error| std::io::Error::other(error.to_string()))?;
        if entry.file_type().is_file() && entry.path().extension().is_some_and(|e| e == "md") {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(result.categories["theory"], 1);
            assert_eq!(result.categories["practice"], 3);
            assert_eq!(result.formats["concept"], 1);
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
        assert_eq!(result.categories[""], 2);
        assert_eq!(result.categories["theory"], 1);
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
                    &format!("## Summary\n{body}")
                ),
                expected,
                "{body}"
            );
        }
        assert_eq!(
            match_page(&terms, None, &spellings, &["Knowledge", "Graph"], ""),
            None
        );
        assert_eq!(
            match_page(&terms, None, &spellings, &["Knowledge\t  Graph"], ""),
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
