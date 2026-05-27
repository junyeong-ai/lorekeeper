//! Keep concept page `## <Related>` sections in sync with Louvain community
//! co-membership.
//!
//! Two concepts are related iff they belong to the same Louvain community in
//! the full-vault wikilink graph. The community structure captures topical
//! affinity: concepts co-referenced from the same daily/document pages cluster
//! together, and their `## 관련` sections reflect that structure.
//!
//! Parallel to [`crate::backlinks`] (which syncs `## 출처`). Same deterministic,
//! idempotent, diff-based section rewrite via [`lk_vault::replace_section`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_core::concept::slugify;
use lk_core::config::{GraphConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_vault::{VaultWriter, replace_section};
use serde::Serialize;

use crate::GraphError;
use crate::backlinks::{is_concept_page, parse_existing_sources, render_sources_body};
use crate::cluster::{self, ClusterResult};
use crate::graph::WikiGraph;
use crate::scan::Page;

#[derive(Debug, Clone, Serialize)]
pub struct RelationUpdate {
    pub path: PathBuf,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub updated: Vec<RelationUpdate>,
    pub unchanged: usize,
    pub dry_run: bool,
}

pub fn sync_concept_relations(
    pages: &[Page],
    vault_root: &Path,
    locale: Locale,
    dry_run: bool,
    dirs: &VaultDirs,
    graph_config: &GraphConfig,
) -> Result<SyncReport, GraphError> {
    let g = WikiGraph::build(pages);
    let clusters = cluster::detect_communities(&g, graph_config);
    let community_map = build_concept_community_map(&clusters, dirs);

    let heading = locale.strings().related;
    let writer = VaultWriter::new(vault_root);
    let mut report = SyncReport {
        dry_run,
        ..SyncReport::default()
    };

    for page in pages {
        if !is_concept_page(&page.path, dirs) {
            continue;
        }

        let Some(stem) = page
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(slugify)
        else {
            continue;
        };

        let related: Vec<String> = community_map
            .get(&stem)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| *s != stem)
            .collect();

        let full_path = vault_root.join(&page.path);
        let raw = std::fs::read_to_string(&full_path)
            .map_err(|e| GraphError::Io(format!("failed to read {}: {e}", full_path.display())))?;

        let existing = parse_existing_sources(&raw, heading);
        let existing_set: BTreeSet<&str> = existing.iter().map(String::as_str).collect();
        let desired_set: BTreeSet<&str> = related.iter().map(String::as_str).collect();

        if existing_set == desired_set {
            report.unchanged += 1;
            continue;
        }

        let added: Vec<String> = desired_set
            .difference(&existing_set)
            .map(|s| (*s).to_owned())
            .collect();
        let removed: Vec<String> = existing_set
            .difference(&desired_set)
            .map(|s| (*s).to_owned())
            .collect();

        let new_body = render_sources_body(&related);
        let updated_content = replace_section(&raw, heading, &new_body);

        if !dry_run && updated_content != raw {
            writer
                .write_page_sync(&page.path, &updated_content)
                .map_err(|e| {
                    GraphError::Io(format!("failed to write {}: {e}", page.path.display()))
                })?;
        }

        report.updated.push(RelationUpdate {
            path: page.path.clone(),
            added,
            removed,
        });
    }

    Ok(report)
}

