use std::path::PathBuf;

use lk_core::config::{ConceptCategory, GraphConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_graph::{
    backlinks, cluster, concept_lint, export, graph, index_drift, merge, normalize, output, scan,
};

use super::GlobalOptions;

struct ResolvedConfig {
    root: PathBuf,
    graph: GraphConfig,
    locale: Locale,
    vault_dirs: VaultDirs,
    concept_categories: Vec<ConceptCategory>,
    llm: lk_core::config::LlmConfig,
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
/// disagreement between sources a curator recorded. Those are never empty in a living vault,
/// so counting them would make the exit code permanently non-zero, and a verdict that is never
/// clean carries no information.
pub async fn run(
    opts: &GlobalOptions,
    cmd: GraphCommand,
    json: bool,
    root_override: Option<PathBuf>,
) -> i32 {
    match run_inner(opts, cmd, json, root_override).await {
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

async fn run_inner(
    opts: &GlobalOptions,
    cmd: GraphCommand,
    json: bool,
    root_override: Option<PathBuf>,
) -> Result<bool, String> {
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
                        stale: !drift.is_in_sync(),
                        absent: drift.absent,
                        missing_from_index: drift.missing_from_index,
                        missing_from_disk: drift.missing_from_disk,
                        fixed: None,
                    },
                    invalid_categories,
                    duplicate_concepts,
                    address_collisions: scan::address_collisions(&views.link_sources),
                    unnormalized: rename_suggestions(&normalize::scan(&views.pages)),
                    respelled_links: graph::respelled_links(
                        &views.link_sources,
                        &views.existence,
                        &rc.vault_dirs,
                    ),
                },
                observations: output::Observations {
                    hubs,
                    orphans,
                    unresolved_conflicts,
                },
            };
            let violated = report.violations.count() > 0;
            if json {
                output::print_json(&report, violated)?;
            } else {
                output::print_lint(&report);
            }
            violated
        }
        GraphCommand::Hubs { top } => {
            let report = output::HubsReport {
                hubs: g.hubs(top, 1),
            };
            // An observation: a hub is true of a vault in good standing.
            let violated = false;
            if json {
                output::print_json(&report, violated)?;
            } else {
                output::print_hubs(&report);
            }
            violated
        }
        GraphCommand::Orphans => {
            let orphans = g.orphans(&rc.graph.metrics.orphan_exclude);
            let count = orphans.len();
            let report = output::OrphansReport { orphans, count };
            // An observation: a concept nothing cites yet is not a false claim.
            let violated = false;
            if json {
                output::print_json(&report, violated)?;
            } else {
                output::print_orphans(&report);
            }
            violated
        }
        GraphCommand::Broken => {
            let broken = graph::broken_links(&views.link_sources, &views.existence, &rc.vault_dirs);
            let count = broken.len();
            let report = output::BrokenReport { broken, count };
            let violated = report.count > 0;
            if json {
                output::print_json(&report, violated)?;
            } else {
                output::print_broken(&report);
            }
            violated
        }
        GraphCommand::Cluster { label, min_size } => {
            if let Some(size) = min_size {
                rc.graph.cluster.min_community_size = size;
            }
            let mut result = cluster::detect_communities(&g, &rc.graph);
            if label {
                cluster::label_communities(&g, &mut result.communities);
            }
            // An observation: communities describe a vault, they do not indict it.
            let violated = false;
            if json {
                output::print_json(&result, violated)?;
            } else {
                output::print_cluster(&result);
            }
            violated
        }
        GraphCommand::Export { with_clusters } => {
            let cluster_result = if with_clusters {
                Some(cluster::detect_communities(&g, &rc.graph))
            } else {
                None
            };
            let graph_export = export::export(&g, cluster_result.as_ref());
            // A dump of the graph, not a verdict on it.
            let violated = false;
            if json {
                output::print_json(&graph_export, violated)?;
            } else {
                output::print_export(&graph_export);
            }
            violated
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
                stale: has,
                absent: drift.absent,
                missing_from_index: drift.missing_from_index,
                missing_from_disk: drift.missing_from_disk,
                fixed,
            };
            let violated = has && fixed.is_none();
            if json {
                output::print_json(&report, violated)?;
            } else {
                output::print_index_sync(&report);
            }
            violated
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
            let violated = has && applied.is_none();
            if json {
                output::print_json(&report, violated)?;
            } else {
                output::print_normalize(&report);
            }
            violated
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
            // An observation: a missing link is a suggestion, never a false claim.
            let violated = false;
            if json {
                output::print_json(&report, violated)?;
            } else {
                output::print_suggest_links(&report);
            }
            violated
        }
        GraphCommand::BacklinksSync { dry_run } => {
            // A provider that queues nothing must not have the sweep write a promise nothing
            // can keep: `lore doctor` reports a recorded input with no answer, and no path
            // re-derives a concept page's markers, so it would never clear.
            let policy = match rc.llm.provider {
                lk_core::config::LlmProvider::Queue => backlinks::SynthesisPolicy::Record,
                lk_core::config::LlmProvider::Noop => backlinks::SynthesisPolicy::Skip,
            };
            let sync = backlinks::sync_concept_backlinks(
                &views.scanned,
                &rc.root,
                dry_run,
                &rc.vault_dirs,
                policy,
            )
            .map_err(|e| format!("{e}"))?;
            let changed = sync.updated.len();
            // Deriving a concept's evidence and writing the section that answers to it are
            // separate acts, and only this sweep knows the first has moved. It computes what
            // is owed; turning that into queued work is the CLI's, because lk-graph reaches
            // no LLM. A dry run enqueues nothing, like every other write it withholds.
            let queued = if dry_run {
                0
            } else {
                enqueue_syntheses(&rc, &sync.resynthesize).await?
            };
            // A page the sweep could not record a count on keeps a `source_count` the graph
            // contradicts, and only a human adding frontmatter fixes it.
            let violated = !sync.skipped.is_empty();
            let report = output::BacklinksSyncReport {
                sync,
                changed,
                queued,
            };
            if json {
                output::print_json(&report, violated)?;
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
            // A successful merge leaves nothing contradicted.
            let violated = false;
            if json {
                output::print_json(&result, violated)?;
            } else {
                output::print_merge(&result);
            }
            violated
        }
    };

    Ok(violated)
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

