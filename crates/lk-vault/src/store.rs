use std::path::{Path, PathBuf};

use async_trait::async_trait;
use lk_core::frontmatter::{self, VaultPage};

use crate::VaultError;

pub(crate) fn read_page(root: &Path, path: &Path) -> Result<VaultPage, VaultError> {
    let relative = path.strip_prefix(root).map_err(std::io::Error::other)?;
    let checked = crate::page_path(root, relative)?;
    let content = std::fs::read_to_string(checked).map_err(|error| {
        std::io::Error::new(error.kind(), format!("read {}: {error}", path.display()))
    })?;
    frontmatter::parse_page(&content)
        .map_err(|error| VaultError::Frontmatter(format!("{}: {error}", path.display())))
}

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

    /// All Markdown descendants, retaining their complete vault-relative addresses.
    async fn list_markdown_recursive(&self, rel_dir: &Path) -> Result<Vec<PathBuf>, VaultError>;
}

/// Load one complete name registry for all lookups in an operation.
pub async fn load_concept_registry(
    reader: &dyn VaultStore,
    dirs: &lk_core::config::VaultDirs,
) -> Result<lk_core::concept::ConceptRegistry, VaultError> {
    use lk_core::concept::{ConceptIdentity, ConceptRegistry};
    let mut registry = ConceptRegistry::new();
    for path in reader
        .list_markdown(&lk_core::vault_path::concepts_dir(dirs))
        .await?
    {
        let slug = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| {
                std::io::Error::other(format!("concept address is not UTF-8: {}", path.display()))
            })?;
        let page = reader.read_page(&path).await?.ok_or_else(|| {
            std::io::Error::other(format!(
                "concept disappeared during registry read: {}",
                path.display()
            ))
        })?;
        let title = page
            .frontmatter
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or(slug);
        let aliases = page
            .frontmatter
            .get("aliases")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str())
            .map(String::from)
            .collect::<Vec<_>>();
        registry.register(
            ConceptIdentity {
                slug: slug.into(),
                title: title.into(),
            },
            &aliases,
        );
    }
    Ok(registry)
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
    async fn list_markdown_recursive(&self, rel_dir: &Path) -> Result<Vec<PathBuf>, VaultError> {
        let full = crate::page_path(&self.root, rel_dir)?;
        crate::search::markdown_files(&self.root, &full)?
            .into_iter()
            .map(|path| {
                let relative = path
                    .strip_prefix(&self.root)
                    .map_err(std::io::Error::other)?;
                crate::page_path(&self.root, relative)?;
                Ok(relative.to_path_buf())
            })
            .collect()
    }

    async fn read_page(&self, rel_path: &Path) -> Result<Option<VaultPage>, VaultError> {
        let full = crate::page_path(&self.root, rel_path)?;
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
        let full = crate::page_path(&self.root, rel_dir)?;
        // A missing directory is the legitimate "nothing here yet" state → empty list.
        // A permission/I/O failure on the metadata probe is a real error and must
        // propagate, not masquerade as "directory absent" — otherwise a transient
        // failure silently degrades callers (e.g. an empty concept registry).
        match tokio::fs::metadata(&full).await {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    format!("vault directory is not a directory: {}", full.display()),
                )
                .into());
            }
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
                crate::page_path(&self.root, rel)?;
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
    async fn list_markdown_recursive(&self, rel_dir: &Path) -> Result<Vec<PathBuf>, VaultError> {
        Ok(self
            .pages
            .keys()
            .filter(|path| {
                path.starts_with(rel_dir) && path.extension().is_some_and(|ext| ext == "md")
            })
            .cloned()
            .collect())
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn filesystem_readers_share_the_vault_boundary() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("page.md"), "External page.").unwrap();
        std::fs::create_dir(root.path().join("wiki")).unwrap();
        let concepts = root.path().join("wiki/concepts");
        std::os::unix::fs::symlink(outside.path(), &concepts).unwrap();
        let vault = FsVault::new(root.path());
        assert!(
            vault
                .read_page(Path::new("wiki/concepts/page.md"))
                .await
                .is_err()
        );
        assert!(
            vault
                .list_markdown(Path::new("wiki/concepts"))
                .await
                .is_err()
        );
        assert!(read_page(root.path(), &concepts.join("page.md")).is_err());
        assert!(crate::search::markdown_files(root.path(), &concepts).is_err());
        assert!(
            crate::build_index(root.path(), lk_core::i18n::Locale::En, &Default::default())
                .is_err()
        );
        std::fs::remove_file(&concepts).unwrap();
        std::fs::create_dir(root.path().join("pages")).unwrap();
        std::fs::write(root.path().join("pages/page.md"), "Internal page.").unwrap();
        std::os::unix::fs::symlink("../pages", &concepts).unwrap();
        assert!(
            vault
                .read_page(Path::new("wiki/concepts/page.md"))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            vault
                .list_markdown(Path::new("wiki/concepts"))
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(read_page(root.path(), &concepts.join("page.md")).is_ok());
        assert_eq!(
            crate::search::markdown_files(root.path(), &concepts)
                .unwrap()
                .len(),
            1
        );
        assert!(
            crate::build_index(root.path(), lk_core::i18n::Locale::En, &Default::default()).is_ok()
        );
        assert!(
            vault
                .read_page(Path::new("wiki/concepts/missing.md"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(vault.read_page(Path::new("../page.md")).await.is_err());
    }

    #[tokio::test]
    async fn filesystem_listing_distinguishes_missing_and_invalid_directories() {
        let root = tempfile::tempdir().unwrap();
        let vault = FsVault::new(root.path());
        let path = Path::new("concepts");
        assert!(vault.list_markdown(path).await.unwrap().is_empty());
        std::fs::write(root.path().join(path), "not a directory").unwrap();
        assert!(vault.list_markdown(path).await.is_err());
    }

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
