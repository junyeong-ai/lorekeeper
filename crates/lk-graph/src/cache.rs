//! Mtime-based scan cache for incremental graph analysis.
//!
//! Stores the modification times of every `.md` file seen during a vault scan.
//! Before re-scanning, [`is_dirty`] walks the same scope directories and compares
//! on-disk mtimes against the cached values. If nothing was added, deleted, or
//! modified, the caller can skip the full scan.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::GraphError;

/// Relative cache file path under the vault root.
const CACHE_REL_PATH: &str = ".lorekeeper/graph-cache.json";

/// Persisted mtime snapshot produced after a successful vault scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanCache {
    /// ISO-8601 timestamp of when the scan was performed.
    pub scanned_at: String,
    /// Number of `.md` pages recorded.
    pub page_count: usize,
    /// Vault-relative path to mtime (seconds since Unix epoch).
    pub mtimes: BTreeMap<PathBuf, i64>,
}

/// Return the canonical cache file path for a vault.
pub fn cache_path(vault_root: &Path) -> PathBuf {
    vault_root.join(CACHE_REL_PATH)
}

/// Load a previously saved cache. Returns `None` if the file is missing or corrupt.
pub fn load(path: &Path) -> Option<ScanCache> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Atomically write the cache to `path` (write to a sibling temp file, then rename).
pub fn save(path: &Path, cache: &ScanCache) -> Result<(), GraphError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| GraphError::Io(format!("create cache dir {}: {e}", parent.display())))?;
    }
    let json = serde_json::to_string_pretty(cache)
        .map_err(|e| GraphError::Io(format!("serialize cache: {e}")))?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes())
        .map_err(|e| GraphError::Io(format!("write cache tmp {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| GraphError::Io(format!("rename cache {}: {e}", path.display())))?;
    Ok(())
}

/// Walk `scope_dirs` under `root` and compare on-disk mtimes against `cache`.
///
/// Returns `true` if any `.md` file was added, deleted, or modified since the
/// cached snapshot — meaning the caller should perform a full rescan.
pub fn is_dirty(
    root: &Path,
    scope_dirs: &[PathBuf],
    follow_links: bool,
    cache: &ScanCache,
) -> Result<bool, GraphError> {
    let mut seen: BTreeMap<PathBuf, i64> = BTreeMap::new();

    for dir in scope_dirs {
        let scan_dir = root.join(dir);
        if !scan_dir.exists() {
            // A scope dir that no longer exists is definitely a change.
            return Ok(true);
        }

        let walker = WalkDir::new(&scan_dir).follow_links(follow_links);
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            let mtime = file_mtime_epoch(path)?;
            seen.insert(rel, mtime);
        }
    }

    // Fast path: different file count means something was added or deleted.
    if seen.len() != cache.mtimes.len() {
        return Ok(true);
    }

    // Compare every entry: same keys and same mtimes.
    for (path, mtime) in &seen {
        match cache.mtimes.get(path) {
            Some(cached) if cached == mtime => {}
            _ => return Ok(true),
        }
    }

    Ok(false)
}

/// Build a fresh [`ScanCache`] by walking `scope_dirs` under `root`.
pub fn build(
    root: &Path,
    scope_dirs: &[PathBuf],
    follow_links: bool,
) -> Result<ScanCache, GraphError> {
    let mut mtimes: BTreeMap<PathBuf, i64> = BTreeMap::new();

    for dir in scope_dirs {
        let scan_dir = root.join(dir);
        if !scan_dir.exists() {
            continue;
        }

        let walker = WalkDir::new(&scan_dir).follow_links(follow_links);
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            let mtime = file_mtime_epoch(path)?;
            mtimes.insert(rel, mtime);
        }
    }

    let now = jiff::Timestamp::now();
    Ok(ScanCache {
        scanned_at: now.to_string(),
        page_count: mtimes.len(),
        mtimes,
    })
}

