//! User-curated inbox source. The user drops files (`.md`, `.txt`, `.markdown`,
//! `.html`, `.htm` by default) into `<inbox_dir>` and `lore ingest` picks them
//! up through the same pipeline as automated sources — dedup, classify,
//! concept extraction, work-log routing.
//!
//! Archival is deferred to `post_commit_archive()`, called only after the
//! pipeline has committed daily pages and dedup — so a mid-pipeline crash
//! leaves files in the inbox for safe retry. Re-pushing the same content is a
//! no-op thanks to `content-hash` dedup.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use crate::{ExtractContext, Source, SourceError};

pub struct ManualSource;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManualParams {
    /// Directory the user drops files into. Resolved relative to the vault root
    /// by the caller; this adapter takes the absolute path.
    inbox_dir: PathBuf,
    /// File extensions the adapter consumes. Unknown extensions are left in
    /// place untouched so users can mix non-document files in the inbox without
    /// surprise deletion.
    #[serde(default = "default_extensions")]
    extensions: Vec<String>,
    /// Archive consumed files under `<inbox_dir>/archived/{date}/` after the
    /// pipeline has committed daily pages and dedup.
    ///
    /// **Default: true** — archival is now deferred to `post_commit_archive()`,
    /// which the pipeline calls only after successful commit. A mid-pipeline
    /// failure leaves files in the inbox for safe retry; `content-hash` dedup
    /// absorbs any re-runs.
    #[serde(default = "default_archive")]
    archive_after_ingest: bool,
}

fn default_extensions() -> Vec<String> {
    vec![
        "md".into(),
        "txt".into(),
        "markdown".into(),
        "html".into(),
        "htm".into(),
    ]
}

fn default_archive() -> bool {
    true
}

/// Validate this source's params at config-load time, before any I/O.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    let p: ManualParams = crate::parse_params(params)?;
    if p.extensions.is_empty() {
        return Err(SourceError::InvalidParams(
            "manual `extensions` must list at least one file extension".into(),
        ));
    }
    for ext in &p.extensions {
        if ext.starts_with('.') {
            return Err(SourceError::InvalidParams(format!(
                "manual `extensions` entries must not include leading dot (got '{ext}')"
            )));
        }
    }
    Ok(())
}

impl ManualSource {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ManualSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Source for ManualSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        _ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: ManualParams = crate::parse_params(params)?;

        let inbox = &p.inbox_dir;
        if !inbox.exists() {
            tracing::info!(inbox = %inbox.display(), "manual: inbox dir absent, skipping");
            return Ok(Vec::new());
        }

        let mut items = Vec::new();
        let entries = std::fs::read_dir(inbox)
            .map_err(|e| SourceError::Parse(format!("read inbox {}: {e}", inbox.display())))?;

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "manual: skipping unreadable inbox entry");
                    continue;
                }
            };
            let path = entry.path();
            // Reject symlinks — a symlink in the inbox could otherwise read
            // outside the inbox directory. Use `symlink_metadata` so the check
            // doesn't follow the link.
            let file_type = match path.symlink_metadata() {
                Ok(m) => m.file_type(),
                Err(_) => continue,
            };
            if !file_type.is_file() {
                continue;
            }
            let ext = match path.extension().and_then(|s| s.to_str()) {
                Some(e) => e.to_lowercase(),
                None => continue,
            };
            if !p.extensions.iter().any(|e| e == &ext) {
                continue;
            }

            match read_item(&path) {
                Ok(item) => items.push(item),
                Err(e) => {
                    tracing::warn!(
                        file = %path.display(),
                        error = %e,
                        "manual: skipping file (read failed)"
                    );
                }
            }
        }

        tracing::info!(
            inbox = %inbox.display(),
            count = items.len(),
            "manual: ingested"
        );
        Ok(items)
    }
}

