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

/// Line numbers retention must keep whatever their age: each source's newest COLLECTED
/// entry, which is the exact line `IngestLog::find_last_collection` returns.
///
/// The ingest log is two things at once — a HISTORY of runs, and the state store `lore
/// health` and `lore status` read. A retention horizon may prune the history; pruning the
/// state would make health's verdict a function of how long a source has been broken.
///
/// It has to be the newest COLLECTED entry, not merely the newest one. A source failing
/// daily has a recent `failed` entry and a much older success, and `find_last_collection`
/// skips failures — so protecting the newest line protects a `failed` the reader ignores
/// while the success it actually wants ages past the cutoff and is pruned. The source then
/// stops reading `stale` (which exits non-zero) and starts reading "never ingested" (which
/// does not, without `--strict`): the alarm turning itself off precisely as the problem
/// gets older, which is the whole thing this exists to prevent. A source that has never
/// been collected has no state to protect, and "never ingested" is then the truth.
fn newest_collection_per_source(entries: &[(usize, lk_vault::LogEntry)]) -> HashSet<usize> {
    let mut newest: HashMap<&str, (i64, usize)> = HashMap::new();
    for (line_no, entry) in entries {
        if !entry.status.is_collected() {
            continue;
        }
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

/// Which lines of the ingest log this run deletes: those older than the horizon that are
/// not some source's newest collection. Both halves must hold — an expired line that IS
/// state stays, which is the whole rule, and it is decided here rather than inline so the
/// rule has one testable statement instead of only the helper that feeds it.
///
/// A malformed line parses as nothing, so it is never state and never expired: it is
/// carried through verbatim, leaving corruption visible instead of quietly rewritten away.
fn expired_log_lines(lines: &[&str], cutoff_secs: i64) -> HashSet<usize> {
    let parsed: Vec<(usize, lk_vault::LogEntry)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| serde_json::from_str(line).ok().map(|e| (i, e)))
        .collect();
    let state = newest_collection_per_source(&parsed);
    parsed
        .iter()
        .filter(|(i, e)| e.timestamp.as_second() < cutoff_secs && !state.contains(i))
        .map(|(i, _)| *i)
        .collect()
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

        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        let expired = expired_log_lines(&lines, cutoff_secs);

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
             (kept {} of {}, including each source's latest collection).",
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
    //
    // The intent plane's own stores divide the same way. The TRANSITION LOG is knowledge — it
    // is what a completed task becomes, and `lore ingest --date <past>` reproduces a day's
    // archive from it — so it is never pruned, which is also what makes "have I answered this
    // observation before" exact rather than bounded by a horizon. The proposal snapshots and
    // the standing reminders are STATE, not history: pruning them would retract what a source
    // said is open and what a person asked to be told.
    //
    // The intent plane's stores are STATE, not history: pruning them would retract what a
    // source says is open, what a person asked to be told, and what a calendar holds. Each is a
    // snapshot of a window rather than a file per day, so none of them grows without bound —
    // there is nothing here for a horizon to answer.
    //
    // What is removed is a snapshot no CONFIGURED source answers for. A source deleted from
    // `config.yaml` leaves a file that can never be read again, and a file nothing can read is
    // not state. Disabling a source is not deleting it: `enabled: false` is a pause, and its
    // snapshot is kept for the day it comes back.
    if let Some(personal) = &config.personal
        && personal.tasks.is_some()
    {
        let configured: Vec<String> = config.sources.keys().cloned().collect();
        let candidates = lk_task::Candidates::new(&vault_root);
        let schedule = lk_task::Schedule::new(&vault_root);
        let mut orphans = candidates
            .orphans(&configured)
            .map_err(|e| miette::miette!("{e}"))?;
        orphans.extend(
            schedule
                .orphans(&configured)
                .map_err(|e| miette::miette!("{e}"))?,
        );
        if !orphans.is_empty() {
            if !dry_run {
                lk_task::Candidates::retire(&orphans).map_err(|e| miette::miette!("{e}"))?;
            }
            eprintln!(
                "{prefix}intent: removed {} snapshot(s) no configured source answers for.",
                orphans.len()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{expired_log_lines, newest_collection_per_source, prune_cutoff_secs};

    fn line(source_id: &str, secs: i64, status: lk_vault::LogStatus) -> String {
        serde_json::to_string(&entry(source_id, secs, status)).unwrap()
    }

    /// The helper below computes the protected set; this is the only statement that the
    /// prune HONOURS it. Both halves of the rule are exercised at once — the old line that
    /// is state must survive, and the old line that is not must go — because an
    /// implementation that dropped either half still satisfies the other.
    #[test]
    fn an_expired_line_that_is_a_sources_only_collection_is_not_pruned() {
        let owned = [
            line("jira", 100, lk_vault::LogStatus::Skipped), // ancient, but jira's state
            "{ not json".to_owned(),                         // corruption, carried verbatim
            line("jira", 200, lk_vault::LogStatus::Failed),  // ancient history
            line("gmail", 5_000, lk_vault::LogStatus::Skipped), // inside the horizon
        ];
        let lines: Vec<&str> = owned.iter().map(String::as_str).collect();

        let mut expired: Vec<usize> = expired_log_lines(&lines, 1_000).into_iter().collect();
        expired.sort();
        // The malformed line sits BETWEEN the two that matter, so the returned indices
        // address positions in the input rather than in the parsed subset — the two diverge
        // the moment a line fails to parse, and this function's answer is fed straight back
        // as an index into the input.
        assert_eq!(
            expired,
            vec![2],
            "only the ancient failure is history; the ancient collection is jira's state, \
             the recent line is inside the horizon, and the malformed line is neither"
        );
    }

    /// Age alone is not enough and state alone is not enough — the rule is the conjunction.
    /// A recent line is never pruned even though it is not anyone's state.
    #[test]
    fn a_line_inside_the_horizon_survives_whether_or_not_it_is_state() {
        let owned = [
            line("jira", 5_000, lk_vault::LogStatus::Failed),
            line("jira", 6_000, lk_vault::LogStatus::Skipped),
            // The horizon itself: an entry exactly `retention_days` old has reached the
            // horizon, not passed it, so it is kept. Pinned because either boundary reads
            // as reasonable and the difference is invisible until a day's history vanishes
            // one run early.
            line("jira", 1_000, lk_vault::LogStatus::Failed),
        ];
        let lines: Vec<&str> = owned.iter().map(String::as_str).collect();
        assert!(
            expired_log_lines(&lines, 1_000).is_empty(),
            "nothing has passed the horizon here"
        );
    }

    fn entry(source_id: &str, secs: i64, status: lk_vault::LogStatus) -> lk_vault::LogEntry {
        lk_vault::LogEntry {
            timestamp: jiff::Timestamp::from_second(secs).unwrap(),
            source_id: source_id.into(),
            status,
            event_count: 0,
            duration_ms: 1,
            error: None,
        }
    }

    fn collected(source_id: &str, secs: i64) -> lk_vault::LogEntry {
        entry(source_id, secs, lk_vault::LogStatus::Skipped)
    }

    fn failed(source_id: &str, secs: i64) -> lk_vault::LogEntry {
        entry(source_id, secs, lk_vault::LogStatus::Failed)
    }

    /// Retention prunes HISTORY. The newest COLLECTED entry per source is not history — it
    /// is the state `lore health` reads — so it survives any horizon.
    #[test]
    fn each_sources_latest_collection_survives_any_horizon() {
        let entries = vec![
            (0, collected("jira", 100)),
            (1, collected("jira", 200)),
            (2, collected("gmail", 50)),
        ];
        let mut state: Vec<usize> = newest_collection_per_source(&entries).into_iter().collect();
        state.sort();
        assert_eq!(
            state,
            vec![1, 2],
            "the newest collection per source, and only those"
        );
    }

    /// The line to protect is the one the reader RETURNS. A source failing daily has a
    /// recent failure and an old success; `find_last_collection` skips the failure, so
    /// protecting the newest line would protect a line nobody reads and let the success
    /// age out — turning `stale` (exit 1) into "never ingested" (silent) as the outage
    /// gets older, which is exactly what this rule exists to prevent.
    #[test]
    fn a_recent_failure_never_shadows_the_older_success_a_health_check_reads() {
        let entries = vec![
            (0, collected("jira", 100)),
            (1, failed("jira", 200)),
            (2, failed("jira", 300)),
        ];
        let state = newest_collection_per_source(&entries);
        assert_eq!(
            state.into_iter().collect::<Vec<_>>(),
            vec![0],
            "the old success is the state; the recent failures are history"
        );
    }

    #[test]
    fn a_tie_on_timestamp_still_yields_exactly_one_line_per_source() {
        let entries = vec![
            (0, collected("jira", 100)),
            (1, collected("jira", 100)),
            (2, collected("gmail", 100)),
        ];
        let state = newest_collection_per_source(&entries);
        assert_eq!(state.len(), 2, "one line per source: {state:?}");
        assert!(state.contains(&2), "gmail must be represented");
        assert!(
            state.contains(&1),
            "the later line wins a tie deterministically"
        );
    }

    /// A source that has never been collected has no state to protect — and then "never
    /// ingested" is the truth, not a signal that went quiet.
    #[test]
    fn a_source_with_only_failures_has_no_state_to_protect() {
        assert!(newest_collection_per_source(&[(0, failed("jira", 100))]).is_empty());
        assert!(newest_collection_per_source(&[]).is_empty());
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
