use std::collections::HashSet;

use lk_core::event::Event;

/// Collapse repeats of the SAME item within a single fetch batch, keeping the first
/// occurrence in order.
///
/// Identity is the [`EventId`](lk_core::event::EventId) — `source:date:hash(external_id)`,
/// the source's own stable per-item id. Two events collapse iff they share that id, which
/// only happens when one fetch surfaces the literal same item twice (e.g. paginated
/// history overlap). This is an EXACT-identity merge: it can never drop a distinct
/// observation. Items that merely share a url, a title, or a body are NOT merged — the
/// same article carried by two RSS feeds keeps both (each feed namespaces its id, so the
/// two are distinct provenance), and convergence to one knowledge asset happens in the
/// concept/graph layer, never here.
///
/// This is the ONLY deduplication the pipeline performs. An event's date pins it to one
/// `<daily>/{source}/{date}` page regardless of which overlapping window fetched it, and
/// that page is re-rendered IN FULL each run, so re-runs are byte-identical. (A streaming
/// source additionally unions the fetch with its per-date event log — see `event_log` —
/// but that log only ever ADDS to a page, it never suppresses an observation.)
pub fn deduplicate(events: Vec<Event>) -> Vec<Event> {
    let mut seen: HashSet<String> = HashSet::with_capacity(events.len());
    let mut kept = Vec::with_capacity(events.len());
    for event in events {
        if seen.insert(event.id.as_str().to_string()) {
            kept.push(event);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::config::SourceType;
    use lk_core::event::{Event, EventId};

    fn ev(external_id: &str, title: &str) -> Event {
        let date = jiff::civil::date(2026, 5, 23);
        Event {
            id: EventId::new("test", date, external_id),
            source_id: "test".into(),
            source_type: SourceType::Gmail,
            timestamp: jiff::Timestamp::UNIX_EPOCH,
            date,
            title: title.into(),
            body: String::new(),
            url: None,
            author: None,
            labels: vec![],
            classification: None,
            performance_category: None,
            is_self: false,
            is_personal: false,
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn collapses_repeats_of_the_same_id() {
        // The same item surfaced twice in one fetch (e.g. pagination overlap) is one event.
        let kept = deduplicate(vec![ev("msg-1", "A"), ev("msg-1", "A")]);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn keeps_distinct_ids_even_when_title_matches() {
        // Same headline is NOT a merge signal — two feeds carrying the same article (distinct
        // feed-namespaced ids) are distinct provenance and both survive.
        let kept = deduplicate(vec![
            ev("openai:weekly-roundup", "Weekly roundup"),
            ev("aggregator:weekly-roundup", "Weekly roundup"),
        ]);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn preserves_first_occurrence_order() {
        let kept = deduplicate(vec![ev("a", "A"), ev("b", "B"), ev("a", "A2")]);
        let titles: Vec<_> = kept.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["A", "B"]);
    }

    #[test]
    fn empty_batch_is_empty() {
        assert!(deduplicate(vec![]).is_empty());
    }
}
