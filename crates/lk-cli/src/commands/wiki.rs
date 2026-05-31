//! `lore wiki <subcommand>` — vault-wide markdown utilities that live outside the
//! ingest/synthesis hot path.

use std::path::PathBuf;

use lk_core::frontmatter;
use lk_core::i18n::Locale;

use super::{find_config, load_config};

#[derive(clap::Subcommand)]
pub enum WikiCmd {
    /// Generate `<vault>/<wiki>/index.md` — hierarchical catalog of every vault page
    Index {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Generate `<vault>/<wiki>/log.md` — reverse-chronological knowledge timeline
    /// (when each concept/document/exploration entered or was last refreshed)
    Log {
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

pub async fn run(opts: &super::GlobalOptions, cmd: WikiCmd) -> miette::Result<()> {
    match cmd {
        WikiCmd::Index { root } => run_index(opts, root).await,
        WikiCmd::Log { root } => run_log(opts, root).await,
        WikiCmd::Concepts { json } => run_concepts(opts, json).await,
    }
}

pub async fn run_log(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    let (vault_root, _locale, _threshold, dirs) =
        resolve_vault_with_threshold(opts, root_override)?;

    tracing::info!(vault = %vault_root.display(), "building wiki knowledge log");

    let path = lk_vault::write_timeline(&vault_root, &dirs)
        .map_err(|e| miette::miette!("write log.md: {e}"))?;

    eprintln!("Wrote {}", path.display());
    Ok(())
}

pub async fn run_index(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    let (vault_root, locale, threshold, dirs) = resolve_vault_with_threshold(opts, root_override)?;

    tracing::info!(vault = %vault_root.display(), locale = ?locale, "building wiki index");

    let path = lk_vault::write_index(&vault_root, locale, threshold, &dirs)
        .await
        .map_err(|e| miette::miette!("write index.md: {e}"))?;

    eprintln!("Wrote {}", path.display());
    Ok(())
}

fn resolve_vault_with_threshold(
    opts: &super::GlobalOptions,
    root_override: Option<PathBuf>,
) -> miette::Result<(PathBuf, Locale, usize, lk_core::config::VaultDirs)> {
    match root_override {
        Some(r) => {
            let (locale, threshold, dirs) = match find_config(opts).and_then(|p| load_config(&p)) {
                Ok(config) => (
                    config.vault.locale(),
                    config.concepts.index_split_threshold,
                    config.vault.dirs.clone(),
                ),
                Err(_) => (
                    Locale::default(),
                    100,
                    lk_core::config::VaultDirs::default(),
                ),
            };
            Ok((r, locale, threshold, dirs))
        }
        None => {
            let path = find_config(opts)?;
            let config = load_config(&path)?;
            Ok((
                config.vault.root_path(),
                config.vault.locale(),
                config.concepts.index_split_threshold,
                config.vault.dirs.clone(),
            ))
        }
    }
}

async fn run_concepts(opts: &super::GlobalOptions, json: bool) -> miette::Result<()> {
    let config_path = find_config(opts)?;
    let config = load_config(&config_path)?;
    let vault_root = config.vault.root_path();
    let concept_dir = vault_root.join(&config.vault.dirs.wiki).join("concepts");

    let mut entries: Vec<ConceptEntry> = Vec::new();

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
            Err(_) => continue,
        };
        let page = match frontmatter::parse_page(&content) {
            Ok(p) => p,
            Err(_) => continue,
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
        let source_count = page
            .frontmatter
            .get("source_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if slug.is_empty() {
            continue;
        }
        entries.push(ConceptEntry {
            slug,
            title,
            category,
            source_count,
        });
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
                })
            })
            .collect();
        let out = serde_json::to_string_pretty(&arr).map_err(|e| miette::miette!("JSON: {e}"))?;
        println!("{out}");
    } else {
        println!("# {} concepts", entries.len());
        for e in &entries {
            println!(
                "{}\t{}\t{}\t{}",
                e.slug, e.title, e.category, e.source_count
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
}
