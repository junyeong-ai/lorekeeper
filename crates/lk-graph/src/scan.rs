//! Vault scan: walk markdown files and build [`Page`] records.
//!
//! slug normalization, frontmatter parsing, and wikilink extraction — now live in
//! `lk-core` (`slugify`, `frontmatter::parse_page`, `wikilink`). This module keeps only
//! the I/O concerns: filesystem walking (walkdir + rayon) and assembling [`Page`]s.

use std::collections::HashSet;
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
    let id = slug_from_path(rel);

    let mut outgoing = Vec::new();
    let mut seen = HashSet::new();
    for raw_target in wikilink::extract_wikilinks(&parsed.body) {
        let target = slugify(raw_target);
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

/// Slug id for a vault-relative path: drop the extension, normalize separators to `/`,
/// then slugify each path segment (so `wiki/Concept A.md` → `wiki/concept-a`).
pub fn slug_from_path(rel: &Path) -> String {
    let no_ext = rel.with_extension("");
    let s = no_ext.to_string_lossy().replace('\\', "/");
    s.split('/').map(slugify).collect::<Vec<_>>().join("/")
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
    fn slug_from_path_basic() {
        assert_eq!(
            slug_from_path(Path::new("wiki/Concept A.md")),
            "wiki/concept-a"
        );
        assert_eq!(
            slug_from_path(Path::new("wiki/Bad_Name.md")),
            "wiki/bad-name"
        );
    }

    #[test]
    fn slug_from_path_preserves_directory_structure() {
        assert_eq!(
            slug_from_path(Path::new("wiki/sub/Topic Name.md")),
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
