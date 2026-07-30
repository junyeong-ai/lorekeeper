use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use lk_core::link;

use crate::GraphError;
use crate::graph::WikiGraph;
use crate::scan::{self, ScannedPage, VaultExistence};

#[derive(Debug)]
pub struct IndexDrift {
    pub missing_from_index: Vec<String>,
    pub missing_from_disk: Vec<String>,
}

impl IndexDrift {
    pub fn is_in_sync(&self) -> bool {
        self.missing_from_index.is_empty() && self.missing_from_disk.is_empty()
    }
}

pub fn diff(
    graph: &WikiGraph,
    existence: &VaultExistence,
    root: &Path,
    wiki_dir: &Path,
    orphan_exclude: &[String],
) -> Result<IndexDrift, GraphError> {
    let index_path = root.join(wiki_dir).join(lk_core::vault_path::INDEX_FILE);

    // A MISSING index is a legitimate "not built yet" state → treat as empty so every
    // page reports missing-from-index (prompting `lore wiki index`). A real read error
    // (permissions, corruption) must NOT masquerade as "in sync" — propagate it.
    let content = match std::fs::read_to_string(&index_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(GraphError::Io(format!(
                "read {}: {e}",
                index_path.display()
            )));
        }
    };

    // `build_index` catalogs pages as `- [title](relative-path)` entries; resolve each
    // destination against the index's own location to the page id it addresses.
    // Summaries are plain text (`link::strip_links`), so every link here is a real
    // catalog entry, never a link embedded in summary prose. Drift is checked against the
    // graph's `node_ids()` (the analysis scope, i.e. `<wiki>/`); daily/synthesis pages are
    // catalogued too but live outside that scope, so they are not drift-tracked here.
    let index_rel = wiki_dir.join(lk_core::vault_path::INDEX_FILE);
    let mut index_links = HashSet::new();
    for dest in link::extract_dests(&content) {
        if let Some(resolved) = link::resolve_dest(&index_rel, &dest) {
            let id = scan::path_slug(&resolved);
            if !id.is_empty() {
                index_links.insert(id);
            }
        }
    }

    let exclude: HashSet<&str> = orphan_exclude.iter().map(String::as_str).collect();

    // Per-segment slug that PRESERVES path structure (`knowledge/wiki`, not
    // `knowledge-wiki`) so the prefix matches the page ids the scanner produces —
    // a nested `vault.dirs.wiki` must not silently drop pages from the drift check.
    let index_dir_prefix = format!("{}/", scan::path_slug(wiki_dir));

    // Reserved meta pages (index/log/map/AGENTS) are excluded from the graph at
    // construction, so `node_ids()` never yields them — no separate exclusion needed here.
    let disk_ids: HashSet<&str> = graph.node_ids().collect();

    let mut missing_from_index: Vec<String> = disk_ids
        .iter()
        .filter(|&&id| {
            if !id.starts_with(&index_dir_prefix) {
                return false;
            }
            if exclude.contains(id) {
                return false;
            }
            !index_links.contains(id)
        })
        .map(ToString::to_string)
        .collect();
    missing_from_index.sort();

    // An index entry is missing from disk only if it resolves to no page
    // anywhere in the vault — checked against the full-vault existence universe,
    // not the (wiki-only) analysis scope, so path entries to `daily/` pages
    // aren't false-flagged.
    let mut missing_from_disk: Vec<String> = index_links
        .iter()
        .filter(|slug| !existence.is_resolvable(slug))
        .cloned()
        .collect();
    missing_from_disk.sort();

    Ok(IndexDrift {
        missing_from_index,
        missing_from_disk,
    })
}

