//! Navigable index of Louvain communities in the wiki's citation and concept-link graph.
//! Regenerated as `<wiki>/map.md`; it never writes relationships into concept pages.
//! Group membership is a navigation aid. Relationship meaning and evidence live in
//! the linked pages.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lk_core::config::{GraphConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_core::link;
use lk_core::vault_path::concepts_dir;
use lk_vault::{NavigationCatalog, NavigationKind, NavigationRow};

use crate::cluster::detect_communities;
use crate::graph::WikiGraph;
use crate::scan::under;

/// Build the `map.md` markdown for `graph`. Concepts are the knowledge nodes the map
/// navigates to; documents and explorations stay in the graph as CLUSTERING EVIDENCE
/// (their citations shape the communities) but are not listed as destinations. Each
/// listed cluster is headed by its highest-degree concept. Deterministic: clusters are
/// sorted largest-first with a stable tiebreak, and concepts within a cluster by
/// descending degree then id, so the same vault always yields byte-identical output.
/// Communities with one concept share a compact trailing list ordered by id.
pub fn build_map(
    graph: &WikiGraph,
    config: &GraphConfig,
    dirs: &VaultDirs,
    locale: Locale,
) -> String {
    let catalog = build_map_catalog(graph, config, dirs, locale);
    catalog.pages[&Path::new(&dirs.wiki).join(lk_core::vault_path::MAP_FILE)].clone()
}