fn build_concept_community_map(
    clusters: &ClusterResult,
    dirs: &VaultDirs,
) -> BTreeMap<String, Vec<String>> {
    let mut slug_to_community: BTreeMap<String, usize> = BTreeMap::new();

    let concepts_prefix = format!("{}/concepts/", dirs.wiki);

    for (community_id, community) in clusters.communities.iter().enumerate() {
        for member_id in &community.members {
            let Some(slug_raw) = member_id.strip_prefix(&concepts_prefix) else {
                continue;
            };
            if slug_raw.contains('/') {
                continue;
            }
            if let Some(stem) = slugify(slug_raw) {
                slug_to_community.insert(stem, community_id);
            }
        }
    }

    let mut community_to_slugs: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    for (slug, &cid) in &slug_to_community {
        community_to_slugs
            .entry(cid)
            .or_default()
            .insert(slug.clone());
    }

    let mut result: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for slugs in community_to_slugs.values() {
        let sorted: Vec<String> = slugs.iter().cloned().collect();
        for slug in &sorted {
            result.insert(slug.clone(), sorted.clone());
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_page(id: &str, rel: &str, outgoing: &[&str]) -> Page {
        Page {
            id: id.to_owned(),
            path: PathBuf::from(rel),
            title: id.to_owned(),
            outgoing: outgoing.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn write_concept(dir: &TempDir, stem: &str, related: &[&str]) {
        let path = dir.path().join("wiki/concepts").join(format!("{stem}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let related_body = if related.is_empty() {
            String::new()
        } else {
            related
                .iter()
                .map(|s| format!("- [[{s}]]"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let body = format!(
            "---\nid: {stem}\n---\n\n# {stem}\n\n## 핵심\n\nSynthesis.\n\n## 출처\n\n\n## 관련\n\n{related_body}\n"
        );
        std::fs::write(&path, body).unwrap();
    }

    fn write_daily(dir: &TempDir, id: &str, filename: &str) {
        let path = dir.path().join(filename);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("---\nid: {id}\n---\n\n# {id}\n")).unwrap();
    }

    fn default_graph_config() -> GraphConfig {
        let mut cfg = GraphConfig::default();
        cfg.cluster.min_community_size = 1;
        cfg
    }

    #[test]
    fn inserts_related_concepts() {
        let dir = TempDir::new().unwrap();
        // concept-a, concept-b, concept-c all link a shared hub document
        // The hub document links back to all three → same community
        write_concept(&dir, "concept-a", &[]);
        write_concept(&dir, "concept-b", &[]);
        write_concept(&dir, "concept-c", &[]);
        write_daily(&dir, "daily/src/2026-05-27", "daily/src/2026-05-27.md");

        let pages = vec![
            make_page("wiki/concepts/concept-a", "wiki/concepts/concept-a.md", &[]),
            make_page("wiki/concepts/concept-b", "wiki/concepts/concept-b.md", &[]),
            make_page("wiki/concepts/concept-c", "wiki/concepts/concept-c.md", &[]),
            make_page(
                "daily/src/2026-05-27",
                "daily/src/2026-05-27.md",
                &["concept-a", "concept-b", "concept-c"],
            ),
        ];

        let report = sync_concept_relations(
            &pages,
            dir.path(),
            Locale::Ko,
            false,
            &VaultDirs::default(),
            &default_graph_config(),
        )
        .unwrap();

        assert_eq!(report.updated.len(), 3);
        for update in &report.updated {
            assert_eq!(update.added.len(), 2, "each concept should gain 2 related");
        }

        let content =
            std::fs::read_to_string(dir.path().join("wiki/concepts/concept-a.md")).unwrap();
        assert!(content.contains("- [[concept-b]]"));
        assert!(content.contains("- [[concept-c]]"));
    }

    #[test]
    fn dry_run_does_not_write() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "concept-a", &[]);
        write_concept(&dir, "concept-b", &[]);
        write_daily(&dir, "daily/src/2026-05-27", "daily/src/2026-05-27.md");
        let before =
            std::fs::read_to_string(dir.path().join("wiki/concepts/concept-a.md")).unwrap();

        let pages = vec![
            make_page("wiki/concepts/concept-a", "wiki/concepts/concept-a.md", &[]),
            make_page("wiki/concepts/concept-b", "wiki/concepts/concept-b.md", &[]),
            make_page(
                "daily/src/2026-05-27",
                "daily/src/2026-05-27.md",
                &["concept-a", "concept-b"],
            ),
        ];

        let report = sync_concept_relations(
            &pages,
            dir.path(),
            Locale::Ko,
            true,
            &VaultDirs::default(),
            &default_graph_config(),
        )
        .unwrap();

        assert!(report.dry_run);
        assert!(!report.updated.is_empty());

        let after = std::fs::read_to_string(dir.path().join("wiki/concepts/concept-a.md")).unwrap();
        assert_eq!(before, after, "dry-run must not write");
    }

    #[test]
    fn idempotent_on_second_run() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "concept-a", &[]);
        write_concept(&dir, "concept-b", &[]);
        write_daily(&dir, "daily/src/2026-05-27", "daily/src/2026-05-27.md");

        let pages = vec![
            make_page("wiki/concepts/concept-a", "wiki/concepts/concept-a.md", &[]),
            make_page("wiki/concepts/concept-b", "wiki/concepts/concept-b.md", &[]),
            make_page(
                "daily/src/2026-05-27",
                "daily/src/2026-05-27.md",
                &["concept-a", "concept-b"],
            ),
        ];

        let first = sync_concept_relations(
            &pages,
            dir.path(),
            Locale::Ko,
            false,
            &VaultDirs::default(),
            &default_graph_config(),
        )
        .unwrap();
        assert!(!first.updated.is_empty());

        let second = sync_concept_relations(
            &pages,
            dir.path(),
            Locale::Ko,
            false,
            &VaultDirs::default(),
            &default_graph_config(),
        )
        .unwrap();
        assert!(second.updated.is_empty());
        assert_eq!(second.unchanged, 2);
    }

    #[test]
    fn singleton_community_stays_empty() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "lonely", &[]);

        let pages = vec![make_page(
            "wiki/concepts/lonely",
            "wiki/concepts/lonely.md",
            &[],
        )];

        let report = sync_concept_relations(
            &pages,
            dir.path(),
            Locale::Ko,
            false,
            &VaultDirs::default(),
            &default_graph_config(),
        )
        .unwrap();

        assert!(report.updated.is_empty());
        assert_eq!(report.unchanged, 1);
    }

    #[test]
    fn preserves_other_sections() {
        let dir = TempDir::new().unwrap();
        write_concept(&dir, "concept-a", &[]);
        write_concept(&dir, "concept-b", &[]);
        write_daily(&dir, "daily/src/2026-05-27", "daily/src/2026-05-27.md");

        let pages = vec![
            make_page("wiki/concepts/concept-a", "wiki/concepts/concept-a.md", &[]),
            make_page("wiki/concepts/concept-b", "wiki/concepts/concept-b.md", &[]),
            make_page(
                "daily/src/2026-05-27",
                "daily/src/2026-05-27.md",
                &["concept-a", "concept-b"],
            ),
        ];

        sync_concept_relations(
            &pages,
            dir.path(),
            Locale::Ko,
            false,
            &VaultDirs::default(),
            &default_graph_config(),
        )
        .unwrap();

        let content =
            std::fs::read_to_string(dir.path().join("wiki/concepts/concept-a.md")).unwrap();
        assert!(content.contains("## 핵심\n\nSynthesis."));
        assert!(content.contains("## 출처"));
    }

    #[test]
    fn english_locale() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki/concepts/concept-a.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nid: concept-a\n---\n\n# concept-a\n\n## Sources\n\n\n## Related\n\n\n",
        )
        .unwrap();
        let path_b = dir.path().join("wiki/concepts/concept-b.md");
        std::fs::write(
            &path_b,
            "---\nid: concept-b\n---\n\n# concept-b\n\n## Sources\n\n\n## Related\n\n\n",
        )
        .unwrap();
        write_daily(&dir, "daily/src/2026-05-27", "daily/src/2026-05-27.md");

        let pages = vec![
            make_page("wiki/concepts/concept-a", "wiki/concepts/concept-a.md", &[]),
            make_page("wiki/concepts/concept-b", "wiki/concepts/concept-b.md", &[]),
            make_page(
                "daily/src/2026-05-27",
                "daily/src/2026-05-27.md",
                &["concept-a", "concept-b"],
            ),
        ];

        sync_concept_relations(
            &pages,
            dir.path(),
            Locale::En,
            false,
            &VaultDirs::default(),
            &default_graph_config(),
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Related\n\n- [[concept-b]]"));
    }

    #[test]
    fn removes_stale_relations() {
        let dir = TempDir::new().unwrap();
        // concept-a has an existing stale relation to concept-x
        write_concept(&dir, "concept-a", &["concept-x"]);
        write_concept(&dir, "concept-b", &[]);
        write_daily(&dir, "daily/src/2026-05-27", "daily/src/2026-05-27.md");

        // Only concept-a and concept-b are in the graph; concept-x doesn't exist
        let pages = vec![
            make_page("wiki/concepts/concept-a", "wiki/concepts/concept-a.md", &[]),
            make_page("wiki/concepts/concept-b", "wiki/concepts/concept-b.md", &[]),
            make_page(
                "daily/src/2026-05-27",
                "daily/src/2026-05-27.md",
                &["concept-a", "concept-b"],
            ),
        ];

        let report = sync_concept_relations(
            &pages,
            dir.path(),
            Locale::Ko,
            false,
            &VaultDirs::default(),
            &default_graph_config(),
        )
        .unwrap();

        let a_update = report
            .updated
            .iter()
            .find(|u| u.path.to_string_lossy().contains("concept-a"))
            .expect("concept-a should be updated");
        assert!(a_update.removed.contains(&"concept-x".to_owned()));
        assert!(a_update.added.contains(&"concept-b".to_owned()));

        let content =
            std::fs::read_to_string(dir.path().join("wiki/concepts/concept-a.md")).unwrap();
        assert!(content.contains("- [[concept-b]]"));
        assert!(!content.contains("concept-x"));
    }
}
