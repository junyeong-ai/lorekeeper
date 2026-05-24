use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;

use lk_core::concept::slugify;
use lk_core::config::GraphConfig;
use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::Serialize;

use crate::scan::Page;

pub struct WikiGraph {
    graph: DiGraph<NodeData, ()>,
    id_to_node: HashMap<String, NodeIndex>,
    name_to_node: HashMap<String, NodeIndex>,
    broken: Vec<BrokenLink>,
}

pub(crate) struct NodeData {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BrokenLink {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HubEntry {
    pub id: String,
    pub title: String,
    pub degree: usize,
    pub outgoing: usize,
    pub incoming: usize,
}

impl WikiGraph {
    pub fn build(pages: &[Page]) -> Self {
        let mut graph = DiGraph::new();
        let mut id_to_node = HashMap::with_capacity(pages.len());
        let mut name_to_node: HashMap<String, NodeIndex> = HashMap::with_capacity(pages.len());

        for page in pages {
            let node = graph.add_node(NodeData {
                id: page.id.clone(),
                title: page.title.clone(),
            });
            id_to_node.insert(page.id.clone(), node);

            let filename_slug = page
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(slugify)
                .unwrap_or_default();

            if !filename_slug.is_empty() {
                match name_to_node.entry(filename_slug) {
                    Entry::Vacant(e) => {
                        e.insert(node);
                    }
                    Entry::Occupied(e) => {
                        let existing_id = &graph[*e.get()].id;
                        eprintln!(
                            "warning: ambiguous slug '{}': {} shadows {}",
                            e.key(),
                            existing_id,
                            page.id
                        );
                    }
                }
            }
        }

        let mut broken = Vec::new();

        for page in pages {
            let source_idx = id_to_node[&page.id];
            for target in &page.outgoing {
                if let Some(&target_idx) = name_to_node.get(target.as_str()) {
                    if source_idx != target_idx {
                        graph.add_edge(source_idx, target_idx, ());
                    }
                } else {
                    broken.push(BrokenLink {
                        source: page.id.clone(),
                        target: target.clone(),
                    });
                }
            }
        }

        broken.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));
        broken.dedup_by(|a, b| a.source == b.source && a.target == b.target);

        WikiGraph {
            graph,
            id_to_node,
            name_to_node,
            broken,
        }
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    pub fn component_count(&self) -> usize {
        petgraph::algo::connected_components(&self.graph)
    }

    pub fn hubs(&self, top: usize, min_degree: usize) -> Vec<HubEntry> {
        let mut entries: Vec<HubEntry> = self
            .id_to_node
            .iter()
            .map(|(id, &idx)| {
                let out = self.graph.edges_directed(idx, Direction::Outgoing).count();
                let inc = self.graph.edges_directed(idx, Direction::Incoming).count();
                HubEntry {
                    id: id.clone(),
                    title: self.graph[idx].title.clone(),
                    degree: out + inc,
                    outgoing: out,
                    incoming: inc,
                }
            })
            .filter(|e| e.degree >= min_degree)
            .collect();

        entries.sort_by(|a, b| b.degree.cmp(&a.degree).then_with(|| a.id.cmp(&b.id)));
        entries.truncate(top);
        entries
    }

    pub fn orphans(&self, config: &GraphConfig) -> Vec<String> {
        let exclude: HashSet<&str> = config
            .graph
            .orphan_exclude
            .iter()
            .map(String::as_str)
            .collect();

        let mut result: Vec<String> = self
            .id_to_node
            .iter()
            .filter(|(id, idx)| {
                if exclude.contains(id.as_str()) {
                    return false;
                }
                let idx = **idx;
                let out = self.graph.edges_directed(idx, Direction::Outgoing).count();
                let inc = self.graph.edges_directed(idx, Direction::Incoming).count();
                out == 0 && inc == 0
            })
            .map(|(id, _)| id.clone())
            .collect();

        result.sort();
        result
    }

    pub fn broken_links(&self) -> &[BrokenLink] {
        &self.broken
    }

    pub fn node_ids(&self) -> impl Iterator<Item = &str> {
        self.id_to_node.keys().map(String::as_str)
    }

    pub fn resolve_filename(&self, slug: &str) -> Option<&str> {
        self.name_to_node
            .get(slug)
            .map(|&idx| self.graph[idx].id.as_str())
    }

    pub fn node_index(&self, id: &str) -> Option<usize> {
        self.id_to_node.get(id).map(|n| n.index())
    }

    pub fn node_id(&self, index: usize) -> &str {
        &self.graph[NodeIndex::new(index)].id
    }

    pub fn node_title(&self, index: usize) -> &str {
        &self.graph[NodeIndex::new(index)].title
    }

