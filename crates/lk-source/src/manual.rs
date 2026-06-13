//! User-curated inbox source. The user drops files (`.md`, `.txt`, `.markdown`,
//! `.html`, `.htm` by default) into `<inbox_dir>` and `lore ingest` picks them
//! up through the same pipeline as automated sources — classify, concept
//! extraction, work-log routing.
//!
//! Archival is deferred to `archive_consumed_files()`, which the CLI calls only
//! once this source's vault writes and the queue flush have succeeded — so a
//! write/flush failure leaves files in the inbox for safe retry. Each file's
//! content-fingerprinted id makes a re-render idempotent.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use crate::{ExtractContext, Source, SourceError};

pub struct ManualSource;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManualParams {
    /// Directory the user drops files into. `~/` expands to the home directory,
    /// and a relative path resolves against the vault root — never the process
    /// CWD — so cron-scheduled and interactive runs read the same inbox
    /// (see [`resolve_inbox_dir`]).
    inbox_dir: PathBuf,
    /// File extensions the adapter consumes. Unknown extensions are left in
    /// place untouched so users can mix non-document files in the inbox without
    /// surprise deletion.
    #[serde(default = "default_extensions")]
    extensions: Vec<String>,
    /// Archive consumed files under `<inbox_dir>/archived/{date}/` after the
    /// pipeline has written its pages.
    ///
    /// **Default: true** — archival is deferred to `archive_consumed_files()`,
    /// which the CLI calls only after every vault write and the queue flush
    /// succeeded. A write/flush failure leaves files in the inbox for safe
    /// retry; the idempotent re-render absorbs any re-runs.
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
    crate::parse_validated::<ManualParams>(params).map(|_| ())
}

impl crate::ValidatedParams for ManualParams {
    fn validate(&self) -> Result<(), SourceError> {
        // Enforced at consumption (both `extract` and `archive_consumed_files` route through
        // `parse_validated`), not just at the config boundary: an `inbox_dir` of `.` would
        // otherwise resolve to the vault root and the adapter would scan — and archive — vault
        // pages themselves.
        validate_inbox_dir(&self.inbox_dir)?;
        if self.extensions.is_empty() {
            return Err(SourceError::InvalidParams(
                "manual `extensions` must list at least one file extension".into(),
            ));
        }
        for ext in &self.extensions {
            if ext.starts_with('.') {
                return Err(SourceError::InvalidParams(format!(
                    "manual `extensions` entries must not include leading dot (got '{ext}')"
                )));
            }
        }
        Ok(())
    }
}

/// Reject any `inbox_dir` that could resolve to — or above — the vault root, where
/// the adapter would scan and (on success) ARCHIVE vault pages themselves. The check
/// is on path *components*, not a literal blacklist, so the whole escape class is closed
/// at once: a `..` component (`foo/..`, `../x`, `a/../..`) can climb to or past the root,
/// and a path with no `Normal` component (`""`, `.`, `./`, bare `/`) collapses onto the
/// base it joins. A valid inbox therefore has at least one real path segment and no
/// parent traversal — whether relative (anchored at the vault root) or absolute (its own
/// directory). Validating the raw configured path is sufficient: tilde expansion only
/// ever adds `Normal` components (an absolute home path), never a `..`. Mirrors the
/// component-based discipline of `lk_core`'s vault-dir validation rather than enumerating
/// bad strings.
fn validate_inbox_dir(inbox: &Path) -> Result<(), SourceError> {
    use std::path::Component;

    let mut has_normal = false;
    for component in inbox.components() {
        match component {
            Component::Normal(_) => has_normal = true,
            Component::ParentDir => {
                return Err(SourceError::InvalidParams(format!(
                    "manual `inbox_dir` ('{}') must not contain '..' — it could resolve \
                     above the vault root and scan/archive files outside a dedicated inbox",
                    inbox.display()
                )));
            }
            // RootDir / Prefix (absolute anchor) and CurDir are fine on their own; the
            // `has_normal` requirement below is what rejects a path that is ONLY those.
            _ => {}
        }
    }
    if !has_normal {
        return Err(SourceError::InvalidParams(format!(
            "manual `inbox_dir` ('{}') must name a dedicated directory: an empty, `.`, or \
             root-only path resolves to the vault root and would archive vault pages",
            inbox.display()
        )));
    }
    Ok(())
}

