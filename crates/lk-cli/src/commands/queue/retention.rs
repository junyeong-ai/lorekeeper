use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_vault::{MutationPlan, MutationSession};
use miette::IntoDiagnostic;

use super::{PruneSummary, TaskStatus};

pub(super) struct PreparedPrune {
    summary: PruneSummary,
    plan: MutationPlan,
    root: PathBuf,
    selection_directories: Vec<PathBuf>,
    removed: Vec<(&'static str, usize)>,
}

impl PreparedPrune {
    pub fn preview(self) -> miette::Result<PruneSummary> {
        if self.plan.has_changes()
            && self
                .root
                .join(".lorekeeper/mutation/pending.json")
                .try_exists()
                .into_diagnostic()?
        {
            return Err(miette::miette!(
                "pending mutation must be recovered before queue retirement preview"
            ));
        }
        Ok(self.summary)
    }

    pub fn apply(self, session: &MutationSession) -> miette::Result<PruneSummary> {
        if session.root() != self.root {
            return Err(miette::miette!("queue retirement belongs to another vault"));
        }
        if self.plan.has_changes() {
            session.apply(self.plan).into_diagnostic()?;
        }
        super::super::work::cleanup_selection_directories(
            session.root(),
            &self.selection_directories,
        )?;
        for (kind, count) in self.removed {
            if count != 0 {
                eprintln!("pruned {count} unreferenced {kind}");
            }
        }
        Ok(self.summary)
    }
}

pub(super) fn prepare(root: &Path, queue: &Path) -> miette::Result<PreparedPrune> {
    let mut summary = PruneSummary::default();
    let mut plan = MutationPlan::default();
    let mut revisions = BTreeMap::new();
    let mut pending = BTreeSet::new();
    let mut retained_pending = BTreeSet::new();
    let mut retained = BTreeSet::new();
    let mut changed = BTreeSet::new();
    let mut archives = BTreeSet::new();
    let mut complete = true;
    super::rejections::validate_store(root)?;
    for file in super::pending_queue_files(queue)? {
        let (raw, read) = match lk_queue::read_tasks_snapshot(&file) {
            Ok(snapshot) => snapshot,
            Err(error) => match std::fs::metadata(&file) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => continue,
                _ => return Err(error).into_diagnostic(),
            },
        };
        let relative = file.strip_prefix(root).into_diagnostic()?.to_path_buf();
        for task in &read.tasks {
            pending.insert((task.task_id.clone(), task.cache_hash.clone()));
        }
        if !read.is_rewritable() {
            complete = false;
            summary.kept_unparseable += read.unparseable;
            summary.kept_blocked += read.tasks.len();
            if let Ok(raw) = String::from_utf8(raw) {
                plan.require_unchanged(relative, raw);
            }
            continue;
        }
        let raw = String::from_utf8(raw).into_diagnostic()?;
        let mut tasks = Vec::with_capacity(read.tasks.len());
        let (mut dropped, mut work_left) = (0, 0);
        for task in read.tasks {
            let valid = task.generation_policy().is_ok() && task.validate_retained_input().is_ok();
            let status = if valid {
                super::classify_validated_task(root, &task, &mut revisions)?
            } else {
                complete = false;
                super::classify_task(root, &task, &mut revisions)?
            };
            match status {
                TaskStatus::Current => {
                    work_left += 1;
                    summary.kept_current += 1;
                    tasks.push(task);
                }
                TaskStatus::Done => {
                    summary.retired_done += 1;
                    tasks.push(task);
                }
                TaskStatus::Stale => {
                    summary.pruned_stale += 1;
                    dropped += 1;
                }
                TaskStatus::MissingTarget => {
                    summary.pruned_missing_target += 1;
                    dropped += 1;
                }
                TaskStatus::Unreadable => {
                    work_left += 1;
                    summary.kept_unreadable += 1;
                    tasks.push(task);
                }
            }
        }
        for task in &tasks {
            let identity = (task.task_id.clone(), task.cache_hash.clone());
            retained.insert(identity.clone());
            if work_left != 0 {
                retained_pending.insert(identity);
            }
        }
        if tasks.is_empty() {
            summary.files_deleted += 1;
            changed.insert(relative.clone());
            plan.remove(relative, raw);
        } else if work_left == 0 {
            summary.files_archived += 1;
            let destination = super::free_path_avoiding(
                &queue.join(lk_queue::PROCESSED_SUBDIR),
                &file,
                &archives,
            );
            archives.insert(destination.clone());
            let destination = destination
                .strip_prefix(root)
                .into_diagnostic()?
                .to_path_buf();
            changed.insert(relative.clone());
            let after = if dropped == 0 {
                raw.clone()
            } else {
                lk_queue::serialize_tasks(&tasks).into_diagnostic()?
            };
            plan.create(destination, after);
            plan.remove(relative, raw);
        } else if dropped != 0 {
            summary.files_rewritten += 1;
            changed.insert(relative.clone());
            plan.edit(
                relative,
                raw,
                lk_queue::serialize_tasks(&tasks).into_diagnostic()?,
            );
        } else {
            plan.require_unchanged(relative, raw);
        }
    }
    let mut removed = vec![(
        "queue rejection receipt(s)",
        super::rejections::plan_prune(root, complete.then_some(&retained), &mut plan)?,
    )];
    let mut selection_directories = Vec::new();
    if complete {
        #[cfg(unix)]
        {
            removed.push((
                "whole-section action receipt(s)",
                super::super::work::section::plan_prune(root, &retained_pending, &mut plan)?,
            ));
            removed.push((
                "native result action receipt(s)",
                super::super::work::generation::plan_prune(root, &retained_pending, &mut plan)?,
            ));
        }
        removed.push((
            "extraction discovery recipe(s)",
            super::super::work::plan_prune_discovery(root, &retained_pending, &mut plan)?,
        ));
        let selections = super::super::work::plan_prune_selection(
            root,
            &pending,
            &retained_pending,
            &changed,
            &mut plan,
        )?;
        removed.push(("extraction selection ledger(s)", selections.removed));
        selection_directories = selections.directories;
    }
    plan.validate_addresses(root).into_diagnostic()?;
    Ok(PreparedPrune {
        summary,
        plan,
        root: root.canonicalize().into_diagnostic()?,
        selection_directories,
        removed,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        pending: String,
        action: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let request = lk_queue::SummarizeRequest {
                text: "Retained source.".into(),
                max_sentences: 3,
                focus: None,
                locale: lk_core::i18n::Locale::En,
                source_type: None,
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/source.md".into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            };
            let task = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task(
                "retirement",
                lk_queue::TaskKind::Summarize,
                request,
            );
            let pending = format!(" \r\n{}\r\n", serde_json::to_string(&task).unwrap());
            let identity = serde_json::to_vec(&(&task.task_id, &task.cache_hash)).unwrap();
            let action = Path::new(".lorekeeper/queue/section-actions")
                .join(format!("{}.json", blake3::hash(&identity).to_hex()));
            let fixture = Self {
                _directory: directory,
                root,
                pending,
                action,
            };
            fixture.write(Path::new("wiki/documents/source.md"), &format!("---\ntype: document\nllm_inputs:\n  summary: {}\n  summary_done: {}\n---\n## Summary\n\n", task.cache_hash, task.cache_hash));
            fixture.write(Path::new(".lorekeeper/queue/run.jsonl"), &fixture.pending);
            fixture.write(&fixture.action, &json!({"schema":"whole-section-action-v1", "task_id":task.task_id, "cache_hash":task.cache_hash, "context_revision":"a".repeat(64), "request_revision":"b".repeat(64), "reason":"Preserved prior action."}).to_string());
            fixture.write(
                Path::new(".lorekeeper/work/paid/node.json"),
                "untouched paid bytes\n",
            );
            fixture
        }

        fn write(&self, path: &Path, raw: &str) {
            let full = self.root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, raw).unwrap();
        }

        fn prepare(&self) -> PreparedPrune {
            prepare(&self.root, &self.root.join(".lorekeeper/queue")).unwrap()
        }

        fn verify(&self) {
            assert!(!self.root.join(".lorekeeper/queue/run.jsonl").exists());
            assert!(!self.root.join(&self.action).exists());
            assert_eq!(
                std::fs::read_to_string(self.root.join(".lorekeeper/queue/processed/run.jsonl"))
                    .unwrap(),
                self.pending
            );
            assert_eq!(
                std::fs::read_to_string(self.root.join(".lorekeeper/work/paid/node.json")).unwrap(),
                "untouched paid bytes\n"
            );
            assert!(!self.root.join(".lorekeeper/mutation/pending.json").exists());
        }
    }

