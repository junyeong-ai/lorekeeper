use std::path::PathBuf;

use lk_core::config::{ConceptCategory, GraphConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_graph::{
    audit, backlinks, cluster, concept_lint, export, graph, index_drift, merge, normalize, output,
    scan,
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
pub enum GraphCommand {
    /// Run all graph checks in one pass
    Lint,
    /// Show top hub pages by link degree
    Hubs {
        /// Number of top hubs to display
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    /// Find orphan pages with zero links
    Orphans,
    /// Find broken links pointing to non-existent pages
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
        /// Apply renames and repoint links
        #[arg(long)]
        fix: bool,
    },
    /// Suggest links between co-clustered pages that are not yet linked
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
    /// Merge a duplicate concept into a canonical one: repoint every link from
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

/// Returns exit code: 0 = every claim the vault makes holds, 1 = it contradicts itself,
/// 2 = runtime error.
///
/// A non-zero exit is reserved for a claim that is FALSE and has a named repair: a link whose
/// destination does not exist, a catalog that disagrees with the disk, a category outside the
/// configured vocabulary, one name answering to two pages, a filename that disagrees with its
/// own normalized slug, a derived count no sweep could write. What a vault in good standing
/// legitimately carries is reported and exits 0 — the concepts nothing cites yet, the hubs, a
/// disagreement between sources an audit recorded, a concept whose evidence changed since its
/// last audit. Those are never empty in a living vault, so counting them would make the exit
/// code permanently non-zero, and a verdict that is never clean carries no information.
pub fn run(
    opts: &GlobalOptions,
    cmd: GraphCommand,
    json: bool,
    root_override: Option<PathBuf>,
) -> i32 {
    match run_inner(opts, cmd, json, root_override) {
        Ok(violated) => {
            if violated {
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
    cmd: GraphCommand,
    json: bool,
    root_override: Option<PathBuf>,
) -> Result<bool, String> {
    // These two read the concept pages directly and need no scan of the vault.
    match cmd {
        GraphCommand::AuditCandidates => {
            return run_audit_candidates(opts, root_override, json);
        }
        GraphCommand::AuditMark { ref slug } => {
            return run_audit_mark(opts, root_override, json, slug);
        }
        _ => {}
    }

    let mut rc = resolve_config_full(opts, root_override)?;
    let views = scan::VaultViews::resolve(&rc.root, &rc.graph, &rc.vault_dirs)
        .map_err(|e| format!("{e}"))?;
    let g = graph::WikiGraph::build_with_existence(&views.pages, &views.existence, &rc.vault_dirs);

    let violated = match cmd {
        GraphCommand::Lint => {
            let hubs = g.hubs(10, rc.graph.metrics.min_hub_degree);
            let orphans = g.orphans(&rc.graph.metrics.orphan_exclude);
            let broken = graph::broken_links(&views.link_sources, &views.existence, &rc.vault_dirs);
            let drift = index_drift::diff(&rc.root, rc.locale, &rc.vault_dirs)
                .map_err(|e| format!("{e}"))?;
            // Read the concept pages once; the three concept lints are pure functions
            // over the result rather than each re-walking `{wiki}/concepts/`.
            let concept_pages = concept_lint::scan_concept_pages(&rc.root, &rc.vault_dirs.wiki)
                .map_err(|e| format!("{e}"))?;
            let invalid_categories =
                concept_lint::find_invalid_categories(&concept_pages, &rc.concept_categories);
            let duplicate_concepts = concept_lint::find_duplicate_concepts(&concept_pages);
            let unresolved_conflicts = concept_lint::find_unresolved_conflicts(&concept_pages);

            let report = output::LintReport {
                pages: g.node_count(),
                links: g.edge_count(),
                components: g.component_count(),
                violations: output::Violations {
                    broken,
                    index: output::IndexSyncReport {
                        missing_from_index: drift.missing_from_index,
                        missing_from_disk: drift.missing_from_disk,
                        fixed: None,
                    },
                    invalid_categories,
                    duplicate_concepts,
                    address_collisions: scan::address_collisions(&views.scanned),
                    unnormalized: rename_suggestions(&normalize::scan(&views.pages)),
                },
                observations: output::Observations {
                    hubs,
                    orphans,
                    unresolved_conflicts,
                },
            };
            let violated = report.violations.count() > 0;
            if json {
                output::print_json(&report)?;
            } else {
                output::print_lint(&report);
            }
            violated
        }
        GraphCommand::Hubs { top } => {
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
        GraphCommand::Orphans => {
            let orphans = g.orphans(&rc.graph.metrics.orphan_exclude);
            let count = orphans.len();
            let report = output::OrphansReport { orphans, count };
            if json {
                output::print_json(&report)?;
            } else {
                output::print_orphans(&report);
            }
            false
        }
        GraphCommand::Broken => {
            let broken = graph::broken_links(&views.link_sources, &views.existence, &rc.vault_dirs);
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
        GraphCommand::Cluster { label, min_size } => {
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
        GraphCommand::Export { with_clusters } => {
            let cluster_result = if with_clusters {
                Some(cluster::detect_communities(&g, &rc.graph))
            } else {
                None
            };
            let graph_export = export::export(&g, cluster_result.as_ref());
            if json {
                output::print_json(&graph_export)?;
            } else {
                output::print_export(&graph_export);
            }
            false
        }
        GraphCommand::IndexSync { fix } => {
            let drift = index_drift::diff(&rc.root, rc.locale, &rc.vault_dirs)
                .map_err(|e| format!("{e}"))?;
            let has = !drift.is_in_sync();
            // The repair re-derives the whole catalog, so it resolves drift in both
            // directions at once — there is no half it can leave behind.
            let fixed = if fix && has {
                Some(
                    index_drift::fix(&drift, &rc.root, &rc.vault_dirs)
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
                output::print_index_sync(&report);
            }
            has && fixed.is_none()
        }
        GraphCommand::Normalize { fix } => {
            // Rename candidates come from the analysis scope: only the wiki's own pages
            // are addressed by slug. A daily or synthesis page's filename is a DATE
            // (`2026-W30`), which slugifying would lowercase into a path the pipeline
            // does not write.
            let renames = normalize::scan(&views.pages);
            let has = !renames.is_empty();
            let applied = if fix && has {
                Some(
                    normalize::apply(&renames, &views.scanned, &rc.root)
                        .map_err(|e| format!("{e}"))?,
                )
            } else {
                None
            };
            let report = output::NormalizeReport {
                renames: rename_suggestions(&renames),
                applied,
            };
            if json {
                output::print_json(&report)?;
            } else {
                output::print_normalize(&report);
            }
            has && applied.is_none()
        }
        GraphCommand::SuggestLinks { min_community_size } => {
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
        GraphCommand::BacklinksSync { dry_run } => {
            let sync = backlinks::sync_concept_backlinks(
                &views.scanned,
                &rc.root,
                rc.locale,
                dry_run,
                &rc.vault_dirs,
            )
            .map_err(|e| format!("{e}"))?;
            let changed = sync.updated.len();
            // A page the sweep could not record a count on keeps a `source_count` the graph
            // contradicts, and only a human adding frontmatter fixes it.
            let violated = !sync.skipped.is_empty();
            let report = output::BacklinksSyncReport { sync, changed };
            if json {
                output::print_json(&report)?;
            } else {
                output::print_backlinks(&report);
            }
            violated
        }
        GraphCommand::Merge {
            ref from,
            ref into,
            dry_run,
            force,
        } => {
            let result = merge::merge_concepts(
                &views.scanned,
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
            // A successful merge leaves nothing contradicted.
            false
        }
        // Dispatched at the top of `run_inner`: they read the concept pages directly.
        GraphCommand::AuditCandidates | GraphCommand::AuditMark { .. } => unreachable!(),
    };

    Ok(violated)
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
    // A worklist, not a defect: a concept whose evidence changed since its last audit says
    // nothing false about the vault, so the list can be read under `set -e`.
    Ok(false)
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

/// A rename candidate as the report carries it: the slug a file has and the one its own name
/// normalizes to. `lint` and `normalize` present the same finding, so both read it from here.
fn rename_suggestions(renames: &[normalize::Rename]) -> Vec<output::RenameSuggestion> {
    renames
        .iter()
        .map(|r| output::RenameSuggestion {
            from: r.old_slug.clone(),
            to: r.new_slug.clone(),
        })
        .collect()
}

fn resolve_config_full(
    opts: &GlobalOptions,
    root_override: Option<PathBuf>,
) -> Result<ResolvedConfig, String> {
    // Single override semantics (shared with `wiki`/`schema`): a present config drives
    // dirs/locale/graph/categories even under `--root` (only the root is overridden) — falling
    // back to defaults wholesale would scan the WRONG dirs, skip category lint, and resolve
    // headings in the wrong locale. Defaults apply ONLY when no config file exists.
    let super::RootConfig { root, config } =
        super::resolve_root_config(opts, root_override).map_err(|e| format!("{e}"))?;
    match config {
        Some(config) => Ok(ResolvedConfig {
            root,
            locale: config.vault.locale(),
            vault_dirs: config.vault.dirs.clone(),
            graph: config.graph,
            concept_categories: config.concepts.categories,
        }),
        None => {
            let vault_dirs = VaultDirs::default();
            let mut graph = GraphConfig::default();
            graph.apply_vault_defaults(&vault_dirs);
            Ok(ResolvedConfig {
                root,
                graph,
                locale: Locale::default(),
                vault_dirs,
                concept_categories: Vec::new(),
            })
        }
    }
}
