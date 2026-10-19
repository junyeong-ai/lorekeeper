//! Explicit semantic attribution to immutable RSS observations. No title/position inference.
use serde::{Deserialize, Serialize};

use crate::rss_original::{RssOriginalReference, is_valid_digest};

pub const CONCEPT_EVIDENCE_FIELD: &str = "concept_evidence";
pub const RSS_EVIDENCE_SCHEMA: &str = "rss-event-evidence-v1";
pub const EXTRACTION_POLICY: &str = "concept-extraction-rss-v3";
pub const SYNTHESIS_POLICY: &str = "concept-synthesis-rss-v2";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RssEventSelector {
    pub schema: String,
    pub event_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RssEventSelection {
    pub event_id: String,
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConceptEvidence {
    pub schema: String,
    pub concept: String,
    pub manifest: RssOriginalReference,
    pub events: Vec<RssEventSelection>,
}

impl RssEventSelector {
    pub fn validate(&self, revision: &str) -> Result<(), String> {
        if self.schema != RSS_EVIDENCE_SCHEMA || !is_valid_digest(revision) {
            return Err("unsupported or malformed RSS event evidence".into());
        }
        let (prefix, hash) = self
            .event_id
            .rsplit_once(':')
            .ok_or("invalid RSS event id")?;
        let (source, date) = prefix.rsplit_once(':').ok_or("invalid RSS event id")?;
        if !crate::rss_original::is_valid_source(source)
            || date.parse::<jiff::civil::Date>().is_err()
            || hash.len() != 16
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid RSS event id".into());
        }
        Ok(())
    }
}

pub fn bindings(page: &crate::frontmatter::VaultPage) -> Result<Vec<ConceptEvidence>, String> {
    let Some(value) = page.frontmatter.get(CONCEPT_EVIDENCE_FIELD) else {
        return Ok(Vec::new());
    };
    let bindings: Vec<ConceptEvidence> =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if bindings.len() > crate::rss_original::MAX_RSS_EVENTS
        || bindings.iter().map(|b| b.events.len()).sum::<usize>()
            > crate::rss_original::MAX_RSS_EVENTS
    {
        return Err("RSS concept evidence exceeds event budget".into());
    }
    for binding in &bindings {
        if binding.schema != RSS_EVIDENCE_SCHEMA
            || binding.events.is_empty()
            || !is_page_address(&binding.concept)
        {
            return Err("invalid concept evidence declaration".into());
        }
        binding.manifest.validate()?;
        for event in &binding.events {
            RssEventSelector {
                schema: RSS_EVIDENCE_SCHEMA.into(),
                event_id: event.event_id.clone(),
            }
            .validate(&event.revision)?;
        }
    }
    Ok(bindings)
}

pub fn is_page_address(path: &str) -> bool {
    !path.contains('\\')
        && std::path::Path::new(path)
            .extension()
            .is_some_and(|s| s == "md")
        && std::path::Path::new(path)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}