    #[test]
    fn retirement_recovers_every_prepared_prefix_with_exact_archive_and_paid_bytes() {
        for prefix in 0..=3 {
            let fixture = Fixture::new();
            let prepared = fixture.prepare();
            assert!(prepared.plan.has_changes());
            let plan = serde_json::to_value(&prepared.plan).unwrap();
            let steps = plan["steps"].as_array().unwrap();
            assert_eq!(steps.len(), 3);
            fixture.write(
                Path::new(".lorekeeper/mutation/pending.json"),
                &json!({"version":1, "plan":plan}).to_string(),
            );
            for step in &steps[..prefix] {
                let path = Path::new(step["path"].as_str().unwrap());
                match step["kind"].as_str().unwrap() {
                    "create" => fixture.write(path, step["after"].as_str().unwrap()),
                    "remove" => std::fs::remove_file(fixture.root.join(path)).unwrap(),
                    _ => panic!("unexpected retirement step"),
                }
            }
            let session = MutationSession::acquire(&fixture.root).unwrap();
            fixture.verify();
            assert!(!fixture.prepare().plan.has_changes());
            drop(session);
        }
    }

    #[test]
    fn late_store_corruption_and_changed_inputs_leave_every_queued_file_unchanged() {
        for store in [
            "section-actions/unknown.json",
            "result-actions/unknown.json",
            "discovery/unknown.recovery.json",
            "selection/unknown",
        ] {
            let fixture = Fixture::new();
            let path = Path::new(".lorekeeper/queue").join(store);
            fixture.write(&path, "{}");
            let before = std::fs::read(fixture.root.join(&fixture.action)).unwrap();
            for dry_run in [true, false] {
                assert!(
                    super::super::prune_queue(
                        &fixture.root,
                        &fixture.root.join(".lorekeeper/queue"),
                        dry_run
                    )
                    .is_err(),
                    "{store}"
                );
                assert_eq!(
                    std::fs::read_to_string(fixture.root.join(".lorekeeper/queue/run.jsonl"))
                        .unwrap(),
                    fixture.pending
                );
                assert_eq!(
                    std::fs::read(fixture.root.join(&fixture.action)).unwrap(),
                    before
                );
                assert!(!fixture.root.join(".lorekeeper/queue/processed").exists());
                assert!(
                    !fixture
                        .root
                        .join(".lorekeeper/mutation/pending.json")
                        .exists()
                );
            }
        }
        let fixture = Fixture::new();
        let prepared = fixture.prepare();
        fixture.write(&fixture.action, "{}");
        let session = MutationSession::acquire(&fixture.root).unwrap();
        assert!(prepared.apply(&session).is_err());
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(".lorekeeper/queue/run.jsonl")).unwrap(),
            fixture.pending
        );
        assert!(!fixture.root.join(".lorekeeper/queue/processed").exists());
        assert!(
            !fixture
                .root
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
    }
    #[test]
    fn preview_of_a_change_refuses_an_unrecovered_journal_without_creating_a_claim() {
        let fixture = Fixture::new();
        fixture.write(
            Path::new(".lorekeeper/mutation/pending.json"),
            "unknown historical journal",
        );
        assert!(fixture.prepare().preview().is_err());
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(".lorekeeper/mutation/pending.json"))
                .unwrap(),
            "unknown historical journal"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(".lorekeeper/queue/run.jsonl")).unwrap(),
            fixture.pending
        );
        assert!(!fixture.root.join(".lorekeeper/mutation/lock").exists());
        assert!(!fixture.root.join(".lorekeeper/queue/processed").exists());
    }
}
