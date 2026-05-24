use std::path::{Path, PathBuf};

use crate::VaultError;
use crate::frontmatter::{self, Page};

pub struct VaultReader {
    root: PathBuf,
}

impl VaultReader {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub async fn read_page(&self, rel_path: &Path) -> Result<Option<Page>, VaultError> {
        let full = self.root.join(rel_path);
        match tokio::fs::read_to_string(&full).await {
            Ok(content) => {
                let page = frontmatter::parse_page(&content).map_err(VaultError::Frontmatter)?;
                Ok(Some(page))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn list_markdown(&self, rel_dir: &Path) -> Result<Vec<PathBuf>, VaultError> {
        let full = self.root.join(rel_dir);
        if !tokio::fs::metadata(&full)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return Ok(vec![]);
        }

        let mut entries = tokio::fs::read_dir(&full).await?;
        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md")
                && let Ok(rel) = path.strip_prefix(&self.root)
            {
                files.push(rel.to_path_buf());
            }
        }
        files.sort();
        Ok(files)
    }
}
