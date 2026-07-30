//! Vault scan: walk markdown files and build [`ScannedPage`] records.
//!
//! The pure domain logic — slug normalization, frontmatter parsing, and markdown-link
//! extraction/resolution — lives in `lk-core` (`slugify`, `frontmatter::parse_page`,
//! `link`). This module keeps only the I/O concerns: filesystem walking (walkdir +
//! rayon) and assembling [`ScannedPage`]s.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use lk_core::concept::slugify;
use lk_core::config::{GraphConfig, VaultDirs};
use lk_core::frontmatter::{self, Frontmatter};
use lk_core::link;
use lk_core::vault_path::{CONCEPTS_SUBDIR, DOCUMENTS_SUBDIR, EXPLORATIONS_SUBDIR};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::GraphError;

/// A scanned vault page: its slug id, vault-relative path, display title, and its outgoing
/// links (deduped by address, first-appearance order).
///
/// Resolution happens at scan time: each internal markdown-link destination is resolved
/// against the page's own location ([`link::resolve_dest`]), so every downstream consumer is a
/// plain lookup with no second resolution to disagree about.
#[derive(Debug, Clone, Default)]
pub struct ScannedPage {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub outgoing: Vec<Link>,
}

/// One resolved link, carried as both of the things a consumer asks it for.
///
/// The two are not interchangeable, and conflating them answers the wrong question. `dest` is
/// an ADDRESS — the vault-relative path the link names — and only a path can answer "is there a
/// file there". `id` is the page id that address belongs to, which is the graph's node key.
/// Slugifying a path is lossy, so `Bad_Name.md` and `bad-name.md` share an id while only one of
/// them is a file: checking existence by id reports a link to the other one sound.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Link {
    pub dest: String,
    pub id: String,
}

impl Link {
    /// A link to a vault-relative address. The id is DERIVED, so the pair cannot drift.
    pub fn to(dest: &str) -> Self {
        Self {
            id: path_slug(Path::new(dest)),
            dest: dest.to_owned(),
        }
    }
}

/// Walk the vault at `root` and parse every markdown file it holds.
///
/// The walk is the WHOLE vault, always. Which pages are analysed, and whose links are checked,
/// are questions about the result — [`VaultViews`] answers them by filtering — because a walk
/// that had already dropped a page cannot tell "not there" from "not looked at", in whichever
/// direction a caller happens to resolve it.
pub fn scan_vault(root: &Path, follow_links: bool) -> Result<Vec<ScannedPage>, GraphError> {
    if !root.exists() {
        return Err(GraphError::ScanDirNotFound(root.to_path_buf()));
    }

    // Dot-directories are never knowledge: `.obsidian` is the app's own config, `.trash` is
    // where Obsidian puts a DELETED page — resolving a link to one would report a deleted
    // page as present — and `.lorekeeper` is this tool's state.
    let walker = WalkDir::new(root)
        .follow_links(follow_links)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'))
        });

    let mut file_paths: Vec<PathBuf> = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "skipping unreadable path");
                continue;
            }
        };
        let path = entry.path();
        // `is_file` FOLLOWS the link, so a symlinked page is a page: it is readable at exactly
        // this address, and a link to it resolves in every editor. `follow_links` decides
        // whether the walk descends symlinked directories, which is a separate question.
        if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
            file_paths.push(path.to_path_buf());
        }
    }
    file_paths.sort();

    // A page whose body will not parse still EXISTS, and its id comes from the path rather than
    // from anything inside it — so it yields a page with no title and no links rather than
    // vanishing. Dropping it would make a link to a real file on disk read as broken, and leave
    // it out of every count of what the vault holds.
    let mut pages: Vec<ScannedPage> = file_paths
        .par_iter()
        .map(|path| {
            parse_file(path, root).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "unparseable page: id only");
                let rel = path.strip_prefix(root).unwrap_or(path);
                ScannedPage {
                    id: path_slug(rel),
                    path: rel.to_path_buf(),
                    title: path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("untitled")
                        .to_owned(),
                    outgoing: Vec::new(),
                }
            })
        })
        .collect();

    pages.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(pages)
}

