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
    pub classification: Option<String>,
    pub is_personal: bool,
    /// Source-agnostic hash of title+body. Used by the `content-hash` dedup
    /// strategy to catch the same content arriving via multiple sources or
    /// re-pushed manually.
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
        let short = &hex[..12];
        Self(format!("{source_id}:{date}:{short}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable hash of an event's title + body for content-equivalence dedup.
/// Unlike `EventId` which scopes by source + date + external_id, this hash
/// is source-agnostic — useful for catching the same article ingested via
/// two different sources, or the same PDF re-pushed by the user.
pub fn content_hash(title: &str, body: &str) -> String {
    // Normalize whitespace so trivial reformatting doesn't break the match.
    let normalized = format!(
        "{}\n{}",
        title.split_whitespace().collect::<Vec<_>>().join(" "),
        body.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    blake3::hash(normalized.as_bytes()).to_hex()[..16].to_string()
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct RawItem {
    pub external_id: Option<String>,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    pub author: Option<String>,
    pub timestamp: jiff::Timestamp,
    pub metadata: serde_json::Value,
}
