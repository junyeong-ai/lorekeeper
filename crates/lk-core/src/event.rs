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
