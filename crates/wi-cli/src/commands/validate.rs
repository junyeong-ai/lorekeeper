use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOpts) -> miette::Result<()> {
    let path = find_config(opts)?;
    let config = load_config(&path)?;
    let enabled: Vec<_> = config.enabled_sources().map(|(id, _)| id).collect();
    eprintln!("Config valid: {}", path.display());
    eprintln!("  vault: {}", config.vault.root);
    eprintln!("  timezone: {:?}", config.vault.timezone);
    eprintln!("  sources ({}): {}", enabled.len(), enabled.join(", "));
    Ok(())
}
