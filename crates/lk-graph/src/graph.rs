use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use lk_core::config::VaultDirs;

use crate::scan::{ScannedPage, VaultExistence};

pub struct WikiGraph {
    graph: DiGraph<NodeData, ()>,
    id_to_node: HashMap<String, NodeIndex>,
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

/// Every link in `pages` whose destination is not a page in the vault, deduped and ordered by
/// (source, target).
///
/// Not a graph property: a broken link involves no node, no edge and no community, only one
/// page's destination and the set of pages on disk. Taking the page set as a parameter is what
/// keeps the two questions apart — `graph.scope.dirs` chooses which subgraph
/// `hubs`/`cluster`/`suggest-links` reason about, and has no say in where a link may be written.
/// A link is broken wherever it appears, and `queue apply` writes concept links on daily pages.
///
/// `existence` must be built from a VAULT-ROOT scan, which is what integrity commands do:
/// "absent from the universe" means absent from the vault only if the universe looked
/// everywhere. A vault holds user-authored folders outside the four page dirs, and links may
/// point at them.
///
/// Reserved meta-pages (`lk_core::vault_path::RESERVED_WIKI_FILES`) are skipped as SOURCES —
/// they catalog every page they can see and are re-derived WHOLE, so a stale link in one is
/// repaired by regenerating it, and `wiki refresh` runs before `graph lint` in the scheduled
/// pipeline. They remain valid DESTINATIONS: they are files, and a page linking the catalog is
/// linking something real.
pub fn broken_links(
    pages: &[ScannedPage],
    existence: &VaultExistence,
    dirs: &VaultDirs,
) -> Vec<BrokenLink> {
    let is_reserved = crate::scan::reserved_page_predicate(Path::new(&dirs.wiki));
    let mut broken: Vec<BrokenLink> = pages
        .iter()
        .filter(|page| !is_reserved(page.id.as_str()))
        .flat_map(|page| {
            page.outgoing
                .iter()
                .filter(|link| !existence.is_resolvable(&link.dest))
                .map(|link| BrokenLink {
                    source: page.id.clone(),
                    target: link.dest.clone(),
                })
        })
        .collect();

    broken.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));
    broken.dedup_by(|a, b| a.source == b.source && a.target == b.target);
    broken
}

#[derive(Debug, Clone, Serialize)]
pub struct HubPageReference {
    pub id: String,
    pub title: String,
    pub degree: usize,
    pub outgoing: usize,
    pub incoming: usize,
}

impl WikiGraph {
    /// Build the analysis graph over `pages`, treating that set as the whole
    /// universe: a page with no edges is an orphan. The right model when the scope
    /// *is* the vault (and the convenient default for tests); for a narrowed scope
    /// whose links reach pages outside it, use [`Self::build_with_existence`].
    pub fn build(pages: &[ScannedPage], dirs: &VaultDirs) -> Self {
        Self::build_with_existence(pages, &VaultExistence::build(pages, dirs), dirs)
    }

