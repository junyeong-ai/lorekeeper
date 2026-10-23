use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use lk_core::event::Event;

use crate::PipelineError;

/// Durable per-date record of the raw events observed for a STREAMING source — the SOURCE
/// OF TRUTH its daily page projects. One JSONL file per `(source, date)` under
/// `.lorekeeper/events/{source}/{date}.jsonl`, one `Event` per line.
///
/// Why it exists: a daily page is re-rendered IN FULL from the events available for that
/// date. A streaming source (RSS) fetches a rolling, capped window — an item observed on
/// day N scrolls out of the feed before a later run re-renders page N, and re-rendering
/// from the depleted fetch alone would silently drop it. So `plan` UNIONs each fetch with
/// the stored log (`merge_by_id`) and the page projects the merged set: nothing observed
/// is ever lost. Complete-refetch sources (Gmail/Jira/Calendar/Slack/Drive) reproduce
/// their whole window on demand and keep NO log — see `SourceDescriptor::streaming`.
///
/// This is the OPPOSITE of a suppression cache: it never blocks regeneration, it ENABLES
/// it. Delete a page → the next render reproduces it from the log (self-heal); `--date`
/// repairs any day. The log holds RAW (pre-LLM) bodies, so a re-render always feeds the
/// refine task raw text — there is no per-block refine state and no skill change.
pub struct EventLog {
    root: PathBuf,
}

impl EventLog {
    /// `vault_root` is the vault directory; the log lives under `.lorekeeper/events`.
    pub fn new(vault_root: &Path) -> Self {
        Self {
            root: vault_root.join(".lorekeeper").join("events"),
        }
    }

    fn path(&self, source_id: &str, date: jiff::civil::Date) -> PathBuf {
        self.root.join(source_id).join(format!("{date}.jsonl"))
    }

    /// Read the stored events for `(source, date)`, or an empty vec if none have been
    /// recorded yet. An unparseable line is a HARD ERROR, never skipped: the caller would
    /// otherwise re-write the parsed subset and silently drop the corrupt event forever. We
    /// produce the log only via atomic temp+fsync+rename, so a malformed line means external
    /// corruption — surface it (the source fails this run, the log is left intact for
    /// recovery) instead of amplifying it into permanent loss.
    pub fn read(
        &self,
        source_id: &str,
        date: jiff::civil::Date,
    ) -> Result<Vec<Event>, PipelineError> {
        let path = self.path(source_id, date);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(PipelineError::EventLog(format!(
                    "read {}: {e}",
                    path.display()
                )));
            }
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut line_number = 0;
        let mut events = Vec::new();
        let mut corruption = None;
        loop {
            line.clear();
            if reader
                .read_line(&mut line)
                .map_err(|e| PipelineError::EventLog(format!("read {}: {e}", path.display())))?
                == 0
            {
                break;
            }
            line_number += 1;
            // Validate the whole file's UTF-8 and IO before reporting JSON corruption.
            if corruption.is_some() {
                continue;
            }
            let line = line
                .strip_suffix('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line))
                .unwrap_or(&line);
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(line) {
                Ok(event) => events.push(event),
                Err(e) => {
                    corruption = Some(PipelineError::EventLog(format!(
                        "{} is corrupt at line {}: {e} (left intact — recover or delete it)",
                        path.display(),
                        line_number,
                    )))
                }
            }
        }
        match corruption {
            Some(error) => Err(error),
            None => Ok(events),
        }
    }

