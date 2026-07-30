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

/// A scanned vault page: its slug id, vault-relative path, display title, and the
/// resolved page ids of its outgoing links (deduped, first-appearance order).
///
/// Resolution happens at scan time: each internal markdown-link destination is
/// resolved against the page's own location ([`link::resolve_dest`]) and normalized
/// to a page id ([`path_slug`]). A destination that escapes the vault root keeps its
/// decoded text — it matches no page, so the graph reports it broken as written.
#[derive(Debug, Clone, Default)]
pub struct ScannedPage {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub outgoing: Vec<String>,
}

/// Walk the configured scope directories under `root` and parse every `.md` file.
pub fn scan_vault(root: &Path, config: &GraphConfig) -> Result<Vec<ScannedPage>, GraphError> {
    let exclude = build_exclude_set(&config.scope.exclude)?;
    let mut file_paths: Vec<PathBuf> = Vec::new();

    for dir in &config.scope.dirs {
        let scan_dir = root.join(dir);
        if !scan_dir.exists() {
            return Err(GraphError::ScanDirNotFound(scan_dir));
        }

        // Dot-directories are never knowledge. `.obsidian` is the app's own config, `.trash` is
        // where Obsidian puts a DELETED page — resolving a link to one would report a deleted
        // page as present — and `.lorekeeper` is this tool's state. The filter lives here rather
        // than at the call sites because it is a fact about a vault, and it is what makes
        // scanning from the vault root (the existence universe) mean "every page there is".
        let walker = WalkDir::new(&scan_dir)
            .follow_links(config.scope.follow_links)
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with('.'))
            });
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "skipping unreadable path");
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md") {
                let rel = path.strip_prefix(root).unwrap_or(path);
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if !exclude.is_match(&rel_str) {
                    file_paths.push(path.to_path_buf());
                }
            }
        }
    }

    // Deduplicating on the path TEXT is not enough, and the text is exactly what differs: two
    // scope directories can name ONE directory while spelling it differently — case on a
    // case-insensitive filesystem, NFC against NFD on APFS, a symlink — and the same physical
    // file then arrives under both prefixes. Both copies carry the same page id, but a page's
    // type is decided by prefix-matching its raw path, so one copy reads as a concept page
    // and the other as a page citing it, and `backlinks-sync` writes the concept's own
    // curated cross-references into its sources. Canonical identity is the filesystem's
    // answer to "the same file", which is the question being asked; a path that cannot be
    // canonicalized stands for itself, since a file that no longer resolves cannot collide
    // with one that does.
    //
    // Resolved in WALK order, before sorting: the surviving spelling is the one the
    // earliest-listed scope directory gave it, and that order is the vault's own precedence
    // (`wiki` first). Keeping the sort-first copy would let an alias outrank the directory
    // every other page addresses, so links to it from elsewhere would resolve nowhere.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    file_paths
        .retain(|path| seen.insert(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())));
    file_paths.sort();

    let mut pages: Vec<ScannedPage> = file_paths
        .par_iter()
        .filter_map(|path| match parse_file(path, root) {
            Ok(page) => Some(page),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping unparseable page");
                None
            }
        })
        .collect();

    pages.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(pages)
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
        let target = match link::resolve_dest(rel, &dest) {
            Some(resolved) => path_slug(&resolved),
            // Escapes the vault root: keep the destination as written — it matches
            // no page id, so the graph reports it broken rather than dropping it.
            None => dest,
        };
        if !target.is_empty() && seen.insert(target.clone()) {
            outgoing.push(target);
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

/// The vault-wide existence universe consulted by integrity checks
/// (broken-link resolution and orphan detection) so they reason about *every*
/// page on disk — not just the analysis scope (`graph.scope.dirs`).
///
/// Without it, a `wiki/` concept linking a `daily/` page would be reported as a
/// broken link, and a `wiki/` concept linked only from `daily/` pages would be
/// reported as an orphan — both false positives caused by the narrow analysis
/// scope. Built from a full-vault scan ([`scan_vault`] with all page dirs).
#[derive(Debug, Clone)]
pub struct VaultExistence {
    /// Every page id the scan found, INCLUDING the generated catalogs — the question
    /// "is a file addressed by this id" has one answer, and a catalog is a file a page may
    /// legitimately link.
    ids: HashSet<String>,
    /// `ids` minus the generated catalogs: the connectivity question orphan detection asks.
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
        // artifacts, not knowledge nodes. They are real files, so they RESOLVE as link targets;
        // what they must not do is count as connectivity, because index.md links every page it
        // catalogs and would mark every concept "linked", defeating orphan detection.
        let is_reserved = reserved_page_predicate(Path::new(&dirs.wiki));
        let mut ids = HashSet::with_capacity(pages.len());
        let mut knowledge = HashSet::with_capacity(pages.len());
        for page in pages {
            ids.insert(page.id.clone());
            if !is_reserved(page.id.as_str()) {
                knowledge.insert(page.id.clone());
            }
        }

        let mut linked = HashSet::new();
        for page in pages {
            if is_reserved(page.id.as_str()) {
                continue;
            }
            for target in &page.outgoing {
                if *target != page.id && knowledge.contains(target) {
                    linked.insert(target.clone());
                }
            }
        }

        Self {
            ids,
            knowledge,
            linked,
        }
    }

    /// Whether a resolved link target addresses a page in the vault — a knowledge page or a
    /// generated catalog, both of which are files on disk.
    pub fn is_resolvable(&self, target: &str) -> bool {
        self.ids.contains(target)
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
/// page still EXISTS, so a link to it resolves, and its existence was never the exclusion's
/// business: building the existence universe with the globs applied reported
/// `wiki/concepts/excluded.md` as a missing destination while the file sat on disk, the same
/// defect as leaving a user's own folder unscanned. So integrity commands scan without them and
/// apply this to the node set instead.
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
    fn vault_existence_tracks_ids_and_linked() {
        let pages = vec![
            ScannedPage {
                id: "daily/team-slack/2026-05-22".to_owned(),
                path: PathBuf::from("daily/team-slack/2026-05-22.md"),
                title: "t".to_owned(),
                outgoing: vec!["wiki/concepts/confluence-cloud".to_owned()],
            },
            ScannedPage {
                id: "wiki/concepts/confluence-cloud".to_owned(),
                path: PathBuf::from("wiki/concepts/confluence-cloud.md"),
                title: "Confluence Cloud".to_owned(),
                outgoing: vec![],
            },
        ];
        let ex = VaultExistence::build(&pages, &VaultDirs::default());
        assert!(ex.is_resolvable("daily/team-slack/2026-05-22"));
        assert!(ex.is_resolvable("wiki/concepts/confluence-cloud"));
        assert!(!ex.is_resolvable("nope"));
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
        assert_eq!(page.outgoing, vec!["wiki/concepts/kubernetes"]);
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
        assert_eq!(page.outgoing, vec!["wiki/concepts/b"]);
    }

    #[test]
    fn parse_file_keeps_escaping_dest_as_broken_target() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("a.md"), "[out](../../outside/x.md)\n").unwrap();
        let page = parse_file(&wiki.join("a.md"), tmp.path()).unwrap();
        // The destination escapes the vault root: kept as written, never resolvable.
        assert_eq!(page.outgoing, vec!["../../outside/x.md"]);
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
    fn a_page_reached_through_two_names_for_one_directory_is_scanned_once() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("a.md"), "# A\n\n[b](b.md)\n").unwrap();
        std::fs::write(wiki.join("b.md"), "# B\n").unwrap();
        if !symlink_dir(&wiki, &tmp.path().join("notes")) {
            return;
        }

        let mut config = GraphConfig::default();
        config.scope.dirs = vec![PathBuf::from("wiki"), PathBuf::from("notes")];
        let pages = scan_vault(tmp.path(), &config).unwrap();

        let ids: Vec<&str> = pages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["wiki/a", "wiki/b"], "each page exactly once");
    }

    #[test]
    fn scan_respects_exclude_and_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("keep.md"), "# Keep\n").unwrap();
        std::fs::write(wiki.join("skip.md"), "# Skip\n").unwrap();

        let mut config = GraphConfig::default();
        config.scope.dirs = vec![PathBuf::from("wiki")];
        config.scope.exclude = vec!["wiki/skip.md".to_string()];
        let pages = scan_vault(tmp.path(), &config).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].id, "wiki/keep");

        config.scope.dirs = vec![PathBuf::from("nonexistent")];
        assert!(scan_vault(tmp.path(), &config).is_err());
    }
}
