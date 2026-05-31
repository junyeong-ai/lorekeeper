//! Vault scan: walk markdown files and build [`ScannedPage`] records.
//!
//! slug normalization, frontmatter parsing, and wikilink extraction — now live in
//! `lk-core` (`slugify`, `frontmatter::parse_page`, `wikilink`). This module keeps only
//! the I/O concerns: filesystem walking (walkdir + rayon) and assembling [`ScannedPage`]s.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use lk_core::concept::slugify;
use lk_core::config::{GraphConfig, VaultDirs};
use lk_core::frontmatter::{self, Frontmatter};
use lk_core::wikilink;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::GraphError;

/// A scanned vault page: its slug id, vault-relative path, display title, the
/// slugified targets of its outgoing wikilinks (deduped, first-appearance order),
/// and any declared alias slugs.
#[derive(Debug, Clone, Default)]
pub struct ScannedPage {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub outgoing: Vec<String>,
    /// Slugified `aliases` frontmatter — alternate names a bare `[[alias]]` link
    /// resolves to (concept pages only, in practice). The page's own stem slug is
    /// excluded (a self-alias is a no-op), so this holds only genuine synonyms.
    pub aliases: Vec<String>,
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

        let walker = WalkDir::new(&scan_dir).follow_links(config.scope.follow_links);
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

    file_paths.sort();
    file_paths.dedup();

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
    for raw_target in wikilink::extract_wikilinks(&parsed.body) {
        let target = resolve_wikilink_target(&raw_target);
        if !target.is_empty() && seen.insert(target.clone()) {
            outgoing.push(target);
        }
    }