/// Extract the modification time of `path` as seconds since the Unix epoch.
fn file_mtime_epoch(path: &Path) -> Result<i64, GraphError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| GraphError::Io(format!("metadata {}: {e}", path.display())))?;
    let mtime = meta
        .modified()
        .map_err(|e| GraphError::Io(format!("mtime {}: {e}", path.display())))?;
    let duration = mtime.duration_since(std::time::UNIX_EPOCH).map_err(|e| {
        GraphError::Io(format!("mtime {} predates unix epoch: {e}", path.display()))
    })?;
    Ok(duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_save_load() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_file = tmp.path().join(".lorekeeper/graph-cache.json");

        let mut mtimes = BTreeMap::new();
        mtimes.insert(PathBuf::from("wiki/a.md"), 1000);
        mtimes.insert(PathBuf::from("wiki/b.md"), 2000);

        let cache = ScanCache {
            scanned_at: "2026-01-01T00:00:00Z".to_string(),
            page_count: 2,
            mtimes,
        };

        save(&cache_file, &cache).unwrap();
        let loaded = load(&cache_file).expect("should load saved cache");
        assert_eq!(loaded.page_count, 2);
        assert_eq!(loaded.mtimes.len(), 2);
        assert_eq!(loaded.scanned_at, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn load_returns_none_for_missing() {
        assert!(load(Path::new("/nonexistent/cache.json")).is_none());
    }

    #[test]
    fn load_returns_none_for_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("bad.json");
        std::fs::write(&file, "not json{{{").unwrap();
        assert!(load(&file).is_none());
    }

    #[test]
    fn is_dirty_detects_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("a.md"), "# A\n").unwrap();

        let cache = build(tmp.path(), &[PathBuf::from("wiki")], false).unwrap();
        assert_eq!(cache.page_count, 1);

        // Not dirty yet.
        assert!(!is_dirty(tmp.path(), &[PathBuf::from("wiki")], false, &cache).unwrap());

        // Add a file.
        std::fs::write(wiki.join("b.md"), "# B\n").unwrap();
        assert!(is_dirty(tmp.path(), &[PathBuf::from("wiki")], false, &cache).unwrap());
    }

    #[test]
    fn is_dirty_detects_change_in_secondary_watched_dir() {
        // Integrity commands watch more than one dir (e.g. `wiki` ∪ `daily`). A change
        // in a NON-first watched dir must be detected — otherwise `--incremental` could
        // serve stale orphan/broken-link results after a `daily/` page changes.
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        let daily = tmp.path().join("daily");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::create_dir_all(&daily).unwrap();
        std::fs::write(wiki.join("a.md"), "# A\n").unwrap();
        std::fs::write(daily.join("d.md"), "# D\n").unwrap();

        let watched = [PathBuf::from("wiki"), PathBuf::from("daily")];
        let cache = build(tmp.path(), &watched, false).unwrap();
        assert!(!is_dirty(tmp.path(), &watched, false, &cache).unwrap());

        // A new file in the secondary dir is a change.
        std::fs::write(daily.join("e.md"), "# E\n").unwrap();
        assert!(is_dirty(tmp.path(), &watched, false, &cache).unwrap());
    }

    #[test]
    fn is_dirty_detects_deleted_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("a.md"), "# A\n").unwrap();
        std::fs::write(wiki.join("b.md"), "# B\n").unwrap();

        let cache = build(tmp.path(), &[PathBuf::from("wiki")], false).unwrap();
        assert_eq!(cache.page_count, 2);

        std::fs::remove_file(wiki.join("b.md")).unwrap();
        assert!(is_dirty(tmp.path(), &[PathBuf::from("wiki")], false, &cache).unwrap());
    }

    #[test]
    fn is_dirty_detects_missing_scope_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ScanCache {
            scanned_at: "t".to_string(),
            page_count: 0,
            mtimes: BTreeMap::new(),
        };
        // A scope dir that doesn't exist = dirty.
        assert!(is_dirty(tmp.path(), &[PathBuf::from("gone")], false, &cache).unwrap());
    }

    #[test]
    fn build_ignores_non_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("a.md"), "# A\n").unwrap();
        std::fs::write(wiki.join("b.txt"), "not md\n").unwrap();

        let cache = build(tmp.path(), &[PathBuf::from("wiki")], false).unwrap();
        assert_eq!(cache.page_count, 1);
        assert!(cache.mtimes.contains_key(&PathBuf::from("wiki/a.md")));
    }
}
