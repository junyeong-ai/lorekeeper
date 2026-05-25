//! Vault scan: walk markdown files and build [`Page`] records.
//!
//! slug normalization, frontmatter parsing, and wikilink extraction — now live in
//! `lk-core` (`slugify`, `frontmatter::parse_page`, `wikilink`). This module keeps only
//! the I/O concerns: filesystem walking (walkdir + rayon) and assembling [`Page`]s.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use lk_core::concept::slugify;
use lk_core::config::GraphConfig;
use lk_core::frontmatter::{self, Frontmatter};
use lk_core::wikilink;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::GraphError;

/// A scanned vault page: its slug id, vault-relative path, display title, and the
/// slugified targets of its outgoing wikilinks (deduped, first-appearance order).
#[derive(Debug, Clone)]
pub struct Page {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub outgoing: Vec<String>,
}

/// Walk the configured scope directories under `root` and parse every `.md` file.
pub fn scan_vault(root: &Path, config: &GraphConfig) -> Result<Vec<Page>, GraphError> {
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
                    eprintln!("warning: skipping unreadable path: {e}");
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

    let mut pages: Vec<Page> = file_paths
        .par_iter()
        .filter_map(|path| match parse_file(path, root) {
            Ok(page) => Some(page),
            Err(e) => {
                eprintln!("warning: skipping {}: {e}", path.display());
                None
            }
        })
        .collect();

    pages.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(pages)
}

fn parse_file(path: &Path, root: &Path) -> Result<Page, String> {
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

    Ok(Page {
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
#[derive(Debug, Clone, Default)]
pub struct VaultExistence {
    /// Filename slug → page id, first insertion winning on duplicates (mirrors
    /// the ambiguous-slug shadowing in [`crate::graph::WikiGraph`], so the two
    /// resolve a bare `[[name]]` to the same page).
    by_filename: HashMap<String, String>,
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
    pub fn from_pages(pages: &[Page]) -> Self {
        let mut by_filename: HashMap<String, String> = HashMap::with_capacity(pages.len());
        let mut ids = HashSet::with_capacity(pages.len());
        for page in pages {
            ids.insert(page.id.clone());
            let slug = stem_slug(&page.path);
            if !slug.is_empty() {
                by_filename.entry(slug).or_insert_with(|| page.id.clone());
            }
        }

        let mut existence = Self {
            by_filename,
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
/// (e.g. a concept's `## 출처` backlinks) would read as broken.
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
        assert_eq!(resolve_wikilink_target("Confluence Cloud"), "confluence-cloud");
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
            Page {
                id: "daily/team-slack/2026-05-22".to_owned(),
                path: PathBuf::from("daily/team-slack/2026-05-22.md"),
                title: "t".to_owned(),
                outgoing: vec!["confluence-cloud".to_owned()],
            },
            Page {
                id: "wiki/concepts/confluence-cloud".to_owned(),
                path: PathBuf::from("wiki/concepts/confluence-cloud.md"),
                title: "Confluence Cloud".to_owned(),
                outgoing: vec![],
            },
        ];
        let ex = VaultExistence::from_pages(&pages);
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
    fn path_slug_basic() {
        assert_eq!(
            path_slug(Path::new("wiki/Concept A.md")),
            "wiki/concept-a"
        );
        assert_eq!(
            path_slug(Path::new("wiki/Bad_Name.md")),
            "wiki/bad-name"
        );
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
        config.scope.exclude = vec!["wiki/skip.md".to_string()];
        let pages = scan_vault(tmp.path(), &config).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].id, "wiki/keep");

        config.scope.dirs = vec![PathBuf::from("nonexistent")];
        assert!(scan_vault(tmp.path(), &config).is_err());
    }
}
