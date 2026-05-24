use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use lk_core::concept::slugify;
use lk_core::config::GraphConfig;
use lk_core::wikilink;

use crate::GraphError;
use crate::graph::WikiGraph;
use crate::scan;

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

pub fn diff(graph: &WikiGraph, root: &Path, config: &GraphConfig) -> IndexDrift {
    let index_path = root.join(&config.scope.dirs[0]).join("index.md");

    let Ok(content) = std::fs::read_to_string(&index_path) else {
        return IndexDrift {
            missing_from_index: vec![],
            missing_from_disk: vec![],
        };
    };

    let mut index_links = HashSet::new();
    for page in wikilink::extract_wikilinks(&content) {
        let slug = slugify(page);
        if !slug.is_empty() {
            index_links.insert(slug);
        }
    }

    let exclude: HashSet<&str> = config
        .graph
        .orphan_exclude
        .iter()
        .map(String::as_str)
        .collect();

    let index_dir_slug = slugify(&config.scope.dirs[0].to_string_lossy().replace('\\', "/"));
    let index_dir_prefix = format!("{index_dir_slug}/");

    let index_id = scan::slug_from_path(&config.scope.dirs[0].join("index"));

    let disk_ids: HashSet<&str> = graph.node_ids().collect();

    let mut missing_from_index: Vec<String> = disk_ids
        .iter()
        .filter(|&&id| {
            if !id.starts_with(&index_dir_prefix) {
                return false;
            }
            if id == index_id {
                return false;
            }
            if exclude.contains(id) {
                return false;
            }
            let filename = id.rsplit('/').next().unwrap_or(id);
            !index_links.contains(filename)
        })
        .map(ToString::to_string)
        .collect();
    missing_from_index.sort();

    let mut missing_from_disk: Vec<String> = index_links
        .iter()
        .filter(|slug| graph.resolve_filename(slug).is_none())
        .cloned()
        .collect();
    missing_from_disk.sort();

    IndexDrift {
        missing_from_index,
        missing_from_disk,
    }
}

pub fn fix(drift: &IndexDrift, root: &Path, config: &GraphConfig) -> Result<usize, GraphError> {
    if drift.missing_from_index.is_empty() {
        return Ok(0);
    }

    let index_path = root.join(&config.scope.dirs[0]).join("index.md");
    let mut content = std::fs::read_to_string(&index_path)
        .map_err(|e| GraphError::Io(format!("failed to read {}: {e}", index_path.display())))?;

    let mut added = 0;
    for page_id in &drift.missing_from_index {
        let name = page_id.rsplit('/').next().unwrap_or(page_id);
        let _ = write!(content, "\n- [[{name}]]");
        added += 1;
    }

    if added > 0 && !content.ends_with('\n') {
        content.push('\n');
    }

    std::fs::write(&index_path, &content)
        .map_err(|e| GraphError::Io(format!("failed to write {}: {e}", index_path.display())))?;

    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Page;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_page(id: &str, outgoing: &[&str]) -> Page {
        Page {
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
            wiki.join("index.md"),
            "# Index\n\n- [[alpha]]\n- [[beta]]\n",
        )
        .unwrap();
    }

    #[test]
    fn detect_missing_from_index() {
        let tmp = TempDir::new().unwrap();
        setup_wiki(tmp.path());

        let config = GraphConfig::default();
        let pages = vec![
            make_page("wiki/index", &["alpha", "beta"]),
            make_page("wiki/alpha", &[]),
            make_page("wiki/beta", &[]),
            make_page("wiki/gamma", &[]),
        ];

        let graph = WikiGraph::build(&pages);
        let drift = diff(&graph, tmp.path(), &config);

        assert!(drift.missing_from_index.contains(&"wiki/gamma".to_owned()));
        assert!(drift.missing_from_disk.is_empty());
    }

    #[test]
    fn detect_missing_from_disk() {
        let tmp = TempDir::new().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(
            wiki.join("index.md"),
            "# Index\n\n- [[alpha]]\n- [[nonexistent]]\n",
        )
        .unwrap();

        let config = GraphConfig::default();
        let pages = vec![
            make_page("wiki/index", &["alpha", "nonexistent"]),
            make_page("wiki/alpha", &[]),
        ];

        let graph = WikiGraph::build(&pages);
        let drift = diff(&graph, tmp.path(), &config);

        assert!(drift.missing_from_disk.contains(&"nonexistent".to_owned()));
    }

    #[test]
    fn fix_adds_to_index() {
        let tmp = TempDir::new().unwrap();
        setup_wiki(tmp.path());

        let config = GraphConfig::default();
        let drift = IndexDrift {
            missing_from_index: vec!["wiki/gamma".to_owned()],
            missing_from_disk: vec![],
        };

        let added = fix(&drift, tmp.path(), &config).unwrap();
        assert_eq!(added, 1);

        let content = std::fs::read_to_string(tmp.path().join("wiki/index.md")).unwrap();
        assert!(content.contains("[[gamma]]"));
    }

    #[test]
    fn missing_index_returns_empty_drift() {
        let tmp = TempDir::new().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();

        let config = GraphConfig::default();
        let pages = vec![make_page("wiki/alpha", &[]), make_page("wiki/beta", &[])];

        let graph = WikiGraph::build(&pages);
        let drift = diff(&graph, tmp.path(), &config);

        assert!(drift.is_in_sync());
    }

    #[test]
    fn fix_idempotent_when_no_drift() {
        let tmp = TempDir::new().unwrap();
        setup_wiki(tmp.path());

        let config = GraphConfig::default();
        let drift = IndexDrift {
            missing_from_index: vec![],
            missing_from_disk: vec![],
        };

        let added = fix(&drift, tmp.path(), &config).unwrap();
        assert_eq!(added, 0);
    }
}
