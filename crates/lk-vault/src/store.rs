use std::path::{Path, PathBuf};

use async_trait::async_trait;
use lk_core::frontmatter::{self, Frontmatter, VaultPage};

use crate::VaultError;

/// Distinct page owners and validated scanned addresses from one collection scan.
pub struct MarkdownInventory {
    pub owners: Vec<PathBuf>,
    pub addresses: Vec<PathBuf>,
}

/// Load distinct Markdown owners and validated occupied addresses from a collection.
/// Filesystem aliases share one owner, preferring its direct address; hard links stay
/// distinct. Nested directory aliases require a target already fully scanned directly
/// in this collection. An incomplete traversal or an address outside the vault is an error.
pub fn load_markdown_inventory(
    root: &Path,
    rel_dir: &Path,
) -> Result<MarkdownInventory, VaultError> {
    let full = crate::page_path(root, rel_dir)?;
    match std::fs::metadata(&full) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(std::io::Error::other("vault collection is not a directory").into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MarkdownInventory {
                owners: Vec::new(),
                addresses: Vec::new(),
            });
        }
        Err(error) => return Err(error.into()),
    }
    let mut owners = std::collections::BTreeMap::<PathBuf, (bool, PathBuf)>::new();
    let mut addresses = Vec::new();
    let mut directories = std::collections::BTreeSet::new();
    let mut directory_aliases = Vec::new();
    for entry in walkdir::WalkDir::new(&full).follow_links(false) {
        let entry = entry.map_err(|error| std::io::Error::other(error.to_string()))?;
        if entry.depth() == 0 || entry.file_type().is_dir() {
            directories.insert(entry.path().canonicalize()?);
            continue;
        }
        if entry.depth() > 0
            && entry.file_type().is_symlink()
            && std::fs::metadata(entry.path())?.is_dir()
        {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(std::io::Error::other)?;
            directory_aliases.push(crate::page_path(root, relative)?.canonicalize()?);
            continue;
        }
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "md")
        {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(std::io::Error::other)?;
        let checked = crate::page_path(root, relative)?;
        if !std::fs::metadata(&checked)?.is_file() {
            return Err(
                std::io::Error::other("Markdown collection entry is not a regular file").into(),
            );
        }
        let canonical = checked.canonicalize()?;
        addresses.push(relative.to_path_buf());
        let candidate = (entry.file_type().is_symlink(), relative.to_path_buf());
        owners
            .entry(canonical)
            .and_modify(|owner| {
                if candidate < *owner {
                    *owner = candidate.clone();
                }
            })
            .or_insert(candidate);
    }
    if directory_aliases
        .iter()
        .any(|target| !directories.contains(target))
    {
        return Err(std::io::Error::other(
            "vault collection contains an untraversed directory alias",
        )
        .into());
    }
    let mut paths = owners
        .into_values()
        .map(|(_, path)| path)
        .collect::<Vec<_>>();
    paths.sort();
    addresses.sort();
    Ok(MarkdownInventory {
        owners: paths,
        addresses,
    })
}

pub(crate) fn read_page(root: &Path, path: &Path) -> Result<VaultPage, VaultError> {
    let relative = path.strip_prefix(root).map_err(std::io::Error::other)?;
    let checked = crate::page_path(root, relative)?;
    let content = std::fs::read_to_string(checked).map_err(|error| {
        std::io::Error::new(error.kind(), format!("read {}: {error}", path.display()))
    })?;
    frontmatter::parse_page(&content)
        .map_err(|error| VaultError::Frontmatter(format!("{}: {error}", path.display())))
}

