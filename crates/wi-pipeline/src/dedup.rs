use std::path::Path;

use redb::{Database, ReadableTable, TableDefinition};
use strsim::sorensen_dice;

use wi_core::config::DedupStrategy;
use wi_core::event::Event;

use crate::PipelineError;

const EVENT_IDS: TableDefinition<&str, u64> = TableDefinition::new("event_ids");
const URLS: TableDefinition<&str, u64> = TableDefinition::new("urls");
const TITLES: TableDefinition<&str, u64> = TableDefinition::new("titles");

/// True only for errors that mean the on-disk table layout no longer matches this build's
/// schema. These are recoverable by recreating the cache. `Storage` errors (I/O, corruption)
/// are NOT schema mismatches and must propagate.
fn is_schema_mismatch(e: &redb::TableError) -> bool {
    matches!(
        e,
        redb::TableError::TableTypeMismatch { .. }
            | redb::TableError::TypeDefinitionChanged { .. }
            | redb::TableError::TableIsMultimap(_)
            | redb::TableError::TableIsNotMultimap(_)
    )
}

pub struct DedupCache {
    /// `None` is a read-only "empty" cache used by dry-run: it never creates or
    /// writes a file, treats every event as novel, and no-ops on record/prune.
    db: Option<Database>,
    title_threshold: f64,
}

