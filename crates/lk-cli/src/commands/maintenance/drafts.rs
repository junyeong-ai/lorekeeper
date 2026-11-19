//! Interactive sessions stage observations, curation plans and screening or discovery
//! records under `.lorekeeper/drafts/`, one private entry per draft. A command reads a
//! draft only when given its path, so a draft nobody resumes only occupies disk:
//! maintenance removes one once every file in it is older than retention.
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use miette::IntoDiagnostic;

const STORE: &str = ".lorekeeper/drafts";

fn fail(message: &str) -> miette::Report {
    miette::miette!("drafts: {message}")
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DraftsReport {
    pub removed: usize,
    pub files: usize,
    pub bytes: u64,
    pub recent: usize,
    pub unsupported: usize,
}

pub(super) fn present(root: &Path) -> miette::Result<bool> {
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

struct Draft {
    directories: Vec<PathBuf>,
    files: Vec<PathBuf>,
    bytes: u64,
    newest: Option<SystemTime>,
}

/// A draft's directories, each before its contents, and its files; `None` when it holds
/// anything else, which keeps the whole draft for a person to inspect.
fn inventory(entry: PathBuf) -> miette::Result<Option<Draft>> {
    let mut draft = Draft {
        directories: Vec::new(),
        files: Vec::new(),
        bytes: 0,
        newest: None,
    };
    let mut pending = vec![entry];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).into_diagnostic()?;
        if metadata.is_dir() {
            for child in std::fs::read_dir(&path).into_diagnostic()? {
                pending.push(child.into_diagnostic()?.path());
            }
            draft.directories.push(path);
        } else if metadata.is_file() {
            draft.bytes = draft
                .bytes
                .checked_add(metadata.len())
                .ok_or_else(|| fail("size overflow"))?;
            draft.newest = draft
                .newest
                .max(Some(metadata.modified().into_diagnostic()?));
            draft.files.push(path);
        } else {
            return Ok(None);
        }
    }
    Ok(Some(draft))
}

/// Remove drafts whose newest file predates `cutoff_secs`; a draft without files has none.
/// The caller holds the workflow lease and passes its mutation session; without a session
/// this only reports what a removal would cover. Drafts are not journaled: an interrupted
/// run leaves older files that the next run removes, and a file written meanwhile keeps
/// its draft.
pub(super) fn prune(
    root: &Path,
    session: Option<&lk_vault::MutationSession>,
    cutoff_secs: i64,
) -> miette::Result<DraftsReport> {
    if session.is_some_and(|session| session.root() != root) {
        return Err(fail("the mutation session belongs to another vault"));
    }
    if !present(root)? {
        return Ok(DraftsReport::default());
    }
    let cutoff = u64::try_from(cutoff_secs)
        .ok()
        .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)))
        .ok_or_else(|| fail("invalid retention cutoff"))?;
    let store = lk_vault::page_path(root, Path::new(STORE)).into_diagnostic()?;
    let mut report = DraftsReport::default();
    for entry in std::fs::read_dir(store).into_diagnostic()? {
        let Some(draft) = inventory(entry.into_diagnostic()?.path())? else {
            report.unsupported += 1;
            continue;
        };
        if draft.newest.is_some_and(|newest| newest >= cutoff)
            || (session.is_some() && !remove(&draft)?)
        {
            report.recent += 1;
            continue;
        }
        report.removed += 1;
        report.files += draft.files.len();
        report.bytes = report
            .bytes
            .checked_add(draft.bytes)
            .ok_or_else(|| fail("size overflow"))?;
    }
    Ok(report)
}

/// Whether the draft is gone; a directory that gained an entry during removal stays.
fn remove(draft: &Draft) -> miette::Result<bool> {
    for file in &draft.files {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).into_diagnostic(),
        }
    }
    for directory in draft.directories.iter().rev() {
        match std::fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                return Ok(false);
            }
            Err(error) => return Err(error).into_diagnostic(),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str, age_days: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        let modified = SystemTime::now() - Duration::from_secs(age_days * 86_400);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn maintenance_removes_only_drafts_whose_every_file_has_expired() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let cutoff = (SystemTime::now() - Duration::from_secs(10 * 86_400))
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(!present(&root).unwrap());
        assert_eq!(prune(&root, None, cutoff).unwrap(), DraftsReport::default());

        let store = root.join(STORE);
        write(&store.join("expired/observation.json"), "{}", 20);
        write(&store.join("expired/nested/plan.json"), "[]", 30);
        write(&store.join("resumed/old.json"), "{}", 30);
        write(&store.join("resumed/new.json"), "{}", 1);
        write(&store.join("loose.json"), "{}", 20);
        std::fs::create_dir_all(store.join("empty/inner")).unwrap();
        #[cfg(unix)]
        {
            write(&store.join("linked/old.json"), "{}", 30);
            std::os::unix::fs::symlink(root.join("elsewhere"), store.join("linked/link")).unwrap();
        }
        let expected = DraftsReport {
            removed: 3,
            files: 3,
            bytes: 6,
            recent: 1,
            unsupported: usize::from(cfg!(unix)),
        };
        assert_eq!(prune(&root, None, cutoff).unwrap(), expected);
        assert!(store.join("expired/nested/plan.json").exists());

        let session = lk_vault::MutationSession::acquire(&root).unwrap();
        assert_eq!(
            prune(session.root(), Some(&session), cutoff).unwrap(),
            expected
        );
        for gone in ["expired", "loose.json", "empty"] {
            assert!(!store.join(gone).exists(), "{gone} remains");
        }
        assert!(store.join("resumed/old.json").exists());
        #[cfg(unix)]
        assert!(store.join("linked/old.json").exists());

        std::fs::remove_dir_all(&store).unwrap();
        std::fs::write(&store, "a file where the store was").unwrap();
        assert!(present(&root).is_err());
        assert!(prune(session.root(), Some(&session), cutoff).is_err());
    }
}