    /// Atomically replace the log for `(source, date)` with `events` (temp + fsync +
    /// rename), so a crash mid-write never leaves a half-written log on disk. One `lore
    /// ingest` plans its sources sequentially, so there is no in-run race; two ingests run
    /// concurrently against the same source is unsupported (last writer wins) — harmless
    /// for a streaming feed, whose items reappear and re-merge on the next run.
    pub fn write(
        &self,
        source_id: &str,
        date: jiff::civil::Date,
        events: &[Event],
    ) -> Result<(), PipelineError> {
        let path = self.path(source_id, date);
        let dir = path.parent().expect("log path always has a parent");
        std::fs::create_dir_all(dir)
            .map_err(|e| PipelineError::EventLog(format!("create dir {}: {e}", dir.display())))?;

        let mut buf = String::with_capacity(events.len() * 256);
        for ev in events {
            let line = serde_json::to_string(ev)
                .map_err(|e| PipelineError::EventLog(format!("serialize event: {e}")))?;
            buf.push_str(&line);
            buf.push('\n');
        }

        lk_core::fs::write_atomic(&path, buf.as_bytes(), None)
            .map_err(|e| PipelineError::EventLog(format!("write {}: {e}", path.display())))?;
        Ok(())
    }
}

/// Union the stored events with this run's freshly-fetched events for one date, keyed by
/// [`EventId`](lk_core::event::EventId). A fresh event WINS on collision — a still-in-feed
/// item picks up any in-place edit to its content; a stored-only event (one that has
/// scrolled out of the feed) is preserved. The result is sorted by `(timestamp desc, id)`
/// so the page is deterministic regardless of fetch order.
pub fn merge_by_id(stored: Vec<Event>, fresh: Vec<Event>) -> Vec<Event> {
    use std::collections::HashMap;

    let mut by_id: HashMap<String, Event> = HashMap::with_capacity(stored.len() + fresh.len());
    for ev in stored {
        by_id.insert(ev.id.as_str().to_string(), ev);
    }
    for ev in fresh {
        by_id.insert(ev.id.as_str().to_string(), ev); // fresh wins
    }
    let mut merged: Vec<Event> = by_id.into_values().collect();
    merged.sort_by(Event::canonical_cmp);
    merged
}