/// ONE scan of the vault, partitioned into the three views that answer three different
/// questions. Conflating any two of them reports something false.
pub struct VaultViews {
    /// Every page in the vault. The existence universe, and the view the mutating commands
    /// repoint links across — a citation of a renamed or merged page lives wherever it was
    /// written.
    pub scanned: Vec<ScannedPage>,
    pub existence: VaultExistence,
    /// The pages this tool writes and manages: the analysis scope ∪ every page directory. A
    /// vault also holds the user's own folders, and a stray link in a hand-written note is
    /// neither this tool's output nor something it can repair.
    pub link_sources: Vec<ScannedPage>,
    /// The analysis scope — the graph's nodes and edges.
    pub pages: Vec<ScannedPage>,
}

impl VaultViews {
    /// `scope.exclude` and `scope.dirs` narrow the last two views only: an excluded page still
    /// exists, so it still resolves a link and still has its own links repointed by a rename.
    pub fn resolve(root: &Path, graph: &GraphConfig, dirs: &VaultDirs) -> Result<Self, GraphError> {
        let scanned = scan_vault(root, graph.scope.follow_links)?;
        let existence = VaultExistence::build(&scanned, dirs);
        let excluded = Excludes::compile(&graph.scope.exclude)?;

        let managed = page_dirs(root, dirs);
        let in_scope = |page: &ScannedPage, scope: &[PathBuf]| {
            scope.iter().any(|dir| under(&page.id, dir)) && !excluded.matches(&page.path)
        };

        Ok(Self {
            link_sources: scanned
                .iter()
                .filter(|p| in_scope(p, &graph.scope.dirs) || in_scope(p, &managed))
                .cloned()
                .collect(),
            pages: scanned
                .iter()
                .filter(|p| in_scope(p, &graph.scope.dirs))
                .cloned()
                .collect(),
            scanned,
            existence,
        })
    }
}

/// Whether a page id sits under a configured vault-relative directory.
///
/// Compared on the ID, not the raw path: an id is the address after normalization, so a vault
/// whose directory on disk is spelled `Wiki` while the config says `wiki` still matches. A raw
/// prefix test answers no there — `is_dir` passes on a case-insensitive volume, so the directory
/// "exists", no page ever matches it, and every graph command reports an empty vault and exits 0.
fn under(id: &str, dir: &Path) -> bool {
    let prefix = path_slug(dir);
    if prefix.is_empty() {
        return true;
    }
    id == prefix || id.starts_with(&format!("{prefix}/"))
}

/// Every vault-relative page directory that exists on disk — anything this tool writes a page
/// into. Missing directories are skipped so a partially-populated vault doesn't error out.
fn page_dirs(root: &Path, dirs: &VaultDirs) -> Vec<PathBuf> {
    [&dirs.wiki, &dirs.daily, &dirs.personal, &dirs.synthesis]
        .iter()
        .filter(|name| root.join(name).is_dir())
        .map(|s| PathBuf::from(s.as_str()))
        .collect()
}

fn parse_file(path: &Path, root: &Path) -> Result<ScannedPage, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

    let parsed = frontmatter::parse_page(&raw)?;

    let title = frontmatter_title(&parsed.frontmatter)
        .or_else(|| extract_first_heading(&parsed.body))
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("untitled")
                .to_owned()
        });

    let rel = path.strip_prefix(root).unwrap_or(path);
    let id = path_slug(rel);

    let mut outgoing = Vec::new();
    let mut seen = HashSet::new();
    for dest in link::extract_dests(&parsed.body) {
        // Only `.md` destinations address knowledge pages; a link to any other file
        // (an attachment) is not a graph edge.
        if !Path::new(&dest).extension().is_some_and(|ext| ext == "md") {
            continue;
        }
        let address = match link::resolve_dest(rel, &dest) {
            Some(resolved) => rel_str(&resolved),
            // Escapes the vault root: keep the destination as written — it names no file in
            // the vault, so it is reported broken rather than dropped.
            None => dest,
        };
        // Deduped by ADDRESS, not by id: two dead spellings of one id are two broken links,
        // and each names its own repair.
        if !address.is_empty() && seen.insert(address.clone()) {
            outgoing.push(Link::to(&address));
        }
    }

    Ok(ScannedPage {
        id,
        path: rel.to_path_buf(),
        title,
        outgoing,
    })
}

