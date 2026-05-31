use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOptions) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let log = lk_vault::IngestLog::new(vault_root.join(".lorekeeper").join("ingest.jsonl"));

    for (id, sc) in config.enabled_sources() {
        let last = log
            .last_success(id)
            .await
            .map_err(|e| miette::miette!("read ingest log: {e}"))?;
        match last {
            Some(e) => eprintln!(
                "  {id} ({}) — last: {}, {} events",
                sc.source_type, e.timestamp, e.events_count
            ),
            None => eprintln!("  {id} ({}) — never ingested", sc.source_type),
        }
    }
    Ok(())
}