    pub fn node_degree(&self, index: usize) -> usize {
        let idx = NodeIndex::new(index);
        self.graph.edges_directed(idx, Direction::Outgoing).count()
            + self.graph.edges_directed(idx, Direction::Incoming).count()
    }

    pub fn edge_pairs(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.graph.edge_indices().filter_map(|e| {
            self.graph
                .edge_endpoints(e)
                .map(|(s, t)| (s.index(), t.index()))
        })
    }

    /// Sorted unique neighbor node indices (in or out) of `node`. Used by
    /// `suggest_links` to rank candidate pairs by shared-neighbor count.
    pub(crate) fn neighbors(&self, index: usize) -> Vec<usize> {
        let idx = NodeIndex::new(index);
        let mut ns: Vec<usize> = self
            .graph
            .neighbors_undirected(idx)
            .map(|n| n.index())
            .collect();
        ns.sort_unstable();
        ns.dedup();
        ns
    }

    /// Whether a directed edge exists in either direction between two node indices.
    pub(crate) fn has_edge_between(&self, a: usize, b: usize) -> bool {
        let (a, b) = (NodeIndex::new(a), NodeIndex::new(b));
        self.graph.find_edge(a, b).is_some() || self.graph.find_edge(b, a).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Page;
    use std::path::PathBuf;

    fn make_page(id: &str, outgoing: &[&str]) -> Page {
        let name = id.rsplit('/').next().unwrap_or(id);
        Page {
            id: id.to_owned(),
            path: PathBuf::from(format!("{id}.md")),
            title: name.to_owned(),
            outgoing: outgoing.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn basic_graph_construction() {
        let pages = vec![
            make_page("wiki/alpha", &["beta"]),
            make_page("wiki/beta", &["alpha"]),
            make_page("wiki/gamma", &[]),
        ];
        let g = WikiGraph::build(&pages);

        assert_eq!(g.node_count(), 3);
        assert_eq!(g.edge_count(), 2);
        assert_eq!(g.component_count(), 2);
    }

    #[test]
    fn broken_links_detected() {
        let pages = vec![
            make_page("wiki/alpha", &["nonexistent"]),
            make_page("wiki/beta", &[]),
        ];
        let g = WikiGraph::build(&pages);

        assert_eq!(g.broken_links().len(), 1);
        assert_eq!(g.broken_links()[0].source, "wiki/alpha");
        assert_eq!(g.broken_links()[0].target, "nonexistent");
    }

    #[test]
    fn hubs_sorted_by_degree() {
        let pages = vec![
            make_page("wiki/hub", &["a", "b", "c"]),
            make_page("wiki/a", &["hub"]),
            make_page("wiki/b", &["hub"]),
            make_page("wiki/c", &[]),
        ];
        let g = WikiGraph::build(&pages);

        let hubs = g.hubs(10, 1);
        assert!(!hubs.is_empty());
        assert_eq!(hubs[0].id, "wiki/hub");
        assert!(hubs[0].degree >= 3);
    }

    #[test]
    fn orphans_detected() {
        let config = GraphConfig::default();
        let pages = vec![
            make_page("wiki/connected", &["other"]),
            make_page("wiki/other", &[]),
            make_page("wiki/orphan", &[]),
        ];
        let g = WikiGraph::build(&pages);

        let orphans = g.orphans(&config);
        assert_eq!(orphans, vec!["wiki/orphan"]);
    }

    #[test]
    fn orphan_exclude_respected() {
        let mut config = GraphConfig::default();
        config.graph.orphan_exclude = vec!["wiki/orphan".to_owned()];

        let pages = vec![
            make_page("wiki/connected", &["other"]),
            make_page("wiki/other", &[]),
            make_page("wiki/orphan", &[]),
        ];
        let g = WikiGraph::build(&pages);

        let orphans = g.orphans(&config);
        assert!(orphans.is_empty());
    }

    #[test]
    fn self_links_excluded() {
        let pages = vec![make_page("wiki/self", &["self"])];
        let g = WikiGraph::build(&pages);

        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn filename_resolution() {
        let pages = vec![
            make_page("wiki/concept-a", &["concept-b"]),
            make_page("wiki/concept-b", &[]),
        ];
        let g = WikiGraph::build(&pages);

        assert_eq!(g.edge_count(), 1);
        assert!(g.broken_links().is_empty());
    }

    #[test]
    fn duplicate_filename_keeps_first() {
        let pages = vec![make_page("wiki/alpha", &[]), make_page("docs/alpha", &[])];
        let g = WikiGraph::build(&pages);

        assert_eq!(g.node_count(), 2);
        let resolved = g.resolve_filename("alpha");
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap(), "wiki/alpha");
    }
}