fn frontmatter_title(fm: &Frontmatter) -> Option<String> {
    fm.get("title")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

fn extract_first_heading(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("# ") {
            return Some(heading.trim().to_owned());
        }
    }
    None
}

/// Every page on disk, for the integrity checks that must reason about the vault rather than
/// the analysis scope (`graph.scope.dirs`): a `wiki/` concept linking a `daily/` page is not a
/// broken link, and a concept linked only from `daily/` is not an orphan.
///
/// Built from a scan of the vault ROOT, so "absent" means absent from the vault.
#[derive(Debug, Clone)]
pub struct VaultExistence {
    /// Every page ADDRESS the scan found — the vault-relative path as spelled on disk, catalogs
    /// INCLUDED, since a catalog is a file a page may legitimately link.
    paths: HashSet<String>,
    /// Every page id, minus the generated catalogs: the connectivity question orphan detection
    /// asks.
    /// `index.md` links every page it catalogs, so counting a link to it as a connection would
    /// mask exactly the orphans that detection exists to find.
    knowledge: HashSet<String>,
    /// Page ids that are the resolved target of a link from *another* page
    /// (self-links excluded). Drives orphan inbound exemption.
    linked: HashSet<String>,
}

impl VaultExistence {
    /// Derive the universe from a page scan. The caller passes every page the vault has —
    /// integrity commands scan from the vault ROOT for exactly this reason — so an id absent
    /// from `ids` is absent from the vault, not merely from somewhere nobody looked.
    pub fn build(pages: &[ScannedPage], dirs: &VaultDirs) -> Self {
        // Navigation/catalog meta-files (index.md, log.md, map.md, AGENTS.md) are generated
        // artifacts, not knowledge. They are real files, so they RESOLVE as link targets; what
        // they must not do is count as connectivity, since index.md links every page it catalogs
        // and would mark every concept "linked", defeating orphan detection.
        let is_reserved = reserved_page_predicate(Path::new(&dirs.wiki));
        let mut paths = HashSet::with_capacity(pages.len());
        let mut knowledge = HashSet::with_capacity(pages.len());
        for page in pages {
            paths.insert(rel_str(&page.path));
            if !is_reserved(page.id.as_str()) {
                knowledge.insert(page.id.clone());
            }
        }

        let mut linked = HashSet::new();
        for page in pages {
            if is_reserved(page.id.as_str()) {
                continue;
            }
            for link in &page.outgoing {
                if link.id != page.id && knowledge.contains(&link.id) {
                    linked.insert(link.id.clone());
                }
            }
        }

        Self {
            paths,
            knowledge,
            linked,
        }
    }

    /// Whether a link's address names a file in the vault — a knowledge page or a generated
    /// catalog, both of which are files on disk.
    pub fn is_resolvable(&self, dest: &str) -> bool {
        self.paths.contains(dest)
    }

    /// Whether a resolved link target addresses a KNOWLEDGE page — the question orphan
    /// connectivity asks, where reaching a generated catalog is not reaching anything.
    pub fn is_knowledge(&self, target: &str) -> bool {
        self.knowledge.contains(target)
    }

    /// Whether `page_id` is the resolved target of a link from another page.
    pub fn is_linked(&self, page_id: &str) -> bool {
        self.linked.contains(page_id)
    }
}

/// Whether a page is a concept page (`{dirs.wiki}/concepts/{slug}.md`). Anchored to
/// the configured `dirs.wiki` — never a hardcoded path segment — and shared by
/// `backlinks` and the concept lints.
pub(crate) fn is_concept_page(path: &Path, dirs: &VaultDirs) -> bool {
    let s = path.to_string_lossy().replace('\\', "/");
    s.starts_with(&format!("{}/{CONCEPTS_SUBDIR}/", dirs.wiki))
        && path.extension().is_some_and(|ext| ext == "md")
}

