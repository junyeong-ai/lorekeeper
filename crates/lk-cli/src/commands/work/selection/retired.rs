//! The packet-selection store earlier releases kept beside the queue. Screening decisions
//! live per candidate under `queue/screening`, so no command reads this store: it only
//! occupies disk until maintenance removes it.
use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;

const STORE: &str = ".lorekeeper/queue/selection";

fn fail(message: &str) -> miette::Report {
    miette::miette!("retired selection store: {message}")
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::commands) struct RetiredStore {
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
}

pub(in crate::commands) fn present(root: &Path) -> miette::Result<bool> {
    let store = lk_vault::page_path(root, Path::new(STORE)).into_diagnostic()?;
    match std::fs::symlink_metadata(store) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(fail(
            "it must be a direct directory; inspect and remove it by hand",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).into_diagnostic(),
    }
}

/// Every entry of the store, each directory before its contents, without opening a file.
/// Anything but a directory or a regular file refuses the whole retirement.
fn inventory(root: &Path) -> miette::Result<(Vec<PathBuf>, Vec<PathBuf>, u64)> {
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut bytes = 0u64;
    let mut pending = vec![lk_vault::page_path(root, Path::new(STORE)).into_diagnostic()?];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).into_diagnostic()?;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path).into_diagnostic()? {
                pending.push(entry.into_diagnostic()?.path());
            }
            directories.push(path);
        } else if metadata.is_file() {
            bytes = bytes
                .checked_add(metadata.len())
                .ok_or_else(|| fail("size overflow"))?;
            files.push(path);
        } else {
            return Err(fail(
                "an entry is neither a regular file nor a directory; inspect and remove the store by hand",
            ));
        }
    }
    Ok((directories, files, bytes))
}

/// Remove the retired store. The caller holds the workflow lease and passes its mutation
/// session; without a session this only reports what a removal would cover. Nothing reads
/// these bytes, so the removal is not journaled: an interrupted run leaves a smaller store
/// that the next run finishes.
pub(in crate::commands) fn retire(
    root: &Path,
    session: Option<&lk_vault::MutationSession>,
) -> miette::Result<RetiredStore> {
    if session.is_some_and(|session| session.root() != root) {
        return Err(fail("the mutation session belongs to another vault"));
    }
    if !present(root)? {
        return Ok(RetiredStore::default());
    }
    let queue = lk_vault::page_path(root, Path::new(".lorekeeper/queue")).into_diagnostic()?;
    let _claim = crate::commands::queue::claim_writer(&queue, session.is_none())?;
    let (directories, files, bytes) = inventory(root)?;
    if session.is_some() {
        for file in &files {
            std::fs::remove_file(file).into_diagnostic()?;
        }
        for directory in directories.iter().rev() {
            std::fs::remove_dir(directory).into_diagnostic()?;
        }
    }
    Ok(RetiredStore {
        files: files.len(),
        directories: directories.len(),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retirement_reports_first_and_removes_only_plain_files_and_directories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        assert!(!present(&root).unwrap());
        assert_eq!(retire(&root, None).unwrap(), RetiredStore::default());

        let store = root.join(STORE);
        std::fs::create_dir_all(store.join("task/batches")).unwrap();
        std::fs::write(store.join("task/manifest.json"), "not parsed").unwrap();
        std::fs::write(store.join("task/batches/0.json"), [0xff, 0xfe]).unwrap();
        let current = root.join(".lorekeeper/queue/screening/task/ledger.json");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, "{}\n").unwrap();
        let expected = RetiredStore {
            files: 2,
            directories: 3,
            bytes: 12,
        };
        assert!(present(&root).unwrap());
        assert_eq!(retire(&root, None).unwrap(), expected);
        assert!(store.join("task/batches/0.json").exists());

        let session = lk_vault::MutationSession::acquire(&root).unwrap();
        #[cfg(unix)]
        {
            let link = store.join("task/link");
            std::os::unix::fs::symlink(&current, &link).unwrap();
            let error = retire(session.root(), Some(&session)).unwrap_err();
            assert!(error.to_string().contains("neither a regular file"));
            assert!(store.join("task/manifest.json").exists());
            std::fs::remove_file(link).unwrap();
        }
        assert_eq!(retire(session.root(), Some(&session)).unwrap(), expected);
        assert!(!present(&root).unwrap());
        assert_eq!(
            retire(session.root(), Some(&session)).unwrap(),
            RetiredStore::default()
        );
        assert_eq!(std::fs::read_to_string(current).unwrap(), "{}\n");

        std::fs::write(&store, "a file where the store was").unwrap();
        assert!(present(&root).is_err());
        assert!(retire(session.root(), Some(&session)).is_err());
        assert!(store.exists());
    }
}
