use super::schema::render_agents_md;
use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOpts) -> miette::Result<()> {
    let path = find_config(opts)?;
    let config = load_config(&path)?;

    // Validate each enabled source's params against its adapter schema so config
    // errors (missing keys, wrong types, typos) surface here rather than at the
    // first scheduled ingest run.
    let enabled: Vec<&str> = config.enabled_sources().map(|(id, _)| id).collect();
    for (id, sc) in config.enabled_sources() {
        lk_source::validate_params(sc.source_type, &sc.params)
            .map_err(|e| miette::miette!("sources.{id} ({}): {e}", sc.source_type))?;
    }

    eprintln!("Config valid: {}", path.display());
    eprintln!("  vault: {}", config.vault.root);
    eprintln!("  timezone: {:?}", config.vault.timezone);
    eprintln!("  sources ({}): {}", enabled.len(), enabled.join(", "));

    // Check AGENTS.md drift — warn if missing or out of date.
    let locale = config.vault.locale();
    let expected = render_agents_md(locale);
    let agents_path = config.vault.root_path().join("wiki").join("AGENTS.md");
    let needs_regen = match tokio::fs::read_to_string(&agents_path).await {
        Ok(on_disk) => on_disk != expected,
        Err(_) => true,
    };
    if needs_regen {
        eprintln!(
            "  warning: wiki/AGENTS.md is missing or out of date; run `lore schema` to regenerate"
        );
    }

    Ok(())
}