/// True iff this vault-relative path can act as a citation *source* — an event,
/// work-log, synthesis, document, or exploration page where a concept appearance is
/// meaningful provenance. Concept-to-concept links and navigation pages are excluded:
/// cross-references between concepts are curated structure (`## Related`), not activity.
/// Single-sourced here as THE definition of "a real source" — `backlinks` (what fills
/// `## Sources`) resolves citations through it.
pub(crate) fn is_valid_source(path: &Path, dirs: &VaultDirs) -> bool {
    let s = path.to_string_lossy().replace('\\', "/");
    s.starts_with(&format!("{}/", dirs.daily))
        || s.starts_with(&format!("{}/", dirs.personal))
        || s.starts_with(&format!("{}/", dirs.synthesis))
        || s.starts_with(&format!("{}/{DOCUMENTS_SUBDIR}/", dirs.wiki))
        || s.starts_with(&format!("{}/{EXPLORATIONS_SUBDIR}/", dirs.wiki))
}

/// Page ids of Lorekeeper's reserved wiki meta files (the index catalog, the time log,
/// the navigation map, and the AGENTS.md schema doc) under `wiki_dir`, e.g. `wiki/index`,
/// `wiki/log`, `wiki/map`, `wiki/agents`.
/// Single-sourced from [`lk_core::vault_path::RESERVED_WIKI_FILES`] so the graph's
/// orphan / index-drift checks exclude exactly what the index builder skips.
pub fn reserved_page_ids(wiki_dir: &Path) -> Vec<String> {
    lk_core::vault_path::RESERVED_WIKI_FILES
        .iter()
        .map(|name| {
            let stem = Path::new(name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(name);
            path_slug(&wiki_dir.join(stem))
        })
        .collect()
}

/// Returns a predicate matching any reserved navigation/catalog meta page
/// (`index.md`/`log.md`/`map.md`/`AGENTS.md`) that must stay out of the analysis graph
/// (nodes AND edges): they link every page, so as nodes they would be spurious mega-hubs
/// that merge separate communities and mask real orphans.
pub fn reserved_page_predicate(wiki_dir: &Path) -> impl Fn(&str) -> bool {
    let ids: HashSet<String> = reserved_page_ids(wiki_dir).into_iter().collect();
    move |id: &str| ids.contains(id)
}

/// A vault-relative path as one comparable address: separators normalized to `/`, nothing else
/// touched. The spelling is the address — it is what the filesystem answers to.
pub fn rel_str(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Two files whose paths slugify to ONE page id.
///
/// The id is the graph's node key, so only one of them can hold the node: the other's edges are
/// attributed to its twin, and it is absent from every list derived from the node map while
/// still counted in the totals. `wiki/documents/A B.md` beside `wiki/documents/a-b.md` is enough
/// — no exotic filesystem needed — which is why this is reported rather than resolved: which of
/// the two the tool should keep is not its call to make.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AddressCollision {
    pub id: String,
    pub paths: Vec<String>,
}

pub fn address_collisions(pages: &[ScannedPage]) -> Vec<AddressCollision> {
    let mut by_id: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    for page in pages {
        by_id
            .entry(page.id.as_str())
            .or_default()
            .push(rel_str(&page.path));
    }
    by_id
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(id, mut paths)| {
            paths.sort();
            AddressCollision {
                id: id.to_owned(),
                paths,
            }
        })
        .collect()
}

/// Slug id for a vault-relative path: drop the extension, normalize separators to `/`,
/// then slugify each path segment (so `wiki/Concept A.md` → `wiki/concept-a`).
pub fn path_slug(rel: &Path) -> String {
    let no_ext = rel.with_extension("");
    let s = no_ext.to_string_lossy().replace('\\', "/");
    s.split('/')
        .filter_map(slugify)
        .collect::<Vec<_>>()
        .join("/")
}

/// The `graph.scope.exclude` globs as a predicate over a vault-relative path.
///
/// Exclusion narrows the ANALYSIS — which pages are graph nodes — not the vault. An excluded
/// page still exists, so a link to it resolves; its existence is not the exclusion's business.
/// Integrity commands therefore scan without the globs and apply this to the node set.
pub struct Excludes(GlobSet);

