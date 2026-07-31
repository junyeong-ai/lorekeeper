//! public API of `lk-graph` against the same `tests/fixtures/small` vault.

use std::path::{Path, PathBuf};

use lk_core::config::{GraphConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_graph::{cluster, export, graph, index_drift, normalize, output, scan};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/small")
}

/// The analysis view of a vault — what `hubs`/`cluster`/`normalize` read.
fn fixture_pages(root: &Path, config: &GraphConfig) -> Vec<scan::ScannedPage> {
    scan::VaultViews::resolve(root, config, &VaultDirs::default())
        .unwrap()
        .pages
}

fn default_config() -> GraphConfig {
    let mut config = GraphConfig::default();
    config.scope.dirs = vec![std::path::PathBuf::from("wiki")];
    config
}

// --- Build / stats ---

#[test]
fn build_correct_page_count() {
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    // The fixture has 5 markdown files, but `index.md` is a reserved navigation/catalog
    // meta-file — NOT a knowledge node — so the analysis graph has 4 nodes (3 concepts +
    // orphan-page). A catalog page that links every concept must never become a graph node.
    assert_eq!(g.node_count(), 4);
    assert!(g.edge_count() > 0);
    assert!(g.component_count() > 0);
}

#[test]
fn reserved_meta_files_are_excluded_from_the_graph() {
    // `index.md` (a reserved meta-file that links every concept) must not be a node — as a
    // node it would be the top hub and would connect every otherwise-separate community.
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    assert!(
        pages.iter().any(|p| p.id == "wiki/index"),
        "fixture includes index.md in the raw scan"
    );
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    assert!(
        g.node_ids().all(|id| id != "wiki/index"),
        "index.md must be excluded from the analysis graph"
    );
    assert!(
        g.hubs(10, 0).iter().all(|h| h.id != "wiki/index"),
        "index.md must never appear as a hub"
    );
}

// --- Hubs ---

#[test]
fn hubs_returns_results() {
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    let hubs = g.hubs(3, 1);
    assert!(!hubs.is_empty());
}

// --- Orphans ---

#[test]
fn orphans_detected() {
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    let orphans = g.orphans(&config.metrics.orphan_exclude);
    assert!(!orphans.is_empty(), "fixture has orphan-page.md");
}

// --- Broken links ---

/// Link integrity is a whole-vault question, so it takes the page set rather than the graph:
/// the fixture is its own vault here, and the existence universe is built from all of it.
fn fixture_broken_links(pages: &[scan::ScannedPage]) -> Vec<graph::BrokenLink> {
    let dirs = VaultDirs::default();
    graph::broken_links(pages, &scan::VaultExistence::build(pages, &dirs), &dirs)
}

#[test]
fn broken_links_detected() {
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    let broken = fixture_broken_links(&pages);
    assert!(!broken.is_empty(), "fixture has nonexistent-page link");
    assert!(broken.iter().any(|b| b.target.contains("nonexistent")));
}

#[test]
fn broken_json_has_count() {
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    let broken = fixture_broken_links(&pages);
    let report = output::BrokenReport {
        count: broken.len(),
        broken,
    };
    let json = serde_json::to_value(&report).unwrap();
    assert!(json["count"].as_u64().unwrap() >= 1);
    assert!(json["broken"].is_array());
}

// --- Index sync ---

#[test]
fn index_sync_detects_drift() {
    // The catalog's own builder decides which pages belong in it, so the vault is laid out the
    // way it lays one out: a catalogued page absent from `index.md` is drift.
    let tmp = tempfile::tempdir().unwrap();
    let concepts = tmp.path().join("wiki/concepts");
    std::fs::create_dir_all(&concepts).unwrap();
    std::fs::write(
        concepts.join("alpha.md"),
        "---\nid: alpha\ntype: concept\ntitle: \"Alpha\"\n---\n\n# Alpha\n",
    )
    .unwrap();
    std::fs::write(
        concepts.join("beta.md"),
        "---\nid: beta\ntype: concept\ntitle: \"Beta\"\n---\n\n# Beta\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("wiki/index.md"),
        "# Index\n\n- [Alpha](concepts/alpha.md)\n",
    )
    .unwrap();

    let drift = index_drift::diff(tmp.path(), Locale::default(), &VaultDirs::default()).unwrap();
    assert_eq!(drift.missing_from_index, vec!["wiki/concepts/beta"]);
    assert!(!drift.is_in_sync());
}

