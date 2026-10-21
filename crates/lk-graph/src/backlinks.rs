//! Re-derive every concept page's evidence-dependent state from the actual link graph —
//! this module is the single source-of-truth for all of it. A concept's evidence is the set
//! of pages that cite it. The bounded `## <Sources>` discovery instruction, frontmatter
//! `source_count`, and `llm_inputs.synthesis` input share this authority. Complete citations
//! are exposed through prepared discovery, never a second exhaustive list on every concept.
//! The input determines what its `## <Synthesis>` is owed against. Ingest writes none of
//! them — it renders the sources heading empty, preserves the on-disk count verbatim
//! (0 for a new page), and carries the
//! recorded input through unchanged — so this sync is what makes them correct. Manual edits
//! (a daily page gains or loses a concept link) and source-page deletions are reflected on
//! the next run.
//!
//! Pure set comparison: no heuristics, no LLM, no per-source rules. An eligible source
//! contributes evidence iff its body links resolve to the concept.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use lk_core::concept::{Citation, citation_digest};
use lk_core::config::VaultDirs;
use lk_core::frontmatter;
use lk_vault::{
    MutationPlan, MutationSession, clear_llm_input, record_llm_input, replace_section,
    resolve_section, set_frontmatter_field,
};
use serde::Serialize;

use crate::GraphError;
use crate::scan::{ScannedPage, VaultExistence, is_concept_page};

/// Outcome of one concept page's reconciliation.
#[derive(Debug, Clone, Serialize)]
pub struct ConceptUpdate {
    /// Vault-relative path of the concept page that was (or would be) updated.
    pub path: PathBuf,
    /// Exact additions when the prior complete legacy list is available; otherwise null.
    pub added: Option<Vec<String>>,
    /// Exact removals when the prior complete legacy list is available; otherwise null.
    pub removed: Option<Vec<String>>,
}

/// A concept page whose written synthesis was taken as the answer for the evidence it
/// carries.
#[derive(Debug, Clone, Serialize)]
pub struct AdoptedSynthesis {
    pub path: PathBuf,
    /// How many pages cite the concept.
    ///
    /// Carried because adoption is only obviously right at ONE: the prose was written from
    /// the single source that exists. Above one it is a deferral — a January sentence
    /// standing as the answer for a set spanning six months — and `lore doctor` reads the
    /// page as fully answered, so nothing else would ever say so. Naming the count turns a
    /// silent deferral into a list an operator can act on.
    pub citations: usize,
}

/// A concept page whose synthesis was written against a citation set the page no longer
/// carries — or against none at all. The work this sweep hands to the LLM queue.
#[derive(Debug, Clone, Serialize)]
pub struct ConceptResynthesis {
    /// Vault-relative path of the concept page.
    pub path: PathBuf,
    /// The synthesis heading as the page spells it, so a result lands where the page can
    /// receive it even when `vault.locale` has moved since the page was written.
    pub anchor: String,
    /// The related-concepts heading, spelled the same way, or `None` where the page carries
    /// none.
    ///
    /// A concept's relations answer to the same evidence its synthesis does — which other
    /// concepts the citing material relates this one to — so they are owed by the same move
    /// and written by the same act. Carrying the heading here is what lets one task fill both
    /// sections under one marker; without it the writer would have to resolve a heading from
    /// the locale table, which is the coupling `anchor` exists to remove. `None` where the
    /// page has no such section: `replace_section` leaves a page unchanged when the heading is
    /// absent, so a writer handed one would report a write that never happened.
    pub related_anchor: Option<String>,
    /// The pages citing this concept, sorted — the evidence the rewritten synthesis
    /// answers to.
    pub citations: Vec<Citation>,
    /// The bodies the rewrite will REPLACE — one entry per section it writes that holds
    /// one, in the order it writes them.
    ///
    /// Every other LLM-owned section names what a re-render is about to discard
    /// (`llm_cache::SectionDecision::discarding`), because a body somebody wrote without
    /// recording it is not recoverable once the page is rewritten — and these two are the
    /// sections most likely to hold exactly that, since a human or `/lore-wiki add` authors
    /// them at creation. One act writes both, so both are reported: naming only the
    /// synthesis left an authored relations body to be replaced in silence, which is the
    /// loss the report exists to prevent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub discarding: Vec<DiscardedBody>,
}

/// A section body a rewrite will replace, under the heading the page spells it with.
#[derive(Debug, Clone, Serialize)]
pub struct DiscardedBody {
    /// The heading as the page spells it, without the `## ` marker.
    pub heading: String,
    /// The body text, trimmed. The page is the only copy.
    pub body: String,
}

/// Outcome of a backlinks-sync run (a mutating operation), mirroring `MergeResult`:
/// domain-module operation outcomes are `*Result`; the CLI presentation wrapper is
/// `output::BacklinksSyncReport`.
#[derive(Debug, Default, Serialize)]
pub struct BacklinksSyncResult {
    /// Concept pages whose derived state differed and was rewritten (or would be,
    /// under `--dry-run`).
    pub updated: Vec<ConceptUpdate>,
    /// Count of concept pages whose derived state was already correct.
    pub unchanged: usize,
    /// Concept pages whose written synthesis was adopted as the answer for the evidence they
    /// carry, because they had recorded no input before this run. Reported so an upgrade says
    /// what it decided rather than leaving it to be discovered.
    pub adopted: Vec<AdoptedSynthesis>,
    /// Concept pages whose recorded synthesis input was withdrawn: nothing cites them any
    /// more, and the input was never answered, so the rewrite it promised can never be
    /// written. Reported because a queued task for one is about to stop being work.
    pub withdrawn: Vec<PathBuf>,
    /// Concept pages whose `## <Synthesis>` has not been written against the citation set
    /// the page now carries. Reported rather than acted on here: this crate computes what
    /// is true of the vault and never calls an LLM, so the caller is what turns the list
    /// into queued work.
    ///
    /// Independent of `updated` — a page whose sources were already correct can still be
    /// owed a synthesis, because writing the section is a separate act from deriving the
    /// evidence it answers to.
    pub resynthesize: Vec<ConceptResynthesis>,
    /// Concept pages that could not record their derived state: they carry no frontmatter
    /// block, one that will not parse, no heading to hold one of the sections, or no declared
    /// concept type where a new typed synthesis is required. Reported rather than silently
    /// skipped; these pages remain unchanged and the command exits non-zero while any remain.
    /// Skipped rather than fatal, because aborting mid-sweep leaves every page already written
    /// synced and every page after it not, and one corrupt page blocks it forever.
    ///
    /// Always serialized. This list IS the command's verdict, so omitting it when empty hides
    /// the field on exactly the clean runs a consumer sees most, where `undefined` reads as
    /// neither empty nor absent.
    pub skipped: Vec<PathBuf>,
    /// Cited concept pages carrying no synthesis heading. Their discovery body and count ARE
    /// written — the two facts are independent — but nothing can be owed against evidence the
    /// page has nowhere to answer.
    pub headless: Vec<PathBuf>,
    /// Custom Sources bodies retained verbatim while derived count/input reconcile.
    pub preserved_sources: Vec<PathBuf>,
    /// Whether this was a dry run (no writes were performed).
    pub dry_run: bool,
}

/// Recompute the `## <Sources>` section on every `{wiki}/concepts/{slug}.md` page from
/// the incoming links observed in `pages`, and rewrite any page whose section
/// differs. Returns the diff per page plus a count of pages that needed no change.
///
/// `dry_run = true` skips all writes but still reports what would change. Re-running
/// the command after a real run is a no-op: the sources block is generated by the
/// same routine that compares against it.
///
/// Takes no locale: the existing Sources heading selects the discovery instruction's
/// language. A page carrying no supported heading is skipped rather than given one.
/// Whether this sweep records the input a concept's synthesis is owed against.
///
/// The input is a promise that something will answer it: `lore doctor` reports a recorded
/// input with no matching answer, and nothing re-renders a concept page's markers from
/// scratch, so an input written where no drain can ever run is a finding that never clears.
/// A run with no LLM plane behind it therefore keeps the citation bookkeeping and writes no
/// promise — the same reason a concept nothing cites records nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthesisPolicy {
    Record,
    Skip,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum CitationTarget {
    Existing(PathBuf),
    Missing(String),
    Outside(String),
}

fn citation_context(
    root: &Path,
    canonical_root: &Path,
    page: &ScannedPage,
) -> Result<BTreeSet<CitationTarget>, GraphError> {
    page.outgoing
        .iter()
        .map(|link| {
            if link.id.is_none() {
                return Ok(CitationTarget::Outside(link.dest.clone()));
            }
            match root.join(&link.dest).canonicalize() {
                Ok(target) => {
                    if !target.starts_with(canonical_root) {
                        return Err(GraphError::Io(format!(
                            "citation target escapes vault: {}",
                            link.dest
                        )));
                    }
                    Ok(CitationTarget::Existing(target))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(CitationTarget::Missing(link.dest.clone()))
                }
                Err(error) => Err(GraphError::Io(error.to_string())),
            }
        })
        .collect()
}

/// Keep every scanned address for navigation, but count each verified source owner once.
/// Canonical paths collapse symlink spellings without collapsing distinct hard-link entries.
struct CitationSource<'a> {
    page: &'a ScannedPage,
    owner: PathBuf,
    aliases: Vec<&'a ScannedPage>,
}

fn citation_sources<'a>(
    pages: &'a [ScannedPage],
    root: &Path,
    dirs: &VaultDirs,
    existence: &VaultExistence,
) -> Result<Vec<CitationSource<'a>>, GraphError> {
    let canonical_root = root
        .canonicalize()
        .map_err(|e| GraphError::Io(e.to_string()))?;
    // Source role belongs to the verified owner, not whichever eligible alias exposes it.
    // Canonical paths collapse symlinks while keeping distinct hard-link entries distinct.
    let concept_owners = pages
        .iter()
        .filter(|page| {
            is_concept_page(&page.path, dirs) || page.format.as_deref() == Some("concept")
        })
        .map(|page| {
            lk_vault::page_path(root, &page.path)
                .and_then(std::fs::canonicalize)
                .map_err(|error| GraphError::Io(error.to_string()))
        })
        .collect::<Result<BTreeSet<_>, GraphError>>()?;
    let mut groups: BTreeMap<PathBuf, Vec<&ScannedPage>> = BTreeMap::new();
    for page in pages
        .iter()
        .filter(|page| crate::scan::is_citation_origin(page, dirs, existence))
    {
        let canonical = lk_vault::page_path(root, &page.path)
            .and_then(std::fs::canonicalize)
            .map_err(|e| GraphError::Io(e.to_string()))?;
        let owner = canonical
            .strip_prefix(&canonical_root)
            .map_err(|error| GraphError::Io(error.to_string()))?;
        if is_concept_page(owner, dirs) || concept_owners.contains(&canonical) {
            continue;
        }
        if page.revision.is_none() {
            return Err(GraphError::UnreadableSource(page.path.clone()));
        }
        groups.entry(canonical).or_default().push(page);
    }

    let mut sources = Vec::with_capacity(groups.len());
    for (canonical, mut aliases) in groups {
        aliases.sort_by(|a, b| a.path.cmp(&b.path));
        let first = aliases[0];
        if aliases.len() > 1 {
            let raw =
                std::fs::read_to_string(&canonical).map_err(|e| GraphError::Io(e.to_string()))?;
            let parsed = frontmatter::parse_page(&raw)
                .map_err(|_| GraphError::UnreadableSource(first.path.clone()))?;
            let revision = lk_core::concept::source_revision(&parsed);
            let first_destinations = citation_context(root, &canonical_root, first)?;
            for alias in &aliases {
                // Relative links can mean different things at two readable spellings. Choosing
                // one or unioning both would silently change the evidence attributed to this file.
                let alias_raw = std::fs::read_to_string(
                    lk_vault::page_path(root, &alias.path)
                        .map_err(|e| GraphError::Io(e.to_string()))?,
                )
                .map_err(|e| GraphError::Io(e.to_string()))?;
                if alias_raw != raw
                    || alias.revision.as_deref() != Some(revision.as_str())
                    || alias.rss_evidence != first.rss_evidence
                    || citation_context(root, &canonical_root, alias)? != first_destinations
                {
                    return Err(GraphError::Io(format!(
                        "source aliases have conflicting citation contexts: {} and {}",
                        first.path.display(),
                        alias.path.display()
                    )));
                }
            }
        }
        let direct = canonical
            .strip_prefix(&canonical_root)
            .map_err(|e| GraphError::Io(e.to_string()))?;
        sources.push(CitationSource {
            page: aliases
                .iter()
                .copied()
                .find(|page| page.path == direct)
                .unwrap_or(first),
            owner: canonical,
            aliases,
        });
    }
    sources.sort_by(|a, b| a.page.id.cmp(&b.page.id));
    Ok(sources)
}

