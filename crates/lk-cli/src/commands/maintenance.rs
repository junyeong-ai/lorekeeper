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

/// True if an event-log file stem (`{date}`) records a day strictly before `cutoff`, so it
/// is safe to prune. A stem that isn't a date is KEPT — never delete a file we can't date.
fn event_log_expired(file_stem: Option<&str>, cutoff: jiff::civil::Date) -> bool {
    file_stem
        .and_then(|s| s.parse::<jiff::civil::Date>().ok())
        .is_some_and(|d| d < cutoff)
}

pub async fn run(opts: &super::GlobalOptions) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let retention_days = config.maintenance.retention_days;
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

    // 3. Prune the per-date streaming event log past retention. Each file is
    // `.lorekeeper/events/{source}/{date}.jsonl`; the date is in the name, so prune by the
    // day it records (semantic), not mtime. A frozen page already holds those items, so
    // dropping its long-past log only forfeits re-projecting that day — never live data.
    let events_dir = vault_root.join(".lorekeeper").join("events");
    if events_dir.exists() {
        let today = jiff::Timestamp::now()
            .to_zoned(config.vault.timezone())
            .date();
        let cutoff_date = today.saturating_sub(jiff::Span::new().days(retention_days));
        let mut pruned = 0usize;
        let mut sources = tokio::fs::read_dir(&events_dir)
            .await
            .map_err(|e| miette::miette!("read events dir: {e}"))?;
        while let Some(src) = sources
            .next_entry()
            .await
            .map_err(|e| miette::miette!("events source entry: {e}"))?
        {
            if !src.path().is_dir() {
                continue;
            }
            let mut days = tokio::fs::read_dir(src.path())
                .await
                .map_err(|e| miette::miette!("read {}: {e}", src.path().display()))?;
            while let Some(day) = days
                .next_entry()
                .await
                .map_err(|e| miette::miette!("events day entry: {e}"))?
            {
                let path = day.path();
                let too_old = path.extension().is_some_and(|e| e == "jsonl")
                    && event_log_expired(path.file_stem().and_then(|s| s.to_str()), cutoff_date);
                if too_old {
                    tokio::fs::remove_file(&path)
                        .await
                        .map_err(|e| miette::miette!("remove {}: {e}", path.display()))?;
                    pruned += 1;
                }
            }
        }
        eprintln!(
            "events: pruned {pruned} log file(s) recording days older than {retention_days}d."
        );
    } else {
        eprintln!("events: no event log to maintain.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{event_log_expired, prune_cutoff_secs};

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

    #[test]
    fn event_log_expiry_is_strict_and_date_aware() {
        let cutoff = jiff::civil::date(2026, 3, 1);
        assert!(
            event_log_expired(Some("2026-02-28"), cutoff),
            "before cutoff → prune"
        );
        assert!(
            !event_log_expired(Some("2026-03-01"), cutoff),
            "on cutoff → keep"
        );
        assert!(
            !event_log_expired(Some("2026-03-02"), cutoff),
            "after cutoff → keep"
        );
        assert!(
            !event_log_expired(Some("not-a-date"), cutoff),
            "unparseable → keep"
        );
        assert!(!event_log_expired(None, cutoff), "no stem → keep");
    }
}
