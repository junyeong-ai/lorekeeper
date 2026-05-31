use serde::{Deserialize, Serialize};

use crate::config::SourceType;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub source_id: String,
    pub source_type: SourceType,
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
    pub classification: Option<String>,
    /// Personal-performance contribution bucket (e.g. `project-delivery`), used only
    /// for work-log / review category distribution. Set from a `classify` rule's
    /// optional `work_category` field — the single EXPLICIT bridge from a content
    /// signal to the performance taxonomy. `None` lets `resolve_category` fall back
    /// to the coarse per-source-type map. Never inferred from free-form text.
    pub performance_category: Option<String>,
    /// Authored by the configured identity, as determined by the source adapter
    /// from its structured authorship fields (email From, message author id,
    /// issue assignee, calendar organizer/attendee). The deterministic ownership
    /// signal — never inferred downstream from free-form text.
    pub is_self: bool,
    /// `is_self` gated by the source's `track_personal`: the event counts toward
    /// the user's personal work-log and performance reviews.
    pub is_personal: bool,
    /// Hash of date+title+body. Used by the `content-hash` dedup strategy to
    /// catch the same content arriving via multiple sources on the same day.
    /// Scoped by `date` so a templated/recurring body with an identical
    /// title+body on a different day is kept as a distinct observation rather
    /// than silently merged.
    pub content_hash: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
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
}

/// Stable hash of an event's date + title + body for content-equivalence dedup.
/// Unlike `EventId` which scopes by source + date + external_id, this hash is
/// source-agnostic — it catches the same article ingested via two different
/// sources on the same day, or the same file re-pushed by the user. It is
/// scoped by `date` so a recurring/templated body (a daily digest, a newsletter
/// with a constant subject) is NOT collapsed across days: an identical
/// title+body observed on a different day is a distinct observation.
pub fn content_hash(date: jiff::civil::Date, title: &str, body: &str) -> String {
    use std::fmt::Write as _;
    // One normalized buffer: whitespace runs collapse to a single space (so trivial
    // reformatting doesn't break the match) and `date` scopes the hash to its day.
    let mut normalized = String::with_capacity(title.len() + body.len() + 16);
    let _ = writeln!(normalized, "{date}");
    push_whitespace_normalized(&mut normalized, title);
    normalized.push('\n');
    push_whitespace_normalized(&mut normalized, body);
    blake3::hash(normalized.as_bytes()).to_hex()[..16].to_string()
}

/// Append `text` with internal whitespace runs collapsed to one space and edges
/// trimmed — applied identically on `record` and `dedup` so reformatting alone
/// never changes the hash.
fn push_whitespace_normalized(out: &mut String, text: &str) {
    for (i, word) in text.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(word);
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod content_hash_tests {
    use super::content_hash;

    #[test]
    fn identical_content_same_day_collides() {
        let d = jiff::civil::date(2026, 5, 23);
        assert_eq!(
            content_hash(d, "Daily digest", "all ok"),
            content_hash(d, "Daily digest", "all ok")
        );
    }

    #[test]
    fn identical_content_different_day_is_distinct() {
        // A templated/recurring body with an identical title+body on a different day
        // is a distinct observation — it must NOT collapse to the same hash.
        let a = content_hash(jiff::civil::date(2026, 5, 23), "Daily digest", "all ok");
        let b = content_hash(jiff::civil::date(2026, 5, 24), "Daily digest", "all ok");
        assert_ne!(a, b);
    }

    #[test]
    fn whitespace_reformatting_is_ignored() {
        let d = jiff::civil::date(2026, 5, 23);
        assert_eq!(
            content_hash(d, "A  B", "x\n\ny"),
            content_hash(d, "A B", "x y")
        );
    }
}

/// Intermediate representation produced by source adapters before normalization.
/// Each adapter maps its API response into one or more `RawItem`s, which
/// `lk-pipeline::normalize` then converts into `Event`s (assigning date, id,
/// content hash, etc.).
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
    pub metadata: serde_json::Value,
}
