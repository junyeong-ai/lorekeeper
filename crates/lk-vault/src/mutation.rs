use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::VaultError;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationPlan {
    steps: Vec<Step>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Step {
    Create {
        path: PathBuf,
        after: String,
    },
    Edit {
        path: PathBuf,
        before: String,
        after: String,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        expected: String,
    },
    Remove {
        path: PathBuf,
        expected: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    plan: MutationPlan,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    plan_revision: String,
    completed: usize,
    checksum: String,
}

impl Progress {
    fn new(plan_revision: String, completed: usize) -> Self {
        let checksum = Self::checksum(&plan_revision, completed);
        Self {
            plan_revision,
            completed,
            checksum,
        }
    }

    fn checksum(plan_revision: &str, completed: usize) -> String {
        lk_core::concept::file_revision(&format!("{plan_revision}:{completed}"))
    }
}

impl MutationPlan {
    pub fn create(&mut self, path: impl Into<PathBuf>, after: String) {
        self.steps.push(Step::Create {
            path: path.into(),
            after,
        });
    }

    pub fn publish(
        &mut self,
        root: &Path,
        path: impl Into<PathBuf>,
        after: String,
    ) -> Result<(), VaultError> {
        let path = path.into();
        let full = crate::writable_page_path(root, &path)?;
        crate::writer::refuse_format_change(&full, &after)?;
        match fs::read_to_string(full) {
            Ok(before) => self.edit(path, before, after),
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.create(path, after),
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// Publish from the snapshot that produced the output, never a newly read baseline.
    pub fn publish_expected(
        &mut self,
        root: &Path,
        path: impl Into<PathBuf>,
        before: Option<String>,
        after: String,
    ) -> Result<(), VaultError> {
        let path = path.into();
        let full = crate::writable_page_path(root, &path)?;
        crate::writer::refuse_format_change(&full, &after)?;
        match before {
            Some(before) => self.edit(path, before, after),
            None => self.create(path, after),
        }
        Ok(())
    }

    pub fn edit(&mut self, path: impl Into<PathBuf>, before: String, after: String) {
        if before != after {
            self.steps.push(Step::Edit {
                path: path.into(),
                before,
                after,
            });
        }
    }

    /// Pin an existing dependency even when no replacement is needed. Unlike `edit`,
    /// this retains an equal-before/after step for preflight and journal replay.
    /// The dependency must remain unchanged through the rest of the plan and recovery.
    pub fn require_unchanged(&mut self, path: impl Into<PathBuf>, expected: String) {
        self.steps.push(Step::Edit {
            path: path.into(),
            before: expected.clone(),
            after: expected,
        });
    }

    pub fn rename(&mut self, from: impl Into<PathBuf>, to: impl Into<PathBuf>, expected: String) {
        self.steps.push(Step::Rename {
            from: from.into(),
            to: to.into(),
            expected,
        });
    }

    pub fn remove(&mut self, path: impl Into<PathBuf>, expected: String) {
        self.steps.push(Step::Remove {
            path: path.into(),
            expected,
        });
    }
}

/// Serializes cooperating graph mutations and resumes their durable write plans.
pub struct MutationSession {
    root: PathBuf,
    journal: PathBuf,
    _lock: File,
}

impl MutationSession {
    pub fn acquire(root: &Path) -> Result<Self, VaultError> {
        Self::acquire_with_wait(root, false)
    }

    pub fn acquire_waiting(root: &Path) -> Result<Self, VaultError> {
        Self::acquire_with_wait(root, true)
    }

    fn acquire_with_wait(root: &Path, wait: bool) -> Result<Self, VaultError> {
        let root = root.canonicalize()?;
        let state = root.join(".lorekeeper/mutation");
        if !lk_core::fs::canonical_prefix(&state).starts_with(&root) {
            return Err(io::Error::other("mutation state escapes vault").into());
        }
        create_directories(&state, &root)?;
        let state = state.canonicalize()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state.join("lock"))?;
        if wait {
            lock.lock()?;
        } else {
            lock.try_lock()
                .map_err(|e| io::Error::other(format!("vault mutation is busy: {e}")))?;
        }
        let session = Self {
            root,
            journal: state.join("pending.json"),
            _lock: lock,
        };
        match fs::read(&session.journal) {
            Ok(bytes) => {
                let journal: Journal = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::other(format!("invalid mutation journal: {e}")))?;
                if journal.version != 1 {
                    return Err(io::Error::other("unsupported or invalid mutation journal").into());
                }
                session.finish(&journal)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => session.clear_progress()?,
            Err(e) => return Err(e.into()),
        }
        Ok(session)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn apply(&self, plan: MutationPlan) -> Result<(), VaultError> {
        if self.journal.try_exists()? {
            return Err(io::Error::other(
                "a mutation is pending; recover it before starting another plan",
            )
            .into());
        }
        if plan.steps.is_empty() {
            return Ok(());
        }
        self.preflight(&plan)?;
        self.clear_progress()?;
        let journal = Journal { version: 1, plan };
        self.save(&journal)?;
        self.finish(&journal)
    }

    fn path(&self, relative: &Path) -> Result<PathBuf, VaultError> {
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || relative.starts_with(".lorekeeper/mutation")
        {
            return Err(
                io::Error::other(format!("invalid mutation path: {}", relative.display())).into(),
            );
        }
        let path = crate::writable_page_path(&self.root, relative)?;
        let resolved = lk_core::fs::canonical_prefix(&path);
        if resolved.starts_with(self.journal.parent().expect("journal has a parent")) {
            return Err(io::Error::other("mutation cannot replace its recovery state").into());
        }
        if !resolved.starts_with(&self.root) {
            return Err(io::Error::other(format!(
                "mutation path escapes vault: {}",
                relative.display()
            ))
            .into());
        }
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(io::Error::other(format!(
                "mutation requires a direct file address: {}",
                relative.display()
            ))
            .into());
        }
        Ok(path)
    }

    fn read(&self, path: &Path) -> Result<Option<String>, VaultError> {
        match fs::read_to_string(self.path(path)?) {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn same_entry(&self, from: &Path, to: &Path) -> Result<bool, VaultError> {
        Ok(self.path(from)?.canonicalize()? == self.path(to)?.canonicalize()?)
    }

    fn conflict(path: &Path) -> VaultError {
        io::Error::other(format!("mutation conflict at {}; preserve the journal and reconcile the changed file before retrying", path.display())).into()
    }

    fn preflight(&self, plan: &MutationPlan) -> Result<(), VaultError> {
        let mut state: HashMap<PathBuf, Option<String>> = HashMap::new();
        for step in &plan.steps {
            let paths: Vec<&Path> = match step {
                Step::Create { path, .. } | Step::Edit { path, .. } | Step::Remove { path, .. } => {
                    vec![path]
                }
                Step::Rename { from, to, .. } => vec![from, to],
            };
            for path in paths {
                if !state.contains_key(path) {
                    state.insert(path.to_owned(), self.read(path)?);
                }
            }
            match step {
                Step::Create { path, after } => {
                    if state[path].is_some() {
                        return Err(Self::conflict(path));
                    }
                    state.insert(path.clone(), Some(after.clone()));
                }
                Step::Edit {
                    path,
                    before,
                    after,
                } => {
                    if state[path].as_ref() != Some(before) {
                        return Err(Self::conflict(path));
                    }
                    state.insert(path.clone(), Some(after.clone()));
                }
                Step::Rename { from, to, expected } => {
                    if state[from].as_ref() != Some(expected) {
                        return Err(Self::conflict(from));
                    }
                    if state[to].is_some() && !self.same_entry(from, to)? {
                        return Err(Self::conflict(to));
                    }
                    state.insert(from.clone(), None);
                    state.insert(to.clone(), Some(expected.clone()));
                }
                Step::Remove { path, expected } => {
                    if state[path].as_ref() != Some(expected) {
                        return Err(Self::conflict(path));
                    }
                    state.insert(path.clone(), None);
                }
            }
        }
        Ok(())
    }

    fn save(&self, journal: &Journal) -> Result<(), VaultError> {
        let bytes =
            serde_json::to_vec(journal).map_err(|e| VaultError::Serialization(e.to_string()))?;
        lk_core::fs::write_atomic(&self.journal, &bytes, Some(0o600))?;
        Ok(())
    }

    fn finish(&self, journal: &Journal) -> Result<(), VaultError> {
        let plan = serde_json::to_string(&journal.plan)
            .map_err(|e| VaultError::Serialization(e.to_string()))?;
        let plan_revision = lk_core::concept::file_revision(&plan);
        let progress_path = self.journal.with_file_name("progress.json");
        let mut completed = 0;
        match fs::read(&progress_path) {
            Ok(bytes) => {
                let progress: Progress = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::other(format!("invalid mutation progress: {e}")))?;
                if progress.plan_revision != plan_revision
                    || progress.checksum
                        != Progress::checksum(&progress.plan_revision, progress.completed)
                    || progress.completed > journal.plan.steps.len()
                {
                    return Err(
                        io::Error::other("mutation progress does not match its plan").into(),
                    );
                }
                completed = progress.completed;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        for step in &journal.plan.steps[..completed] {
            if let Step::Edit {
                path,
                before,
                after,
            } = step
                && before == after
                && self.read(path)?.as_ref() != Some(before)
            {
                return Err(Self::conflict(path));
            }
        }
        while let Some(step) = journal.plan.steps.get(completed) {
            match step {
                Step::Create { path, after } => {
                    let current = self.read(path)?;
                    if current.as_ref() != Some(after) {
                        if current.is_some() {
                            return Err(Self::conflict(path));
                        }
                        let absolute = self.path(path)?;
                        if let Some(parent) = absolute.parent() {
                            create_directories(parent, &self.root)?;
                        }
                        lk_core::fs::write_atomic(&absolute, after.as_bytes(), None)?;
                    }
                }
                Step::Edit {
                    path,
                    before,
                    after,
                } => {
                    let current = self.read(path)?;
                    if current.as_ref() != Some(after) {
                        if current.as_ref() != Some(before) {
                            return Err(Self::conflict(path));
                        }
                        lk_core::fs::write_atomic(&self.path(path)?, after.as_bytes(), None)?;
                    }
                }
                Step::Rename { from, to, expected } => {
                    let source = self.read(from)?;
                    let target = self.read(to)?;
                    if source.is_none() && target.as_ref() == Some(expected) {
                        // A completed rename can precede its durable progress receipt.
                    } else {
                        if source.as_ref() != Some(expected) {
                            return Err(Self::conflict(from));
                        }
                        if target.is_some() && !self.same_entry(from, to)? {
                            return Err(Self::conflict(to));
                        }
                        let from = self.path(from)?;
                        let to = self.path(to)?;
                        File::open(&from)?.sync_all()?;
                        fs::rename(&from, &to)?;
                        sync_parent(&from)?;
                        sync_parent(&to)?;
                    }
                }
                Step::Remove { path, expected } => {
                    if let Some(current) = self.read(path)? {
                        if &current != expected {
                            return Err(Self::conflict(path));
                        }
                        let path = self.path(path)?;
                        fs::remove_file(&path)?;
                        sync_parent(&path)?;
                    }
                }
            }
            completed += 1;
            let progress = Progress::new(plan_revision.clone(), completed);
            let bytes = serde_json::to_vec(&progress)
                .map_err(|e| VaultError::Serialization(e.to_string()))?;
            lk_core::fs::write_atomic(&progress_path, &bytes, Some(0o600))?;
        }
        fs::remove_file(&self.journal)?;
        sync_parent(&self.journal)?;
        self.clear_progress()?;
        Ok(())
    }

    fn clear_progress(&self) -> Result<(), VaultError> {
        let path = self.journal.with_file_name("progress.json");
        match fs::remove_file(&path) {
            Ok(()) => sync_parent(&path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}

fn create_directories(path: &Path, root: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    for directory in path.ancestors().take_while(|p| p.starts_with(root)) {
        File::open(directory)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}

fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> MutationPlan {
        let mut p = MutationPlan::default();
        p.rename("Old.md", "new.md", "original".into());
        p.edit("links.md", "Old.md".into(), "new.md".into());
        p
    }

    #[test]
    fn recovery_uses_small_progress_receipts_without_rewriting_the_plan() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("page.md"), "a").unwrap();
        fs::write(root.path().join("guard.md"), "expected").unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut plan = MutationPlan::default();
        plan.edit("page.md", "a".into(), "b".into());
        plan.edit("page.md", "b".into(), "c".into());
        plan.edit("guard.md", "expected".into(), "done".into());
        let journal = Journal { version: 1, plan };
        session.save(&journal).unwrap();
        let original = fs::read(&session.journal).unwrap();
        fs::write(root.path().join("guard.md"), "external").unwrap();
        assert!(session.finish(&journal).is_err());
        assert_eq!(fs::read(&session.journal).unwrap(), original);
        let receipt: Progress = serde_json::from_slice(
            &fs::read(session.journal.with_file_name("progress.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt.completed, 2);
        drop(session);
        fs::write(root.path().join("guard.md"), "expected").unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("page.md")).unwrap(),
            "c"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("guard.md")).unwrap(),
            "done"
        );
        assert!(!session.journal.exists());
        assert!(!session.journal.with_file_name("progress.json").exists());
    }

    #[test]
    fn recovery_rechecks_preserved_dependencies_before_retiring_originals() {
        for damaged in [None, Some("archive.md"), Some("receipt.json")] {
            for remove in [false, true] {
                let root = tempfile::tempdir().unwrap();
                fs::write(root.path().join("original.md"), "original").unwrap();
                fs::write(root.path().join("sentinel.md"), "expected").unwrap();
                let session = MutationSession::acquire(root.path()).unwrap();
                let mut plan = MutationPlan::default();
                for path in ["archive.md", "receipt.json"] {
                    plan.create(path, "preserved".into());
                    plan.require_unchanged(path, "preserved".into());
                }
                plan.edit("sentinel.md", "expected".into(), "done".into());
                plan.remove("original.md", "original".into());
                let journal = Journal { version: 1, plan };
                session.save(&journal).unwrap();
                fs::write(root.path().join("sentinel.md"), "external").unwrap();
                assert!(session.finish(&journal).is_err());
                let progress_path = session.journal.with_file_name("progress.json");
                let progress_bytes = fs::read(&progress_path).unwrap();
                let progress: Progress = serde_json::from_slice(&progress_bytes).unwrap();
                assert_eq!(progress.completed, 4);
                let journal_path = session.journal.clone();
                let journal_bytes = fs::read(&journal_path).unwrap();
                if let Some(path) = damaged {
                    if remove {
                        fs::remove_file(root.path().join(path)).unwrap();
                    } else {
                        fs::write(root.path().join(path), "changed").unwrap();
                    }
                }
                fs::write(root.path().join("sentinel.md"), "expected").unwrap();
                drop(session);
                let resumed = MutationSession::acquire(root.path());
                if damaged.is_some() {
                    assert!(resumed.is_err());
                    assert_eq!(
                        fs::read_to_string(root.path().join("original.md")).unwrap(),
                        "original"
                    );
                    assert_eq!(
                        fs::read_to_string(root.path().join("sentinel.md")).unwrap(),
                        "expected"
                    );
                    assert_eq!(fs::read(&journal_path).unwrap(), journal_bytes);
                    assert_eq!(fs::read(&progress_path).unwrap(), progress_bytes);
                } else {
                    assert!(resumed.is_ok());
                    assert!(!root.path().join("original.md").exists());
                    assert!(!journal_path.exists());
                    assert!(!progress_path.exists());
                }
            }
        }
    }

    #[test]
    fn orphaned_progress_is_removed_before_a_new_plan() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let progress = session.journal.with_file_name("progress.json");
        fs::write(&progress, "orphaned").unwrap();
        drop(session);
        let session = MutationSession::acquire(root.path()).unwrap();
        assert!(!progress.exists());
        let mut plan = MutationPlan::default();
        plan.create("page.md", "new".into());
        session.apply(plan).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("page.md")).unwrap(),
            "new"
        );
    }

    #[test]
    fn mismatched_progress_preserves_the_pending_plan() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut plan = MutationPlan::default();
        plan.create("page.md", "new".into());
        session.save(&Journal { version: 1, plan }).unwrap();
        let receipt = Progress::new("different".into(), 1);
        fs::write(
            session.journal.with_file_name("progress.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        drop(session);
        assert!(MutationSession::acquire(root.path()).is_err());
        assert!(!root.path().join("page.md").exists());
        assert!(
            root.path()
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
    }

    #[test]
    fn a_corrupt_cursor_cannot_skip_unfinished_writes() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut plan = MutationPlan::default();
        plan.create("page.md", "new".into());
        let revision = lk_core::concept::file_revision(&serde_json::to_string(&plan).unwrap());
        session.save(&Journal { version: 1, plan }).unwrap();
        let mut receipt = Progress::new(revision, 0);
        receipt.completed = 1;
        fs::write(
            session.journal.with_file_name("progress.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        drop(session);
        assert!(MutationSession::acquire(root.path()).is_err());
        assert!(!root.path().join("page.md").exists());
        assert!(
            root.path()
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
    }

    #[test]
    fn a_pending_plan_cannot_be_overwritten_by_the_next_result() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut first = MutationPlan::default();
        first.create("first.md", "first".into());
        session
            .save(&Journal {
                version: 1,
                plan: first,
            })
            .unwrap();
        let pending = fs::read(&session.journal).unwrap();
        let mut next = MutationPlan::default();
        next.create("next.md", "next".into());
        assert!(session.apply(next).is_err());
        assert_eq!(fs::read(&session.journal).unwrap(), pending);
        assert!(!root.path().join("next.md").exists());
        drop(session);
        let _session = MutationSession::acquire(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("first.md")).unwrap(),
            "first"
        );
    }

    #[test]
    fn rename_does_not_accept_a_distinct_hard_link_as_a_spelling_alias() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("old.md"), "original").unwrap();
        fs::hard_link(root.path().join("old.md"), root.path().join("new.md")).unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut plan = MutationPlan::default();
        plan.rename("old.md", "new.md", "original".into());
        assert!(session.apply(plan).is_err());
        assert!(root.path().join("old.md").exists());
        assert!(root.path().join("new.md").exists());
        assert!(!session.journal.exists());
    }

    #[test]
    fn creation_recovery_keeps_the_original_before_a_later_conflict() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("origin.md"), "original").unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut plan = MutationPlan::default();
        plan.create("new.md", "concept".into());
        plan.edit("origin.md", "original".into(), "linked".into());
        session.save(&Journal { version: 1, plan }).unwrap();
        fs::write(root.path().join("new.md"), "concept").unwrap();
        fs::write(root.path().join("origin.md"), "user edit").unwrap();
        drop(session);
        assert!(MutationSession::acquire(root.path()).is_err());
        assert_eq!(
            fs::read_to_string(root.path().join("origin.md")).unwrap(),
            "user edit"
        );
        fs::write(root.path().join("origin.md"), "original").unwrap();
        let _session = MutationSession::acquire(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("origin.md")).unwrap(),
            "linked"
        );
    }

    #[test]
    fn recovery_resumes_a_rename_without_a_progress_receipt() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("Old.md"), "original").unwrap();
        fs::write(root.path().join("links.md"), "Old.md").unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        session
            .save(&Journal {
                version: 1,
                plan: plan(),
            })
            .unwrap();
        fs::rename(root.path().join("Old.md"), root.path().join("new.md")).unwrap();
        drop(session);
        let session = MutationSession::acquire(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("links.md")).unwrap(),
            "new.md"
        );
        assert!(!session.journal.exists());
    }

    #[test]
    fn recovery_preserves_external_edits_and_the_pending_plan() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("Old.md"), "original").unwrap();
        fs::write(root.path().join("links.md"), "Old.md").unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        session
            .save(&Journal {
                version: 1,
                plan: plan(),
            })
            .unwrap();
        fs::rename(root.path().join("Old.md"), root.path().join("new.md")).unwrap();
        fs::write(root.path().join("links.md"), "user edit").unwrap();
        drop(session);
        assert!(MutationSession::acquire(root.path()).is_err());
        assert_eq!(
            fs::read_to_string(root.path().join("links.md")).unwrap(),
            "user edit"
        );
        assert!(
            root.path()
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
    }

    #[test]
    fn preflight_checks_the_entire_plan_before_any_write() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("Old.md"), "original").unwrap();
        fs::write(root.path().join("links.md"), "changed").unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        assert!(session.apply(plan()).is_err());
        assert!(root.path().join("Old.md").exists());
        assert!(!root.path().join("new.md").exists());
        assert!(!session.journal.exists());
        assert!(MutationSession::acquire(root.path()).is_err());
    }
}