/// Queue one synthesis task per concept whose evidence has moved, and commit them as one
/// file. The queue's own invariant carries over unchanged: a task file becomes visible only
/// after every page it names was written, which `sync_concept_backlinks` has already done by
/// the time this runs.
///
/// The task carries the citation set as its input and the page's own heading as its anchor,
/// so the drain writes the section the page actually has — the same reason every other task
/// carries one.
async fn enqueue_syntheses(
    rc: &ResolvedConfig,
    owed: &[backlinks::ConceptResynthesis],
) -> Result<usize, String> {
    // A task already waiting is the same task. This sweep runs on every pipeline pass and by
    // hand, and re-queueing what a drain has not reached yet accumulates identical jobs that
    // all classify `current` — so `queue prune` keeps them and the drain does the work again
    // for each. The ingest path has no such hazard: a re-render REPLACES that run's file.
    let pending = pending_synthesis_targets(&rc.root);
    let owed: Vec<&backlinks::ConceptResynthesis> = owed
        .iter()
        .filter(|entry| !pending.contains(&entry.path.to_string_lossy().into_owned()))
        .collect();
    if owed.is_empty() {
        return Ok(0);
    }
    let client = super::build_llm_client_for(rc.llm.provider, &rc.root);
    for entry in owed.iter().copied() {
        client
            .synthesize_concept(lk_queue::ConceptSynthesisRequest {
                citations: entry.citations.clone(),
                target: lk_queue::TaskTarget {
                    vault_path: entry.path.to_string_lossy().into_owned(),
                    kind: lk_queue::TargetKind::ConceptSynthesis,
                    anchor: entry.anchor.clone(),
                },
            })
            .await
            .map_err(|e| format!("queue synthesis for {}: {e}", entry.path.display()))?;
    }
    client.flush().await.map_err(|e| format!("{e}"))?;
    Ok(owed.len())
}

/// The `vault_path` of every concept-synthesis task already waiting in the queue.
///
/// Read straight off the pending files rather than through a classification: the question is
/// only whether this exact work is already asked for, and a task whose page moved on is
/// `stale` — which `queue prune` drops and the next sweep re-queues, so treating it as
/// pending here costs one cycle and never loses the work. An unreadable queue file yields
/// nothing, which re-queues rather than skips.
fn pending_synthesis_targets(vault_root: &std::path::Path) -> std::collections::HashSet<String> {
    let dir = vault_root.join(".lorekeeper").join("queue");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return std::collections::HashSet::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .flat_map(|content| {
            content
                .lines()
                .filter_map(|line| serde_json::from_str::<lk_queue::QueueTask>(line).ok())
                .filter(|task| task.target.kind == lk_queue::TargetKind::ConceptSynthesis)
                .map(|task| task.target.vault_path)
                .collect::<Vec<_>>()
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
            llm: config.llm,
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
                llm: lk_core::config::LlmConfig::default(),
            })
        }
    }
}
