use std::path::Path;

use redb::{Database, ReadableTable, TableDefinition};
use strsim::sorensen_dice;

use wi_core::config::DedupStrategy;
use wi_core::event::Event;

use crate::PipelineError;

const EVENT_IDS: TableDefinition<&str, u64> = TableDefinition::new("event_ids");
const URLS: TableDefinition<&str, u64> = TableDefinition::new("urls");
const TITLES: TableDefinition<&str, u64> = TableDefinition::new("titles");

pub struct DedupCache {
    db: Database,
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
                Err(e) if !wiped => {
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        "dedup schema mismatch detected; wiping cache and recreating"
                    );
                    drop(txn);
                    drop(db);
                    std::fs::remove_file(path).ok();
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
        let txn = self
            .db
            .begin_read()
            .map_err(|e| PipelineError::Dedup(e.to_string()))?;

        let id_table = txn
            .open_table(EVENT_IDS)
            .map_err(|e| PipelineError::Dedup(e.to_string()))?;
        let url_table = txn
            .open_table(URLS)
            .map_err(|e| PipelineError::Dedup(e.to_string()))?;
        let title_table = txn
            .open_table(TITLES)
            .map_err(|e| PipelineError::Dedup(e.to_string()))?;

        let mut novel = Vec::with_capacity(events.len());

        for event in events {
            let mut dup = false;

            for strategy in cascade {
                match strategy {
                    DedupStrategy::EventId => {
                        if id_table
                            .get(event.id.as_str())
                            .map_err(|e| PipelineError::Dedup(e.to_string()))?
                            .is_some()
                        {
                            dup = true;
                            break;
                        }
                    }
                    DedupStrategy::Url => {
                        if let Some(ref url) = event.url
                            && url_table
                                .get(url.as_str())
                                .map_err(|e| PipelineError::Dedup(e.to_string()))?
                                .is_some()
                        {
                            dup = true;
                            break;
                        }
                    }
                    DedupStrategy::Title => {
                        let prefix = format!("{}:", event.date);
                        let iter = title_table
                            .iter()
                            .map_err(|e| PipelineError::Dedup(e.to_string()))?;

                        for entry in iter {
                            let entry = entry.map_err(|e| PipelineError::Dedup(e.to_string()))?;
                            let key = entry.0.value();
                            if let Some(existing) = key.strip_prefix(&prefix)
                                && sorensen_dice(&event.title, existing) >= self.title_threshold
                            {
                                dup = true;
                                break;
                            }
                        }
                        if dup {
                            break;
                        }
                    }
                }
            }

            if !dup {
                novel.push(event);
            }
        }

        Ok(novel)
    }

    pub fn record(&self, events: &[Event]) -> Result<(), PipelineError> {
        let now = jiff::Timestamp::now().as_second() as u64;
        let txn = self
            .db
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
        let txn = self
            .db
            .begin_write()
            .map_err(|e| PipelineError::Dedup(e.to_string()))?;

        let mut total_removed = 0u64;

        {
            for table_def in [EVENT_IDS, URLS, TITLES] {
                let mut table = txn
                    .open_table(table_def)
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?;

                let to_remove: Vec<String> = table
                    .iter()
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?
                    .filter_map(|entry| entry.ok())
                    .filter(|(_, v)| v.value() < cutoff_secs)
                    .map(|(k, _)| k.value().to_string())
                    .collect();

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
