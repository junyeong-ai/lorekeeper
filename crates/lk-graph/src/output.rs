use serde::Serialize;

use crate::alias::{AliasConflict, AliasConflictKind};
use crate::audit::AuditCandidate;
use crate::backlinks::{BacklinksSyncResult, ConceptUpdate};
use crate::cluster::{ClusterResult, LinkSuggestion};
use crate::concept_lint::{InvalidCategoryConcept, NearDuplicateConcept, UnresolvedConflict};
use crate::export::GraphExport;
use crate::graph::{BrokenLink, HubPageReference};
use crate::merge::MergeResult;

/// Wraps the hub list so `--json` emits a named object (`{"hubs": [...]}`) consistent with
/// the other graph reports, rather than a bare top-level array — the wrapper IS the JSON
/// presentation structure, not an empty pass-through. No `count` field: `hubs` is already a
/// top-N list, so (unlike Orphans/Broken, where the total is the salient number) a count
/// would be redundant.
#[derive(Debug, Serialize)]
pub struct HubsReport {
    pub hubs: Vec<HubPageReference>,
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
    pub renames: Vec<RenameSuggestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct RenameSuggestion {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Serialize)]
pub struct LintReport {
    pub pages: usize,
    pub wikilinks: usize,
    pub components: usize,
    pub hubs: Vec<HubPageReference>,
    pub orphans: Vec<String>,
    pub broken: Vec<BrokenLink>,
    pub index: IndexSyncReport,
    pub invalid_categories: Vec<InvalidCategoryConcept>,
    pub near_duplicate_concepts: Vec<NearDuplicateConcept>,
    pub unresolved_conflicts: Vec<UnresolvedConflict>,
    pub alias_conflicts: Vec<AliasConflict>,
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

pub fn print_export(r: &GraphExport) {
    println!("nodes: {}", r.nodes.len());
    println!("edges: {}", r.edges.len());
    let with_clusters = r.nodes.iter().any(|n| n.community.is_some());
    if with_clusters {
        println!("clusters: included");
    }
    println!("(use `lore graph --json export` for full graph data)");
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

    if !r.invalid_categories.is_empty() {
        println!(
            "\nInvalid concept categories ({}):",
            r.invalid_categories.len()
        );
        for c in &r.invalid_categories {
            println!(
                "  {}  category={}  ({})",
                c.slug,
                c.category,
                c.path.display()
            );
        }
    }

    if !r.near_duplicate_concepts.is_empty() {
        println!(
            "\nNear-duplicate concepts ({}):",
            r.near_duplicate_concepts.len()
        );
        for d in &r.near_duplicate_concepts {
            println!("  {} ~ {}  ({:.2})", d.a, d.b, d.similarity);
        }
    }

    if !r.unresolved_conflicts.is_empty() {
        println!(
            "\nUnresolved concept conflicts ({}):",
            r.unresolved_conflicts.len()
        );
        for c in &r.unresolved_conflicts {
            if c.note.is_empty() {
                println!("  {}  ({})", c.slug, c.path.display());
            } else {
                println!("  {}  {}  ({})", c.slug, c.note, c.path.display());
            }
        }
    }

    if !r.alias_conflicts.is_empty() {
        println!("\nAlias conflicts ({}):", r.alias_conflicts.len());
        for c in &r.alias_conflicts {
            match c.kind {
                AliasConflictKind::Duplicate => println!(
                    "  [[{}]] claimed by {} — resolves to only one",
                    c.alias,
                    c.claimants.join(", ")
                ),
                AliasConflictKind::ShadowsRealPage => println!(
                    "  [[{}]] on {} is inert — a real page already owns that slug",
                    c.alias,
                    c.claimants.join(", ")
                ),
            }
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
            "  {} <-> {} (score: {:.3}, shared neighbors: {})",
            s.a, s.b, s.score, s.shared_neighbors
        );
    }
    println!("{} suggestion(s)", r.count);
}

#[derive(Debug, Serialize)]
pub struct AuditCandidatesReport {
    pub candidates: Vec<AuditCandidate>,
    pub count: usize,
}

pub fn print_audit_candidates(r: &AuditCandidatesReport) {
    println!("=== Concepts due for contradiction audit ===");
    if r.candidates.is_empty() {
        println!("\nNothing to audit.");
        return;
    }
    for c in &r.candidates {
        println!(
            "  {}  ({} source(s), set changed since last audit)  {}",
            c.slug,
            c.source_count,
            c.path.display()
        );
    }
    println!("\n{} concept(s) to audit", r.count);
}

#[derive(Debug, Serialize)]
pub struct BacklinksSyncReport {
    #[serde(flatten)]
    pub sync: BacklinksSyncResult,
    pub changed: usize,
}

pub fn print_merge(result: &MergeResult) {
    let mode = if result.dry_run { " (dry-run)" } else { "" };
    let total: usize = result.rewritten.iter().map(|r| r.links).sum();
    println!(
        "Merge{mode}: [[{}]] → [[{}]] — {total} link(s) across {} page(s)",
        result.from_slug,
        result.into_slug,
        result.rewritten.len()
    );
    for r in &result.rewritten {
        println!("  {} ({} link(s))", r.path.display(), r.links);
    }
    if result.deleted {
        let verb = if result.dry_run {
            "would delete"
        } else {
            "deleted"
        };
        println!("  {verb} concept page: {}", result.from_slug);
    }
    if result.from_authored {
        if result.dry_run {
            println!(
                "  ! '{}' has authored body content a merge would discard — salvage it \
                 into '{}', then re-run with --force.",
                result.from_slug, result.into_slug
            );
        } else {
            println!(
                "  ! '{}' had authored body content; it was discarded (--force). \
                 Confirm '{}' captured anything worth keeping.",
                result.from_slug, result.into_slug
            );
        }
    }
    println!("  next: run `lore graph backlinks-sync` to re-derive sources + source_count");
}

pub fn print_backlinks(r: &BacklinksSyncReport) {
    println!("=== Backlinks sync ===");

    if r.sync.dry_run {
        println!("(dry-run: no files written)");
    }

    if r.sync.updated.is_empty() {
        println!("\nAll {} concept page(s) in sync.", r.sync.unchanged);
        return;
    }

    println!("\nUpdated: {} concept page(s)", r.sync.updated.len());
    for entry in &r.sync.updated {
        println!("  {}{}", entry.path.display(), format_diff(entry));
    }
    println!("Unchanged: {} concept page(s)", r.sync.unchanged);
}

fn format_diff(update: &ConceptUpdate) -> String {
    let mut parts = Vec::new();
    if !update.added.is_empty() {
        parts.push(format!("+{} source(s)", update.added.len()));
    }
    if !update.removed.is_empty() {
        parts.push(format!("-{} source(s)", update.removed.len()));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  ({})", parts.join(", "))
    }
}
