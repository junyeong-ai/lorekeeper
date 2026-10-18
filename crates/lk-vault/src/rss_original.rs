//! Immutable RSS collection text and explicitly unbound legacy log discovery.
use crate::{
    MutationSession, VaultError,
    original_io::{read_bytes, store_path},
};
use lk_core::rss_original::{
    MAX_RSS_CAPTURE_BYTES, MAX_RSS_EVENT_BYTES, MAX_RSS_EVENTS, MAX_RSS_MANIFEST_BYTES,
    RSS_ORIGINAL_FIELD, RSS_ORIGINAL_STORE, RssOriginalCapture, RssOriginalManifest,
    RssOriginalReference, canonical_bytes, digest, is_valid_source, verify_payload,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

fn err(e: impl std::fmt::Display) -> VaultError {
    VaultError::Original(e.to_string())
}
fn path(root: &Path, relative: &str, create: bool) -> Result<PathBuf, VaultError> {
    store_path(root, Path::new(relative), RSS_ORIGINAL_STORE, create)
}
fn publish(root: &Path, relative: &str, bytes: &[u8]) -> Result<(), VaultError> {
    let dest = path(root, relative, true)?;
    match lk_core::fs::publish_exclusive(&dest, bytes, Some(0o600))? {
        lk_core::fs::PublishOutcome::Created | lk_core::fs::PublishOutcome::Reused => Ok(()),
        lk_core::fs::PublishOutcome::Occupied => {
            Err(err(format!("RSS immutable address occupied: {relative}")))
        }
    }
}
pub fn publish_rss_original(
    session: &MutationSession,
    capture: &RssOriginalCapture,
) -> Result<RssOriginalReference, VaultError> {
    capture.validate().map_err(err)?;
    let reference = capture.manifest.reference().map_err(err)?;
    for (entry, bytes) in capture.manifest.events.iter().zip(&capture.payloads) {
        publish(
            session.root(),
            &format!("{RSS_ORIGINAL_STORE}/events/{}.json", entry.revision),
            bytes,
        )?;
    }
    publish(
        session.root(),
        &reference.manifest_path,
        &canonical_bytes(&capture.manifest).map_err(err)?,
    )?;
    Ok(reference)
}
pub struct RssOriginalOptions<'a> {
    pub event: Option<&'a str>,
    pub offset: usize,
    pub limit: usize,
    pub text_offset: usize,
    pub text_limit: usize,
    pub revision: Option<&'a str>,
}
impl Default for RssOriginalOptions<'_> {
    fn default() -> Self {
        Self {
            event: None,
            offset: 0,
            limit: 20,
            text_offset: 0,
            text_limit: 4096,
            revision: None,
        }
    }
}
#[derive(Serialize)]
pub struct RssOriginalEntry {
    pub event_id: String,
    pub revision: String,
    pub event_timestamp: jiff::Timestamp,
    pub title: String,
    pub title_truncated: bool,
    pub url: Option<String>,
    pub url_truncated: bool,
}
#[derive(Serialize)]
pub struct RssOriginalText {
    pub text_format: &'static str,
    /// Exact immutable JSON payload address (includes full author/feed metadata). Legacy is unbound.
    pub payload_path: Option<String>,
    pub event_id: String,
    pub revision: String,
    pub event_timestamp: jiff::Timestamp,
    pub captured_at: Option<jiff::Timestamp>,
    pub offset: usize,
    pub limit: usize,
    pub total_characters: usize,
    pub text: String,
    pub next_offset: Option<usize>,
}
#[derive(Serialize)]
pub struct RssOriginalView {
    pub kind: &'static str,
    pub page: String,
    pub page_revision: String,
    pub authority: &'static str,
    pub content_kind: &'static str,
    pub verification: &'static str,
    pub captured_at: Option<jiff::Timestamp>,
    pub reference: Option<RssOriginalReference>,
    pub revision: String,
    pub total_events: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
    pub events: Vec<RssOriginalEntry>,
    pub selected: Option<RssOriginalText>,
}
fn short(value: &str, max: usize) -> (String, bool) {
    (
        value.chars().take(max).collect(),
        value.chars().count() > max,
    )
}
pub(crate) fn read_manifest(
    root: &Path,
    reference: &RssOriginalReference,
) -> Result<RssOriginalManifest, VaultError> {
    reference.validate().map_err(err)?;
    let bytes = read_bytes(
        &path(root, &reference.manifest_path, false)?,
        MAX_RSS_MANIFEST_BYTES,
    )?;
    let m: RssOriginalManifest = serde_json::from_slice(&bytes).map_err(err)?;
    if m.reference().map_err(err)? != *reference || canonical_bytes(&m).map_err(err)? != bytes {
        return Err(err("RSS manifest digest/address mismatch"));
    }
    Ok(m)
}
/// Metadata-first: validates the manifest, not all unselected payloads. A selected
/// payload is length/digest/identity checked. Never fetches a URL or repairs a log.
pub fn inspect_rss_original(
    root: &Path,
    page: &Path,
    options: &RssOriginalOptions<'_>,
) -> Result<Option<RssOriginalView>, VaultError> {
    if !(1..=100).contains(&options.limit) || !(1..=65536).contains(&options.text_limit) {
        return Err(err(
            "RSS original limit must be 1..100; text-limit 1..65536",
        ));
    }
    let raw = std::fs::read_to_string(crate::page_path(root, page)?)?;
    let parsed = crate::parse_page(&raw).map_err(err)?;
    let value = parsed
        .frontmatter
        .get("source_record")
        .and_then(|r| r.get(RSS_ORIGINAL_FIELD));
    let mut legacy_guard = None;
    let (manifest, reference, legacy) = if let Some(value) = value {
        if parsed.frontmatter.get("type").and_then(|v| v.as_str()) != Some("daily") {
            return Err(err("RSS original reference requires a daily page"));
        }
        let reference: RssOriginalReference = serde_json::from_value(value.clone()).map_err(err)?;
        let manifest = read_manifest(root, &reference)?;
        if parsed.frontmatter.get("source").and_then(|v| v.as_str())
            != Some(manifest.source_id.as_str())
            || parsed.frontmatter.get("created").and_then(|v| v.as_str())
                != Some(manifest.date.to_string().as_str())
        {
            return Err(err("daily page differs from RSS original source/day"));
        }
        (manifest, Some(reference), None)
    } else {
        if parsed.frontmatter.get("type").and_then(|v| v.as_str()) != Some("daily") {
            return Ok(None);
        }
        let Some(source) = parsed
            .frontmatter
            .get("source")
            .and_then(|v| v.as_str())
            .filter(|s| is_valid_source(s))
        else {
            return Ok(None);
        };
        let Some(date) = parsed
            .frontmatter
            .get("created")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<jiff::civil::Date>().ok())
        else {
            return Ok(None);
        };
        let relative = format!(".lorekeeper/events/{source}/{date}.jsonl");
        let file = match store_path(root, Path::new(&relative), ".lorekeeper/events", false) {
            Ok(path) => path,
            Err(VaultError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let bytes = match read_bytes(&file, MAX_RSS_CAPTURE_BYTES) {
            Ok(b) => b,
            Err(VaultError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let text = std::str::from_utf8(&bytes).map_err(err)?;
        let mut events = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if events.len() == MAX_RSS_EVENTS {
                return Err(err("legacy RSS log exceeds event budget"));
            }
            let event: lk_core::event::Event = serde_json::from_str(line).map_err(err)?;
            if event.source_id != source || event.date != date {
                return Err(err("legacy RSS log has a different source/day"));
            }
            events.push(event);
        }
        if events.is_empty() {
            return Err(err("empty legacy RSS event log"));
        }
        let capture =
            RssOriginalCapture::from_events(&events.iter().collect::<Vec<_>>()).map_err(err)?;
        // Pin the entire mutable file, including ordering/metadata edits. Not proof of
        // this page's historical derivation or the present source configuration.
        let revision = format!("legacy-file:{}", digest(&bytes));
        legacy_guard = Some((file, digest(&bytes)));
        (capture.manifest, None, Some((revision, capture.payloads)))
    };
    let revision = reference
        .as_ref()
        .map(|r| r.revision.clone())
        .unwrap_or_else(|| legacy.as_ref().unwrap().0.clone());
    if options.revision.is_some_and(|r| r != revision) {
        return Err(err("RSS original revision changed"));
    }
    if options.event.is_some() && options.revision.is_none() {
        return Err(err("selected RSS event requires --revision from discovery"));
    }
    let mut view = RssOriginalView {
        kind: "rss",
        page: page.to_string_lossy().into(),
        page_revision: lk_core::concept::file_revision(&raw),
        authority: if reference.is_some() {
            "page_bound_collection"
        } else {
            "legacy_unbound"
        },
        content_kind: "normalized_pre_classification_collection_text_not_provider_wire",
        verification: if legacy.is_some() {
            "legacy_log_exact_bytes"
        } else if options.event.is_some() {
            "manifest_and_selected_payload"
        } else {
            "manifest_only"
        },
        captured_at: None,
        reference,
        revision,
        total_events: manifest.events.len(),
        offset: options.offset,
        limit: options.limit,
        next_offset: None,
        events: Vec::new(),
        selected: None,
    };
    for e in manifest
        .events
        .iter()
        .skip(options.offset)
        .take(options.limit)
    {
        let (title, title_truncated) = short(&e.title, 256);
        let (url, url_truncated) = e
            .url
            .as_deref()
            .map(|s| short(s, 2048))
            .map_or((None, false), |(s, t)| (Some(s), t));
        view.events.push(RssOriginalEntry {
            event_id: e.event_id.clone(),
            revision: e.revision.clone(),
            event_timestamp: e.event_timestamp,
            title,
            title_truncated,
            url,
            url_truncated,
        });
    }
    let end = options.offset.saturating_add(view.events.len());
    if end < view.total_events {
        view.next_offset = Some(end);
    }
    if let Some(id) = options.event {
        let (index, entry) = manifest
            .events
            .iter()
            .enumerate()
            .find(|(_, e)| e.event_id == id)
            .ok_or_else(|| err("RSS event not in this inventory"))?;
        let bytes = if let Some((_, payloads)) = &legacy {
            payloads[index].clone()
        } else {
            read_bytes(
                &path(
                    root,
                    &format!("{RSS_ORIGINAL_STORE}/events/{}.json", entry.revision),
                    false,
                )?,
                MAX_RSS_EVENT_BYTES,
            )?
        };
        let payload = verify_payload(&manifest, entry, &bytes).map_err(err)?;
        let normalized_text = format!("{}\n\n{}", payload.title, payload.body);
        let text = normalized_text
            .chars()
            .skip(options.text_offset)
            .take(options.text_limit)
            .collect::<String>();
        let total = normalized_text.chars().count();
        let end = options.text_offset.saturating_add(text.chars().count());
        view.selected = Some(RssOriginalText {
            text_format: "title_then_blank_line_then_body",
            payload_path: view
                .reference
                .as_ref()
                .map(|_| format!("{RSS_ORIGINAL_STORE}/events/{}.json", entry.revision)),
            event_id: id.into(),
            revision: entry.revision.clone(),
            event_timestamp: payload.event_timestamp,
            captured_at: payload.captured_at,
            offset: options.text_offset,
            limit: options.text_limit,
            total_characters: total,
            text,
            next_offset: (end < total).then_some(end),
        });
    }
    if let Some((file, expected)) = legacy_guard
        && digest(&read_bytes(&file, MAX_RSS_CAPTURE_BYTES)?) != expected
    {
        return Err(err("legacy RSS log changed while reading original"));
    }
    // Guard read-only discovery against a page changed while reading the original.
    if std::fs::read_to_string(crate::page_path(root, page)?)? != raw {
        return Err(err("page changed while reading RSS original"));
    }
    Ok(Some(view))
}

/// Page publication depends on the exact originals it references, including recovery
/// after a crash. Completed no-op guards are rechecked by MutationSession recovery.
pub fn guard_rss_original(
    plan: &mut crate::MutationPlan,
    root: &Path,
    capture: &RssOriginalCapture,
) -> Result<(), VaultError> {
    capture.validate().map_err(err)?;
    let reference = capture.manifest.reference().map_err(err)?;
    let manifest = canonical_bytes(&capture.manifest).map_err(err)?;
    for (entry, expected) in capture.manifest.events.iter().zip(&capture.payloads) {
        let relative = format!("{RSS_ORIGINAL_STORE}/events/{}.json", entry.revision);
        if read_bytes(&path(root, &relative, false)?, MAX_RSS_EVENT_BYTES)? != *expected {
            return Err(err("RSS original changed before page publication"));
        }
        plan.require_unchanged(
            PathBuf::from(relative),
            String::from_utf8(expected.clone()).map_err(err)?,
        );
    }
    if read_bytes(
        &path(root, &reference.manifest_path, false)?,
        MAX_RSS_MANIFEST_BYTES,
    )? != manifest
    {
        return Err(err("RSS manifest changed before page publication"));
    }
    plan.require_unchanged(
        PathBuf::from(reference.manifest_path),
        String::from_utf8(manifest).map_err(err)?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event() -> lk_core::event::Event {
        serde_json::from_value(serde_json::json!({"id":"rss:2026-10-18:0000000000000001","source_id":"rss","source_type":"rss",
            "timestamp":"2026-10-18T00:00:00Z","date":"2026-10-18","title":"Title","body":"가나다 original",
            "url":"https://example.com","author":null,"labels":[],"category":null,"performance_category":null,
            "is_self":false,"is_personal":false,"metadata":{"feed_id":"one"}})).unwrap()
    }
    fn page(root: &Path, reference: Option<&RssOriginalReference>) {
        let record = reference
            .map(|r| serde_json::json!({RSS_ORIGINAL_FIELD:r}))
            .unwrap_or(serde_json::json!({}));
        std::fs::create_dir_all(root.join("daily")).unwrap();
        std::fs::write(root.join("daily/rss.md"),format!("---\ntype: daily\nsource: rss\ncreated: 2026-10-18\nsource_record: {record}\n---\nDerived summary, not original.\n")).unwrap();
    }
    #[test]
    fn rss_discovery_pins_body_reads_and_never_overwrites_revision_objects() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        let mut event = event();
        let first = RssOriginalCapture::from_events(&[&event]).unwrap();
        let reference = publish_rss_original(&session, &first).unwrap();
        page(tmp.path(), Some(&reference));
        let p = Path::new("daily/rss.md");
        let report = inspect_rss_original(tmp.path(), p, &RssOriginalOptions::default())
            .unwrap()
            .unwrap();
        assert_eq!(report.authority, "page_bound_collection");
        assert!(report.selected.is_none());
        let id = &first.manifest.events[0].event_id;
        assert!(
            inspect_rss_original(
                tmp.path(),
                p,
                &RssOriginalOptions {
                    event: Some(id),
                    ..Default::default()
                }
            )
            .is_err()
        );
        let selected = inspect_rss_original(
            tmp.path(),
            p,
            &RssOriginalOptions {
                event: Some(id),
                revision: Some(&report.revision),
                text_offset: 8,
                text_limit: 2,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap()
        .selected
        .unwrap();
        assert_eq!(selected.text, "나다");
        assert_eq!(selected.next_offset, Some(10));
        event.body = "new revision".into();
        let second = RssOriginalCapture::from_events(&[&event]).unwrap();
        let revised = publish_rss_original(&session, &second).unwrap();
        assert_ne!(reference, revised);
        assert!(tmp.path().join(&reference.manifest_path).exists());
        let object = tmp.path().join(format!(
            "{RSS_ORIGINAL_STORE}/events/{}.json",
            first.manifest.events[0].revision
        ));
        std::fs::write(&object, b"corrupt").unwrap();
        assert!(publish_rss_original(&session, &first).is_err());
        assert_eq!(std::fs::read(&object).unwrap(), b"corrupt");
        assert!(
            inspect_rss_original(
                tmp.path(),
                p,
                &RssOriginalOptions {
                    event: Some(id),
                    revision: Some(&report.revision),
                    ..Default::default()
                }
            )
            .is_err()
        );
        let mut plan = crate::MutationPlan::default();
        assert!(guard_rss_original(&mut plan, tmp.path(), &first).is_err());
    }
    #[test]
    fn truncated_inventory_title_remains_fully_readable_in_selected_text() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        let mut e = event();
        e.title = "제".repeat(300);
        let capture = RssOriginalCapture::from_events(&[&e]).unwrap();
        let reference = publish_rss_original(&session, &capture).unwrap();
        page(tmp.path(), Some(&reference));
        let p = Path::new("daily/rss.md");
        let inventory = inspect_rss_original(tmp.path(), p, &Default::default())
            .unwrap()
            .unwrap();
        assert!(inventory.events[0].title_truncated);
        assert_eq!(inventory.verification, "manifest_only");
        let selected = inspect_rss_original(
            tmp.path(),
            p,
            &RssOriginalOptions {
                event: Some(&capture.manifest.events[0].event_id),
                revision: Some(&inventory.revision),
                text_offset: 256,
                text_limit: 46,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        let selected = selected.selected.unwrap();
        assert_eq!(selected.text, format!("{}\n\n", "제".repeat(44)));
        assert!(tmp.path().join(selected.payload_path.unwrap()).is_file());
    }
    #[test]
    fn original_dependency_change_refuses_page_publication_after_planning() {
        let tmp = tempfile::TempDir::new().unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        let e = event();
        let capture = RssOriginalCapture::from_events(&[&e]).unwrap();
        publish_rss_original(&session, &capture).unwrap();
        std::fs::write(tmp.path().join("existing.md"), "before").unwrap();
        let mut plan = crate::MutationPlan::default();
        guard_rss_original(&mut plan, tmp.path(), &capture).unwrap();
        plan.publish_expected(
            tmp.path(),
            Path::new("existing.md"),
            Some("before".into()),
            "after".into(),
        )
        .unwrap();
        let object = tmp.path().join(format!(
            "{RSS_ORIGINAL_STORE}/events/{}.json",
            capture.manifest.events[0].revision
        ));
        std::fs::write(object, "corrupted after plan").unwrap();
        assert!(session.apply(plan).is_err());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("existing.md")).unwrap(),
            "before"
        );
    }
    #[test]
    fn legacy_log_is_unbound_read_only_and_revision_guards_every_edit() {
        let tmp = tempfile::TempDir::new().unwrap();
        page(tmp.path(), None);
        let dir = tmp.path().join(".lorekeeper/events/rss");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("2026-10-18.jsonl");
        let e = event();
        std::fs::write(&log, format!("{}\n", serde_json::to_string(&e).unwrap())).unwrap();
        let p = Path::new("daily/rss.md");
        let before = std::fs::read(&log).unwrap();
        let report = inspect_rss_original(tmp.path(), p, &RssOriginalOptions::default())
            .unwrap()
            .unwrap();
        assert_eq!(report.authority, "legacy_unbound");
        assert!(report.reference.is_none());
        assert!(!tmp.path().join(RSS_ORIGINAL_STORE).exists());
        assert_eq!(std::fs::read(&log).unwrap(), before);
        std::fs::write(&log, format!("{}\n\n", serde_json::to_string(&e).unwrap())).unwrap();
        assert!(
            inspect_rss_original(
                tmp.path(),
                p,
                &RssOriginalOptions {
                    revision: Some(&report.revision),
                    ..Default::default()
                }
            )
            .is_err()
        );
        std::fs::write(
            &log,
            format!("{0}\n{0}\n", serde_json::to_string(&e).unwrap()),
        )
        .unwrap();
        assert!(inspect_rss_original(tmp.path(), p, &RssOriginalOptions::default()).is_err());
    }
}
