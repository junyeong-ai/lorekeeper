//! `lore wiki <subcommand>` — vault-wide markdown utilities that live outside the
//! ingest/synthesis hot path.

use std::path::PathBuf;

use lk_core::frontmatter;
use lk_core::i18n::Locale;

use super::{find_config, load_config};

#[derive(clap::Subcommand)]
pub enum WikiCommand {
    /// Generate `<vault>/<wiki>/index.md` — hierarchical catalog of every vault page
    Index {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Generate `<vault>/<wiki>/log.md` — reverse-chronological knowledge timeline
    /// (when each concept/document/exploration first entered the vault, by `created`)
    Log {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Generate `<vault>/<wiki>/map.md` — a navigable knowledge map: concepts grouped by
    /// citation cluster (Louvain), hub-first, for agent/human traversal without embeddings
    Map {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Regenerate every page derived from the vault: the catalog, the timeline and the map
    Refresh {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// List all concept pages in the vault
    Concepts {
        /// Output as JSON array
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(opts: &super::GlobalOptions, cmd: WikiCommand) -> miette::Result<()> {
    match cmd {
        WikiCommand::Index { root } => run_index(opts, root).await,
        WikiCommand::Log { root } => run_log(opts, root).await,
        WikiCommand::Map { root } => run_map(opts, root).await,
        WikiCommand::Refresh { root } => run_refresh(opts, root).await,
        WikiCommand::Concepts { json } => run_concepts(opts, json).await,
    }
}

/// Regenerate every page derived from the vault's contents, in one call.
///
/// Each of these is a materialized view, true only while something re-derives it, and every
/// caller that adds pages has to re-derive all of them. One command is what makes naming a subset
/// impossible: a caller cannot refresh two of three, and a page added to the set reaches every
/// caller without any of them changing.
///
/// Every view is attempted even when an earlier one fails, and the failures are reported
/// together. The views are independent — one failing says nothing about the others — and the
/// scheduled pipeline is deliberately not `set -e` for the same reason: a stage that cannot run
/// must not decide for the stages after it.
///
/// `lore schema` stays separate: `AGENTS.md` derives from config, not from the vault, so a run
/// that only added pages has nothing to refresh there.
pub async fn run_refresh(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    use lk_core::vault_path::{INDEX_FILE, MAP_FILE, TIMELINE_FILE};

    let mut failed: Vec<String> = Vec::new();
    for (page, outcome) in [
        (INDEX_FILE, run_index(opts, root_override.clone()).await),
        (TIMELINE_FILE, run_log(opts, root_override.clone()).await),
        (MAP_FILE, run_map(opts, root_override).await),
    ] {
        if let Err(error) = outcome {
            eprintln!("✗ {page}: {error}");
            failed.push(page.to_string());
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    Err(miette::miette!(
        "{} of the vault's derived pages could not be written: {}",
        failed.len(),
        failed.join(", ")
    ))
}

pub async fn run_map(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    let (vault_root, locale, dirs, graph_config) = resolve_wiki_context(opts, root_override)?;

    tracing::info!(vault = %vault_root.display(), "building wiki knowledge map");

    let pages = lk_graph::scan::scan_vault(&vault_root, &graph_config)
        .map_err(|e| miette::miette!("scan vault: {e}"))?;
    let graph = lk_graph::graph::WikiGraph::build(&pages, &dirs);
    let content = lk_graph::map::build_map(&graph, &graph_config, &dirs, locale);

    let rel = std::path::Path::new(&dirs.wiki).join(lk_core::vault_path::MAP_FILE);
    lk_vault::VaultWriter::new(&vault_root)
        .write_page(&rel, &content)
        .await
        .map_err(|e| miette::miette!("write map.md: {e}"))?;

    eprintln!("Wrote {}", vault_root.join(&rel).display());
    Ok(())
}

pub async fn run_log(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    let (vault_root, locale, dirs, _graph) = resolve_wiki_context(opts, root_override)?;

    tracing::info!(vault = %vault_root.display(), "building wiki knowledge log");

    let path = lk_vault::write_timeline(&vault_root, locale, &dirs)
        .map_err(|e| miette::miette!("write log.md: {e}"))?;

    eprintln!("Wrote {}", path.display());
    Ok(())
}

pub async fn run_index(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    let (vault_root, locale, dirs, _graph) = resolve_wiki_context(opts, root_override)?;

    tracing::info!(vault = %vault_root.display(), locale = ?locale, "building wiki index");

    let path = lk_vault::write_index(&vault_root, locale, &dirs)
        .await
        .map_err(|e| miette::miette!("write index.md: {e}"))?;

    eprintln!("Wrote {}", path.display());
    Ok(())
}

/// Resolve the context every `lore wiki` page builder needs — vault root, locale, vault
/// dirs, and the graph config (scope, with vault defaults applied). The single resolution
/// path for `index`/`log`/`map`, so they behave identically: an explicit `--root` runs even
/// without a config file (defaults fill in locale/dirs/graph), while the config-derived path
/// surfaces a load error loudly.
fn resolve_wiki_context(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<(
    PathBuf,
    Locale,
    lk_core::config::VaultDirs,
    lk_core::config::GraphConfig,
)> {
    let super::RootConfig { root, config } = super::resolve_root_config(opts, root_override)?;
    match config {
        Some(config) => Ok((
            root,
            config.vault.locale(),
            config.vault.dirs.clone(),
            config.graph.clone(),
        )),
        // No config file at all (binary-only use under `--root`): defaults fill in.
        None => {
            let dirs = lk_core::config::VaultDirs::default();
            let mut graph = lk_core::config::GraphConfig::default();
            graph.apply_vault_defaults(&dirs);
            Ok((root, Locale::default(), dirs, graph))
        }
    }
}

async fn run_concepts(opts: &super::GlobalOptions, json: bool) -> miette::Result<()> {
    let config_path = find_config(opts)?;
    let config = load_config(&config_path)?;
    let vault_root = config.vault.root_path();
    let concept_dir = vault_root.join(lk_core::vault_path::concepts_dir(&config.vault.dirs));

    let mut entries: Vec<ConceptEntry> = Vec::new();
    // A page this cannot read is a concept the caller will not see. `/lore-process` loads this as
    // its dedup baseline, so an answer that silently omits one has the drain mint a second page
    // for a concept the vault already holds — or overwrite the one it could not read. Collected
    // and refused at the end rather than warned about: a warning on stderr leaves a JSON array
    // that looks complete.
    let mut unreadable: Vec<String> = Vec::new();

    let dir_exists = tokio::fs::metadata(&concept_dir)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);

    if !dir_exists {
        if json {
            println!("[]");
        } else {
            println!("# 0 concepts");
        }
        return Ok(());
    }

    let mut dir = tokio::fs::read_dir(&concept_dir)
        .await
        .map_err(|e| miette::miette!("read {}: {e}", concept_dir.display()))?;

    while let Some(entry) = dir.next_entry().await.map_err(|e| miette::miette!("{e}"))? {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                unreadable.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let page = match frontmatter::parse_page(&content) {
            Ok(p) => p,
            Err(e) => {
                unreadable.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let slug = page
            .frontmatter
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let title = page
            .frontmatter
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let category = page
            .frontmatter
            .get("category")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
            .to_string();
        let source_count = page.frontmatter.source_count().unwrap_or(0);
        // Registered synonyms beyond the title, so when `/lore-process` loads the vault's
        // concept registry at run-start it can match an alias-only surface form to this
        // concept instead of forking a duplicate page. (The title is already carried in `title`.)
        let aliases = page
            .frontmatter
            .get("aliases")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| *s != title)
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if slug.is_empty() {
            continue;
        }
        entries.push(ConceptEntry {
            slug,
            title,
            category,
            source_count,
            aliases,
        });
    }

    if !unreadable.is_empty() {
        return Err(miette::miette!(
            "{} concept page(s) could not be read, so this registry is incomplete and dedup \
             against it would mint a second page for a concept the vault already holds:\n  {}",
            unreadable.len(),
            unreadable.join("\n  ")
        ));
    }

    entries.sort_by(|a, b| a.slug.cmp(&b.slug));

    if json {
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "slug": e.slug,
                    "title": e.title,
                    "category": e.category,
                    "source_count": e.source_count,
                    "aliases": e.aliases,
                })
            })
            .collect();
        let out = serde_json::to_string_pretty(&arr).map_err(|e| miette::miette!("JSON: {e}"))?;
        println!("{out}");
    } else {
        println!("# {} concepts", entries.len());
        for e in &entries {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                e.slug,
                e.title,
                e.category,
                e.source_count,
                e.aliases.join(", ")
            );
        }
    }

    Ok(())
}

struct ConceptEntry {
    slug: String,
    title: String,
    category: String,
    source_count: u64,
    aliases: Vec<String>,
}