#[test]
fn index_sync_fix_mutates_and_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let concepts = tmp.path().join("wiki/concepts");
    std::fs::create_dir_all(&concepts).unwrap();
    std::fs::write(
        concepts.join("alpha.md"),
        "---\nid: alpha\ntype: concept\ntitle: \"Alpha\"\n---\n\n# Alpha\n",
    )
    .unwrap();
    std::fs::write(
        concepts.join("beta.md"),
        "---\nid: beta\ntype: concept\ntitle: \"Beta\"\n---\n\n# Beta\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("wiki/index.md"),
        "# Index\n\n- [Alpha](concepts/alpha.md)\n",
    )
    .unwrap();

    let dirs = VaultDirs::default();
    let drift = index_drift::diff(tmp.path(), Locale::default(), &dirs).unwrap();
    assert!(!drift.is_in_sync());

    assert_eq!(index_drift::fix(&drift, tmp.path(), &dirs).unwrap(), 1);
    let content = std::fs::read_to_string(tmp.path().join("wiki/index.md")).unwrap();
    assert!(content.contains("[Beta](concepts/beta.md)"), "{content}");

    let after = index_drift::diff(tmp.path(), Locale::default(), &dirs).unwrap();
    assert!(after.is_in_sync(), "{after:?}");
}

// --- Normalize ---

#[test]
fn normalize_fixture_already_normalized() {
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    let renames = normalize::scan(&pages);
    assert!(renames.is_empty());
}

#[test]
fn normalize_fix_renames_and_rewrites() {
    let tmp = tempfile::tempdir().unwrap();
    let wiki = tmp.path().join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    std::fs::write(wiki.join("Bad_Name.md"), "# Bad Name\n").unwrap();
    std::fs::write(
        wiki.join("other.md"),
        "# Other\n\nSee [Bad](Bad_Name.md).\n",
    )
    .unwrap();

    let config = default_config();
    let pages = fixture_pages(tmp.path(), &config);

    let renames = normalize::scan(&pages);
    assert!(!renames.is_empty());

    normalize::apply(&renames, &pages, tmp.path()).unwrap();

    assert!(!wiki.join("Bad_Name.md").exists());
    assert!(wiki.join("bad-name.md").exists());
    let content = std::fs::read_to_string(wiki.join("other.md")).unwrap();
    assert!(content.contains("[Bad](bad-name.md)"));
}

// --- Cluster ---

#[test]
fn cluster_json_has_communities() {
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    let mut result = cluster::detect_communities(&g, &config);
    cluster::label_communities(&g, &mut result.communities);

    let json = serde_json::to_value(&result).unwrap();
    assert!(json["communities"].is_array());
    assert!(json["modularity"].is_number());
    assert!(json["iterations"].is_u64());

    let first = &json["communities"][0];
    assert!(first["id"].is_u64());
    assert!(first["size"].is_u64());
    assert!(first["members"].is_array());
}

// --- Export ---

#[test]
fn export_json_with_clusters() {
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    let cluster_result = cluster::detect_communities(&g, &config);
    let graph_export = export::export(&g, Some(&cluster_result));

    assert!(!graph_export.nodes.is_empty());
    assert!(!graph_export.edges.is_empty());
    assert!(graph_export.nodes.iter().any(|n| n.community.is_some()));
}

// --- Lint ---

#[test]
fn lint_combined_report() {
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());

    let hubs = g.hubs(10, config.metrics.min_hub_degree);
    let orphans = g.orphans(&config.metrics.orphan_exclude);
    let broken = fixture_broken_links(&pages);
    let drift = index_drift::diff(&root, Locale::default(), &VaultDirs::default()).unwrap();

    let report = output::LintReport {
        pages: g.node_count(),
        links: g.edge_count(),
        components: g.component_count(),
        violations: output::Violations {
            broken,
            index: output::IndexSyncReport {
                stale: false,
                absent: drift.absent,
                missing_from_index: drift.missing_from_index,
                missing_from_disk: drift.missing_from_disk,
                fixed: None,
            },
            invalid_categories: Vec::new(),
            duplicate_concepts: Vec::new(),
            address_collisions: Vec::new(),
            unnormalized: Vec::new(),
        },
        observations: output::Observations {
            hubs,
            orphans,
            unresolved_conflicts: Vec::new(),
        },
    };

    assert!(report.observations.count() > 0);
    // 4 knowledge nodes: index.md (reserved meta-file) is excluded from the graph.
    assert_eq!(report.pages, 4);

    let json = serde_json::to_value(&report).unwrap();
    assert!(json["pages"].is_u64());
    assert!(json["observations"]["orphans"].is_array());
    assert!(json["violations"]["broken"].is_array());
    assert!(json["violations"]["invalid_categories"].is_array());
    assert!(json["violations"]["index"]["missing_from_index"].is_array());
}

// --- Suggest links ---

#[test]
fn suggest_links_from_fixture() {
    let root = fixture_root();
    let config = default_config();
    let pages = fixture_pages(&root, &config);
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    let clusters = cluster::detect_communities(&g, &config);
    let result = cluster::suggest_links(&g, &clusters, 1);
    // Just verify it's well-formed — the fixture may or may not produce suggestions.
    for s in &result.pairs {
        assert!(!s.a.is_empty());
        assert!(!s.b.is_empty());
    }
}

// --- Missing directory exits with error ---

#[test]
fn scan_missing_dir_errors() {
    let result = scan::scan_vault(Path::new("/nonexistent/path"), false);
    assert!(result.is_err());
}
