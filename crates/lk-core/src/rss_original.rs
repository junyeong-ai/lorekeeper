//! Versioned evidence for normalized, pre-classification RSS observations.
//! This is the text acquired by the adapter, not the provider's HTTP/XML bytes.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{config::SourceType, event::Event};

pub const RSS_ORIGINAL_FIELD: &str = "rss_original";
pub const RSS_ORIGINAL_SCHEME: &str = "rss-events-v1";
pub const RSS_ORIGINAL_STORE: &str = ".lorekeeper/evidence/rss";
pub const MAX_RSS_EVENT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RSS_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_RSS_EVENTS: usize = 10_000;
pub const MAX_RSS_MANIFEST_BYTES: usize = 2 * 1024 * 1024;

/// v1 serialization: JSON without whitespace; object keys recursively ordered by Rust
/// String Ord, arrays in their declared order, serde_json scalar serialization. Not JCS.
pub fn canonical_bytes(value: &impl Serialize) -> Result<Vec<u8>, String> {
    fn sorted(value: Value) -> Value {
        match value {
            Value::Object(map) => {
                let ordered: BTreeMap<_, _> =
                    map.into_iter().map(|(k, v)| (k, sorted(v))).collect();
                Value::Object(ordered.into_iter().collect())
            }
            Value::Array(values) => Value::Array(values.into_iter().map(sorted).collect()),
            scalar => scalar,
        }
    }
    serde_json::to_vec(&sorted(
        serde_json::to_value(value).map_err(|e| e.to_string())?,
    ))
    .map_err(|e| e.to_string())
}
pub fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
pub fn is_valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn is_valid_source(value: &str) -> bool {
    !value.is_empty() && !matches!(value, "." | "..") && !value.contains(['/', '\\', '\0'])
}

