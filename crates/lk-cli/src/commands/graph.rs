use std::path::PathBuf;

use lk_core::config::GraphConfig;
use lk_core::i18n::Locale;
use lk_graph::{backlinks, cluster, export, graph, index, normalize, output, scan, stale};

use super::GlobalOpts;

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
    /// Rewrite each `wiki/concepts/*` page's `## <Sources>` section from the graph
    BacklinksSync {
        /// Report what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
}

/// Returns exit code: 0 = ok/no findings, 1 = findings, 2 = runtime error.
pub fn run(opts: &GlobalOpts, cmd: GraphCmd, json: bool, root_override: Option<PathBuf>) -> i32 {
    match run_inner(opts, cmd, json, root_override) {
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
) -> Result<bool, String> {
    // `stale` and `backlinks-sync` need timezone/locale from the full config —
    // and don't use the WikiGraph at all — so dispatch them up front before paying
    // the graph-build cost.
    match cmd {
        GraphCmd::Stale { days } => return run_stale(opts, root_override, json, days),
        GraphCmd::BacklinksSync { dry_run } => {
            return run_backlinks_sync(opts, root_override, json, dry_run);
        }
        _ => {}
    }

    let (root, mut config) = resolve_config(opts, root_override)?;

    let pages = scan::scan_vault(&root, &config).map_err(|e| format!("{e}"))?;
    let g = graph::WikiGraph::build(&pages);

    let has_findings = match cmd {
        GraphCmd::Lint => {
            let hubs = g.hubs(10, config.graph.min_hub_degree);
            let orphans = g.orphans(&config);
            let broken = g.broken_links().to_vec();
            let drift = index::diff(&g, &root, &config);

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
            let orphans = g.orphans(&config);
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
                config.cluster.min_community_size = size;
            }
            let mut result = cluster::detect_communities(&g, &config);
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
                Some(cluster::detect_communities(&g, &config))
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
            let drift = index::diff(&g, &root, &config);
            let has = !drift.is_in_sync();
            let has_unfixable = !drift.missing_from_disk.is_empty();
            let fixed = if fix && !drift.missing_from_index.is_empty() {
                Some(index::fix(&drift, &root, &config).map_err(|e| format!("{e}"))?)
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
                Some(normalize::apply(&renames, &pages, &root).map_err(|e| format!("{e}"))?)
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
                config.cluster.min_community_size = size;
            }
            let clusters = cluster::detect_communities(&g, &config);
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
        GraphCmd::Stale { .. } | GraphCmd::BacklinksSync { .. } => unreachable!(),
    };

    Ok(has_findings)
}

fn run_stale(
    opts: &GlobalOpts,
    root_override: Option<PathBuf>,
    json: bool,
    days: u32,
) -> Result<bool, String> {
    let (root, config, tz, _locale) = resolve_config_full(opts, root_override)?;
    let pages = scan::scan_vault(&root, &config).map_err(|e| format!("{e}"))?;

    // Same date derivation as the ingest pipeline (`commands::ingest`): now in the
    // vault's timezone, then the calendar date. UTC-by-accident gives the wrong
    // answer near midnight.
    let today = jiff::Timestamp::now().to_zoned(tz).date();

    let stale_pages = stale::find_stale(&pages, &root, today, days).map_err(|e| format!("{e}"))?;
    let count = stale_pages.len();
    let report = output::StaleReport {
        threshold_days: days,
        stale: stale_pages,
        count,
    };

    if json {
        output::print_json(&report)?;
    } else {
        output::print_stale(&report);
    }
    Ok(count > 0)
}

fn run_backlinks_sync(
    opts: &GlobalOpts,
    root_override: Option<PathBuf>,
    json: bool,
    dry_run: bool,
) -> Result<bool, String> {
    let (root, mut config, _tz, locale) = resolve_config_full(opts, root_override)?;

    // Backlinks must see every page that can wikilink a concept — not just `wiki/`.
    // Daily ingest output and personal pages reference concepts too; if the scope
    // excludes them, the diff against existing sources becomes a destructive churn
    // that removes correct daily/me backlinks on every run.
    config.scope.dirs = vault_page_dirs(&root);

    let pages = scan::scan_vault(&root, &config).map_err(|e| format!("{e}"))?;

    let sync = backlinks::sync_concept_backlinks(&pages, &root, locale, dry_run)
        .map_err(|e| format!("{e}"))?;
    let changed = sync.updated.len();
    let report = output::BacklinksSyncReport { sync, changed };

    if json {
        output::print_json(&report)?;
    } else {
        output::print_backlinks_sync(&report);
    }
    // Per spec: `backlinks-sync` exits 0 even when it makes changes — a change is
    // a successful normalisation, not a finding to escalate.
    Ok(false)
}

/// Resolve vault root and graph config. If `--root` is given, uses that and default
/// config. Otherwise loads `config.yaml` and reads `vault.root` + `graph:`.
fn resolve_config(
    opts: &GlobalOpts,
    root_override: Option<PathBuf>,
) -> Result<(PathBuf, GraphConfig), String> {
    if let Some(root) = root_override {
        return Ok((root, GraphConfig::default()));
    }

    let config_path = super::find_config(opts).map_err(|e| format!("{e}"))?;
    let config = super::load_config(&config_path).map_err(|e| format!("{e}"))?;
    let root = config.vault.root_path();
    Ok((root, config.graph))
}

/// Like [`resolve_config`] but also returns the configured timezone and locale.
/// `stale` needs the timezone to compute "today" consistently with the rest of
/// the pipeline (`Event::date` is derived the same way), and `backlinks-sync`
/// needs the locale to pick the `## <Sources>` heading text. A `--root` override
/// gets system timezone + default locale (Ko), since no config is loaded.
/// Every vault-relative page directory that exists on disk — anything that can
/// wikilink another page belongs in the scope. Used by commands that need a full
/// backlink view (not the user-configured `graph.scope.dirs`, which is
/// intentionally narrowed for analysis like `lint`/`cluster`). Missing
/// directories are skipped so a partially-populated vault doesn't error out.
fn vault_page_dirs(root: &std::path::Path) -> Vec<PathBuf> {
    [
        "wiki",
        "daily",
        "me",
        "weekly",
        "monthly",
        "quarterly",
        "annually",
    ]
    .iter()
    .filter(|name| root.join(name).is_dir())
    .map(PathBuf::from)
    .collect()
}

fn resolve_config_full(
    opts: &GlobalOpts,
    root_override: Option<PathBuf>,
) -> Result<(PathBuf, GraphConfig, jiff::tz::TimeZone, Locale), String> {
    if let Some(root) = root_override {
        return Ok((
            root,
            GraphConfig::default(),
            jiff::tz::TimeZone::system(),
            Locale::default(),
        ));
    }

    let config_path = super::find_config(opts).map_err(|e| format!("{e}"))?;
    let config = super::load_config(&config_path).map_err(|e| format!("{e}"))?;
    let root = config.vault.root_path();
    let tz = config.vault.timezone();
    let locale = config.vault.locale();
    Ok((root, config.graph, tz, locale))
}