/// Canonicalize the longest EXISTING ancestor of `path` (resolving symlinks there) and
/// re-attach the not-yet-existent remainder, so two paths that share a real, possibly
/// symlinked ancestor are compared on the same canonical basis even when neither fully
/// exists. `Path::canonicalize` requires the whole path to exist; this degrades to that
/// when it does, and to a coherent partial resolution when it doesn't — never mixing a
/// resolved side with a lexical one. Falls back to the lexical path only when nothing
/// in the chain exists (e.g. an empty or fully-synthetic path).
fn canonical_prefix(path: &Path) -> PathBuf {
    let mut ancestor = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canon) = ancestor.canonicalize() {
            let mut resolved = canon;
            for segment in tail.iter().rev() {
                resolved.push(segment);
            }
            return resolved;
        }
        let Some(name) = ancestor.file_name().map(|n| n.to_os_string()) else {
            return path.to_path_buf();
        };
        tail.push(name);
        if !ancestor.pop() {
            return path.to_path_buf();
        }
    }
}

/// Resolve the configured inbox path deterministically: `~`/`~/…` expands to the
/// user's home (config values are written in a shell mindset, but nothing else
/// expands them) and a relative path anchors at the vault root — never the
/// process CWD, which differs between an interactive shell and a cron line.
/// The single resolution used by both `extract` and `archive_consumed_files`,
/// so the directory scanned and the directory archived into can never diverge.
///
/// This is the second of two vault-safety layers, at the only altitude that can see
/// `vault_root`. Load-time [`validate_inbox_dir`] rejects the lexical escape class
/// (`..`, empty/root-only) without config context; here — where the path is fully
/// resolved — the inbox is rejected if it IS the vault root or an ANCESTOR of it,
/// where the adapter would scan and archive the vault's own pages. The comparison is
/// on canonicalized paths (when they exist), so an absolute inbox equal to the vault
/// root, OR a symlinked inbox whose target is the vault root, is caught alike; a
/// descendant inbox (the normal `<vault>/inbox`) or an unrelated external directory
/// is allowed. A still-absent inbox can't alias anything yet and is left to `extract`
/// to treat as empty.
fn resolve_inbox_dir(configured: &Path, vault_root: &Path) -> Result<PathBuf, SourceError> {
    let expanded = lk_core::config::expand_tilde(&configured.to_string_lossy());
    let resolved = if expanded.is_absolute() {
        expanded
    } else {
        vault_root.join(expanded)
    };

    // Compare both paths on ONE basis via `canonical_prefix`: each is canonicalized
    // through its longest EXISTING ancestor (resolving symlinks there) with the
    // not-yet-created remainder re-appended. Plain `canonicalize` requires the whole
    // path to exist, which on a first run (vault root not created until the write phase)
    // would pit a symlink-resolved inbox against a lexical vault root — e.g. `/tmp` →
    // `/private/tmp` vs lexical `/tmp/vault` — and silently miss that the inbox is the
    // vault's ancestor. Resolving the shared real prefix identically closes that.
    let resolved_real = canonical_prefix(&resolved);
    let vault_real = canonical_prefix(vault_root);
    if resolved_real == vault_real || vault_real.starts_with(&resolved_real) {
        return Err(SourceError::InvalidParams(format!(
            "manual `inbox_dir` ('{}') resolves to the vault root or an ancestor of it \
             ('{}') — it would scan and archive the vault's own pages; point it at a \
             dedicated directory",
            configured.display(),
            vault_root.display()
        )));
    }
    Ok(resolved)
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
        let p: ManualParams = crate::parse_validated(params)?;

        let inbox = resolve_inbox_dir(&p.inbox_dir, &ctx.vault_root)?;
        if !inbox.exists() {
            // warn, not info: the source is explicitly configured, so a missing inbox
            // is a setup gap (or a path typo) that would otherwise no-op silently forever.
            tracing::warn!(inbox = %inbox.display(), "manual: inbox dir absent, skipping");
            return Ok(Vec::new());
        }

        let mut items = Vec::new();
        let entries = std::fs::read_dir(&inbox)
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
    // Full filename (with extension) so `note.md` and `note.txt` get distinct ids.
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("manual")
        .to_string();
    // Fingerprint the content into the external_id so re-dropping a file with the SAME name
    // but EDITED content on the same day yields a distinct `EventId` (a distinct document
    // page) rather than re-rendering the same one; an unchanged re-drop keeps a stable id.
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

/// Move every consumed inbox file into `<inbox>/archived/{date}/` after the pipeline
/// has written the vault pages. The CLI calls this only once this source's vault writes
/// and the queue flush have succeeded, so a write/flush failure leaves the inbox intact
/// for safe retry. Each scanned file maps to one event (its content-fingerprinted id is
/// unique per file), so archiving the run's `events` clears the whole batch — a file
/// left behind would be re-scanned every run.
pub fn archive_consumed_files(
    params: &serde_json::Value,
    events: &[lk_core::event::Event],
    date: jiff::civil::Date,
    vault_root: &Path,
) -> Result<(), SourceError> {
    let p: ManualParams = crate::parse_validated(params)?;
    if !p.archive_after_ingest || events.is_empty() {
        return Ok(());
    }
    let inbox = resolve_inbox_dir(&p.inbox_dir, vault_root)?;
    let archive_dir = inbox.join("archived").join(date.to_string());
    std::fs::create_dir_all(&archive_dir)
        .map_err(|e| SourceError::Parse(format!("create archive dir: {e}")))?;
    for event in events {
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
            tracing::warn!(file = %src.display(), error = %e, "manual: archive failed");
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
            vault_root: std::path::PathBuf::new(),
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
            vault_root: std::path::PathBuf::new(),
        };
        let items = src.extract(&params, &ctx).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "my topic");
    }

    #[tokio::test]
    async fn extract_rejects_unsafe_inbox_dir_at_consumption() {
        // The `inbox_dir` invariant is enforced in `extract` itself, not only in the
        // offline `validate_params`: `.` resolves to the vault root, where the adapter
        // would scan — and archive — vault pages. `extract` routes params through
        // `parse_validated`, so it rejects the unsafe config before touching the disk.
        let src = ManualSource::new();
        let params = serde_json::json!({ "inbox_dir": ".", "extensions": ["md"] });
        let ctx = ExtractContext {
            target_date: jiff::civil::date(2026, 5, 24),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::default(),
            identity: lk_core::config::Identity::default(),
            vault_root: std::path::PathBuf::new(),
        };
        let err = src.extract(&params, &ctx).await.unwrap_err();
        assert!(
            matches!(err, SourceError::InvalidParams(_)),
            "extract must reject an unsafe inbox_dir at consumption, got {err:?}"
        );
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
            identity: lk_core::config::Identity::default(),
            vault_root: std::path::PathBuf::new(),
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
                timestamp: item.timestamp,
                date: ctx.target_date,
                title: item.title.clone(),
                body: item.body.clone(),
                url: None,
                author: None,
                labels: vec![],
                category: None,
                performance_category: None,
                is_self: false,
                is_personal: false,
                metadata: item.metadata.clone(),
            })
            .collect();

        archive_consumed_files(&params, &events, ctx.target_date, std::path::Path::new(""))
            .unwrap();
        assert!(!tmp.path().join("a.md").exists());
        assert!(tmp.path().join("archived/2026-05-24/a.md").exists());
    }

    #[tokio::test]
    async fn archives_every_scanned_file() {
        // Every file the run consumed is archived so nothing lingers in the inbox to be
        // re-scanned on the next run.
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("one.md"), "# One\n\nx");
        write(&tmp.path().join("two.md"), "# Two\n\ny");
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
            vault_root: std::path::PathBuf::new(),
        };
        let items = src.extract(&params, &ctx).await.unwrap();
        let events: Vec<lk_core::event::Event> = items
            .iter()
            .map(|item| lk_core::event::Event {
                id: lk_core::event::EventId::new("manual", ctx.target_date, &item.title),
                source_id: "manual".into(),
                source_type: lk_core::config::SourceType::Manual,
                timestamp: item.timestamp,
                date: ctx.target_date,
                title: item.title.clone(),
                body: item.body.clone(),
                url: None,
                author: None,
                labels: vec![],
                category: None,
                performance_category: None,
                is_self: false,
                is_personal: false,
                metadata: item.metadata.clone(),
            })
            .collect();
        archive_consumed_files(&params, &events, ctx.target_date, std::path::Path::new(""))
            .unwrap();

        // Every scanned file is gone from the inbox top level and present under
        // archived/ — nothing lingers to be re-scanned.
        assert!(!tmp.path().join("one.md").exists());
        assert!(!tmp.path().join("two.md").exists());
        assert!(tmp.path().join("archived/2026-05-24/one.md").exists());
        assert!(tmp.path().join("archived/2026-05-24/two.md").exists());
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

    #[test]
    fn validate_rejects_inbox_that_resolves_to_or_above_vault_root() {
        // The whole escape class — not just the literal empty/dot strings — must be
        // rejected: paths that collapse onto the vault root (`""`, `.`, `./`, `/`) and
        // paths that climb out via `..` (`foo/..`, `../x`, `a/../..`). A literal
        // blacklist would miss every form but the first two; the component check
        // closes all of them.
        for bad in ["", ".", "..", "./", "/", "foo/..", "../x", "a/../.."] {
            let params = serde_json::json!({ "inbox_dir": bad });
            assert!(
                validate_params(&params).is_err(),
                "inbox_dir {bad:?} must be rejected (resolves to or above the vault root)"
            );
        }
    }

    #[test]
    fn validate_accepts_dedicated_inbox_dirs() {
        // A real segment with no parent traversal is fine, relative or absolute,
        // including the `~/…` shell form (expansion only adds Normal components).
        for ok in ["inbox", "a/b", "/abs/inbox", "~/Documents/inbox"] {
            let params = serde_json::json!({ "inbox_dir": ok });
            assert!(
                validate_params(&params).is_ok(),
                "inbox_dir {ok:?} must be accepted"
            );
        }
    }

    #[test]
    fn inbox_dir_resolution_is_cwd_independent() {
        // Non-existent paths exercise the lexical fallback (canonicalize can't run),
        // which is the pure resolution contract under test here.
        // Absolute paths pass through untouched.
        assert_eq!(
            resolve_inbox_dir(Path::new("/abs/inbox"), Path::new("/vault")).unwrap(),
            PathBuf::from("/abs/inbox")
        );
        // A relative path anchors at the vault root, never the process CWD —
        // a cron run and an interactive run must read the same inbox.
        assert_eq!(
            resolve_inbox_dir(Path::new("inbox"), Path::new("/vault")).unwrap(),
            PathBuf::from("/vault/inbox")
        );
        // `~/` expands to the home directory (the example config's shape).
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(
                resolve_inbox_dir(Path::new("~/inbox"), Path::new("/vault")).unwrap(),
                PathBuf::from(home).join("inbox")
            );
        }
    }

    #[test]
    fn resolve_rejects_inbox_aliasing_the_vault_root() {
        // The lexical `validate_inbox_dir` can't see the vault root; this resolve-time
        // guard is the layer that does. An absolute inbox equal to the vault root, an
        // ancestor of it, and a symlink whose target IS the vault root must all be
        // rejected before any scan/archive — caught by canonicalized comparison.
        let tmp = TempDir::new().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();

        // Inbox == vault root (absolute).
        assert!(resolve_inbox_dir(&vault, &vault).is_err());
        // Inbox is an ancestor of the vault root.
        assert!(resolve_inbox_dir(tmp.path(), &vault).is_err());
        // A descendant inbox is the normal, allowed shape.
        assert!(resolve_inbox_dir(Path::new("inbox"), &vault).is_ok());

        // A symlinked inbox whose target is the vault root is caught by canonicalize,
        // even though its lexical path looks like an innocent descendant.
        #[cfg(unix)]
        {
            let link = tmp.path().join("link-to-vault");
            std::os::unix::fs::symlink(&vault, &link).unwrap();
            assert!(
                resolve_inbox_dir(&link, &vault).is_err(),
                "a symlink resolving to the vault root must be rejected"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn resolve_rejects_symlinked_ancestor_of_an_absent_vault_root() {
        // The mixed-basis trap: the inbox EXISTS through a symlink while the vault root
        // does NOT yet exist (first run — the vault is created later, in the write phase).
        // Comparing a symlink-resolved inbox against a lexical vault root would miss that
        // the inbox is the vault's ancestor. `canonical_prefix` resolves the shared real
        // prefix on both sides, so the ancestor relationship survives.
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        // `link` -> `real`; the vault will live under `link/vault` but doesn't exist yet.
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let absent_vault = link.join("vault");
        // inbox = `link` (exists via symlink) is an ancestor of the absent vault root.
        assert!(
            resolve_inbox_dir(&link, &absent_vault).is_err(),
            "an existing (symlinked) ancestor of a not-yet-created vault root must be rejected"
        );
        // A genuine descendant of the same absent vault root is still allowed.
        assert!(resolve_inbox_dir(Path::new("inbox"), &absent_vault).is_ok());
    }

    #[tokio::test]
    async fn relative_inbox_dir_reads_under_vault_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("inbox")).unwrap();
        write(&tmp.path().join("inbox/note.md"), "# N\n\nx");
        let src = ManualSource::new();
        let params = serde_json::json!({
            "inbox_dir": "inbox",
            "archive_after_ingest": false,
        });
        let ctx = ExtractContext {
            target_date: jiff::civil::date(2026, 5, 24),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::default(),
            identity: lk_core::config::Identity::default(),
            vault_root: tmp.path().to_path_buf(),
        };
        let items = src.extract(&params, &ctx).await.unwrap();
        assert_eq!(
            items.len(),
            1,
            "relative inbox must resolve under the vault root"
        );
    }
}
