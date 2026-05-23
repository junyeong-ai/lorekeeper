use super::{find_config, load_config};

pub async fn run(opts: &super::GlobalOpts) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let log = wi_vault::IngestLog::new(vault_root.join(".wiki-ingest").join("ingest.jsonl"));

    for (id, sc) in config.enabled_sources() {
        match log.last_success(id).await.ok().flatten() {
            Some(e) => eprintln!(
                "  {id} ({}) — last: {}, {} events",
                sc.source_type, e.timestamp, e.events_count
            ),
            None => eprintln!("  {id} ({}) — never ingested", sc.source_type),
        }
    }
    Ok(())
}
