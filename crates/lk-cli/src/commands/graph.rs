use std::path::{Path, PathBuf};

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
    // `backlinks-sync` and the audit/merge commands need locale from the full config —
    // dispatch them up front before paying the graph-build cost.
    match cmd {
        GraphCommand::AuditCandidates => {
            return run_audit_candidates(opts, root_override, json);
        }
        GraphCommand::AuditMark { ref slug } => {
            return run_audit_mark(opts, root_override, json, slug);
        }
        GraphCommand::BacklinksSync { dry_run } => {
            return run_backlinks(opts, root_override, json, dry_run);
        }
        GraphCommand::Merge {
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

    // The SINGLE definition of what this command reads. Integrity commands resolve links
    // against a whole-vault existence universe; analysis commands read `scope.dirs` only.
    let integrity = matches!(
        cmd,
        GraphCommand::Lint
            | GraphCommand::Broken
            | GraphCommand::Orphans
            | GraphCommand::IndexSync { .. }
    );
    let scan_dirs = command_scan_dirs(integrity, &rc.graph.scope.dirs);

    // ONE scan, partitioned into three views that answer three different questions. Conflating
    // any two of them reports something false:
    //
    // - `scanned` — every page in the vault. The existence universe: "is a file addressed by
    //   this id" is exact only over all of them.
    // - `link_sources` — the pages this tool writes and manages, `scope.dirs` ∪ the page dirs.
    //   A user's own note is not the pipeline's output and not its to repair, so a stray link
    //   inside one is not a violation of the vault's contract.
    // - `pages` — the analysis scope, the graph's nodes and edges.
    //
    // `scope.exclude` narrows the last two, never the first: an excluded page still exists.
    let mut scan_cfg = rc.graph.clone();
    scan_cfg.scope.dirs = scan_dirs;
    if integrity {
        scan_cfg.scope.exclude = Vec::new();
    }
    let scanned = scan::scan_vault(&rc.root, &scan_cfg).map_err(|e| format!("{e}"))?;
    let existence = scan::VaultExistence::build(&scanned, &rc.vault_dirs);
    let excluded = scan::Excludes::compile(&rc.graph.scope.exclude).map_err(|e| format!("{e}"))?;
    let in_scope = |page: &scan::ScannedPage, dirs: &[PathBuf]| {
        dirs.iter().any(|d| page.path.starts_with(d)) && !excluded.matches(&page.path)
    };
    let managed_dirs = command_source_dirs(&rc.graph.scope.dirs, &rc.root, &rc.vault_dirs);
    let link_sources: Vec<scan::ScannedPage> = scanned
        .iter()
        .filter(|p| in_scope(p, &managed_dirs))
        .cloned()
        .collect();
    let pages: Vec<scan::ScannedPage> = scanned
        .iter()
        .filter(|p| in_scope(p, &rc.graph.scope.dirs))
        .cloned()
        .collect();
    let g = graph::WikiGraph::build_with_existence(&pages, &existence, &rc.vault_dirs);

    let violated = match cmd {
        GraphCommand::Lint => {
            let hubs = g.hubs(10, rc.graph.metrics.min_hub_degree);
            let orphans = g.orphans(&rc.graph.metrics.orphan_exclude);
            let broken = graph::broken_links(&link_sources, &existence, &rc.vault_dirs);
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
            let broken = graph::broken_links(&link_sources, &existence, &rc.vault_dirs);
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
                    index_drift::fix(&drift, &pages, &rc.root, Path::new(&rc.vault_dirs.wiki))
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
            has && (fixed.is_none() || has_unfixable)
        }
        GraphCommand::Normalize { fix } => {
            // Rename candidates come from the analysis scope: only the wiki's own pages
            // are addressed by slug. A daily or synthesis page's filename is a DATE
            // (`2026-W30`), which slugifying would lowercase into a path the pipeline
            // does not write.
            let renames = normalize::scan(&pages);
            let has = !renames.is_empty();
            let applied = if fix && has {
                // Citations of a renamed page live anywhere in the vault — a daily page
                // is the usual case — so the rewrite reads every page dir, the same
                // full-vault view `merge` uses. Rewriting only the rename scope would
                // leave those citations pointing at a file that no longer exists, and
                // `graph broken` matches at the id level so it would not report them.
                //
                // The analysis scope is UNIONed in rather than assumed to sit inside those
                // dirs: `graph.scope.dirs` is validated only as a relative in-vault path,
                // so it can name a directory outside the standard four — and a page renamed
                // out of one whose citations were never visited is the very defect this
                // rewrite exists to prevent.
                let mut cfg = rc.graph.clone();
                cfg.scope.dirs = vault_page_dirs(&rc.root, &rc.vault_dirs);
                for dir in &rc.graph.scope.dirs {
                    if !cfg.scope.dirs.contains(dir) {
                        cfg.scope.dirs.push(dir.clone());
                    }
                }
                let everywhere = scan::scan_vault(&rc.root, &cfg).map_err(|e| format!("{e}"))?;
                Some(
                    normalize::apply(&renames, &everywhere, &rc.root)
                        .map_err(|e| format!("{e}"))?,
                )
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
        // Dispatched at the top of `run_inner` because they need full-vault scope
        // and/or config that doesn't touch the in-scope WikiGraph.
        GraphCommand::AuditCandidates
        | GraphCommand::AuditMark { .. }
        | GraphCommand::BacklinksSync { .. }
        | GraphCommand::Merge { .. } => {
            unreachable!()
        }
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

fn run_backlinks(
    opts: &GlobalOptions,
    root_override: Option<PathBuf>,
    json: bool,
    dry_run: bool,
) -> Result<bool, String> {
    let mut rc = resolve_config_full(opts, root_override)?;

    rc.graph.scope.dirs = vault_page_dirs(&rc.root, &rc.vault_dirs);

    let pages = scan::scan_vault(&rc.root, &rc.graph).map_err(|e| format!("{e}"))?;

    let sync =
        backlinks::sync_concept_backlinks(&pages, &rc.root, rc.locale, dry_run, &rc.vault_dirs)
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

    Ok(violated)
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
    // A successful merge leaves nothing contradicted — exit 0.
    Ok(false)
}

/// The directories a graph command reads. Analysis commands (hubs/cluster/…) read `scope.dirs`
/// only — that narrowing is the whole point of the setting. Integrity commands
/// (lint/broken/orphans/index-sync) read the VAULT ROOT, because they answer "does a page exist
/// at this id", and that question is exact only over every page there is. Anything narrower
/// makes "not scanned" indistinguishable from "not there", in whichever direction the narrowing
/// resolves it. `scan_vault` skips dot-directories, so `.trash` never resolves.
/// The pages whose links are CHECKED: the analysis scope plus every page directory this tool
/// writes. A vault also holds the user's own folders, and a stray link in a hand-written note is
/// neither the pipeline's output nor something it can repair — reporting one as a violation gates
/// the scheduled pipeline on content the tool does not manage.
fn command_source_dirs(scope_dirs: &[PathBuf], root: &Path, dirs: &VaultDirs) -> Vec<PathBuf> {
    let mut sources = scope_dirs.to_vec();
    for dir in vault_page_dirs(root, dirs) {
        if !sources.contains(&dir) {
            sources.push(dir);
        }
    }
    sources
}

fn command_scan_dirs(integrity: bool, scope_dirs: &[PathBuf]) -> Vec<PathBuf> {
    if integrity {
        // The vault root: an empty relative path joins to `root` itself.
        return vec![PathBuf::new()];
    }
    scope_dirs.to_vec()
}

/// Every vault-relative page directory that exists on disk — anything that can
/// link another page. Used by commands that need a full-vault view
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

#[cfg(test)]
mod tests {
    use super::command_scan_dirs;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn analysis_commands_read_scope_only() {
        // The narrowing is the whole point of `graph.scope.dirs` for hubs/cluster/suggest-links.
        assert_eq!(command_scan_dirs(false, &[p("wiki")]), vec![p("wiki")]);
        assert_eq!(
            command_scan_dirs(false, &[p("wiki"), p("docs")]),
            vec![p("wiki"), p("docs")]
        );
    }

    #[test]
    fn integrity_commands_read_the_whole_vault_whatever_the_scope() {
        // An empty relative path joins to the vault root.
        for scope in [vec![p("wiki")], vec![p("wiki/concepts")], vec![]] {
            assert_eq!(command_scan_dirs(true, &scope), vec![PathBuf::new()]);
        }
    }
}