impl Excludes {
    pub fn compile(patterns: &[String]) -> Result<Self, GraphError> {
        build_exclude_set(patterns).map(Self)
    }

    /// Matched against the same form the scan uses: vault-relative, `/`-separated.
    pub fn matches(&self, rel: &Path) -> bool {
        self.0
            .is_match(rel.to_string_lossy().replace('\\', "/").as_str())
    }
}

fn build_exclude_set(patterns: &[String]) -> Result<GlobSet, GraphError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .map_err(|e| GraphError::InvalidExclude(pattern.clone(), e.to_string()))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| GraphError::InvalidExclude("<set>".to_string(), e.to_string()))
}

#[cfg(test)]
mod tests {
    /// Links as the ADDRESSES they name, which is what existence is asked about.
    fn dests(page: &ScannedPage) -> Vec<&str> {
        page.outgoing.iter().map(|l| l.dest.as_str()).collect()
    }

    /// A directory symlink, or `false` where the platform or filesystem refuses one. Windows
    /// needs a privilege most accounts lack, so the caller treats absence as "skip this case".
    fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> bool {
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(target, link);
        made.is_ok()
    }

    use super::*;

    #[test]
    fn reserved_predicate_excludes_only_meta_files() {
        let is_reserved = reserved_page_predicate(Path::new("wiki"));
        assert!(is_reserved("wiki/index"));
        assert!(is_reserved("wiki/log"));
        assert!(is_reserved("wiki/map"));
        assert!(is_reserved("wiki/agents"));
        // Real knowledge nodes are NOT reserved.
        assert!(!is_reserved("wiki/concepts/rag"));
        assert!(!is_reserved("wiki/documents/report"));
        // A concept whose slug merely contains "index" is not a meta page.
        assert!(!is_reserved("wiki/concepts/index-fund"));
    }

    /// `is_valid_source` is the single definition of what may cite a concept, and
    /// `backlinks-sync` derives every concept's sources section and `source_count` from
    /// exactly the pages it admits — wiping any entry not backed by an admitted one. So a
    /// page class dropped here does not merely go uncounted: its citations are erased from
    /// the concept pages that carry them, with nothing left recording that they existed.
    /// Each accepted root therefore needs its own case; covering one incidentally leaves the
    /// rest free to be narrowed silently.
    #[test]
    fn every_page_class_that_may_cite_a_concept_is_admitted() {
        let dirs = VaultDirs::default();
        for cites in [
            "daily/team-slack/2026-05-22.md",
            "me/work-log/2026-05-22.md",
            "synthesis/weekly/2026-W21.md",
            "wiki/documents/report.md",
            "wiki/explorations/why-rag.md",
        ] {
            assert!(
                is_valid_source(Path::new(cites), &dirs),
                "{cites} may cite a concept"
            );
        }
        // A concept's own links are curated structure, not provenance, and the meta pages
        // are generated FROM the graph — admitting either would cite from the output.
        for not_a_source in [
            "wiki/concepts/rag.md",
            "wiki/index.md",
            "wiki/log.md",
            "wiki/map.md",
            "wiki/AGENTS.md",
        ] {
            assert!(
                !is_valid_source(Path::new(not_a_source), &dirs),
                "{not_a_source} is not provenance"
            );
        }
    }

    #[test]
    fn an_unparseable_page_still_exists() {
        // Its id comes from the PATH, so nothing inside the file is needed to know it is there.
        // Dropping it would make a link to a real file on disk read as broken.
        let tmp = tempfile::TempDir::new().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("good.md"), "---\nid: good\n---\n\n# Good\n").unwrap();
        std::fs::write(wiki.join("broken.md"), "---\nid: broken\ntitle: unclosed\n").unwrap();

        let pages = scan_vault(tmp.path(), false).unwrap();