fn incoming_citations(
    sources: &[CitationSource<'_>],
    existence: &VaultExistence,
) -> BTreeMap<String, BTreeSet<String>> {
    // Reverse index: concept page id → sorted set of source page ids that cite it.
    // Each link already carries its resolved page id (scan resolves every destination
    // against its page's location), so crediting is a set-membership check — source_count,
    // complete citation discovery and synthesis use this same set. Only non-concept content
    // pages count as sources (`is_valid_source`); a concept→concept link belongs in
    // `## Related`, and navigation pages aren't sources. BTreeMap/BTreeSet keep the
    // complete evidence projection deterministic.
    // `sources` contains one validated owner per eligible physical origin.
    let mut incoming: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for source in sources {
        let page = source.page;
        for target in page
            .outgoing
            .iter()
            .filter_map(|link| existence.reached(link))
        {
            // Self-references are excluded for the same reason the graph excludes
            // self-edges.
            if target != page.id && existence.is_knowledge(target) {
                incoming
                    .entry(target.to_owned())
                    .or_default()
                    .insert(page.id.clone());
            }
        }
    }

    incoming
}

fn rss_evidence(
    pages: &[ScannedPage],
    sources: &[CitationSource<'_>],
    root: &Path,
    dirs: &VaultDirs,
) -> Result<BTreeMap<String, BTreeMap<String, Vec<Citation>>>, GraphError> {
    let mut result = BTreeMap::new();
    for source in sources
        .iter()
        .map(|source| source.page)
        .filter(|page| page.rss_evidence)
    {
        let raw = std::fs::read_to_string(
            lk_vault::page_path(root, &source.path).map_err(|e| GraphError::Io(e.to_string()))?,
        )
        .map_err(|e| GraphError::Io(e.to_string()))?;
        let page = frontmatter::parse_page(&raw).map_err(|e| GraphError::Io(e.to_string()))?;
        if source.revision.as_deref() != Some(lk_core::concept::source_revision(&page).as_str()) {
            return Err(GraphError::Io("RSS source changed during scan".into()));
        }
        let evidence = lk_vault::rss_concept_citations(root, &source.path, &raw)
            .map_err(|e| GraphError::Io(e.to_string()))?;
        let mut resolved: BTreeMap<String, Vec<Citation>> = BTreeMap::new();
        for (concept, citations) in evidence {
            let direct = pages.iter().find(|p| {
                p.path == Path::new(&concept)
                    && is_concept_page(&p.path, dirs)
                    && p.format.as_deref() == Some("concept")
            });
            let matched = if direct.is_some() {
                direct
            } else {
                let full = lk_vault::page_path(root, Path::new(&concept))
                    .and_then(std::fs::canonicalize)
                    .map_err(|e| GraphError::Io(e.to_string()))?;
                let canonical_root =
                    std::fs::canonicalize(root).map_err(|e| GraphError::Io(e.to_string()))?;
                let relative = full
                    .strip_prefix(&canonical_root)
                    .map_err(|e| GraphError::Io(e.to_string()))?;
                pages.iter().find(|p| p.path == relative)
            };
            let matched = matched
                .filter(|p| {
                    is_concept_page(&p.path, dirs) && p.format.as_deref() == Some("concept")
                })
                .ok_or_else(|| {
                    GraphError::Io("RSS evidence concept is not a concept page".into())
                })?;
            resolved
                .entry(matched.path.to_string_lossy().into_owned())
                .or_default()
                .extend(citations);
        }
        for citations in resolved.values_mut() {
            citations.sort();
            citations.dedup();
        }
        result.insert(source.id.clone(), resolved);
    }
    Ok(result)
}

fn citations_for(
    sources: &[String],
    concept: &Path,
    by_id: &HashMap<&str, &ScannedPage>,
    rss: &BTreeMap<String, BTreeMap<String, Vec<Citation>>>,
) -> Vec<Citation> {
    sources
        .iter()
        .flat_map(|id| {
            let source = by_id[id.as_str()];
            rss.get(id)
                .and_then(|m| m.get(&concept.to_string_lossy().to_string()))
                .cloned()
                .unwrap_or_else(|| {
                    vec![Citation::at_revision(
                        &source.path,
                        source.revision.clone().expect("validated source revision"),
                    )]
                })
        })
        .collect()
}

/// The shared derivation used by reconciliation, resynthesis, audit and prepared discovery.
struct CitationGraph<'a> {
    incoming: BTreeMap<String, BTreeSet<String>>,
    rss: BTreeMap<String, BTreeMap<String, Vec<Citation>>>,
    by_id: HashMap<&'a str, &'a ScannedPage>,
    sources: BTreeMap<&'a str, CitationSource<'a>>,
}

impl<'a> CitationGraph<'a> {
    fn build(pages: &'a [ScannedPage], root: &Path, dirs: &VaultDirs) -> Result<Self, GraphError> {
        let existence = VaultExistence::build_at(root, pages, dirs)?;
        let sources = citation_sources(pages, root, dirs, &existence)?;
        Ok(Self {
            incoming: incoming_citations(&sources, &existence),
            rss: rss_evidence(pages, &sources, root, dirs)?,
            by_id: pages.iter().map(|page| (page.id.as_str(), page)).collect(),
            sources: sources
                .into_iter()
                .map(|source| (source.page.id.as_str(), source))
                .collect(),
        })
    }

    fn sources_for(&self, page: &ScannedPage) -> Vec<String> {
        self.incoming
            .get(&page.id)
            .map(|sources| sources.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn citations_for(&self, page: &ScannedPage, sources: &[String]) -> Vec<Citation> {
        let mut citations = citations_for(sources, &page.path, &self.by_id, &self.rss);
        citations.sort();
        citations.dedup();
        citations
    }

    fn concepts(
        &self,
        pages: &[ScannedPage],
        dirs: &VaultDirs,
    ) -> Vec<lk_vault::ConceptCitationSet> {
        pages
            .iter()
            .filter(|page| {
                is_concept_page(&page.path, dirs) && page.format.as_deref() == Some("concept")
            })
            .map(|page| {
                let sources = self.sources_for(page);
                lk_vault::ConceptCitationSet {
                    path: page.path.clone(),
                    source_count: sources.len(),
                    citations: self.citations_for(page, &sources),
                }
            })
            .collect()
    }
}

/// Derive the current evidence for all concepts in one graph pass, including empty sets.
pub fn concept_evidence(
    pages: &[ScannedPage],
    root: &Path,
    dirs: &VaultDirs,
) -> Result<BTreeMap<PathBuf, Vec<Citation>>, GraphError> {
    Ok(CitationGraph::build(pages, root, dirs)?
        .concepts(pages, dirs)
        .into_iter()
        .map(|concept| (concept.path, concept.citations))
        .collect())
}

/// Graph captures its whole existence universe independently of selected search scope.
/// Exact file revisions bind the bytes parsed by scan; canonical destinations bind the
/// filesystem resolution used by citation authority, including currently missing targets.
pub struct CapturedConceptCitations {
    root: PathBuf,
    dirs: VaultDirs,
    follow_links: bool,
    file_revisions: BTreeMap<PathBuf, Option<String>>,
    projection: lk_vault::ConceptCitationProjection,
}

impl CapturedConceptCitations {
    pub fn projection(&self) -> &lk_vault::ConceptCitationProjection {
        &self.projection
    }

    /// Revalidate graph inputs and their overlapping selected search bytes before SQLite
    /// publication. Search separately guards its own inventory, which can include addresses
    /// excluded by graph scan policy (for example hidden descendants).
    /// This detects drift; it does not freeze external editors after the final check.
    pub fn verify(&self, selected: &[(PathBuf, String)]) -> Result<(), GraphError> {
        for (path, revision) in selected {
            if let Some(captured) = self.file_revisions.get(path)
                && captured.as_ref() != Some(revision)
            {
                return Err(GraphError::Io(format!(
                    "search page differs from captured graph input: {}",
                    path.display()
                )));
            }
        }
        let current = capture_concept_citations(&self.root, &self.dirs, self.follow_links)?;
        if current.projection != self.projection {
            return Err(GraphError::Io(
                "graph evidence inputs or filesystem resolution changed during snapshot".into(),
            ));
        }
        Ok(())
    }
}

/// Capture the complete graph-derived projection without publishing or recovering anything.
pub fn capture_concept_citations(
    root: &Path,
    dirs: &VaultDirs,
    follow_links: bool,
) -> Result<CapturedConceptCitations, GraphError> {
    let root = root
        .canonicalize()
        .map_err(|error| GraphError::Io(error.to_string()))?;
    let scanned = crate::scan::scan_vault_with_revisions(&root, follow_links)?;
    let concepts =
        CitationGraph::build(&scanned.pages, &root, dirs)?.concepts(&scanned.pages, dirs);
    let mut addresses = BTreeSet::new();
    for page in &scanned.pages {
        addresses.insert(page.path.clone());
        addresses.extend(
            page.outgoing
                .iter()
                .filter(|link| link.id.is_some())
                .map(|link| PathBuf::from(&link.dest)),
        );
    }
    addresses.extend(
        lk_core::vault_path::RESERVED_WIKI_FILES
            .iter()
            .map(|name| Path::new(&dirs.wiki).join(name)),
    );
    let resolutions = addresses
        .into_iter()
        .map(|path| {
            let full = lk_vault::page_path(&root, &path)
                .map_err(|error| GraphError::Io(error.to_string()))?;
            let canonical = match full.canonicalize() {
                Ok(canonical) => Some(canonical),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(GraphError::Io(error.to_string())),
            };
            Ok((path, canonical))
        })
        .collect::<Result<BTreeMap<_, _>, GraphError>>()?;
    let identity = serde_json::to_string(&(
        &root,
        dirs,
        follow_links,
        &scanned.file_revisions,
        resolutions,
        &concepts,
    ))
    .map_err(|error| GraphError::Io(error.to_string()))?;
    Ok(CapturedConceptCitations {
        root,
        dirs: dirs.clone(),
        follow_links,
        file_revisions: scanned.file_revisions,
        projection: lk_vault::ConceptCitationProjection {
            input_revision: lk_core::concept::file_revision(&identity),
            concepts,
        },
    })
}

/// Derive one exact concept's current evidence and replacement sections without writes.
/// This does not depend on retained queue history or an unanswered completion marker.
pub fn concept_resynthesis(
    pages: &[ScannedPage],
    vault_root: &Path,
    dirs: &VaultDirs,
    path: &Path,
) -> Result<ConceptResynthesis, GraphError> {
    let fail = |message: &str| GraphError::Io(message.to_owned());
    lk_vault::page_path(vault_root, path).map_err(|e| fail(&e.to_string()))?;
    let page = pages
        .iter()
        .find(|p| p.path == path)
        .filter(|p| is_concept_page(&p.path, dirs) && p.format.as_deref() == Some("concept"))
        .ok_or_else(|| fail("expected an exact existing concept Markdown path"))?;
    let evidence = CitationGraph::build(pages, vault_root, dirs)?;
    let sources = evidence.sources_for(page);
    if sources.is_empty() {
        return Err(fail("concept has no current citations"));
    }
    let raw = std::fs::read_to_string(
        lk_vault::page_path(vault_root, path).map_err(|e| fail(&e.to_string()))?,
    )
    .map_err(|e| fail(&e.to_string()))?;
    frontmatter::parse_page(&raw).map_err(|e| fail(&e.to_string()))?;
    let section = resolve_section(&raw, |s| s.concept_synthesis)
        .ok_or_else(|| fail("concept lacks synthesis heading"))?;
    if resolve_section(&raw, |s| s.concept_sources).is_none() {
        return Err(fail("concept lacks sources heading"));
    }
    let related = resolve_section(&raw, |s| s.related);
    Ok(ConceptResynthesis {
        path: path.to_path_buf(),
        anchor: format!("## {}", section.heading),
        related_anchor: related.as_ref().map(|s| format!("## {}", s.heading)),
        citations: evidence.citations_for(page, &sources),
        discarding: [Some(&section), related.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let body = s.body.trim();
                (!body.is_empty()).then(|| DiscardedBody {
                    heading: s.heading.to_owned(),
                    body: body.to_owned(),
                })
            })
            .collect(),
    })
}

pub fn sync_concept_backlinks(
    pages: &[ScannedPage],
    vault_root: &Path,
    dry_run: bool,
    dirs: &VaultDirs,
    synthesis: SynthesisPolicy,
) -> Result<BacklinksSyncResult, GraphError> {
    if dry_run {
        let (_, mut report) = plan_concept_backlinks(pages, vault_root, dirs, synthesis)?;
        report.dry_run = true;
        return Ok(report);
    }
    let session =
        MutationSession::acquire(vault_root).map_err(|error| GraphError::Io(error.to_string()))?;
    sync_concept_backlinks_in_session(pages, &session, dirs, synthesis)
}

/// Publish one complete guarded reconciliation under an already acquired writer session.
pub fn sync_concept_backlinks_in_session(
    pages: &[ScannedPage],
    session: &MutationSession,
    dirs: &VaultDirs,
    synthesis: SynthesisPolicy,
) -> Result<BacklinksSyncResult, GraphError> {
    let (plan, report) = plan_concept_backlinks(pages, session.root(), dirs, synthesis)?;
    session
        .apply(plan)
        .map_err(|error| GraphError::Io(error.to_string()))?;
    Ok(report)
}

/// Derive a complete before/after plan without writing, locking or recovering journals.
/// All eligible sources retain their scanned authored revisions and canonical direct owners.
pub fn plan_concept_backlinks(
    pages: &[ScannedPage],
    vault_root: &Path,
    dirs: &VaultDirs,
    synthesis: SynthesisPolicy,
) -> Result<(MutationPlan, BacklinksSyncResult), GraphError> {
    plan_concept_backlinks_inner(pages, vault_root, dirs, synthesis, false)
}

/// Retain unchanged owed targets and evidence when a caller also publishes derived work.
pub fn plan_concept_backlinks_for_work(
    pages: &[ScannedPage],
    vault_root: &Path,
    dirs: &VaultDirs,
    synthesis: SynthesisPolicy,
) -> Result<(MutationPlan, BacklinksSyncResult), GraphError> {
    plan_concept_backlinks_inner(pages, vault_root, dirs, synthesis, true)
}

fn plan_concept_backlinks_inner(
    pages: &[ScannedPage],
    vault_root: &Path,
    dirs: &VaultDirs,
    synthesis: SynthesisPolicy,
    guard_work: bool,
) -> Result<(MutationPlan, BacklinksSyncResult), GraphError> {
    let evidence = CitationGraph::build(pages, vault_root, dirs)?;
    let canonical_root = vault_root
        .canonicalize()
        .map_err(|error| GraphError::Io(error.to_string()))?;
    let mut targets = BTreeMap::<PathBuf, (String, String)>::new();
    let mut report = BacklinksSyncResult::default();

    for page in pages {
        if !is_concept_page(&page.path, dirs) {
            continue;
        }

        if page.id.is_empty() {
            continue;
        }

        // Sources for this concept = eligible source owners whose links reach it.
        let sources = evidence.sources_for(page);

        let citations = evidence.citations_for(page, &sources);

        let full_path = vault_root.join(&page.path);
        let raw = std::fs::read_to_string(&full_path)
            .map_err(|e| GraphError::Io(format!("failed to read {}: {e}", full_path.display())))?;

        // The page was read successfully; a frontmatter that will not parse here is real
        // corruption, and every write below would have to guess what it is replacing.
        // Record and skip the malformed page rather than inventing its metadata or
        // blocking every independently readable concept in the reconciliation plan.
        let Ok(parsed) = frontmatter::parse_page(&raw) else {
            report.skipped.push(page.path.clone());
            continue;
        };
        if !parsed.frontmatter.is_concept_or_legacy() {
            continue;
        }

        // Headings are found under ANY locale (a page authored before a `vault.locale` switch
        // keeps its old-language spelling) and rewritten under the one the page actually
        // carries — mirrors the pipeline's all-locale `capture_section`.
        //
        // Preserve the page-format contract: a missing Sources heading is reported/skipped,
        // rather than inventing a section while reconciling its derived bookkeeping.
        let Some(sources_section) = resolve_section(&raw, |s| s.concept_sources) else {
            report.skipped.push(page.path.clone());
            continue;
        };

        let sources_body = crate::sources::classify(sources_section.body, &page.path);
        let existing_set = match &sources_body {
            crate::sources::SourcesBody::Legacy(sources) => {
                Some(sources.iter().map(String::as_str).collect::<BTreeSet<_>>())
            }
            crate::sources::SourcesBody::Compact | crate::sources::SourcesBody::Authored => None,
        };
        let desired_set: BTreeSet<&str> = sources.iter().map(String::as_str).collect();

        // `source_count` is re-derived here too (ingest preserves the on-disk value but never
        // computes it): the authoritative count is the number of incoming citations, the same
        // set exposed by exact citation discovery.
        let desired_count = sources.len() as u64;

        // The digest of that same set is what the page's synthesis is owed against, and
        // recording it is this sweep's job for the same reason the count is: it is the one
        // place that knows which pages cite this concept.
        //
        // A concept NOTHING cites has no evidence for a synthesis to answer to, so it records
        // no input and is owed nothing — there would be nothing for a drain to read. A concept
        // whose last citation was deleted keeps the input it ANSWERED: its synthesis still
        // states what the vault learned, and no rewrite could improve on it from a set that is
        // now empty.
        //
        // An input it never answered is the opposite case and must be WITHDRAWN. It is a
        // promise that something will write that section from those citations, and the
        // citations are gone — so the queued task keeps classifying `current` against the
        // input still on the page, and a drain reaching it writes a synthesis of pages the
        // vault no longer holds, stamped as answered. Nothing later can clear it: the sweep
        // that recorded the promise is the only thing that knows it was made.
        //
        // The synthesis heading is required only where a synthesis is OWED. Requiring it
        // unconditionally would let a page missing that one section keep a citation count the
        // graph contradicts, forever, and hold the sweep's exit code non-zero with it — a
        // page nothing cites owes no synthesis and is still owed a correct count of zero.
        let synthesis_section = resolve_section(&raw, |s| s.concept_synthesis);
        let owed_input = (synthesis == SynthesisPolicy::Record
            && !sources.is_empty()
            && synthesis_section.is_some())
        .then(|| citation_digest(&citations));
        let completion_key = frontmatter::field::completion(frontmatter::field::SYNTHESIS);
        let recorded_answer = llm_input(&parsed, &completion_key);
        let recorded_input = llm_input(&parsed, frontmatter::field::SYNTHESIS);
        // Keyed on the citation set going EMPTY, never on nothing being owed. Nothing is owed
        // for three different reasons — no citations, no LLM plane behind this run, no
        // synthesis heading to answer in — and only the first says the promise can never be
        // kept. Under `Skip` a fully cited page would have its input stripped and the queued
        // task killed by a run whose whole point is to touch no LLM plane; a page with no
        // heading is already handled by the queue's own anchor check, and withdrawing it here
        // would have one run print that nothing cites a page while the next line reports it as
        // cited.
        let withdrawing =
            sources.is_empty() && recorded_input.is_some() && recorded_answer != recorded_input;

        // A page that carries a written synthesis and has never recorded an input is one
        // somebody WROTE — by hand, by `/lore-wiki add`, or as the grounding sentence of the
        // extraction that created it — before anything tracked what it answers to. Its prose
        // is adopted as the answer for the evidence the page has today, not queued for
        // replacement.
        //
        // The alternative is what an upgrade would otherwise do: every cited concept is owed
        // at once, the drain REWRITES rather than appends, and the whole authored corpus is
        // replaced in the first unattended run — irreversibly, for a vault not in git. No
        // amount of warning printed to a scheduler's log undoes that, because the rewrite
        // follows seconds later in the same run.
        //
        // Adoption costs nothing that is not recovered: the next time the citation set MOVES
        // the page is owed a rewrite like any other. An explicit rewrite from current
        // citations is requested through `lore graph resynthesize <path>`.
        // What is adopted has to BE an answer. A section holding only a heading, or only a
        // `> [!conflict]` callout, is a marker somebody left rather than a statement about the
        // concept — and adopting a lone callout is the worst of the two, because `graph lint`
        // then reports an open disagreement that nothing is scheduled to restate or resolve.
        let adopting = owed_input.is_some()
            && recorded_answer.is_none()
            && llm_input(&parsed, frontmatter::field::SYNTHESIS).is_none()
            && synthesis_section.is_some_and(|section| carries_an_answer(section.body));

        // Deriving the evidence and writing the section that answers to it are separate acts,
        // so the two questions are asked separately: a page whose sources list is already
        // correct can still be owed a synthesis, and asking them together would leave it owed
        // forever.
        let mut requires_synthesis = false;
        if let (Some(digest), Some(section)) = (&owed_input, &synthesis_section)
            && !adopting
            && recorded_answer != Some(digest.as_str())
        {
            // Legacy pages support discovery/bookkeeping and adoption of authored prose,
            // but typed synthesis publication requires an explicit concept declaration.
            // Do not record a promise whose queued result cannot apply to this target.
            if !parsed.frontmatter.is_declared_concept() {
                report.skipped.push(page.path.clone());
                continue;
            }
            requires_synthesis = true;
            let related = resolve_section(&raw, |s| s.related);
            report.resynthesize.push(ConceptResynthesis {
                path: page.path.clone(),
                anchor: format!("## {}", section.heading),
                related_anchor: related
                    .as_ref()
                    .map(|section| format!("## {}", section.heading)),
                citations,
                discarding: [Some(section), related.as_ref()]
                    .into_iter()
                    .flatten()
                    .filter_map(|section| {
                        let body = section.body.trim();
                        (!body.is_empty()).then(|| DiscardedBody {
                            heading: section.heading.to_string(),
                            body: body.to_string(),
                        })
                    })
                    .collect(),
            });
        }

        // A cited page with nowhere to put a synthesis is a defect of its own: it can record
        // its citations and cannot record what they are owed. Reported so the count still
        // lands and the missing section is named, rather than one blocking the other.
        if !sources.is_empty() && synthesis_section.is_none() {
            report.headless.push(page.path.clone());
        }
        if adopting {
            report.adopted.push(AdoptedSynthesis {
                path: page.path.clone(),
                citations: sources.len(),
            });
        }
        if withdrawing {
            report.withdrawn.push(page.path.clone());
        }

        // A page with no frontmatter block, or none carrying the `llm_inputs:` mapping, has
        // nowhere to record what this sweep derives. Skipped like the corrupt case above, and
        // the command still exits non-zero — including for the synthesis it can no longer be
        // owed, since a task naming a page that cannot receive the result would fail every
        // drain forever.
        let sources_content = if sources_body == crate::sources::SourcesBody::Authored {
            report.preserved_sources.push(page.path.clone());
            raw.clone()
        } else {
            let locale = lk_core::i18n::Locale::ALL
                .iter()
                .find(|locale| locale.strings().concept_sources == sources_section.heading)
                .expect("resolved source heading belongs to a known locale");
            replace_section(
                &raw,
                sources_section.heading,
                crate::sources::render(*locale),
            )
        };
        let Some(updated_content) = set_source_count(&sources_content, desired_count).and_then(
            |content| match &owed_input {
                None if withdrawing => clear_llm_input(&content, frontmatter::field::SYNTHESIS),
                None => Some(content),
                Some(digest) => {
                    let stamp =
                        serde_json::to_string(digest).expect("a hex digest always serializes");
                    let recorded =
                        record_llm_input(&content, frontmatter::field::SYNTHESIS, &stamp)?;
                    match adopting {
                        // Adoption stamps BOTH: the input the prose is taken to answer, and the
                        // answer itself. One without the other is the unanswered state `lore
                        // doctor` reports.
                        true => record_llm_input(&recorded, &completion_key, &stamp),
                        false => Some(recorded),
                    }
                }
            },
        ) else {
            // Every decision this page was going to appear under comes back out: the write
            // did not happen, so the report must not announce one. Leaving `adopted` behind
            // had the run claim a page's prose was accepted as its answer while the page was
            // untouched — and say so beside the line reporting it skipped, on every run.
            report.skipped.push(page.path.clone());
            report.resynthesize.retain(|owed| owed.path != page.path);
            report.headless.retain(|path| path != &page.path);
            report.adopted.retain(|adopted| adopted.path != page.path);
            report.withdrawn.retain(|path| path != &page.path);
            continue;
        };

        // The complete derived page includes projection ownership and evidence revisions.
        let changed = updated_content != raw;
        if !changed {
            report.unchanged += 1;
            if !guard_work || !requires_synthesis {
                continue;
            }
        }

        // Preserve the publication rule that leaf symlinks cannot be edited. Directory
        // aliases share one direct owner; distinct hard-link entries remain distinct.
        let full = lk_vault::writable_page_path(vault_root, &page.path)
            .and_then(std::fs::canonicalize)
            .map_err(|error| GraphError::Io(error.to_string()))?;
        let owner = full
            .strip_prefix(&canonical_root)
            .map_err(|error| GraphError::Io(error.to_string()))?
            .to_path_buf();
        if let Some((before, after)) = targets.get(&owner) {
            if before != &raw || after != &updated_content {
                return Err(GraphError::Io(format!(
                    "concept aliases have conflicting backlink plans: {}",
                    page.path.display()
                )));
            }
        } else {
            targets.insert(owner, (raw.clone(), updated_content));
        }

        if changed {
            report.updated.push(ConceptUpdate {
                path: page.path.clone(),
                added: existing_set.as_ref().map(|existing| {
                    desired_set
                        .difference(existing)
                        .map(|s| (*s).to_owned())
                        .collect()
                }),
                removed: existing_set.as_ref().map(|existing| {
                    existing
                        .difference(&desired_set)
                        .map(|s| (*s).to_owned())
                        .collect()
                }),
            });
        }
    }

    let mut plan = MutationPlan::default();
    guard_backlink_sources(&mut plan, vault_root, &canonical_root, &evidence)?;
    if targets.is_empty() {
        // Validation still detects stale scans, but an unchanged projection has nothing
        // to publish and needs no dependency-only journal or per-source fsyncs.
        return Ok((MutationPlan::default(), report));
    }
    for (path, (before, after)) in targets {
        if before == after {
            plan.require_unchanged(path, before);
        } else {
            plan.edit(path, before, after);
        }
    }
    Ok((plan, report))
}

fn guard_backlink_sources(
    plan: &mut MutationPlan,
    root: &Path,
    canonical_root: &Path,
    evidence: &CitationGraph<'_>,
) -> Result<(), GraphError> {
    // Sources with no old incoming links are negative dependencies too: adding a
    // citation after scanning must not publish a stale empty set or source count.
    for source in evidence.sources.values() {
        let raw = std::fs::read_to_string(&source.owner)
            .map_err(|error| GraphError::Io(error.to_string()))?;
        let page = frontmatter::parse_page(&raw)
            .map_err(|_| GraphError::UnreadableSource(source.page.path.clone()))?;
        if source.page.revision.as_deref()
            != Some(lk_core::concept::source_revision(&page).as_str())
        {
            return Err(GraphError::Io(format!(
                "backlink source changed since scan: {}",
                source.page.path.display()
            )));
        }
        for alias in &source.aliases {
            let full = lk_vault::page_path(root, &alias.path)
                .map_err(|error| GraphError::Io(error.to_string()))?;
            if full
                .canonicalize()
                .map_err(|error| GraphError::Io(error.to_string()))?
                != source.owner
                || std::fs::read_to_string(&full)
                    .map_err(|error| GraphError::Io(error.to_string()))?
                    != raw
            {
                return Err(GraphError::Io(format!(
                    "backlink source alias changed during planning: {}",
                    alias.path.display()
                )));
            }
        }
        let owner = source
            .owner
            .strip_prefix(canonical_root)
            .map_err(|error| GraphError::Io(error.to_string()))?;
        if source.page.rss_evidence {
            lk_vault::guard_rss_evidence(plan, root, &source.page.path, &raw)
                .map_err(|error| GraphError::Io(error.to_string()))?;
        }
        plan.require_unchanged(owner, raw);
    }
    Ok(())
}

/// Whether a section body says something about the concept, rather than only marking it.
///
/// Actual headings and explicit callout blocks annotate the section. Ordinary quotes,
/// literal code and other authored content remain answers; this makes no support judgment.
fn carries_an_answer(body: &str) -> bool {
    use pulldown_cmark::{Event, Tag};

    let markdown = lk_core::markdown::MarkdownInput::from_text(body);
    let mut ignored_until = 0;
    for (event, range) in markdown.offset_events(lk_core::markdown::OPTIONS) {
        if range.start < ignored_until {
            continue;
        }
        match event {
            Event::Start(Tag::Heading { .. }) => ignored_until = range.end,
            Event::Start(Tag::BlockQuote(_)) => {
                let first = lk_core::markdown::lines_with_endings(&body[range.clone()])
                    .next()
                    .unwrap_or_default();
                // A nested callout is not the enclosing ordinary quote's annotation.
                if crate::concept_lint::parse_callout(first).is_some_and(|(depth, _, _)| depth == 1)
                {
                    ignored_until = range.end;
                }
            }
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text)
                if !text.trim().is_empty() =>
            {
                return true;
            }
            Event::Start(Tag::Image { .. }) => return true,
            _ => {}
        }
    }
    false
}

/// The value recorded under `llm_inputs.<key>`, if the page carries one.
fn llm_input<'a>(page: &'a frontmatter::VaultPage, key: &str) -> Option<&'a str> {
    page.frontmatter
        .get(frontmatter::field::LLM_INPUTS)
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_str())
}