pub fn merge_streaming_events(
    stored: Vec<Event>,
    mut fresh: Vec<Event>,
) -> Result<Vec<Event>, PipelineError> {
    use lk_core::{config::SourceType, rss_original::RssEntryIdentity};
    use std::collections::HashMap;

    let failure =
        |reason: &str| PipelineError::EventLog(format!("ambiguous RSS entry ownership: {reason}"));
    let mut by_id = HashMap::new();
    let mut owners = HashMap::new();
    for event in &stored {
        if let Some(previous) = by_id.insert(event.id.as_str(), event)
            && previous != event
        {
            return Err(failure("retained event id has different observations"));
        }
        let Some(identity) =
            RssEntryIdentity::from_metadata(&event.metadata).map_err(PipelineError::EventLog)?
        else {
            continue;
        };
        if event.source_type != SourceType::Rss
            || (event.id != identity.event_id(&event.source_id, event.date)
                && event.id != identity.legacy_event_id(&event.source_id, event.date))
        {
            return Err(failure("retained identity does not name its event"));
        }
        let key = (event.source_id.clone(), event.date, identity);
        if let Some(previous) = owners.insert(key, event.id.clone())
            && previous != event.id
        {
            return Err(failure("one entry has multiple retained event ids"));
        }
    }

    let mut selected = HashMap::new();
    for event in &mut fresh {
        let Some(identity) =
            RssEntryIdentity::from_metadata(&event.metadata).map_err(PipelineError::EventLog)?
        else {
            continue;
        };
        if event.source_type != SourceType::Rss
            || event.id != identity.event_id(&event.source_id, event.date)
        {
            return Err(failure("fresh identity does not name its event"));
        }
        let key = (event.source_id.clone(), event.date, identity.clone());
        let mut owner = owners.get(&key).cloned();
        let modern = by_id.get(event.id.as_str());
        if let Some(retained) = modern {
            if RssEntryIdentity::from_metadata(&retained.metadata)
                .map_err(PipelineError::EventLog)?
                .as_ref()
                != Some(&identity)
            {
                return Err(failure("modern event id has different retained provenance"));
            }
            owner = Some(retained.id.clone());
        }
        let legacy = identity.legacy_event_id(&event.source_id, event.date);
        if let Some(retained) = by_id.get(legacy.as_str()) {
            match retained
                .metadata
                .get("feed_id")
                .and_then(serde_json::Value::as_str)
            {
                Some(feed_id) if feed_id == identity.feed_id => {
                    if retained.source_type != SourceType::Rss
                        || retained.source_id != event.source_id
                        || retained.date != event.date
                        || RssEntryIdentity::from_metadata(&retained.metadata)
                            .map_err(PipelineError::EventLog)?
                            .is_some_and(|other| other != identity)
                        || owner.as_ref().is_some_and(|owner| *owner != retained.id)
                    {
                        return Err(failure("entry has conflicting modern and retained owners"));
                    }
                    owner = Some(retained.id.clone());
                }
                Some(_) => {}
                None => return Err(failure("legacy event lacks a verified feed id")),
            }
        }
        if let Some(owner) = owner {
            event.id = owner;
        }
        if let Some(previous) = selected.insert(event.id.clone(), identity.clone())
            && previous != identity
        {
            return Err(failure(
                "distinct fresh entries selected one retained event",
            ));
        }
    }
    Ok(merge_by_id(stored, fresh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::config::SourceType;
    use lk_core::event::EventId;
    use tempfile::TempDir;

    fn ev(external_id: &str, title: &str, secs: i64) -> Event {
        let date = jiff::civil::date(2026, 5, 23);
        Event {
            id: EventId::new("ai-news", date, external_id),
            source_id: "ai-news".into(),
            source_type: SourceType::Rss,
            timestamp: jiff::Timestamp::from_second(secs).unwrap(),
            date,
            title: title.into(),
            body: "raw body".into(),
            url: Some(format!("https://example.com/{external_id}")),
            author: None,
            labels: vec![],
            category: None,
            performance_category: None,
            is_self: false,
            is_personal: false,
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn read_absent_log_is_empty() {
        let dir = TempDir::new().unwrap();
        let log = EventLog::new(dir.path());
        let date = jiff::civil::date(2026, 5, 23);
        assert!(log.read("ai-news", date).unwrap().is_empty());
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = TempDir::new().unwrap();
        let log = EventLog::new(dir.path());
        let date = jiff::civil::date(2026, 5, 23);
        let events = vec![ev("a", "A", 100), ev("b", "B", 200)];
        log.write("ai-news", date, &events).unwrap();
        let back = log.read("ai-news", date).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].id, events[0].id);
    }

    #[test]
    fn read_errors_on_a_corrupt_line_instead_of_dropping_it() {
        // A corrupt line must NOT be silently skipped: the caller would re-write the parsed
        // subset and lose the event forever. Surface it and leave the file intact.
        let dir = TempDir::new().unwrap();
        let log = EventLog::new(dir.path());
        let date = jiff::civil::date(2026, 5, 23);
        log.write("ai-news", date, &[ev("a", "A", 100)]).unwrap();
        let path = dir
            .path()
            .join(".lorekeeper")
            .join("events")
            .join("ai-news")
            .join("2026-05-23.jsonl");
        std::fs::write(&path, "{not valid json\n").unwrap();
        assert!(log.read("ai-news", date).is_err());
        assert!(path.exists(), "the corrupt log is left intact for recovery");
    }

    #[test]
    fn read_preserves_complete_events_across_buffer_and_line_boundaries() {
        let dir = TempDir::new().unwrap();
        let log = EventLog::new(dir.path());
        let date = jiff::civil::date(2026, 5, 23);
        let mut first = ev("a", "한글 source", 100);
        first.body = format!("    # literal\r\n\n{}\n```\ncode\n```\n", "가".repeat(8193));
        let events = vec![first, ev("b", "Second", 200)];
        let first = serde_json::to_string(&events[0]).unwrap();
        let second = serde_json::to_string(&events[1]).unwrap();
        let path = log.path("ai-news", date);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for raw in [
            format!("{first}\n\n{second}\n"),
            format!("\n \t\r\n{first}\r\n\r\n{second}"),
            format!("{first}\r\n{second}\r\n"),
        ] {
            std::fs::write(&path, &raw).unwrap();
            assert_eq!(log.read("ai-news", date).unwrap(), events);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        }
    }

    #[test]
    fn read_rejects_invalid_utf8_after_multiple_lines_without_returning_a_prefix() {
        let dir = TempDir::new().unwrap();
        let log = EventLog::new(dir.path());
        let date = jiff::civil::date(2026, 5, 23);
        let first = serde_json::to_string(&ev("a", "A", 100)).unwrap();
        let path = log.path("ai-news", date);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for opening in [first.as_str(), "{not valid json"] {
            let mut raw = format!("{opening}\n\n").into_bytes();
            raw.extend_from_slice(&[0xff, b'\n']);
            raw.extend_from_slice(first.as_bytes());
            std::fs::write(&path, &raw).unwrap();
            let error = log.read("ai-news", date).unwrap_err().to_string();
            assert!(error.contains("read "), "{error}");
            assert!(!error.contains("corrupt at line"), "{error}");
            assert_eq!(std::fs::read(&path).unwrap(), raw);
        }
    }

    #[test]
    fn read_reports_truncated_json_at_its_original_physical_line() {
        let dir = TempDir::new().unwrap();
        let log = EventLog::new(dir.path());
        let date = jiff::civil::date(2026, 5, 23);
        let first = serde_json::to_string(&ev("a", "A", 100)).unwrap();
        let path = log.path("ai-news", date);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for ending in ["", "\n", "\r\n"] {
            let raw = format!("{first}\n\n \t\r\n{{\"id\":{ending}");
            std::fs::write(&path, &raw).unwrap();
            let error = log.read("ai-news", date).unwrap_err().to_string();
            let expected = serde_json::from_str::<Event>("{\"id\":").unwrap_err();
            assert!(error.contains("corrupt at line 4"), "{error}");
            assert!(error.contains(&expected.to_string()), "{error}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        }
    }

    #[test]
    fn merge_preserves_stored_only_and_fresh_wins() {
        // Stored {a, b}; fresh {a(updated), c}. Result = {a(fresh), b(preserved), c}.
        let stored = vec![ev("a", "A-old", 300), ev("b", "B", 200)];
        let fresh = vec![ev("a", "A-new", 300), ev("c", "C", 100)];
        let merged = merge_by_id(stored, fresh);
        assert_eq!(
            merged.len(),
            3,
            "stored-only 'b' is preserved (not depleted)"
        );
        let a = merged.iter().find(|e| e.title.starts_with("A")).unwrap();
        assert_eq!(a.title, "A-new", "fresh wins on id collision");
    }

    #[test]
    fn merge_is_deterministically_ordered_newest_first() {
        let merged = merge_by_id(vec![], vec![ev("a", "A", 100), ev("b", "B", 300)]);
        let titles: Vec<_> = merged.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["B", "A"], "sorted by timestamp descending");
    }

    fn rss_event(feed: &str, entry: &str, title: &str, legacy: bool) -> Event {
        let identity = lk_core::rss_original::RssEntryIdentity::new(feed, entry);
        let mut event = ev(&identity.external_id(), title, 100);
        event.timestamp = "2026-05-23T00:00:00Z".parse().unwrap();
        event.body = title.into();
        event.url = None;
        event.metadata = serde_json::json!({"feed_id":feed});
        if legacy {
            event.id = identity.legacy_event_id(&event.source_id, event.date);
        } else {
            event.metadata["rss_identity"] = serde_json::to_value(identity).unwrap();
        }
        event
    }

    #[test]
    fn framed_rss_entries_remain_distinct_and_reingestion_is_stable() {
        let first = rss_event("org:news", "item1", "First", false);
        let second = rss_event("org", "news:item1", "Second", false);
        let merged = merge_streaming_events(vec![], vec![first.clone(), second.clone()]).unwrap();
        assert_eq!(merged.len(), 2);
        assert_ne!(merged[0].id, merged[1].id);
        assert_eq!(
            merge_streaming_events(merged.clone(), vec![second, first]).unwrap(),
            merged
        );
    }

    #[test]
    fn retained_rss_identity_preserves_prior_originals_and_exact_selectors() {
        use lk_core::rss_original::RssOriginalCapture;
        let temp = TempDir::new().unwrap();
        let log = EventLog::new(temp.path());
        let old = rss_event("org:news", "item1", "Old body", true);
        let stored_only = rss_event("other", "off-feed", "Retained", true);
        let original = RssOriginalCapture::from_events(&[&old, &stored_only]).unwrap();
        let session = lk_vault::MutationSession::acquire(temp.path()).unwrap();
        let reference = lk_vault::publish_rss_original(&session, &original).unwrap();
        let manifest_path = temp.path().join(&reference.manifest_path);
        let manifest_bytes = std::fs::read(&manifest_path).unwrap();
        let selection = lk_core::rss_evidence::RssEventSelection {
            event_id: original.manifest.events[0].event_id.clone(),
            revision: original.manifest.events[0].revision.clone(),
        };
        let selected_before =
            lk_vault::load_rss_selected_payload(temp.path(), &original.manifest, &selection)
                .unwrap();
        log.write("ai-news", old.date, &[old.clone(), stored_only.clone()])
            .unwrap();

        let updated = rss_event("org:news", "item1", "Updated body", false);
        let other = rss_event("org", "news:item1", "Independent", false);
        let merged = merge_streaming_events(
            log.read("ai-news", old.date).unwrap(),
            vec![other.clone(), updated.clone()],
        )
        .unwrap();
        assert_eq!(merged.len(), 3);
        assert_eq!(
            merged
                .iter()
                .find(|event| event.title == "Updated body")
                .unwrap()
                .id,
            old.id
        );
        assert_eq!(
            merged
                .iter()
                .find(|event| event.title == "Independent")
                .unwrap()
                .id,
            other.id
        );
        assert!(merged.contains(&stored_only));
        log.write("ai-news", old.date, &merged).unwrap();
        let repeated =
            merge_streaming_events(log.read("ai-news", old.date).unwrap(), vec![updated, other])
                .unwrap();
        assert_eq!(repeated, merged);
        let new_capture =
            RssOriginalCapture::from_events(&repeated.iter().collect::<Vec<_>>()).unwrap();
        lk_vault::publish_rss_original(&session, &new_capture).unwrap();
        assert_eq!(std::fs::read(manifest_path).unwrap(), manifest_bytes);
        assert_eq!(
            lk_vault::load_rss_selected_payload(temp.path(), &original.manifest, &selection)
                .unwrap(),
            selected_before
        );
    }

    #[test]
    fn uncertain_or_competing_retained_rss_owners_refuse_reconciliation() {
        let modern = rss_event("org:news", "item1", "Modern", false);
        let legacy = rss_event("org:news", "item1", "Legacy", true);
        let cases = [
            vec![legacy.clone(), modern.clone()],
            vec![{
                let mut event = legacy.clone();
                event.id = modern.id.clone();
                event.metadata["feed_id"] = "different-feed".into();
                event
            }],
            vec![{
                let mut event = legacy.clone();
                event.metadata = serde_json::Value::Null;
                event
            }],
            vec![{
                let mut event = modern.clone();
                event.metadata["rss_identity"]["entry_id"] = "other".into();
                event
            }],
        ];
        for stored in cases {
            assert!(merge_streaming_events(stored, vec![modern.clone()]).is_err());
        }
    }
}
