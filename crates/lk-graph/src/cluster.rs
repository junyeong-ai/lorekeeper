use std::collections::BTreeMap;

use lk_core::config::GraphConfig;
use serde::Serialize;

use crate::graph::WikiGraph;

#[derive(Debug, Clone, Serialize)]
pub struct Community {
    pub id: u32,
    pub size: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterResult {
    pub communities: Vec<Community>,
    pub modularity: f64,
    pub iterations: usize,
}

pub fn detect_communities(graph: &WikiGraph, config: &GraphConfig) -> ClusterResult {
    let n = graph.node_count();
    if n == 0 {
        return ClusterResult {
            communities: Vec::new(),
            modularity: 0.0,
            iterations: 0,
        };
    }

    let mut adjacency: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); n];
    let mut total_weight = 0.0f64;
    for (src, tgt) in graph.edge_pairs() {
        if src == tgt {
            continue;
        }
        *adjacency[src].entry(tgt).or_insert(0.0) += 1.0;
        *adjacency[tgt].entry(src).or_insert(0.0) += 1.0;
        total_weight += 1.0;
    }

    let degree: Vec<f64> = adjacency
        .iter()
        .map(|nbrs| nbrs.values().sum::<f64>())
        .collect();

    let n_u32 = u32::try_from(n).expect("node count exceeds u32::MAX");

    let (membership, iterations) = if total_weight > 0.0 {
        run_louvain(&adjacency, &degree, total_weight, n_u32, config)
    } else {
        ((0..n_u32).collect(), 0)
    };

    let mut by_community: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (i, &c) in membership.iter().enumerate() {
        by_community.entry(c).or_default().push(i);
    }

    let min_size = config.cluster.min_community_size;

    // Modularity measures the partition Louvain actually optimized. Communities below
    // `min_size` are dropped from the reported result (the filter below) but are never
    // re-partitioned first: scattering their nodes into singletons would skew this
    // metric away from the clustering it is meant to describe.
    let modularity = compute_modularity(
        &adjacency,
        &degree,
        total_weight,
        &membership,
        config.cluster.resolution,
    );

    let mut communities: Vec<Community> = by_community
        .into_iter()
        .filter(|(_, members)| members.len() >= min_size)
        .map(|(_, members)| build_community(graph, members))
        .collect();

    // Largest community first; ties broken by smallest first-member id so the output
    // order is deterministic. Ids are then renumbered to match this display order — the
    // `id` is an output rank, not a sort key.
    communities.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then_with(|| a.members[0].cmp(&b.members[0]))
    });

    for (new_id, community) in communities.iter_mut().enumerate() {
        community.id = u32::try_from(new_id).expect("community count exceeds u32::MAX");
    }

    ClusterResult {
        communities,
        modularity,
        iterations,
    }
}

pub fn label_communities(graph: &WikiGraph, communities: &mut [Community]) {
    for community in communities {
        community.label = community
            .members
            .iter()
            .max_by_key(|id| graph.node_index(id).map_or(0, |i| graph.node_degree(i)))
            .cloned();
    }
}

fn build_community(graph: &WikiGraph, mut member_indices: Vec<usize>) -> Community {
    member_indices.sort_unstable();
    let mut members: Vec<String> = member_indices
        .iter()
        .map(|&i| graph.node_id(i).to_owned())
        .collect();
    members.sort();
    Community {
        id: 0,
        size: members.len(),
        label: None,
        members,
    }
}

