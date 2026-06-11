use std::path::{Path, PathBuf};

use lk_core::config::{ConceptCategory, GraphConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_graph::{
    alias, audit, backlinks, cache, cluster, concept_lint, export, graph, index_drift, merge,
    normalize, output, scan,
};

use super::GlobalOptions;

struct ResolvedConfig {
    root: PathBuf,
    graph: GraphConfig,
    locale: Locale,
    vault_dirs: VaultDirs,
    concept_categories: Vec<ConceptCategory>,
}

#[derive(clap::Subcommand)]
pub enum GraphCmd {
    /// Run all graph checks in one pass
    Lint,
    /// Show top hub pages by wikilink degree
    Hubs {
        /// Number of top hubs to display
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    /// Find orphan pages with zero links
    Orphans,
    /// Find broken wikilinks pointing to non-existent pages
    Broken,
    /// Detect topic communities via Louvain modularity optimization
    Cluster {
        /// Include the top-degree page id as label per community
        #[arg(long)]
        label: bool,
        /// Drop communities smaller than this size (overrides config)
        #[arg(long)]
        min_size: Option<usize>,
    },
    /// Export the full node-link graph as JSON
    Export {
        /// Include community assignments per node
        #[arg(long)]
        with_clusters: bool,
    },
    /// Check index.md sync with disk pages
    IndexSync {
        /// Auto-fix: add missing pages to index.md
        #[arg(long)]
        fix: bool,
    },
    /// Check slug normalization and optionally rename
    Normalize {
        /// Apply renames and update wikilinks
        #[arg(long)]
        fix: bool,
    },
    /// Suggest wikilinks between co-clustered pages that are not yet linked
    SuggestLinks {
        /// Minimum community size for suggestions (overrides config)
        #[arg(long)]
        min_community_size: Option<usize>,
    },
    /// List concept pages due for a contradiction (re-)audit: multiply-cited, with a
    /// source set that changed since the last `/lore-wiki audit` (tracked by the
    /// `audited_sources_hash` frontmatter marker)
    AuditCandidates,
    /// Mark a concept as audited — record its current source set so it leaves the
    /// `audit-candidates` worklist until its sources change again
    AuditMark {
        /// Concept slug to mark as audited
        slug: String,
    },
    /// Rewrite each concept page's `## <Sources>` section from the graph
    BacklinksSync {
        /// Report what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Merge a duplicate concept into a canonical one: rewrite every wikilink from
    /// `<from>` to `<into>`, then delete the `<from>` page. Run `backlinks-sync`
    /// afterward to re-derive the merged `## Sources` + `source_count`.
    Merge {
        /// Slug of the duplicate concept to fold in (its page is deleted)
        from: String,
        /// Slug of the canonical concept to keep
        into: String,
        /// Report what would change without writing or deleting
        #[arg(long)]
        dry_run: bool,
        /// Proceed even when `<from>` has authored body content that the merge would
        /// discard (default: abort so you can salvage the prose into `<into>` first)
        #[arg(long)]
        force: bool,
    },
}

/// Returns exit code: 0 = ok/no findings, 1 = findings, 2 = runtime error.
pub fn run(
    opts: &GlobalOptions,
    cmd: GraphCmd,
    json: bool,
    root_override: Option<PathBuf>,
    incremental: bool,
) -> i32 {
    match run_inner(opts, cmd, json, root_override, incremental) {
        Ok(has_findings) => {
            if has_findings {
                1
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            2
        }
    }
}

fn run_inner(
    opts: &GlobalOptions,
    cmd: GraphCmd,
    json: bool,
    root_override: Option<PathBuf>,
    incremental: bool,
) -> Result<bool, String> {
    // `backlinks-sync` and the audit/merge commands need locale from the full config —
    // dispatch them up front before paying the graph-build cost.
    match cmd {
        GraphCmd::AuditCandidates => {
            return run_audit_candidates(opts, root_override, json);
        }
        GraphCmd::AuditMark { ref slug } => {
            return run_audit_mark(opts, root_override, json, slug);
        }
        GraphCmd::BacklinksSync { dry_run } => {
            return run_backlinks(opts, root_override, json, dry_run, incremental);
        }
        GraphCmd::Merge {
            ref from,
            ref into,
            dry_run,
            force,
        } => {
            return run_merge(opts, root_override, json, from, into, dry_run, force);
        }
        _ => {}
    }

    let mut rc = resolve_config_full(opts, root_override)?;

    // The SINGLE definition of what this command reads — the `--incremental` cache
    // watches and rebuilds exactly this set, so a change anywhere it reads (e.g. a
    // `<daily>/` page that flips an orphan/broken-link result) is never missed.
    let integrity = matches!(
        cmd,
        GraphCmd::Lint | GraphCmd::Broken | GraphCmd::Orphans | GraphCmd::IndexSync { .. }
    );
    let scan_dirs = command_scan_dirs(
        integrity,
        &rc.graph.scope.dirs,
        vault_page_dirs(&rc.root, &rc.vault_dirs),
    );

    if incremental {
        let cp = cache::cache_path(&rc.root);
        if let Some(cached) = cache::load(&cp) {
            let dirty = cache::is_dirty(&rc.root, &scan_dirs, rc.graph.scope.follow_links, &cached)
                .map_err(|e| format!("{e}"))?;
            if !dirty {
                eprintln!("No changes since last scan");
                return Ok(false);
            }
        }
    }

    // One scan over `scan_dirs`. Integrity commands derive the full-vault existence
    // universe from it and the scope-subset graph nodes by filtering (no second walk);
    // analysis commands use the scan as-is.
    let mut scan_cfg = rc.graph.clone();
    scan_cfg.scope.dirs = scan_dirs.clone();
    let scanned = scan::scan_vault(&rc.root, &scan_cfg).map_err(|e| format!("{e}"))?;
    let existence = scan::VaultExistence::from_pages(&scanned, &rc.vault_dirs);
    let pages: Vec<scan::ScannedPage> = if integrity {
        scanned
            .into_iter()
            .filter(|p| rc.graph.scope.dirs.iter().any(|d| p.path.starts_with(d)))
            .collect()
    } else {
        scanned
    };
    let g = graph::WikiGraph::build_with_existence(&pages, &existence, &rc.vault_dirs);

    let has_findings = match cmd {
        GraphCmd::Lint => {
            let hubs = g.hubs(10, rc.graph.metrics.min_hub_degree);
            let orphans = g.orphans(
                &rc.graph.metrics.orphan_exclude,
                Path::new(&rc.vault_dirs.wiki),
            );
            let broken = g.broken_links().to_vec();
            let drift = index_drift::diff(
                &g,
                &existence,
                &rc.root,
                Path::new(&rc.vault_dirs.wiki),
                &rc.graph.metrics.orphan_exclude,
            )
            .map_err(|e| format!("{e}"))?;
            // Read the concept pages once; the three concept lints are pure functions
            // over the result rather than each re-walking `{wiki}/concepts/`.
            let concept_pages = concept_lint::scan_concept_pages(&rc.root, &rc.vault_dirs.wiki)
                .map_err(|e| format!("{e}"))?;
            let invalid_categories =
                concept_lint::find_invalid_categories(&concept_pages, &rc.concept_categories);
            let near_duplicate_concepts = concept_lint::find_near_duplicate_concepts(
                &concept_pages,
                rc.graph.metrics.concept_near_duplicate_threshold,
            );
            let unresolved_conflicts = concept_lint::find_unresolved_conflicts(&concept_pages);
            // Alias conflicts read the scanned pages (which carry `aliases`), not the
            // concept-lint page set — the two declaration failures a silent first-wins
            // alias resolution would otherwise hide.
            let alias_conflicts = alias::find_alias_conflicts(&pages, &existence, &rc.vault_dirs);

            let findings = orphans.len()
                + broken.len()
                + drift.missing_from_index.len()
                + drift.missing_from_disk.len()
                + invalid_categories.len()
                + near_duplicate_concepts.len()
                + unresolved_conflicts.len()
                + alias_conflicts.len();

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
                invalid_categories,
                near_duplicate_concepts,
                unresolved_conflicts,
                alias_conflicts,
                findings,
            };
            if json {
                output::print_json(&report)?;
            } else {
                output::print_lint(&report);
            }
            findings > 0
        }
        GraphCmd::Hubs { top } => {
            let report = output::HubsReport {
                hubs: g.hubs(top, 1),
            };
            if json {
                output::print_json(&report)?;
            } else {
                output::print_hubs(&report);
            }
            false
        }
        GraphCmd::Orphans => {
            let orphans = g.orphans(
                &rc.graph.metrics.orphan_exclude,
                Path::new(&rc.vault_dirs.wiki),
            );
            let count = orphans.len();
            let report = output::OrphansReport { orphans, count };
            let has = report.count > 0;
            if json {
                output::print_json(&report)?;
            } else {
                output::print_orphans(&report);
            }
            has
        }
        GraphCmd::Broken => {
            let broken = g.broken_links().to_vec();
            let count = broken.len();
            let report = output::BrokenReport { broken, count };
            let has = report.count > 0;
            if json {
                output::print_json(&report)?;
            } else {
                output::print_broken(&report);
            }
            has
        }
        GraphCmd::Cluster { label, min_size } => {
            if let Some(size) = min_size {
                rc.graph.cluster.min_community_size = size;
            }
            let mut result = cluster::detect_communities(&g, &rc.graph);
            if label {
                cluster::label_communities(&g, &mut result.communities);
            }
            if json {
                output::print_json(&result)?;
            } else {
                output::print_cluster(&result);
            }
            false
        }
        GraphCmd::Export { with_clusters } => {
            let cluster_result = if with_clusters {
                Some(cluster::detect_communities(&g, &rc.graph))
            } else {
                None
            };
            let graph_export = export::export(&g, cluster_result.as_ref());
            let report = output::ExportReport {
                graph: graph_export,
            };
            if json {
                output::print_json(&report)?;
            } else {
                output::print_export(&report);
            }
            false
        }
        GraphCmd::IndexSync { fix } => {
            let drift = index_drift::diff(
                &g,
                &existence,
                &rc.root,
                Path::new(&rc.vault_dirs.wiki),
                &rc.graph.metrics.orphan_exclude,
            )
            .map_err(|e| format!("{e}"))?;
            let has = !drift.is_in_sync();
            let has_unfixable = !drift.missing_from_disk.is_empty();
            let fixed = if fix && !drift.missing_from_index.is_empty() {
                Some(
                    index_drift::fix(&drift, &rc.root, Path::new(&rc.vault_dirs.wiki))
                        .map_err(|e| format!("{e}"))?,
                )
            } else {
                None
            };
            let report = output::IndexSyncReport {
                missing_from_index: drift.missing_from_index,
                missing_from_disk: drift.missing_from_disk,
                fixed,
            };
            if json {
                output::print_json(&report)?;
            } else {
                output::print_index_report(&report);
            }
            has && (fixed.is_none() || has_unfixable)
        }
        GraphCmd::Normalize { fix } => {
            let renames = normalize::scan(&pages);
            let has = !renames.is_empty();
            let applied = if fix && has {
                Some(normalize::apply(&renames, &pages, &rc.root).map_err(|e| format!("{e}"))?)
            } else {
                None
            };
            let report = output::NormalizeReport {
                renames: renames
                    .iter()
                    .map(|r| output::RenameSuggestion {
                        from: r.old_slug.clone(),
                        to: r.new_slug.clone(),
                    })
                    .collect(),
                applied,
            };
            if json {
                output::print_json(&report)?;
            } else {
                output::print_normalize(&report);
            }
            has && applied.is_none()
        }
        GraphCmd::SuggestLinks { min_community_size } => {
            if let Some(size) = min_community_size {
                rc.graph.cluster.min_community_size = size;
            }
            let clusters = cluster::detect_communities(&g, &rc.graph);
            let result = cluster::suggest_links(
                &g,
                &clusters,
                rc.graph.cluster.suggest_min_shared_neighbors,
            );
            let count = result.pairs.len();
            let report = output::SuggestLinksReport {
                pairs: result.pairs,
                count,
            };
            if json {
                output::print_json(&report)?;
            } else {
                output::print_suggest_links(&report);
            }
            false
        }
        // Dispatched at the top of `run_inner` because they need full-vault scope
        // and/or config that doesn't touch the in-scope WikiGraph.
        GraphCmd::AuditCandidates
        | GraphCmd::AuditMark { .. }
        | GraphCmd::BacklinksSync { .. }
        | GraphCmd::Merge { .. } => {
            unreachable!()
        }
    };

    // Persist the mtime cache over the SAME set we scanned so the next `--incremental`
    // run's dirty-check is consistent with what this command reads.
    if incremental {
        save_cache_best_effort(&rc.root, &scan_dirs, rc.graph.scope.follow_links);
    }

    Ok(has_findings)
}

fn run_audit_candidates(
    opts: &GlobalOptions,
    root_override: Option<PathBuf>,
    json: bool,
) -> Result<bool, String> {
    let rc = resolve_config_full(opts, root_override)?;
    let candidates = audit::find_audit_candidates(&rc.root, &rc.vault_dirs.wiki, rc.locale)
        .map_err(|e| format!("{e}"))?;
    let count = candidates.len();
    let report = output::AuditCandidatesReport { candidates, count };
    if json {
        output::print_json(&report)?;
    } else {
        output::print_audit_candidates(&report);
    }
    Ok(count > 0)
}

fn run_audit_mark(
    opts: &GlobalOptions,
    root_override: Option<PathBuf>,
    json: bool,
    slug: &str,
) -> Result<bool, String> {
    let rc = resolve_config_full(opts, root_override)?;
    let changed = audit::mark_audited(&rc.root, &rc.vault_dirs.wiki, slug, rc.locale)
        .map_err(|e| format!("{e}"))?;
    if json {
        output::print_json(&serde_json::json!({ "slug": slug, "changed": changed }))?;
    } else if changed {
        println!("Marked '{slug}' as audited.");
    } else {
        println!("'{slug}' was already up to date.");
    }
    Ok(false)
}

fn run_backlinks(
    opts: &GlobalOptions,
    root_override: Option<PathBuf>,
    json: bool,
    dry_run: bool,
    incremental: bool,
) -> Result<bool, String> {
    let mut rc = resolve_config_full(opts, root_override)?;

    rc.graph.scope.dirs = vault_page_dirs(&rc.root, &rc.vault_dirs);

    if incremental {
        let cp = cache::cache_path(&rc.root);
        if let Some(cached) = cache::load(&cp) {
            let dirty = cache::is_dirty(
                &rc.root,
                &rc.graph.scope.dirs,
                rc.graph.scope.follow_links,
                &cached,
            )
            .map_err(|e| format!("{e}"))?;
            if !dirty {
                eprintln!("No changes since last scan");
                return Ok(false);
            }
        }
    }

    let pages = scan::scan_vault(&rc.root, &rc.graph).map_err(|e| format!("{e}"))?;

    let sync =
        backlinks::sync_concept_backlinks(&pages, &rc.root, rc.locale, dry_run, &rc.vault_dirs)
            .map_err(|e| format!("{e}"))?;
    let changed = sync.updated.len();
    let report = output::BacklinksSyncReport { sync, changed };

    if json {
        output::print_json(&report)?;
    } else {
        output::print_backlinks_report(&report);
    }

    if incremental && !dry_run {
        save_cache_best_effort(&rc.root, &rc.graph.scope.dirs, rc.graph.scope.follow_links);
    }

    Ok(false)
}

fn run_merge(
    opts: &GlobalOptions,
    root_override: Option<PathBuf>,
    json: bool,
    from: &str,
    into: &str,
    dry_run: bool,
    force: bool,
) -> Result<bool, String> {
    let mut rc = resolve_config_full(opts, root_override)?;
    // Merge rewrites links across the whole vault, so scan every page dir, not just
    // the analysis scope — the same full-vault view backlinks-sync uses.
    rc.graph.scope.dirs = vault_page_dirs(&rc.root, &rc.vault_dirs);

    let pages = scan::scan_vault(&rc.root, &rc.graph).map_err(|e| format!("{e}"))?;
    let result = merge::merge_concepts(
        &pages,
        &rc.root,
        &rc.vault_dirs.wiki,
        from,
        into,
        dry_run,
        force,
    )
    .map_err(|e| format!("{e}"))?;

    if json {
        output::print_json(&result)?;
    } else {
        output::print_merge(&result);
    }
    // Never a "findings" exit — a successful merge is exit 0.
    Ok(false)
}

/// Best-effort cache save: build a fresh mtime snapshot and persist it. Warnings
/// are printed to stderr but errors are not propagated — a failed cache save must
/// not turn a successful graph command into a failure.
fn save_cache_best_effort(root: &std::path::Path, scan_dirs: &[PathBuf], follow_links: bool) {
    if let Ok(fresh) = cache::build(root, scan_dirs, follow_links) {
        let cp = cache::cache_path(root);
        if let Err(e) = cache::save(&cp, &fresh) {
            eprintln!("warning: failed to save graph cache: {e}");
        }
    }
}

/// The directories a graph command reads — the single source of truth shared by the
/// scan, the `--incremental` cache watch set, and the cache rebuild. Integrity
/// commands (lint/broken/orphans/index-sync) resolve links against the full-vault
/// existence universe, so they read `scope.dirs` ∪ every page dir; analysis commands
/// (hubs/cluster/…) read `scope.dirs` only. `page_dirs` already in scope are not
/// duplicated, preserving scope-first order.
fn command_scan_dirs(
    integrity: bool,
    scope_dirs: &[PathBuf],
    page_dirs: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut dirs = scope_dirs.to_vec();
    if integrity {
        for d in page_dirs {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    dirs
}

/// Every vault-relative page directory that exists on disk — anything that can
/// wikilink another page. Used by commands that need a full-vault view
/// (`backlinks-sync`, and the existence universe behind `lint`'s integrity
/// checks) rather than the user-configured `graph.scope.dirs`, which stays
/// narrowed for structural analysis (`hubs`/`cluster`/`suggest-links`). Missing
/// directories are skipped so a partially-populated vault doesn't error out.
fn vault_page_dirs(root: &std::path::Path, dirs: &VaultDirs) -> Vec<PathBuf> {
    [&dirs.wiki, &dirs.daily, &dirs.personal, &dirs.synthesis]
        .iter()
        .filter(|name| root.join(name).is_dir())
        .map(|s| PathBuf::from(s.as_str()))
        .collect()
}

fn resolve_config_full(
    opts: &GlobalOptions,
    root_override: Option<PathBuf>,
) -> Result<ResolvedConfig, String> {
    if let Some(root) = root_override {
        let vault_dirs = VaultDirs::default();
        let mut graph = GraphConfig::default();
        graph.apply_vault_defaults(&vault_dirs);
        return Ok(ResolvedConfig {
            root,
            graph,
            locale: Locale::default(),
            vault_dirs,
            concept_categories: Vec::new(),
        });
    }

    let config_path = super::find_config(opts).map_err(|e| format!("{e}"))?;
    let config = super::load_config(&config_path).map_err(|e| format!("{e}"))?;
    Ok(ResolvedConfig {
        root: config.vault.root_path(),
        locale: config.vault.locale(),
        vault_dirs: config.vault.dirs.clone(),
        graph: config.graph,
        concept_categories: config.concepts.categories,
    })
}

#[cfg(test)]
mod tests {
    use super::command_scan_dirs;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn analysis_commands_read_scope_only() {
        // Non-integrity commands ignore page dirs entirely.
        let dirs = command_scan_dirs(false, &[p("wiki")], vec![p("wiki"), p("daily"), p("me")]);
        assert_eq!(dirs, vec![p("wiki")]);
    }

    #[test]
    fn integrity_commands_union_scope_with_page_dirs_scope_first() {
        // Integrity commands add every page dir not already in scope, scope-first.
        let dirs = command_scan_dirs(
            true,
            &[p("wiki")],
            vec![p("wiki"), p("daily"), p("me"), p("syn")],
        );
        assert_eq!(dirs, vec![p("wiki"), p("daily"), p("me"), p("syn")]);
    }

    #[test]
    fn integrity_does_not_duplicate_scope_dirs_already_listed() {
        // A page dir already in scope is not appended twice.
        let dirs = command_scan_dirs(true, &[p("daily"), p("wiki")], vec![p("wiki"), p("daily")]);
        assert_eq!(dirs, vec![p("daily"), p("wiki")]);
    }
}