fn load_frontmatter(mut input: impl std::io::BufRead) -> Result<Frontmatter, VaultError> {
    use std::io::{BufRead, Read};

    let mut opening = Vec::new();
    // BOM + delimiter + CRLF fit in eight bytes; the extra byte rejects longer first lines.
    input.by_ref().take(9).read_until(b'\n', &mut opening)?;
    let first = opening.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&opening);
    if first != b"---\n" && first != b"---\r\n" {
        return Ok(Frontmatter::default());
    }
    let mut header = String::from_utf8(opening).map_err(std::io::Error::other)?;
    loop {
        let start = header.len();
        if input.read_line(&mut header)? == 0 {
            return Err(VaultError::Frontmatter("unclosed frontmatter block".into()));
        }
        if frontmatter::is_delimiter_line(&header[start..]) {
            return frontmatter::parse_page(&header)
                .map(|page| page.frontmatter)
                .map_err(VaultError::Frontmatter);
        }
    }
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

    /// Read declared metadata only. Body validity and publication revisions require `read_page`.
    async fn read_frontmatter(&self, rel_path: &Path) -> Result<Option<Frontmatter>, VaultError> {
        Ok(self.read_page(rel_path).await?.map(|page| page.frontmatter))
    }

    /// Vault-relative paths of every `.md` file directly under `rel_dir`, sorted. A
    /// missing directory is the legitimate empty state and yields an empty list.
    async fn list_markdown(&self, rel_dir: &Path) -> Result<Vec<PathBuf>, VaultError>;

    /// Distinct Markdown descendants with complete vault-relative addresses. Filesystem
    /// aliases share one owner, preferring its direct address; hard links stay distinct.
    /// Refuse directory aliases that cannot be traversed completely.
    async fn list_markdown_recursive(&self, rel_dir: &Path) -> Result<Vec<PathBuf>, VaultError>;

    /// Name routing uses owners; allocation reserves every occupied address, including aliases.
    async fn markdown_inventory(&self, rel_dir: &Path) -> Result<MarkdownInventory, VaultError> {
        let owners = self.list_markdown_recursive(rel_dir).await?;
        Ok(MarkdownInventory {
            addresses: owners.clone(),
            owners,
        })
    }
}