/// Read one inbox file into a `RawItem`. The title is derived from the
/// first non-blank line if the body starts with a markdown H1, otherwise from
/// the file stem.
fn read_item(path: &Path) -> Result<RawItem, SourceError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| SourceError::Parse(format!("read {}: {e}", path.display())))?;

    // Convert HTML to Markdown so the vault stores clean content, not raw tags.
    // The user dropped this file deliberately, so when readability can't isolate an
    // article core, convert the whole page — there is no cleaner source to keep.
    let body = match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => {
            let base_url = url::Url::parse("file:///inbox").unwrap();
            crate::markdown::readable_html_to_markdown(&raw, &base_url)
                .unwrap_or_else(|| crate::markdown::html_to_markdown(&raw))
        }
        _ => raw,
    };

    let Some(mtime) = path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| jiff::Timestamp::from_second(d.as_secs() as i64).ok())
    else {
        return Err(SourceError::Parse(format!(
            "unreadable timestamp: {}",
            path.display()
        )));
    };

    let (title, content) = split_title(&body, path);
    // Full filename (with extension) so `note.md` and `note.txt` don't collide
    // on the external_id and dedup as the same item.
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("manual")
        .to_string();
    // Fingerprint the content into the external_id so re-dropping a file with the
    // SAME name but EDITED content on the same day yields a distinct event rather
    // than colliding on `EventId` (source:date:hash(external_id)) and being dropped
    // as a duplicate. Unchanged re-drops keep a stable id and still dedup.
    let fingerprint = &blake3::hash(body.as_bytes()).to_hex()[..8];

    Ok(RawItem {
        external_id: Some(format!("manual:{file_name}:{fingerprint}")),
        title,
        body: content,
        url: None,
        author: None,
        timestamp: mtime,
        is_self: false,
        metadata: serde_json::json!({
            "source_file": path.display().to_string(),
        }),
    })
}

