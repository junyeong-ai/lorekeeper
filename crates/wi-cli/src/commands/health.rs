use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOpts, strict: bool) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let log = wi_vault::IngestLog::new(vault_root.join(".wiki-ingest").join("ingest.jsonl"));

    let now = jiff::Timestamp::now();
    let stale_after_secs: i64 = 48 * 3600;

    let mut fresh = 0u32;
    let mut stale = 0u32;
    let mut never = 0u32;

    for (id, sc) in config.enabled_sources() {
        let last = log
            .last_success(id)
            .await
            .map_err(|e| miette::miette!("read ingest log: {e}"))?;
        match last {
            Some(entry) => {
                let age_secs = now.as_second() - entry.timestamp.as_second();
                let hours = age_secs / 3600;
                if age_secs > stale_after_secs {
                    eprintln!("⚠ {id} ({}) — {}h ago, STALE", sc.source_type, hours);
                    stale += 1;
                } else {
                    eprintln!("✓ {id} ({}) — {}h ago", sc.source_type, hours);
                    fresh += 1;
                }
            }
            None => {
                eprintln!("✗ {id} ({}) — never ingested", sc.source_type);
                never += 1;
            }
        }
    }

    eprintln!("\n{fresh} fresh, {stale} stale, {never} never");

    if stale > 0 || (strict && never > 0) {
        std::process::exit(1);
    }
    Ok(())
}