        let ids: Vec<&str> = pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["wiki/broken", "wiki/good"]);
        let unparseable = pages.iter().find(|p| p.id == "wiki/broken").unwrap();
        assert!(
            unparseable.outgoing.is_empty(),
            "a page whose body did not parse contributes no links"
        );
    }

    #[test]
    fn vault_existence_tracks_ids_and_linked() {
        let pages = vec![
            ScannedPage {
                id: "daily/team-slack/2026-05-22".to_owned(),
                path: PathBuf::from("daily/team-slack/2026-05-22.md"),
                title: "t".to_owned(),
                outgoing: vec![Link::to("wiki/concepts/confluence-cloud.md")],
            },
            ScannedPage {
                id: "wiki/concepts/confluence-cloud".to_owned(),
                path: PathBuf::from("wiki/concepts/confluence-cloud.md"),
                title: "Confluence Cloud".to_owned(),
                outgoing: vec![],
            },
        ];
        let ex = VaultExistence::build(&pages, &VaultDirs::default());
        assert!(ex.is_resolvable("daily/team-slack/2026-05-22.md"));
        assert!(ex.is_resolvable("wiki/concepts/confluence-cloud.md"));
        assert!(!ex.is_resolvable("nope.md"));
        // The daily page links the concept → its page id is a link target.
        assert!(ex.is_linked("wiki/concepts/confluence-cloud"));
        // The daily page itself is linked by nobody.
        assert!(!ex.is_linked("daily/team-slack/2026-05-22"));
    }

    #[test]
    fn path_slug_basic() {
        assert_eq!(path_slug(Path::new("wiki/Concept A.md")), "wiki/concept-a");
        assert_eq!(path_slug(Path::new("wiki/Bad_Name.md")), "wiki/bad-name");
    }

    #[test]
    fn path_slug_preserves_directory_structure() {
        assert_eq!(
            path_slug(Path::new("wiki/sub/Topic Name.md")),
            "wiki/sub/topic-name"
        );
    }

    #[test]
    fn parse_file_resolves_links_against_page_location() {
        let tmp = tempfile::tempdir().unwrap();
        let daily = tmp.path().join("daily/x");
        std::fs::create_dir_all(&daily).unwrap();
        std::fs::write(
            daily.join("2026-05-22.md"),
            "---\ntitle: Day\n---\n\n# Day\n\n\
             [K](../../wiki/concepts/kubernetes.md) and again \
             [K](../../wiki/concepts/kubernetes.md), plus [ext](https://x.y/z.md).\n",
        )
        .unwrap();
        let page = parse_file(&daily.join("2026-05-22.md"), tmp.path()).unwrap();
        assert_eq!(page.id, "daily/x/2026-05-22");
        assert_eq!(page.title, "Day");
        // Both links resolve to the same page id and dedupe; the external is skipped.
        assert_eq!(dests(&page), vec!["wiki/concepts/kubernetes.md"]);
    }

    #[test]
    fn parse_file_skips_non_md_destinations() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("a.md"),
            "[pdf](../files/report.pdf) and [page](concepts/b.md)\n",
        )
        .unwrap();
        let page = parse_file(&wiki.join("a.md"), tmp.path()).unwrap();
        assert_eq!(dests(&page), vec!["wiki/concepts/b.md"]);
    }

    #[test]
    fn parse_file_keeps_escaping_dest_as_broken_target() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("a.md"), "[out](../../outside/x.md)\n").unwrap();
        let page = parse_file(&wiki.join("a.md"), tmp.path()).unwrap();
        // The destination escapes the vault root: kept as written, never resolvable.
        assert_eq!(dests(&page), vec!["../../outside/x.md"]);
    }

    #[test]
    fn title_falls_back_to_heading_then_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("h.md"), "# Just A Heading\n\nbody\n").unwrap();
        std::fs::write(wiki.join("s.md"), "no heading here\n").unwrap();
        let h = parse_file(&wiki.join("h.md"), tmp.path()).unwrap();
        let s = parse_file(&wiki.join("s.md"), tmp.path()).unwrap();
        assert_eq!(h.title, "Just A Heading");
        assert_eq!(s.title, "s");
    }

    /// Two scope directories that name ONE directory must yield each page once. The
    /// filesystem calls them the same directory for spellings the config sees as distinct —
    /// case on macOS, NFC against NFD on APFS, a `.` segment — and a page scanned twice is
    /// scanned under two prefixes, so one copy types as a concept page and the other as a
    /// page citing it. `backlinks-sync` then writes the concept's own curated
    /// cross-references into its sources, which is the one thing citation derivation must
    /// never do. A `.` segment will not stand in for the case here: Rust's `Path` equality
    /// already folds it, so the text dedup this replaced would pass such a case while still
    /// failing on the spellings that actually occur. A second name for one directory is the
    /// real shape, and a symlink is the portable way to make one.
    #[test]
    fn a_page_reachable_at_two_addresses_exists_at_both() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("a.md"), "# A\n\n[b](b.md)\n").unwrap();
        std::fs::write(wiki.join("b.md"), "# B\n").unwrap();
        if !symlink_dir(&wiki, &tmp.path().join("notes")) {
            return;
        }

        // `follow_links` is the vault owner declaring that a symlink is part of the vault, so
        // both spellings are addresses a reader can open — and a link to either resolves.
        // Collapsing them onto one would make the other read as broken, and which of the two
        // survived would depend on readdir order.
        let pages = scan_vault(tmp.path(), true).unwrap();
        let ids: Vec<&str> = pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["notes/a", "notes/b", "wiki/a", "wiki/b"]);

        // Left alone, a symlink is not walked into, so the vault is what the walk finds.
        let pages = scan_vault(tmp.path(), false).unwrap();
        let ids: Vec<&str> = pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["wiki/a", "wiki/b"]);
    }

    #[test]
    fn the_views_narrow_what_is_analysed_and_never_what_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("keep.md"), "# Keep\n").unwrap();
        std::fs::write(wiki.join("skip.md"), "# Skip\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("private")).unwrap();
        std::fs::write(tmp.path().join("private/note.md"), "# Note\n").unwrap();

        let mut config = GraphConfig::default();
        config.scope.dirs = vec![PathBuf::from("wiki")];
        config.scope.exclude = vec!["wiki/skip.md".to_string()];
        let views = VaultViews::resolve(tmp.path(), &config, &VaultDirs::default()).unwrap();

        assert_eq!(
            views
                .pages
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["wiki/keep"],
            "the analysis scope is narrowed by both settings"
        );
        for id in ["wiki/keep", "wiki/skip", "private/note"] {
            assert!(
                views.existence.is_resolvable(&format!("{id}.md")),
                "{id} exists whatever the analysis scope is"
            );
        }
        assert!(
            !views
                .link_sources
                .iter()
                .any(|p| p.id == "private/note" || p.id == "wiki/skip"),
            "neither a user's own folder nor an excluded page is this tool's to lint"
        );
    }

    #[test]
    fn a_vault_root_that_is_not_there_is_an_error() {
        assert!(scan_vault(Path::new("/nonexistent-vault-root"), false).is_err());
    }

    #[test]
    fn a_directory_matches_the_configured_spelling_the_filesystem_folded() {
        // `is_dir` passes on a case-insensitive volume, so the directory "exists" while a raw
        // prefix test matches no page — every graph command then reports an empty vault, green.
        assert!(under("wiki/concepts/x", Path::new("Wiki")));
        assert!(under("wiki/concepts/x", Path::new("wiki")));
        assert!(!under("wikipedia/x", Path::new("wiki")));
        assert!(under("anything", Path::new("")));
    }

    #[test]
    fn two_files_at_one_address_are_reported() {
        let pages = vec![
            ScannedPage {
                id: "wiki/documents/a-b".to_owned(),
                path: PathBuf::from("wiki/documents/A B.md"),
                ..ScannedPage::default()
            },
            ScannedPage {
                id: "wiki/documents/a-b".to_owned(),
                path: PathBuf::from("wiki/documents/a-b.md"),
                ..ScannedPage::default()
            },
            ScannedPage {
                id: "wiki/documents/c".to_owned(),
                path: PathBuf::from("wiki/documents/c.md"),
                ..ScannedPage::default()
            },
        ];

        let collisions = address_collisions(&pages);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].id, "wiki/documents/a-b");
        assert_eq!(
            collisions[0].paths,
            vec!["wiki/documents/A B.md", "wiki/documents/a-b.md"]
        );
    }
}
