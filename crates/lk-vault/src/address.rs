//! Resolve addresses against the filesystem that actually holds this vault.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::VaultError;

/// Resolve a vault-relative page address, including existing symlink ancestors.
/// Missing descendants are allowed; unreadable and dangling ancestors are errors.
pub fn page_path(root: &Path, relative: &Path) -> std::io::Result<PathBuf> {
    use std::path::Component;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(std::io::Error::other("page address must be vault-relative"));
    }
    let canonical_root = destination(root)?;
    let full = root.join(relative);
    if !destination(&full)?.starts_with(&canonical_root) {
        return Err(std::io::Error::other(format!(
            "page address escapes vault: {}",
            relative.display()
        )));
    }
    Ok(full)
}

/// Page publication must not replace a symlink with a different directory entry.
pub fn writable_page_path(root: &Path, relative: &Path) -> std::io::Result<PathBuf> {
    let full = page_path(root, relative)?;
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::other(
            "page publication requires a direct file address",
        )),
        Ok(_) => Ok(full),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(full),
        Err(error) => Err(error),
    }
}

/// Compare directory entries after resolving actual filesystem aliases. Hard
/// links retain distinct addresses; missing entries do not establish identity.
pub fn same_existing_entry(root: &Path, a: &Path, b: &Path) -> Result<bool, VaultError> {
    let canonical = |path: &Path| match page_path(root, path)?.canonicalize() {
        Ok(path) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    };
    Ok(matches!((canonical(a)?, canonical(b)?), (Some(a), Some(b)) if a == b))
}

/// Resolve a page's direct owner without conflating distinct hard-link entries.
/// Missing descendants retain their exact address; invalid/dangling ancestors fail.
pub fn resolve_page_owner(root: &Path, path: &Path) -> Result<PathBuf, VaultError> {
    let full = page_path(root, path)?;
    destination(&full)?
        .strip_prefix(destination(root)?)
        .map(Path::to_path_buf)
        .map_err(|error| VaultError::Io(std::io::Error::other(error.to_string())))
}

/// Find an existing page's owner; missing or dangling entries establish no owner.
pub fn find_page_owner(root: &Path, path: &Path) -> Result<Option<PathBuf>, VaultError> {
    let owner = match page_path(root, path).and_then(|full| full.canonicalize()) {
        Ok(owner) => owner,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    owner
        .strip_prefix(root.canonicalize()?)
        .map(|path| Some(path.to_path_buf()))
        .map_err(|error| VaultError::Io(std::io::Error::other(error.to_string())))
}

pub(crate) fn destination(path: &Path) -> std::io::Result<PathBuf> {
    let (mut resolved, missing) = split_destination(path)?;
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn split_destination(path: &Path) -> std::io::Result<(PathBuf, Vec<std::ffi::OsString>)> {
    let absolute = std::path::absolute(path)?;
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                return Ok((ancestor.canonicalize()?, missing));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = ancestor.file_name() else {
                    return Err(error);
                };
                missing.push(name.to_owned());
                ancestor = ancestor.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Ask the destination filesystem about names that do not exist yet. Private
/// directory-only probes inherit the nearest existing parent's naming rules;
/// payloads and final addresses are never created by this validator.
#[derive(Default)]
pub(crate) struct PlannedAddressResolver {
    probes: BTreeMap<ProbeIdentity, NameProbe>,
}

struct NameProbe {
    directory: tempfile::TempDir,
    ancestor: PathBuf,
    owners: BTreeMap<ProbeIdentity, PathBuf>,
}

#[cfg(unix)]
type ProbeIdentity = (u64, u64);
#[cfg(not(unix))]
type ProbeIdentity = PathBuf;

fn probe_identity(path: &Path) -> std::io::Result<ProbeIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path)?;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    path.canonicalize()
}

impl PlannedAddressResolver {
    pub(crate) fn resolve(&mut self, path: &Path) -> std::io::Result<PathBuf> {
        let (ancestor, missing) = split_destination(path)?;
        if missing.is_empty() {
            return Ok(ancestor);
        }
        let probe = match self.probes.entry(probe_identity(&ancestor)?) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => entry.insert(NameProbe {
                directory: tempfile::Builder::new()
                    .prefix(".lorekeeper-address-")
                    .tempdir_in(&ancestor)?,
                ancestor,
                owners: BTreeMap::new(),
            }),
        };
        let mut candidate = probe.directory.path().to_path_buf();
        let mut owner = probe.ancestor.clone();
        for name in missing.iter().rev() {
            candidate.push(name);
            std::fs::create_dir_all(&candidate)?;
            owner.push(name);
            // Only private directories use inode identity. They cannot be distinct
            // hard-link entries; real page owners still use canonical addresses.
            owner = probe
                .owners
                .entry(probe_identity(&candidate)?)
                .or_insert(owner)
                .clone();
        }
        Ok(owner)
    }

    pub(crate) fn close(self) -> std::io::Result<()> {
        for probe in self.probes.into_values() {
            probe.directory.close()?;
        }
        Ok(())
    }
}

/// Exact addresses stay distinct. A different spelling is an alias only when the
/// filesystem resolves it to one known canonical page. No Unicode/case heuristic
/// establishes existence. Results, including absence, are cached for one scan.
pub struct ExistingPageResolver {
    root: PathBuf,
    canonical_root: PathBuf,
    exact: BTreeSet<PathBuf>,
    canonical: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    cached: BTreeMap<PathBuf, Option<PathBuf>>,
}

