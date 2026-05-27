use std::path::{Path, PathBuf};

use lk_core::config::{GraphConfig, VaultDirs};
use lk_core::i18n::Locale;
use lk_graph::{
    backlinks, cache, cluster, export, graph, index, normalize, output, relations, scan, stale,
};

use super::GlobalOpts;

struct ResolvedConfig {
    root: PathBuf,
    graph: GraphConfig,
    tz: jiff::tz::TimeZone,
    locale: Locale,
    vault_dirs: VaultDirs,
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
    /// Report pages whose `updated`/`created` frontmatter is older than N days
    Stale {
        /// Threshold in days (default: 90)
        #[arg(long, default_value_t = 90)]
        days: u32,
    },
    /// Rewrite each concept page's `## <Sources>` section from the graph
    BacklinksSync {
        /// Report what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Rewrite each concept page's `## <Related>` section from community detection
    RelationsSync {
        /// Report what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
}

/// Returns exit code: 0 = ok/no findings, 1 = findings, 2 = runtime error.
pub fn run(
    opts: &GlobalOpts,
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
    opts: &GlobalOpts,
    cmd: GraphCmd,
    json: bool,
    root_override: Option<PathBuf>,
    incremental: bool,
) -> Result<bool, String> {
    // `stale`, `backlinks-sync`, and `relations-sync` need timezone/locale from
    // the full config — dispatch them up front before paying the graph-build cost.
    match cmd {
        GraphCmd::Stale { days } => {
            return run_stale(opts, root_override, json, days, incremental);
        }
        GraphCmd::BacklinksSync { dry_run } => {
            return run_backlinks_sync(opts, root_override, json, dry_run, incremental);
        }
        GraphCmd::RelationsSync { dry_run } => {
            return run_relations_sync(opts, root_override, json, dry_run, incremental);
        }
        _ => {}
    }

    let mut rc = resolve_config_full(opts, root_override)?;

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

    let existence = if matches!(
        cmd,
        GraphCmd::Lint | GraphCmd::Broken | GraphCmd::Orphans | GraphCmd::IndexSync { .. }
    ) {
        build_vault_existence(&rc.root, &rc.graph, &rc.vault_dirs)?
    } else {
        scan::VaultExistence::from_pages(&pages)
    };
    let g = graph::WikiGraph::build_with_existence(&pages, &existence);

    let has_findings = match cmd {
        GraphCmd::Lint => {
            let hubs = g.hubs(10, rc.graph.graph.min_hub_degree);
            let orphans = g.orphans(
                &rc.graph.graph.orphan_exclude,
                Path::new(&rc.vault_dirs.wiki),
            );
            let broken = g.broken_links().to_vec();
            let drift = index::diff(
                &g,
                &existence,
                &rc.root,
                Path::new(&rc.vault_dirs.wiki),
                &rc.graph.graph.orphan_exclude,
            );

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
                &rc.graph.graph.orphan_exclude,
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
            let drift = index::diff(
                &g,
                &existence,
                &rc.root,
                Path::new(&rc.vault_dirs.wiki),
                &rc.graph.graph.orphan_exclude,
            );
            let has = !drift.is_in_sync();
            let has_unfixable = !drift.missing_from_disk.is_empty();
            let fixed = if fix && !drift.missing_from_index.is_empty() {
                Some(
                    index::fix(&drift, &rc.root, Path::new(&rc.vault_dirs.wiki))
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
                    .map(|r| output::RenameEntry {
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
            let result = cluster::suggest_links(&g, &clusters);
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
        // Dispatched at the top of `run_inner` because they need timezone/locale
        // from the full config and don't touch the WikiGraph.
        GraphCmd::Stale { .. }
        | GraphCmd::BacklinksSync { .. }
        | GraphCmd::RelationsSync { .. } => unreachable!(),
    };

    // Persist the mtime cache after a successful scan so the next `--incremental`
    // run can skip the rescan if nothing changed.
    if incremental {
        save_cache_best_effort(&rc.root, &rc.graph);
    }

    Ok(has_findings)
}

fn run_stale(
    opts: &GlobalOpts,
    root_override: Option<PathBuf>,
    json: bool,
    days: u32,
    incremental: bool,
) -> Result<bool, String> {
    let rc = resolve_config_full(opts, root_override)?;

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

    let today = jiff::Timestamp::now().to_zoned(rc.tz).date();

    let stale_pages = stale::find_stale(&pages, &rc.root, today, days, &rc.vault_dirs)
        .map_err(|e| format!("{e}"))?;
    let count = stale_pages.len();
    let report = output::StaleReport {
        threshold_days: days,
        stale: stale_pages,
        count,
    };

    if json {
        output::print_json(&report)?;
    } else {
        output::print_stale(&report, &rc.vault_dirs);
    }

    if incremental {
        save_cache_best_effort(&rc.root, &rc.graph);
    }

    Ok(count > 0)
}

fn run_backlinks_sync(
    opts: &GlobalOpts,
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
        output::print_backlinks_sync(&report);
    }

    if incremental && !dry_run {
        save_cache_best_effort(&rc.root, &rc.graph);
    }

    Ok(false)
}

fn run_relations_sync(
    opts: &GlobalOpts,
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

    let sync = relations::sync_concept_relations(
        &pages,
        &rc.root,
        rc.locale,
        dry_run,
        &rc.vault_dirs,
        &rc.graph,
    )
    .map_err(|e| format!("{e}"))?;
    let changed = sync.updated.len();
    let report = output::RelationsSyncReport { sync, changed };

    if json {
        output::print_json(&report)?;
    } else {
        output::print_relations_sync(&report);
    }

    if incremental && !dry_run {
        save_cache_best_effort(&rc.root, &rc.graph);
    }

    Ok(false)
}

/// Best-effort cache save: build a fresh mtime snapshot and persist it. Warnings
/// are printed to stderr but errors are not propagated — a failed cache save must
/// not turn a successful graph command into a failure.
fn save_cache_best_effort(root: &std::path::Path, config: &GraphConfig) {
    if let Ok(fresh) = cache::build(root, &config.scope.dirs, config.scope.follow_links) {
        let cp = cache::cache_path(root);
        if let Err(e) = cache::save(&cp, &fresh) {
            eprintln!("warning: failed to save graph cache: {e}");
        }
    }
}

/// Resolve vault root, graph config, timezone, locale, and directory layout.
/// A `--root` override uses system timezone + default locale + default dirs.
/// Scan the full vault into a [`scan::VaultExistence`] for integrity checks.
/// Reuses the analysis `config` (exclude globs, follow_links) but widens the
/// scope to every page directory, so broken-link and orphan detection reason
/// about pages outside `graph.scope.dirs`.
fn build_vault_existence(
    root: &std::path::Path,
    config: &GraphConfig,
    vault_dirs: &VaultDirs,
) -> Result<scan::VaultExistence, String> {
    let mut full = config.clone();
    full.scope.dirs = vault_page_dirs(root, vault_dirs);
    let pages = scan::scan_vault(root, &full).map_err(|e| format!("{e}"))?;
    Ok(scan::VaultExistence::from_pages(&pages))
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
    opts: &GlobalOpts,
    root_override: Option<PathBuf>,
) -> Result<ResolvedConfig, String> {
    if let Some(root) = root_override {
        let vault_dirs = VaultDirs::default();
        let mut graph = GraphConfig::default();
        graph.apply_vault_defaults(&vault_dirs);
        return Ok(ResolvedConfig {
            root,
            graph,
            tz: jiff::tz::TimeZone::system(),
            locale: Locale::default(),
            vault_dirs,
        });
    }

    let config_path = super::find_config(opts).map_err(|e| format!("{e}"))?;
    let config = super::load_config(&config_path).map_err(|e| format!("{e}"))?;
    Ok(ResolvedConfig {
        root: config.vault.root_path(),
        tz: config.vault.timezone(),
        locale: config.vault.locale(),
        vault_dirs: config.vault.dirs.clone(),
        graph: config.graph,
    })
}
