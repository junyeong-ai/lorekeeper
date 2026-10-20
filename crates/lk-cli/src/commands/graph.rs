mod resynthesize;
mod retract_citation;

use std::path::PathBuf;

use lk_core::config::{ConceptCategory, GraphConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_graph::{backlinks, cluster, concept_lint, export, graph, merge, normalize, output, scan};

use super::GlobalOptions;

struct ResolvedConfig {
    root: PathBuf,
    graph: GraphConfig,
    locale: Locale,
    vault_dirs: VaultDirs,
    concept_categories: Vec<ConceptCategory>,
    llm: lk_core::config::LlmConfig,
}

#[derive(Clone, Copy, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NeighborDirection {
    Incoming,
    Outgoing,
    Mutual,
}

impl NeighborDirection {
    fn matches(self, page: &graph::NeighborPage) -> bool {
        match self {
            Self::Incoming => page.incoming,
            Self::Outgoing => page.outgoing,
            Self::Mutual => page.incoming && page.outgoing,
        }
    }
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
    /// Follow direct incoming/outgoing links across managed pages, including daily evidence
    Neighbors {
        /// Exact vault-relative Markdown path returned by search or browse
        path: PathBuf,
        /// Exact declared page type; an empty value selects pages without a type
        #[arg(long = "type")]
        format: Option<String>,
        /// Link direction; incoming/outgoing also include mutually linked pages
        #[arg(long, value_enum)]
        direction: Option<NeighborDirection>,
        /// Maximum neighboring pages to return
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Skip this many neighboring pages in path order
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
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
        /// Maximum candidates to return (1..=100)
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Skip this many candidates in ranked order
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Require the same candidate revision across pages
        #[arg(long)]
        revision: Option<String>,
    },
    /// Rewrite each concept page's `## <Sources>` section from the graph
    BacklinksSync {
        /// Report what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Request a fresh synthesis for one exact concept from its current citations.
    Resynthesize {
        /// Exact existing vault-relative concept Markdown path
        path: PathBuf,
        /// Preview current evidence and replaced sections without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Retract one mistaken evidence citation from an origin's related-concepts list.
    RetractCitation {
        /// Exact existing vault-relative source Markdown path
        origin: PathBuf,
        /// Exact existing vault-relative concept Markdown path
        concept: PathBuf,
        /// Required exact file revision from `lore wiki read`
        #[arg(long)]
        revision: String,
        /// Retract only this explicitly selected RSS event/version pair
        #[arg(long, requires = "event_revision")]
        event: Option<String>,
        /// Exact immutable payload revision of the selected RSS event
        #[arg(long, requires = "event")]
        event_revision: Option<String>,
        /// Validate and preview without archiving or writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Merge a duplicate concept into a canonical one: repoint every link from
    /// `<from>` to `<into>`, then delete the `<from>` page. Run `backlinks-sync`
    /// afterward to re-derive the merged `## Sources` + `source_count`.
    Merge {
        /// Exact collection-relative address without .md of the concept to delete
        from: String,
        /// Exact collection-relative address without .md of the concept to keep
        into: String,
        /// Full file revision read before comparing the concept to delete
        #[arg(long, requires = "into_revision", required_unless_present = "dry_run")]
        from_revision: Option<String>,
        /// Full file revision read before comparing the survivor
        #[arg(long, requires = "from_revision", required_unless_present = "dry_run")]
        into_revision: Option<String>,
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
/// destination does not exist, a category outside the
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

/// Every structural claim the vault makes about itself, and every observation that guides a
/// human's next decision.
///
/// Split from the `lint` arm so the front-door summary reports the same verdict this command
/// exits on. Re-deriving it there would be a second answer to one question, and the two would
/// agree only until either was edited.
fn lint_report(
    rc: &ResolvedConfig,
    g: &graph::WikiGraph,
    views: &scan::VaultViews,
) -> Result<output::LintReport, String> {
    // Read the concept pages once; the three concept lints are pure functions over the result
    // rather than each re-walking `{wiki}/concepts/`.
    let concept_pages = concept_lint::scan_concept_pages(&rc.root, &rc.vault_dirs.wiki)
        .map_err(|e| format!("{e}"))?;

    Ok(output::LintReport {
        errors: Vec::new(),
        pages: g.node_count(),
        links: g.edge_count(),
        components: g.component_count(),
        violations: output::Violations {
            broken: graph::broken_links(&views.link_sources, &views.existence, &rc.vault_dirs),
            invalid_categories: concept_lint::find_invalid_categories(
                &concept_pages,
                &rc.concept_categories,
            ),
            duplicate_concepts: concept_lint::find_duplicate_concepts(&concept_pages),
            address_collisions: scan::address_collisions(&views.link_sources),
            unnormalized: rename_suggestions(&normalize::scan(&views.pages)),
            respelled_links: graph::respelled_links(
                &views.link_sources,
                &views.existence,
                &rc.vault_dirs,
            ),
        },
        observations: output::Observations {
            hubs: g.hubs(10, rc.graph.metrics.min_hub_degree),
            orphans: g.orphans(&rc.graph.metrics.orphan_exclude),
            unresolved_conflicts: concept_lint::find_unresolved_conflicts(&concept_pages),
        },
    })
}

/// The lint verdict for a vault, resolved and scanned from nothing but the caller's options.
pub(crate) fn lint(
    opts: &GlobalOptions,
    root_override: Option<PathBuf>,
) -> Result<output::LintReport, String> {
    let rc = resolve_config_full(opts, root_override)?;
    let views = scan::VaultViews::resolve(&rc.root, &rc.graph, &rc.vault_dirs)
        .map_err(|e| format!("{e}"))?;
    let g = graph::WikiGraph::build_with_existence(&views.pages, &views.existence, &rc.vault_dirs);
    lint_report(&rc, &g, &views)
}

async fn run_inner(
    opts: &GlobalOptions,
    cmd: GraphCommand,
    json: bool,
    root_override: Option<PathBuf>,
) -> Result<bool, String> {
    if let GraphCommand::SuggestLinks { limit, .. } = &cmd
        && !(1..=100).contains(limit)
    {
        return Err("suggest-links requires a limit between 1 and 100".into());
    }
    let mut rc = resolve_config_full(opts, root_override)?;
    let mutation = if matches!(
        &cmd,
        GraphCommand::Normalize { fix: true }
            | GraphCommand::Merge { dry_run: false, .. }
            | GraphCommand::BacklinksSync { dry_run: false }
            | GraphCommand::Resynthesize { dry_run: false, .. }
            | GraphCommand::RetractCitation { dry_run: false, .. }
    ) {
        Some(lk_vault::MutationSession::acquire(&rc.root).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let views = scan::VaultViews::resolve(&rc.root, &rc.graph, &rc.vault_dirs)
        .map_err(|e| format!("{e}"))?;
    let g = graph::WikiGraph::build_with_existence(&views.pages, &views.existence, &rc.vault_dirs);

    let violated = match cmd {
        GraphCommand::RetractCitation {
            origin,
            concept,
            revision,
            event,
            event_revision,
            dry_run,
        } => {
            let selection = event.zip(event_revision).map(|(event_id, revision)| {
                lk_core::rss_evidence::RssEventSelection { event_id, revision }
            });
            let report = retract_citation::request_selected(
                &rc,
                &views.scanned,
                &origin,
                &concept,
                &revision,
                dry_run,
                mutation.as_ref(),
                selection.as_ref(),
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
            false
        }
        GraphCommand::Resynthesize { path, dry_run } => {
            let report =
                resynthesize::request(&rc, &views.scanned, &path, dry_run, mutation.as_ref())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
            );
            false
        }
        GraphCommand::Neighbors {
            path,
            format,
            direction,
            limit,
            offset,
        } => {
            if limit == 0 {
                return Err("--limit must be > 0".into());
            }
            if path.extension().is_none_or(|extension| extension != "md") {
                return Err("expected an exact vault-relative .md path".into());
            }
            lk_vault::page_path(&rc.root, &path).map_err(|error| error.to_string())?;
            let graph = graph::WikiGraph::build_with_existence(
                &views.link_sources,
                &views.existence,
                &rc.vault_dirs,
            );
            let id = scan::page_id(&path);
            let neighbors = graph
                .page_neighbors(&id)
                .ok_or_else(|| format!("not a managed knowledge page: {}", path.display()))?;
            let mut formats = std::collections::BTreeMap::<String, usize>::new();
            for neighbor in &neighbors {
                *formats
                    .entry(neighbor.format.clone().unwrap_or_default())
                    .or_default() += 1;
            }
            let neighbors: Vec<_> = neighbors
                .into_iter()
                .filter(|neighbor| {
                    format.as_ref().is_none_or(|format| {
                        neighbor.format.as_deref().unwrap_or_default() == format
                    }) && direction.is_none_or(|direction| direction.matches(neighbor))
                })
                .collect();
            let total = neighbors.len();
            let neighbors: Vec<_> = neighbors.into_iter().skip(offset).take(limit).collect();
            let end = offset.saturating_add(neighbors.len());
            let next_offset = (end < total).then_some(end);
            if json {
                output::print_json(
                    &serde_json::json!({
                        "path": path, "neighbors": neighbors, "total": total,
                        "offset": offset, "next_offset": next_offset,
                        "type": format, "direction": direction, "formats": formats,
                    }),
                    false,
                )?;
            } else {
                eprintln!("{} of {total} neighbors (offset {offset})", neighbors.len());
                for neighbor in neighbors {
                    let direction = match (neighbor.incoming, neighbor.outgoing) {
                        (true, true) => "both",
                        (true, false) => "incoming",
                        _ => "outgoing",
                    };
                    println!("{direction}  {}  {}", neighbor.path, neighbor.title);
                }
                if let Some(offset) = next_offset {
                    eprintln!("next page: repeat with --offset {offset}");
                }
            }
            false
        }
        GraphCommand::Lint => {
            let report = lint_report(&rc, &g, &views)?;
            let violated = report.violations.count() > 0;
            if json {
                output::print_json(&report, violated || !report.errors.is_empty())?;
            } else {
                output::print_lint(&report);
            }
            if !report.errors.is_empty() {
                return Err(report.errors.join("; "));
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
        GraphCommand::Normalize { fix } => {
            // Rename candidates come from the analysis scope: only the wiki's own pages
            // are addressed by slug. A daily or synthesis page's filename is a DATE
            // (`2026-W30`), which slugifying would lowercase into a path the pipeline
            // does not write.
            let renames = normalize::scan(&views.pages);
            let has = !renames.is_empty();
            let applied = if fix && has {
                Some(
                    normalize::apply_in_session(
                        &renames,
                        &views.scanned,
                        mutation
                            .as_ref()
                            .expect("mutation command holds the vault lock"),
                    )
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
        GraphCommand::SuggestLinks {
            min_community_size,
            limit,
            offset,
            revision,
        } => {
            if let Some(size) = min_community_size {
                rc.graph.cluster.min_community_size = size;
            }
            let clusters = cluster::detect_communities(&g, &rc.graph);
            let result = cluster::suggest_links(
                &g,
                &clusters,
                rc.graph.cluster.suggest_min_shared_neighbors,
            );
            let serialized = serde_json::to_string(&result).map_err(|error| error.to_string())?;
            let current = lk_core::concept::file_revision(&serialized);
            if revision.is_some_and(|expected| expected != current) {
                return Err("link candidates changed; restart candidate navigation".into());
            }
            let total = result.pairs.len();
            let pairs: Vec<_> = result.pairs.into_iter().skip(offset).take(limit).collect();
            let end = offset.saturating_add(pairs.len());
            let report = output::SuggestLinksReport {
                pairs,
                revision: current,
                total,
                offset,
                next_offset: (end < total).then_some(end),
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
            let report = synchronize_backlinks(&rc, &views.scanned, dry_run, mutation.as_ref())?;
            // A page the sweep could not record a count on keeps a `source_count` the graph
            // contradicts, and only a human adding frontmatter fixes it.
            let violated = !report.sync.skipped.is_empty();
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
            ref from_revision,
            ref into_revision,
            dry_run,
            force,
        } => {
            let request = merge::MergeRequest {
                from,
                into,
                revisions: from_revision
                    .as_deref()
                    .zip(into_revision.as_deref())
                    .map(|(from, into)| merge::MergeRevisions { from, into }),
                force,
            };
            let result = if let Some(session) = mutation.as_ref() {
                let workflow_id = match std::env::var("LORE_WORKFLOW_ID") {
                    Ok(value) => Some(lk_core::execution::WorkflowId::try_from(value)?),
                    Err(std::env::VarError::NotPresent) => None,
                    Err(std::env::VarError::NotUnicode(_)) => {
                        return Err("LORE_WORKFLOW_ID must be UTF-8".into());
                    }
                };
                merge::merge_in_session(
                    &views.scanned,
                    session,
                    &rc.vault_dirs.wiki,
                    &request,
                    workflow_id.as_ref(),
                )
            } else {
                merge::merge_concepts(
                    &views.scanned,
                    &rc.root,
                    &rc.vault_dirs.wiki,
                    &request,
                    dry_run,
                )
            }
            .map_err(|e| e.to_string())?;
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

fn synchronize_backlinks(
    rc: &ResolvedConfig,
    pages: &[scan::ScannedPage],
    dry_run: bool,
    session: Option<&lk_vault::MutationSession>,
) -> Result<output::BacklinksSyncReport, String> {
    let policy = match rc.llm.provider {
        lk_core::config::LlmProvider::Queue => backlinks::SynthesisPolicy::Record,
        lk_core::config::LlmProvider::Noop => backlinks::SynthesisPolicy::Skip,
    };
    if dry_run {
        let sync = backlinks::sync_concept_backlinks(pages, &rc.root, true, &rc.vault_dirs, policy)
            .map_err(|error| error.to_string())?;
        return Ok(output::BacklinksSyncReport {
            changed: sync.updated.len(),
            sync,
            queued: 0,
        });
    }
    let _claim = if policy == backlinks::SynthesisPolicy::Record {
        super::queue::claim_writer(&rc.root.join(".lorekeeper/queue"), false)
            .map_err(|error| error.to_string())?
    } else {
        None
    };
    let (plan, report) = plan_backlinks_publication(rc, pages, policy)?;
    if report.changed > 0 || report.queued > 0 {
        session
            .ok_or("backlinks publication requires a vault mutation session")?
            .apply(plan)
            .map_err(|error| error.to_string())?;
    }
    Ok(report)
}

fn plan_backlinks_publication(
    rc: &ResolvedConfig,
    pages: &[scan::ScannedPage],
    policy: backlinks::SynthesisPolicy,
) -> Result<(lk_vault::MutationPlan, output::BacklinksSyncReport), String> {
    let (mut plan, sync) =
        backlinks::plan_concept_backlinks_for_work(pages, &rc.root, &rc.vault_dirs, policy)
            .map_err(|error| error.to_string())?;
    // A task already waiting for the SAME INPUT is the same task. This sweep runs on every
    // pipeline pass and by hand, and re-queueing what a drain has not reached yet accumulates
    // identical jobs that all classify `current` — so `queue prune` keeps them and the drain
    // does the work again for each. The shared queue admission also reuses exact pending requests.
    //
    // Keyed on the input AND on the task still being alive — both, because a task can stop
    // being answerable in two independent ways. Its input can move on (an ordinary page
    // deletion changes a citation set), and it can lose its anchor while the input holds
    // still (`vault.locale` renames every heading, which the digest deliberately ignores).
    // Either way `queue count` counts only work, so the drain that would archive the file is
    // skipped, and a suppression that trusted the file's mere presence would pin the page
    // forever with a recorded input nothing can answer — the finding-that-never-clears
    // `SynthesisPolicy` exists to prevent, reached through a third door. So the aliveness
    // question is asked of `queue status`'s own classifier rather than of the bytes on disk.
    let pending =
        super::queue::pending_work_inputs(&rc.root, lk_queue::TargetKind::ConceptSynthesis);
    let owed: Vec<&backlinks::ConceptResynthesis> = sync
        .resynthesize
        .iter()
        .filter(|entry| {
            !pending.contains(&(
                entry.path.to_string_lossy().into_owned(),
                lk_core::concept::citation_digest(&entry.citations),
            ))
        })
        .collect();
    let queue = rc.root.join(".lorekeeper/queue");
    let client = lk_queue::QueueLlmClient::new(queue.clone());
    let mut tasks = Vec::new();
    for entry in owed {
        let task = client.build_task(
            "syn",
            lk_queue::TaskKind::SynthesizeConcept,
            lk_queue::ConceptSynthesisRequest {
                citations: entry.citations.clone(),
                locale: rc.locale,
                related_anchor: entry.related_anchor.clone(),
                target: lk_queue::TaskTarget {
                    vault_path: entry.path.to_string_lossy().into_owned(),
                    kind: lk_queue::TargetKind::ConceptSynthesis,
                    anchor: entry.anchor.clone(),
                },
            },
        );
        task.validate_retained_input()
            .map_err(|error| error.to_string())?;
        tasks.push(task);
    }
    let tasks = if tasks.is_empty() {
        tasks
    } else {
        lk_queue::new_requests(&queue, &tasks).map_err(|error| error.to_string())?
    };
    if !tasks.is_empty() {
        plan.create(
            PathBuf::from(".lorekeeper/queue").join(format!("{}.backlinks.jsonl", client.run_id())),
            lk_queue::serialize_tasks(&tasks).map_err(|error| error.to_string())?,
        );
    }
    Ok((
        plan,
        output::BacklinksSyncReport {
            changed: sync.updated.len(),
            sync,
            queued: tasks.len(),
        },
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = "wiki/concepts/retry.md";

    fn fixture() -> (tempfile::TempDir, ResolvedConfig) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/concepts")).unwrap();
        std::fs::create_dir_all(root.path().join("wiki/documents")).unwrap();
        std::fs::write(root.path().join(TARGET), "---\ntype: concept\ntitle: Retry\nsource_count: 0\nllm_inputs:\n  synthesis: old\n  synthesis_done: old\n---\n## Synthesis\nPrior scoped answer.\n\n## Related\n\n## Sources\n").unwrap();
        std::fs::write(
            root.path().join("wiki/documents/source.md"),
            "---\ntype: document\n---\nRetry three times. [Retry](../concepts/retry.md)\n",
        )
        .unwrap();
        let config = ResolvedConfig {
            root: root.path().into(),
            graph: Default::default(),
            locale: Locale::En,
            vault_dirs: Default::default(),
            concept_categories: vec![],
            llm: Default::default(),
        };
        (root, config)
    }

    fn pages(config: &ResolvedConfig) -> Vec<scan::ScannedPage> {
        scan::scan_vault(&config.root, false).unwrap()
    }

    #[test]
    fn backlinks_queue_contention_preserves_pages_and_preview_is_read_only() {
        let (_root, config) = fixture();
        let before = std::fs::read_to_string(config.root.join(TARGET)).unwrap();
        let preview = synchronize_backlinks(&config, &pages(&config), true, None).unwrap();
        assert_eq!(preview.changed, 1);
        assert_eq!(preview.queued, 0);
        assert!(!config.root.join(".lorekeeper").exists());
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        let _claim =
            super::super::queue::claim_writer(&config.root.join(".lorekeeper/queue"), false)
                .unwrap();
        assert!(synchronize_backlinks(&config, &pages(&config), false, Some(&session)).is_err());
        assert_eq!(
            std::fs::read_to_string(config.root.join(TARGET)).unwrap(),
            before
        );
        let preview = synchronize_backlinks(&config, &pages(&config), true, None).unwrap();
        assert!(preview.sync.dry_run);
        assert!(
            !config
                .root
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
    }

    #[test]
    fn backlinks_queue_path_conflict_refuses_all_page_changes() {
        let (_root, config) = fixture();
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        let _claim =
            super::super::queue::claim_writer(&config.root.join(".lorekeeper/queue"), false)
                .unwrap();
        let before = std::fs::read_to_string(config.root.join(TARGET)).unwrap();
        let (plan, report) = plan_backlinks_publication(
            &config,
            &pages(&config),
            backlinks::SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.queued, 1);
        let serialized = serde_json::to_value(&plan).unwrap();
        let queue_path = serialized["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| {
                step["kind"] == "create"
                    && step["path"]
                        .as_str()
                        .is_some_and(|path| path.ends_with(".jsonl"))
            })
            .unwrap()["path"]
            .as_str()
            .unwrap();
        std::fs::write(config.root.join(queue_path), "occupied queue entry").unwrap();
        assert!(session.apply(plan).is_err());
        assert_eq!(
            std::fs::read_to_string(config.root.join(TARGET)).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read_to_string(config.root.join(queue_path)).unwrap(),
            "occupied queue entry"
        );
        assert!(
            !config
                .root
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
    }

    #[test]
    fn backlinks_page_and_request_recover_together_and_reuse_pending_work() {
        let (_root, config) = fixture();
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        let claim =
            super::super::queue::claim_writer(&config.root.join(".lorekeeper/queue"), false)
                .unwrap();
        let (plan, report) = plan_backlinks_publication(
            &config,
            &pages(&config),
            backlinks::SynthesisPolicy::Record,
        )
        .unwrap();
        assert_eq!(report.changed, 1);
        assert_eq!(report.queued, 1);
        let serialized = serde_json::to_value(plan).unwrap();
        let target_after = serialized["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["path"] == TARGET)
            .unwrap()["after"]
            .as_str()
            .unwrap();
        std::fs::write(config.root.join(TARGET), target_after).unwrap();
        let journal = serde_json::json!({"version":1,"plan":serialized});
        std::fs::write(
            config.root.join(".lorekeeper/mutation/pending.json"),
            journal.to_string(),
        )
        .unwrap();
        drop(claim);
        drop(session);
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        let hash = lk_core::concept::citation_digest(&report.sync.resynthesize[0].citations);
        let task = super::super::queue::current_synthesis_request(&config.root, TARGET, &hash)
            .unwrap()
            .unwrap();
        task.validate_retained_input().unwrap();
        assert_eq!(
            task.input["citations"],
            serde_json::to_value(&report.sync.resynthesize[0].citations).unwrap()
        );
        let repeated =
            synchronize_backlinks(&config, &pages(&config), false, Some(&session)).unwrap();
        assert_eq!(repeated.changed, 0);
        assert_eq!(repeated.queued, 0);
        let same = super::super::queue::current_synthesis_request(&config.root, TARGET, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(task.task_id, same.task_id);
        assert!(
            !config
                .root
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
    }

    #[test]
    fn backlinks_noop_provider_does_not_require_the_queue_claim() {
        let (_root, mut config) = fixture();
        config.llm.provider = lk_core::config::LlmProvider::Noop;
        let session = lk_vault::MutationSession::acquire(&config.root).unwrap();
        let _claim =
            super::super::queue::claim_writer(&config.root.join(".lorekeeper/queue"), false)
                .unwrap();
        let result =
            synchronize_backlinks(&config, &pages(&config), false, Some(&session)).unwrap();
        assert_eq!(result.changed, 1);
        assert_eq!(result.queued, 0);
        let concept = std::fs::read_to_string(config.root.join(TARGET)).unwrap();
        assert!(concept.contains("source_count: 1"));
        assert!(concept.contains("synthesis: old"));
    }
}
