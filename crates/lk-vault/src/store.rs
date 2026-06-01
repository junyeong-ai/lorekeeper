use std::path::{Path, PathBuf};

use async_trait::async_trait;
use lk_core::frontmatter::{self, VaultPage};

use crate::VaultError;

/// Read access to a knowledge vault, abstracted from its storage backend.
///
/// The pipeline's materialized-view logic (read the previous render, decide whether the
/// LLM cache is still valid) depends on this trait rather than the filesystem directly,
/// so it is backend-agnostic and unit-testable against an in-memory store. The production
/// backend is [`FsVault`]; tests use [`InMemoryVault`]. A new backend (a remote vault, a
/// different knowledge store) is a new implementor — no pipeline change.
#[async_trait]
pub trait VaultStore: Send + Sync {
    /// Read and parse a vault page by its vault-relative path. `Ok(None)` means the page
    /// doesn't exist (a legitimate "never written" state, not an error).
    async fn read_page(&self, rel_path: &Path) -> Result<Option<VaultPage>, VaultError>;

    /// Vault-relative paths of every `.md` file directly under `rel_dir`, sorted. A
    /// missing directory is the legitimate empty state and yields an empty list.
    async fn list_markdown(&self, rel_dir: &Path) -> Result<Vec<PathBuf>, VaultError>;
}

/// Filesystem-backed [`VaultStore`] rooted at a vault directory.
pub struct FsVault {
    root: PathBuf,
}

impl FsVault {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait]
impl VaultStore for FsVault {
    async fn read_page(&self, rel_path: &Path) -> Result<Option<VaultPage>, VaultError> {
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

    async fn list_markdown(&self, rel_dir: &Path) -> Result<Vec<PathBuf>, VaultError> {
        let full = self.root.join(rel_dir);
        // A missing directory is the legitimate "nothing here yet" state → empty list.
        // A permission/I/O failure on the metadata probe is a real error and must
        // propagate, not masquerade as "directory absent" — otherwise a transient
        // failure silently degrades callers (e.g. an empty concept registry).
        match tokio::fs::metadata(&full).await {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Ok(vec![]), // path exists but isn't a directory
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
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

/// In-memory [`VaultStore`] for tests: pages keyed by vault-relative path, no disk I/O.
/// Mirrors [`FsVault`] semantics — `read_page` returns `None` for an absent page,
/// `list_markdown` returns the sorted `.md` paths directly under a directory. Keys and the
/// `rel_dir` argument are matched exactly, so callers pass normalized vault-relative paths
/// (no trailing slash, no `.` segment) — the same form the `vault_path` builders produce.
#[derive(Default)]
pub struct InMemoryVault {
    pages: std::collections::BTreeMap<PathBuf, String>,
}

impl InMemoryVault {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a page at `rel_path` with raw markdown content. Chainable.
    pub fn with_page(mut self, rel_path: impl Into<PathBuf>, content: impl Into<String>) -> Self {
        self.pages.insert(rel_path.into(), content.into());
        self
    }
}

#[async_trait]
impl VaultStore for InMemoryVault {
    async fn read_page(&self, rel_path: &Path) -> Result<Option<VaultPage>, VaultError> {
        match self.pages.get(rel_path) {
            Some(content) => {
                let page = frontmatter::parse_page(content).map_err(VaultError::Frontmatter)?;
                Ok(Some(page))
            }
            None => Ok(None),
        }
    }

    async fn list_markdown(&self, rel_dir: &Path) -> Result<Vec<PathBuf>, VaultError> {
        let mut files: Vec<PathBuf> = self
            .pages
            .keys()
            .filter(|p| p.parent() == Some(rel_dir) && p.extension().is_some_and(|ext| ext == "md"))
            .cloned()
            .collect();
        files.sort();
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_vault_reads_seeded_page_and_misses_absent() {
        let vault =
            InMemoryVault::new().with_page("wiki/concepts/rag.md", "---\nid: rag\n---\n\n# RAG\n");
        let page = vault
            .read_page(Path::new("wiki/concepts/rag.md"))
            .await
            .unwrap();
        assert_eq!(
            page.unwrap().frontmatter.get("id").and_then(|v| v.as_str()),
            Some("rag")
        );
        assert!(
            vault
                .read_page(Path::new("wiki/concepts/absent.md"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn in_memory_vault_lists_only_direct_children() {
        let vault = InMemoryVault::new()
            .with_page("wiki/concepts/b.md", "b")
            .with_page("wiki/concepts/a.md", "a")
            .with_page("wiki/concepts/sub/deep.md", "deep")
            .with_page("wiki/concepts/note.txt", "txt");
        let files = vault
            .list_markdown(Path::new("wiki/concepts"))
            .await
            .unwrap();
        assert_eq!(
            files,
            vec![
                PathBuf::from("wiki/concepts/a.md"),
                PathBuf::from("wiki/concepts/b.md"),
            ],
            "only direct .md children, sorted"
        );
        // A directory with nothing under it is the empty state, not an error.
        assert!(
            vault
                .list_markdown(Path::new("wiki/empty"))
                .await
                .unwrap()
                .is_empty()
        );
    }
}
