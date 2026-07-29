use std::collections::{HashMap, HashSet};

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

/// Line numbers of the newest entry for each source, which retention must keep whatever
/// their age.
///
/// The ingest log is two things at once: a HISTORY of runs, and the state store
/// `IngestLog::find_last_collection` reads to answer when a source was last collected. A
/// retention horizon may prune the history; pruning the state would make `lore health`'s
/// verdict a function of how long a source has been broken. Past the horizon, every entry
/// for a dead source disappears and it stops reading `stale` (which exits non-zero) and
/// starts reading "never ingested" (which does not, without `--strict`) — the alarm turns
/// itself off precisely as the problem gets older.
fn newest_entry_per_source(entries: &[(usize, lk_vault::LogEntry)]) -> HashSet<usize> {
    let mut newest: HashMap<&str, (i64, usize)> = HashMap::new();
    for (line_no, entry) in entries {
        let seen = (entry.timestamp.as_second(), *line_no);
        newest
            .entry(entry.source_id.as_str())
            .and_modify(|held| {
                if seen > *held {
                    *held = seen;
                }
            })
            .or_insert(seen);
    }
    newest.into_values().map(|(_, line_no)| line_no).collect()
}

pub async fn run(opts: &super::GlobalOptions, dry_run: bool) -> miette::Result<()> {
    let config = load_config(&find_config(opts)?)?;
    let vault_root = config.vault.root_path();
    let retention_days = config.maintenance.retention_days;
    let log_path = vault_root.join(".lorekeeper").join("ingest.jsonl");
    let cutoff_secs = prune_cutoff_secs(jiff::Timestamp::now().as_second(), retention_days);
    let prefix = if dry_run { "[dry-run] " } else { "" };

    // 1. Prune ingest log
    if log_path.exists() {
        let content = tokio::fs::read_to_string(&log_path)
            .await
            .map_err(|e| miette::miette!("read log: {e}"))?;

        // A malformed line is neither parsed nor pruned — it is carried through verbatim,
        // so corruption stays visible instead of being quietly rewritten away.
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        let parsed: Vec<(usize, lk_vault::LogEntry)> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, line)| serde_json::from_str(line).ok().map(|e| (i, e)))
            .collect();
        let state = newest_entry_per_source(&parsed);
        let expired: HashSet<usize> = parsed
            .iter()
            .filter(|(i, e)| e.timestamp.as_second() < cutoff_secs && !state.contains(i))
            .map(|(i, _)| *i)
            .collect();

        let kept: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(i, _)| !expired.contains(i))
            .map(|(_, line)| *line)
            .collect();

        if !dry_run && !expired.is_empty() {
            let new_content = kept.join("\n") + "\n";
            super::write_atomic(log_path.clone(), new_content.into_bytes())
                .await
                .map_err(|e| miette::miette!("write log: {e}"))?;
        }

        eprintln!(
            "{prefix}log: pruned {} entries older than {retention_days}d \
             (kept {} of {}, including each source's latest).",
            expired.len(),
            kept.len(),
            lines.len()
        );
    } else {
        eprintln!("{prefix}log: no log file to maintain.");
    }

    // 2. Prune processed queue files (older than retention). Pending queue files
    // (the top-level .lorekeeper/queue/*.jsonl) are NEVER deleted by maintenance —
    // they represent unfinished semantic work that `/lore-process` must drain.
    let processed_dir = vault_root
        .join(".lorekeeper")
        .join("queue")
        .join(lk_queue::PROCESSED_SUBDIR);
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
                if !dry_run {
                    tokio::fs::remove_file(&path)
                        .await
                        .map_err(|e| miette::miette!("remove {}: {e}", path.display()))?;
                }
                pruned += 1;
            }
        }
        eprintln!("{prefix}queue: pruned {pruned} processed file(s) older than {retention_days}d.");
    } else {
        eprintln!("{prefix}queue: no processed/ directory to maintain.");
    }

    // Stale `.jsonl.tmp` files left over from crashes mid-flush are NOT pruned here —
    // `lore ingest` does that at startup, where it's guaranteed not to race a concurrent
    // ingest's active flush. Maintenance touches only post-rename artifacts.
    //
    // The per-date streaming event logs (`.lorekeeper/events/{source}/{date}.jsonl`) are
    // NEVER pruned: they are the permanent raw layer a streaming source projects its daily
    // pages from, so `lore ingest --date <past>` can re-render ANY day — a deleted or
    // damaged page self-heals only as far back as the log exists. Knowledge layers are
    // permanent; retention applies to operational history only.

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{newest_entry_per_source, prune_cutoff_secs};

    fn entry(source_id: &str, secs: i64) -> lk_vault::LogEntry {
        lk_vault::LogEntry {
            timestamp: jiff::Timestamp::from_second(secs).unwrap(),
            source_id: source_id.into(),
            status: lk_vault::LogStatus::Skipped,
            event_count: 0,
            duration_ms: 1,
            error: None,
        }
    }

    /// Retention prunes HISTORY. The newest entry per source is not history — it is the
    /// state `lore health` reads, and pruning it makes a source that has been dead longer
    /// than the horizon report "never ingested" (silent) instead of stale (exit 1).
    #[test]
    fn each_sources_latest_entry_survives_any_horizon() {
        let entries = vec![
            (0, entry("jira", 100)),
            (1, entry("jira", 200)),
            (2, entry("gmail", 50)),
        ];
        let mut state: Vec<usize> = newest_entry_per_source(&entries).into_iter().collect();
        state.sort();
        assert_eq!(
            state,
            vec![1, 2],
            "the newest line per source, and only those"
        );
    }

    /// Ties cannot leave a source unrepresented, and cannot pick two lines for one source.
    #[test]
    fn a_tie_on_timestamp_still_yields_exactly_one_line_per_source() {
        let entries = vec![
            (0, entry("jira", 100)),
            (1, entry("jira", 100)),
            (2, entry("gmail", 100)),
        ];
        let state = newest_entry_per_source(&entries);
        assert_eq!(state.len(), 2, "one line per source: {state:?}");
        assert!(state.contains(&2), "gmail must be represented");
        assert!(
            state.contains(&1),
            "the later line wins a tie deterministically"
        );
    }

    #[test]
    fn an_empty_log_has_no_state_to_protect() {
        assert!(newest_entry_per_source(&[]).is_empty());
    }

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
