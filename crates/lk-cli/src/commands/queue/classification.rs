use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::TaskStatus;

pub(super) struct CapturedClassification {
    status: TaskStatus,
    task_status: TaskStatus,
    pages: BTreeMap<PathBuf, Option<String>>,
}

impl CapturedClassification {
    pub fn status(&self) -> TaskStatus {
        self.status
    }

    pub fn task_status(&self) -> TaskStatus {
        self.task_status
    }

    pub fn into_pages(self) -> BTreeMap<PathBuf, Option<String>> {
        self.pages
    }

    pub fn into_completed_task(mut self) -> miette::Result<Self> {
        if self.status != TaskStatus::Current || self.task_status != TaskStatus::Done {
            return Err(miette::miette!(
                "classification does not prove an answered task with a reusable result"
            ));
        }
        self.status = TaskStatus::Done;
        Ok(self)
    }

    pub fn has_superseded_policy(&self, task: &lk_queue::QueueTask) -> miette::Result<bool> {
        if self.status != TaskStatus::Stale {
            return Ok(false);
        }
        let page = lk_vault::parse_page(self.target_raw(task)?)
            .map_err(|error| miette::miette!("{error}"))?;
        Ok(super::policy_request_status_in_page(&page, task) == Some(TaskStatus::Stale))
    }

    pub fn target_raw(&self, task: &lk_queue::QueueTask) -> miette::Result<&str> {
        let target = super::resolve_target_path(&task.target.vault_path)
            .ok_or_else(|| miette::miette!("classification has no valid target address"))?;
        self.pages
            .get(&target)
            .and_then(Option::as_deref)
            .ok_or_else(|| miette::miette!("classification has no readable target snapshot"))
    }

    pub fn retirement_plan(self) -> miette::Result<lk_vault::MutationPlan> {
        if !matches!(
            self.status,
            TaskStatus::Done | TaskStatus::Stale | TaskStatus::MissingTarget
        ) {
            return Err(miette::miette!(
                "classification does not authorize retirement"
            ));
        }
        let mut plan = lk_vault::MutationPlan::default();
        for (path, raw) in self.pages {
            match raw {
                Some(raw) => plan.require_unchanged(path, raw),
                None => plan.require_absent(path),
            }
        }
        Ok(plan)
    }
}

pub(super) fn capture(
    root: &Path,
    task: &lk_queue::QueueTask,
    revisions: &mut BTreeMap<PathBuf, Result<String, TaskStatus>>,
) -> miette::Result<CapturedClassification> {
    capture_artifact(root, task, revisions, super::Artifact::Task)
}

pub(super) fn capture_result(
    root: &Path,
    task: &lk_queue::QueueTask,
) -> miette::Result<CapturedClassification> {
    capture_artifact(root, task, &mut BTreeMap::new(), super::Artifact::Result)
}

fn capture_artifact(
    root: &Path,
    task: &lk_queue::QueueTask,
    revisions: &mut BTreeMap<PathBuf, Result<String, TaskStatus>>,
    artifact: super::Artifact,
) -> miette::Result<CapturedClassification> {
    let mut pages = BTreeMap::new();
    let mut classification =
        super::classify_with_reader(root, task, revisions, artifact, &mut |path| {
            read_snapshot(root, path, &mut pages)
        })?;
    let retires = |classification: &super::TaskClassification| {
        let status = classification.status;
        matches!(
            status,
            TaskStatus::Done | TaskStatus::Stale | TaskStatus::MissingTarget
        ) || status == TaskStatus::Current && classification.task_status == TaskStatus::Done
    };
    if retires(&classification)
        && classification
            .dependencies
            .iter()
            .any(|path| !pages.contains_key(path))
    {
        pages.clear();
        let mut fresh_revisions = BTreeMap::new();
        classification =
            super::classify_with_reader(root, task, &mut fresh_revisions, artifact, &mut |path| {
                read_snapshot(root, path, &mut pages)
            })?;
        revisions.extend(fresh_revisions);
    }
    if retires(&classification) {
        pages.retain(|path, _| classification.dependencies.contains(path));
    } else {
        pages.clear();
    }
    Ok(CapturedClassification {
        status: classification.status,
        task_status: classification.task_status,
        pages,
    })
}

fn read_snapshot(
    root: &Path,
    path: &Path,
    pages: &mut BTreeMap<PathBuf, Option<String>>,
) -> std::io::Result<String> {
    let raw = match pages.entry(path.to_path_buf()) {
        std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::btree_map::Entry::Vacant(entry) => {
            let raw = match super::read_page(root, path) {
                Ok(raw) => Some(raw),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            entry.insert(raw)
        }
    };
    raw.clone()
        .ok_or_else(|| std::io::ErrorKind::NotFound.into())
}