    /// Build the analysis graph over `pages` (the scope) while resolving
    /// orphan detection against the full-vault `existence` universe.
    ///
    /// The graph itself — nodes and edges — stays scope-internal, so `hubs`,
    /// `cluster`, and `suggest_links` are unaffected. A scope page is exempt from
    /// `orphans` when it links to, or is linked from, any page in the vault
    /// (tracked in `cross_scope_connected`). Broken links are not a graph
    /// property at all — see [`broken_links`].
    pub fn build_with_existence(
        pages: &[ScannedPage],
        existence: &VaultExistence,
        dirs: &VaultDirs,
    ) -> Self {
        let mut graph = DiGraph::new();
        let mut id_to_node = HashMap::with_capacity(pages.len());

        // Navigation/catalog meta-files (index.md, log.md, map.md, AGENTS.md) are generated
        // artifacts, not knowledge nodes — keep them out of the analysis graph entirely
        // (nodes AND edges). index.md/map.md link every member they catalog, so as nodes
        // they would be spurious mega-hubs, merge otherwise-separate communities, and
        // (via inbound links) mask real orphans.
        let is_reserved = crate::scan::reserved_page_predicate(Path::new(&dirs.wiki));

        for page in pages {
            if is_reserved(page.id.as_str()) {
                continue;
            }
            let node = graph.add_node(NodeData {
                id: page.id.clone(),
                title: page.title.clone(),
            });
            id_to_node.insert(page.id.clone(), node);
        }

        let mut cross_scope_connected = HashSet::new();

        for page in pages {
            if is_reserved(page.id.as_str()) {
                continue;
            }
            let source_idx = id_to_node[&page.id];
            for target in page
                .outgoing
                .iter()
                .filter_map(|link| existence.reached(link))
            {
                // Each link already carries the resolved page id (scan resolves every
                // destination against its page's location), so an edge is a plain lookup —
                // asked of `reached`, so a link whose address is not a file is not an edge.
                if let Some(&target_idx) = id_to_node.get(target) {
                    // `update_edge`, not `add_edge`: two spellings of one address are two
                    // links but one connection, and a parallel edge would double the degree.
                    if source_idx != target_idx {
                        graph.update_edge(source_idx, target_idx, ());
                    }
                } else if existence.is_knowledge(target) {
                    // Resolves to a knowledge page outside the analysis scope: a vault-wide
                    // outbound connection for orphan purposes. No edge — the target node is not
                    // in the scope graph. A generated catalog is deliberately not knowledge
                    // here: `index.md` lists every page, so counting a link to it as a
                    // connection would exempt the very pages orphan detection looks for.
                    cross_scope_connected.insert(page.id.clone());
                }
            }

            // Inbound from anywhere in the vault: this page is the resolved
            // target of a link from another page (possibly out of scope), even
            // if nothing in scope links it.
            if existence.is_linked(&page.id) {
                cross_scope_connected.insert(page.id.clone());
            }
        }

        WikiGraph {
            graph,
            id_to_node,
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

    pub fn hubs(&self, top: usize, min_degree: usize) -> Vec<HubPageReference> {
        let mut entries: Vec<HubPageReference> = self
            .id_to_node
            .iter()
            .map(|(id, &idx)| {
                let out = self.graph.edges_directed(idx, Direction::Outgoing).count();
                let inc = self.graph.edges_directed(idx, Direction::Incoming).count();
                HubPageReference {
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

    pub fn orphans(&self, orphan_exclude: &[String]) -> Vec<String> {
        // Reserved meta-files are excluded from the graph at construction, so they can never
        // be orphan candidates here — the only exclusions left are the user's configured ones.
        let exclude: HashSet<String> = orphan_exclude.iter().cloned().collect();

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
    /// `suggest_links` to find shared neighbors and score candidate pairs by their
    /// Adamic-Adar index (which weights each shared neighbor by 1/ln of its own degree).
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
    use crate::scan::Link;
    use crate::scan::{ScannedPage, VaultExistence};
    use lk_core::config::GraphConfig;
    use std::path::PathBuf;

    fn build_page(id: &str, outgoing: &[&str]) -> ScannedPage {
        let name = id.rsplit('/').next().unwrap_or(id);
        ScannedPage {
            id: id.to_owned(),
            path: PathBuf::from(format!("{id}.md")),
            title: name.to_owned(),
            outgoing: outgoing
                .iter()
                .map(|s| Link::to(&format!("{s}.md")))
                .collect(),
        }
    }

    #[test]
    fn basic_graph_construction() {
        let pages = vec![
            build_page("wiki/alpha", &["wiki/beta"]),
            build_page("wiki/beta", &["wiki/alpha"]),
            build_page("wiki/gamma", &[]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        assert_eq!(g.node_count(), 3);
        assert_eq!(g.edge_count(), 2);
        assert_eq!(g.component_count(), 2);
    }

    fn broken(pages: &[ScannedPage]) -> Vec<BrokenLink> {
        let dirs = VaultDirs::default();
        broken_links(pages, &VaultExistence::build(pages, &dirs), &dirs)
    }

    #[test]
    fn broken_links_detected() {
        let pages = vec![
            build_page("wiki/alpha", &["wiki/nonexistent"]),
            build_page("wiki/beta", &[]),
        ];
        let found = broken(&pages);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, "wiki/alpha");
        assert_eq!(found[0].target, "wiki/nonexistent.md");
    }

    #[test]
    fn a_link_written_outside_the_analysis_scope_is_still_checked() {
        // `graph.scope.dirs` chooses the analysis subgraph and nothing else, so a link written
        // on a daily page — where `queue apply` writes concept links — is checked like any other.
        let pages = vec![
            build_page("daily/ai-news/2026-06-12", &["wiki/concepts/gone"]),
            build_page("wiki/concepts/here", &[]),
        ];
        let found = broken(&pages);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, "daily/ai-news/2026-06-12");
        assert_eq!(found[0].target, "wiki/concepts/gone.md");
    }

    /// A vault holds user-authored folders outside the four page dirs, and a page there is a
    /// page: the universe covers the whole vault, so a link to one resolves.
    #[test]
    fn a_page_under_a_users_own_folder_resolves_like_any_other() {
        let dirs = VaultDirs::default();
        let pages = vec![
            build_page(
                "daily/notes/2026-05-23",
                &["notes/hand-written", "wiki/concepts/gone"],
            ),
            // An ordinary Obsidian note, under none of the four page dirs.
            build_page("notes/hand-written", &[]),
        ];
        let found = broken_links(&pages, &VaultExistence::build(&pages, &dirs), &dirs);

        assert_eq!(found.len(), 1, "the note exists, so only `gone` is broken");
        assert_eq!(found[0].target, "wiki/concepts/gone.md");
    }

    /// A destination that escapes the vault root is kept as written rather than resolved to an
    /// id, and no vault page can be addressed by it — so it is nowhere, and reported.
    #[test]
    fn a_destination_that_escapes_the_vault_root_is_always_reported() {
        let dirs = VaultDirs::default();
        let pages = vec![build_page(
            "wiki/concepts/a",
            &["../../../outside.md", "/etc/passwd.md"],
        )];
        let existence = VaultExistence::build(&pages, &dirs);
        let found = broken_links(&pages, &existence, &dirs);

        assert_eq!(
            found.len(),
            2,
            "an escaping destination is nowhere, not unscanned: {found:?}"
        );
    }

    /// The generated catalogs are files, so a page linking one is linking something real —
    /// their exclusion from the analysis graph is about NODES, not about existing.
    #[test]
    fn a_link_to_the_generated_catalog_resolves() {
        let pages = vec![
            build_page("daily/notes/2026-05-23", &["wiki/index"]),
            build_page("wiki/index", &[]),
        ];
        assert!(broken(&pages).is_empty());
    }

    /// …but reaching the catalog is not reaching anything, for orphan purposes: `index.md`
    /// lists every page it catalogs, so counting a link to it as connectivity would exempt the
    /// very pages orphan detection exists to find.
    #[test]
    fn linking_only_the_catalog_does_not_exempt_a_page_from_being_an_orphan() {
        let dirs = VaultDirs::default();
        let scope = vec![build_page("wiki/concepts/lonely", &["wiki/index"])];
        let full = vec![
            build_page("wiki/concepts/lonely", &["wiki/index"]),
            build_page("wiki/index", &["wiki/concepts/lonely"]),
        ];
        let existence = VaultExistence::build(&full, &dirs);
        let g = WikiGraph::build_with_existence(&scope, &existence, &dirs);

        assert_eq!(g.orphans(&[]), vec!["wiki/concepts/lonely"]);
    }

    #[test]
    fn a_catalog_page_is_never_a_broken_link_source() {
        // `index.md` lists every page it can see and is re-derived from the vault, so an entry
        // for a page that is gone is index drift, reported by `index-sync` rather than twice.
        let pages = vec![
            build_page("wiki/index", &["wiki/concepts/gone"]),
            build_page("wiki/concepts/here", &[]),
        ];
        assert!(broken(&pages).is_empty());
    }

    #[test]
    fn hubs_sorted_by_degree() {
        let pages = vec![
            build_page("wiki/hub", &["wiki/a", "wiki/b", "wiki/c"]),
            build_page("wiki/a", &["wiki/hub"]),
            build_page("wiki/b", &["wiki/hub"]),
            build_page("wiki/c", &[]),
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
            build_page("wiki/connected", &["wiki/other"]),
            build_page("wiki/other", &[]),
            build_page("wiki/orphan", &[]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        let orphans = g.orphans(&config.metrics.orphan_exclude);
        assert_eq!(orphans, vec!["wiki/orphan"]);
    }

    #[test]
    fn orphan_exclude_respected() {
        let mut config = GraphConfig::default();
        config.metrics.orphan_exclude = vec!["wiki/orphan".to_owned()];

        let pages = vec![
            build_page("wiki/connected", &["wiki/other"]),
            build_page("wiki/other", &[]),
            build_page("wiki/orphan", &[]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        let orphans = g.orphans(&config.metrics.orphan_exclude);
        assert!(orphans.is_empty());
    }

    #[test]
    fn self_links_excluded() {
        let pages = vec![build_page("wiki/self", &["wiki/self"])];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn same_stem_pages_stay_distinct() {
        // Two pages share the filename stem `alpha`; each is addressed only by its
        // own path, so a link to `docs/alpha` never lands on `wiki/alpha`.
        let pages = vec![
            build_page("wiki/alpha", &[]),
            build_page("docs/alpha", &[]),
            build_page("wiki/linker", &["docs/alpha"]),
        ];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        assert_eq!(g.node_count(), 3);
        let edges: Vec<(String, String)> = g
            .edge_pairs()
            .map(|(s, t)| (g.node_id(s).to_owned(), g.node_id(t).to_owned()))
            .collect();
        assert_eq!(
            edges,
            vec![("wiki/linker".to_owned(), "docs/alpha".to_owned())]
        );
        assert!(broken(&pages).is_empty());
    }

    #[test]
    fn a_link_that_leaves_the_analysis_scope_resolves_against_the_whole_vault() {
        // A `wiki/` concept links a `daily/` page, which is outside the analysis scope but on
        // disk. It is the existence universe — not the scope — that decides.
        let full = vec![
            build_page("wiki/concepts/foo", &["daily/team-slack/2026-05-22"]),
            build_page("daily/team-slack/2026-05-22", &[]),
        ];
        assert!(broken(&full).is_empty());

        let dirs = VaultDirs::default();
        let narrow = VaultExistence::build(&full[..1], &dirs);
        assert_eq!(broken_links(&full[..1], &narrow, &dirs).len(), 1);
    }

    #[test]
    fn orphan_exempted_by_external_inbound() {
        // A `wiki/` concept with no in-scope edges, linked only from a `daily/`
        // page (out of scope), is not a true orphan.
        let config = GraphConfig::default();
        let scope = vec![build_page("wiki/concepts/bar", &[])];
        let full = vec![
            build_page("wiki/concepts/bar", &[]),
            build_page("daily/team-slack/2026-05-22", &["wiki/concepts/bar"]),
        ];

        let legacy = WikiGraph::build(&scope, &VaultDirs::default());
        assert_eq!(
            legacy.orphans(&config.metrics.orphan_exclude),
            vec!["wiki/concepts/bar"]
        );

        let existence = VaultExistence::build(&full, &VaultDirs::default());
        let g = WikiGraph::build_with_existence(&scope, &existence, &VaultDirs::default());
        assert!(g.orphans(&config.metrics.orphan_exclude).is_empty());
    }

    #[test]
    fn orphan_exempted_by_external_outbound() {
        // A `wiki/` concept that links out to an existing `daily/` page (and is
        // linked by nothing) is connected to the vault, not an orphan.
        let config = GraphConfig::default();
        let scope = vec![build_page(
            "wiki/concepts/baz",
            &["daily/team-slack/2026-05-22"],
        )];
        let full = vec![
            build_page("wiki/concepts/baz", &["daily/team-slack/2026-05-22"]),
            build_page("daily/team-slack/2026-05-22", &[]),
        ];

        let existence = VaultExistence::build(&full, &VaultDirs::default());
        let g = WikiGraph::build_with_existence(&scope, &existence, &VaultDirs::default());
        assert!(g.orphans(&config.metrics.orphan_exclude).is_empty());
    }

    #[test]
    fn self_only_link_is_orphan() {
        // A page whose only link points at itself is disconnected; the
        // self-reference must not exempt it from orphan status.
        let config = GraphConfig::default();
        let pages = vec![build_page("wiki/lonely", &["wiki/lonely"])];
        let g = WikiGraph::build(&pages, &VaultDirs::default());

        assert_eq!(
            g.orphans(&config.metrics.orphan_exclude),
            vec!["wiki/lonely"]
        );
    }

    #[test]
    fn truly_disconnected_page_still_orphan_with_existence() {
        // Existence awareness must not hide a genuinely disconnected page:
        // nothing links it and it links nothing that exists.
        let config = GraphConfig::default();
        let scope = vec![
            build_page("wiki/connected", &["wiki/other"]),
            build_page("wiki/other", &[]),
            build_page("wiki/lonely", &[]),
        ];
        let existence = VaultExistence::build(&scope, &VaultDirs::default());
        let g = WikiGraph::build_with_existence(&scope, &existence, &VaultDirs::default());

        assert_eq!(
            g.orphans(&config.metrics.orphan_exclude),
            vec!["wiki/lonely"]
        );
    }
}
