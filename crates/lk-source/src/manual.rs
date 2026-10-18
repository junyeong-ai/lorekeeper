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

/// Where a `manual` source reads from, resolved the way the adapter itself resolves it.
///
/// `lore fetch` writes into the same directory this scans, and the resolution is not a path
/// join: a `~` expands, a relative path anchors at the vault root rather than the process
/// CWD, and a path that reaches the vault root is refused. A caller re-deriving it would get
/// a different directory under a scheduler than under a shell.
pub fn inbox_dir(
    params: &serde_json::Value,
    vault_root: &Path,
    dirs: &lk_core::config::VaultDirs,
) -> Result<PathBuf, SourceError> {
    let p: ManualParams = crate::parse_validated(params)?;
    resolve_inbox_dir(&p.inbox_dir, vault_root, dirs)
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

/// Resolve against the vault, keeping raw input separate from managed output and
/// private runtime state. Canonical prefixes also cover symlinks and first-run paths.
fn resolve_inbox_dir(
    configured: &Path,
    vault_root: &Path,
    dirs: &lk_core::config::VaultDirs,
) -> Result<PathBuf, SourceError> {
    let expanded = lk_core::config::expand_tilde(&configured.to_string_lossy());
    let resolved = if expanded.is_absolute() {
        expanded
    } else {
        vault_root.join(expanded)
    };

    let resolved_real = lk_core::fs::canonical_prefix(&resolved);
    let vault_real = lk_core::fs::canonical_prefix(vault_root);
    if resolved_real == vault_real || vault_real.starts_with(&resolved_real) {
        return Err(SourceError::InvalidParams(format!(
            "manual `inbox_dir` ('{}') resolves to the vault root or an ancestor of it \
             ('{}') — it would scan and archive the vault's own pages; point it at a \
             dedicated directory",
            configured.display(),
            vault_root.display()
        )));
    }
    for namespace in [
        &dirs.daily,
        &dirs.personal,
        &dirs.synthesis,
        &dirs.wiki,
        ".lorekeeper",
    ] {
        let managed = lk_core::fs::canonical_prefix(&vault_root.join(namespace));
        if resolved_real.starts_with(&managed) || managed.starts_with(&resolved_real) {
            return Err(SourceError::InvalidParams(format!(
                "manual `inbox_dir` ('{}') overlaps managed vault namespace ('{}'); \
                 use a dedicated input directory separate from managed pages and runtime state",
                configured.display(),
                vault_root.join(namespace).display()
            )));
        }
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
        Ok(capture_manual(params, ctx)?.items)
    }
}

/// Read one inbox file into a `RawItem`. The title is derived from the
/// first non-blank line if the body starts with a markdown H1, otherwise from
/// the file stem.
#[cfg(test)]
fn read_item(path: &Path) -> Result<RawItem, SourceError> {
    let root = path
        .parent()
        .ok_or_else(|| SourceError::Parse("manual entry has no parent".into()))?
        .canonicalize()
        .map_err(|e| SourceError::Parse(e.to_string()))?;
    let canonical = root.join(
        path.file_name()
            .ok_or_else(|| SourceError::Parse("manual filename".into()))?,
    );
    let capture = crate::manual_bundle::capture(&root, &canonical)?;
    read_captured_item(path, &capture)
}

fn read_captured_item(
    path: &Path,
    capture: &lk_core::manual_original::ManualBundleCapture,
) -> Result<RawItem, SourceError> {
    let at = capture
        .manifest
        .members
        .iter()
        .position(|m| m.path == capture.manifest.entry)
        .ok_or_else(|| SourceError::Parse("missing manual entry".into()))?;
    let raw = std::str::from_utf8(&capture.contents[at])
        .map_err(|e| SourceError::Parse(e.to_string()))?
        .to_owned();
    // Integrity hash of the file's raw bytes AS READ HERE. `archive_consumed_files` re-reads
    // the file and re-hashes before moving it: a mismatch means the file was swapped (same
    // name, new content) between extract and archive, so the new content has NOT been
    // materialized into the vault yet and must be left in the inbox for the next ingest —
    // never archived away on the strength of the path merely still existing.
    let source_hash = blake3::hash(raw.as_bytes()).to_hex().to_string();

    // Convert HTML to Markdown so the vault stores clean content, not raw tags.
    // The user dropped this file deliberately, so when readability can't isolate an
    // article core, convert the whole page — there is no cleaner source to keep.
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let body = match extension.as_deref() {
        Some("html" | "htm") => {
            let canonical_parent = path
                .parent()
                .ok_or_else(|| SourceError::Parse("manual entry parent".into()))?
                .canonicalize()
                .map_err(|e| SourceError::Parse(e.to_string()))?;
            let base_path = canonical_parent.join(
                path.file_name()
                    .ok_or_else(|| SourceError::Parse("manual entry filename".into()))?,
            );
            let base_url = url::Url::from_file_path(base_path).map_err(|()| {
                SourceError::Parse("manual HTML requires an absolute entry path".into())
            })?;
            crate::markdown::readable_html_to_markdown(&raw, &base_url)
                .unwrap_or_else(|| crate::markdown::html_to_markdown(&raw))
        }
        _ => raw,
    };

    if body.len() > lk_core::manual_original::MAX_ORIGINAL_ENTRY_BYTES {
        return Err(SourceError::Parse(
            "manual derived Markdown exceeds 32 MiB".into(),
        ));
    }
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

    // Fingerprint everything the file contributes to the vault — its frontmatter as well as
    // its prose — so re-dropping a file with the SAME name but EDITED content on the same day
    // yields a distinct `EventId`; the pipeline still reuses the document owned by this
    // source_file location and archives its prior evidence. An unchanged re-drop keeps a
    // stable event id. Taken before the split because the title and
    // the source address reach the page too: a re-fetch that corrected only the title would
    // otherwise dedup away and leave the page stating the old one.
    let fingerprint = blake3::hash(body.as_bytes()).to_hex()[..8].to_string();

    let (front, body) = split_frontmatter(&body, path);
    let (title, content) = match front
        .as_ref()
        .and_then(|f| f.get("title"))
        .and_then(|v| v.as_str())
    {
        Some(stated) => (stated.trim().to_string(), body.clone()),
        None => split_title(&body, path),
    };
    // Full filename (with extension) so `note.md` and `note.txt` get distinct ids.
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("manual")
        .to_string();
    let mut metadata = serde_json::json!({
        "source_file": path.display().to_string(),
        "source_hash": source_hash,
    });
    metadata["source_record"] = serde_json::json!({});
    metadata["source_record"][lk_core::manual_original::MANUAL_ORIGINAL_FIELD] =
        serde_json::to_value(capture.manifest.reference().map_err(SourceError::Parse)?)
            .map_err(|e| SourceError::Parse(e.to_string()))?;
    if let Some(front) = front.as_ref() {
        // Source provenance, not page authority: imported machine-looking keys and lifecycle
        // declarations stay inside the source namespace. The pipeline includes this record in
        // semantic input and the document page without promoting them to its own fields.
        metadata["source_record"]["manual_frontmatter"] = serde_json::json!(front.fields);
    }
    Ok(RawItem {
        labels: Vec::new(),
        external_id: Some(format!("manual:{file_name}:{fingerprint}")),
        title,
        body: content,
        // Where the document came from, where it says so. A file the user saved from the web
        // carries the page's address, and it is the citation a reader follows back — deriving
        // one from the filename instead would invent provenance.
        url: front
            .as_ref()
            .and_then(|f| f.get("source_url"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        author: None,
        timestamp: mtime,
        is_self: false,
        work: None,
        metadata,
    })
}

/// Separate a dropped file's frontmatter from its prose.
///
/// A note written in Obsidian carries frontmatter, and reading it as prose would render the
/// block into the vault page as text. A file whose leading `---` is not YAML has no
/// frontmatter — it is prose that happens to start with a rule — so it is kept whole and the
/// mismatch is reported rather than costing the user the file.
pub(super) fn split_frontmatter(
    raw: &str,
    path: &Path,
) -> (Option<lk_core::frontmatter::Frontmatter>, String) {
    let parts = lk_core::frontmatter::split_page(raw);
    if !parts.closed || parts.yaml.trim().is_empty() {
        return (None, raw.to_string());
    }
    match lk_core::frontmatter::parse_page(raw) {
        Ok(page) => (Some(page.frontmatter), page.body),
        Err(e) => {
            tracing::warn!(
                file = %path.display(),
                error = %e,
                "manual: leading `---` block is not YAML, reading the file as prose"
            );
            (None, raw.to_string())
        }
    }
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
    ctx: &ExtractContext,
) -> Result<(), SourceError> {
    let p: ManualParams = crate::parse_validated(params)?;
    if !p.archive_after_ingest || events.is_empty() {
        return Ok(());
    }
    let inbox = resolve_inbox_dir(&p.inbox_dir, &ctx.vault_root, &ctx.vault_dirs)?;
    let archive_dir = inbox.join("archived").join(ctx.target_date.to_string());
    let inbox_real = inbox
        .canonicalize()
        .map_err(|e| SourceError::Parse(format!("resolve inbox: {e}")))?;
    let archive_real = lk_core::fs::canonical_prefix(&archive_dir);
    if archive_real == inbox_real || !archive_real.starts_with(&inbox_real) {
        return Err(SourceError::InvalidParams(
            "manual archive directory must remain within its inbox".into(),
        ));
    }
    std::fs::create_dir_all(&archive_dir)
        .map_err(|e| SourceError::Parse(format!("create archive dir: {e}")))?;
    for event in events {
        let Some(src_str) = event.metadata.get("source_file").and_then(|v| v.as_str()) else {
            continue;
        };
        let src = std::path::PathBuf::from(src_str);
        if src
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .as_ref()
            != Some(&inbox_real)
        {
            continue;
        }
        if !src
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_file())
        {
            continue;
        }
        // Re-read and re-hash: only archive the EXACT bytes we extracted. If the file is gone,
        // unreadable, or its content changed since extract (a same-name mid-run swap), skip it —
        // the next ingest will pick up whatever is there now. This is what makes "archive only
        // after the vault write succeeded" honest: we never evict content we didn't materialize.
        let Ok(current) = std::fs::read_to_string(&src) else {
            continue;
        };
        let expected_hash = event.metadata.get("source_hash").and_then(|v| v.as_str());
        let current_hash = blake3::hash(current.as_bytes()).to_hex();
        if expected_hash != Some(current_hash.as_str()) {
            tracing::warn!(
                file = %src.display(),
                "manual: inbox file changed since extract; leaving it for the next ingest"
            );
            continue;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        if let Err(e) = preserve_archive(&archive_dir, name, current.as_bytes()) {
            tracing::warn!(file = %src.display(), error = %e, "manual: archive failed; retaining inbox original");
            continue;
        }
        // Publication can take time: do not clear a replacement that arrived while preserving
        // the extracted bytes. The archive is already durable, so retaining the input on an
        // uncertain read is safe and a later ingest can retry.
        let still_consumed = src
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_file())
            && std::fs::read(&src).is_ok_and(|bytes| bytes == current.as_bytes());
        if still_consumed {
            if let Err(e) = std::fs::remove_file(&src) {
                tracing::warn!(file = %src.display(), error = %e, "manual: failed to remove archived inbox file");
            }
        } else {
            tracing::warn!(file = %src.display(), "manual: inbox file changed during archival; retaining it");
        }
    }
    Ok(())
}

/// Every candidate is checked by exclusive publication, including suffix candidates. A hash
/// narrows a name; it never authorizes overwriting bytes already held at that address.
fn preserve_archive(
    archive_dir: &Path,
    name: &std::ffi::OsStr,
    contents: &[u8],
) -> std::io::Result<PathBuf> {
    let hash = blake3::hash(contents).to_hex();
    let name_path = Path::new(name);
    let stem = name_path.file_stem().unwrap_or(name).to_string_lossy();
    let extension = name_path.extension().map(|ext| ext.to_string_lossy());
    let mut suffix_len = None;
    loop {
        let dest = match suffix_len {
            None => archive_dir.join(name),
            Some(n) => {
                let suffix = if n <= hash.len() {
                    hash[..n].to_owned()
                } else {
                    format!("{hash}-{}", n - hash.len())
                };
                // The original may already consume the filesystem's 255-byte component
                // budget. Reserve the suffix/extension first and truncate only at a UTF-8
                // boundary; an unusually long configured extension uses a digest address.
                let ending = match extension.as_ref() {
                    Some(ext) => format!(".{suffix}.{ext}"),
                    None => format!(".{suffix}"),
                };
                let candidate = if ending.len() < 255 {
                    let mut keep = stem.len().min(255 - ending.len());
                    while !stem.is_char_boundary(keep) {
                        keep -= 1;
                    }
                    format!("{}{ending}", &stem[..keep])
                } else {
                    format!("archive.{suffix}.raw")
                };
                archive_dir.join(candidate)
            }
        };
        match lk_core::fs::publish_exclusive(&dest, contents, Some(0o600))? {
            lk_core::fs::PublishOutcome::Created | lk_core::fs::PublishOutcome::Reused => {
                return Ok(dest);
            }
            lk_core::fs::PublishOutcome::Occupied => {
                suffix_len = Some(suffix_len.map_or(8, |n| n + 2));
            }
        }
    }
}

/// One bounded manual source capture, retained beside the source's planned pages.
pub struct ManualExtraction {
    pub items: Vec<RawItem>,
    pub originals: Vec<lk_core::manual_original::ManualBundleCapture>,
    /// Per-entry refusal remains visible to the CLI even when other entries succeeded.
    pub failures: Vec<String>,
}
impl ManualExtraction {
    pub fn project_links(&mut self, documents: &Path) -> Result<(), SourceError> {
        for (item, capture) in self.items.iter_mut().zip(&self.originals) {
            let path = Path::new(&capture.source_file);
            let root = path
                .parent()
                .ok_or_else(|| SourceError::Parse("manual parent".into()))?
                .canonicalize()
                .map_err(|e| SourceError::Parse(e.to_string()))?;
            let entry = root.join(
                path.file_name()
                    .ok_or_else(|| SourceError::Parse("manual filename".into()))?,
            );
            item.body =
                crate::manual_bundle::project_links(&item.body, &root, &entry, capture, documents)?;
        }
        if self.items.iter().map(|item| item.body.len()).sum::<usize>()
            > lk_core::manual_original::MAX_MANUAL_BATCH_BYTES
        {
            return Err(SourceError::Parse(
                "manual projected source batch exceeds 256 MiB; use a smaller self-contained batch"
                    .into(),
            ));
        }
        Ok(())
    }
    pub fn byte_len(&self) -> usize {
        self.originals
            .iter()
            .map(lk_core::manual_original::ManualBundleCapture::byte_len)
            .sum()
    }
}

pub fn capture_manual(
    params: &serde_json::Value,
    ctx: &ExtractContext,
) -> Result<ManualExtraction, SourceError> {
    let p: ManualParams = crate::parse_validated(params)?;
    let inbox = resolve_inbox_dir(&p.inbox_dir, &ctx.vault_root, &ctx.vault_dirs)?;
    let root = inbox
        .canonicalize()
        .map_err(|e| SourceError::Parse(format!("read inbox: {e}")))?;
    let mut result = ManualExtraction {
        items: Vec::new(),
        originals: Vec::new(),
        failures: Vec::new(),
    };
    let mut total = 0usize;
    let mut total_members = 0usize;
    let mut total_derived = 0usize;
    let mut attempted = 0;
    let mut entries = std::fs::read_dir(&inbox)
        .map_err(|e| SourceError::Parse(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| SourceError::Parse(e.to_string()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let kind = path
            .symlink_metadata()
            .map_err(|e| SourceError::Parse(e.to_string()))?
            .file_type();
        if !kind.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if !p.extensions.iter().any(|e| e == &ext.to_ascii_lowercase()) {
            continue;
        }
        attempted += 1;
        if attempted > lk_core::manual_original::MAX_ORIGINAL_MEMBERS {
            return Err(SourceError::Parse(
                "manual batch exceeds 2048 entries; split the inbox batch".into(),
            ));
        }
        let canonical = root.join(entry.file_name());
        let observation = crate::manual_bundle::capture_limited(
            &root,
            &canonical,
            lk_core::manual_original::MAX_MANUAL_BATCH_BYTES.saturating_sub(total),
        )
        .and_then(|mut capture| {
            capture.source_file = path.display().to_string();
            let item = read_captured_item(&path, &capture)?;
            Ok((item, capture))
        });
        match observation {
            Ok((item, capture)) => {
                total_members += capture.manifest.members.len();
                total_derived += item.body.len();
                if total_members > 8192
                    || total_derived > lk_core::manual_original::MAX_MANUAL_BATCH_BYTES
                {
                    return Err(SourceError::Parse("manual source batch exceeds 8192 member records or 256 MiB derived text; use a smaller self-contained batch".into()));
                }
                total = total.checked_add(capture.byte_len()).ok_or_else(|| {
                    SourceError::Parse("manual source batch size overflow".into())
                })?;
                if total > lk_core::manual_original::MAX_MANUAL_BATCH_BYTES {
                    return Err(SourceError::Parse(
                        "manual source batch exceeds 256 MiB; split the inbox batch".into(),
                    ));
                }
                result.items.push(item);
                result.originals.push(capture);
            }
            Err(error) => {
                tracing::warn!(file=%path.display(),error=%error,"manual: entry refused; retaining input");
                result.failures.push(format!("{}: {error}", path.display()));
            }
        }
    }
    crate::require_any_observation("inbox file", result.items.len(), attempted)?;
    Ok(result)
}

/// Optional input retirement is never the original store. Assets are never removed.
pub fn archive_manual_originals(
    params: &serde_json::Value,
    events: &[lk_core::event::Event],
    captures: &[lk_core::manual_original::ManualBundleCapture],
    ctx: &ExtractContext,
) -> Result<(), SourceError> {
    let filtered: Vec<_> = events
        .iter()
        .filter(|event| {
            let Some(path) = event.metadata.get("source_file").and_then(|v| v.as_str()) else {
                return false;
            };
            let Some(capture) = captures.iter().find(|c| c.source_file == path) else {
                return false;
            };
            if captures.iter().any(|other| other.manifest.entry!=capture.manifest.entry && other.manifest.members.iter().any(|member|member.path==capture.manifest.entry)) {
                tracing::warn!(file=path,"manual: entry is another bundle's dependency; retaining it as a shared attachment");
                return false;
            }
            if !capture.manifest.unsupported.is_empty() {
                tracing::warn!(
                    file = path,
                    "manual: original retained; unsupported dependency constructs"
                );
                return false;
            }
            let Some(parent) = Path::new(path).parent() else {
                return false;
            };
            let Ok(root) = parent.canonicalize() else {
                return false;
            };
            if !crate::manual_bundle::unchanged(&root, capture) {
                tracing::warn!(
                    file = path,
                    "manual: original bundle changed; retaining input"
                );
                return false;
            }
            true
        })
        .cloned()
        .collect();
    archive_consumed_files(params, &filtered, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    async fn one_item(dir: &Path) -> RawItem {
        let ctx = ExtractContext {
            target_date: jiff::civil::date(2026, 5, 24),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::default(),
            identity: lk_core::config::Identity::default(),
            vault_root: std::path::PathBuf::new(),
            vault_dirs: Default::default(),
        };
        let params = serde_json::json!({ "inbox_dir": dir, "archive_after_ingest": false });
        let mut items = ManualSource::new().extract(&params, &ctx).await.unwrap();
        assert_eq!(items.len(), 1);
        items.remove(0)
    }

    #[test]
    fn archive_checks_occupied_suffixes_instead_of_overwriting_originals() {
        let tmp = TempDir::new().unwrap();
        let raw = b"newly collected original";
        let hash = blake3::hash(raw).to_hex();
        let name = std::ffi::OsStr::new("note.md");
        write(&tmp.path().join(name), "first original");
        let occupied_suffix = tmp.path().join(format!("note.{}.md", &hash[..8]));
        write(
            &occupied_suffix,
            "another original whose filename happens to match the suffix",
        );
        let destination = preserve_archive(tmp.path(), name, raw).unwrap();
        assert_ne!(destination, occupied_suffix);
        assert_eq!(std::fs::read(&destination).unwrap(), raw);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(name)).unwrap(),
            "first original"
        );
        assert_eq!(
            std::fs::read_to_string(&occupied_suffix).unwrap(),
            "another original whose filename happens to match the suffix"
        );
        assert_eq!(
            preserve_archive(tmp.path(), name, raw).unwrap(),
            destination
        );
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 3);
    }

    #[test]
    fn archive_collision_with_maximum_length_filename_keeps_original_and_retries_stably() {
        let tmp = TempDir::new().unwrap();
        for (i, filename) in [
            format!("{}.md", "a".repeat(252)),
            format!("{}.md", "가".repeat(84)),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(filename.len(), 255);
            let dir = tmp.path().join(i.to_string());
            std::fs::create_dir(&dir).unwrap();
            write(&dir.join(&filename), "previous original");
            let raw = b"new exact raw original";
            let destination = preserve_archive(&dir, std::ffi::OsStr::new(&filename), raw).unwrap();
            assert!(destination.file_name().unwrap().to_str().unwrap().len() <= 255);
            assert_eq!(std::fs::read(&destination).unwrap(), raw);
            assert_eq!(
                std::fs::read_to_string(dir.join(&filename)).unwrap(),
                "previous original"
            );
            assert_eq!(
                preserve_archive(&dir, std::ffi::OsStr::new(&filename), raw).unwrap(),
                destination
            );
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        }
    }

    #[test]
    fn concurrent_archivers_preserve_each_distinct_original() {
        let tmp = TempDir::new().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let dir = tmp.path().to_owned();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let raw = format!("distinct raw original {i}");
                    barrier.wait();
                    let destination =
                        preserve_archive(&dir, std::ffi::OsStr::new("note.md"), raw.as_bytes())
                            .unwrap();
                    (destination, raw)
                })
            })
            .collect();
        let mut destinations = std::collections::HashSet::new();
        for handle in handles {
            let (destination, raw) = handle.join().unwrap();
            assert_eq!(std::fs::read_to_string(&destination).unwrap(), raw);
            assert!(destinations.insert(destination));
        }
        assert_eq!(destinations.len(), 8);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 8);
    }

    #[tokio::test]
    async fn unreadable_eligible_files_are_not_a_quiet_inbox() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), [0xff, 0xfe]).unwrap();
        std::fs::write(tmp.path().join("b.md"), [0x80]).unwrap();
        let params = serde_json::json!({"inbox_dir": tmp.path()});
        let ctx = ExtractContext {
            target_date: jiff::civil::date(2026, 5, 24),
            timezone: jiff::tz::TimeZone::UTC,
            locale: lk_core::i18n::Locale::default(),
            identity: lk_core::config::Identity::default(),
            vault_root: PathBuf::new(),
            vault_dirs: Default::default(),
        };
        assert!(matches!(
            ManualSource::new().extract(&params, &ctx).await,
            Err(SourceError::NothingObserved { attempted: 2, .. })
        ));
        // Partial failures retain the readable observations.
        write(&tmp.path().join("good.md"), "# Good\nReadable.");
        let items = ManualSource::new().extract(&params, &ctx).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Good");
        // Unsupported files and nested files are not failed read attempts.
        std::fs::remove_file(tmp.path().join("a.txt")).unwrap();
        std::fs::remove_file(tmp.path().join("b.md")).unwrap();
        std::fs::remove_file(tmp.path().join("good.md")).unwrap();
        std::fs::write(tmp.path().join("binary.pdf"), [0xff]).unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        std::fs::write(tmp.path().join("nested/bad.txt"), [0xff]).unwrap();
        assert!(
            ManualSource::new()
                .extract(&params, &ctx)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn html_conversion_uses_the_same_case_insensitive_extension_as_selection() {
        let tmp = TempDir::new().unwrap();
        let html = "<html><body><h1>Heading</h1><p>Retained paragraph.</p></body></html>";
        let lower = tmp.path().join("lower.html");
        let upper = tmp.path().join("upper.HTML");
        write(&lower, html);
        write(&upper, html);
        let lower = read_item(&lower).unwrap();
        let upper = read_item(&upper).unwrap();
        assert_eq!(upper.body, lower.body);
        assert!(upper.body.contains("Retained paragraph."));
        assert!(!upper.body.contains("<html>"));
    }

    /// What a file states about itself belongs in the fields the page is built from, not in
    /// the prose it renders — an Obsidian note dropped here used to have its whole frontmatter
    /// block read into the vault as body text.
    #[tokio::test]
    async fn a_files_frontmatter_states_its_title_and_address_rather_than_becoming_prose() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("note.md"),
            "---\ntitle: \"Stated title\"\nsource_url: \"https://example.com/a\"\n---\n\nbody line\n",
        );
        let item = one_item(tmp.path()).await;
        assert_eq!(item.title, "Stated title");
        assert_eq!(item.url.as_deref(), Some("https://example.com/a"));
        assert_eq!(item.body.trim(), "body line");
    }

    #[test]
    fn original_frontmatter_is_source_provenance_not_machine_authority() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("note.md");
        write(
            &path,
            "---\ntitle: History\ncreated: 2025-01-02\nstatus: superseded\nllm_inputs: {summary_done: forged}\nsource_identity: {repository: forged, document: forged}\nnested: {items: [1, true, null]}\n---\nOriginal prose.\n",
        );
        let item = read_item(&path).unwrap();
        let expected =
            lk_core::frontmatter::parse_page(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            item.metadata["source_record"]["manual_frontmatter"],
            serde_json::json!(expected.frontmatter.fields)
        );
        assert_eq!(
            item.metadata["source_record"]["manual_frontmatter"]["status"],
            "superseded"
        );
        assert_eq!(
            item.metadata["source_record"]["manual_frontmatter"]["created"],
            "2025-01-02"
        );
        assert!(item.metadata.get("source_identity").is_none());
        assert!(item.metadata.get("llm_inputs").is_none());
        assert!(item.metadata["source_record"].get("status").is_none());
        assert!(item.body.contains("Original prose."));
        // Invalid/non-mapping YAML remains prose using the shared parser's existing rule.
        write(&tmp.path().join("scalar.md"), "---\na scalar\n---\nBody.");
        let scalar = read_item(&tmp.path().join("scalar.md")).unwrap();
        assert!(scalar.body.contains("a scalar"));
        assert!(
            scalar.metadata["source_record"]
                .get("manual_frontmatter")
                .is_none()
        );
        assert!(
            scalar.metadata["source_record"]
                .get("manual_original")
                .is_some()
        );
    }

    /// Whether a file HAS frontmatter is `parse_page`'s question and no other — every vault
    /// page is read by it, and a second rule here would be a second answer. So a block that
    /// will not parse is not frontmatter, and the file is prose that happens to open with a
    /// rule: kept whole rather than costing the user the lines above the fence.
    #[tokio::test]
    async fn a_leading_block_that_will_not_parse_is_not_frontmatter() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("note.md"),
            "---\ntitle: \"unterminated\n: also broken\n---\n\nbody\n",
        );
        let item = one_item(tmp.path()).await;
        assert!(
            item.body.contains(": also broken"),
            "nothing above the fence may be dropped: {}",
            item.body
        );
        assert!(item.url.is_none());
    }

    /// The id has to move when anything the page will state moves. A re-fetch that corrected
    /// only the title leaves the prose identical, and an id taken over the prose alone would
    /// dedup it away against the page still stating the old one.
    #[tokio::test]
    async fn correcting_only_the_title_is_a_distinct_observation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("note.md");
        write(&path, "---\ntitle: \"Old\"\n---\n\nsame body\n");
        let before = one_item(tmp.path()).await.external_id;
        write(&path, "---\ntitle: \"Corrected\"\n---\n\nsame body\n");
        let after = one_item(tmp.path()).await.external_id;
        assert_ne!(before, after);
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
            vault_dirs: Default::default(),
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
            vault_dirs: Default::default(),
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
            vault_dirs: Default::default(),
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
            vault_dirs: Default::default(),
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

        archive_consumed_files(&params, &events, &ctx).unwrap();
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
            vault_dirs: Default::default(),
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
        archive_consumed_files(&params, &events, &ctx).unwrap();

        // Every scanned file is gone from the inbox top level and present under
        // archived/ — nothing lingers to be re-scanned.
        assert!(!tmp.path().join("one.md").exists());
        assert!(!tmp.path().join("two.md").exists());
        assert!(tmp.path().join("archived/2026-05-24/one.md").exists());
        assert!(tmp.path().join("archived/2026-05-24/two.md").exists());
    }

    #[tokio::test]
    async fn archive_skips_file_swapped_since_extract() {
        // A same-name file replaced with NEW content between extract and archive must NOT be
        // archived — its new content was never materialized into the vault. The integrity hash
        // catches the swap and leaves the file in the inbox for the next ingest.
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("a.md"), "# A\n\noriginal");
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
            vault_dirs: Default::default(),
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

        // Swap the file's content AFTER extract but BEFORE archive.
        write(&tmp.path().join("a.md"), "# A\n\nedited after extract");

        archive_consumed_files(&params, &events, &ctx).unwrap();

        // The swapped file stays in the inbox (NOT archived); its new content is intact so the
        // next ingest can materialize it.
        assert!(
            tmp.path().join("a.md").exists(),
            "a file changed since extract must be left in the inbox"
        );
        assert!(!tmp.path().join("archived/2026-05-24/a.md").exists());
        let kept = std::fs::read_to_string(tmp.path().join("a.md")).unwrap();
        assert!(kept.contains("edited after extract"));
    }

    #[tokio::test]
    async fn archive_disambiguates_same_name_collision() {
        // A same-name file already archived under this date must NOT be clobbered when a
        // different-content file of the same name is consumed the same day — each is its own
        // vault page (content-fingerprinted id), so both originals must be preserved.
        let tmp = TempDir::new().unwrap();
        let archived = tmp.path().join("archived/2026-05-24");
        std::fs::create_dir_all(&archived).unwrap();
        std::fs::write(archived.join("note.md"), "first original").unwrap();

        write(&tmp.path().join("note.md"), "# Note\n\nsecond original");
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
            vault_dirs: Default::default(),
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
        archive_consumed_files(&params, &events, &ctx).unwrap();

        // The earlier archived original is intact; the new one is archived under a distinct
        // (fingerprint-suffixed) name — two files, nothing overwritten.
        assert_eq!(
            std::fs::read_to_string(archived.join("note.md")).unwrap(),
            "first original",
            "pre-existing archived original must not be overwritten"
        );
        let archived_count = std::fs::read_dir(&archived).unwrap().count();
        assert_eq!(archived_count, 2, "both originals preserved");
        assert!(!tmp.path().join("note.md").exists(), "inbox emptied");
    }

    #[tokio::test]
    async fn archive_same_content_redrop_is_a_noop() {
        // The same file re-dropped after it was archived must NOT create a duplicate archive
        // entry — the content is already preserved; only the inbox copy is cleared.
        let tmp = TempDir::new().unwrap();
        let archived = tmp.path().join("archived/2026-05-24");
        std::fs::create_dir_all(&archived).unwrap();
        std::fs::write(archived.join("note.md"), "# Note\n\nsame body").unwrap();

        write(&tmp.path().join("note.md"), "# Note\n\nsame body");
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
            vault_dirs: Default::default(),
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
        archive_consumed_files(&params, &events, &ctx).unwrap();

        // Exactly one archived file (no duplicate), and the inbox copy is cleared.
        assert_eq!(
            std::fs::read_dir(&archived).unwrap().count(),
            1,
            "same-content re-drop must not add a duplicate archive entry"
        );
        assert!(!tmp.path().join("note.md").exists(), "inbox copy cleared");
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
            resolve_inbox_dir(
                Path::new("/abs/inbox"),
                Path::new("/vault"),
                &Default::default()
            )
            .unwrap(),
            PathBuf::from("/abs/inbox")
        );
        // A relative path anchors at the vault root, never the process CWD —
        // a cron run and an interactive run must read the same inbox.
        assert_eq!(
            resolve_inbox_dir(Path::new("inbox"), Path::new("/vault"), &Default::default())
                .unwrap(),
            PathBuf::from("/vault/inbox")
        );
        // `~/` expands to the home directory (the example config's shape), and where there is
        // no home the tilde is left as written and anchors at the vault like any other relative
        // path. Decided by the same predicate `expand_tilde` uses — guarding on `var("HOME")`
        // admits `Ok("")`, which the expansion treats as no home, so the guard would let in
        // exactly the case it exists to exclude and then assert the answer for the other one.
        let resolved = resolve_inbox_dir(
            Path::new("~/inbox"),
            Path::new("/vault"),
            &Default::default(),
        )
        .unwrap();
        match std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|home| !home.is_empty())
        {
            Some(home) => assert_eq!(resolved, PathBuf::from(home).join("inbox")),
            None => assert_eq!(resolved, PathBuf::from("/vault/~/inbox")),
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
        assert!(resolve_inbox_dir(&vault, &vault, &Default::default()).is_err());
        // Inbox is an ancestor of the vault root.
        assert!(resolve_inbox_dir(tmp.path(), &vault, &Default::default()).is_err());
        // A descendant inbox is the normal, allowed shape.
        assert!(resolve_inbox_dir(Path::new("inbox"), &vault, &Default::default()).is_ok());

        // A symlinked inbox whose target is the vault root is caught by canonicalize,
        // even though its lexical path looks like an innocent descendant.
        #[cfg(unix)]
        {
            let link = tmp.path().join("link-to-vault");
            std::os::unix::fs::symlink(&vault, &link).unwrap();
            assert!(
                resolve_inbox_dir(&link, &vault, &Default::default()).is_err(),
                "a symlink resolving to the vault root must be rejected"
            );
        }
    }

    #[cfg(unix)]
    #[test]
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
            resolve_inbox_dir(&link, &absent_vault, &Default::default()).is_err(),
            "an existing (symlinked) ancestor of a not-yet-created vault root must be rejected"
        );
        // A genuine descendant of the same absent vault root is still allowed.
        assert!(resolve_inbox_dir(Path::new("inbox"), &absent_vault, &Default::default()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn retirement_refuses_archive_aliases_and_unowned_entries() {
        let tmp = TempDir::new().unwrap();
        let vault = tmp.path().join("vault");
        let inbox = vault.join("inbox");
        let managed = vault.join("wiki/concepts");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::create_dir_all(&managed).unwrap();
        write(&inbox.join("note.md"), "# Note\n\nEvidence");
        write(&managed.join("existing.md"), "Curated knowledge");
        let ctx = ExtractContext {
            target_date: jiff::civil::date(2026, 5, 24),
            timezone: jiff::tz::TimeZone::UTC,
            locale: Default::default(),
            identity: Default::default(),
            vault_root: vault,
            vault_dirs: Default::default(),
        };
        let params = serde_json::json!({"inbox_dir": "inbox"});
        let capture = capture_manual(&params, &ctx).unwrap();
        let item = &capture.items[0];
        let mut event = lk_core::event::Event {
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
        };
        std::fs::create_dir(inbox.join("archived")).unwrap();
        let archive_date = inbox.join("archived/2026-05-24");
        for destination in [&inbox, &managed] {
            std::os::unix::fs::symlink(destination, &archive_date).unwrap();
            assert!(archive_consumed_files(&params, &[event.clone()], &ctx).is_err());
            assert!(inbox.join("note.md").exists());
            assert!(!managed.join("note.md").exists());
            std::fs::remove_file(&archive_date).unwrap();
        }
        event.metadata["source_file"] = managed
            .join("existing.md")
            .to_string_lossy()
            .to_string()
            .into();
        event.metadata["source_hash"] = blake3::hash(b"Curated knowledge")
            .to_hex()
            .to_string()
            .into();
        archive_consumed_files(&params, &[event], &ctx).unwrap();
        assert_eq!(
            std::fs::read_to_string(managed.join("existing.md")).unwrap(),
            "Curated knowledge"
        );
        assert!(!archive_date.join("existing.md").exists());
    }

    #[test]
    fn inboxes_are_separate_from_custom_managed_roots_before_creation() {
        let tmp = TempDir::new().unwrap();
        let vault = tmp.path().join("vault");
        let dirs = lk_core::config::VaultDirs {
            wiki: "knowledge/pages".into(),
            daily: "observations".into(),
            personal: "personal".into(),
            synthesis: "reviews".into(),
            ..Default::default()
        };
        for path in [
            "knowledge",
            "knowledge/pages",
            "knowledge/pages/concepts",
            "observations/input",
            "personal/inbox",
            "reviews/input",
            ".lorekeeper/queue",
        ] {
            let params = serde_json::json!({"inbox_dir": path});
            assert!(inbox_dir(&params, &vault, &dirs).is_err(), "{path}");
        }
        for path in [
            "inbox",
            "knowledge/raw",
            "wiki",
            "weekly",
            "observations-raw",
        ] {
            let params = serde_json::json!({"inbox_dir": path});
            assert!(inbox_dir(&params, &vault, &dirs).is_ok(), "{path}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn inboxes_cannot_alias_managed_roots_in_either_direction() {
        let tmp = TempDir::new().unwrap();
        let vault = tmp.path().join("vault");
        let external = tmp.path().join("external");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::os::unix::fs::symlink(&external, vault.join("wiki")).unwrap();
        let alias = tmp.path().join("raw-alias");
        std::os::unix::fs::symlink(&external, &alias).unwrap();
        for path in [&external, &alias, &alias.join("not-yet-created")] {
            let params = serde_json::json!({"inbox_dir": path});
            assert!(inbox_dir(&params, &vault, &Default::default()).is_err());
        }
        let params = serde_json::json!({"inbox_dir": "inbox"});
        assert!(inbox_dir(&params, &vault, &Default::default()).is_ok());
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
            vault_dirs: Default::default(),
        };
        let items = src.extract(&params, &ctx).await.unwrap();
        assert_eq!(
            items.len(),
            1,
            "relative inbox must resolve under the vault root"
        );
    }
}
