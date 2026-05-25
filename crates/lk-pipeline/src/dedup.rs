use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use strsim::sorensen_dice;

use lk_core::config::DedupStrategy;
use lk_core::event::Event;

use crate::PipelineError;

const EVENT_IDS: TableDefinition<&str, u64> = TableDefinition::new("event_ids");
const CONTENT_HASHES: TableDefinition<&str, u64> = TableDefinition::new("content_hashes");
const URLS: TableDefinition<&str, u64> = TableDefinition::new("urls");
const TITLES: TableDefinition<&str, u64> = TableDefinition::new("titles");

/// Exact query-parameter keys stripped during URL canonicalisation. These are
/// vendor-tracking parameters whose presence varies by attribution path but
/// never carry resource-identifying information.
const TRACKING_QUERY_EXACT_KEYS: &[&str] = &[
    "fbclid", "gclid", "mc_cid", "mc_eid", "_hsenc", "_hsmi", "ref",
];

/// Query-key prefixes stripped during URL canonicalisation. Only genuine
/// prefixes that always carry a trailing discriminator (utm_source,
/// utm_medium, utm_campaign, utm_term, utm_content).
const TRACKING_QUERY_PREFIXES: &[&str] = &["utm_"];

fn is_tracking_param(key: &str) -> bool {
    TRACKING_QUERY_EXACT_KEYS.contains(&key)
        || TRACKING_QUERY_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

pub(crate) fn normalize_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return raw.to_string();
    };

    let scheme = match url.scheme() {
        "http" | "https" => "https",
        other => {
            // Non-HTTP schemes (ftp, mailto, …) are kept as-is since the
            // scheme itself is semantically meaningful.
            if url.set_scheme(&other.to_ascii_lowercase()).is_err() {
                return raw.to_string();
            }
            url.set_query(None);
            url.set_fragment(None);
            return url.to_string();
        }
    };
    if url.set_scheme(scheme).is_err() {
        return raw.to_string();
    }

    if let Some(host) = url.host_str() {
        let host = host.to_ascii_lowercase();
        if url.set_host(Some(&host)).is_err() {
            return raw.to_string();
        }
    }

    let _ = url.set_username("");
    let _ = url.set_password(None);

    let path = url.path();
    if path.len() > 1 && path.ends_with('/') {
        let path = path.trim_end_matches('/').to_string();
        url.set_path(&path);
    }

    // Strip tracking parameters while preserving resource-identifying query
    // keys. Remaining pairs are sorted for canonical ordering.
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !is_tracking_param(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();

    if pairs.is_empty() {
        url.set_query(None);
    } else {
        let qs = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&pairs)
            .finish();
        url.set_query(Some(&qs));
    }

    url.set_fragment(None);

    url.to_string()
}

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
            match Database::open(path) {
                Ok(db) => Some(db),
                // A stale on-disk format (after a redb major upgrade) can't be recreated
                // here — dry-run must not mutate the vault. Treat it as no usable prior
                // state (every event novel), which the next real run fixes by recreating.
                Err(redb::DatabaseError::UpgradeRequired(_)) => None,
                Err(e) => {
                    return Err(PipelineError::Dedup(format!("open db (read-only): {e}")));
                }
            }
        } else {
            None
        };
        Ok(Self {
            db,
            title_threshold,
        })
    }

    fn open_or_reset(path: &Path) -> Result<Database, PipelineError> {
        let mut backed_up = false;
        loop {
            let db = match Database::create(path) {
                Ok(db) => db,
                Err(redb::DatabaseError::UpgradeRequired(_)) if !backed_up => {
                    Self::backup_stale_cache(path, "file format outdated")?;
                    backed_up = true;
                    continue;
                }
                Err(e) => return Err(PipelineError::Dedup(format!("open db: {e}"))),
            };

            let txn = db
                .begin_write()
                .map_err(|e| PipelineError::Dedup(e.to_string()))?;

            let init = (|| -> Result<(), redb::TableError> {
                txn.open_table(EVENT_IDS)?;
                txn.open_table(CONTENT_HASHES)?;
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
                Err(e) if is_schema_mismatch(&e) && !backed_up => {
                    drop(txn);
                    drop(db);
                    Self::backup_stale_cache(path, "schema changed")?;
                    backed_up = true;
                    continue;
                }
                Err(e) => {
                    return Err(PipelineError::Dedup(format!("init tables: {e}")));
                }
            }
        }
    }

    fn backup_stale_cache(path: &Path, reason: &str) -> Result<(), PipelineError> {
        let backup = path.with_extension(format!(
            "redb.backup.{}-pid{}",
            jiff::Timestamp::now().as_second(),
            std::process::id(),
        ));
        std::fs::rename(path, &backup)
            .map_err(|e| PipelineError::Dedup(format!("backup stale cache: {e}")))?;
        tracing::warn!(
            backup = %backup.display(),
            reason,
            "dedup database format outdated; created backup and started fresh"
        );
        Ok(())
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
        // Open all persisted tables. If any table is missing (e.g. an older
        // dedup.redb predates the content-hash column, or this is a read-only
        // dry-run against a fresh DB), fall back to None — intra-batch dedup
        // still runs via the in-memory seen_* sets.
        let tables = read_txn.as_ref().and_then(|txn| {
            Some((
                txn.open_table(EVENT_IDS).ok()?,
                txn.open_table(CONTENT_HASHES).ok()?,
                txn.open_table(URLS).ok()?,
                txn.open_table(TITLES).ok()?,
            ))
        });

        let mut novel: Vec<Event> = Vec::with_capacity(events.len());
        // Per-run sets so two identical events in the SAME input batch don't both pass
        // as novel — the persisted tables only reflect prior committed runs.
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut seen_hashes: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut seen_urls: std::collections::HashSet<String> = std::collections::HashSet::new();

        for event in events {
            let mut dup = false;

            for strategy in cascade {
                match strategy {
                    DedupStrategy::EventId => {
                        let in_cache = match &tables {
                            Some((ids, _, _, _)) => ids
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
                    DedupStrategy::ContentHash => {
                        let hash = event.content_hash.as_str();
                        let in_cache = match &tables {
                            Some((_, hashes, _, _)) => hashes
                                .get(hash)
                                .map_err(|e| PipelineError::Dedup(e.to_string()))?
                                .is_some(),
                            None => false,
                        };
                        if seen_hashes.contains(hash) || in_cache {
                            dup = true;
                            break;
                        }
                    }
                    DedupStrategy::Url => {
                        if let Some(ref url) = event.url {
                            let url = normalize_url(url);
                            let in_cache = match &tables {
                                Some((_, _, urls, _)) => urls
                                    .get(url.as_str())
                                    .map_err(|e| PipelineError::Dedup(e.to_string()))?
                                    .is_some(),
                                None => false,
                            };
                            if seen_urls.contains(&url) || in_cache {
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
                        if let Some((_, _, _, title_table)) = &tables {
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
                seen_hashes.insert(event.content_hash.clone());
                if let Some(ref url) = event.url {
                    seen_urls.insert(normalize_url(url));
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
            let mut hashes = txn
                .open_table(CONTENT_HASHES)
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
                hashes
                    .insert(event.content_hash.as_str(), now)
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?;

                if let Some(ref url) = event.url {
                    let url = normalize_url(url);
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
            for table_def in [EVENT_IDS, CONTENT_HASHES, URLS, TITLES] {
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
    use lk_core::config::SourceType;
    use lk_core::event::{Event, EventId};
    use tempfile::TempDir;

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
            content_hash: lk_core::event::content_hash(title, ""),
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn normalize_url_removes_trailing_slash() {
        assert_eq!(
            normalize_url("https://example.com/path/"),
            "https://example.com/path"
        );
    }

    #[test]
    fn normalize_url_strips_tracking_params_preserves_resource_params() {
        assert_eq!(
            normalize_url("https://example.com/path?utm_source=feed&id=1"),
            "https://example.com/path?id=1"
        );
    }

    #[test]
    fn normalize_url_strips_all_tracking_variants() {
        assert_eq!(
            normalize_url(
                "https://example.com/page?utm_source=x&utm_medium=y&utm_campaign=z&utm_term=a&utm_content=b&fbclid=abc&gclid=def&mc_cid=ghi&mc_eid=jkl&_hsenc=mno&_hsmi=pqr&ref=stu"
            ),
            "https://example.com/page"
        );
    }

    #[test]
    fn normalize_url_preserves_resource_identifying_params() {
        // YouTube video ID
        assert_eq!(
            normalize_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ&utm_source=share"),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        // Generic resource IDs
        assert_eq!(
            normalize_url("https://example.com/item?id=123&page=2"),
            "https://example.com/item?id=123&page=2"
        );
    }

    #[test]
    fn normalize_url_sorts_remaining_params() {
        assert_eq!(
            normalize_url("https://example.com/path?z=3&a=1&m=2"),
            "https://example.com/path?a=1&m=2&z=3"
        );
    }

    #[test]
    fn normalize_url_removes_fragment() {
        assert_eq!(
            normalize_url("https://example.com/path#section"),
            "https://example.com/path"
        );
    }

    #[test]
    fn normalize_url_lowercases_scheme_and_host() {
        assert_eq!(
            normalize_url("HTTPS://EXAMPLE.COM/path"),
            "https://example.com/path"
        );
    }

    #[test]
    fn normalize_url_leaves_already_normalized_url_unchanged() {
        assert_eq!(
            normalize_url("https://example.com/path"),
            "https://example.com/path"
        );
    }

    #[test]
    fn normalize_url_leaves_non_url_string_unchanged() {
        assert_eq!(normalize_url("not a url"), "not a url");
    }

    #[test]
    fn normalize_url_upgrades_http_to_https() {
        assert_eq!(
            normalize_url("http://example.com/path"),
            "https://example.com/path"
        );
    }

    #[test]
    fn normalize_url_strips_auth_info() {
        assert_eq!(
            normalize_url("https://user:pass@example.com/path"),
            "https://example.com/path"
        );
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
    fn content_hash_dedup_catches_same_content_different_source() {
        // The same article ingested via two sources (different event IDs and URLs)
        // collapses to one event when content-hash is in the cascade.
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), 0.85).unwrap();
        let cascade = vec![DedupStrategy::ContentHash];
        let a = ev("from-rss", "Anthropic releases Opus 4.7", None);
        let b = ev("from-mail", "Anthropic releases Opus 4.7", None);
        let novel = cache.deduplicate(vec![a, b], &cascade).unwrap();
        assert_eq!(novel.len(), 1);
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
    fn url_dedup_matches_normalized_urls() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), 0.85).unwrap();
        let cascade = vec![DedupStrategy::Url];

        cache
            .record(&[ev(
                "a",
                "First",
                Some("HTTPS://EXAMPLE.COM/post/?utm_source=feed#section"),
            )])
            .unwrap();

        let dup = vec![ev("b", "Reposted", Some("https://example.com/post"))];
        assert_eq!(cache.deduplicate(dup, &cascade).unwrap().len(), 0);
    }

    #[test]
    fn url_dedup_distinguishes_different_resource_params() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), 0.85).unwrap();
        let cascade = vec![DedupStrategy::Url];

        cache
            .record(&[ev(
                "a",
                "Video A",
                Some("https://www.youtube.com/watch?v=aaa"),
            )])
            .unwrap();

        // Same host/path but different resource param is novel.
        let novel = vec![ev(
            "b",
            "Video B",
            Some("https://www.youtube.com/watch?v=bbb"),
        )];
        assert_eq!(cache.deduplicate(novel, &cascade).unwrap().len(), 1);

        // Same resource param with extra tracking is still a duplicate.
        let dup = vec![ev(
            "c",
            "Video A reshared",
            Some("https://www.youtube.com/watch?v=aaa&utm_source=twitter"),
        )];
        assert_eq!(cache.deduplicate(dup, &cascade).unwrap().len(), 0);
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
