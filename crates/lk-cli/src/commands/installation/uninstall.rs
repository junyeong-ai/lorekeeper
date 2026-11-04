use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use lk_dist::Installation;

use super::{AgentTarget, Protection, protection::resolve_resource};

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq, Hash)]
struct Identity(u64, u64);
#[cfg(not(unix))]
type Identity = same_file::Handle;

fn identity(path: &Path) -> io::Result<Identity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path)?;
        Ok(Identity(metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        same_file::Handle::from_path(path)
    }
}

#[derive(Clone, Copy)]
enum RemovalKind {
    Tree,
    File,
    EmptyDirectory,
}

struct Removal {
    path: PathBuf,
    kind: RemovalKind,
}

struct Vault {
    paths: Vec<PathBuf>,
    root: PathBuf,
    directory: Option<same_file::Handle>,
}

impl Vault {
    fn capture(paths: Vec<PathBuf>) -> miette::Result<Self> {
        let root = resolve_resource(&paths[0]).map_err(io_error)?;
        let directory = match std::fs::metadata(&root) {
            Ok(metadata) if metadata.is_dir() => {
                Some(same_file::Handle::from_path(&root).map_err(io_error)?)
            }
            Ok(_) => return Err(miette::miette!("uninstall vault must be a directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_error(error)),
        };
        let vault = Self {
            paths,
            root,
            directory,
        };
        vault.validate_selection()?;
        Ok(vault)
    }

    fn validate_selection(&self) -> miette::Result<()> {
        self.validate_root()?;
        for path in &self.paths {
            let resolved = resolve_resource(path).map_err(io_error)?;
            match &self.directory {
                Some(expected)
                    if same_file::Handle::from_path(&resolved).map_err(io_error)? != *expected =>
                {
                    return Err(changed());
                }
                None if resolved != self.root => return Err(changed()),
                _ => {}
            }
        }
        Ok(())
    }

    fn validate_root(&self) -> miette::Result<()> {
        match (&self.directory, std::fs::metadata(&self.root)) {
            (Some(expected), Ok(metadata)) if metadata.is_dir() => {
                if resolve_resource(&self.root).map_err(io_error)? != self.root
                    || same_file::Handle::from_path(&self.root).map_err(io_error)? != *expected
                {
                    return Err(changed());
                }
            }
            (None, Err(error)) if error.kind() == io::ErrorKind::NotFound => {}
            (_, Err(error)) => return Err(io_error(error)),
            _ => return Err(changed()),
        }
        Ok(())
    }
}

pub(super) struct RemovalPlan {
    removals: Vec<Removal>,
    vault: Option<Vault>,
    coordination: HashSet<Identity>,
}

impl RemovalPlan {
    pub fn build(
        installation: &Installation,
        protection: &Protection,
        purge_config: bool,
    ) -> miette::Result<Self> {
        let vault = protection.vault_paths().map(Vault::capture).transpose()?;
        let mut coordination = HashSet::new();
        for path in protection.coordination_paths() {
            capture_ancestors(&path, &mut coordination)?;
        }
        let mut plan = Self {
            removals: Vec::new(),
            vault,
            coordination,
        };
        for engine in AgentTarget::Both.engines() {
            let view = installation.for_engine(*engine);
            for level in view.deployed_skill_levels() {
                if let Some(root) = view.skills_dir(level) {
                    for name in lk_dist::skill_names() {
                        plan.push(root.join(name), RemovalKind::Tree)?;
                    }
                }
            }
        }
        plan.push(installation.templates_dir(), RemovalKind::Tree)?;
        plan.push(installation.data_dir().join("skills"), RemovalKind::Tree)?;
        plan.push(
            installation.data_dir().to_path_buf(),
            RemovalKind::EmptyDirectory,
        )?;
        if purge_config {
            plan.push(installation.config_dir().to_path_buf(), RemovalKind::Tree)?;
        } else {
            let selected = protection
                .configuration_path()
                .map(identity)
                .transpose()
                .map_err(io_error)?;
            for name in lk_dist::config_files()
                .iter()
                .map(|file| file.relative)
                .chain(["data-dir", "deployed-skills"])
            {
                let path = installation.config_dir().join(name);
                match identity(&path) {
                    Ok(actual) if selected.as_ref() == Some(&actual) => continue,
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(io_error(error)),
                }
                plan.push(path, RemovalKind::File)?;
            }
        }
        plan.push(installation.binary().to_path_buf(), RemovalKind::File)?;
        plan.validate(protection)?;
        Ok(plan)
    }

    fn push(&mut self, path: PathBuf, kind: RemovalKind) -> miette::Result<()> {
        let Some(name) = path.file_name() else {
            if matches!(kind, RemovalKind::EmptyDirectory) {
                return Ok(());
            }
            return Err(miette::miette!("uninstall removal has no entry name"));
        };
        let parent = path
            .parent()
            .ok_or_else(|| miette::miette!("uninstall removal has no parent"))?;
        let resolved = resolve_resource(parent).map_err(io_error)?.join(name);
        self.removals.push(Removal {
            path: resolved,
            kind,
        });
        Ok(())
    }

    fn vault_ancestors(&self) -> miette::Result<HashSet<Identity>> {
        let mut entries = HashSet::new();
        if let Some(vault) = &self.vault {
            vault.validate_selection()?;
            if vault.directory.is_some() {
                for path in vault.paths.iter().chain(std::iter::once(&vault.root)) {
                    capture_ancestors(path, &mut entries)?;
                }
            }
        }
        Ok(entries)
    }

    pub fn validate(&self, protection: &Protection) -> miette::Result<()> {
        protection.recheck_configuration()?;
        let vault = self.vault_ancestors()?;
        for removal in &self.removals {
            removal.validate(&vault, &self.coordination)?;
        }
        Ok(())
    }

    pub fn display(&self) {
        eprintln!("This removes:");
        for removal in &self.removals {
            if !matches!(removal.kind, RemovalKind::EmptyDirectory) {
                eprintln!("  {}", removal.path.display());
            }
        }
        eprintln!("\nYour vault is not touched.");
    }

    pub fn apply(self, protection: &Protection) -> miette::Result<()> {
        self.validate(protection)?;
        let vault = self.vault_ancestors()?;
        for removal in self.removals {
            if let Some(root) = &self.vault {
                root.validate_root()?;
            }
            if removal.validate(&vault, &self.coordination)? {
                removal.apply()?;
            }
        }
        Ok(())
    }
}

impl Removal {
    fn validate(
        &self,
        vault: &HashSet<Identity>,
        coordination: &HashSet<Identity>,
    ) -> miette::Result<bool> {
        let actual = match identity(&self.path) {
            Ok(actual) => actual,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(io_error(error)),
        };
        if vault.contains(&actual) || coordination.contains(&actual) {
            if matches!(self.kind, RemovalKind::EmptyDirectory) {
                return Ok(false);
            }
            let protected = if vault.contains(&actual) {
                "vault"
            } else {
                "installation coordination"
            };
            return Err(miette::miette!(
                "uninstall removal overlaps protected {protected}: {}",
                self.path.display()
            ));
        }
        let metadata = std::fs::symlink_metadata(&self.path).map_err(io_error)?;
        match self.kind {
            RemovalKind::Tree if !metadata.is_dir() && !metadata.file_type().is_symlink() => Err(
                miette::miette!("uninstall tree is not a directory: {}", self.path.display()),
            ),
            RemovalKind::File if metadata.is_dir() => Err(miette::miette!(
                "uninstall file is a directory: {}",
                self.path.display()
            )),
            _ => Ok(true),
        }
    }

    fn apply(&self) -> miette::Result<()> {
        let result = match self.kind {
            RemovalKind::Tree => std::fs::remove_dir_all(&self.path),
            RemovalKind::File => std::fs::remove_file(&self.path),
            RemovalKind::EmptyDirectory => {
                let _ = std::fs::remove_dir(&self.path);
                return Ok(());
            }
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    }
}

fn capture_ancestors(path: &Path, entries: &mut HashSet<Identity>) -> miette::Result<()> {
    for ancestor in path.ancestors() {
        entries.insert(identity(ancestor).map_err(io_error)?);
    }
    Ok(())
}

fn changed() -> miette::Report {
    miette::miette!("uninstall vault selection changed; no further removal is allowed")
}
fn io_error(error: io::Error) -> miette::Report {
    miette::miette!("prepare uninstall removal scope: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(path: &Path) -> Vault {
        Vault::capture(vec![path.to_path_buf()]).unwrap()
    }

    fn ancestors(vault: &Vault) -> HashSet<Identity> {
        let mut entries = HashSet::new();
        for path in vault.paths.iter().chain(std::iter::once(&vault.root)) {
            capture_ancestors(path, &mut entries).unwrap();
        }
        entries
    }

    #[test]
    fn uninstall_scope_protects_ancestors_but_allows_project_skill_descendants() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("vault");
        let skill = root.join(".agents/skills/lore-wiki");
        std::fs::create_dir_all(&skill).unwrap();
        let vault = captured(&root);
        let protected = ancestors(&vault);
        for path in [root.clone(), temporary.path().to_path_buf()] {
            assert!(
                Removal {
                    path,
                    kind: RemovalKind::Tree
                }
                .validate(&protected, &HashSet::new())
                .is_err()
            );
        }
        assert!(
            Removal {
                path: root.clone(),
                kind: RemovalKind::EmptyDirectory
            }
            .validate(&protected, &HashSet::new())
            .is_ok_and(|remove| !remove)
        );
        let removal = Removal {
            path: skill.clone(),
            kind: RemovalKind::Tree,
        };
        assert!(removal.validate(&protected, &HashSet::new()).unwrap());
        removal.apply().unwrap();
        assert!(!skill.exists());
        vault.validate_selection().unwrap();
    }

    #[test]
    fn uninstall_scope_rejects_replaced_and_newly_created_vaults() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("vault");
        let missing = captured(&root);
        missing.validate_selection().unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(missing.validate_selection().is_err());
        let present = captured(&root);
        std::fs::rename(&root, temporary.path().join("old")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(present.validate_selection().is_err());
    }

    #[test]
    fn uninstall_scope_keeps_claimed_coordinator_entries_and_containers() {
        let temporary = tempfile::tempdir().unwrap();
        let lock = temporary.path().join("coordinator");
        std::fs::write(&lock, b"").unwrap();
        let mut coordination = HashSet::new();
        capture_ancestors(&lock, &mut coordination).unwrap();
        assert!(
            Removal {
                path: lock,
                kind: RemovalKind::File
            }
            .validate(&HashSet::new(), &coordination)
            .is_err()
        );
        assert!(
            Removal {
                path: temporary.path().to_path_buf(),
                kind: RemovalKind::Tree
            }
            .validate(&HashSet::new(), &coordination)
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_scope_checks_native_aliases_and_selector_retargeting() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("Vault-é");
        let other = temporary.path().join("other");
        let selected = temporary.path().join("selected");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&other).unwrap();
        symlink(&root, &selected).unwrap();
        let vault = captured(&selected);
        let protected = ancestors(&vault);
        assert!(
            Removal {
                path: selected.clone(),
                kind: RemovalKind::Tree
            }
            .validate(&protected, &HashSet::new())
            .is_err()
        );
        for alias in [
            temporary.path().join("vault-é"),
            temporary.path().join("Vault-e\u{301}"),
        ] {
            match identity(&alias) {
                Ok(actual) => assert_eq!(
                    protected.contains(&actual),
                    identity(&root).unwrap() == actual
                ),
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
            }
        }
        std::fs::remove_file(&selected).unwrap();
        symlink(&other, &selected).unwrap();
        assert!(vault.validate_selection().is_err());
    }
}