impl ExistingPageResolver {
    pub fn new(root: &Path, paths: impl IntoIterator<Item = PathBuf>) -> Result<Self, VaultError> {
        let mut resolver = Self {
            root: root.to_path_buf(),
            canonical_root: std::fs::canonicalize(root)?,
            exact: BTreeSet::new(),
            canonical: BTreeMap::new(),
            cached: BTreeMap::new(),
        };
        for path in paths {
            // A page disappearing after enumeration invalidates this scan. Do
            // not retain its exact address while silently omitting its identity.
            let canonical = std::fs::canonicalize(root.join(&path))?;
            resolver
                .canonical
                .entry(canonical)
                .or_default()
                .insert(path.clone());
            resolver.exact.insert(path);
        }
        Ok(resolver)
    }

    /// Return the known page's actual address, not the requested alias spelling.
    pub fn resolve(&mut self, path: &Path) -> Result<Option<PathBuf>, VaultError> {
        if self.exact.contains(path) {
            return Ok(Some(path.to_path_buf()));
        }
        if let Some(result) = self.cached.get(path) {
            return Ok(result.clone());
        }
        let canonical = match std::fs::canonicalize(self.root.join(path)) {
            Ok(canonical) => Some(canonical),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let result = canonical.and_then(|canonical| {
            let owners = self.canonical.get(&canonical)?;
            let actual = canonical.strip_prefix(&self.canonical_root).ok();
            // Canonical target wins over a scanned symlink alias. If no unique
            // owner exists, guessing would change which authored page is read.
            actual
                .filter(|actual| owners.contains(*actual))
                .map(Path::to_path_buf)
                .or_else(|| (owners.len() == 1).then(|| owners.first().unwrap().clone()))
        });
        self.cached.insert(path.to_path_buf(), result.clone());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn page_publication_checks_existing_ancestors_and_leaf_aliases() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();
        let writer = crate::VaultWriter::new(root.path());
        for path in [
            "outside/page.md",
            "outside/new/page.md",
            "../escape.md",
            "/absolute.md",
        ] {
            assert!(
                writer.write_page_sync(Path::new(path), "new").is_err(),
                "{path}"
            );
            assert!(
                writer.edit_page_sync(Path::new(path), "new").is_err(),
                "{path}"
            );
            assert!(find_page_owner(root.path(), Path::new(path)).is_err());
        }
        assert!(!outside.path().join("page.md").exists());
        assert!(!outside.path().join("new").exists());
        std::fs::create_dir(root.path().join("inside")).unwrap();
        std::os::unix::fs::symlink("inside", root.path().join("alias")).unwrap();
        writer
            .write_page_sync(Path::new("alias/page.md"), "original")
            .unwrap();
        std::os::unix::fs::symlink("inside/page.md", root.path().join("leaf.md")).unwrap();
        assert!(page_path(root.path(), Path::new("leaf.md")).is_ok());
        assert_eq!(
            find_page_owner(root.path(), Path::new("leaf.md")).unwrap(),
            Some("inside/page.md".into())
        );
        std::fs::hard_link(
            root.path().join("inside/page.md"),
            root.path().join("hard.md"),
        )
        .unwrap();
        assert_eq!(
            find_page_owner(root.path(), Path::new("hard.md")).unwrap(),
            Some("hard.md".into())
        );
        assert!(
            writer
                .write_page_sync(Path::new("leaf.md"), "replaced")
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("inside/page.md")).unwrap(),
            "original"
        );
        std::os::unix::fs::symlink("absent", root.path().join("dangling")).unwrap();
        assert!(page_path(root.path(), Path::new("dangling/new.md")).is_err());
        assert_eq!(
            find_page_owner(root.path(), Path::new("dangling/new.md")).unwrap(),
            None
        );
        assert_eq!(
            find_page_owner(root.path(), Path::new("absent.md")).unwrap(),
            None
        );
    }
    #[test]
    fn aliases_follow_the_actual_filesystem_and_exact_addresses_win() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lower.md"), "lower").unwrap();
        let alias_exists = root.path().join("LOWER.md").exists();
        let mut resolver = ExistingPageResolver::new(root.path(), ["lower.md".into()]).unwrap();
        assert_eq!(
            resolver.resolve(Path::new("LOWER.md")).unwrap().is_some(),
            alias_exists
        );
        assert_eq!(
            resolver.resolve(Path::new("lower.md")).unwrap(),
            Some("lower.md".into())
        );
        assert_eq!(resolver.resolve(Path::new("missing.md")).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_real_symlink_alias_can_reach_a_known_page() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("owner.md"), "owner").unwrap();
        std::os::unix::fs::symlink("owner.md", root.path().join("alias.md")).unwrap();
        let mut resolver = ExistingPageResolver::new(root.path(), ["owner.md".into()]).unwrap();
        assert_eq!(
            resolver.resolve(Path::new("alias.md")).unwrap(),
            Some("owner.md".into())
        );
        assert!(
            same_existing_entry(root.path(), Path::new("owner.md"), Path::new("alias.md")).unwrap()
        );
        std::fs::hard_link(root.path().join("owner.md"), root.path().join("hard.md")).unwrap();
        assert!(
            !same_existing_entry(root.path(), Path::new("owner.md"), Path::new("hard.md")).unwrap()
        );
        assert!(
            !same_existing_entry(
                root.path(),
                Path::new("missing.md"),
                Path::new("missing.md")
            )
            .unwrap()
        );
    }
}
