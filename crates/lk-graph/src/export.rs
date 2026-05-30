use std::collections::HashMap;

use serde::Serialize;

use crate::cluster::ClusterResult;
use crate::graph::WikiGraph;

#[derive(Debug, Clone, Serialize)]
pub struct GraphExport {
    pub nodes: Vec<NodeExport>,
    pub edges: Vec<EdgeExport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeExport {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeExport {
    pub source: String,
    pub target: String,
}

pub fn export(graph: &WikiGraph, cluster: Option<&ClusterResult>) -> GraphExport {
    let community_of: HashMap<&str, u32> = cluster
        .map(|c| {
            c.communities
                .iter()
                .flat_map(|community| {
                    community
                        .members
                        .iter()
                        .map(move |id| (id.as_str(), community.id))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut nodes: Vec<NodeExport> = (0..graph.node_count())
        .map(|i| {
            let id = graph.node_id(i);
            NodeExport {
                community: community_of.get(id).copied(),
                id: id.to_owned(),
                title: graph.node_title(i).to_owned(),
            }
        })
        .collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));

    let mut edges: Vec<EdgeExport> = graph
        .edge_pairs()
        .map(|(src, tgt)| EdgeExport {
            source: graph.node_id(src).to_owned(),
            target: graph.node_id(tgt).to_owned(),
        })
        .collect();
    edges.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then_with(|| a.target.cmp(&b.target))
    });

    GraphExport { nodes, edges }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::detect_communities;
    use crate::scan::ScannedPage;
    use lk_core::config::{GraphConfig, VaultDirs};
    use std::path::PathBuf;

    fn make_page(id: &str, outgoing: &[&str]) -> ScannedPage {
        let name = id.rsplit('/').next().unwrap_or(id);
        ScannedPage {
            id: id.to_owned(),
            path: PathBuf::from(format!("{id}.md")),
            title: name.to_owned(),
            outgoing: outgoing.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn nodes_and_edges_are_present_and_sorted() {
        let pages = vec![
            make_page("c", &["a"]),
            make_page("a", &["b"]),
            make_page("b", &["c"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let export = export(&graph, None);
        assert_eq!(export.nodes.len(), 3);
        assert_eq!(export.edges.len(), 3);
        assert_eq!(
            export
                .nodes
                .iter()
                .map(|n| n.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert!(export.nodes.iter().all(|n| n.community.is_none()));
    }

    #[test]
    fn community_field_included_when_cluster_present() {
        let config = GraphConfig::default();
        let pages = vec![
            make_page("a", &["b"]),
            make_page("b", &["a"]),
            make_page("c", &["d"]),
            make_page("d", &["c"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let cluster = detect_communities(&graph, &config);
        let export = export(&graph, Some(&cluster));
        assert!(export.nodes.iter().all(|n| n.community.is_some()));
    }
}
