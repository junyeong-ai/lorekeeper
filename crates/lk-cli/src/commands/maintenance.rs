use super::{find_config, load_config};

/// Seconds-since-epoch cutoff for pruning: entries with `seen_at`/mtime older than this
/// are removed. Clamped at 0 so a retention horizon exceeding the time since the Unix
/// epoch keeps everything, never a negative cutoff that would wrap when cast to `u64`
/// and wipe the entire cache. Saturating arithmetic keeps it sound for any input.
fn prune_cutoff_secs(now_secs: i64, retention_days: i64) -> i64 {
    now_secs
        .saturating_sub(retention_days.saturating_mul(86_400))
        .max(0)
}

pub async fn run(opts: &super::GlobalOpts) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let retention_days = config.maintenance.retention_days;
    // redb allows a single writer at a time. Maintenance opens its own DedupCache, so it must
    // NOT overlap an active `lore ingest`. Schedule maintenance in a window that doesn't intersect
    // ingest schedules in your crontab.
    eprintln!(
        "Note: `lore maintenance` must not overlap an active `lore ingest` run (dedup file lock)."
    );
    let log_path = vault_root.join(".lorekeeper").join("ingest.jsonl");
    let cutoff_secs = prune_cutoff_secs(jiff::Timestamp::now().as_second(), retention_days);

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
            let keep = match serde_json::from_str::<lk_vault::LogEntry>(line) {
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
            "log: pruned {dropped} entries older than {retention_days}d (kept {} of {original}).",
            kept.len()
        );
    } else {
        eprintln!("log: no log file to maintain.");
    }

    // 2. Prune processed queue files (older than retention). Pending queue files
    // (the top-level .lorekeeper/queue/*.jsonl) are NEVER deleted by maintenance —
    // they represent unfinished semantic work that `/lore-process` must drain.
    let processed_dir = vault_root
        .join(".lorekeeper")
        .join("queue")
        .join("processed");
    if processed_dir.exists() {
        let mut entries = tokio::fs::read_dir(&processed_dir)
            .await
            .map_err(|e| miette::miette!("read queue/processed: {e}"))?;
        let mut pruned = 0usize;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| miette::miette!("queue entry: {e}"))?
        {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let metadata = entry
                .metadata()
                .await
                .map_err(|e| miette::miette!("metadata: {e}"))?;
            let mtime_secs = metadata.modified().ok().and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs() as i64)
            });
            if mtime_secs.is_some_and(|m| m < cutoff_secs) {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| miette::miette!("remove {}: {e}", path.display()))?;
                pruned += 1;
            }
        }
        eprintln!("queue: pruned {pruned} processed file(s) older than {retention_days}d.");
    } else {
        eprintln!("queue: no processed/ directory to maintain.");
    }

    // Stale `.jsonl.tmp` files left over from crashes mid-flush are NOT pruned here —
    // `lore ingest` does that at startup, where it's guaranteed not to race a concurrent
    // ingest's active flush. Maintenance touches only post-rename artifacts.

    // 3. Prune dedup cache
    let dedup_path = vault_root.join(".lorekeeper").join("dedup.redb");
    if dedup_path.exists() {
        let cache =
            lk_pipeline::DedupCache::open(&dedup_path, config.dedup.extra_tracking_params.clone())
                .map_err(|e| miette::miette!("dedup open: {e}"))?;
        let removed = cache
            .prune(cutoff_secs as u64)
            .map_err(|e| miette::miette!("dedup prune: {e}"))?;
        eprintln!("dedup: pruned {removed} entries older than {retention_days}d.");
    } else {
        eprintln!("dedup: no cache file to maintain.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::prune_cutoff_secs;

    #[test]
    fn cutoff_is_now_minus_retention() {
        assert_eq!(prune_cutoff_secs(1_000_000, 1), 1_000_000 - 86_400);
    }

    #[test]
    fn cutoff_clamps_to_zero_for_retention_longer_than_epoch() {
        // A retention horizon longer than the time elapsed keeps everything (cutoff 0),
        // never a negative value that would wrap to a huge u64 and wipe the cache.
        assert_eq!(prune_cutoff_secs(1_000_000, 1_000_000), 0);
        assert_eq!(prune_cutoff_secs(i64::MAX / 2, i64::MAX), 0);
    }
}