    // Declared aliases (slugified). The page's own stem slug is dropped — the concept
    // template seeds `aliases: [name]`, which slugifies back to the page slug and is a
    // no-op resolution target. Only genuine synonyms remain.
    let self_slug = rel.file_stem().and_then(|s| s.to_str()).and_then(slugify);
    let mut alias_seen = HashSet::new();
    let aliases = parsed
        .frontmatter
        .get("aliases")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.as_str())
                .filter_map(slugify)
                .filter(|a| Some(a) != self_slug.as_ref())
                .filter(|a| alias_seen.insert(a.clone()))
                .collect()
        })
        .unwrap_or_default();

    Ok(ScannedPage {
        id,
        path: rel.to_path_buf(),
        title,
        outgoing,
        aliases,
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
#[derive(Debug, Clone, Default)]
pub struct VaultExistence {
    /// Filename slug → page id. A bare `[[name]]` targets the knowledge node, so a
    /// concept page claims its slug ahead of a same-named non-concept (documents are
    /// referenced by their path form); within a class, first insertion wins. Mirrors
    /// the precedence in [`crate::graph::WikiGraph`] so the two resolve a bare
    /// `[[name]]` to the same page, deterministically regardless of scan order.
    by_filename: HashMap<String, String>,
    /// Alias slug → concept page id, the LOWEST-precedence resolution layer. A
    /// concept's declared `aliases` let a bare `[[synonym]]` resolve to it, but only
    /// when the synonym isn't already a real id or filename slug (those always win).
    by_alias: HashMap<String, String>,
    /// Every page id — the form a path-style `[[dir/sub/name]]` link resolves to.
    ids: HashSet<String>,
    /// Page ids that are the resolved target of a wikilink from *another* page
    /// (self-links excluded). Drives orphan inbound exemption, so it must carry
    /// page identity — a flat slug set would credit every same-named page and
    /// count self-links, both of which mask real orphans.
    linked: HashSet<String>,
}

impl VaultExistence {
    /// Derive the universe from a full-vault page scan. Resolution is done up
    /// front so `linked` holds resolved page ids, not raw target slugs.
    pub fn from_pages(pages: &[ScannedPage], dirs: &VaultDirs) -> Self {
        let mut by_filename: HashMap<String, String> = HashMap::with_capacity(pages.len());
        let mut ids = HashSet::with_capacity(pages.len());
        for page in pages {
            ids.insert(page.id.clone());
        }
        // Concept pages claim their filename slug first (a bare `[[name]]` is a
        // knowledge-node reference), then non-concepts fill any still-vacant slug —
        // so a concept deterministically owns the bare slug over a same-named
        // document, independent of scan order.
        for concept_pass in [true, false] {
            for page in pages {
                if is_concept_page(&page.path, dirs) != concept_pass {
                    continue;
                }
                let slug = stem_slug(&page.path);
                if !slug.is_empty() {
                    by_filename.entry(slug).or_insert_with(|| page.id.clone());
                }
            }
        }

        // Aliases are the lowest-precedence layer: a concept's declared `aliases` let a
        // bare `[[synonym]]` resolve to it, but never override a real id/filename slug,
        // and the first concept to claim an alias wins (deterministic; a collision is
        // surfaced separately by `concept_lint::alias_conflicts`).
        let mut by_alias: HashMap<String, String> = HashMap::new();
        for page in pages {
            if !is_concept_page(&page.path, dirs) {
                continue;
            }
            for alias in &page.aliases {
                if ids.contains(alias) || by_filename.contains_key(alias) {
                    continue;
                }
                // Winner is the lexicographically-smallest claimant id, pinned here so it
                // never depends on the caller's page ordering. The backlinks resolver
                // tiebreaks on the same key, so a duplicate alias resolves to one concept
                // and `source_count` can never diverge from the resolved graph.
                by_alias
                    .entry(alias.clone())
                    .and_modify(|cur| {
                        if page.id < *cur {
                            *cur = page.id.clone();
                        }
                    })
                    .or_insert_with(|| page.id.clone());
            }
        }

        let mut existence = Self {
            by_filename,
            by_alias,
            ids,
            linked: HashSet::new(),
        };
        for page in pages {
            for target in &page.outgoing {
                if let Some(target_id) = existence.resolve(target).map(str::to_owned)
                    && target_id != page.id
                {
                    existence.linked.insert(target_id);
                }
            }
        }
        existence
    }

    /// Resolve a wikilink target to the page id it points at: a path target
    /// (`dir/sub/name`) is its own id; a bare target (`name`) matches a filename.
    pub fn resolve(&self, target: &str) -> Option<&str> {
        self.ids
            .get(target)
            .map(String::as_str)
            .or_else(|| self.by_filename.get(target).map(String::as_str))
            .or_else(|| self.by_alias.get(target).map(String::as_str))
    }

    /// Whether `target` is a REAL page — a page id or a filename slug — independent of
    /// the alias layer. The single source of truth for "an alias must not shadow a real
    /// page", consulted by `WikiGraph` and the alias-conflict lint so every consumer
    /// applies the same full-vault precedence rather than its own local subset.
    pub(crate) fn is_real_page(&self, target: &str) -> bool {
        self.ids.contains(target) || self.by_filename.contains_key(target)
    }

    /// Whether a wikilink target resolves to any page in the vault.
    pub fn resolves(&self, target: &str) -> bool {
        self.resolve(target).is_some()
    }

    /// Whether `page_id` is the resolved target of a wikilink from another page.
    pub fn is_linked(&self, page_id: &str) -> bool {
        self.linked.contains(page_id)
    }
}

/// Whether a page is a concept page (`{dirs.wiki}/concepts/{slug}.md`). A bare
/// `[[name]]` wikilink targets the knowledge node, so a concept owns its filename
/// slug over a same-named document or other page (which is always cited by its path
/// form). Anchored to the configured `dirs.wiki` — never a hardcoded path segment —
/// and is the single definition shared by the resolver (`from_pages`, `WikiGraph`)
/// and `backlinks`.
pub(crate) fn is_concept_page(path: &Path, dirs: &VaultDirs) -> bool {
    let s = path.to_string_lossy().replace('\\', "/");
    s.starts_with(&format!("{}/concepts/", dirs.wiki))
        && path.extension().is_some_and(|ext| ext == "md")
}

/// True iff this vault-relative path can act as a citation *source* — an event,
/// work-log, synthesis, document, or exploration page where a concept appearance is
/// meaningful provenance. Concept-to-concept links and navigation pages are excluded:
/// cross-references between concepts are curated structure (`## Related`), not activity.
/// Single-sourced here so `backlinks` (what fills `## Sources`) and `stale` (what counts
/// as recent reinforcement for liveness) share ONE definition of "a real source".
pub(crate) fn is_valid_source(path: &Path, dirs: &VaultDirs) -> bool {
    let s = path.to_string_lossy().replace('\\', "/");
    s.starts_with(&format!("{}/", dirs.daily))
        || s.starts_with(&format!("{}/", dirs.personal))
        || s.starts_with(&format!("{}/", dirs.synthesis))
        || s.starts_with(&format!("{}/documents/", dirs.wiki))
        || s.starts_with(&format!("{}/explorations/", dirs.wiki))
}

/// Stem-slug of a page path: slugify the file stem (the resolution key used
/// by [`crate::graph`] for filename-based wikilink matching).
pub fn stem_slug(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(slugify)
        .unwrap_or_default()
}

/// Resolve a raw wikilink target to its graph resolution key.
///
/// A **path-style** target (containing a separator, e.g.
/// `daily/team-slack/2026-05-22`) is slugified per segment and rejoined with
/// `/`, so it matches a page id ([`path_slug`], which likewise normalizes
/// `\` → `/`). A **bare** target (e.g. `Confluence Cloud`) is slugified whole,
/// matching a filename slug. Without the path branch, `[[daily/x/y]]` would
/// collapse to `daily-x-y` and resolve to neither form — every cross-folder link
/// (e.g. a concept's `## Sources` backlinks) would read as broken.
pub fn resolve_wikilink_target(raw: &str) -> String {
    if raw.contains(['/', '\\']) {
        raw.split(['/', '\\'])
            .filter_map(slugify)
            .collect::<Vec<_>>()
            .join("/")
    } else {
        slugify(raw).unwrap_or_default()
    }
}

/// Page ids of Lorekeeper's reserved wiki meta files (the index catalog and the
/// AGENTS.md schema doc) under `wiki_dir`, e.g. `wiki/index`, `wiki/agents`.
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
    use super::*;

    #[test]
    fn resolve_wikilink_target_bare_vs_path() {
        // Bare target → filename slug.
        assert_eq!(
            resolve_wikilink_target("Confluence Cloud"),
            "confluence-cloud"
        );
        // Path-style target → per-segment slug rejoined, matching a page id
        // (not collapsed to `daily-team-slack-2026-05-22`).
        assert_eq!(
            resolve_wikilink_target("daily/team-slack/2026-05-22"),
            "daily/team-slack/2026-05-22"
        );
        assert_eq!(
            resolve_wikilink_target("wiki/concepts/Agentic AI"),
            "wiki/concepts/agentic-ai"
        );
        // Empty segments collapse away.
        assert_eq!(resolve_wikilink_target("daily//x/"), "daily/x");
        // Backslash separators are normalized to `/`, matching `path_slug`.
        assert_eq!(
            resolve_wikilink_target("daily\\team-slack\\2026-05-22"),
            "daily/team-slack/2026-05-22"
        );
    }

    #[test]
    fn vault_existence_indexes_both_forms() {
        let pages = vec![
            ScannedPage {
                id: "daily/team-slack/2026-05-22".to_owned(),
                path: PathBuf::from("daily/team-slack/2026-05-22.md"),
                title: "t".to_owned(),
                outgoing: vec!["confluence-cloud".to_owned()],
                aliases: Vec::new(),
            },
            ScannedPage {
                id: "wiki/concepts/confluence-cloud".to_owned(),
                path: PathBuf::from("wiki/concepts/confluence-cloud.md"),
                title: "Confluence Cloud".to_owned(),
                outgoing: vec![],
                aliases: Vec::new(),
            },
        ];
        let ex = VaultExistence::from_pages(&pages, &VaultDirs::default());
        // Both forms resolve: path id and bare filename.
        assert!(ex.resolves("daily/team-slack/2026-05-22"));
        assert!(ex.resolves("2026-05-22"));
        assert!(ex.resolves("confluence-cloud"));
        assert!(!ex.resolves("nope"));
        // A bare filename resolves to the concept's page id.
        assert_eq!(
            ex.resolve("confluence-cloud"),
            Some("wiki/concepts/confluence-cloud")
        );
        // The daily page links the concept → its page id is a link target.
        assert!(ex.is_linked("wiki/concepts/confluence-cloud"));
        // The daily page itself is linked by nobody.
        assert!(!ex.is_linked("daily/team-slack/2026-05-22"));
    }

    #[test]
    fn vault_existence_resolves_concept_alias() {
        // A bare `[[k8s]]` (a declared alias of the kubernetes concept) resolves to
        // the concept page and credits it as linked — so a synonym citation neither
        // breaks nor orphans.
        let pages = vec![
            ScannedPage {
                id: "wiki/concepts/kubernetes".to_owned(),
                path: PathBuf::from("wiki/concepts/kubernetes.md"),
                title: "Kubernetes".to_owned(),
                outgoing: vec![],
                aliases: vec!["k8s".to_owned()],
            },
            ScannedPage {
                id: "daily/x/2026-05-22".to_owned(),
                path: PathBuf::from("daily/x/2026-05-22.md"),
                title: "d".to_owned(),
                outgoing: vec!["k8s".to_owned()],
                aliases: vec![],
            },
        ];
        let ex = VaultExistence::from_pages(&pages, &VaultDirs::default());
        assert_eq!(ex.resolve("k8s"), Some("wiki/concepts/kubernetes"));
        assert!(ex.is_linked("wiki/concepts/kubernetes"));
    }

    #[test]
    fn alias_never_overrides_a_real_page() {
        // A real page literally named `k8s` must win over another concept's `k8s`
        // alias — aliases are the lowest-precedence resolution layer.
        let pages = vec![
            ScannedPage {
                id: "wiki/concepts/kubernetes".to_owned(),
                path: PathBuf::from("wiki/concepts/kubernetes.md"),
                title: "Kubernetes".to_owned(),
                outgoing: vec![],
                aliases: vec!["k8s".to_owned()],
            },
            ScannedPage {
                id: "wiki/concepts/k8s".to_owned(),
                path: PathBuf::from("wiki/concepts/k8s.md"),
                title: "k8s".to_owned(),
                outgoing: vec![],
                aliases: vec![],
            },
        ];
        let ex = VaultExistence::from_pages(&pages, &VaultDirs::default());
        assert_eq!(ex.resolve("k8s"), Some("wiki/concepts/k8s"));
    }

    #[test]
    fn duplicate_alias_resolves_to_smallest_id_regardless_of_order() {
        // Two concepts claim the same alias. The winner must be the lexicographically
        // smallest id and must NOT depend on the order pages are passed in — otherwise a
        // differently-ordered caller (e.g. backlinks) could credit a different concept and
        // make `source_count` disagree with the resolved graph.
        let apple = ScannedPage {
            id: "wiki/concepts/apple".to_owned(),
            path: PathBuf::from("wiki/concepts/apple.md"),
            title: "Apple".to_owned(),
            outgoing: vec![],
            aliases: vec!["fruit".to_owned()],
        };
        let banana = ScannedPage {
            id: "wiki/concepts/banana".to_owned(),
            path: PathBuf::from("wiki/concepts/banana.md"),
            title: "Banana".to_owned(),
            outgoing: vec![],
            aliases: vec!["fruit".to_owned()],
        };
        let forward =
            VaultExistence::from_pages(&[apple.clone(), banana.clone()], &VaultDirs::default());
        let reversed = VaultExistence::from_pages(&[banana, apple], &VaultDirs::default());
        assert_eq!(forward.resolve("fruit"), Some("wiki/concepts/apple"));
        assert_eq!(
            reversed.resolve("fruit"),
            Some("wiki/concepts/apple"),
            "duplicate-alias winner must be order-independent"
        );
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
    fn parse_file_extracts_title_and_links() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("a.md"),
            "---\ntitle: Concept A\n---\n\n# Heading\n\nLinks to [[Concept B]] and [[concept-b]].\n",
        )
        .unwrap();
        let page = parse_file(&wiki.join("a.md"), tmp.path()).unwrap();
        assert_eq!(page.id, "wiki/a");
        assert_eq!(page.title, "Concept A");
        // Both wikilinks slugify to the same target and dedupe.
        assert_eq!(page.outgoing, vec!["concept-b"]);
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
