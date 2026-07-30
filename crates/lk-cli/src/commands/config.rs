use super::{find_config, load_config};

#[derive(clap::Subcommand)]
pub enum ConfigCommand {
    /// Print the resolved absolute vault root on stdout, and nothing else.
    ///
    /// Every other command reports the vault as part of a human-readable summary on stderr.
    /// A deployment script needs the same value as data, and scraping it out of prose makes
    /// the script's correctness depend on wording that was never a contract. This is that
    /// contract: one line, one path, already absolutized against the config file's location.
    VaultRoot,
    /// Print the absolute path of the vault's `AGENTS.md` on stdout, and nothing else.
    ///
    /// Every skill that writes a page reads that file for the page formats, and it lives under
    /// `vault.dirs.wiki` — a configurable directory the skills are told not to read out of
    /// `config.yaml`, because a relative `vault.root` resolves against the config file's own
    /// location and the config file is itself auto-discovered. Without this they assume the
    /// default `wiki`, and on a vault configured otherwise they look in the wrong place, find
    /// nothing, run `lore schema`, and look in the wrong place again.
    /// Print the configured concept categories, one `id\tlabel` per line, and nothing else.
    ///
    /// A category outside this vocabulary is a lint VIOLATION, so every skill that creates a
    /// concept page needs the list — and the same skills are told not to read `config.yaml`,
    /// because a relative `vault.root` resolves against the config file's own directory and the
    /// file itself is auto-discovered. Without this they guess, and a guess fails the pipeline on
    /// the page the run just wrote. `lore wiki concepts` cannot stand in: it reports the
    /// categories already in USE, so a vocabulary entry nothing has used yet is invisible there.
    Categories,
    SchemaPath {
        /// Vault root override, like `lore schema --root` — so the two agree about where
        /// AGENTS.md is for the vault the caller means, not only for the configured one.
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
}

pub async fn run(opts: &super::GlobalOptions, cmd: ConfigCommand) -> miette::Result<()> {
    match cmd {
        ConfigCommand::VaultRoot => {
            let config = load_config(&find_config(opts)?)?;
            println!("{}", config.vault.root_path().display());
            Ok(())
        }
        ConfigCommand::Categories => {
            let config = load_config(&find_config(opts)?)?;
            for category in &config.concepts.categories {
                println!("{}\t{}", category.id, category.label);
            }
            Ok(())
        }
        ConfigCommand::SchemaPath { root } => {
            // Same resolution `lore schema` writes through, so the path this prints is the path
            // that command creates — including under `--root`, where only the root is overridden
            // and the configured wiki dir still applies.
            let super::RootConfig { root, config } = super::resolve_root_config(opts, root)?;
            let wiki = config
                .map(|c| c.vault.dirs.wiki)
                .unwrap_or_else(|| lk_core::config::VaultDirs::default().wiki);
            println!(
                "{}",
                root.join(wiki)
                    .join(lk_core::vault_path::SCHEMA_FILE)
                    .display()
            );
            Ok(())
        }
    }
}
