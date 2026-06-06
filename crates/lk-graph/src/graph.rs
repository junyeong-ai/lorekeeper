use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::path::Path;

use lk_core::config::VaultDirs;

use crate::scan::{ScannedPage, VaultExistence, is_concept_page, stem_slug};

pub struct WikiGraph {
    graph: DiGraph<NodeData, ()>,
    id_to_node: HashMap<String, NodeIndex>,
    broken: Vec<BrokenLink>,
    /// Scope node ids connected to pages outside the analysis scope (linked from,
    /// or linking out to, a page that exists in the vault but is not in scope)
    /// and so are never orphans even with zero in-scope edges. Derived from the
    /// `VaultExistence` passed to [`Self::build`].
    cross_scope_connected: HashSet<String>,
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
pub struct HubPageRef {
    pub id: String,
    pub title: String,
    pub degree: usize,
    pub outgoing: usize,
    pub incoming: usize,
}

impl WikiGraph {
    /// Build the analysis graph over `pages`, treating that set as the whole
    /// universe: a wikilink with no resolvable target is broken, and a page with
    /// no edges is an orphan. The right model when the scope *is* the vault (and
    /// the convenient default for tests); for a narrowed scope whose links reach
    /// pages outside it, use [`Self::build_with_existence`].
    pub fn build(pages: &[ScannedPage], dirs: &VaultDirs) -> Self {
        Self::build_with_existence(pages, &VaultExistence::from_pages(pages, dirs), dirs)
    }

    /// Build the analysis graph over `pages` (the scope) while resolving
    /// integrity checks against the full-vault `existence` universe.
    ///
    /// The graph itself — nodes and edges — stays scope-internal, so `hubs`,
    /// `cluster`, and `suggest_links` are unaffected. Only the two integrity
    /// checks consult `existence`:
    /// - **broken links**: a wikilink leaving the scope is broken *only* if its
    ///   target does not exist anywhere in the vault.
    /// - **orphans**: a scope page is exempt when it links to, or is linked from,
    ///   any page in the vault (tracked in `cross_scope_connected`).
    pub fn build_with_existence(
        pages: &[ScannedPage],
        existence: &VaultExistence,
        dirs: &VaultDirs,
    ) -> Self {
        let mut graph = DiGraph::new();
        let mut id_to_node = HashMap::with_capacity(pages.len());
        // Maps a bare-target filename slug to its node, tagging whether the owner is
        // a concept page. A bare `[[name]]` is a knowledge-node reference, so a
        // concept claims the slug over a same-named non-concept (deterministic, no
        // warning); only a same-class collision is a genuine ambiguity worth warning.
        let mut name_to_node: HashMap<String, (NodeIndex, bool)> =
            HashMap::with_capacity(pages.len());

        for page in pages {
            let node = graph.add_node(NodeData {
                id: page.id.clone(),
                title: page.title.clone(),
            });
            id_to_node.insert(page.id.clone(), node);

            let slug = stem_slug(&page.path);
            if !slug.is_empty() {
                let is_concept = is_concept_page(&page.path, dirs);
                match name_to_node.entry(slug) {
                    Entry::Vacant(e) => {
                        e.insert((node, is_concept));
                    }
                    Entry::Occupied(mut e) => {
                        let (existing_node, existing_is_concept) = *e.get();
                        if is_concept && !existing_is_concept {
                            // Concept claims the bare slug from a non-concept — intended.
                            e.insert((node, true));
                        } else if is_concept == existing_is_concept {
                            // Same class (two documents in different dirs, etc.) — a real
                            // ambiguity. Keep the first; warn through `tracing` (not
                            // stdout) so it honours log config. A bare `[[slug]]` link
                            // resolves to the kept page; the shadowed page stays reachable
                            // by its path form.
                            tracing::warn!(
                                slug = %e.key(),
                                kept = %graph[existing_node].id,
                                shadowed = %page.id,
                                "ambiguous bare slug: two same-class pages share it"
                            );
                        }
                    }
                }
            }
        }

        // Aliases form the lowest-precedence resolution layer (after filename and id):
        // a concept's declared `aliases` let a bare `[[synonym]]` link land on it, but
        // never shadow a real page. When two concepts claim the same alias the smallest-id
        // concept wins — order-independent and the SAME tiebreak `VaultExistence`/`backlinks`
        // use, so all three resolvers pick the same concept. Real-page membership comes from
        // the full-vault `existence` (not just the in-scope nodes), so this precedence
        // matches even when the analysis scope is a strict subset of the vault.
        let mut alias_to_node: HashMap<String, (&str, NodeIndex)> = HashMap::new();
        for page in pages {
            if !is_concept_page(&page.path, dirs) {
                continue;
            }
            let node = id_to_node[&page.id];
            for alias in &page.aliases {
                if existence.is_real_page(alias) {
                    continue;
                }
                alias_to_node
                    .entry(alias.clone())
                    .and_modify(|(cur_id, cur_node)| {
                        if page.id.as_str() < *cur_id {
                            *cur_id = page.id.as_str();
                            *cur_node = node;
                        }
                    })
                    .or_insert((page.id.as_str(), node));
            }
        }

        let mut broken = Vec::new();
        let mut cross_scope_connected = HashSet::new();

        for page in pages {
            let source_idx = id_to_node[&page.id];
            for target in &page.outgoing {
                // Resolve a bare target by filename slug, a path-style target by
                // page id — the two forms `resolve_wikilink_target` produces.
                let in_scope = name_to_node
                    .get(target.as_str())
                    .map(|(node, _)| *node)
                    .or_else(|| id_to_node.get(target.as_str()).copied())
                    .or_else(|| alias_to_node.get(target.as_str()).map(|(_, node)| *node));
                if let Some(target_idx) = in_scope {
                    if source_idx != target_idx {
                        graph.add_edge(source_idx, target_idx, ());
                    }
                } else if existence.resolves(target) {
                    // Resolves to a page outside the analysis scope: not broken,
                    // and a vault-wide outbound connection for orphan purposes.
                    // No edge — the target node is not in the scope graph.
                    cross_scope_connected.insert(page.id.clone());
                } else {
                    broken.push(BrokenLink {
                        source: page.id.clone(),
                        target: target.clone(),
                    });
                }
            }

            // Inbound from anywhere in the vault: this page is the resolved
            // target of a link from another page (possibly out of scope), even
            // if nothing in scope links it.
            if existence.is_linked(&page.id) {
                cross_scope_connected.insert(page.id.clone());
            }
        }

        broken.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));
        broken.dedup_by(|a, b| a.source == b.source && a.target == b.target);