/// Split a markdown body into (title, content) — the leading `# Title` line is
/// promoted to the title, or the file stem is used if no H1 is present.
fn split_title(body: &str, path: &Path) -> (String, String) {
    let trimmed = body.trim_start();
    if let Some(rest) = trimmed.strip_prefix("# ")
        && let Some((first, remaining)) = rest.split_once('\n')
    {
        return (first.trim().to_string(), remaining.trim_start().to_string());
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .replace(['_', '-'], " ");
    (stem, body.to_string())
}

/// Move every consumed inbox file — both the novel events and the deduplicated
/// duplicates — into `<inbox>/archived/{date}/` after the pipeline has committed
/// daily pages and dedup. Duplicates are archived too: their content is already in
/// the vault (dedup matched it), so leaving them in the inbox would re-scan and
/// re-dedup them on every run, growing the inbox without bound. Called by the
/// pipeline's post-commit hook so a mid-pipeline failure leaves the inbox intact
/// for safe retry.
pub fn post_commit_archive(
    params: &serde_json::Value,
    novel: &[lk_core::event::Event],
    duplicates: &[lk_core::event::Event],
    date: jiff::civil::Date,
) -> Result<(), SourceError> {
    let p: ManualParams = crate::parse_params(params)?;
    if !p.archive_after_ingest || (novel.is_empty() && duplicates.is_empty()) {
        return Ok(());
    }
    let archive_dir = p.inbox_dir.join("archived").join(date.to_string());
    std::fs::create_dir_all(&archive_dir)
        .map_err(|e| SourceError::Parse(format!("create archive dir: {e}")))?;
    for event in novel.iter().chain(duplicates) {
        let Some(src_str) = event.metadata.get("source_file").and_then(|v| v.as_str()) else {
            continue;
        };
        let src = std::path::PathBuf::from(src_str);
        if !src.exists() {
            continue;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        let dest = archive_dir.join(name);
        if let Err(e) = std::fs::rename(&src, &dest) {
            tracing::warn!(file = %src.display(), error = %e, "manual: post-commit archive failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    #[tokio::test]
    async fn extracts_markdown_with_h1_title() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("note.md"), "# My Note\n\nbody line\n");

        let src = ManualSource::new();
        let params = serde_json::json!({
            "inbox_dir": tmp.path(),
            "archive_after_ingest": false,
        });
        let ctx = ExtractContext {
            target_date: jiff::civil::date(2026, 5, 24),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::default(),
            identity: lk_core::config::Identity::default(),
        };
        let items = src.extract(&params, &ctx).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "My Note");
        assert!(items[0].body.contains("body line"));
    }

    #[tokio::test]
    async fn falls_back_to_file_stem_when_no_h1() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("my_topic.txt"), "raw content");
        let src = ManualSource::new();
        let params = serde_json::json!({
            "inbox_dir": tmp.path(),
            "archive_after_ingest": false,
        });
        let ctx = ExtractContext {
            target_date: jiff::civil::date(2026, 5, 24),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::default(),
            identity: lk_core::config::Identity::default(),
        };
        let items = src.extract(&params, &ctx).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "my topic");
    }

    #[tokio::test]
    async fn archives_consumed_files_post_commit() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("a.md"), "# A\n\nx");
        let src = ManualSource::new();
        let params = serde_json::json!({
            "inbox_dir": tmp.path(),
            "archive_after_ingest": true,
        });
        let ctx = ExtractContext {
            target_date: jiff::civil::date(2026, 5, 24),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::default(),
            identity: lk_core::config::Identity::default(),
        };
        let items = src.extract(&params, &ctx).await.unwrap();
        // After extract(), file is still in inbox (archival is deferred).
        assert!(tmp.path().join("a.md").exists());

        // Build minimal Event structs carrying the source_file metadata.
        let events: Vec<lk_core::event::Event> = items
            .iter()
            .map(|item| lk_core::event::Event {
                id: lk_core::event::EventId::new("manual", ctx.target_date, &item.title),
                source_id: "manual".into(),
                source_type: lk_core::config::SourceType::Manual,
                date: ctx.target_date,
                title: item.title.clone(),
                body: item.body.clone(),
                url: None,
                author: None,
                labels: vec![],
                classification: None,
                performance_category: None,
                is_self: false,
                is_personal: false,
                content_hash: None,
                metadata: item.metadata.clone(),
            })
            .collect();

        post_commit_archive(&params, &events, &[], ctx.target_date).unwrap();
        assert!(!tmp.path().join("a.md").exists());
        assert!(tmp.path().join("archived/2026-05-24/a.md").exists());
    }

    #[tokio::test]
    async fn archives_deduplicated_files_too_not_just_novel() {
        // A duplicate file's content is already in the vault, so it must be archived
        // alongside novel files — otherwise it lingers in the inbox and is re-scanned
        // and re-deduplicated on every run.
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("novel.md"), "# Novel\n\nx");
        write(&tmp.path().join("dup.md"), "# Dup\n\ny");
        let src = ManualSource::new();
        let params = serde_json::json!({
            "inbox_dir": tmp.path(),
            "archive_after_ingest": true,
        });
        let ctx = ExtractContext {
            target_date: jiff::civil::date(2026, 5, 24),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::default(),
            identity: lk_core::config::Identity::default(),
        };
        let items = src.extract(&params, &ctx).await.unwrap();
        let mut events: Vec<lk_core::event::Event> = items
            .iter()
            .map(|item| lk_core::event::Event {
                id: lk_core::event::EventId::new("manual", ctx.target_date, &item.title),
                source_id: "manual".into(),
                source_type: lk_core::config::SourceType::Manual,
                date: ctx.target_date,
                title: item.title.clone(),
                body: item.body.clone(),
                url: None,
                author: None,
                labels: vec![],
                classification: None,
                performance_category: None,
                is_self: false,
                is_personal: false,
                content_hash: None,
                metadata: item.metadata.clone(),
            })
            .collect();
        // One arbitrary item is treated as a duplicate, the rest as novel.
        let dup = vec![events.pop().unwrap()];
        post_commit_archive(&params, &events, &dup, ctx.target_date).unwrap();

        // Both the novel and the duplicate file are gone from the inbox top level
        // and present under archived/ — nothing lingers to be re-scanned.
        assert!(!tmp.path().join("novel.md").exists());
        assert!(!tmp.path().join("dup.md").exists());
        assert!(tmp.path().join("archived/2026-05-24/novel.md").exists());
        assert!(tmp.path().join("archived/2026-05-24/dup.md").exists());
    }

    #[test]
    fn archive_after_ingest_defaults_to_true() {
        let params = serde_json::json!({
            "inbox_dir": "/tmp/inbox",
        });
        let p: ManualParams = serde_json::from_value(params).unwrap();
        assert!(p.archive_after_ingest);
    }

    #[test]
    fn validate_rejects_dotted_extension() {
        let bad = serde_json::json!({
            "inbox_dir": "/tmp",
            "extensions": [".md"],
        });
        assert!(validate_params(&bad).is_err());
    }

    #[test]
    fn validate_rejects_empty_extensions() {
        let bad = serde_json::json!({
            "inbox_dir": "/tmp",
            "extensions": [],
        });
        assert!(validate_params(&bad).is_err());
    }
}