fn is_valid_event_id(value: &str, source: &str, date: jiff::civil::Date) -> bool {
    value
        .strip_prefix(&format!("{source}:{date}:"))
        .is_some_and(|suffix| {
            suffix.len() == 16
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RssEventPayload {
    pub schema: String,
    pub source_id: String,
    pub date: jiff::civil::Date,
    pub event_id: String,
    /// RSS adapter timestamp: published, falling back to updated. Not acquisition time.
    pub event_timestamp: jiff::Timestamp,
    /// The current adapter does not record acquisition time. Never infer it from mtime.
    pub captured_at: Option<jiff::Timestamp>,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    pub author: Option<String>,
    pub metadata: Value,
}
impl RssEventPayload {
    pub fn from_event(event: &Event) -> Result<Self, String> {
        if event.source_type != SourceType::Rss {
            return Err("original event is not RSS".into());
        }
        let payload = Self {
            schema: RSS_ORIGINAL_SCHEME.into(),
            source_id: event.source_id.clone(),
            date: event.date,
            event_id: event.id.to_string(),
            event_timestamp: event.timestamp,
            captured_at: None,
            title: event.title.clone(),
            body: event.body.clone(),
            url: event.url.clone(),
            author: event.author.clone(),
            metadata: event.metadata.clone(),
        };
        payload.validate()?;
        Ok(payload)
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != RSS_ORIGINAL_SCHEME
            || !is_valid_source(&self.source_id)
            || !is_valid_event_id(&self.event_id, &self.source_id, self.date)
            || canonical_bytes(self)?.len() > MAX_RSS_EVENT_BYTES
        {
            return Err("invalid/oversize RSS original event".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RssEventEntry {
    pub event_id: String,
    pub revision: String,
    pub bytes: usize,
    pub event_timestamp: jiff::Timestamp,
    pub title: String,
    pub url: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RssOriginalManifest {
    pub schema: String,
    pub source_id: String,
    pub date: jiff::civil::Date,
    pub events: Vec<RssEventEntry>,
}
impl RssOriginalManifest {
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        let mut total = 0usize;
        if self.schema != RSS_ORIGINAL_SCHEME
            || !is_valid_source(&self.source_id)
            || self.events.is_empty()
            || self.events.len() > MAX_RSS_EVENTS
        {
            return Err("invalid RSS original manifest".into());
        }
        for e in &self.events {
            if !is_valid_digest(&e.revision)
                || e.bytes > MAX_RSS_EVENT_BYTES
                || !is_valid_event_id(&e.event_id, &self.source_id, self.date)
                || !seen.insert(&e.event_id)
            {
                return Err("invalid/ambiguous RSS event inventory".into());
            }
            total = total
                .checked_add(e.bytes)
                .ok_or("RSS original size overflow")?;
        }
        if total > MAX_RSS_CAPTURE_BYTES || canonical_bytes(self)?.len() > MAX_RSS_MANIFEST_BYTES {
            return Err("RSS original inventory exceeds byte budget".into());
        }
        if self
            .events
            .windows(2)
            .any(|w| w[0].event_id >= w[1].event_id)
        {
            return Err("RSS original inventory must be ordered by event id".into());
        }
        Ok(())
    }
    pub fn reference(&self) -> Result<RssOriginalReference, String> {
        self.validate()?;
        let revision = digest(&canonical_bytes(self)?);
        Ok(RssOriginalReference {
            scheme: RSS_ORIGINAL_SCHEME.into(),
            manifest_path: format!("{RSS_ORIGINAL_STORE}/manifests/{revision}.json"),
            revision,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RssOriginalReference {
    pub scheme: String,
    pub manifest_path: String,
    pub revision: String,
}
impl RssOriginalReference {
    pub fn validate(&self) -> Result<(), String> {
        if self.scheme != RSS_ORIGINAL_SCHEME
            || !is_valid_digest(&self.revision)
            || self.manifest_path
                != format!("{RSS_ORIGINAL_STORE}/manifests/{}.json", self.revision)
        {
            return Err("invalid RSS original address/revision".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct RssOriginalCapture {
    pub manifest: RssOriginalManifest,
    pub payloads: Vec<Vec<u8>>,
}
impl RssOriginalCapture {
    pub fn from_events(events: &[&Event]) -> Result<Self, String> {
        let first = events.first().ok_or("empty RSS capture")?;
        if events.len() > MAX_RSS_EVENTS {
            return Err("too many RSS original events".into());
        }
        let mut pairs = Vec::new();
        let mut total = 0usize;
        for event in events {
            if event.source_id != first.source_id || event.date != first.date {
                return Err("mixed RSS capture source/day".into());
            }
            let payload = RssEventPayload::from_event(event)?;
            let bytes = canonical_bytes(&payload)?;
            total += bytes.len();
            if total > MAX_RSS_CAPTURE_BYTES {
                return Err("RSS original capture exceeds byte budget".into());
            }
            let entry = RssEventEntry {
                event_id: payload.event_id,
                revision: digest(&bytes),
                bytes: bytes.len(),
                event_timestamp: payload.event_timestamp,
                title: payload.title,
                url: payload.url,
            };
            pairs.push((entry, bytes));
        }
        pairs.sort_by(|a, b| a.0.event_id.cmp(&b.0.event_id));
        let (entries, payloads) = pairs.into_iter().unzip();
        let result = Self {
            manifest: RssOriginalManifest {
                schema: RSS_ORIGINAL_SCHEME.into(),
                source_id: first.source_id.clone(),
                date: first.date,
                events: entries,
            },
            payloads,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<(), String> {
        self.manifest.validate()?;
        if self.payloads.len() != self.manifest.events.len() {
            return Err("incomplete RSS capture".into());
        }
        for (entry, bytes) in self.manifest.events.iter().zip(&self.payloads) {
            verify_payload(&self.manifest, entry, bytes)?;
        }
        Ok(())
    }
}
pub fn verify_payload(
    manifest: &RssOriginalManifest,
    entry: &RssEventEntry,
    bytes: &[u8],
) -> Result<RssEventPayload, String> {
    if bytes.len() != entry.bytes
        || bytes.len() > MAX_RSS_EVENT_BYTES
        || digest(bytes) != entry.revision
    {
        return Err("RSS event digest/length mismatch".into());
    }
    let p: RssEventPayload = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    p.validate()?;
    if canonical_bytes(&p)? != bytes
        || p.source_id != manifest.source_id
        || p.date != manifest.date
        || p.event_id != entry.event_id
        || p.event_timestamp != entry.event_timestamp
        || p.title != entry.title
        || p.url != entry.url
    {
        return Err("RSS event differs from manifest".into());
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event() -> Event {
        let date = "2026-10-18".parse().unwrap();
        Event {
            id: crate::event::EventId::new("rss", date, "item"),
            source_id: "rss".into(),
            source_type: SourceType::Rss,
            timestamp: "2026-10-18T00:00:00Z".parse().unwrap(),
            date,
            title: "Original".into(),
            body: "수집 원문".into(),
            url: Some("https://example.com/item".into()),
            author: None,
            labels: vec![],
            category: None,
            performance_category: None,
            is_self: false,
            is_personal: false,
            metadata: serde_json::json!({"feed_id":"one","nested":{"z":1,"a":2}}),
        }
    }
    #[test]
    fn capture_identity_excludes_classification_but_tracks_body_revision() {
        let mut e = event();
        let original = RssOriginalCapture::from_events(&[&e]).unwrap();
        e.labels = vec!["configured label".into()];
        e.category = Some("changed".into());
        e.performance_category = Some("changed".into());
        e.is_self = true;
        e.is_personal = true;
        assert_eq!(
            original.manifest.reference().unwrap(),
            RssOriginalCapture::from_events(&[&e])
                .unwrap()
                .manifest
                .reference()
                .unwrap()
        );
        e.body.push_str(" updated");
        let revised = RssOriginalCapture::from_events(&[&e]).unwrap();
        assert_eq!(
            original.manifest.events[0].event_id,
            revised.manifest.events[0].event_id
        );
        assert_ne!(
            original.manifest.events[0].revision,
            revised.manifest.events[0].revision
        );
        assert_eq!(
            verify_payload(
                &original.manifest,
                &original.manifest.events[0],
                &original.payloads[0]
            )
            .unwrap()
            .captured_at,
            None
        );
    }
    #[test]
    fn typed_event_id_refuses_missing_or_malformed_digest_segment() {
        let payload = RssEventPayload::from_event(&event()).unwrap();
        for suffix in [
            "",
            "abc",
            "000000000000000G",
            "000000000000000A",
            "0000000000000001:extra",
        ] {
            let mut invalid = payload.clone();
            invalid.event_id = format!("rss:2026-10-18:{suffix}");
            assert!(invalid.validate().is_err(), "{suffix}");
        }
    }
    #[test]
    fn canonical_order_and_ambiguous_inventory_are_explicit() {
        let a = serde_json::json!({"b":{"z":1,"a":2},"a":[{"z":1,"a":2}]});
        assert_eq!(
            String::from_utf8(canonical_bytes(&a).unwrap()).unwrap(),
            r#"{"a":[{"a":2,"z":1}],"b":{"a":2,"z":1}}"#
        );
        let e = event();
        assert!(
            RssOriginalCapture::from_events(&[&e, &e])
                .unwrap_err()
                .contains("ambiguous")
        );
        let mut capture = RssOriginalCapture::from_events(&[&e]).unwrap();
        capture.payloads[0].push(b' ');
        assert!(capture.validate().unwrap_err().contains("mismatch"));
        let mut too_big = e;
        too_big.body = "x".repeat(MAX_RSS_EVENT_BYTES);
        assert!(RssOriginalCapture::from_events(&[&too_big]).is_err());
    }
}
