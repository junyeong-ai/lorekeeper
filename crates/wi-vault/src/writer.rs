use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;

use crate::VaultError;
use crate::frontmatter::{self, FrontmatterPatch};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct VaultWriter {
    root: PathBuf,
}

impl VaultWriter {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
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

    pub async fn append(&self, rel_path: &Path, content: &str) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&full)
            .await?;
        file.write_all(content.as_bytes()).await?;
        Ok(())
    }

    pub async fn patch_frontmatter(
        &self,
        rel_path: &Path,
        patch: &FrontmatterPatch,
    ) -> Result<(), VaultError> {
        let full = self.root.join(rel_path);
        let content = tokio::fs::read_to_string(&full).await?;
        let mut page = frontmatter::parse_page(&content).map_err(VaultError::Frontmatter)?;

        for (key, value) in &patch.set {
            page.frontmatter.set(key.clone(), value.clone());
        }
        for key in &patch.remove {
            page.frontmatter.fields.remove(key);
        }

        let updated = frontmatter::serialize_page(&page.frontmatter, &page.body);
        self.write_page(rel_path, &updated).await
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

    #[tokio::test]
    async fn append_creates_file() {
        let (dir, writer) = setup().await;
        let rel = Path::new("wiki/log.md");
        writer.append(rel, "line 1\n").await.unwrap();
        writer.append(rel, "line 2\n").await.unwrap();
        let content = tokio::fs::read_to_string(dir.path().join(rel))
            .await
            .unwrap();
        assert_eq!(content, "line 1\nline 2\n");
    }
}