fn run_louvain(
    adjacency: &[BTreeMap<usize, f64>],
    degree: &[f64],
    total_weight: f64,
    n_u32: u32,
    config: &GraphConfig,
) -> (Vec<u32>, usize) {
    let n = adjacency.len();
    let mut membership: Vec<u32> = (0..n_u32).collect();
    let mut community_total: BTreeMap<u32, f64> =
        (0..n_u32).map(|i| (i, degree[i as usize])).collect();

    let resolution = config.cluster.resolution;
    let two_m = 2.0 * total_weight;
    let mut iterations = 0;

    for _ in 0..config.cluster.max_iterations {
        iterations += 1;
        let mut changed = false;

        for node in 0..n {
            let current = membership[node];
            let k_i = degree[node];

            let mut weights_to: BTreeMap<u32, f64> = BTreeMap::new();
            for (&nbr, &w) in &adjacency[node] {
                if nbr == node {
                    continue;
                }
                *weights_to.entry(membership[nbr]).or_insert(0.0) += w;
            }

            let k_i_in_current = weights_to.get(&current).copied().unwrap_or(0.0);
            let sigma_tot_current = community_total[&current] - k_i;

            let mut best_community = current;
            let mut best_gain = 0.0f64;

            for (&target, &k_i_in) in &weights_to {
                if target == current {
                    continue;
                }
                let sigma_tot_target = community_total[&target];
                let gain = (k_i_in - k_i_in_current) / total_weight
                    - resolution * k_i * (sigma_tot_target - sigma_tot_current)
                        / (two_m * total_weight);
                if gain > best_gain {
                    best_gain = gain;
                    best_community = target;
                }
            }

            if best_community != current {
                membership[node] = best_community;
                *community_total.get_mut(&current).unwrap() -= k_i;
                *community_total.entry(best_community).or_insert(0.0) += k_i;
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    (membership, iterations)
}

fn compute_modularity(
    adjacency: &[BTreeMap<usize, f64>],
    degree: &[f64],
    total_weight: f64,
    membership: &[u32],
    resolution: f64,
) -> f64 {
    if total_weight == 0.0 {
        return 0.0;
    }
    let two_m = 2.0 * total_weight;

    let mut internal = 0.0f64;
    for i in 0..adjacency.len() {
        for (&j, &w) in &adjacency[i] {
            if membership[i] == membership[j] {
                internal += w;
            }
        }
    }

    let mut k_per_community: BTreeMap<u32, f64> = BTreeMap::new();
    for (i, &c) in membership.iter().enumerate() {
        *k_per_community.entry(c).or_insert(0.0) += degree[i];
    }
    let penalty: f64 = k_per_community.values().map(|&k_c| k_c * k_c).sum();

    (internal - resolution * penalty / two_m) / two_m
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkSuggestion {
    pub a: String,
    pub b: String,
    pub shared_neighbors: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuggestResult {
    pub pairs: Vec<LinkSuggestion>,
}

/// Suggest wikilinks to add: pairs of pages in the SAME Louvain community that have NO
/// edge between them and share at least `min_shared_neighbors` neighbors, ranked by
/// shared-neighbor count (descending). The floor suppresses co-citation noise (a single
/// shared neighbor usually means "co-cited by one daily page", not a real relationship).
/// Pure, deterministic, read-only — it reuses the community assignment passed in and
/// never touches the vault.
pub fn suggest_links(
    graph: &WikiGraph,
    cluster: &ClusterResult,
    min_shared_neighbors: usize,
) -> SuggestResult {
    let mut pairs: Vec<LinkSuggestion> = Vec::new();

    for community in &cluster.communities {
        // Resolve members to node indices, dropping any that no longer resolve.
        let mut indices: Vec<usize> = community
            .members
            .iter()
            .filter_map(|id| graph.node_index(id))
            .collect();
        indices.sort_unstable();

        // Precompute neighbor sets once per node for shared-neighbor counting.
        let neighbor_sets: Vec<Vec<usize>> = indices.iter().map(|&i| graph.neighbors(i)).collect();

        for i in 0..indices.len() {
            for j in (i + 1)..indices.len() {
                let (a, b) = (indices[i], indices[j]);
                if graph.has_edge_between(a, b) {
                    continue;
                }
                let shared = shared_count(&neighbor_sets[i], &neighbor_sets[j]);
                if shared < min_shared_neighbors.max(1) {
                    continue;
                }
                let (id_a, id_b) = (graph.node_id(a).to_owned(), graph.node_id(b).to_owned());
                pairs.push(LinkSuggestion {
                    a: id_a,
                    b: id_b,
                    shared_neighbors: shared,
                });
            }
        }
    }

    // Most-shared first; ties broken lexicographically for deterministic output.
    pairs.sort_by(|x, y| {
        y.shared_neighbors
            .cmp(&x.shared_neighbors)
            .then_with(|| x.a.cmp(&y.a))
            .then_with(|| x.b.cmp(&y.b))
    });

    SuggestResult { pairs }
}

/// Count common elements of two sorted, deduped index slices.
fn shared_count(a: &[usize], b: &[usize]) -> usize {
    let (mut i, mut j, mut count) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                count += 1;
                i += 1;
                j += 1;
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScannedPage;
    use lk_core::config::VaultDirs;
    use std::path::PathBuf;

    fn make_page(id: &str, outgoing: &[&str]) -> ScannedPage {
        let name = id.rsplit('/').next().unwrap_or(id);
        ScannedPage {
            id: id.to_owned(),
            path: PathBuf::from(format!("{id}.md")),
            title: name.to_owned(),
            outgoing: outgoing.iter().map(|s| s.to_string()).collect(),
            aliases: Vec::new(),
        }
    }

    #[test]
    fn empty_graph_returns_empty() {
        let config = GraphConfig::default();
        let graph = WikiGraph::build(&[], &VaultDirs::default());
        let result = detect_communities(&graph, &config);
        assert!(result.communities.is_empty());
        assert_eq!(result.modularity, 0.0);
    }

    #[test]
    fn two_disconnected_components_form_two_communities() {
        let config = GraphConfig::default();
        let pages = vec![
            make_page("a", &["b"]),
            make_page("b", &["a"]),
            make_page("c", &["d"]),
            make_page("d", &["c"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let result = detect_communities(&graph, &config);
        assert_eq!(result.communities.len(), 2);
        for c in &result.communities {
            assert_eq!(c.size, 2);
        }
    }

    #[test]
    fn dense_clique_forms_one_community() {
        let config = GraphConfig::default();
        let pages = vec![
            make_page("a", &["b", "c", "d"]),
            make_page("b", &["a", "c", "d"]),
            make_page("c", &["a", "b", "d"]),
            make_page("d", &["a", "b", "c"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let result = detect_communities(&graph, &config);
        assert_eq!(result.communities.len(), 1);
        assert_eq!(result.communities[0].size, 4);
    }

    #[test]
    fn min_community_size_filters_small_communities() {
        let mut config = GraphConfig::default();
        config.cluster.min_community_size = 3;
        let pages = vec![
            make_page("a", &["b"]),
            make_page("b", &["a"]),
            make_page("c", &["d", "e"]),
            make_page("d", &["c", "e"]),
            make_page("e", &["c", "d"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let result = detect_communities(&graph, &config);
        assert_eq!(result.communities.len(), 1);
        assert_eq!(result.communities[0].size, 3);
    }

    #[test]
    fn labels_default_to_none() {
        let config = GraphConfig::default();
        let pages = vec![
            make_page("a", &["b", "c"]),
            make_page("b", &["a"]),
            make_page("c", &["a"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let result = detect_communities(&graph, &config);
        assert!(result.communities.iter().all(|c| c.label.is_none()));
    }

    #[test]
    fn label_communities_picks_highest_degree_member() {
        let config = GraphConfig::default();
        let pages = vec![
            make_page("hub", &["leaf-1", "leaf-2", "leaf-3"]),
            make_page("leaf-1", &["hub"]),
            make_page("leaf-2", &["hub"]),
            make_page("leaf-3", &["hub"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let mut result = detect_communities(&graph, &config);
        label_communities(&graph, &mut result.communities);
        assert_eq!(result.communities.len(), 1);
        assert_eq!(result.communities[0].label.as_deref(), Some("hub"));
    }

    #[test]
    fn communities_sorted_by_size_with_sequential_ids() {
        let config = GraphConfig::default();
        let pages = vec![
            make_page("a", &["b", "c"]),
            make_page("b", &["a", "c"]),
            make_page("c", &["a", "b"]),
            make_page("x", &["y"]),
            make_page("y", &["x"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let result = detect_communities(&graph, &config);
        assert_eq!(result.communities.len(), 2);
        assert!(result.communities[0].size >= result.communities[1].size);
        for (i, c) in result.communities.iter().enumerate() {
            assert_eq!(c.id, i as u32);
        }
    }

    #[test]
    fn modularity_positive_for_clear_structure() {
        let config = GraphConfig::default();
        let pages = vec![
            make_page("a1", &["a2", "a3"]),
            make_page("a2", &["a1", "a3"]),
            make_page("a3", &["a1", "a2"]),
            make_page("b1", &["b2", "b3"]),
            make_page("b2", &["b1", "b3"]),
            make_page("b3", &["b1", "b2"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let result = detect_communities(&graph, &config);
        assert!(result.modularity > 0.0);
    }

    #[test]
    fn suggest_links_finds_unlinked_pair_with_shared_neighbor() {
        // a–b and a–c are linked (a is a hub) but b and c are NOT linked to each other,
        // yet share neighbor `a` and sit in the same community → one suggestion.
        let config = GraphConfig::default();
        let pages = vec![
            make_page("a", &["b", "c"]),
            make_page("b", &["a"]),
            make_page("c", &["a"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let cluster = detect_communities(&graph, &config);
        let result = suggest_links(&graph, &cluster, 1);
        assert_eq!(result.pairs.len(), 1);
        let s = &result.pairs[0];
        assert_eq!((s.a.as_str(), s.b.as_str()), ("b", "c"));
        assert_eq!(s.shared_neighbors, 1);
    }

    #[test]
    fn suggest_links_floor_suppresses_single_shared_neighbor() {
        // b and c share exactly ONE neighbor (a). With the default floor of 2 that
        // co-citation-level signal is suppressed.
        let config = GraphConfig::default();
        let pages = vec![
            make_page("a", &["b", "c"]),
            make_page("b", &["a"]),
            make_page("c", &["a"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let cluster = detect_communities(&graph, &config);
        let result = suggest_links(&graph, &cluster, 2);
        assert!(
            result.pairs.is_empty(),
            "single shared neighbor must not be suggested at floor 2"
        );
    }

    #[test]
    fn suggest_links_skips_already_linked_pairs() {
        // A complete triangle: every pair already has an edge → no suggestions.
        let config = GraphConfig::default();
        let pages = vec![
            make_page("a", &["b", "c"]),
            make_page("b", &["a", "c"]),
            make_page("c", &["a", "b"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let cluster = detect_communities(&graph, &config);
        let result = suggest_links(&graph, &cluster, 1);
        assert!(result.pairs.is_empty());
    }

    #[test]
    fn suggest_links_ranked_by_shared_neighbor_count() {
        // hub1 and hub2 each connect to {x,y,z}; the leaves share more neighbors with
        // each other than with the hubs.
        let config = GraphConfig::default();
        let pages = vec![
            make_page("hub1", &["x", "y", "z"]),
            make_page("hub2", &["x", "y", "z"]),
            make_page("x", &["hub1", "hub2"]),
            make_page("y", &["hub1", "hub2"]),
            make_page("z", &["hub1", "hub2"]),
        ];
        let graph = WikiGraph::build(&pages, &VaultDirs::default());
        let cluster = detect_communities(&graph, &config);
        let result = suggest_links(&graph, &cluster, 1);
        assert!(!result.pairs.is_empty());
        // Output is sorted by shared_neighbors descending.
        for w in result.pairs.windows(2) {
            assert!(w[0].shared_neighbors >= w[1].shared_neighbors);
        }
    }
}
