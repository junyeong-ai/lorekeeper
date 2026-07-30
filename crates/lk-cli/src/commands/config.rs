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
    SchemaPath,
}

pub async fn run(opts: &super::GlobalOptions, cmd: ConfigCommand) -> miette::Result<()> {
    match cmd {
        ConfigCommand::VaultRoot => {
            let config = load_config(&find_config(opts)?)?;
            println!("{}", config.vault.root_path().display());
            Ok(())
        }
        ConfigCommand::SchemaPath => {
            let config = load_config(&find_config(opts)?)?;
            println!(
                "{}",
                config
                    .vault
                    .root_path()
                    .join(&config.vault.dirs.wiki)
                    .join(lk_core::vault_path::SCHEMA_FILE)
                    .display()
            );
            Ok(())
        }
    }
}
