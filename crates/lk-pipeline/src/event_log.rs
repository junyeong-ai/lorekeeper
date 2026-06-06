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
/// their whole window on demand and keep NO log — see `SourceType::is_streaming`.
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
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(PipelineError::EventLog(format!(
                    "read {}: {e}",
                    path.display()
                )));
            }
        };
        let mut events = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let ev = serde_json::from_str::<Event>(line).map_err(|e| {
                PipelineError::EventLog(format!(
                    "{} is corrupt at line {}: {e} (left intact — recover or delete it)",
                    path.display(),
                    i + 1
                ))
            })?;
            events.push(ev);
        }
        Ok(events)
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
}
