//! public API of `lk-graph` against the same `tests/fixtures/small` vault.

use std::path::{Path, PathBuf};

use lk_core::config::{GraphConfig, VaultDirs};
use lk_graph::{cluster, export, graph, index, normalize, output, scan};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/small")
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
    let pages = scan::scan_vault(&root, &config).unwrap();
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    assert_eq!(g.node_count(), 5);
    assert!(g.edge_count() > 0);
    assert!(g.component_count() > 0);
}

// --- Hubs ---

#[test]
fn hubs_returns_results() {
    let root = fixture_root();
    let config = default_config();
    let pages = scan::scan_vault(&root, &config).unwrap();
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    let hubs = g.hubs(3, 1);
    assert!(!hubs.is_empty());
}

// --- Orphans ---

#[test]
fn orphans_detected() {
    let root = fixture_root();
    let config = default_config();
    let pages = scan::scan_vault(&root, &config).unwrap();
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    let orphans = g.orphans(&config.graph.orphan_exclude, Path::new("wiki"));
    assert!(!orphans.is_empty(), "fixture has orphan-page.md");
}

// --- Broken links ---

#[test]
fn broken_links_detected() {
    let root = fixture_root();
    let config = default_config();
    let pages = scan::scan_vault(&root, &config).unwrap();
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    let broken = g.broken_links();
    assert!(!broken.is_empty(), "fixture has nonexistent-page link");
    assert!(broken.iter().any(|b| b.target.contains("nonexistent")));
}

#[test]
fn broken_json_has_count() {
    let root = fixture_root();
    let config = default_config();
    let pages = scan::scan_vault(&root, &config).unwrap();
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    let report = output::BrokenReport {
        broken: g.broken_links().to_vec(),
        count: g.broken_links().len(),
    };
    let json = serde_json::to_value(&report).unwrap();
    assert!(json["count"].as_u64().unwrap() >= 1);
    assert!(json["broken"].is_array());
}

// --- Index sync ---

#[test]
fn index_sync_detects_drift() {
    let root = fixture_root();
    let config = default_config();
    let pages = scan::scan_vault(&root, &config).unwrap();
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());
    let existence = scan::VaultExistence::from_pages(&pages, &VaultDirs::default());
    let drift = index::diff(&g, &existence, &root, Path::new("wiki"), &[]).unwrap();
    // concept-c and orphan-page are missing from index.md
    assert!(!drift.is_in_sync());
}

#[test]
fn index_sync_fix_mutates_and_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let wiki = tmp.path().join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    std::fs::write(wiki.join("index.md"), "# Index\n\n- [[alpha]]\n").unwrap();
    std::fs::write(wiki.join("alpha.md"), "# Alpha\n").unwrap();
    std::fs::write(wiki.join("beta.md"), "# Beta\n").unwrap();

    let config = default_config();
    let pages = scan::scan_vault(tmp.path(), &config).unwrap();
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());

    // Before fix.
    let existence = scan::VaultExistence::from_pages(&pages, &VaultDirs::default());
    let drift = index::diff(&g, &existence, tmp.path(), Path::new("wiki"), &[]).unwrap();
    assert!(!drift.is_in_sync());

    // Fix.
    let added = index::fix(&drift, tmp.path(), Path::new("wiki")).unwrap();
    assert_eq!(added, 1);
    let content = std::fs::read_to_string(wiki.join("index.md")).unwrap();
    assert!(content.contains("[[wiki/beta]]"));

    // After fix, re-scan to pick up potentially changed pages.
    let pages2 = scan::scan_vault(tmp.path(), &config).unwrap();
    let g2 = graph::WikiGraph::build(&pages2, &VaultDirs::default());
    let existence2 = scan::VaultExistence::from_pages(&pages2, &VaultDirs::default());
    let drift2 = index::diff(&g2, &existence2, tmp.path(), Path::new("wiki"), &[]).unwrap();
    assert!(drift2.is_in_sync());
}

// --- Normalize ---

#[test]
fn normalize_fixture_already_normalized() {
    let root = fixture_root();
    let config = default_config();
    let pages = scan::scan_vault(&root, &config).unwrap();
    let renames = normalize::scan(&pages);
    assert!(renames.is_empty());
}

#[test]
fn normalize_fix_renames_and_rewrites() {
    let tmp = tempfile::tempdir().unwrap();
    let wiki = tmp.path().join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    std::fs::write(wiki.join("Bad_Name.md"), "# Bad Name\n").unwrap();
    std::fs::write(wiki.join("other.md"), "# Other\n\nSee [[Bad_Name]].\n").unwrap();

    let config = default_config();
    let pages = scan::scan_vault(tmp.path(), &config).unwrap();

    let renames = normalize::scan(&pages);
    assert!(!renames.is_empty());

    normalize::apply(&renames, &pages, tmp.path()).unwrap();

    assert!(!wiki.join("Bad_Name.md").exists());
    assert!(wiki.join("bad-name.md").exists());
    let content = std::fs::read_to_string(wiki.join("other.md")).unwrap();
    assert!(content.contains("[[bad-name]]"));
}

// --- Cluster ---

#[test]
fn cluster_json_has_communities() {
    let root = fixture_root();
    let config = default_config();
    let pages = scan::scan_vault(&root, &config).unwrap();
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
    let pages = scan::scan_vault(&root, &config).unwrap();
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
    let pages = scan::scan_vault(&root, &config).unwrap();
    let g = graph::WikiGraph::build(&pages, &VaultDirs::default());

    let hubs = g.hubs(10, config.graph.min_hub_degree);
    let orphans = g.orphans(&config.graph.orphan_exclude, Path::new("wiki"));
    let broken = g.broken_links().to_vec();
    let existence = scan::VaultExistence::from_pages(&pages, &VaultDirs::default());
    let drift = index::diff(&g, &existence, &root, Path::new("wiki"), &[]).unwrap();

    let findings = orphans.len()
        + broken.len()
        + drift.missing_from_index.len()
        + drift.missing_from_disk.len();

    let report = output::LintReport {
        pages: g.node_count(),
        wikilinks: g.edge_count(),
        components: g.component_count(),
        hubs,
        orphans,
        broken,
        index: output::IndexSyncReport {
            missing_from_index: drift.missing_from_index,
            missing_from_disk: drift.missing_from_disk,
            fixed: None,
        },
        invalid_categories: Vec::new(),
        near_duplicate_concepts: Vec::new(),
        unresolved_conflicts: Vec::new(),
        alias_conflicts: Vec::new(),
        findings,
    };

    assert!(report.findings > 0);
    assert_eq!(report.pages, 5);

    let json = serde_json::to_value(&report).unwrap();
    assert!(json["pages"].is_u64());
    assert!(json["orphans"].is_array());
    assert!(json["broken"].is_array());
    assert!(json["findings"].as_u64().unwrap() > 0);
    assert!(json["invalid_categories"].is_array());
}

// --- Suggest links ---

#[test]
fn suggest_links_from_fixture() {
    let root = fixture_root();
    let config = default_config();
    let pages = scan::scan_vault(&root, &config).unwrap();
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
fn missing_directory_errors() {
    let config = default_config();
    let result = scan::scan_vault(Path::new("/nonexistent/path"), &config);
    assert!(result.is_err());
}
