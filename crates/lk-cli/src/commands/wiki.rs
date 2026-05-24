//! `lore wiki <subcommand>` — vault-wide markdown utilities that live outside the
//! ingest/synthesis hot path. Currently hosts `lore wiki index`, which catalogs every
//! Lorekeeper-managed page into `wiki/index.md`.

use std::path::PathBuf;

use lk_core::i18n::Locale;

use super::{find_config, load_config};

#[derive(clap::Subcommand)]
pub enum WikiCmd {
    /// Generate `<vault>/wiki/index.md` — hierarchical catalog of every vault page
    Index {
        /// Vault root override (default: vault.root from config)
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

pub async fn run(opts: &super::GlobalOpts, cmd: WikiCmd) -> miette::Result<()> {
    match cmd {
        WikiCmd::Index { root } => run_index(opts, root).await,
    }
}

pub async fn run_index(
    opts: &super::GlobalOpts,
    root_override: Option<PathBuf>,
) -> miette::Result<()> {
    let (vault_root, locale) = resolve_vault(opts, root_override)?;

    tracing::info!(vault = %vault_root.display(), locale = ?locale, "building wiki index");

    let path = lk_vault::write_index(&vault_root, locale)
        .await
        .map_err(|e| miette::miette!("write wiki/index.md: {e}"))?;

    eprintln!("Wrote {}", path.display());
    Ok(())
}

/// Resolve the vault root + output locale. `--root` skips config loading and falls back
/// to the default locale (Ko) if the config can't be located — mirrors `lore schema`.
fn resolve_vault(
    opts: &super::GlobalOpts,
    root_override: Option<PathBuf>,
) -> miette::Result<(PathBuf, Locale)> {
    match root_override {
        Some(r) => {
            let locale = match find_config(opts).and_then(|p| load_config(&p)) {
                Ok(config) => config.vault.locale(),
                Err(_) => Locale::default(),
            };
            Ok((r, locale))
        }
        None => {
            let path = find_config(opts)?;
            let config = load_config(&path)?;
            Ok((config.vault.root_path(), config.vault.locale()))
        }
    }
}