/// Set the frontmatter `source_count` — a thin wrapper over the single-sourced
/// `set_frontmatter_field` so backlinks and the audit marker share one writer.
fn set_source_count(content: &str, count: u64) -> Option<String> {
    set_frontmatter_field(
        content,
        frontmatter::field::SOURCE_COUNT,
        &count.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Link;
    use lk_vault::section_body;
    use tempfile::TempDir;

    fn fixture_source_revision(id: &str) -> String {
        lk_core::concept::source_revision(&frontmatter::parse_page(&format!("# {id}\n")).unwrap())
    }

    fn fixture_digest(ids: &[String]) -> String {
        citation_digest(
            &ids.iter()
                .map(|id| Citation {
                    rss_event: None,
                    path: format!("{id}.md"),
                    revision: fixture_source_revision(id),
                })
                .collect::<Vec<_>>(),
        )
    }

    // A ScannedPage asserts that its address was enumerated on disk. Older
    // fixtures wrote only the concept being edited; make the source addresses
    // real as well now that production validates filesystem alias ownership.
    fn sync_fixture(
        pages: &[ScannedPage],
        root: &Path,
        dry_run: bool,
        dirs: &VaultDirs,
        synthesis: SynthesisPolicy,
    ) -> Result<BacklinksSyncResult, GraphError> {
        for page in pages {
            let path = root.join(&page.path);
            if !path.exists() {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, format!("# {}\n", page.id)).unwrap();
            }
        }
        super::sync_concept_backlinks(pages, root, dry_run, dirs, synthesis)
    }

    fn build_page(id: &str, rel: &str, outgoing: &[&str]) -> ScannedPage {
        ScannedPage {
            rss_evidence: false,
            revision: Some(fixture_source_revision(id)),
            id: id.to_owned(),
            path: PathBuf::from(rel),
            title: id.to_owned(),
            format: None,
            outgoing: outgoing
                .iter()
                .map(|s| Link::to(&format!("{s}.md")))
                .collect(),
        }
    }

    fn write_owner_test_concept(root: &Path) -> PathBuf {
        let path = PathBuf::from("wiki/concepts/topic.md");
        std::fs::create_dir_all(root.join(path.parent().unwrap())).unwrap();
        std::fs::write(root.join(&path), "---\ntype: concept\ntitle: Topic\nsource_count: 0\n---\n## Synthesis\n\n## Sources\n\n## Related\n").unwrap();
        path
    }

    fn build_guarded_fixture() -> (TempDir, VaultDirs, Vec<ScannedPage>) {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let topic = write_owner_test_concept(root.path());
        std::fs::copy(
            root.path().join(topic),
            root.path().join("wiki/concepts/other.md"),
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("daily")).unwrap();
        std::fs::write(root.path().join("daily/report.md"), "---\ntype: daily\n---\n[Topic](../wiki/concepts/topic.md)\n[Other](../wiki/concepts/other.md)\n").unwrap();
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        (root, dirs, pages)
    }

    #[test]
    fn guarded_backlink_plan_rejects_target_or_used_source_edits_before_any_write() {
        for changed in ["wiki/concepts/topic.md", "daily/report.md"] {
            let (root, dirs, pages) = build_guarded_fixture();
            let session = MutationSession::acquire(root.path()).unwrap();
            let first =
                std::fs::read_to_string(root.path().join("wiki/concepts/other.md")).unwrap();
            let second =
                std::fs::read_to_string(root.path().join("wiki/concepts/topic.md")).unwrap();
            let (plan, report) =
                plan_concept_backlinks(&pages, root.path(), &dirs, SynthesisPolicy::Record)
                    .unwrap();
            assert_eq!(report.updated.len(), 2);
            let external = format!(
                "{}\nExternal edit.\n",
                std::fs::read_to_string(root.path().join(changed)).unwrap()
            );
            std::fs::write(root.path().join(changed), &external).unwrap();
            let error = session.apply(plan).unwrap_err();
            assert!(error.to_string().contains("mutation conflict"), "{error}");
            assert_eq!(
                std::fs::read_to_string(root.path().join("wiki/concepts/other.md")).unwrap(),
                first,
                "a later target conflict must not publish the earlier target"
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join("wiki/concepts/topic.md")).unwrap(),
                if changed.ends_with("topic.md") {
                    external.clone()
                } else {
                    second
                }
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join(changed)).unwrap(),
                external
            );
            assert!(
                !root
                    .path()
                    .join(".lorekeeper/mutation/pending.json")
                    .exists()
            );
        }
    }

    #[test]
    fn zero_link_source_changes_are_guarded_before_planning_and_publication() {
        for after_plan in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let dirs = VaultDirs::default();
            let target = write_owner_test_concept(root.path());
            let before = std::fs::read_to_string(root.path().join(&target)).unwrap();
            std::fs::create_dir_all(root.path().join("daily")).unwrap();
            let source = root.path().join("daily/quiet.md");
            std::fs::write(&source, "---\ntype: daily\n---\nNo citations yet.\n").unwrap();
            let pages = crate::scan::scan_vault(root.path(), false).unwrap();
            let external = "---\ntype: daily\n---\n[Topic](../wiki/concepts/topic.md)\n";
            if after_plan {
                let session = MutationSession::acquire(root.path()).unwrap();
                let (plan, report) =
                    plan_concept_backlinks(&pages, root.path(), &dirs, SynthesisPolicy::Record)
                        .unwrap();
                assert_eq!(report.updated.len(), 1);
                std::fs::write(&source, external).unwrap();
                let error = session.apply(plan).unwrap_err();
                assert!(error.to_string().contains("mutation conflict"), "{error}");
                assert!(
                    !root
                        .path()
                        .join(".lorekeeper/mutation/pending.json")
                        .exists()
                );
            } else {
                std::fs::write(&source, external).unwrap();
                let error =
                    plan_concept_backlinks(&pages, root.path(), &dirs, SynthesisPolicy::Record)
                        .err()
                        .unwrap();
                assert!(
                    error.to_string().contains("source changed since scan"),
                    "{error}"
                );
                assert!(!root.path().join(".lorekeeper").exists());
            }
            assert_eq!(
                std::fs::read_to_string(root.path().join(&target)).unwrap(),
                before
            );
            assert_eq!(std::fs::read_to_string(&source).unwrap(), external);
        }
    }

    #[test]
    fn unchanged_backlink_plans_validate_all_sources_without_publication_steps() {
        let (root, dirs, pages) = build_guarded_fixture();
        sync_concept_backlinks(&pages, root.path(), false, &dirs, SynthesisPolicy::Record).unwrap();
        let source = root.path().join("daily/quiet.md");
        std::fs::write(&source, "---\ntype: daily\n---\nNo citations yet.\n").unwrap();
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        let (plan, report) =
            plan_concept_backlinks(&pages, root.path(), &dirs, SynthesisPolicy::Record).unwrap();
        assert_eq!(report.unchanged, 2);
        assert!(
            serde_json::to_value(plan).unwrap()["steps"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        std::fs::write(
            &source,
            "---\ntype: daily\n---\n[Topic](../wiki/concepts/topic.md)\n",
        )
        .unwrap();
        let error = plan_concept_backlinks(&pages, root.path(), &dirs, SynthesisPolicy::Record)
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("source changed since scan"),
            "{error}"
        );
    }

    #[test]
    fn unchanged_backlink_work_plan_guards_targets_and_sources() {
        for changed in ["wiki/concepts/topic.md", "daily/report.md"] {
            let (root, dirs, pages) = build_guarded_fixture();
            sync_concept_backlinks(&pages, root.path(), false, &dirs, SynthesisPolicy::Record)
                .unwrap();
            let pages = crate::scan::scan_vault(root.path(), false).unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let (mut plan, report) = plan_concept_backlinks_for_work(
                &pages,
                root.path(),
                &dirs,
                SynthesisPolicy::Record,
            )
            .unwrap();
            assert!(report.updated.is_empty());
            assert_eq!(report.resynthesize.len(), 2);
            plan.create("work.json", "pending work".into());
            let prior = std::fs::read_to_string(root.path().join(changed)).unwrap();
            std::fs::write(
                root.path().join(changed),
                format!("{prior}\nExternal change.\n"),
            )
            .unwrap();
            assert!(session.apply(plan).is_err());
            assert!(!root.path().join("work.json").exists());
            assert!(
                !root
                    .path()
                    .join(".lorekeeper/mutation/pending.json")
                    .exists()
            );
        }
    }

    #[test]
    fn backlink_planning_refuses_a_newer_source_baseline_and_preview_never_recovers() {
        let (root, dirs, pages) = build_guarded_fixture();
        let target = root.path().join("wiki/concepts/topic.md");
        let before = std::fs::read_to_string(&target).unwrap();
        let source = root.path().join("daily/report.md");
        let original = std::fs::read_to_string(&source).unwrap();
        std::fs::write(&source, format!("{original}\nChanged after scan.\n")).unwrap();
        let error = plan_concept_backlinks(&pages, root.path(), &dirs, SynthesisPolicy::Record)
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("source changed since scan"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
        assert!(!root.path().join(".lorekeeper").exists());
        std::fs::write(&source, original).unwrap();
        let _session = MutationSession::acquire(root.path()).unwrap();
        let pending = root.path().join(".lorekeeper/mutation/pending.json");
        std::fs::write(&pending, "invalid pending journal").unwrap();
        let preview =
            sync_concept_backlinks(&pages, root.path(), true, &dirs, SynthesisPolicy::Record)
                .unwrap();
        assert!(preview.dry_run);
        assert_eq!(preview.updated.len(), 2);
        assert_eq!(
            std::fs::read_to_string(&pending).unwrap(),
            "invalid pending journal"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
    }

    #[test]
    fn backlink_publication_recovers_the_complete_guarded_plan_and_is_idempotent() {
        let (root, dirs, pages) = build_guarded_fixture();
        let session = MutationSession::acquire(root.path()).unwrap();
        let (plan, _) =
            plan_concept_backlinks(&pages, root.path(), &dirs, SynthesisPolicy::Record).unwrap();
        let serialized = serde_json::to_value(&plan).unwrap();
        let first = serialized["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["path"] == "wiki/concepts/other.md")
            .unwrap();
        // A crash after one write retains the complete plan, not just the completed page.
        std::fs::write(
            root.path().join("wiki/concepts/other.md"),
            first["after"].as_str().unwrap(),
        )
        .unwrap();
        let pending = root.path().join(".lorekeeper/mutation/pending.json");
        std::fs::write(
            &pending,
            serde_json::to_vec(&serde_json::json!({"version":1,"plan":serialized})).unwrap(),
        )
        .unwrap();
        drop(session);
        let session = MutationSession::acquire(root.path()).unwrap();
        assert!(!pending.exists());
        for path in ["wiki/concepts/other.md", "wiki/concepts/topic.md"] {
            let raw = std::fs::read_to_string(root.path().join(path)).unwrap();
            assert!(raw.contains("source_count: 1"));
            assert_compact_sources(&raw, "Sources");
        }
        let same =
            sync_concept_backlinks_in_session(&pages, &session, &dirs, SynthesisPolicy::Record)
                .unwrap();
        assert_eq!(same.unchanged, 2);
        assert!(same.updated.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn source_aliases_cannot_turn_concept_relations_into_evidence() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = write_owner_test_concept(root.path());
        let source = Path::new("wiki/concepts/source.md");
        // The configured concept address establishes its role even for legacy pages
        // that lack a declared type. An eligible symlink cannot change that role.
        std::fs::write(root.path().join(source), "---\ntitle: Source\nsource_count: 0\n---\n## Synthesis\n\n## Sources\n\n## Related\n[Topic](/wiki/concepts/topic.md)\n").unwrap();
        std::fs::create_dir_all(root.path().join("daily")).unwrap();
        std::os::unix::fs::symlink(
            "../wiki/concepts/source.md",
            root.path().join("daily/source.md"),
        )
        .unwrap();
        // A declared concept outside that collection is also never source evidence.
        std::fs::write(
            root.path().join("daily/concept.md"),
            "---\ntype: concept\n---\n[Topic](/wiki/concepts/topic.md)\n",
        )
        .unwrap();
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        assert!(concept_evidence(&pages, root.path(), &dirs).unwrap()[&concept].is_empty());
        let applied =
            sync_concept_backlinks(&pages, root.path(), false, &dirs, SynthesisPolicy::Record)
                .unwrap();
        assert_eq!(applied.updated.len(), 2);
        assert!(applied.resynthesize.is_empty());
        for path in [&concept, &source.to_path_buf()] {
            let raw = std::fs::read_to_string(root.path().join(path)).unwrap();
            assert!(raw.contains("source_count: 0"));
            assert_compact_sources(&raw, "Sources");
        }
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        assert!(concept_evidence(&pages, root.path(), &dirs).unwrap()[&concept].is_empty());
        let captured = capture_concept_citations(root.path(), &dirs, false).unwrap();
        assert!(
            captured
                .projection()
                .concepts
                .iter()
                .all(|entry| entry.source_count == 0 && entry.citations.is_empty())
        );
        let same =
            sync_concept_backlinks(&pages, root.path(), false, &dirs, SynthesisPolicy::Record)
                .unwrap();
        assert_eq!(same.unchanged, 2);
        assert!(same.updated.is_empty());
        assert!(
            std::fs::symlink_metadata(root.path().join("daily/source.md"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn hidden_legacy_concept_owner_cannot_become_source_through_visible_alias() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = write_owner_test_concept(root.path());
        std::fs::create_dir_all(root.path().join("wiki/concepts/.hidden")).unwrap();
        std::fs::write(
            root.path().join("wiki/concepts/.hidden/legacy.md"),
            "---\ntitle: Legacy\n---\n[Topic](/wiki/concepts/topic.md)\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("daily")).unwrap();
        std::os::unix::fs::symlink(
            "../wiki/concepts/.hidden/legacy.md",
            root.path().join("daily/legacy.md"),
        )
        .unwrap();
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        assert!(
            pages
                .iter()
                .any(|page| page.path == Path::new("daily/legacy.md"))
        );
        assert!(
            !pages
                .iter()
                .any(|page| page.path == Path::new("wiki/concepts/.hidden/legacy.md"))
        );
        assert!(concept_evidence(&pages, root.path(), &dirs).unwrap()[&concept].is_empty());
    }

    #[test]
    fn prepared_discovery_preserves_page_roles_and_uncaptured_concept_evidence() {
        use lk_vault::SynthesisInputStatus as Status;

        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let fixtures = [
            (
                "wiki/concepts/topic.md",
                "type: concept\n",
                "Topic",
                Status::EmptyEvidence,
            ),
            (
                "wiki/concepts/.drafts/hidden.md",
                "type: concept\n",
                "Hidden",
                Status::NotCaptured,
            ),
            ("wiki/concepts/legacy.md", "", "Legacy", Status::NotCaptured),
            (
                "wiki/concepts/document.md",
                "type: document\n",
                "Document",
                Status::NotApplicable,
            ),
        ];
        for (path, format, title, _) in fixtures {
            let full = root.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(
                full,
                format!(
                    "---\n{format}title: {title}\naliases: [{title}Alias]\n---\nScoped knowledge.\n"
                ),
            )
            .unwrap();
        }
        let documents = root.path().join("wiki/documents");
        std::fs::create_dir_all(&documents).unwrap();
        std::fs::write(
            documents.join("mentions.md"),
            "---\ntype: document\ntitle: Mentions\n---\nDocumentAlias LegacyAlias\n",
        )
        .unwrap();
        let captured = capture_concept_citations(root.path(), &dirs, false).unwrap();
        assert_eq!(captured.projection().concepts.len(), 1);
        let session = lk_vault::MutationSession::acquire(root.path()).unwrap();
        let snapshot = lk_vault::prepare_search_with_citations(
            &session,
            &dirs,
            captured.projection(),
            |selected| {
                captured
                    .verify(selected)
                    .map_err(|error| std::io::Error::other(error.to_string()).into())
            },
        )
        .unwrap();
        let browse = lk_vault::search(
            root.path(),
            &dirs,
            &lk_vault::SearchOptions {
                snapshot: Some(&snapshot.snapshot),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(browse.total, 5);
        for (path, _, title, status) in fixtures {
            assert_eq!(
                browse
                    .hits
                    .iter()
                    .find(|hit| hit.path == path)
                    .unwrap()
                    .synthesis
                    .status,
                status
            );
            for selected_snapshot in [None, Some(snapshot.snapshot.as_str())] {
                let search = lk_vault::search(
                    root.path(),
                    &dirs,
                    &lk_vault::SearchOptions {
                        query: Some(title),
                        snapshot: selected_snapshot,
                        ..Default::default()
                    },
                )
                .unwrap();
                let hit = search.hits.iter().find(|hit| hit.path == path).unwrap();
                assert_eq!(
                    hit.synthesis.status,
                    if selected_snapshot.is_none() && status == Status::EmptyEvidence {
                        Status::NotCaptured
                    } else {
                        status
                    }
                );
                assert_eq!(search.total, if title == "Legacy" { 2 } else { 1 });
            }
            let context = lk_vault::inspect_page_context(
                root.path(),
                &dirs,
                Path::new(path),
                &lk_vault::ContextOptions {
                    snapshot: &snapshot.snapshot,
                    limit: 1,
                    offset: 0,
                },
            )
            .unwrap();
            assert_eq!(context.synthesis.status, status);
            match context.evidence {
                lk_vault::ContextEvidence::Available(evidence) => {
                    assert_eq!(status, Status::EmptyEvidence);
                    assert_eq!(evidence.total, 0);
                }
                lk_vault::ContextEvidence::NotCaptured => assert_eq!(status, Status::NotCaptured),
                lk_vault::ContextEvidence::NotApplicable => {
                    assert_eq!(status, Status::NotApplicable)
                }
            }
        }
    }

    #[test]
    fn captured_citations_share_authority_and_reject_input_or_inventory_drift() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = write_owner_test_concept(root.path());
        std::fs::create_dir_all(root.path().join("daily")).unwrap();
        let source = root.path().join("daily/report.md");
        let raw = "---\ntype: daily\ntitle: Report\nupdated: 2026-10-18\n---\n[Topic](../wiki/concepts/topic.md)\n[Again](../wiki/concepts/topic.md)\n";
        std::fs::write(&source, raw).unwrap();
        std::fs::write(
            root.path().join("daily/board.md"),
            "---\ntype: task-board\n---\n[Topic](../wiki/concepts/topic.md)\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("wiki/concepts/other.md"),
            "---\ntype: concept\n---\n[Topic](topic.md)\n",
        )
        .unwrap();
        // Search includes hidden descendants, while graph policy intentionally does not.
        std::fs::create_dir_all(root.path().join("wiki/documents/.drafts")).unwrap();
        std::fs::write(
            root.path().join("wiki/documents/.drafts/hidden.md"),
            "---\ntype: document\ntitle: Hidden draft\n---\n[Topic](../../concepts/topic.md)\n",
        )
        .unwrap();
        let captured = capture_concept_citations(root.path(), &dirs, false).unwrap();
        let projected = captured
            .projection()
            .concepts
            .iter()
            .find(|entry| entry.path == concept)
            .unwrap();
        assert_eq!(projected.source_count, 1);
        assert_eq!(projected.citations.len(), 1);
        assert!(
            captured
                .projection()
                .concepts
                .iter()
                .any(|entry| entry.path.ends_with("other.md") && entry.citations.is_empty())
        );
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        assert_eq!(
            projected.citations,
            concept_evidence(&pages, root.path(), &dirs).unwrap()[&concept]
        );
        assert_eq!(
            projected.citations,
            concept_resynthesis(&pages, root.path(), &dirs, &concept)
                .unwrap()
                .citations
        );
        assert_eq!(
            projected.citations,
            sync_concept_backlinks(&pages, root.path(), true, &dirs, SynthesisPolicy::Record)
                .unwrap()
                .resynthesize[0]
                .citations
        );

        let session = lk_vault::MutationSession::acquire(root.path()).unwrap();
        let prepare = |captured: &CapturedConceptCitations| {
            lk_vault::prepare_search_with_citations(
                &session,
                &dirs,
                captured.projection(),
                |selected| {
                    captured
                        .verify(selected)
                        .map_err(|error| std::io::Error::other(error.to_string()).into())
                },
            )
        };
        let snapshot = prepare(&captured).unwrap();
        let before = std::fs::read(root.path().join(".lorekeeper/search.sqlite")).unwrap();
        let options = lk_vault::CitationOptions {
            snapshot: &snapshot.snapshot,
            limit: 1,
            offset: 0,
        };
        assert_eq!(
            lk_vault::inspect_concept_citations(root.path(), &dirs, &concept, &options)
                .unwrap()
                .citations,
            projected.citations
        );
        let empty = lk_vault::inspect_concept_citations(
            root.path(),
            &dirs,
            Path::new("wiki/concepts/other.md"),
            &options,
        )
        .unwrap();
        assert_eq!(empty.source_count, 0);
        assert_eq!(empty.total, 0);
        assert!(empty.citations.is_empty());

        // Bookkeeping does not change source_revision, but snapshot bytes must still agree.
        std::fs::write(&source, raw.replace("2026-10-18", "2026-10-19")).unwrap();
        assert!(prepare(&captured).is_err());
        assert_eq!(
            std::fs::read(root.path().join(".lorekeeper/search.sqlite")).unwrap(),
            before
        );
        std::fs::write(&source, raw).unwrap();
        captured.verify(&[]).unwrap();
        // This page is outside search scope, but remains part of graph existence capture.
        std::fs::create_dir_all(root.path().join("authored")).unwrap();
        std::fs::write(root.path().join("authored/note.md"), "# Unmanaged\n").unwrap();
        assert!(prepare(&captured).is_err());
        assert_eq!(
            std::fs::read(root.path().join(".lorekeeper/search.sqlite")).unwrap(),
            before
        );
        assert_eq!(
            lk_vault::inspect_concept_citations(root.path(), &dirs, &concept, &options)
                .unwrap()
                .citations,
            projected.citations
        );
        std::fs::remove_file(root.path().join("authored/note.md")).unwrap();
        captured.verify(&[]).unwrap();
        std::fs::write(
            root.path().join("daily/broken.md"),
            "---\ninvalid: [\n---\n",
        )
        .unwrap();
        assert!(capture_concept_citations(root.path(), &dirs, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn captured_citations_pin_resolution_of_untraversed_directory_aliases() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        write_owner_test_concept(root.path());
        std::fs::create_dir_all(root.path().join("alternatives/concepts")).unwrap();
        std::fs::copy(
            root.path().join("wiki/concepts/topic.md"),
            root.path().join("alternatives/concepts/topic.md"),
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("daily")).unwrap();
        std::fs::write(
            root.path().join("daily/report.md"),
            "---\ntype: daily\n---\n[Topic](../route/concepts/topic.md)\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("wiki", root.path().join("route")).unwrap();
        let captured = capture_concept_citations(root.path(), &dirs, false).unwrap();
        assert_eq!(captured.projection().concepts[0].source_count, 1);
        captured.verify(&[]).unwrap();
        std::fs::remove_file(root.path().join("route")).unwrap();
        std::os::unix::fs::symlink("alternatives", root.path().join("route")).unwrap();
        assert!(captured.verify(&[]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn directory_and_leaf_source_aliases_share_evidence_but_hard_links_remain_distinct() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = write_owner_test_concept(root.path());
        let backing = root.path().join("daily/shared");
        std::fs::create_dir_all(&backing).unwrap();
        std::fs::write(
            backing.join("report.md"),
            "---\ntype: document\ntitle: Report\n---\n[Topic](../../wiki/concepts/topic.md)\n",
        )
        .unwrap();
        let baseline = crate::scan::scan_vault(root.path(), false).unwrap();
        let expected = concept_resynthesis(&baseline, root.path(), &dirs, &concept)
            .unwrap()
            .citations;
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].path, "daily/shared/report.md");
        let first = sync_concept_backlinks(
            &baseline,
            root.path(),
            false,
            &dirs,
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(first.resynthesize[0].citations, expected);
        let unchanged = std::fs::read_to_string(root.path().join(&concept)).unwrap();
        assert!(unchanged.contains("source_count: 1"));
        assert!(unchanged.contains(&citation_digest(&expected)));
        assert_compact_sources(&unchanged, "Sources");

        std::os::unix::fs::symlink("../daily/shared", root.path().join("wiki/documents")).unwrap();
        std::os::unix::fs::symlink("report.md", backing.join("alias.md")).unwrap();
        let aliased = crate::scan::scan_vault(root.path(), true).unwrap();
        assert_eq!(aliased.len(), 5, "all readable addresses remain scanned");
        assert_eq!(
            concept_evidence(&aliased, root.path(), &dirs).unwrap()[&concept],
            expected
        );
        let captured = capture_concept_citations(root.path(), &dirs, true).unwrap();
        assert_eq!(captured.projection().concepts[0].source_count, 1);
        assert_eq!(captured.projection().concepts[0].citations, expected);
        assert_eq!(
            concept_resynthesis(&aliased, root.path(), &dirs, &concept)
                .unwrap()
                .citations,
            expected
        );
        let same =
            sync_concept_backlinks(&aliased, root.path(), false, &dirs, SynthesisPolicy::Record)
                .unwrap();
        assert!(same.updated.is_empty());
        assert_eq!(same.resynthesize[0].citations, expected);
        assert_eq!(
            std::fs::read_to_string(root.path().join(&concept)).unwrap(),
            unchanged
        );

        std::fs::hard_link(backing.join("report.md"), backing.join("second.md")).unwrap();
        let distinct = crate::scan::scan_vault(root.path(), true).unwrap();
        let evidence = concept_evidence(&distinct, root.path(), &dirs).unwrap();
        assert_eq!(evidence[&concept].len(), 2);
        assert_eq!(evidence[&concept][1].path, "daily/shared/second.md");
        let captured = capture_concept_citations(root.path(), &dirs, true).unwrap();
        assert_eq!(captured.projection().concepts[0].source_count, 2);
        assert_eq!(
            captured.projection().concepts[0].citations,
            evidence[&concept]
        );
        let updated = sync_concept_backlinks(
            &distinct,
            root.path(),
            false,
            &dirs,
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(updated.resynthesize[0].citations, evidence[&concept]);
        let written = std::fs::read_to_string(root.path().join(&concept)).unwrap();
        assert!(written.contains("source_count: 2"));
        assert!(written.contains(&citation_digest(&evidence[&concept])));
        assert_compact_sources(&written, "Sources");
        assert!(
            !section_body(&written, "Sources")
                .unwrap()
                .contains("documents/")
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_alias_contexts_compare_verified_target_owners_and_keep_missing_targets_distinct() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = write_owner_test_concept(root.path());
        std::fs::create_dir_all(root.path().join("daily/nested")).unwrap();
        let source = root.path().join("daily/report.md");
        std::fs::write(
            &source,
            "---\ntype: daily\ntitle: Report\n---\n[Topic](../wiki/concepts/topic.md)\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("../report.md", root.path().join("daily/nested/report.md"))
            .unwrap();
        std::os::unix::fs::symlink("../wiki", root.path().join("daily/wiki")).unwrap();
        let pages = crate::scan::scan_vault(root.path(), true).unwrap();
        let existence = VaultExistence::build_at(root.path(), &pages, &dirs).unwrap();
        assert_ne!(
            existence.reached(&Link::to("wiki/concepts/topic.md")),
            existence.reached(&Link::to("daily/wiki/concepts/topic.md")),
            "exact readable target aliases retain their distinct address IDs"
        );
        let evidence = concept_evidence(&pages, root.path(), &dirs).unwrap();
        assert_eq!(evidence[&concept].len(), 1);
        assert_eq!(evidence[&concept][0].path, "daily/report.md");

        // Both spellings are now missing, at different addresses. Omitting unresolved links
        // would falsely make these different contexts look equivalent.
        std::fs::remove_file(root.path().join("daily/wiki")).unwrap();
        std::fs::write(
            &source,
            "---\ntype: daily\ntitle: Report\n---\n[Missing](../missing.md)\n",
        )
        .unwrap();
        let pages = crate::scan::scan_vault(root.path(), true).unwrap();
        let before = std::fs::read_to_string(root.path().join(&concept)).unwrap();
        assert!(concept_evidence(&pages, root.path(), &dirs).is_err());
        assert!(concept_resynthesis(&pages, root.path(), &dirs, &concept).is_err());
        let error =
            sync_concept_backlinks(&pages, root.path(), false, &dirs, SynthesisPolicy::Record)
                .unwrap_err();
        assert!(error.to_string().contains("conflicting citation contexts"));
        assert_eq!(
            std::fs::read_to_string(root.path().join(&concept)).unwrap(),
            before
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_owner_without_an_eligible_direct_address_uses_a_stable_verified_alias() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = write_owner_test_concept(root.path());
        std::fs::create_dir_all(root.path().join("archive")).unwrap();
        std::fs::create_dir_all(root.path().join("daily")).unwrap();
        std::fs::write(
            root.path().join("archive/report.md"),
            "---\ntype: document\ntitle: Report\n---\n[Topic](../wiki/concepts/topic.md)\n",
        )
        .unwrap();
        for name in ["b", "a"] {
            std::os::unix::fs::symlink(
                "../archive/report.md",
                root.path().join(format!("daily/{name}.md")),
            )
            .unwrap();
        }
        let mut pages = crate::scan::scan_vault(root.path(), false).unwrap();
        let evidence = concept_evidence(&pages, root.path(), &dirs).unwrap();
        assert_eq!(evidence[&concept].len(), 1);
        assert_eq!(evidence[&concept][0].path, "daily/a.md");
        pages.reverse();
        assert_eq!(
            concept_evidence(&pages, root.path(), &dirs).unwrap(),
            evidence
        );
    }

    #[cfg(unix)]
    #[test]
    fn rss_source_aliases_keep_one_selected_citation_and_immutable_publication_guards() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = write_owner_test_concept(root.path());
        let event: lk_core::event::Event = serde_json::from_value(serde_json::json!({
            "id":"rss:2026-10-18:0000000000000001", "source_id":"rss", "source_type":"rss",
            "timestamp":"2026-10-18T00:00:00Z", "date":"2026-10-18", "title":"Report", "body":"Original report.",
            "url":"https://example.test/report", "author":null, "labels":[], "category":null,
            "performance_category":null, "is_self":false, "is_personal":false, "metadata":{}
        })).unwrap();
        let capture = lk_core::rss_original::RssOriginalCapture::from_events(&[&event]).unwrap();
        let session = lk_vault::MutationSession::acquire(root.path()).unwrap();
        let reference = lk_vault::publish_rss_original(&session, &capture).unwrap();
        let selected = &capture.manifest.events[0];
        let binding = serde_json::json!([{
            "schema":lk_core::rss_evidence::RSS_EVIDENCE_SCHEMA,
            "concept":concept.to_string_lossy(), "manifest":reference,
            "events":[{"event_id":selected.event_id, "revision":selected.revision}]
        }]);
        let backing = root.path().join("daily/shared");
        std::fs::create_dir_all(&backing).unwrap();
        let record = serde_json::json!({"rss_original":reference});
        std::fs::write(backing.join("report.md"), format!("---\ntype: daily\ntitle: Report\nsource: rss\ncreated: 2026-10-18\nsource_record: {record}\nconcept_evidence: {binding}\n---\n## Related Concepts\n- [Topic](../../wiki/concepts/topic.md)\n")).unwrap();
        std::os::unix::fs::symlink("../daily/shared", root.path().join("wiki/documents")).unwrap();
        std::os::unix::fs::symlink("report.md", backing.join("alias.md")).unwrap();
        let pages = crate::scan::scan_vault(root.path(), true).unwrap();
        let evidence = concept_evidence(&pages, root.path(), &dirs).unwrap();
        assert_eq!(evidence[&concept].len(), 1);
        let citation = &evidence[&concept][0];
        assert_eq!(citation.path, "daily/shared/report.md");
        assert_eq!(citation.revision, selected.revision);
        assert_eq!(
            citation.rss_event.as_ref().unwrap().event_id,
            selected.event_id
        );
        let captured = capture_concept_citations(root.path(), &dirs, true).unwrap();
        assert_eq!(captured.projection().concepts[0].source_count, 1);
        assert_eq!(
            captured.projection().concepts[0].citations,
            evidence[&concept]
        );
        assert_eq!(
            concept_resynthesis(&pages, root.path(), &dirs, &concept)
                .unwrap()
                .citations,
            evidence[&concept]
        );
        let sync =
            sync_concept_backlinks_in_session(&pages, &session, &dirs, SynthesisPolicy::Record)
                .unwrap();
        assert_eq!(sync.resynthesize[0].citations, evidence[&concept]);
        let written = std::fs::read_to_string(root.path().join(&concept)).unwrap();
        assert!(written.contains("source_count: 1"));
        assert!(written.contains(&citation_digest(&evidence[&concept])));
        assert_compact_sources(&written, "Sources");

        let mut plan = lk_vault::MutationPlan::default();
        lk_vault::guard_rss_citation(&mut plan, root.path(), &concept, citation).unwrap();
        plan.create(
            "published.md",
            "Must remain absent after evidence changes.".into(),
        );
        std::fs::write(
            root.path().join(format!(
                ".lorekeeper/evidence/rss/events/{}.json",
                selected.revision
            )),
            "corrupt",
        )
        .unwrap();
        assert!(session.apply(plan).is_err());
        assert!(!root.path().join("published.md").exists());
        assert!(concept_evidence(&pages, root.path(), &dirs).is_err());
        assert!(captured.verify(&[]).is_err());
    }

    /// A concept page as the template renders one: every section the sweep reads, and the
    /// `llm_inputs:` mapping it records the synthesis input in.
    fn write_concept(dir: &TempDir, stem: &str, source_lines: &[&str]) {
        write_legacy_concept_page(dir, stem, source_lines, "");
    }

    fn write_concept_page(dir: &TempDir, stem: &str, source_lines: &[&str], llm_inputs: &str) {
        write_concept_body(
            dir,
            stem,
            source_lines.len(),
            llm_inputs,
            crate::sources::render(lk_core::i18n::Locale::Ko),
        );
    }

    fn write_legacy_concept_page(
        dir: &TempDir,
        stem: &str,
        source_lines: &[&str],
        llm_inputs: &str,
    ) {
        write_concept_body(
            dir,
            stem,
            source_lines.len(),
            llm_inputs,
            &source_lines.join("\n"),
        );
    }

    fn write_concept_body(
        dir: &TempDir,
        stem: &str,
        count: usize,
        llm_inputs: &str,
        sources: &str,
    ) {
        let path = dir.path().join("wiki/concepts").join(format!("{stem}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let sources = if sources.is_empty() {
            String::new()
        } else {
            format!("{sources}\n\n")
        };
        std::fs::write(
            &path,
            format!(
                "---\nid: {stem}\nsource_count: {count}\nllm_inputs:\n{llm_inputs}---\n\n\
                 # {stem}\n\n## 핵심\n\nWhat {stem} is.\n\n## 출처\n\n{sources}\
                 ## 메타\n\n- key: value\n"
            ),
        )
        .unwrap();
    }

    fn assert_compact_sources(content: &str, heading: &str) {
        assert_eq!(
            crate::sources::classify(
                section_body(content, heading).unwrap(),
                Path::new("wiki/concepts/topic.md")
            ),
            crate::sources::SourcesBody::Compact
        );
    }

    #[test]
    fn compact_sources_bound_prose_without_truncating_evidence_or_inventing_deltas() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = write_owner_test_concept(root.path());
        std::fs::create_dir_all(root.path().join("daily")).unwrap();
        for index in 0..240 {
            std::fs::write(
                root.path().join(format!("daily/source-{index:03}.md")),
                "---\ntype: daily\n---\n[Topic](../wiki/concepts/topic.md)\n",
            )
            .unwrap();
        }
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        let before = std::fs::read_to_string(root.path().join(&concept)).unwrap();
        let first =
            sync_concept_backlinks(&pages, root.path(), false, &dirs, SynthesisPolicy::Record)
                .unwrap();
        assert_eq!(first.updated[0].added.as_ref().unwrap().len(), 240);
        let after = std::fs::read_to_string(root.path().join(&concept)).unwrap();
        let body = section_body(&after, "Sources").unwrap();
        assert_compact_sources(&after, "Sources");
        assert!(body.chars().count() < 300);
        let authored_revision =
            |raw: &str| lk_core::concept::source_revision(&frontmatter::parse_page(raw).unwrap());
        assert_ne!(
            authored_revision(&before),
            authored_revision(&after),
            "the exact body migration truthfully changes authored revision"
        );
        let captured = capture_concept_citations(root.path(), &dirs, false).unwrap();
        let session = lk_vault::MutationSession::acquire(root.path()).unwrap();
        let snapshot = lk_vault::prepare_search_with_citations(
            &session,
            &dirs,
            captured.projection(),
            |selected| {
                captured
                    .verify(selected)
                    .map_err(|error| std::io::Error::other(error.to_string()).into())
            },
        )
        .unwrap();
        let mut options = lk_vault::CitationOptions {
            snapshot: &snapshot.snapshot,
            limit: 100,
            offset: 0,
        };
        let mut read = Vec::new();
        loop {
            let page = lk_vault::inspect_concept_citations(root.path(), &dirs, &concept, &options)
                .unwrap();
            assert_eq!(page.source_count, 240);
            assert_eq!(page.total, 240);
            assert_eq!(
                page.digest,
                citation_digest(&first.resynthesize[0].citations)
            );
            read.extend(page.citations);
            let Some(offset) = page.next_offset else {
                break;
            };
            options.offset = offset;
        }
        assert_eq!(read, first.resynthesize[0].citations);
        std::fs::write(
            root.path().join("daily/source-240.md"),
            "---\ntype: daily\n---\n[Topic](../wiki/concepts/topic.md)\n",
        )
        .unwrap();
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        let changed =
            sync_concept_backlinks_in_session(&pages, &session, &dirs, SynthesisPolicy::Record)
                .unwrap();
        assert_eq!(changed.updated[0].added, None);
        assert_eq!(changed.updated[0].removed, None);
        assert_eq!(changed.resynthesize[0].citations.len(), 241);
        let grown = std::fs::read_to_string(root.path().join(&concept)).unwrap();
        assert_eq!(section_body(&grown, "Sources").unwrap(), body);
        assert_eq!(
            authored_revision(&grown),
            authored_revision(&after),
            "future citation growth changes evidence input/count, not the stable projection"
        );
        let same =
            sync_concept_backlinks_in_session(&pages, &session, &dirs, SynthesisPolicy::Record)
                .unwrap();
        assert_eq!(same.unchanged, 1);
        assert!(same.updated.is_empty());
    }

    #[test]
    fn custom_sources_stay_verbatim_while_count_and_input_are_reconciled() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = write_owner_test_concept(root.path());
        let custom = "\n- [Old](../../daily/old.md)\n\nAuthored explanation.\n\n```markdown\n## Quoted\n```\n\n";
        let original = std::fs::read_to_string(root.path().join(&concept))
            .unwrap()
            .replace("## Sources\n\n", &format!("## Sources\n{custom}"));
        std::fs::write(root.path().join(&concept), &original).unwrap();
        std::fs::create_dir_all(root.path().join("daily")).unwrap();
        std::fs::write(
            root.path().join("daily/report.md"),
            "---\ntype: daily\n---\n[Topic](../wiki/concepts/topic.md)\n",
        )
        .unwrap();
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        let report =
            sync_concept_backlinks(&pages, root.path(), false, &dirs, SynthesisPolicy::Record)
                .unwrap();
        assert_eq!(report.preserved_sources, vec![concept.clone()]);
        assert_eq!(report.updated[0].added, None);
        assert_eq!(report.updated[0].removed, None);
        assert_eq!(report.resynthesize[0].citations.len(), 1);
        let updated = std::fs::read_to_string(root.path().join(&concept)).unwrap();
        assert_eq!(
            section_body(&updated, "Sources"),
            section_body(&original, "Sources")
        );
        assert!(updated.contains("source_count: 1"));
        assert!(updated.contains(&citation_digest(&report.resynthesize[0].citations)));
    }

    /// The `llm_inputs` block of a page whose synthesis was written from `citations`.
    fn synthesized_from(citations: &[&str]) -> String {
        let digest = fixture_digest(
            &citations
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>(),
        );
        format!("  synthesis: \"{digest}\"\n  synthesis_done: \"{digest}\"\n")
    }

    #[test]
    fn a_case_alias_is_evidence_only_when_the_filesystem_reaches_it() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(root.path().join("daily/news")).unwrap();
        std::fs::write(
            root.path().join("wiki/concepts/topic.md"),
            "---\ntype: concept\ntitle: Topic\n---\n## Synthesis\n\n## Sources\n\n## Related\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("daily/news/day.md"),
            "---\ntype: daily\ntitle: Day\n---\n[Topic](../../wiki/concepts/TOPIC.md)\n",
        )
        .unwrap();
        let aliases = root.path().join("wiki/concepts/TOPIC.md").is_file();
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        let report = super::sync_concept_backlinks(
            &pages,
            root.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        let output = std::fs::read_to_string(root.path().join("wiki/concepts/topic.md")).unwrap();
        assert!(output.contains(&format!("source_count: {}", usize::from(aliases))));
        assert_compact_sources(&output, "Sources");
        assert_eq!(report.resynthesize.is_empty(), !aliases);
    }

    #[test]
    fn punctuation_distinct_source_addresses_keep_their_own_revision() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(root.path().join("wiki/documents")).unwrap();
        std::fs::write(root.path().join("wiki/concepts/topic.md"), "---\ntype: concept\ntitle: Topic\n---\n# Topic\n\n## Synthesis\n\n## Sources\n\n## Related\n").unwrap();
        std::fs::write(
            root.path().join("wiki/documents/a-b.md"),
            "---\ntype: document\ntitle: Citing\n---\n[Topic](../concepts/topic.md)\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("wiki/documents/a_b.md"),
            "---\ntype: document\ntitle: Unrelated\n---\nDifferent evidence.\n",
        )
        .unwrap();
        let pages = crate::scan::scan_vault(root.path(), false).unwrap();
        let report = sync_fixture(
            &pages,
            root.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        let output = std::fs::read_to_string(root.path().join("wiki/concepts/topic.md")).unwrap();
        assert_compact_sources(&output, "Sources");
        assert!(!output.contains("a_b.md"));
        assert_eq!(
            report.resynthesize[0].citations[0].path,
            "wiki/documents/a-b.md"
        );
    }

    #[test]
    fn inserts_missing_backlink() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "oy365", &[]);

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            // A daily page that links the concept.
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.updated.len(), 1);
        assert_eq!(report.unchanged, 0);
        assert_eq!(
            report.updated[0].added,
            Some(vec!["daily/slack/2026-05-20".into()])
        );
        assert_eq!(report.updated[0].removed, Some(Vec::new()));

        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        assert_compact_sources(&content, "출처");
        assert!(content.contains("source_count: 1"));
    }

    /// `source_count` is re-derived, not merely kept consistent with the section beside it.
    /// A page whose entries are already right but whose count is wrong — a hand edit, a run
    /// that died between the two writes — is exactly the drift this command exists to
    /// repair, and it is the one shape that looks unchanged if the two facts are checked as
    /// alternatives rather than together.
    #[test]
    fn a_correct_source_list_with_a_wrong_count_is_still_repaired() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/oy365.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: oy365\nsource_count: 7\nllm_inputs:\n---\n\n# oy365\n\n## 핵심\n\nWhat oy365 is.\n\n## 출처\n\n- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)\n\n## 메타\n\n- key: value\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.unchanged, 0, "a wrong count is not 'unchanged'");
        assert_eq!(report.updated.len(), 1);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("source_count: 1"), "{content}");
    }

    #[test]
    fn removes_stale_backlink() {
        let dir = TempDir::new().unwrap();
        // Disk says the concept is referenced by an old daily page that no longer
        // links to it.
        write_concept(&dir, "oy365", &["- [old](../../daily/slack/2026-01-01.md)"]);

        let pages = vec![build_page(
            "wiki/concepts/oy365",
            "wiki/concepts/oy365.md",
            &[],
        )];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.updated.len(), 1);
        assert_eq!(report.updated[0].added, Some(Vec::new()));
        assert_eq!(
            report.updated[0].removed,
            Some(vec!["daily/slack/2026-01-01".into()])
        );

        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        assert_compact_sources(&content, "출처");
        assert!(!content.contains("2026-01-01"));
    }

    #[test]
    fn preserves_other_sections() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/oy365.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: oy365\nllm_inputs:\n---\n\n# oy365\n\n## 핵심\n\nThis is the synthesis paragraph.\n\n## 출처\n\n- [old](../../daily/x/old.md)\n\n## 메타\n\n- references: 1\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/x/2026-05-20",
                "daily/x/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## 핵심\n\nThis is the synthesis paragraph."));
        assert!(content.contains("## 메타\n\n- references: 1"));
        assert_compact_sources(&content, "출처");
        assert!(!content.contains("(../../daily/x/old.md)"));
    }

    #[test]
    fn dry_run_does_not_write() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "oy365", &[]);
        let before = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/x/2026-05-20",
                "daily/x/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            true,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.updated.len(), 1);

        let after = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        assert_eq!(before, after, "dry-run must not write");
    }

    #[test]
    fn idempotent_on_second_run() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "oy365", &[]);

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/x/2026-05-20",
                "daily/x/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        // First run rewrites the page.
        let first = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(first.updated.len(), 1);

        // Second run: every concept page is already in sync.
        let second = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(second.updated.is_empty());
        assert_eq!(second.unchanged, 1);
    }

    #[test]
    fn english_locale_uses_sources_heading() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/oy365.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: oy365\nllm_inputs:\n---\n\n# oy365\n\n## Synthesis\n\nWhat oy365 is.\n\n## Sources\n\n\n## Metadata\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/x/2026-05-20",
                "daily/x/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            section_body(&content, "Sources").unwrap().trim(),
            crate::sources::render(lk_core::i18n::Locale::En)
        );
    }

    #[test]
    fn self_link_to_concept_is_ignored() {
        // A concept page that links itself should not list itself as a source.
        let dir = TempDir::new().unwrap();
        write_concept_page(&dir, "oy365", &[], &synthesized_from(&[]));

        let pages = vec![build_page(
            "wiki/concepts/oy365",
            "wiki/concepts/oy365.md",
            &["wiki/concepts/oy365"],
        )];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.updated.len(), 0);
        assert_eq!(report.unchanged, 1);
    }

    #[test]
    fn concept_to_concept_link_is_not_a_source() {
        // Only content pages qualify as sources; a citation from another concept
        // belongs in `## Related` and must not be credited here.
        let dir = TempDir::new().unwrap();
        write_concept_page(&dir, "target", &[], &synthesized_from(&[]));
        write_concept_page(&dir, "citer", &[], &synthesized_from(&[]));

        let pages = vec![
            build_page("wiki/concepts/target", "wiki/concepts/target.md", &[]),
            build_page(
                "wiki/concepts/citer",
                "wiki/concepts/citer.md",
                &["wiki/concepts/target"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.updated.is_empty());
    }

    /// A concept whose evidence has moved is owed a rewritten synthesis, and the sweep that
    /// derives the evidence is what says so. The anchor travels with it: a page keeps the
    /// heading it was written under, so a result must land there rather than under whatever
    /// `vault.locale` says today.
    #[test]
    fn a_concept_whose_citations_moved_is_owed_a_synthesis() {
        let dir = TempDir::new().unwrap();
        write_concept_page(&dir, "oy365", &[], &synthesized_from(&[]));
        let path = dir.path().join("wiki/concepts/oy365.md");
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replacen("---\n", "---\ntype: concept\n", 1)).unwrap();

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.resynthesize.len(), 1, "{report:?}");
        let owed = &report.resynthesize[0];
        assert_eq!(owed.anchor, "## 핵심");
        assert_eq!(owed.citations[0].path, "daily/slack/2026-05-20.md");

        // The page now records the input its synthesis is owed against, so the drain that
        // writes the section has something to stamp against.
        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        let digest = fixture_digest(&["daily/slack/2026-05-20".to_owned()]);
        assert!(
            content.contains(&format!("synthesis: \"{digest}\"")),
            "{content}"
        );
    }

    /// A synthesis already written from the set the page carries is owed nothing, so a
    /// re-run of the janitor never spends an LLM session on a concept nothing has changed
    /// about — the same self-perpetuating cache every other section has.
    #[test]
    fn a_synthesis_matching_its_evidence_is_owed_nothing() {
        let dir = TempDir::new().unwrap();
        write_concept_page(
            &dir,
            "oy365",
            &["- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)"],
            &synthesized_from(&["daily/slack/2026-05-20"]),
        );

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.resynthesize.is_empty(), "{report:?}");
        assert_eq!(report.unchanged, 1);
    }

    /// Repairing a citation label does not change the source revision it already answers.
    #[test]
    fn a_stale_citation_label_does_not_owe_a_new_synthesis() {
        let dir = TempDir::new().unwrap();
        write_legacy_concept_page(
            &dir,
            "oy365",
            &["- [Stale Label](../documents/spec.md)"],
            &synthesized_from(&["wiki/documents/spec"]),
        );

        let mut source = build_page(
            "wiki/documents/spec",
            "wiki/documents/spec.md",
            &["wiki/concepts/oy365"],
        );
        source.title = "Current Title".into();
        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            source,
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(
            report.resynthesize.is_empty(),
            "the source revision is unchanged: {report:?}"
        );
        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        assert_compact_sources(&content, "출처");
        assert!(!content.contains("Stale Label"));
    }

    /// The two questions are independent. A page whose sources list is already correct can
    /// still be owed a synthesis, and asking them together would leave it owed forever.
    #[test]
    fn a_page_in_sync_can_still_be_owed_a_synthesis() {
        let dir = TempDir::new().unwrap();
        write_concept_page(
            &dir,
            "oy365",
            &["- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)"],
            &format!(
                "  synthesis: \"{}\"\n",
                fixture_digest(&["daily/slack/2026-05-20".to_owned()])
            ),
        );
        let path = dir.path().join("wiki/concepts/oy365.md");
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replacen("---\n", "---\ntype: concept\n", 1)).unwrap();

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.unchanged, 1, "nothing to rewrite: {report:?}");
        assert_eq!(
            report.resynthesize.len(),
            1,
            "the input is recorded and unanswered: {report:?}"
        );
    }

    /// A page that already holds written prose and has recorded no input is ADOPTED: the
    /// prose becomes the answer for the evidence the page carries. Queueing it instead is an
    /// upgrade that replaces every authored synthesis in the vault in one unattended run.
    #[test]
    fn authored_definition_adoption_uses_commonmark_physical_lines() {
        for (body, ending) in ["\n", "\r\n", "\r"].into_iter().flat_map(|ending| {
            [
                "> [!note] Annotation\n\nAn authored definition.\n",
                "> An authored definition.\n",
                "> \\[!note] An authored definition.\n",
                "> ```text\n> # Literal definition\n> ```\n",
                "> > [!note] Nested annotation\n>\n> An authored definition.\n",
                "- > > [!note] Nested annotation\n  >\n  > An authored definition.\n",
            ]
            .into_iter()
            .map(move |body| (body, ending))
        }) {
            let dir = TempDir::new().unwrap();
            write_concept_page(&dir, "legacy", &[], "");
            let concept = dir.path().join("wiki/concepts/legacy.md");
            let body = body.replace('\n', ending);
            let raw = std::fs::read_to_string(&concept)
                .unwrap()
                .replace("What legacy is.", &body);
            std::fs::write(&concept, raw).unwrap();
            let pages = vec![
                build_page("wiki/concepts/legacy", "wiki/concepts/legacy.md", &[]),
                build_page(
                    "daily/slack/2026-05-20",
                    "daily/slack/2026-05-20.md",
                    &["wiki/concepts/legacy"],
                ),
            ];
            let report = sync_fixture(
                &pages,
                dir.path(),
                false,
                &VaultDirs::default(),
                SynthesisPolicy::Record,
            )
            .unwrap();
            assert_eq!(report.adopted.len(), 1, "{ending:?}: {report:?}");
            assert!(report.resynthesize.is_empty(), "{report:?}");
            let updated = std::fs::read_to_string(concept).unwrap();
            assert!(updated.contains(&body));
            assert!(updated.contains("synthesis_done:"));
        }
    }

    #[test]
    fn a_written_synthesis_with_no_recorded_input_is_adopted_not_replaced() {
        let dir = TempDir::new().unwrap();
        write_concept_page(&dir, "legacy", &[], "");

        let pages = vec![
            build_page("wiki/concepts/legacy", "wiki/concepts/legacy.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/legacy"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(
            report
                .adopted
                .iter()
                .map(|a| a.path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("wiki/concepts/legacy.md")]
        );
        assert_eq!(report.adopted[0].citations, 1);
        assert!(report.resynthesize.is_empty(), "{report:?}");

        // Both markers, so the page reads as answered rather than as work nothing did.
        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/legacy.md")).unwrap();
        let digest = fixture_digest(&["daily/slack/2026-05-20".to_owned()]);
        assert!(
            content.contains(&format!("synthesis: \"{digest}\"")),
            "{content}"
        );
        assert!(
            content.contains(&format!("synthesis_done: \"{digest}\"")),
            "{content}"
        );
    }

    /// A page the sweep could not WRITE was decided nothing about. Reporting it adopted
    /// beside the line reporting it skipped announces a decision that did not happen, and
    /// repeats it on every run.
    #[test]
    fn a_page_that_could_not_be_written_is_not_reported_adopted() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/bare.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "# Bare\n\n## 핵심\n\nHand written prose.\n\n## 출처\n\n## 메타\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/bare", "wiki/concepts/bare.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/bare"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.skipped, vec![PathBuf::from("wiki/concepts/bare.md")]);
        assert!(report.adopted.is_empty(), "{report:?}");
    }

    /// A concept's relations answer to the same evidence its synthesis does, so one task
    /// writes both and the sweep hands it both headings — each spelled as the page spells it,
    /// since a `vault.locale` switch renames them and the digest deliberately ignores locale.
    /// A page without the section gets `None`: `replace_section` leaves a page unchanged when
    /// the heading is absent, so a writer handed an anchor the page lacks would report a write
    /// that never happened.
    #[test]
    fn what_is_owed_carries_both_headings_as_the_page_spells_them() {
        for (body, expected) in [
            ("## 핵심\n\n## 출처\n\n## 관련\n", Some("## 관련")),
            (
                "## Synthesis\n\n## Sources\n\n## Related\n",
                Some("## Related"),
            ),
            ("## 핵심\n\n## 출처\n", None),
        ] {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("wiki/concepts/rag.md");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                format!("---\nid: rag\ntype: concept\nsource_count: 0\nllm_inputs:\n---\n\n# RAG\n\n{body}"),
            )
            .unwrap();

            let pages = vec![
                build_page("wiki/concepts/rag", "wiki/concepts/rag.md", &[]),
                build_page(
                    "daily/slack/2026-05-20",
                    "daily/slack/2026-05-20.md",
                    &["wiki/concepts/rag"],
                ),
            ];

            let report = sync_fixture(
                &pages,
                dir.path(),
                false,
                &VaultDirs::default(),
                SynthesisPolicy::Record,
            )
            .unwrap();

            let owed = report.resynthesize.first().unwrap_or_else(|| {
                panic!("a cited page with a synthesis heading is owed one: {report:?}")
            });
            assert_eq!(
                owed.related_anchor.as_deref(),
                expected,
                "body was:\n{body}"
            );
        }
    }

    /// What is adopted has to BE an answer. A body of nothing but a callout is a marker
    /// somebody left, and freezing one has `graph lint` report an open disagreement that
    /// nothing is scheduled to restate or resolve.
    #[test]
    fn a_body_that_only_annotates_is_not_an_answer_to_adopt() {
        for body in [
            "> [!conflict] two sources disagree on the retention window",
            "> [!note] Annotation only\n> No authored definition outside this annotation.",
            "> > [!conflict] nested annotation only",
            "- > [!conflict] list annotation only",
            "### Notes",
            "## Later\n\n> a quoted line",
        ] {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("wiki/concepts/marker.md");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                format!(
                    "---\nid: marker\ntype: concept\nsource_count: 0\nllm_inputs:\n---\n\n# M\n\n\
                     ## 핵심\n\n{body}\n\n## 출처\n\n## 메타\n"
                ),
            )
            .unwrap();

            let pages = vec![
                build_page("wiki/concepts/marker", "wiki/concepts/marker.md", &[]),
                build_page(
                    "daily/slack/2026-05-20",
                    "daily/slack/2026-05-20.md",
                    &["wiki/concepts/marker"],
                ),
            ];

            let report = sync_fixture(
                &pages,
                dir.path(),
                false,
                &VaultDirs::default(),
                SynthesisPolicy::Record,
            )
            .unwrap();
            assert!(report.adopted.is_empty(), "{body}: {report:?}");
            assert_eq!(report.resynthesize.len(), 1, "{body}: {report:?}");
        }
    }

    /// Adoption is about PROSE, not about the marker being absent. A page whose synthesis is
    /// empty has nothing to adopt, so it is owed one — otherwise a concept created without a
    /// grounding sentence would be marked answered while its section stayed blank.
    #[test]
    fn an_empty_synthesis_with_no_recorded_input_is_owed_one() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/blank.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: blank\ntype: concept\nsource_count: 0\nllm_inputs:\n---\n\n# Blank\n\n\
             ## 핵심\n\n## 출처\n\n## 메타\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/blank", "wiki/concepts/blank.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/blank"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.adopted.is_empty(), "{report:?}");
        assert_eq!(report.resynthesize.len(), 1, "{report:?}");
    }

    #[test]
    fn new_synthesis_requires_a_declared_concept_but_skip_keeps_legacy_bookkeeping() {
        for declared in [false, true] {
            for changed_input in [false, true] {
                for policy in [SynthesisPolicy::Record, SynthesisPolicy::Skip] {
                    let dir = TempDir::new().unwrap();
                    let path = PathBuf::from("wiki/concepts/topic.md");
                    std::fs::create_dir_all(dir.path().join(path.parent().unwrap())).unwrap();
                    let format = if declared { "type: concept\n" } else { "" };
                    let input = if changed_input {
                        synthesized_from(&[])
                    } else {
                        String::new()
                    };
                    let body = if changed_input {
                        "An established answer."
                    } else {
                        ""
                    };
                    let raw = format!(
                        "---\n{format}title: Topic\nsource_count: 77\nllm_inputs:\n{input}\
                         ---\n## Synthesis\n{body}\n\n## Sources\n\n## Related\n"
                    );
                    std::fs::write(dir.path().join(&path), &raw).unwrap();
                    let pages = vec![
                        build_page("wiki/concepts/topic", "wiki/concepts/topic.md", &[]),
                        build_page("daily/source", "daily/source.md", &["wiki/concepts/topic"]),
                    ];
                    let report =
                        sync_fixture(&pages, dir.path(), false, &VaultDirs::default(), policy)
                            .unwrap();
                    let updated = std::fs::read_to_string(dir.path().join(&path)).unwrap();
                    if !declared && policy == SynthesisPolicy::Record {
                        assert_eq!(report.skipped, vec![path], "{report:?}");
                        assert!(report.updated.is_empty(), "{report:?}");
                        assert!(report.resynthesize.is_empty(), "{report:?}");
                        assert_eq!(updated, raw);
                        continue;
                    }
                    assert!(report.skipped.is_empty(), "{report:?}");
                    assert!(updated.contains("source_count: 1"), "{updated}");
                    assert_compact_sources(&updated, "Sources");
                    assert_eq!(
                        report.resynthesize.len(),
                        usize::from(policy == SynthesisPolicy::Record),
                        "{report:?}"
                    );
                    if policy == SynthesisPolicy::Skip {
                        if changed_input {
                            assert!(updated.contains(&input), "{updated}");
                        } else {
                            assert!(!updated.contains("synthesis:"), "{updated}");
                        }
                    }
                }
            }
        }
    }

    /// A concept page written before the input existed carries no `llm_inputs:` mapping. The
    /// sweep DERIVES that input, so it establishes the mapping rather than skipping every
    /// page in an existing vault.
    #[test]
    fn a_page_with_no_llm_inputs_mapping_gains_one() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/legacy.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: legacy\nsource_count: 0\n---\n\n# Legacy\n\n\
             ## 핵심\n\nWritten before the input existed.\n\n## 출처\n\n\n## 메타\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/legacy", "wiki/concepts/legacy.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/legacy"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.skipped.is_empty(), "{report:?}");
        assert_eq!(report.adopted.len(), 1, "{report:?}");

        let content = std::fs::read_to_string(&path).unwrap();
        let digest = fixture_digest(&["daily/slack/2026-05-20".to_owned()]);
        assert!(
            content.contains(&format!("llm_inputs:\n  synthesis: \"{digest}\"")),
            "{content}"
        );
    }

    /// A concept NOTHING cites is owed nothing: there is no evidence for a synthesis to
    /// answer to, so a queued task would send a drain to read an empty set. It records no
    /// input either, which is what keeps `lore doctor` from reporting an unanswered one
    /// forever.
    #[test]
    fn an_uncited_concept_is_owed_no_synthesis() {
        let dir = TempDir::new().unwrap();
        write_concept_page(&dir, "orphan", &[], "");

        let pages = vec![build_page(
            "wiki/concepts/orphan",
            "wiki/concepts/orphan.md",
            &[],
        )];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.resynthesize.is_empty(), "{report:?}");
        assert_eq!(report.unchanged, 1, "{report:?}");

        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/orphan.md")).unwrap();
        assert!(!content.contains("synthesis:"), "{content}");
    }

    /// A concept whose last citation was deleted keeps the input it already answered. Its
    /// synthesis still states what the vault learned, and no rewrite could improve on it from
    /// a set that is now empty — so it is neither owed work nor left looking unanswered.
    #[test]
    fn losing_every_citation_leaves_the_answered_input_alone() {
        let dir = TempDir::new().unwrap();
        write_concept_page(
            &dir,
            "oy365",
            &["- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)"],
            &synthesized_from(&["daily/slack/2026-05-20"]),
        );

        // The citing page is gone from the vault entirely.
        let pages = vec![build_page(
            "wiki/concepts/oy365",
            "wiki/concepts/oy365.md",
            &[],
        )];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert!(report.resynthesize.is_empty(), "{report:?}");
        assert_eq!(report.updated.len(), 1, "the sources list is now empty");

        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        let digest = fixture_digest(&["daily/slack/2026-05-20".to_owned()]);
        assert!(
            content.contains(&format!("synthesis_done: \"{digest}\"")),
            "the answered input survives losing its evidence:\n{content}"
        );
    }

    /// An input the page never ANSWERED is the opposite case and is withdrawn. It promised
    /// that something would write the section from those citations; the citations are gone,
    /// so the queued task keeps classifying `current` against the input still on the page and
    /// a drain reaching it writes a synthesis of pages the vault no longer holds — stamped as
    /// answered, on a page whose sources list is empty. Nothing later could clear it: this
    /// sweep is the only thing that knows the promise was made.
    #[test]
    fn losing_every_citation_withdraws_an_input_nothing_answered() {
        let dir = TempDir::new().unwrap();
        let digest = fixture_digest(&["daily/slack/2026-05-20".to_owned()]);
        write_concept_page(
            &dir,
            "oy365",
            &["- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)"],
            &format!("  synthesis: \"{digest}\"\n"),
        );

        let pages = vec![build_page(
            "wiki/concepts/oy365",
            "wiki/concepts/oy365.md",
            &[],
        )];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(
            report.withdrawn,
            vec![PathBuf::from("wiki/concepts/oy365.md")]
        );
        assert!(report.resynthesize.is_empty(), "{report:?}");

        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        assert!(
            !content.contains("synthesis:"),
            "an input nothing can answer is removed, not left for a drain:\n{content}"
        );
        assert!(
            content.contains("llm_inputs:"),
            "the mapping itself stays — other keys may live under it:\n{content}"
        );
    }

    /// Withdrawal answers to the citation set, not to whether anything is owed. Nothing is
    /// owed for three reasons and only one of them says the promise is unkeepable: under
    /// `Skip` — the provider a CI or dev run uses precisely to touch no LLM plane — a fully
    /// cited page would lose the input its queued task is keyed to, and `queue prune` would
    /// then delete the task and its file.
    #[test]
    fn a_run_with_no_llm_plane_withdraws_nothing_from_a_cited_page() {
        let dir = TempDir::new().unwrap();
        let digest = fixture_digest(&["daily/slack/2026-05-20".to_owned()]);
        write_concept_page(
            &dir,
            "oy365",
            &["- [daily/slack/2026-05-20](../../daily/slack/2026-05-20.md)"],
            &format!("  synthesis: \"{digest}\"\n"),
        );

        let pages = vec![
            build_page("wiki/concepts/oy365", "wiki/concepts/oy365.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/oy365"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Skip,
        )
        .unwrap();
        assert!(report.withdrawn.is_empty(), "{report:?}");

        let content = std::fs::read_to_string(dir.path().join("wiki/concepts/oy365.md")).unwrap();
        assert!(
            content.contains(&format!("synthesis: \"{digest}\"")),
            "the input its queued task is keyed to survives a run with no LLM plane:\n{content}"
        );
    }

    /// A page with no frontmatter block at all has nowhere to record any of this, and is not
    /// reported as owing a synthesis: the task would name a page that cannot receive the
    /// result, and every drain would fail on it forever.
    #[test]
    fn a_page_with_no_frontmatter_is_skipped_not_queued() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/bare.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "# Bare\n\n## 핵심\n\nNo frontmatter at all.\n\n## 출처\n\n\n## 메타\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/concepts/bare", "wiki/concepts/bare.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/bare"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.skipped, vec![PathBuf::from("wiki/concepts/bare.md")]);
        assert!(report.resynthesize.is_empty(), "{report:?}");
        assert!(report.updated.is_empty(), "{report:?}");
    }

    /// A concept page with no sources heading at all is skipped, not half-written.
    ///
    /// The count and the body are one fact. Writing `source_count: 1` onto a page where no
    /// citation appears — while the report says `+1 source(s)` — is exactly the disagreement
    /// between a derived count and its evidence that this sweep exists to remove.
    #[test]
    fn a_page_with_nowhere_to_record_its_sources_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wiki/concepts/headless.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: headless\ntype: concept\ntitle: \"Headless\"\nsource_count: 0\n---\n\n\
             # Headless\n\n## Synthesis\n\nNo sources heading anywhere.\n",
        )
        .unwrap();
        let pages = vec![
            build_page("wiki/concepts/headless", "wiki/concepts/headless.md", &[]),
            build_page(
                "daily/slack/2026-05-20",
                "daily/slack/2026-05-20.md",
                &["wiki/concepts/headless"],
            ),
        ];

        let report = sync_fixture(
            &pages,
            dir.path(),
            false,
            &VaultDirs::default(),
            SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(
            report.skipped,
            vec![PathBuf::from("wiki/concepts/headless.md")]
        );
        assert!(report.updated.is_empty(), "{report:?}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("source_count: 0"),
            "the count must not claim a citation the page cannot show:\n{after}"
        );
    }
}
