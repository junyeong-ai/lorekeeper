//! User-curated inbox source. The user drops files (`.md`, `.txt`, `.pdf`,
//! `.json`, etc.) or URL lists into `<inbox_dir>` and `lore ingest` picks them
//! up through the same pipeline as automated sources — dedup, classify,
//! concept extraction, work-log routing.
//!
//! After successful extraction the source archives consumed files to
//! `<inbox_dir>/archived/{YYYY-MM-DD}/`. Re-pushing the same content is a
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
    /// Archive consumed files under `<inbox_dir>/archived/{date}/`.
    ///
    /// **Default: false** — archival runs during the extract phase, before vault
    /// writes and dedup commit. A later pipeline failure would leave the inbox
    /// empty without the corresponding events recorded, making re-ingest
    /// impossible. Enable only when this trade-off is understood; the safer
    /// pattern is to leave files in place and let `content-hash` dedup absorb
    /// the re-runs.
    #[serde(default)]
    archive_after_ingest: bool,
}

fn default_extensions() -> Vec<String> {
    vec!["md".into(), "txt".into(), "markdown".into(), "json".into()]
}

/// Validate this source's params at config-load time, before any I/O.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    let p: ManualParams = serde_json::from_value(params.clone())
        .map_err(|e| SourceError::InvalidParams(e.to_string()))?;
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
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: ManualParams = serde_json::from_value(params.clone())
            .map_err(|e| SourceError::InvalidParams(e.to_string()))?;

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

        if p.archive_after_ingest && !items.is_empty() {
            archive_consumed(inbox, ctx.target_date, &items)?;
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
    let body = match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => crate::markdown::html_to_markdown(&raw),
        _ => raw,
    };

    let mtime = path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| jiff::Timestamp::from_second(d.as_secs() as i64).ok())
        .unwrap_or_else(jiff::Timestamp::now);

    let (title, content) = split_title(&body, path);
    // Full filename (with extension) so `note.md` and `note.txt` don't collide
    // on the external_id and dedup as the same item.
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("manual")
        .to_string();

    Ok(RawItem {
        external_id: Some(format!("manual:{file_name}")),
        title,
        body: content,
        url: None,
        author: None,
        timestamp: mtime,
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

/// Move consumed files into `<inbox>/archived/{date}/` so the inbox stays clean.
fn archive_consumed(
    inbox: &Path,
    date: jiff::civil::Date,
    items: &[RawItem],
) -> Result<(), SourceError> {
    let archive_dir = inbox.join("archived").join(date.to_string());
    std::fs::create_dir_all(&archive_dir)
        .map_err(|e| SourceError::Parse(format!("create archive dir: {e}")))?;
    for item in items {
        let Some(src) = item
            .metadata
            .get("source_file")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
        else {
            continue;
        };
        let Some(name) = src.file_name() else {
            continue;
        };
        let dest = archive_dir.join(name);
        if let Err(e) = std::fs::rename(&src, &dest) {
            tracing::warn!(
                file = %src.display(),
                error = %e,
                "manual: archive failed (file left in inbox)"
            );
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
        };
        let items = src.extract(&params, &ctx).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "my topic");
    }

    #[tokio::test]
    async fn archives_consumed_files() {
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
        };
        src.extract(&params, &ctx).await.unwrap();
        assert!(!tmp.path().join("a.md").exists());
        assert!(tmp.path().join("archived/2026-05-24/a.md").exists());
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