/// Build the complete bounded citation map. Publish the catalog, not only its entry
/// string: larger maps keep their destinations in generated detail pages.
pub fn build_map_catalog(
    graph: &WikiGraph,
    config: &GraphConfig,
    dirs: &VaultDirs,
    locale: Locale,
) -> NavigationCatalog {
    let strings = locale.strings();
    let result = detect_communities(graph, config);

    let nodes: HashMap<String, _> = graph
        .hubs(usize::MAX, 0)
        .into_iter()
        .map(|node| (node.id.clone(), node))
        .collect();

    // Scope membership may fold directory spellings; node addresses never do.
    let concept_directory = concepts_dir(dirs);

    // Destinations are relative to the map's own location (`{wiki}/map.md`). A concept
    // page's path is its id plus `.md` — machine-created pages are slug-addressed by
    // construction (`graph normalize` repairs any hand-made exception).
    let map_rel = Path::new(&dirs.wiki).join(lk_core::vault_path::MAP_FILE);
    let dest_of = |id: &str| link::relative_dest(&map_rel, &PathBuf::from(format!("{id}.md")));

    let mut out = String::new();
    writeln!(out, "---\ntype: {}\n---\n", lk_core::vault_path::MAP_FORMAT).unwrap();
    writeln!(out, "# {}", strings.map_title).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{}", strings.map_intro).unwrap();
    writeln!(out).unwrap();

    let header = out.clone();
    let mut rows = Vec::new();
    let mut any = false;
    let mut single_concepts = Vec::new();
    for community in &result.communities {
        let mut concepts: Vec<&String> = community
            .members
            .iter()
            .filter(|id| under(id, &concept_directory))
            .collect();
        if concepts.is_empty() {
            // A cluster of only documents/explorations has no concept to navigate to.
            continue;
        }
        if concepts.len() == 1 {
            single_concepts.push(concepts[0]);
            continue;
        }
        concepts.sort_by(|a, b| {
            nodes[*b]
                .degree
                .cmp(&nodes[*a].degree)
                .then_with(|| a.cmp(b))
        });
        // The highest-degree concept is the cluster's representative (its hub).
        let hub = concepts[0];
        let group = format!(
            "{} ({})",
            link::strip_links(&nodes[hub].title).replace(['\r', '\n'], " "),
            concepts.len()
        );
        writeln!(
            out,
            "## {} ({})",
            link::md_link(&nodes[hub].title, &dest_of(hub)),
            concepts.len()
        )
        .unwrap();
        writeln!(out).unwrap();
        for id in &concepts {
            let node = &nodes[*id];
            rows.push(NavigationRow {
                headings: vec![group.clone()],
                title: node.title.clone(),
                path: PathBuf::from(format!("{id}.md")),
                summary: Some(format!("{} {}", node.degree, strings.map_links)),
            });
            writeln!(
                out,
                "- {} · {} {}",
                link::md_link(&node.title, &dest_of(id)),
                node.degree,
                strings.map_links
            )
            .unwrap();
        }
        writeln!(out).unwrap();
        any = true;
    }

    if !single_concepts.is_empty() {
        single_concepts.sort();
        writeln!(
            out,
            "## {} ({})\n",
            strings.map_single_concept_groups,
            single_concepts.len()
        )
        .unwrap();
        let group = format!(
            "{} ({})",
            strings.map_single_concept_groups,
            single_concepts.len()
        );
        for id in single_concepts {
            let node = &nodes[id];
            rows.push(NavigationRow {
                headings: vec![group.clone()],
                title: node.title.clone(),
                path: PathBuf::from(format!("{id}.md")),
                summary: Some(format!("{} {}", node.degree, strings.map_links)),
            });
            writeln!(
                out,
                "- {} · {} {}",
                link::md_link(&node.title, &dest_of(id)),
                node.degree,
                strings.map_links
            )
            .unwrap();
        }
        writeln!(out).unwrap();
        any = true;
    }

    if !any {
        writeln!(out, "{}", strings.map_empty).unwrap();
    }

    NavigationCatalog::build(
        NavigationKind::Map,
        Path::new(&dirs.wiki),
        strings.map_title,
        &header,
        rows,
    )
    .with_inline(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Link;
    use crate::scan::ScannedPage;
    use lk_core::config::VaultDirs;
    use std::path::PathBuf;

    fn page(id: &str, outgoing: &[&str]) -> ScannedPage {
        ScannedPage {
            revision: Some("revision".into()),
            id: id.to_string(),
            path: PathBuf::from(format!("{id}.md")),
            title: id.rsplit('/').next().unwrap_or(id).to_string(),
            format: None,
            outgoing: outgoing
                .iter()
                .map(|s| Link::to(&format!("{s}.md")))
                .collect(),
        }
    }

    fn graph_of(pages: &[ScannedPage]) -> WikiGraph {
        WikiGraph::build(pages, &VaultDirs::default())
    }

    #[test]
    fn map_scope_keeps_actual_address_spelling() {
        let pages = vec![page("Wiki/concepts/Actual_Name", &[])];
        let graph = graph_of(&pages);
        let map = build_map(
            &graph,
            &GraphConfig::default(),
            &VaultDirs::default(),
            Locale::En,
        );
        assert!(map.contains("Actual_Name.md"), "{map}");
        assert!(!map.contains("actual-name.md"));
    }

    #[test]
    fn empty_graph_yields_intro_and_empty_marker() {
        let g = graph_of(&[]);
        let md = build_map(
            &g,
            &GraphConfig::default(),
            &VaultDirs::default(),
            Locale::En,
        );
        assert!(md.contains("# "), "has a title:\n{md}");
        assert!(
            md.contains(Locale::En.strings().map_empty),
            "shows the empty marker:\n{md}"
        );
    }

    #[test]
    fn lists_only_concepts_using_documents_as_clustering_evidence() {
        // Two mutually-linked concepts, plus a document that cites one of them. The document
        // shapes the cluster (evidence) but is NOT a navigation destination — only concepts
        // are listed with their titles and stable path addresses.
        let mut pages = [
            page("wiki/concepts/rag", &["wiki/concepts/embeddings"]),
            page("wiki/concepts/embeddings", &["wiki/concepts/rag"]),
            page("wiki/documents/survey", &["wiki/concepts/rag"]),
        ];
        pages[0].title = "검색 증강 생성 [RAG]".to_string();
        let g = graph_of(&pages);
        let md = build_map(
            &g,
            &GraphConfig::default(),
            &VaultDirs::default(),
            Locale::En,
        );
        assert!(
            md.contains("[검색 증강 생성 \\[RAG\\]](concepts/rag.md)"),
            "concepts use escaped titles and relative addresses:\n{md}"
        );
        assert!(
            md.contains("[embeddings](concepts/embeddings.md)"),
            "both concepts listed:\n{md}"
        );
        assert!(
            md.starts_with("---\ntype: map\n---\n"),
            "the map declares its page type:\n{md}"
        );
        assert!(
            !md.contains("survey"),
            "documents must NOT be listed as destinations (evidence only):\n{md}"
        );
        // Byte-identical on re-run (determinism is the whole point of a materialized view).
        let again = build_map(
            &g,
            &GraphConfig::default(),
            &VaultDirs::default(),
            Locale::En,
        );
        assert_eq!(md, again);
    }

    #[test]
    fn cluster_of_only_non_concepts_is_skipped() {
        // A document and exploration that cite each other but no concept → no destination,
        // so the cluster is omitted and the map shows the empty marker.
        let pages = [
            page("wiki/documents/a", &["wiki/explorations/b"]),
            page("wiki/explorations/b", &["wiki/documents/a"]),
        ];
        let g = graph_of(&pages);
        let md = build_map(
            &g,
            &GraphConfig::default(),
            &VaultDirs::default(),
            Locale::En,
        );
        assert!(
            md.contains(Locale::En.strings().map_empty),
            "a concept-less graph yields the empty marker:\n{md}"
        );
    }

    #[test]
    fn single_concept_groups_share_a_compact_navigation_list() {
        let g = graph_of(&[
            page("wiki/concepts/alone", &[]),
            page("wiki/concepts/sourced", &["wiki/documents/source"]),
            page("wiki/documents/source", &["wiki/concepts/sourced"]),
            page("wiki/concepts/a", &["wiki/concepts/b"]),
            page("wiki/concepts/b", &["wiki/concepts/a"]),
        ]);
        let md = build_map(
            &g,
            &GraphConfig::default(),
            &VaultDirs::default(),
            Locale::En,
        );
        assert!(md.contains("## Single-concept groups (2)"), "{md}");
        for id in ["alone", "sourced"] {
            assert_eq!(md.matches(&format!("(concepts/{id}.md)")).count(), 1);
            assert!(!md.contains(&format!("## [{id}]")));
        }
        assert!(md.contains("## [a](concepts/a.md) (2)"), "{md}");
        assert!(!md.contains(Locale::En.strings().map_empty));
    }

    #[test]
    fn large_maps_keep_all_concepts_in_bounded_details_without_promoting_evidence() {
        let mut pages: Vec<_> = (0..101)
            .map(|number| page(&format!("wiki/concepts/item-{number:03}"), &[]))
            .collect();
        pages.push(page("wiki/documents/evidence", &[]));
        let graph = graph_of(&pages);
        for locale in [Locale::Ko, Locale::En] {
            let catalog = build_map_catalog(
                &graph,
                &GraphConfig::default(),
                &VaultDirs::default(),
                locale,
            );
            let root = &catalog.pages[Path::new("wiki/map.md")];
            assert!(root.contains(locale.strings().map_intro));
            assert!(root.contains("catalog/map/0-0.md"));
            assert_eq!(catalog.pages.len(), 3);
            let mut reached = std::collections::HashSet::new();
            for (path, content) in &catalog.pages {
                assert!(
                    content
                        .lines()
                        .filter(|line| line.starts_with("- "))
                        .count()
                        <= lk_vault::NAVIGATION_PAGE_ENTRIES
                );
                assert!(!content.contains("documents/evidence"));
                if path == Path::new("wiki/map.md") {
                    continue;
                }
                assert!(content.contains(locale.strings().map_single_concept_groups));
                for dest in link::extract_dests(content) {
                    let target = link::resolve_dest(path, &dest).unwrap();
                    if target != Path::new("wiki/map.md") {
                        assert!(reached.insert(target));
                    }
                }
            }
            assert_eq!(reached.len(), 101);
            assert_eq!(
                catalog.pages,
                build_map_catalog(
                    &graph,
                    &GraphConfig::default(),
                    &VaultDirs::default(),
                    locale
                )
                .pages
            );
        }
    }
}