/// Load one complete name registry for all lookups in an operation.
pub async fn load_concept_registry(
    reader: &dyn VaultStore,
    dirs: &lk_core::config::VaultDirs,
) -> Result<lk_core::concept::ConceptRegistry, VaultError> {
    use lk_core::concept::{ConceptIdentity, ConceptRegistry};
    let mut registry = ConceptRegistry::new();
    let directory = lk_core::vault_path::concepts_dir(dirs);
    let inventory = reader.markdown_inventory(&directory).await?;
    for path in &inventory.addresses {
        let address = path
            .strip_prefix(&directory)
            .map_err(std::io::Error::other)?
            .with_extension("");
        registry.reserve_address(
            &address
                .to_str()
                .ok_or_else(|| std::io::Error::other("concept address is not UTF-8"))?
                .replace(std::path::MAIN_SEPARATOR, "/"),
        );
    }
    for path in inventory.owners {
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| {
                std::io::Error::other(format!("concept address is not UTF-8: {}", path.display()))
            })?;
        let relative = path
            .strip_prefix(&directory)
            .map_err(std::io::Error::other)?;
        let slug = relative
            .with_extension("")
            .to_str()
            .ok_or_else(|| {
                std::io::Error::other(format!("concept address is not UTF-8: {}", path.display()))
            })?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let metadata = reader.read_frontmatter(&path).await?.ok_or_else(|| {
            std::io::Error::other(format!(
                "concept disappeared during registry read: {}",
                path.display()
            ))
        })?;
        if !metadata.is_concept_or_legacy() {
            continue;
        }
        let title = metadata
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or(stem);
        let aliases = metadata
            .get("aliases")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str())
            .map(String::from)
            .collect::<Vec<_>>();
        registry.register(
            ConceptIdentity {
                slug,
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
    async fn read_frontmatter(&self, rel_path: &Path) -> Result<Option<Frontmatter>, VaultError> {
        let root = self.root.clone();
        let path = rel_path.to_owned();
        tokio::task::spawn_blocking(move || {
            let full = crate::page_path(&root, &path)?;
            let file = match std::fs::File::open(full) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            load_frontmatter(std::io::BufReader::new(file)).map(Some)
        })
        .await
        .map_err(std::io::Error::other)?
    }

    async fn list_markdown_recursive(&self, rel_dir: &Path) -> Result<Vec<PathBuf>, VaultError> {
        Ok(self.markdown_inventory(rel_dir).await?.owners)
    }

    async fn markdown_inventory(&self, rel_dir: &Path) -> Result<MarkdownInventory, VaultError> {
        load_markdown_inventory(&self.root, rel_dir)
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
    async fn read_frontmatter(&self, rel_path: &Path) -> Result<Option<Frontmatter>, VaultError> {
        self.pages
            .get(rel_path)
            .map(|content| load_frontmatter(content.as_bytes()))
            .transpose()
    }

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

    #[test]
    fn metadata_projection_matches_page_delimiters_without_consuming_the_body() {
        for header in [
            "---\ntitle: Example\naliases: [Other]\n---\n",
            "\u{feff}---\r\ntitle: Example\r\n---\r\n",
            "---\nnote: |\n  ---\n  still metadata\ntitle: Example\n---\n",
        ] {
            let mut bytes = header.as_bytes().to_vec();
            bytes.extend_from_slice(&[0xff; 128]);
            let mut input = std::io::Cursor::new(bytes);
            let actual = load_frontmatter(&mut input).unwrap();
            let expected = frontmatter::parse_page(header).unwrap().frontmatter;
            assert_eq!(actual.fields, expected.fields);
            assert_eq!(input.position() as usize, header.len());
        }
        assert!(load_frontmatter(b"---\ntitle: Example\n---".as_slice()).is_ok());
        for body in [
            "--- ",
            "---",
            "  ---\n",
            "# Ordinary text",
            "\u{feff}heading",
        ] {
            let content = body.repeat(4096);
            let mut input = std::io::Cursor::new(content);
            assert!(load_frontmatter(&mut input).unwrap().fields.is_empty());
            assert!(input.position() <= 9);
        }
        for invalid in [
            "---\ntitle: Unclosed\n",
            "---\ntitle: A\ntitle: B\n---\n",
            "---\ntitle: [unfinished\n---\n",
        ] {
            assert!(load_frontmatter(invalid.as_bytes()).is_err());
        }
    }

    #[tokio::test]
    async fn metadata_readers_preserve_absence_and_boundary_errors() {
        let root = tempfile::tempdir().unwrap();
        let vault = FsVault::new(root.path());
        let path = Path::new("page.md");
        assert!(vault.read_frontmatter(path).await.unwrap().is_none());
        assert!(
            vault
                .read_frontmatter(Path::new("../page.md"))
                .await
                .is_err()
        );
        std::fs::write(root.path().join(path), "---\ntitle: Title\n---\nBody.").unwrap();
        assert_eq!(
            vault.read_frontmatter(path).await.unwrap().unwrap().fields,
            vault
                .read_page(path)
                .await
                .unwrap()
                .unwrap()
                .frontmatter
                .fields
        );
        std::fs::write(root.path().join(path), "---\ntitle: Unclosed").unwrap();
        assert!(vault.read_frontmatter(path).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_inventory_shares_symlink_owners_but_preserves_hard_links() {
        use lk_core::concept::Resolution;

        let root = tempfile::tempdir().unwrap();
        let directory = Path::new("wiki/concepts");
        let nested = root.path().join(directory).join("group");
        std::fs::create_dir_all(&nested).unwrap();
        let owner = nested.join("owner.md");
        std::fs::write(
            &owner,
            "---\ntitle: Established\naliases: [Synonym]\n---\nDefinition.\n",
        )
        .unwrap();
        for name in ["a-alias.md", "z-alias.md"] {
            std::os::unix::fs::symlink("group/owner.md", root.path().join(directory).join(name))
                .unwrap();
        }
        std::os::unix::fs::symlink("group", root.path().join(directory).join("directory-alias"))
            .unwrap();
        let reader = FsVault::new(root.path());
        assert_eq!(
            reader.list_markdown_recursive(directory).await.unwrap(),
            vec![directory.join("group/owner.md")]
        );
        let registry = load_concept_registry(&reader, &Default::default())
            .await
            .unwrap();
        assert!(
            matches!(registry.resolve("Synonym"), Resolution::Owned(owner) if owner.slug == "group/owner")
        );
        for name in ["A Alias", "Z Alias"] {
            assert!(matches!(registry.resolve(name), Resolution::Absent));
            assert_ne!(
                registry.available_slug(name),
                lk_core::concept::slugify(name)
            );
        }
        let inventory = reader.markdown_inventory(directory).await.unwrap();
        assert_eq!(inventory.owners.len(), 1);
        assert_eq!(inventory.addresses.len(), 3);

        std::fs::hard_link(&owner, nested.join("independent.md")).unwrap();
        assert_eq!(
            reader
                .list_markdown_recursive(directory)
                .await
                .unwrap()
                .len(),
            2
        );
        let registry = load_concept_registry(&reader, &Default::default())
            .await
            .unwrap();
        let Resolution::Ambiguous { claimants } = registry.resolve("Synonym") else {
            panic!("hard-linked addresses remain distinct owners");
        };
        assert_eq!(claimants.len(), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_inventory_validates_nested_aliases_and_refuses_incomplete_walks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let nested = root.path().join("wiki/concepts/group");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            root.path().join("source.md"),
            "---\ntitle: Alias Owner\n---\nBody.",
        )
        .unwrap();
        std::os::unix::fs::symlink("../../../source.md", nested.join("alias.md")).unwrap();
        let reader = FsVault::new(root.path());
        let registry = load_concept_registry(&reader, &Default::default())
            .await
            .unwrap();
        assert!(
            matches!(registry.resolve("Alias Owner"), lk_core::concept::Resolution::Owned(owner) if owner.slug == "group/alias")
        );
        for target in [outside.path().join("outside.md"), nested.join("missing.md")] {
            std::fs::write(outside.path().join("outside.md"), "Outside.").unwrap();
            std::os::unix::fs::symlink(&target, nested.join("invalid.md")).unwrap();
            assert!(
                load_concept_registry(&reader, &Default::default())
                    .await
                    .is_err()
            );
            std::fs::remove_file(nested.join("invalid.md")).unwrap();
        }
        std::os::unix::fs::symlink(root.path(), nested.join("directory-alias")).unwrap();
        assert!(
            load_concept_registry(&reader, &Default::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn registry_uses_concept_types_but_reserves_every_occupied_address() {
        use lk_core::concept::Resolution;

        let mut vault = InMemoryVault::new().with_page(
            "wiki/concepts/legacy.md",
            "---\ntitle: Legacy\naliases: [LegacyAlias]\n---\nLegacy definition.\n",
        );
        for (slug, declared_type) in [
            ("document", "document"),
            ("null", "null"),
            ("array", "[concept]"),
        ] {
            vault = vault.with_page(
                format!("wiki/concepts/{slug}.md"),
                format!("---\ntype: {declared_type}\ntitle: {slug}\naliases: [Legacy]\n---\nOther content.\n"),
            );
        }
        let registry = load_concept_registry(&vault, &Default::default())
            .await
            .unwrap();
        for name in ["Legacy", "LegacyAlias"] {
            assert!(
                matches!(registry.resolve(name), Resolution::Owned(owner) if owner.slug == "legacy")
            );
        }
        for name in ["document", "null", "array"] {
            assert!(matches!(registry.resolve(name), Resolution::Absent));
            assert_ne!(registry.available_slug(name).as_deref(), Some(name));
        }
        let unreadable = vault.with_page("wiki/concepts/broken.md", "---\ntype: [\n---\n");
        assert!(
            load_concept_registry(&unreadable, &Default::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn registry_preserves_nested_addresses_aliases_and_retired_reservations() {
        use lk_core::concept::Resolution;

        let vault = InMemoryVault::new()
            .with_page(
                "wiki/concepts/first/entry.md",
                "---\ntitle: Established\naliases: [Synonym, Shared]\nknowledge_status: retired\n---\nHistorical definition.\n",
            )
            .with_page(
                "wiki/concepts/second/entry.md",
                "---\ntitle: Other\naliases: [Shared]\n---\nOther definition.\n",
            )
            .with_page("wiki/concepts/third/fallback.md", "No declared title.\n");
        let registry = load_concept_registry(&vault, &Default::default())
            .await
            .unwrap();
        for name in ["Established", "Synonym"] {
            let Resolution::Owned(owner) = registry.resolve(name) else {
                panic!("nested retired owner must reserve {name}");
            };
            assert_eq!(owner.slug, "first/entry");
            assert_eq!(owner.title, "Established");
        }
        let Resolution::Ambiguous { claimants } = registry.resolve("Shared") else {
            panic!("same basename in different folders must remain distinct owners");
        };
        assert_eq!(claimants.len(), 2);
        assert_eq!(claimants[0].slug, "first/entry");
        assert_eq!(claimants[1].slug, "second/entry");
        assert!(matches!(registry.resolve("entry"), Resolution::Absent));
        let Resolution::Owned(fallback) = registry.resolve("fallback") else {
            panic!("title fallback remains the file stem, not the nested address");
        };
        assert_eq!(fallback.slug, "third/fallback");
    }

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
        assert!(
            vault
                .list_markdown_recursive(Path::new("wiki/concepts"))
                .await
                .is_err()
        );
        assert!(read_page(root.path(), &concepts.join("page.md")).is_err());
        assert!(crate::search::markdown_files(root.path(), &concepts).is_err());
        assert!(crate::search::load_pages(root.path(), &Default::default(), false).is_err());
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
        assert_eq!(
            vault
                .list_markdown_recursive(Path::new("wiki/concepts"))
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
        assert!(crate::search::load_pages(root.path(), &Default::default(), false).is_ok());
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