impl DedupCache {
    pub fn open(path: &Path, title_threshold: f64) -> Result<Self, PipelineError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PipelineError::Dedup(format!("create dir: {e}")))?;
        }

        let db = Self::open_or_reset(path)?;

        Ok(Self {
            db: Some(db),
            title_threshold,
        })
    }

    /// Open WITHOUT creating anything — for dry-run, which must not touch the vault.
    /// If the cache file exists it is opened for reading so the preview reflects real
    /// dedup state; if it doesn't, every event is reported novel (as a real first run
    /// would also see), and no file is created.
    pub fn open_read_only(path: &Path, title_threshold: f64) -> Result<Self, PipelineError> {
        let db = if path.exists() {
            Some(
                Database::open(path)
                    .map_err(|e| PipelineError::Dedup(format!("open db (read-only): {e}")))?,
            )
        } else {
            None
        };
        Ok(Self {
            db,
            title_threshold,
        })
    }

    fn open_or_reset(path: &Path) -> Result<Database, PipelineError> {
        let mut wiped = false;
        loop {
            let db = Database::create(path)
                .map_err(|e| PipelineError::Dedup(format!("open db: {e}")))?;

            let txn = db
                .begin_write()
                .map_err(|e| PipelineError::Dedup(e.to_string()))?;

            let init = (|| -> Result<(), redb::TableError> {
                txn.open_table(EVENT_IDS)?;
                txn.open_table(URLS)?;
                txn.open_table(TITLES)?;
                Ok(())
            })();

            match init {
                Ok(()) => {
                    txn.commit()
                        .map_err(|e| PipelineError::Dedup(e.to_string()))?;
                    return Ok(db);
                }
                // Only a deliberate schema change (table key/value types differ from what
                // this build defines) is safe to recover by wiping. Storage-level errors
                // (I/O failure, on-disk corruption) must fail closed — silently wiping
                // would destroy recoverable dedup history and silently re-ingest everything.
                Err(e) if is_schema_mismatch(&e) && !wiped => {
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        "dedup schema changed; recreating cache"
                    );
                    drop(txn);
                    drop(db);
                    std::fs::remove_file(path)
                        .map_err(|e| PipelineError::Dedup(format!("remove stale cache: {e}")))?;
                    wiped = true;
                    continue;
                }
                Err(e) => {
                    return Err(PipelineError::Dedup(format!("init tables: {e}")));
                }
            }
        }
    }

    pub fn deduplicate(
        &self,
        events: Vec<Event>,
        cascade: &[DedupStrategy],
    ) -> Result<Vec<Event>, PipelineError> {
        // Persisted tables exist only when a real (writable) cache is open. In read-only/
        // empty mode (dry-run, no file) they're absent, but intra-batch dedup must STILL
        // run so a dry-run preview matches what a real run would actually write.
        let read_txn = match &self.db {
            Some(db) => Some(
                db.begin_read()
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?,
            ),
            None => None,
        };
        let tables = match &read_txn {
            Some(txn) => Some((
                txn.open_table(EVENT_IDS)
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?,
                txn.open_table(URLS)
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?,
                txn.open_table(TITLES)
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?,
            )),
            None => None,
        };

        let mut novel: Vec<Event> = Vec::with_capacity(events.len());
        // Per-run sets so two identical events in the SAME input batch don't both pass
        // as novel — the persisted tables only reflect prior committed runs.
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut seen_urls: std::collections::HashSet<String> = std::collections::HashSet::new();

        for event in events {
            let mut dup = false;

            for strategy in cascade {
                match strategy {
                    DedupStrategy::EventId => {
                        let in_cache = match &tables {
                            Some((ids, _, _)) => ids
                                .get(event.id.as_str())
                                .map_err(|e| PipelineError::Dedup(e.to_string()))?
                                .is_some(),
                            None => false,
                        };
                        if seen_ids.contains(event.id.as_str()) || in_cache {
                            dup = true;
                            break;
                        }
                    }
                    DedupStrategy::Url => {
                        if let Some(ref url) = event.url {
                            let in_cache = match &tables {
                                Some((_, urls, _)) => urls
                                    .get(url.as_str())
                                    .map_err(|e| PipelineError::Dedup(e.to_string()))?
                                    .is_some(),
                                None => false,
                            };
                            if seen_urls.contains(url) || in_cache {
                                dup = true;
                                break;
                            }
                        }
                    }
                    DedupStrategy::Title => {
                        // Titles are keyed `{date}:{title}`; all of one date's titles form a
                        // contiguous lexicographic run. Seek to the date prefix and stop at
                        // the first key outside it, so this scans one date's titles rather
                        // than the entire (up to 90-day) cache. Also compare against titles
                        // already accepted earlier in this same batch.
                        let prefix = format!("{}:", event.date);
                        if let Some((_, _, title_table)) = &tables {
                            let range = title_table
                                .range(prefix.as_str()..)
                                .map_err(|e| PipelineError::Dedup(e.to_string()))?;
                            for entry in range {
                                let entry =
                                    entry.map_err(|e| PipelineError::Dedup(e.to_string()))?;
                                let key = entry.0.value();
                                let Some(existing) = key.strip_prefix(&prefix) else {
                                    break;
                                };
                                if sorensen_dice(&event.title, existing) >= self.title_threshold {
                                    dup = true;
                                    break;
                                }
                            }
                        }
                        if !dup {
                            dup = novel.iter().any(|n| {
                                n.date == event.date
                                    && sorensen_dice(&n.title, &event.title) >= self.title_threshold
                            });
                        }
                        if dup {
                            break;
                        }
                    }
                }
            }

            if !dup {
                seen_ids.insert(event.id.as_str().to_string());
                if let Some(ref url) = event.url {
                    seen_urls.insert(url.clone());
                }
                novel.push(event);
            }
        }

        Ok(novel)
    }

    pub fn record(&self, events: &[Event]) -> Result<(), PipelineError> {
        // A read-only/empty cache never persists — dry-run never commits anyway.
        let Some(db) = &self.db else {
            return Ok(());
        };
        let now = jiff::Timestamp::now().as_second() as u64;
        let txn = db
            .begin_write()
            .map_err(|e| PipelineError::Dedup(e.to_string()))?;

        {
            let mut ids = txn
                .open_table(EVENT_IDS)
                .map_err(|e| PipelineError::Dedup(e.to_string()))?;
            let mut urls = txn
                .open_table(URLS)
                .map_err(|e| PipelineError::Dedup(e.to_string()))?;
            let mut titles = txn
                .open_table(TITLES)
                .map_err(|e| PipelineError::Dedup(e.to_string()))?;

            for event in events {
                ids.insert(event.id.as_str(), now)
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?;

                if let Some(ref url) = event.url {
                    urls.insert(url.as_str(), now)
                        .map_err(|e| PipelineError::Dedup(e.to_string()))?;
                }

                let title_key = format!("{}:{}", event.date, event.title);
                titles
                    .insert(title_key.as_str(), now)
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?;
            }
        }

        txn.commit()
            .map_err(|e| PipelineError::Dedup(e.to_string()))?;

        Ok(())
    }

    /// Remove entries with `seen_at_secs < cutoff_secs`. Returns number of entries removed.
    pub fn prune(&self, cutoff_secs: u64) -> Result<u64, PipelineError> {
        let Some(db) = &self.db else {
            return Ok(0);
        };
        let txn = db
            .begin_write()
            .map_err(|e| PipelineError::Dedup(e.to_string()))?;

        let mut total_removed = 0u64;

        {
            for table_def in [EVENT_IDS, URLS, TITLES] {
                let mut table = txn
                    .open_table(table_def)
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?;

                // Collect each row as a Result and propagate the first read failure rather
                // than dropping it — a swallowed error would leave stale entries while
                // reporting a successful prune.
                let mut to_remove: Vec<String> = Vec::new();
                for entry in table
                    .iter()
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?
                {
                    let (k, v) = entry.map_err(|e| PipelineError::Dedup(e.to_string()))?;
                    if v.value() < cutoff_secs {
                        to_remove.push(k.value().to_string());
                    }
                }

                for key in &to_remove {
                    table
                        .remove(key.as_str())
                        .map_err(|e| PipelineError::Dedup(e.to_string()))?;
                    total_removed += 1;
                }
            }
        }

        txn.commit()
            .map_err(|e| PipelineError::Dedup(e.to_string()))?;

        Ok(total_removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use wi_core::config::SourceType;
    use wi_core::event::{Event, EventId};

    fn ev(id_suffix: &str, title: &str, url: Option<&str>) -> Event {
        let date = jiff::civil::date(2026, 5, 23);
        Event {
            id: EventId::new("test", date, id_suffix),
            source_id: "test".into(),
            source_type: SourceType::Gmail,
            date,
            title: title.into(),
            body: String::new(),
            url: url.map(String::from),
            author: None,
            labels: vec![],
            classification: None,
            is_personal: false,
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn dedup_filters_seen_events() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), 0.85).unwrap();

        let cascade = vec![DedupStrategy::EventId];
        let events = vec![ev("a", "A", None), ev("b", "B", None)];

        let novel = cache.deduplicate(events.clone(), &cascade).unwrap();
        assert_eq!(novel.len(), 2);

        cache.record(&events).unwrap();

        let novel2 = cache.deduplicate(events, &cascade).unwrap();
        assert_eq!(novel2.len(), 0);
    }

    #[test]
    fn cascade_tries_each_strategy_in_order() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), 0.85).unwrap();
        let cascade = vec![
            DedupStrategy::EventId,
            DedupStrategy::Url,
            DedupStrategy::Title,
        ];

        cache
            .record(&[ev("seed", "Seeded Title", Some("https://example.com/a"))])
            .unwrap();

        // Matches via Url only (different id, different title).
        let by_url = vec![ev(
            "other",
            "Totally different",
            Some("https://example.com/a"),
        )];
        assert_eq!(cache.deduplicate(by_url, &cascade).unwrap().len(), 0);

        // Matches via Title only (different id, no url).
        let by_title = vec![ev("other2", "Seeded Title", None)];
        assert_eq!(cache.deduplicate(by_title, &cascade).unwrap().len(), 0);

        // Matches nothing in the cascade → novel.
        let novel = vec![ev("fresh", "Brand New", Some("https://example.com/z"))];
        assert_eq!(cache.deduplicate(novel, &cascade).unwrap().len(), 1);
    }

    #[test]
    fn read_only_empty_cache_still_dedups_within_batch() {
        // Dry-run with no existing cache (open_read_only → None) must still drop
        // intra-batch duplicates so the preview matches a real run.
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open_read_only(&dir.path().join("absent.redb"), 0.85).unwrap();
        let cascade = vec![DedupStrategy::EventId];
        let batch = vec![ev("a", "A", None), ev("a", "A", None)];
        let novel = cache.deduplicate(batch, &cascade).unwrap();
        assert_eq!(
            novel.len(),
            1,
            "intra-batch dup must be dropped even with no cache file"
        );
        // And no file was created.
        assert!(!dir.path().join("absent.redb").exists());
    }

    #[test]
    fn intra_batch_duplicates_are_filtered() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), 0.85).unwrap();
        let cascade = vec![DedupStrategy::EventId, DedupStrategy::Url];

        // Two events with the same id in ONE batch (nothing committed yet).
        let batch = vec![
            ev("a", "A", Some("http://x")),
            ev("a", "A", Some("http://x")),
        ];
        let novel = cache.deduplicate(batch, &cascade).unwrap();
        assert_eq!(
            novel.len(),
            1,
            "duplicate within the same batch must be dropped"
        );
    }

    #[test]
    fn url_dedup_filters_same_url_different_id() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), 0.85).unwrap();
        let cascade = vec![DedupStrategy::Url];

        cache
            .record(&[ev("a", "First", Some("https://example.com/post"))])
            .unwrap();

        // Same URL, different event-id and title → still a duplicate via the URL strategy.
        let dup = vec![ev("b", "Reposted", Some("https://example.com/post"))];
        assert_eq!(cache.deduplicate(dup, &cascade).unwrap().len(), 0);

        // Different URL is novel.
        let novel = vec![ev("c", "Other", Some("https://example.com/other"))];
        assert_eq!(cache.deduplicate(novel, &cascade).unwrap().len(), 1);
    }

    #[test]
    fn title_dedup_matches_within_date_only() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), 0.85).unwrap();

        // Record a title under 2026-05-23.
        cache
            .record(&[ev("a", "Anthropic releases Opus 4.7", None)])
            .unwrap();

        let cascade = vec![DedupStrategy::Title];

        // A near-identical title on the same date is a duplicate.
        let same_day = vec![ev("b", "Anthropic releases Opus 4.7!", None)];
        assert_eq!(cache.deduplicate(same_day, &cascade).unwrap().len(), 0);

        // The same title on a DIFFERENT date is novel (date-partitioned scan).
        let other_day = Event {
            date: jiff::civil::date(2026, 6, 1),
            ..ev("c", "Anthropic releases Opus 4.7", None)
        };
        assert_eq!(
            cache.deduplicate(vec![other_day], &cascade).unwrap().len(),
            1
        );
    }

    #[test]
    fn prune_removes_old_entries() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), 0.85).unwrap();

        cache.record(&[ev("a", "A", Some("http://x"))]).unwrap();

        let future_cutoff = jiff::Timestamp::now().as_second() as u64 + 1;
        let removed = cache.prune(future_cutoff).unwrap();
        assert!(
            removed >= 3,
            "should remove from all 3 tables (event_id + url + title)"
        );
    }
}
