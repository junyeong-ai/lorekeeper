use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::DistError;
use crate::embedded::EmbeddedFile;

/// A selected root may be an alias; manifest-derived directories may not be links.
pub(crate) struct ArtifactTree {
    root: PathBuf,
    directory: PathBuf,
    parents: Vec<PathBuf>,
}

impl ArtifactTree {
    pub(crate) fn build(
        root: &Path,
        directory: &Path,
        files: &[EmbeddedFile],
    ) -> Result<Self, DistError> {
        let relative = directory.strip_prefix(root).map_err(|_| {
            DistError::Unsupported(format!(
                "deployment directory is outside its selected root: {}",
                directory.display()
            ))
        })?;
        let root = lk_core::fs::resolve_resource(root)
            .map_err(|error| DistError::io(format!("resolve {}", root.display()), error))?;
        let mut parents = BTreeSet::from([root.clone()]);
        append_directories(&root, relative, &mut parents)?;
        let directory = root.join(relative);
        for file in files {
            let relative = Path::new(file.relative);
            validate_relative(relative)?;
            if relative.as_os_str().is_empty() {
                return Err(DistError::Unsupported(
                    "carried file has an empty path".into(),
                ));
            }
            if let Some(parent) = relative.parent() {
                append_directories(&directory, parent, &mut parents)?;
            }
        }
        let mut parents: Vec<_> = parents.into_iter().collect();
        parents.sort_by_key(|path| path.components().count());
        for path in &parents {
            validate_directory(path)?;
        }
        Ok(Self {
            root,
            directory,
            parents,
        })
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn provision(&self) -> Result<(), DistError> {
        std::fs::create_dir_all(&self.root)
            .map_err(|error| DistError::io(format!("create {}", self.root.display()), error))?;
        for path in &self.parents {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    validate_directory(path)?;
                }
                Err(error) => {
                    return Err(DistError::io(format!("create {}", path.display()), error));
                }
            }
        }
        Ok(())
    }
}

fn validate_relative(path: &Path) -> Result<(), DistError> {
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(DistError::Unsupported(format!(
            "carried path must be relative without dot components: {}",
            path.display()
        )));
    }
    Ok(())
}

fn append_directories(
    root: &Path,
    relative: &Path,
    parents: &mut BTreeSet<PathBuf>,
) -> Result<(), DistError> {
    validate_relative(relative)?;
    let mut current = root.to_path_buf();
    for part in relative.components() {
        current.push(part);
        parents.insert(current.clone());
    }
    Ok(())
}

fn validate_directory(path: &Path) -> Result<(), DistError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(DistError::Unsupported(format!(
            "deployment directory must be a real directory, not a link or another file type: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DistError::io(format!("inspect {}", path.display()), error)),
    }
}

pub(crate) fn load_text(path: &Path) -> std::io::Result<String> {
    let mut file = lk_core::fs::open_regular_file(path)?;
    let mut body = String::new();
    file.read_to_string(&mut body)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> [EmbeddedFile; 1] {
        [EmbeddedFile {
            relative: "references/a.md",
            contents: "reference",
            mode: 0o644,
        }]
    }

    #[test]
    fn planning_validates_before_provisioning_and_rejects_escaping_manifests() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("missing");
        let directory = root.join("skill");
        let tree = ArtifactTree::build(&root, &directory, &files()).unwrap();
        assert!(!root.exists());
        let invalid = [EmbeddedFile {
            relative: "../escape.md",
            ..files()[0]
        }];
        assert!(ArtifactTree::build(&root, &directory, &invalid).is_err());
        assert!(!root.exists());
        tree.provision().unwrap();
        assert!(directory.join("references").is_dir());
        std::fs::write(directory.join("references/occupied"), "occupied").unwrap();
        assert!(
            ArtifactTree::build(&root, &directory.join("references/occupied"), &files()).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn selected_aliases_resolve_but_derived_aliases_refuse() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("actual");
        std::fs::create_dir(&root).unwrap();
        let alias = temporary.path().join("루트 별칭");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let tree = ArtifactTree::build(&alias, &alias.join("skill"), &files()).unwrap();
        tree.provision().unwrap();
        assert_eq!(tree.directory(), root.canonicalize().unwrap().join("skill"));
        let link = root.join("linked");
        std::os::unix::fs::symlink(root.join("skill"), &link).unwrap();
        assert!(matches!(
            ArtifactTree::build(&alias, &alias.join("linked"), &files()),
            Err(DistError::Unsupported(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn junctions_are_aliases_only_at_the_selected_root() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        let target = temporary.path().join("target");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&target).unwrap();
        let link = root.join("junction");
        let result = std::process::Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:LK_DIST_TEST_LINK -Target $env:LK_DIST_TEST_TARGET | Out-Null"])
            .env("LK_DIST_TEST_LINK", &link).env("LK_DIST_TEST_TARGET", &target)
            .output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(matches!(
            ArtifactTree::build(&root, &link, &files()),
            Err(DistError::Unsupported(_))
        ));
        ArtifactTree::build(&link, &link.join("skill"), &files())
            .unwrap()
            .provision()
            .unwrap();
        assert!(target.join("skill/references").is_dir());
    }
}
