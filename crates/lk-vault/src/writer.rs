use std::path::{Path, PathBuf};

use crate::VaultError;

/// Atomic, durable writer for vault pages. Both methods publish through the single
/// `lk_core::fs::write_atomic` (temp + fsync + rename + dir-fsync, per-writer-unique
/// temp), so the durability and unique-temp guarantees are single-sourced — there is no
/// second atomic-write implementation that could drift. `write_page` is the async ingest
/// path; `write_page_sync` is the sync path (`lk-graph`).
pub struct VaultWriter {
    root: PathBuf,
}

impl VaultWriter {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub async fn write_page(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);
        let content = content.to_owned();
        // File I/O is blocking and `write_atomic` fsyncs, so publish on the blocking pool
        // rather than reimplementing a non-fsyncing async variant (which is exactly the
        // duplicate that would let the durability guarantee drift).
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            lk_core::fs::write_atomic(&full, content.as_bytes(), None)
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok(())
    }

    /// Sync sibling of [`Self::write_page`] for callers outside a tokio runtime
    /// (e.g. `lk-graph`, which is purely sync). Routes through the same
    /// `lk_core::fs::write_atomic`, so it is not a second atomic-write implementation.
    pub fn write_page_sync(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        lk_core::fs::write_atomic(&full, content.as_bytes(), None)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, VaultWriter) {
        let dir = TempDir::new().unwrap();
        let writer = VaultWriter::new(dir.path());
        (dir, writer)
    }

    #[tokio::test]
    async fn write_and_read_back() {
        let (dir, writer) = setup().await;
        let rel = Path::new("daily/test/2026-05-23.md");
        writer.write_page(rel, "# Hello").await.unwrap();
        let content = tokio::fs::read_to_string(dir.path().join(rel))
            .await
            .unwrap();
        assert_eq!(content, "# Hello");
    }

    #[test]
    fn sync_write_creates_parents_and_writes_atomically() {
        let dir = TempDir::new().unwrap();
        let writer = VaultWriter::new(dir.path());
        let rel = Path::new("wiki/concepts/a.md");
        writer.write_page_sync(rel, "# Hello").unwrap();
        let content = std::fs::read_to_string(dir.path().join(rel)).unwrap();
        assert_eq!(content, "# Hello");

        // No `.tmp` leftover after a successful write.
        let parent = dir.path().join("wiki/concepts");
        let leftover = std::fs::read_dir(&parent)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext.to_string_lossy().ends_with(".tmp"))
            });
        assert!(!leftover, "temp file leaked into vault");
    }
}
