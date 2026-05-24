use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::VaultError;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct VaultWriter {
    root: PathBuf,
}

impl VaultWriter {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub async fn write_page(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);

        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Unique temp name (pid + process-global sequence) so two writers targeting the
        // same page never share a temp file or rename each other's partial write. The
        // rename onto the final path is the atomic publish step.
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = full.with_extension(format!("md.{}.{seq}.tmp", std::process::id()));

        match tokio::fs::write(&tmp, content).await {
            Ok(()) => {}
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e.into());
            }
        }
        if let Err(e) = tokio::fs::rename(&tmp, &full).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        Ok(())
    }

    /// Sync sibling of [`Self::write_page`] for callers outside a tokio runtime
    /// (e.g. `lk-graph`, which is purely sync). Same atomic temp + rename semantics
    /// and the same per-process-unique temp naming, so the two can coexist without
    /// stepping on each other's temp files.
    pub fn write_page_sync(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);

        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = full.with_extension(format!("md.{}.{seq}.tmp", std::process::id()));

        if let Err(e) = std::fs::write(&tmp, content) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, &full) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
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
