use serde::Serialize;

use crate::cluster::{ClusterResult, LinkSuggestion};
use crate::export::GraphExport;
use crate::graph::{BrokenLink, HubEntry};
use crate::stale::{Category, StalePage};

#[derive(Debug, Serialize)]
pub struct BuildReport {
    pub pages: usize,
    pub wikilinks: usize,
    pub components: usize,
}

#[derive(Debug, Serialize)]
pub struct HubsReport {
    pub hubs: Vec<HubEntry>,
}

#[derive(Debug, Serialize)]
pub struct OrphansReport {
    pub orphans: Vec<String>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct BrokenReport {
    pub broken: Vec<BrokenLink>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct IndexSyncReport {
    pub missing_from_index: Vec<String>,
    pub missing_from_disk: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct NormalizeReport {
    pub renames: Vec<RenameEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct RenameEntry {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Serialize)]
pub struct ExportReport {
    #[serde(flatten)]
    pub graph: GraphExport,
}

#[derive(Debug, Serialize)]
pub struct LintReport {
    pub pages: usize,
    pub wikilinks: usize,
    pub components: usize,
    pub hubs: Vec<HubEntry>,
    pub orphans: Vec<String>,
    pub broken: Vec<BrokenLink>,
    pub index: IndexSyncReport,
    pub findings: usize,
}

#[derive(Debug, Serialize)]
pub struct SuggestLinksReport {
    pub pairs: Vec<LinkSuggestion>,
    pub count: usize,
}

#[derive(Serialize)]
struct Envelope<T: Serialize> {
    ok: bool,
    data: T,
}

pub fn print_json<T: Serialize>(data: &T) -> Result<(), String> {
    let envelope = Envelope { ok: true, data };
    println!(
        "{}",
        serde_json::to_string_pretty(&envelope).map_err(|e| e.to_string())?
    );
    Ok(())
}

pub fn print_build(r: &BuildReport) {
    println!("pages:      {}", r.pages);
    println!("wikilinks:  {}", r.wikilinks);
    println!("components: {}", r.components);
}

pub fn print_hubs(r: &HubsReport) {
    if r.hubs.is_empty() {
        println!("(no hubs found)");
        return;
    }
    for hub in &r.hubs {
        println!(
            "{:>4} links  {} (in:{} out:{})",
            hub.degree, hub.id, hub.incoming, hub.outgoing
        );
    }
}

pub fn print_orphans(r: &OrphansReport) {
    for id in &r.orphans {
        println!("  {id}");
    }
    println!("{} orphan(s)", r.count);
}

pub fn print_broken(r: &BrokenReport) {
    for link in &r.broken {
        println!("  {} -> {} (not found)", link.source, link.target);
    }
    println!("{} broken link(s)", r.count);
}

pub fn print_index_sync(r: &IndexSyncReport) {
    if r.missing_from_index.is_empty() && r.missing_from_disk.is_empty() {
        println!("index.md is in sync");
        return;
    }
    for p in &r.missing_from_index {
        println!("  +index  {p}");
    }
    for p in &r.missing_from_disk {
        println!("  -disk   {p}");
    }
    if let Some(n) = r.fixed {
        println!("{n} page(s) added to index.md");
    }
}

pub fn print_normalize(r: &NormalizeReport) {
    if r.renames.is_empty() {
        println!("all slugs normalized");
        return;
    }
    for entry in &r.renames {
        if r.applied.is_some() {
            println!("  renamed: {} -> {}", entry.from, entry.to);
        } else {
            println!("  would rename: {} -> {}", entry.from, entry.to);
        }
    }
    if let Some(n) = r.applied {
        println!("{n} file(s) renamed");
    }
}

pub fn print_cluster(r: &ClusterResult) {
    if r.communities.is_empty() {
        println!("(no communities found)");
        return;
    }
    for community in &r.communities {
        let label = community
            .label
            .as_deref()
            .map(|l| format!(" [{l}]"))
            .unwrap_or_default();
        println!("  #{:<3} size={:<4}{label}", community.id, community.size);
    }
    println!(
        "{} community(ies), modularity={:.4}, iterations={}",
        r.communities.len(),
        r.modularity,
        r.iterations
    );
}

pub fn print_export(r: &ExportReport) {
    println!("nodes: {}", r.graph.nodes.len());
    println!("edges: {}", r.graph.edges.len());
    let with_clusters = r.graph.nodes.iter().any(|n| n.community.is_some());
    if with_clusters {
        println!("clusters: included");
    }
    println!("(use --json for full graph data)");
}

pub fn print_lint(r: &LintReport) {
    println!("=== Lint Report ===");
    println!(
        "pages: {}, links: {}, components: {}",
        r.pages, r.wikilinks, r.components
    );

    if !r.hubs.is_empty() {
        println!("\nHub pages:");
        for hub in &r.hubs {
            println!("  {:>4} links  {}", hub.degree, hub.id);
        }
    }

    if !r.orphans.is_empty() {
        println!("\nOrphans ({}):", r.orphans.len());
        for id in &r.orphans {
            println!("  {id}");
        }
    }

    if !r.broken.is_empty() {
        println!("\nBroken links ({}):", r.broken.len());
        for link in &r.broken {
            println!("  {} -> {}", link.source, link.target);
        }
    }

    if !r.index.missing_from_index.is_empty() || !r.index.missing_from_disk.is_empty() {
        println!("\nIndex drift:");
        for p in &r.index.missing_from_index {
            println!("  +index  {p}");
        }
        for p in &r.index.missing_from_disk {
            println!("  -disk   {p}");
        }
    }

    if r.findings == 0 {
        println!("\nNo issues found");
    } else {
        println!("\n{} finding(s)", r.findings);
    }
}

pub fn print_suggest_links(r: &SuggestLinksReport) {
    if r.pairs.is_empty() {
        println!("(no link suggestions)");
        return;
    }
    for s in &r.pairs {
        println!(
            "  {} <-> {} (shared neighbors: {})",
            s.a, s.b, s.shared_neighbors
        );
    }
    println!("{} suggestion(s)", r.count);
}

#[derive(Debug, Serialize)]
pub struct StaleReport {
    pub threshold_days: u32,
    pub stale: Vec<StalePage>,
    pub count: usize,
}

pub fn print_stale(r: &StaleReport) {
    println!("=== Stale pages (>= {} days) ===", r.threshold_days);

    if r.stale.is_empty() {
        println!("\nNo stale pages.");
        return;
    }

    // Bucket by category, preserving the input ordering (oldest first) within
    // each bucket. `Category` derives `Ord`, so the outer iteration is also
    // deterministic.
    let mut buckets: std::collections::BTreeMap<Category, Vec<&StalePage>> =
        std::collections::BTreeMap::new();
    for page in &r.stale {
        buckets.entry(page.category).or_default().push(page);
    }

    for (category, entries) in &buckets {
        println!("\n{} ({}):", category.label(), entries.len());
        for entry in entries {
            println!(
                "  {:>4} days  {}  (updated: {})",
                entry.days_old,
                entry.path.display(),
                entry.updated
            );
        }
    }

    println!("\nTotal: {} stale page(s)", r.count);
}
