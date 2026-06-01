use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use lk_core::config::DedupStrategy;
use lk_core::event::Event;

use crate::PipelineError;

const EVENT_IDS: TableDefinition<&str, u64> = TableDefinition::new("event_ids");
const CONTENT_HASHES: TableDefinition<&str, u64> = TableDefinition::new("content_hashes");
const URLS: TableDefinition<&str, u64> = TableDefinition::new("urls");

/// Exact query-parameter keys stripped during URL canonicalisation. These are
/// vendor-tracking parameters whose presence varies by attribution path but
/// never carry resource-identifying information. (Bare `ref` is deliberately NOT
/// here — GitHub/GitLab use it as a revision selector; only `ref_src` is tracking.)
const TRACKING_QUERY_EXACT_KEYS: &[&str] = &[
    "fbclid",
    "gclid",
    "mc_cid",
    "mc_eid",
    "_hsenc",
    "_hsmi",
    "msclkid",
    "ttclid",
    "twclid",
    "wbraid",
    "gbraid",
    "igshid",
    "ref_src",
    "vero_id",
    "vero_conv",
    "oly_enc_id",
    "oly_anon_id",
    "cmpid",
    "ncid",
    "mkt_tok",
];

/// Query-key prefixes stripped during URL canonicalisation. Only genuine
/// prefixes that always carry a trailing discriminator (utm_source,
/// utm_medium, utm_campaign, utm_term, utm_content).
const TRACKING_QUERY_PREFIXES: &[&str] = &["utm_"];

/// A tracking key per the built-in set OR a user-supplied `extra` entry. An extra
/// entry ending in `*` matches a prefix; otherwise it must match the key exactly.
/// Entries are trimmed so they agree with config validation (which trims before
/// checking) regardless of incidental surrounding whitespace.
fn is_tracking_param(key: &str, extra: &[String]) -> bool {
    TRACKING_QUERY_EXACT_KEYS.contains(&key)
        || TRACKING_QUERY_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
        || extra.iter().any(|p| match p.trim().strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => key == p.trim(),
        })
}