        WikiGraph {
            graph,
            id_to_node,
            broken,
            cross_scope_connected,
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

    pub fn hubs(&self, top: usize, min_degree: usize) -> Vec<HubPageRef> {
        let mut entries: Vec<HubPageRef> = self
            .id_to_node
            .iter()
            .map(|(id, &idx)| {
                let out = self.graph.edges_directed(idx, Direction::Outgoing).count();
                let inc = self.graph.edges_directed(idx, Direction::Incoming).count();
                HubPageRef {
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

    pub fn orphans(&self, orphan_exclude: &[String], wiki_dir: &Path) -> Vec<String> {
        let mut exclude: HashSet<String> = orphan_exclude.iter().cloned().collect();
        exclude.extend(crate::scan::reserved_page_ids(wiki_dir));

        let mut result: Vec<String> = self
            .id_to_node
            .iter()
            .filter(|(id, idx)| {
                if exclude.contains(id.as_str()) {
                    return false;
                }
                // A vault-wide connection (in- or out-of-scope) means it is not
                // truly orphaned, even with zero in-scope edges.
                if self.cross_scope_connected.contains(id.as_str()) {
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
    use crate::scan::{ScannedPage, VaultExistence};
    use lk_core::config::GraphConfig;
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
    fn basic_graph_construction() {
        let pages = vec![
            make_page("wiki/alpha", &["beta"]),
            make_page("wiki/beta", &["alpha"]),
            make_page("wiki/gamma", &[]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

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
        let g = WikiGraph::build(&pages, &VaultDirs::default());

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
        let g = WikiGraph::build(&pages, &VaultDirs::default());

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
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        let orphans = g.orphans(&config.metrics.orphan_exclude, Path::new("wiki"));
        assert_eq!(orphans, vec!["wiki/orphan"]);
    }

    #[test]
    fn orphan_exclude_respected() {
        let mut config = GraphConfig::default();
        config.metrics.orphan_exclude = vec!["wiki/orphan".to_owned()];

        let pages = vec![
            make_page("wiki/connected", &["other"]),
            make_page("wiki/other", &[]),
            make_page("wiki/orphan", &[]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        let orphans = g.orphans(&config.metrics.orphan_exclude, Path::new("wiki"));
        assert!(orphans.is_empty());
    }

    #[test]
    fn self_links_excluded() {
        let pages = vec![make_page("wiki/self", &["self"])];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn filename_resolution() {
        let pages = vec![
            make_page("wiki/concept-a", &["concept-b"]),
            make_page("wiki/concept-b", &[]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        assert_eq!(g.edge_count(), 1);
        assert!(g.broken_links().is_empty());
    }

    #[test]
    fn duplicate_filename_keeps_first() {
        // Two pages share the filename slug `alpha`; a `[[alpha]]` link resolves
        // to the first-inserted page (the second is shadowed, with a warning).
        let pages = vec![
            make_page("wiki/alpha", &[]),
            make_page("docs/alpha", &[]),
            make_page("wiki/linker", &["alpha"]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        assert_eq!(g.node_count(), 3);
        let edges: Vec<(String, String)> = g
            .edge_pairs()
            .map(|(s, t)| (g.node_id(s).to_owned(), g.node_id(t).to_owned()))
            .collect();
        assert_eq!(
            edges,
            vec![("wiki/linker".to_owned(), "wiki/alpha".to_owned())]
        );
        assert!(g.broken_links().is_empty());
    }

    #[test]
    fn concept_owns_bare_slug_over_same_named_document() {
        // A document and a concept legitimately share the name `x`. A bare `[[x]]`
        // is a knowledge-node reference, so it resolves to the CONCEPT — regardless
        // of scan order (the document is listed first here) and with no ambiguity
        // warning. The document is reached only by its path form `[[wiki/documents/x]]`.
        let pages = vec![
            make_page("wiki/documents/x", &[]),
            make_page("wiki/concepts/x", &[]),
            make_page("wiki/linker", &["x"]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());
        let edges: Vec<(String, String)> = g
            .edge_pairs()
            .map(|(s, t)| (g.node_id(s).to_owned(), g.node_id(t).to_owned()))
            .collect();
        assert_eq!(
            edges,
            vec![("wiki/linker".to_owned(), "wiki/concepts/x".to_owned())],
            "bare [[x]] must resolve to the concept, not the same-named document"
        );
        assert!(g.broken_links().is_empty());
    }

    #[test]
    fn path_style_link_resolves_to_page_id() {
        // `[[wiki/sub/b]]` (path form) resolves to the page whose id is
        // `wiki/sub/b`, not just the filename `b`.
        let pages = vec![
            make_page("wiki/a", &["wiki/sub/b"]),
            make_page("wiki/sub/b", &[]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        assert_eq!(g.edge_count(), 1);
        assert!(g.broken_links().is_empty());
    }

    #[test]
    fn bare_alias_link_resolves_to_concept() {
        // `[[k8s]]` where `k8s` is a declared alias of the kubernetes concept must
        // form a real edge to that concept node (not a broken link).
        let pages = vec![
            ScannedPage {
                id: "wiki/concepts/kubernetes".to_owned(),
                path: PathBuf::from("wiki/concepts/kubernetes.md"),
                title: "Kubernetes".to_owned(),
                outgoing: vec![],
                aliases: vec!["k8s".to_owned()],
            },
            make_page("wiki/linker", &["k8s"]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());
        assert_eq!(
            g.edge_count(),
            1,
            "[[k8s]] must resolve to the kubernetes concept via its alias"
        );
        assert!(g.broken_links().is_empty());
    }

    #[test]
    fn cross_scope_link_not_broken_with_existence() {
        // A `wiki/` concept links a `daily/` page (out of the analysis scope).
        // Without the existence universe it reads as broken (legacy); with it,
        // the link resolves to an existing vault page and is exempt.
        let scope = vec![make_page(
            "wiki/concepts/foo",
            &["daily/team-slack/2026-05-22"],
        )];
        let full = vec![
            make_page("wiki/concepts/foo", &["daily/team-slack/2026-05-22"]),
            make_page("daily/team-slack/2026-05-22", &[]),
        ];

        let legacy = WikiGraph::build(&scope, &VaultDirs::default());
        assert_eq!(legacy.broken_links().len(), 1);

        let existence = VaultExistence::from_pages(&full, &VaultDirs::default());
        let g = WikiGraph::build_with_existence(&scope, &existence, &VaultDirs::default());
        assert!(g.broken_links().is_empty());
    }

    #[test]
    fn orphan_exempted_by_external_inbound() {
        // A `wiki/` concept with no in-scope edges, linked only from a `daily/`
        // page (out of scope), is not a true orphan.
        let config = GraphConfig::default();
        let scope = vec![make_page("wiki/concepts/bar", &[])];
        let full = vec![
            make_page("wiki/concepts/bar", &[]),
            make_page("daily/team-slack/2026-05-22", &["bar"]),
        ];

        let legacy = WikiGraph::build(&scope, &VaultDirs::default());
        assert_eq!(
            legacy.orphans(&config.metrics.orphan_exclude, Path::new("wiki")),
            vec!["wiki/concepts/bar"]
        );

        let existence = VaultExistence::from_pages(&full, &VaultDirs::default());
        let g = WikiGraph::build_with_existence(&scope, &existence, &VaultDirs::default());
        assert!(
            g.orphans(&config.metrics.orphan_exclude, Path::new("wiki"))
                .is_empty()
        );
    }

    #[test]
    fn orphan_exempted_by_external_outbound() {
        // A `wiki/` concept that links out to an existing `daily/` page (and is
        // linked by nothing) is connected to the vault, not an orphan.
        let config = GraphConfig::default();
        let scope = vec![make_page(
            "wiki/concepts/baz",
            &["daily/team-slack/2026-05-22"],
        )];
        let full = vec![
            make_page("wiki/concepts/baz", &["daily/team-slack/2026-05-22"]),
            make_page("daily/team-slack/2026-05-22", &[]),
        ];

        let existence = VaultExistence::from_pages(&full, &VaultDirs::default());
        let g = WikiGraph::build_with_existence(&scope, &existence, &VaultDirs::default());
        assert!(
            g.orphans(&config.metrics.orphan_exclude, Path::new("wiki"))
                .is_empty()
        );
    }

    #[test]
    fn self_only_link_is_orphan() {
        // A page whose only wikilink points at itself is disconnected; the
        // self-reference must not exempt it from orphan status.
        let config = GraphConfig::default();
        let pages = vec![make_page("wiki/lonely", &["lonely"])];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        assert_eq!(
            g.orphans(&config.metrics.orphan_exclude, Path::new("wiki")),
            vec!["wiki/lonely"]
        );
    }

    #[test]
    fn duplicate_slug_does_not_over_exempt_orphan() {
        // Two pages share the filename `dup`; `[[dup]]` resolves to the first.
        // The second, unreferenced, is still an orphan — a flat slug set would
        // have exempted both.
        let config = GraphConfig::default();
        let pages = vec![
            make_page("wiki/a/dup", &[]),
            make_page("wiki/b/dup", &[]),
            make_page("wiki/linker", &["dup"]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        assert_eq!(
            g.orphans(&config.metrics.orphan_exclude, Path::new("wiki")),
            vec!["wiki/b/dup"]
        );
    }

    #[test]
    fn truly_disconnected_page_still_orphan_with_existence() {
        // Existence awareness must not hide a genuinely disconnected page:
        // nothing links it and it links nothing that exists.
        let config = GraphConfig::default();
        let scope = vec![
            make_page("wiki/connected", &["other"]),
            make_page("wiki/other", &[]),
            make_page("wiki/lonely", &[]),
        ];
        let existence = VaultExistence::from_pages(&scope, &VaultDirs::default());
        let g = WikiGraph::build_with_existence(&scope, &existence, &VaultDirs::default());

        assert_eq!(
            g.orphans(&config.metrics.orphan_exclude, Path::new("wiki")),
            vec!["wiki/lonely"]
        );
    }
}
