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
}

pub async fn run(opts: &super::GlobalOptions, cmd: ConfigCommand) -> miette::Result<()> {
    match cmd {
        ConfigCommand::VaultRoot => {
            let config = load_config(&find_config(opts)?)?;
            println!("{}", config.vault.root_path().display());
            Ok(())
        }
    }
}