pub(crate) fn normalize_url(raw: &str, extra_tracking: &[String]) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return raw.to_string();
    };

    // The host is case-insensitive under every scheme, so canonicalise it once up front.
    // (`mailto:` has no host, so its case-sensitive local-part is untouched.)
    if let Some(host) = url.host_str() {
        let host = host.to_ascii_lowercase();
        if url.set_host(Some(&host)).is_err() {
            return raw.to_string();
        }
    }

    let scheme = match url.scheme() {
        "http" | "https" => "https",
        other => {
            // Non-HTTP schemes (ftp, mailto, …) keep their scheme — it is semantically
            // meaningful — but still drop query/fragment so trivial variants dedup.
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

    let _ = url.set_username("");
    let _ = url.set_password(None);

    let path = url.path();
    if path.len() > 1 && path.ends_with('/') {
        let path = path.trim_end_matches('/').to_string();
        url.set_path(&path);
    }

    // On YouTube, `si` is a pure share/referral token, so the SAME video re-shared with
    // a different `si` must still dedup. It is kept globally (not in the built-in set)
    // because on other hosts `si` can be a resource selector — so it is stripped for
    // YouTube hosts ONLY.
    let strip_si = url.host_str().is_some_and(|h| {
        matches!(
            h,
            "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be"
        )
    });

    // Strip tracking parameters while preserving resource-identifying query
    // keys. Remaining pairs are sorted for canonical ordering.
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !(is_tracking_param(k, extra_tracking) || (strip_si && k == "si")))
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

    // Strip pure anchor fragments (`#section`, `#L42`) — they point within a single
    // page, not at a distinct resource. PRESERVE fragments that carry resource
    // identity, where dropping them would merge two distinct pages and silently drop
    // one observation: SPA hash routes (`#/issues/1`, `#!/path`) and query-like
    // fragments that encode a selector (`#gid=1` for a sheet tab, `#tab=2`, `#page=3`).
    let preserve_fragment = url
        .fragment()
        .is_some_and(|f| f.starts_with('/') || f.starts_with("!/") || f.contains(['=', '&']));
    if !preserve_fragment {
        url.set_fragment(None);
    }

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

/// Outcome of a `dedup` pass: the `novel` events to ingest, and the
/// `duplicates` that matched a prior observation. `duplicates` is retained so the
/// commit can refresh their `seen_at` timestamps — a steady-state re-arrival (an
/// evergreen RSS item, a recurring Jira issue) must not age past the retention
/// window and re-emit as new just because it kept being recognized as a duplicate.
pub struct DedupResult {
    pub novel: Vec<Event>,
    pub duplicates: Vec<Event>,
}

pub struct DedupCache {
    /// `None` is a read-only "empty" cache used by dry-run: it never creates or
    /// writes a file, treats every event as novel, and no-ops on record/prune.
    db: Option<Database>,
    /// User-configured extra tracking-param keys, merged with the built-ins during
    /// URL canonicalisation.
    extra_tracking: Vec<String>,
    /// True for the dry-run cache (`open_read_only`). A read-only cache legitimately
    /// has no persisted tables (it never created them), so a missing-table fallback
    /// is expected and silent; on a writable cache the same fallback is an anomaly
    /// worth warning about.
    read_only: bool,
}

impl DedupCache {
    pub fn open(path: &Path, extra_tracking: Vec<String>) -> Result<Self, PipelineError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PipelineError::Dedup(format!("create dir: {e}")))?;
        }

        let db = Self::open_or_reset(path)?;

        Ok(Self {
            db: Some(db),
            extra_tracking,
            read_only: false,
        })
    }

    /// Open WITHOUT creating anything — for dry-run, which must not touch the vault.
    /// If the cache file exists it is opened for reading so the preview reflects real
    /// dedup state; if it doesn't, every event is reported novel (as a real first run
    /// would also see), and no file is created.
    pub fn open_read_only(path: &Path, extra_tracking: Vec<String>) -> Result<Self, PipelineError> {
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
            extra_tracking,
            read_only: true,
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

    pub fn dedup(
        &self,
        events: Vec<Event>,
        cascade: &[DedupStrategy],
    ) -> Result<DedupResult, PipelineError> {
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
        // Open all persisted lookup tables. A table that simply does not exist yet
        // (a fresh DB, or a read-only dry-run against an older cache predating a
        // column) is the one benign reason to fall back to intra-batch-only dedup.
        // Any OTHER table error — corruption, an unexpected schema state — must
        // surface rather than silently disabling persisted dedup and re-emitting
        // every prior event as novel.
        let (tbl_ids, tbl_hashes, tbl_urls) = match read_txn.as_ref() {
            None => (None, None, None),
            Some(txn) => {
                let open = |def| match txn.open_table(def) {
                    Ok(table) => Ok(Some(table)),
                    Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
                    Err(e) => Err(PipelineError::Dedup(e.to_string())),
                };
                let tables = (open(EVENT_IDS)?, open(CONTENT_HASHES)?, open(URLS)?);
                // A writable cache always has every table (created at open time), so a
                // missing one is an anomaly worth surfacing. A read-only dry-run against an
                // older cache legitimately lacks a newer column — keep each table's `Option`
                // and consult whichever ARE present per strategy, rather than disabling all
                // persisted dedup the moment one is absent.
                if !self.read_only
                    && (tables.0.is_none() || tables.1.is_none() || tables.2.is_none())
                {
                    tracing::warn!(
                        "dedup cache missing lookup tables; persisted dedup degraded for this run"
                    );
                }
                tables
            }
        };

        let mut novel: Vec<Event> = Vec::with_capacity(events.len());
        let mut duplicates: Vec<Event> = Vec::new();
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
                        let in_cache = match &tbl_ids {
                            Some(ids) => ids
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
                        // Only events with a substantive body carry a content hash;
                        // title-only items have `None` and fall through to URL / event-id
                        // so two distinct posts sharing a headline are never merged.
                        if let Some(hash) = event.content_hash.as_deref() {
                            let in_cache = match &tbl_hashes {
                                Some(hashes) => hashes
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
                    }
                    DedupStrategy::Url => {
                        if let Some(ref url) = event.url {
                            let url = normalize_url(url, &self.extra_tracking);
                            let in_cache = match &tbl_urls {
                                Some(urls) => urls
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
                }
            }

            if !dup {
                seen_ids.insert(event.id.as_str().to_string());
                if let Some(hash) = &event.content_hash {
                    seen_hashes.insert(hash.clone());
                }
                if let Some(ref url) = event.url {
                    seen_urls.insert(normalize_url(url, &self.extra_tracking));
                }
                novel.push(event);
            } else {
                duplicates.push(event);
            }
        }

        Ok(DedupResult { novel, duplicates })
    }

    /// Upsert every event's keys with the current time in a single write transaction.
    /// Used for BOTH novel events (first record) and re-seen duplicates (timestamp
    /// refresh) — redb `insert` is an upsert, so re-recording a duplicate slides its
    /// `seen_at` forward and keeps it out of the prune window. Recording novel and
    /// re-seen events together (one transaction) keeps their timestamps atomic: a
    /// crash can never persist one set without the other.
    pub fn record<'a>(
        &self,
        events: impl IntoIterator<Item = &'a Event>,
    ) -> Result<(), PipelineError> {
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

            for event in events {
                ids.insert(event.id.as_str(), now)
                    .map_err(|e| PipelineError::Dedup(e.to_string()))?;
                if let Some(hash) = event.content_hash.as_deref() {
                    hashes
                        .insert(hash, now)
                        .map_err(|e| PipelineError::Dedup(e.to_string()))?;
                }

                if let Some(ref url) = event.url {
                    let url = normalize_url(url, &self.extra_tracking);
                    urls.insert(url.as_str(), now)
                        .map_err(|e| PipelineError::Dedup(e.to_string()))?;
                }
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
            for table_def in [EVENT_IDS, CONTENT_HASHES, URLS] {
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

    /// Canonicalise with the built-in tracking set only (no config extras) — what
    /// the default cascade uses. Shadows the two-arg `super::normalize_url`.
    fn normalize_url(raw: &str) -> String {
        super::normalize_url(raw, &[])
    }

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
            performance_category: None,
            is_self: false,
            is_personal: false,
            content_hash: lk_core::event::content_hash(date, title, ""),
            metadata: serde_json::Value::Null,
        }
    }

    /// Like `ev` but with a substantive body, so the event carries a content hash
    /// (content-hash dedup only applies to events with real content).
    fn ev_body(id_suffix: &str, title: &str, body: &str, url: Option<&str>) -> Event {
        let date = jiff::civil::date(2026, 5, 23);
        Event {
            body: body.into(),
            content_hash: lk_core::event::content_hash(date, title, body),
            ..ev(id_suffix, title, url)
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
                "https://example.com/page?utm_source=x&utm_medium=y&utm_campaign=z&utm_term=a&utm_content=b&fbclid=abc&gclid=def&mc_cid=ghi&mc_eid=jkl&_hsenc=mno&_hsmi=pqr"
            ),
            "https://example.com/page"
        );
    }

    #[test]
    fn normalize_url_preserves_ref_as_resource_identifier() {
        // `ref` identifies a branch/revision/view in many doc & code URLs (GitHub,
        // GitLab, docs sites) — resource-identifying, not tracking. It must survive
        // canonicalization so distinct revisions aren't merged as one resource.
        assert_eq!(
            normalize_url("https://github.com/o/r/blob/x.rs?ref=v2"),
            "https://github.com/o/r/blob/x.rs?ref=v2"
        );
    }

    #[test]
    fn normalize_url_strips_unambiguous_social_tracking_params() {
        // Instagram `igshid` and Twitter `ref_src` are unambiguous attribution tokens
        // and come off. Single-letter ambiguous params (e.g. `si`) are deliberately NOT
        // in the built-in set — they can be a resource selector on some hosts, so
        // stripping them globally would merge distinct pages. Opt in via
        // `dedup.extra_tracking_params` if a specific source needs it.
        assert_eq!(
            super::normalize_url(
                "https://example.com/post?igshid=xyz&ref_src=twsrc&id=42",
                &[],
            ),
            "https://example.com/post?id=42"
        );
        // `si` survives by default (resource-identifying on non-YouTube hosts).
        assert_eq!(
            super::normalize_url("https://example.com/track?si=keepme", &[]),
            "https://example.com/track?si=keepme"
        );
    }

    #[test]
    fn normalize_url_strips_si_on_youtube_only() {
        // On YouTube `si` is a share/referral token: the same video re-shared with a
        // different `si` must canonicalize identically (so it dedups), while `v` survives.
        assert_eq!(
            normalize_url("https://youtu.be/dQw4w9WgXcQ?si=AaA"),
            "https://youtu.be/dQw4w9WgXcQ"
        );
        assert_eq!(
            normalize_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ&si=BbB"),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        // Two shares of the same video with different `si` collapse to one canonical URL.
        assert_eq!(
            normalize_url("https://youtu.be/abc?si=one"),
            normalize_url("https://youtu.be/abc?si=two"),
        );
    }

    #[test]
    fn normalize_url_strips_config_extra_params_exact_and_prefix() {
        let extra = vec!["pk_*".to_string(), "myref".to_string()];
        // Exact `myref` and prefix `pk_*` come off; the resource `id` stays.
        assert_eq!(
            super::normalize_url(
                "https://example.com/p?pk_campaign=c&pk_kwd=k&myref=r&id=9",
                &extra,
            ),
            "https://example.com/p?id=9"
        );
        // Without the config, those same params are preserved (sorted).
        assert_eq!(
            super::normalize_url("https://example.com/p?myref=r&id=9", &[]),
            "https://example.com/p?id=9&myref=r"
        );
    }

    #[test]
    fn normalize_url_strips_ad_platform_tracking_params() {
        assert_eq!(
            normalize_url("https://example.com/page?msclkid=abc123&id=42"),
            "https://example.com/page?id=42"
        );
        assert_eq!(
            normalize_url("https://example.com/page?ttclid=tt1&twclid=tw2"),
            "https://example.com/page"
        );
        assert_eq!(
            normalize_url("https://example.com/page?wbraid=w1&gbraid=g2&tab=news"),
            "https://example.com/page?tab=news"
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
    fn normalize_url_removes_anchor_fragment() {
        assert_eq!(
            normalize_url("https://example.com/path#section"),
            "https://example.com/path"
        );
    }

    #[test]
    fn normalize_url_preserves_hash_route_fragment() {
        // SPA hash routers encode resource identity in the fragment; two routes
        // must stay distinct so neither observation is dropped as a duplicate.
        assert_eq!(
            normalize_url("https://app.example.com/#/issues/1"),
            "https://app.example.com/#/issues/1"
        );
        assert_ne!(
            normalize_url("https://app.example.com/#/issues/1"),
            normalize_url("https://app.example.com/#/issues/2")
        );
    }

    #[test]
    fn normalize_url_preserves_query_like_fragment() {
        // A selector-bearing fragment (a sheet tab, a paginated view) is resource
        // identity, not an in-page anchor — dropping it would merge two distinct
        // resources and silently lose one observation.
        assert_eq!(
            normalize_url("https://docs.google.com/spreadsheets/d/X/edit#gid=0"),
            "https://docs.google.com/spreadsheets/d/X/edit#gid=0"
        );
        assert_ne!(
            normalize_url("https://docs.google.com/spreadsheets/d/X/edit#gid=0"),
            normalize_url("https://docs.google.com/spreadsheets/d/X/edit#gid=1")
        );
        // A pure anchor is still stripped.
        assert_eq!(
            normalize_url("https://example.com/doc#introduction"),
            "https://example.com/doc"
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
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();

        let cascade = vec![DedupStrategy::EventId];
        let events = vec![ev("a", "A", None), ev("b", "B", None)];

        let novel = cache.dedup(events.clone(), &cascade).unwrap().novel;
        assert_eq!(novel.len(), 2);

        cache.record(&events).unwrap();

        let novel2 = cache.dedup(events, &cascade).unwrap().novel;
        assert_eq!(novel2.len(), 0);
    }

    #[test]
    fn re_seen_events_are_returned_as_duplicates_for_timestamp_refresh() {
        // A recurring item recognized as a duplicate must be surfaced (not silently
        // dropped) so `commit` can refresh its `seen_at` and it never ages out of the
        // retention window to re-emit as new.
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();
        let cascade = vec![DedupStrategy::EventId];
        let events = vec![ev("a", "A", None)];
        cache.record(&events).unwrap();

        let result = cache.dedup(events, &cascade).unwrap();
        assert!(result.novel.is_empty());
        assert_eq!(
            result.duplicates.len(),
            1,
            "re-seen event must be returned so commit can refresh its timestamp"
        );
    }

    #[test]
    fn cascade_tries_each_strategy_in_order() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();
        let cascade = vec![DedupStrategy::EventId, DedupStrategy::Url];

        cache
            .record(&[ev("seed", "Seeded Title", Some("https://example.com/a"))])
            .unwrap();

        // Matches via Url only (different id, different title).
        let by_url = vec![ev(
            "other",
            "Totally different",
            Some("https://example.com/a"),
        )];
        assert_eq!(cache.dedup(by_url, &cascade).unwrap().novel.len(), 0);

        // Matches nothing in the cascade → novel (same title is NOT a dedup signal:
        // distinct observations that merely share a headline must both survive).
        let same_title = vec![ev("other2", "Seeded Title", None)];
        assert_eq!(cache.dedup(same_title, &cascade).unwrap().novel.len(), 1);

        // Matches nothing in the cascade → novel.
        let novel = vec![ev("fresh", "Brand New", Some("https://example.com/z"))];
        assert_eq!(cache.dedup(novel, &cascade).unwrap().novel.len(), 1);
    }

    #[test]
    fn content_hash_dedup_catches_same_content_different_source() {
        // The same article ingested via two sources ON THE SAME DAY (different event IDs
        // and URLs) collapses to one event when content-hash is in the cascade. Both
        // carry the SAME substantive body, which is what establishes content-equivalence.
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();
        let cascade = vec![DedupStrategy::ContentHash];
        let body = "Anthropic today announced Opus 4.7, its most capable model.";
        let a = ev_body("from-rss", "Anthropic releases Opus 4.7", body, None);
        let b = ev_body("from-mail", "Anthropic releases Opus 4.7", body, None);
        let novel = cache.dedup(vec![a, b], &cascade).unwrap().novel;
        assert_eq!(novel.len(), 1);
    }

    #[test]
    fn content_hash_does_not_merge_title_only_items() {
        // Two items sharing only a headline (no body, distinct URLs) are NOT provably the
        // same content. Content-hash must not merge them — a false merge silently drops a
        // distinct observation. They fall through the cascade and both survive.
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();
        let cascade = vec![DedupStrategy::ContentHash, DedupStrategy::Url];
        let a = ev("item-a", "Weekly roundup", Some("https://a.example/1"));
        let b = ev("item-b", "Weekly roundup", Some("https://b.example/2"));
        let novel = cache.dedup(vec![a, b], &cascade).unwrap().novel;
        assert_eq!(
            novel.len(),
            2,
            "title-only items with distinct URLs must both survive"
        );
    }

    #[test]
    fn content_hash_does_not_merge_same_content_across_days() {
        // A recurring/templated body with an identical title+body on two different days
        // is a distinct observation — content-hash is date-scoped, so both survive rather
        // than the later day silently collapsing into the earlier one.
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();
        let cascade = vec![DedupStrategy::ContentHash];
        let title = "Daily status: all systems operational";
        let body = "All systems operational. No incidents in the last 24h.";
        let mk = |date: jiff::civil::Date| Event {
            id: EventId::new("digest", date, &date.to_string()),
            source_id: "digest".into(),
            source_type: SourceType::Gmail,
            date,
            title: title.into(),
            body: body.into(),
            url: None,
            author: None,
            labels: vec![],
            classification: None,
            performance_category: None,
            is_self: false,
            is_personal: false,
            content_hash: lk_core::event::content_hash(date, title, body),
            metadata: serde_json::Value::Null,
        };
        let a = mk(jiff::civil::date(2026, 5, 23));
        let b = mk(jiff::civil::date(2026, 5, 24));
        let novel = cache.dedup(vec![a, b], &cascade).unwrap().novel;
        assert_eq!(
            novel.len(),
            2,
            "identical content on different days must both survive (date-scoped hash)"
        );
    }

    #[test]
    fn read_only_empty_cache_still_dedups_within_batch() {
        // Dry-run with no existing cache (open_read_only → None) must still drop
        // intra-batch duplicates so the preview matches a real run.
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open_read_only(&dir.path().join("absent.redb"), vec![]).unwrap();
        let cascade = vec![DedupStrategy::EventId];
        let batch = vec![ev("a", "A", None), ev("a", "A", None)];
        let novel = cache.dedup(batch, &cascade).unwrap().novel;
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
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();
        let cascade = vec![DedupStrategy::EventId, DedupStrategy::Url];

        // Two events with the same id in ONE batch (nothing committed yet).
        let batch = vec![
            ev("a", "A", Some("http://x")),
            ev("a", "A", Some("http://x")),
        ];
        let novel = cache.dedup(batch, &cascade).unwrap().novel;
        assert_eq!(
            novel.len(),
            1,
            "duplicate within the same batch must be dropped"
        );
    }

    #[test]
    fn url_dedup_filters_same_url_different_id() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();
        let cascade = vec![DedupStrategy::Url];

        cache
            .record(&[ev("a", "First", Some("https://example.com/post"))])
            .unwrap();

        // Same URL, different event-id and title → still a duplicate via the URL strategy.
        let dup = vec![ev("b", "Reposted", Some("https://example.com/post"))];
        assert_eq!(cache.dedup(dup, &cascade).unwrap().novel.len(), 0);

        // Different URL is novel.
        let novel = vec![ev("c", "Other", Some("https://example.com/other"))];
        assert_eq!(cache.dedup(novel, &cascade).unwrap().novel.len(), 1);
    }

    #[test]
    fn url_dedup_matches_normalized_urls() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();
        let cascade = vec![DedupStrategy::Url];

        cache
            .record(&[ev(
                "a",
                "First",
                Some("HTTPS://EXAMPLE.COM/post/?utm_source=feed#section"),
            )])
            .unwrap();

        let dup = vec![ev("b", "Reposted", Some("https://example.com/post"))];
        assert_eq!(cache.dedup(dup, &cascade).unwrap().novel.len(), 0);
    }

    #[test]
    fn url_dedup_distinguishes_different_resource_params() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();
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
        assert_eq!(cache.dedup(novel, &cascade).unwrap().novel.len(), 1);

        // Same resource param with extra tracking is still a duplicate.
        let dup = vec![ev(
            "c",
            "Video A reshared",
            Some("https://www.youtube.com/watch?v=aaa&utm_source=twitter"),
        )];
        assert_eq!(cache.dedup(dup, &cascade).unwrap().novel.len(), 0);
    }

    #[test]
    fn prune_removes_old_entries() {
        let dir = TempDir::new().unwrap();
        let cache = DedupCache::open(&dir.path().join("dedup.redb"), vec![]).unwrap();

        cache
            .record(&[ev_body(
                "a",
                "A",
                "a body that yields a content hash",
                Some("http://x"),
            )])
            .unwrap();

        let future_cutoff = jiff::Timestamp::now().as_second() as u64 + 1;
        let removed = cache.prune(future_cutoff).unwrap();
        assert!(
            removed >= 3,
            "should remove from all 3 tables (event_id + content_hash + url)"
        );
    }
}
