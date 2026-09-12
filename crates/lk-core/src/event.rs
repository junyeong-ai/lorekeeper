use serde::{Deserialize, Serialize};

use crate::config::SourceType;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub source_id: String,
    pub source_type: SourceType,
    /// Precise instant the item was observed/published (carried from `RawItem`). `date`
    /// is its calendar day in the vault timezone — the page-bucketing key — while
    /// `timestamp` orders events deterministically WITHIN a day (newest first; see
    /// [`Event::canonical_cmp`]).
    pub timestamp: jiff::Timestamp,
    pub date: jiff::civil::Date,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    pub author: Option<String>,
    pub labels: Vec<String>,
    /// Daily-page grouping bucket assigned by `classify_by_keywords` from the
    /// source's `classify` rules (e.g. `action_required`, `decisions`). A
    /// presentation axis only — it groups events into sections on the rendered
    /// daily page. Deliberately SEPARATE from `performance_category`: "what kind of
    /// communication is this" is orthogonal to "what kind of contribution is this".
    /// A rule may set both, but they never share a value space.
    pub category: Option<String>,
    /// Personal-performance contribution bucket (e.g. `project-delivery`), used only
    /// for work-log / review category distribution. Set from a `classify` rule's
    /// optional `performance_category` field — the single EXPLICIT bridge from a
    /// content signal to the performance taxonomy. `None` lets `resolve_category` fall
    /// back to the coarse per-source-type map. Never inferred from free-form text.
    pub performance_category: Option<String>,
    /// Authored by the configured identity, as determined by the source adapter
    /// from its structured authorship fields (email From, message author id,
    /// issue assignee, calendar organizer/attendee). The deterministic ownership
    /// signal — never inferred downstream from free-form text.
    pub is_self: bool,
    /// `is_self` AND the source is in `personal.tracked_sources`: the event counts toward
    /// the user's personal work-log and reviews. Always `false` without a `personal:` module.
    pub is_personal: bool,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl Event {
    /// The single canonical render/storage order for events, shared by every path that
    /// materializes a page: newest first (`timestamp` descending), ties broken by `id`
    /// ascending for a total order. Because `id` is unique within a source/date, this is
    /// a deterministic total order independent of fetch/merge sequence — the property the
    /// byte-identical re-render and stable LLM-input-hash invariants rest on. Both the
    /// streaming event-log union (`merge_by_id`) and the complete-refetch daily path sort
    /// through this one comparator, so a page's bytes never depend on adapter/API order.
    ///
    /// Totality rests on `id` uniqueness within a sort: two events with the SAME `EventId`
    /// are by definition the same observation, and `dedup::deduplicate` collapses them
    /// before any sort runs — so a tie on both `timestamp` and `id` (which would leave order
    /// input-dependent) cannot occur on a deduplicated set.
    pub fn canonical_cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .timestamp
            .cmp(&self.timestamp)
            .then_with(|| self.id.as_str().cmp(other.id.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(String);

impl EventId {
    pub fn new(source_id: &str, date: jiff::civil::Date, content: &str) -> Self {
        let hash = blake3::hash(content.as_bytes());
        let hex = hash.to_hex();
        let short = &hex[..16];
        Self(format!("{source_id}:{date}:{short}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `blake3(content)[..16]` segment — the content-derived component of the id,
    /// stable per distinct item. The `source:date:` prefix is shared by every event of
    /// a source on a day; this tail is what disambiguates two same-titled documents.
    pub fn content_hash(&self) -> &str {
        self.0
            .rsplit(':')
            .next()
            .expect("EventId is always source:date:hash")
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Work a source declares UNFINISHED, from its own structured fields.
///
/// The same discipline `is_self` follows, for the same reason: the adapter answers from the
/// field its provider defines — a Jira issue's status category and assignee — and never from
/// free-form text. An adapter that would have to READ prose to answer leaves this `None`, and
/// the judgment it would have taken reaches the board through the LLM queue instead, where it
/// is a judgment declared as one rather than a rule quietly guessing.
///
/// This is the "an observation may PROPOSE a task" half of the intent plane's first joint. It
/// proposes and nothing here creates: `lore task propose` writes a line into the board's
/// proposed section, and only a person moves it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWork {
    /// What the proposed line reads as.
    pub summary: String,
    /// The absolute URL addressing it — both the visible link and, through
    /// [`crate::origin::identity`], what stops it being proposed twice.
    pub url: String,
}

/// What a source's own fields say about one item AS WORK.
///
/// The outer `Option` on [`RawItem::work`] is whether the source can answer at all; this is
/// the answer. Both halves are needed because a proposal is retired by OBSERVING that the work
/// is no longer open, never by its absence from a fetch: a query bounded by time returns
/// nothing on a quiet day, and reading that as "everything is finished" retires every proposal
/// the source still holds open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkState {
    /// The user's, and unfinished.
    Open(OpenWork),
    /// Seen, and not the user's unfinished work — finished, or no longer theirs.
    Settled,
}

/// What one fetch saw of the user's unfinished work, in both directions.
///
/// A proposal is retired by OBSERVING that its work is no longer open, never by its absence
/// here: a query bounded by time returns nothing on a quiet day, and reading that as "all
/// finished" retires every proposal the source still holds open. So the settled half travels
/// beside the open half, and what neither mentions is left standing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkObserved {
    pub open: Vec<OpenWork>,
    /// Addresses seen to be no longer the user's unfinished work.
    pub settled: Vec<String>,
}

impl WorkObserved {
    pub fn from_items(items: &[RawItem]) -> Self {
        let mut observed = Self::default();
        for item in items {
            match &item.work {
                Some(WorkState::Open(work)) => observed.open.push(work.clone()),
                Some(WorkState::Settled) => observed.settled.extend(item.url.clone()),
                None => {}
            }
        }
        observed
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty() && self.settled.is_empty()
    }
}

/// Intermediate representation produced by source adapters before normalization.
/// Each adapter maps its API response into one or more `RawItem`s, which
/// `lk-pipeline::normalize` then converts into `Event`s (assigning date and id).
#[derive(Debug, Clone)]
pub struct RawItem {
    pub external_id: Option<String>,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    pub author: Option<String>,
    pub timestamp: jiff::Timestamp,
    /// Authored by the configured identity. The adapter sets this from its
    /// structured authorship fields; the pipeline never re-derives ownership
    /// from free-form text.
    pub is_self: bool,
    /// What this item is as WORK, where the source's fields answer. `None` is a source that
    /// cannot say — never an answer that the work is finished. See [`WorkState`].
    pub work: Option<WorkState>,
    /// How the SOURCE itself classifies this item, where it declares one — a repository's
    /// document kind, never a reading of the text. They join the source's configured labels
    /// as the page's tags, which is what lets a reader ask for decision records without
    /// asking the source that wrote them.
    pub labels: Vec<String>,
    pub metadata: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(secs: i64, external_id: &str) -> Event {
        let date = jiff::civil::date(2026, 5, 23);
        Event {
            id: EventId::new("src", date, external_id),
            source_id: "src".into(),
            source_type: SourceType::Rss,
            timestamp: jiff::Timestamp::from_second(secs).unwrap(),
            date,
            title: external_id.into(),
            body: String::new(),
            url: None,
            author: None,
            labels: vec![],
            category: None,
            performance_category: None,
            is_self: false,
            is_personal: false,
            metadata: serde_json::Value::Null,
        }
    }

    fn order(mut events: Vec<Event>) -> Vec<String> {
        events.sort_by(Event::canonical_cmp);
        events.into_iter().map(|e| e.title).collect()
    }

    #[test]
    fn canonical_order_is_newest_first() {
        let got = order(vec![ev(100, "old"), ev(300, "new"), ev(200, "mid")]);
        assert_eq!(got, ["new", "mid", "old"]);
    }

    #[test]
    fn canonical_order_breaks_timestamp_ties_by_id_total_order() {
        // Two events at the same instant must sort into one deterministic order by id —
        // the property that makes a page's bytes independent of fetch/merge sequence.
        let a = ev(100, "alpha");
        let b = ev(100, "beta");
        assert_eq!(a.timestamp, b.timestamp);
        let mut by_id: Vec<&str> = vec![a.id.as_str(), b.id.as_str()];
        by_id.sort();
        let expected_first = if by_id[0] == a.id.as_str() {
            "alpha"
        } else {
            "beta"
        };
        // Both input permutations yield the identical sorted sequence.
        let one = order(vec![a.clone(), b.clone()]);
        let two = order(vec![b, a]);
        assert_eq!(one, two, "order must not depend on input sequence");
        assert_eq!(one[0], expected_first);
    }
}