pub fn fix(
    drift: &IndexDrift,
    pages: &[ScannedPage],
    root: &Path,
    wiki_dir: &Path,
) -> Result<usize, GraphError> {
    if drift.missing_from_index.is_empty() {
        return Ok(0);
    }

    let index_path = root.join(wiki_dir).join(lk_core::vault_path::INDEX_FILE);
    // A missing index.md is treated as empty (consistent with `diff`): `--fix` then
    // writes a fresh index containing every drifted entry. "Fix" means "make it
    // correct" whether the catalog exists yet or not.
    let mut content = match std::fs::read_to_string(&index_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(GraphError::Io(format!(
                "failed to read {}: {e}",
                index_path.display()
            )));
        }
    };

    // Each appended entry needs the drifted page's real path (for the relative
    // destination) and title (for the display text) — looked up from the scan.
    let index_rel = wiki_dir.join(lk_core::vault_path::INDEX_FILE);
    let mut added = 0;
    for page_id in &drift.missing_from_index {
        let Some(page) = pages.iter().find(|p| &p.id == page_id) else {
            return Err(GraphError::Io(format!(
                "drifted page id not in scan: {page_id}"
            )));
        };
        let dest = link::relative_dest(&index_rel, &page.path);
        let _ = write!(content, "\n- {}", link::md_link(&page.title, &dest));
        added += 1;
    }

    if added > 0 && !content.ends_with('\n') {
        content.push('\n');
    }

    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| GraphError::Io(format!("create {}: {e}", parent.display())))?;
    }
    lk_core::fs::write_atomic(&index_path, content.as_bytes(), None)
        .map_err(|e| GraphError::Io(format!("failed to write {}: {e}", index_path.display())))?;

    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScannedPage;
    use lk_core::config::VaultDirs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn build_page(id: &str, outgoing: &[&str]) -> ScannedPage {
        ScannedPage {
            id: id.to_owned(),
            path: PathBuf::from(format!("{id}.md")),
            title: id.rsplit('/').next().unwrap_or(id).to_owned(),
            outgoing: outgoing.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn setup_wiki(dir: &Path) {
        let wiki = dir.join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join(lk_core::vault_path::INDEX_FILE),
            "# Index\n\n- [alpha](alpha.md)\n- [beta](beta.md)\n",
        )
        .unwrap();
    }

    #[test]
    fn detect_missing_from_index() {
        let tmp = TempDir::new().unwrap();
        setup_wiki(tmp.path());

        let pages = vec![
            build_page("wiki/index", &["wiki/alpha", "wiki/beta"]),
            build_page("wiki/alpha", &[]),
            build_page("wiki/beta", &[]),
            build_page("wiki/gamma", &[]),
        ];

        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let existence = VaultExistence::build(&pages, &VaultDirs::default());
        let drift = diff(&graph, &existence, tmp.path(), Path::new("wiki"), &[]).unwrap();

        assert!(drift.missing_from_index.contains(&"wiki/gamma".to_owned()));
        assert!(drift.missing_from_disk.is_empty());
    }

    #[test]
    fn detect_missing_from_disk() {
        let tmp = TempDir::new().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join(lk_core::vault_path::INDEX_FILE),
            "# Index\n\n- [alpha](alpha.md)\n- [nonexistent](nonexistent.md)\n",
        )
        .unwrap();

        let pages = vec![
            build_page("wiki/index", &["wiki/alpha", "wiki/nonexistent"]),
            build_page("wiki/alpha", &[]),
        ];

        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let existence = VaultExistence::build(&pages, &VaultDirs::default());
        let drift = diff(&graph, &existence, tmp.path(), Path::new("wiki"), &[]).unwrap();

        assert!(
            drift
                .missing_from_disk
                .contains(&"wiki/nonexistent".to_owned())
        );
    }

    #[test]
    fn fix_adds_to_index() {
        let tmp = TempDir::new().unwrap();
        setup_wiki(tmp.path());

        let drift = IndexDrift {
            missing_from_index: vec!["wiki/gamma".to_owned()],
            missing_from_disk: vec![],
        };
        let pages = vec![build_page("wiki/gamma", &[])];

        let added = fix(&drift, &pages, tmp.path(), Path::new("wiki")).unwrap();
        assert_eq!(added, 1);

        // The appended entry is a relative link titled by the page's title.
        let content = std::fs::read_to_string(tmp.path().join("wiki/index.md")).unwrap();
        assert!(content.contains("- [gamma](gamma.md)"));
    }

    #[test]
    fn fix_creates_index_when_absent() {
        // `--fix` on a vault that has no index.md yet must CREATE it with the missing
        // entries — consistent with `diff` (which treats absence as all-drift), not
        // error out. "Fix" means make it correct whether the catalog exists or not.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("wiki")).unwrap();
        assert!(!tmp.path().join("wiki/index.md").exists());

        let drift = IndexDrift {
            missing_from_index: vec!["wiki/alpha".to_owned(), "wiki/beta".to_owned()],
            missing_from_disk: vec![],
        };
        let pages = vec![build_page("wiki/alpha", &[]), build_page("wiki/beta", &[])];
        let added = fix(&drift, &pages, tmp.path(), Path::new("wiki")).unwrap();
        assert_eq!(added, 2);

        let content = std::fs::read_to_string(tmp.path().join("wiki/index.md")).unwrap();
        assert!(content.contains("- [alpha](alpha.md)"));
        assert!(content.contains("- [beta](beta.md)"));
    }

    #[test]
    fn missing_index_reports_all_pages_missing_not_in_sync() {
        // A not-yet-built index must NOT read as "in sync" (that would hide that the
        // catalog is absent). Every wiki page is reported missing-from-index so the
        // user is prompted to run `lore wiki index`.
        let tmp = TempDir::new().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();

        let pages = vec![build_page("wiki/alpha", &[]), build_page("wiki/beta", &[])];

        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let existence = VaultExistence::build(&pages, &VaultDirs::default());
        let drift = diff(&graph, &existence, tmp.path(), Path::new("wiki"), &[]).unwrap();

        assert!(!drift.is_in_sync());
        assert!(drift.missing_from_index.contains(&"wiki/alpha".to_owned()));
        assert!(drift.missing_from_index.contains(&"wiki/beta".to_owned()));
    }

    #[test]
    fn fix_idempotent_when_no_drift() {
        let tmp = TempDir::new().unwrap();
        setup_wiki(tmp.path());

        let drift = IndexDrift {
            missing_from_index: vec![],
            missing_from_disk: vec![],
        };

        let added = fix(&drift, &[], tmp.path(), Path::new("wiki")).unwrap();
        assert_eq!(added, 0);
    }
}
