use super::{find_config, load_config};

const RETENTION_DAYS: i64 = 90;

pub async fn run(opts: &super::GlobalOpts) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    // redb allows a single writer at a time. Maintenance opens its own DedupCache, so it must
    // NOT overlap an active `wi ingest`. Schedule maintenance in a window that doesn't intersect
    // ingest schedules in your crontab.
    eprintln!(
        "Note: `wi maintenance` must not overlap an active `wi ingest` run (dedup file lock)."
    );
    let log_path = vault_root.join(".wiki-ingest").join("ingest.jsonl");
    let cutoff_secs = jiff::Timestamp::now().as_second() - (RETENTION_DAYS * 24 * 3600);

    // 1. Prune ingest log
    if log_path.exists() {
        let content = tokio::fs::read_to_string(&log_path)
            .await
            .map_err(|e| miette::miette!("read log: {e}"))?;

        let mut kept: Vec<String> = Vec::new();
        let mut original = 0usize;
        let mut dropped = 0usize;

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            original += 1;
            let keep = match serde_json::from_str::<wi_vault::LogEntry>(line) {
                Ok(entry) => entry.timestamp.as_second() >= cutoff_secs,
                Err(_) => true,
            };
            if keep {
                kept.push(line.to_string());
            } else {
                dropped += 1;
            }
        }

        let new_content = kept.join("\n") + "\n";
        tokio::fs::write(&log_path, new_content)
            .await
            .map_err(|e| miette::miette!("write log: {e}"))?;

        eprintln!(
            "log: pruned {dropped} entries older than {RETENTION_DAYS}d (kept {} of {original}).",
            kept.len()
        );
    } else {
        eprintln!("log: no log file to maintain.");
    }

    // 2. Prune dedup cache
    let dedup_path = vault_root.join(".wiki-ingest").join("dedup.redb");
    if dedup_path.exists() {
        let cache = wi_pipeline::dedup_cache_for_maintenance(&dedup_path, &config)
            .map_err(|e| miette::miette!("dedup open: {e}"))?;
        let removed = cache
            .prune(cutoff_secs as u64)
            .map_err(|e| miette::miette!("dedup prune: {e}"))?;
        eprintln!("dedup: pruned {removed} entries older than {RETENTION_DAYS}d.");
    } else {
        eprintln!("dedup: no cache file to maintain.");
    }

    Ok(())
}
