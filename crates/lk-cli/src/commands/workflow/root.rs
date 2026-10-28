use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{IDENTITY_ENV, ROOT_ENV, SELECTOR_ENV};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    fn load(path: &Path) -> miette::Result<Self> {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::metadata(path)
            .map_err(|error| miette::miette!("inspect workflow vault: {error}"))?;
        if !metadata.is_dir() {
            return Err(miette::miette!("workflow vault must be a directory"));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[derive(Clone, Debug)]
pub(in crate::commands) struct VaultRootPin {
    selected: PathBuf,
    root: PathBuf,
    identity: DirectoryIdentity,
}

fn changed() -> miette::Report {
    miette::miette!(
        "workflow vault selection changed; unfinished work is retained; start a new workflow for the intended vault"
    )
}

impl VaultRootPin {
    pub(in crate::commands) fn capture(selected: &Path, expected: &Path) -> miette::Result<Self> {
        let root = expected
            .canonicalize()
            .map_err(|error| miette::miette!("resolve workflow vault: {error}"))?;
        let pin = Self {
            selected: selected.to_path_buf(),
            identity: DirectoryIdentity::load(&root)?,
            root,
        };
        pin.validate()?;
        Ok(pin)
    }

    pub(in crate::commands) fn inherited() -> miette::Result<Option<Self>> {
        let values = [ROOT_ENV, SELECTOR_ENV, IDENTITY_ENV].map(std::env::var_os);
        if values.iter().all(Option::is_none) {
            if std::env::var_os("LORE_WORKFLOW_ID").is_some()
                && std::env::var_os(super::super::CONFIG_REVISION_ENV).is_some()
            {
                return Err(miette::miette!(
                    "workflow vault context is missing; start a new workflow for the installed version"
                ));
            }
            return Ok(None);
        }
        let [Some(root), Some(selected), Some(identity)] = values else {
            return Err(miette::miette!("incomplete workflow vault context"));
        };
        let identity = identity
            .to_str()
            .ok_or_else(|| miette::miette!("invalid workflow vault identity"))?;
        let pin = Self {
            root: PathBuf::from(root),
            selected: PathBuf::from(selected),
            identity: serde_json::from_str(identity)
                .map_err(|_| miette::miette!("invalid workflow vault identity"))?,
        };
        Ok(Some(pin))
    }

    pub(in crate::commands) fn root(&self) -> &Path {
        &self.root
    }

    pub(in crate::commands) fn validate(&self) -> miette::Result<()> {
        if !self.selected.is_absolute() || !self.root.is_absolute() {
            return Err(miette::miette!("invalid workflow vault paths"));
        }
        let selected = self.selected.canonicalize().map_err(|_| changed())?;
        if DirectoryIdentity::load(&selected)? != self.identity
            || (selected != self.root && DirectoryIdentity::load(&self.root)? != self.identity)
        {
            return Err(changed());
        }
        Ok(())
    }

    pub(in crate::commands) fn validate_target(&self, target: &Path) -> miette::Result<()> {
        self.validate()?;
        if target != self.root {
            let target = target.canonicalize().map_err(|_| changed())?;
            if target != self.root && DirectoryIdentity::load(&target)? != self.identity {
                return Err(changed());
            }
        }
        Ok(())
    }

    pub(in crate::commands) fn configure(&self, command: &mut std::process::Command) {
        command
            .env(ROOT_ENV, &self.root)
            .env(SELECTOR_ENV, &self.selected)
            .env(
                IDENTITY_ENV,
                serde_json::to_string(&self.identity).expect("directory identity serializes"),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn aliases_accept_only_the_captured_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("first");
        let second = fixture.path().join("second");
        let selected = fixture.path().join("selected");
        let equivalent = fixture.path().join("equivalent");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        symlink(&first, &selected).unwrap();
        symlink(&first, &equivalent).unwrap();
        let pin = VaultRootPin::capture(&selected, &first).unwrap();
        pin.validate_target(&equivalent).unwrap();
        assert!(pin.validate_target(&second).is_err());
        std::fs::remove_file(&selected).unwrap();
        symlink(&equivalent, &selected).unwrap();
        pin.validate().unwrap();
        std::fs::remove_file(&selected).unwrap();
        symlink(&second, &selected).unwrap();
        assert!(pin.validate().is_err());
    }

    #[test]
    fn replacing_the_directory_at_the_same_address_refuses() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("vault");
        std::fs::create_dir(&root).unwrap();
        let pin = VaultRootPin::capture(&root, &root).unwrap();
        std::fs::rename(&root, fixture.path().join("old-vault")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(pin.validate().is_err());
    }

    #[test]
    fn case_and_unicode_aliases_follow_actual_directory_identity() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("Root-caf\u{e9}");
        std::fs::create_dir(&root).unwrap();
        let pin = VaultRootPin::capture(&root, &root).unwrap();
        for spelling in ["root-caf\u{e9}", "Root-cafe\u{301}"] {
            let alternate = fixture.path().join(spelling);
            let same = same_file::is_same_file(&root, &alternate).unwrap_or(false);
            assert_eq!(pin.validate_target(&alternate).is_ok(), same);
        }
    }
}
