//! Knowledge map: a navigable, agent-readable index of the wiki graph organized by
//! citation cluster. It MATERIALIZES the Louvain communities (which `graph cluster`
//! otherwise computes and discards) into `<wiki>/map.md`, so an agent — or a human in
//! Obsidian — can navigate the vault by emergent structure (what co-occurs with what)
//! without any embedding or retrieval layer. "Navigate, don't retrieve."
//!
//! This is a read-only materialized view, regenerated wholesale each run like `index.md`
//! and `log.md` — it never writes `[[related]]` edges into concept pages. Communities
//! encode *co-citation*, not curated topical relatedness, so the page is labelled as a
//! citation map, never presented as authored relationships.

use std::collections::HashMap;
use std::fmt::Write as _;

use lk_core::config::{GraphConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_core::vault_path::concepts_dir;

use crate::cluster::detect_communities;
use crate::graph::WikiGraph;

/// Build the `map.md` markdown for `graph`. Concepts are the knowledge nodes the map
/// navigates to; documents and explorations stay in the graph as CLUSTERING EVIDENCE
/// (their citations shape the communities) but are not listed as destinations. Each
/// listed cluster is headed by its highest-degree concept. Deterministic: clusters are
/// sorted largest-first with a stable tiebreak, and concepts within a cluster by
/// descending degree then id, so the same vault always yields byte-identical output.
pub fn build_map(
    graph: &WikiGraph,
    config: &GraphConfig,
    dirs: &VaultDirs,
    locale: Locale,
) -> String {
    let strings = locale.strings();
    let result = detect_communities(graph, config);

    // Degree of every node (incl. citations from documents/explorations), for hub-first
    // ordering and the per-concept link annotation.
    let degree: HashMap<String, usize> = graph
        .hubs(usize::MAX, 0)
        .into_iter()
        .map(|h| (h.id, h.degree))
        .collect();

    // A concept page id is `{wiki}/concepts/{slug}` (per-segment slugified).
    let concept_prefix = format!("{}/", crate::scan::path_slug(&concepts_dir(dirs)));

    let mut out = String::new();
    writeln!(out, "# {}", strings.map_title).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{}", strings.map_intro).unwrap();
    writeln!(out).unwrap();

    let mut any = false;
    for community in &result.communities {
        let mut concepts: Vec<&String> = community
            .members
            .iter()
            .filter(|id| id.starts_with(&concept_prefix))
            .collect();
        if concepts.is_empty() {
            // A cluster of only documents/explorations has no concept to navigate to.
            continue;
        }
        concepts.sort_by(|a, b| degree.get(*b).cmp(&degree.get(*a)).then_with(|| a.cmp(b)));
        // The highest-degree concept is the cluster's representative (its hub).
        let hub = concepts[0];
        writeln!(out, "## [[{}|{}]] ({})", hub, leaf(hub), concepts.len()).unwrap();
        writeln!(out).unwrap();
        for id in &concepts {
            let links = degree.get(*id).copied().unwrap_or(0);
            writeln!(
                out,
                "- [[{}|{}]] · {} {}",
                id,
                leaf(id),
                links,
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

    out
}

/// The last path segment of a node id (`wiki/concepts/foo` → `foo`) — the readable
/// display label for an unambiguous path-form wikilink target.
fn leaf(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScannedPage;
    use lk_core::config::VaultDirs;
    use std::path::PathBuf;

    fn page(id: &str, outgoing: &[&str]) -> ScannedPage {
        ScannedPage {
            id: id.to_string(),
            path: PathBuf::from(format!("{id}.md")),
            title: leaf(id).to_string(),
            outgoing: outgoing.iter().map(|s| s.to_string()).collect(),
            aliases: vec![],
        }
    }

    fn graph_of(pages: &[ScannedPage]) -> WikiGraph {
        WikiGraph::build(pages, &VaultDirs::default())
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
        // are listed, by path id with leaf display.
        let pages = [
            page("wiki/concepts/rag", &["wiki/concepts/embeddings"]),
            page("wiki/concepts/embeddings", &["wiki/concepts/rag"]),
            page("wiki/documents/survey", &["wiki/concepts/rag"]),
        ];
        let g = graph_of(&pages);
        let md = build_map(
            &g,
            &GraphConfig::default(),
            &VaultDirs::default(),
            Locale::En,
        );
        assert!(
            md.contains("[[wiki/concepts/rag|rag]]"),
            "concepts are listed by path id with leaf display:\n{md}"
        );
        assert!(
            md.contains("[[wiki/concepts/embeddings|embeddings]]"),
            "both concepts listed:\n{md}"
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
}
