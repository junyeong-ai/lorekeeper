use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_vault::{MutationPlan, MutationSession};
use miette::IntoDiagnostic;

use super::classification::{CapturedClassification, capture as classify_for_retirement};
use super::{PruneSummary, TaskStatus};

pub(super) struct PreparedPrune {
    summary: PruneSummary,
    plan: MutationPlan,
    root: PathBuf,
    screening_directories: Vec<PathBuf>,
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
        for (kind, count) in self.removed {
            if count != 0 {
                eprintln!("[dry-run] would prune {count} unreferenced {kind}");
            }
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
        super::super::work::cleanup_screening_directories(
            session.root(),
            &self.screening_directories,
        )?;
        for (kind, count) in self.removed {
            if count != 0 {
                eprintln!("pruned {count} unreferenced {kind}");
            }
        }
        Ok(self.summary)
    }
}

/// What a retirement removes besides the pending rows it retires or drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reach {
    /// Pending rows only. A native run retires its queue before planning and leaves sidecar
    /// retention, which also reads processed history, to maintenance.
    Rows,
    /// Pending rows and the receipts, recipes and ledgers no retained task references.
    Sidecars,
}

pub(super) fn prepare(
    root: &Path,
    queue: &Path,
    reach: Reach,
    control: &lk_vault::ReadControl,
) -> miette::Result<PreparedPrune> {
    prepare_with_classifier(root, queue, reach, control, &mut |task, revisions| {
        classify_for_retirement(root, task, revisions)
    })
}

fn prepare_with_classifier(
    root: &Path,
    queue: &Path,
    reach: Reach,
    control: &lk_vault::ReadControl,
    classify: &mut impl FnMut(
        &lk_queue::QueueTask,
        &mut BTreeMap<PathBuf, Result<String, TaskStatus>>,
    ) -> miette::Result<CapturedClassification>,
) -> miette::Result<PreparedPrune> {
    let mut summary = PruneSummary::default();
    let mut plan = MutationPlan::default();
    let mut revisions = BTreeMap::new();
    let mut pending = BTreeSet::new();
    let mut retained_pending = BTreeSet::new();
    let mut retained = BTreeSet::new();
    let mut changed = BTreeSet::new();
    let mut archives = BTreeSet::new();
    let mut pages = BTreeMap::new();
    let mut complete = true;
    // A task a staged result still answers stays pending whatever its classification: apply
    // publishes that result or retires it through its verified path, and only then may a
    // later prune retire the task. Dropping the row first orphans the result.
    let staged = super::rejections::staged_identities(root)?;
    for file in super::pending_queue_files(queue)? {
        control.check().into_diagnostic()?;
        let relative = file.strip_prefix(root).into_diagnostic()?.to_path_buf();
        let full = match lk_vault::page_path(root, &relative) {
            Ok(full) => full,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).into_diagnostic(),
        };
        let metadata = match std::fs::symlink_metadata(&file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).into_diagnostic(),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(miette::miette!(
                "queue retirement requires direct regular files"
            ));
        }
        let (raw, read) = match lk_queue::read_tasks_snapshot(&full) {
            Ok(snapshot) => snapshot,
            Err(error) => match std::fs::metadata(&file) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => continue,
                _ => return Err(error).into_diagnostic(),
            },
        };
        for task in &read.tasks {
            pending.insert((task.task_id.clone(), task.cache_hash.clone()));
        }
        if !read.is_rewritable() {
            complete = false;
            summary.kept_unparseable += read.unparseable;
            summary.kept_blocked += read.tasks.len();
            if let Ok(raw) = String::from_utf8(raw) {
                plan.require(relative, raw, lk_vault::Pin::Editable);
            }
            continue;
        }
        let raw = String::from_utf8(raw).into_diagnostic()?;
        let lines = row_lines(&raw);
        if lines.len() != read.tasks.len() {
            return Err(miette::miette!(
                "queue file rows do not match their decoded tasks: {}",
                relative.display()
            ));
        }
        let mut rows = Vec::with_capacity(read.tasks.len());
        let mut done_pages = Ok(BTreeMap::new());
        let (mut dropped, mut work_left) = (0, 0);
        for (line, task) in lines.into_iter().zip(read.tasks) {
            if task.generation_policy().is_err() || task.validate_retained_input().is_err() {
                complete = false;
            }
            let classification = classify(&task, &mut revisions)?;
            let status = classification.status();
            let captured_pages = classification.into_pages();
            if status != TaskStatus::Current
                && status != TaskStatus::Unreadable
                && staged.contains(&(task.task_id.clone(), task.cache_hash.clone()))
            {
                work_left += 1;
                summary.kept_staged += 1;
                rows.push((task, line, false));
                continue;
            }
            match status {
                TaskStatus::Current => {
                    work_left += 1;
                    summary.kept_current += 1;
                    rows.push((task, line, false));
                }
                // Processed history must hold the row as written, or the next full cleanup that
                // reads it refuses; such an answer stays pending and guards nothing.
                TaskStatus::Done if !super::rejections::is_retainable(line) => {
                    summary.kept_answered += 1;
                    rows.push((task, line, false));
                }
                TaskStatus::Done => {
                    done_pages = done_pages.and_then(|mut pages| {
                        retain_snapshots(&mut pages, captured_pages)?;
                        Ok(pages)
                    });
                    rows.push((task, line, true));
                }
                TaskStatus::Stale => {
                    retain_snapshots(&mut pages, captured_pages).map_err(classification_drift)?;
                    summary.pruned_stale += 1;
                    dropped += 1;
                }
                TaskStatus::MissingTarget => {
                    retain_snapshots(&mut pages, captured_pages).map_err(classification_drift)?;
                    summary.pruned_missing_target += 1;
                    dropped += 1;
                }
                TaskStatus::Unreadable => {
                    work_left += 1;
                    summary.kept_unreadable += 1;
                    rows.push((task, line, false));
                }
                TaskStatus::InvalidTarget => {
                    work_left += 1;
                    summary.kept_invalid_target += 1;
                    rows.push((task, line, false));
                }
            }
        }
        // An answered task leaves the pending queue for processed history once the pages that
        // proved it answered agree, with each other and with every other retirement's captures,
        // whatever its run still holds. Only a run whose rows all retire must prove that; a run
        // with work left keeps answers it could not prove.
        let proof = done_pages.and_then(|done_pages| match conflict(&pages, &done_pages) {
            Some(path) => Err(path),
            None => Ok(done_pages),
        });
        let retire_answered = match proof {
            Ok(done_pages) => {
                retain_snapshots(&mut pages, done_pages).map_err(classification_drift)?;
                true
            }
            Err(path) if work_left == 0 => return Err(classification_drift(path)),
            Err(_) => false,
        };
        let done = rows.iter().filter(|(_, _, answered)| *answered).count();
        let (answered, kept): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|(_, _, answered)| *answered && retire_answered);
        let history: String = answered.iter().map(|(_, line, _)| *line).collect();
        let remaining: String = kept.iter().map(|(_, line, _)| *line).collect();
        let answered: Vec<_> = answered.into_iter().map(|(task, _, _)| task).collect();
        let kept: Vec<_> = kept.into_iter().map(|(task, _, _)| task).collect();
        summary.retired_done += answered.len();
        summary.kept_answered += done - answered.len();
        for task in answered.iter().chain(&kept) {
            retained.insert((task.task_id.clone(), task.cache_hash.clone()));
        }
        for task in &kept {
            retained_pending.insert((task.task_id.clone(), task.cache_hash.clone()));
        }
        if kept.is_empty() && answered.is_empty() {
            summary.files_deleted += 1;
            changed.insert(relative.clone());
            plan.remove(relative, raw);
            continue;
        }
        if !answered.is_empty() {
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
            if !kept.is_empty() {
                summary.files_rewritten += 1;
                plan.create(destination, history);
                plan.edit(relative, raw, remaining);
            } else if dropped == 0 {
                plan.rename(relative, destination, raw);
            } else {
                plan.rename(relative, destination.clone(), raw.clone());
                plan.edit(destination, raw, history);
            }
        } else if dropped != 0 {
            summary.files_rewritten += 1;
            changed.insert(relative.clone());
            plan.edit(relative, raw, remaining);
        } else {
            plan.require(relative, raw, lk_vault::Pin::Editable);
        }
    }
    let mut guarded = MutationPlan::default();
    for (path, raw) in pages {
        match raw {
            Some(raw) => guarded.require(path, raw, lk_vault::Pin::Editable),
            None => guarded.require_absent(path),
        }
    }
    guarded.extend(plan);
    let mut plan = guarded;
    let mut removed = Vec::new();
    let mut screening_directories = Vec::new();
    if reach == Reach::Sidecars {
        removed.push((
            "queue rejection receipt(s)",
            super::rejections::plan_prune(root, complete.then_some(&retained), &mut plan)?,
        ));
    }
    if reach == Reach::Sidecars && complete {
        removed.push((
            "native preparation attempt(s)",
            super::super::work::preparation::plan_prune_queue(root, &retained_pending, &mut plan)?,
        ));
        #[cfg(unix)]
        {
            removed.push((
                "whole-section action receipt(s)",
                super::super::work::section::plan_prune(root, &retained_pending, &mut plan)?,
            ));
            removed.push((
                "native generation receipt(s)",
                super::super::work::generation::plan_prune(root, &retained_pending, &mut plan)?,
            ));
        }
        removed.push((
            "extraction discovery recipe(s)",
            super::super::work::plan_prune_discovery(root, &retained_pending, &mut plan)?,
        ));
        removed.push((
            "synthesis context snapshot(s)",
            super::super::work::plan_prune_context_snapshots(root, &retained_pending, &mut plan)?,
        ));
        let screening = super::super::work::plan_prune_screening(
            root,
            &pending,
            &retained_pending,
            &changed,
            &mut plan,
        )?;
        removed.push(("extraction screening ledger(s)", screening.removed));
        screening_directories = screening.directories;
    }
    plan.validate_addresses(root).into_diagnostic()?;
    Ok(PreparedPrune {
        summary,
        plan,
        root: root.canonicalize().into_diagnostic()?,
        screening_directories,
        removed,
    })
}

fn retain_snapshots(
    retained: &mut BTreeMap<PathBuf, Option<String>>,
    snapshots: BTreeMap<PathBuf, Option<String>>,
) -> Result<(), PathBuf> {
    for (path, raw) in snapshots {
        match retained.entry(path) {
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() != &raw => {
                return Err(entry.key().clone());
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(raw);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Each decoded row's exact line, terminator included, in file order. A rewritable file's
/// nonblank lines are its rows one to one; rows move or stay as written, so retirement never
/// re-encodes retained input or drops a field its decoder does not know.
fn row_lines(raw: &str) -> Vec<&str> {
    raw.split_inclusive('\n')
        .filter(|line| !line.bytes().all(|byte| byte.is_ascii_whitespace()))
        .collect()
}

/// The first page both captures hold with different bytes or absence.
fn conflict(
    retained: &BTreeMap<PathBuf, Option<String>>,
    snapshots: &BTreeMap<PathBuf, Option<String>>,
) -> Option<PathBuf> {
    snapshots
        .iter()
        .find(|(path, raw)| retained.get(*path).is_some_and(|held| held != *raw))
        .map(|(path, _)| path.clone())
}

fn classification_drift(path: PathBuf) -> miette::Report {
    miette::miette!(
        "queue classification inputs changed during retirement: {}",
        path.display()
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        pending: String,
        archived: String,
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
                archived: pending.clone(),
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
            prepare(
                &self.root,
                &self.root.join(".lorekeeper/queue"),
                Reach::Sidecars,
                &lk_vault::ReadControl::default(),
            )
            .unwrap()
        }

        fn add_missing_task(&mut self) {
            let mut task: lk_queue::QueueTask = serde_json::from_str(self.pending.trim()).unwrap();
            self.archived = format!("{}\r\n", serde_json::to_string(&task).unwrap());
            task.task_id.push_str("-missing");
            task.target.vault_path = "wiki/documents/deleted.md".into();
            self.pending
                .push_str(&serde_json::to_string(&task).unwrap());
            self.pending.push('\n');
            self.write(Path::new(".lorekeeper/queue/run.jsonl"), &self.pending);
        }

        fn verify(&self) {
            assert!(!self.root.join(".lorekeeper/queue/run.jsonl").exists());
            assert!(!self.root.join(&self.action).exists());
            assert_eq!(
                std::fs::read_to_string(self.root.join(".lorekeeper/queue/processed/run.jsonl"))
                    .unwrap(),
                self.archived
            );
            assert_eq!(
                std::fs::read_to_string(self.root.join(".lorekeeper/work/paid/node.json")).unwrap(),
                "untouched paid bytes\n"
            );
            assert!(!self.root.join(".lorekeeper/mutation/pending.json").exists());
        }
    }

    #[test]
    fn queue_storage_aliases_keep_the_vault_boundary_before_claiming_a_writer() {
        for external in [false, true] {
            let fixture = Fixture::new();
            let outside = tempfile::tempdir().unwrap();
            let destination = if external {
                outside.path().join("queue")
            } else {
                fixture.root.join("retained-queue")
            };
            let queue = fixture.root.join(".lorekeeper/queue");
            std::fs::rename(&queue, &destination).unwrap();
            std::os::unix::fs::symlink(&destination, &queue).unwrap();
            for dry_run in [true, false] {
                let result = super::super::prune_queue(
                    &fixture.root,
                    &crate::commands::ConfigurationPin::Absent,
                    &queue,
                    dry_run,
                );
                if external {
                    assert!(result.is_err());
                    assert!(!destination.join(".lock").exists());
                    assert_eq!(
                        std::fs::read_to_string(destination.join("run.jsonl")).unwrap(),
                        fixture.pending
                    );
                } else {
                    assert_eq!(result.unwrap().files_archived, 1);
                    if dry_run {
                        assert!(destination.join("run.jsonl").exists());
                    } else {
                        fixture.verify();
                    }
                }
            }
        }
    }

    #[test]
    fn retirement_refuses_changed_classification_pages_before_publication_or_recovery() {
        for decision in ["done", "stale", "policy", "missing-section", "missing-file"] {
            for recover in [false, true] {
                let fixture = Fixture::new();
                let target = Path::new("wiki/documents/source.md");
                let original = std::fs::read_to_string(fixture.root.join(target)).unwrap();
                let current = original
                    .lines()
                    .filter(|line| !line.contains("summary_done:"))
                    .collect::<Vec<_>>()
                    .join("\n");
                match decision {
                    "done" => {}
                    "stale" => fixture.write(target, &current.replace("  summary:", "  other:")),
                    "policy" => {
                        let task: lk_queue::QueueTask =
                            serde_json::from_str(fixture.pending.trim()).unwrap();
                        fixture.write(target, &current.replace("---\n##", &format!("  summary_policy_requested_input: {}\n  summary_policy_requested: changed\n---\n##",task.cache_hash)));
                    }
                    "missing-section" => {
                        fixture.write(target, &current.replace("## Summary", "## Other"));
                    }
                    "missing-file" => std::fs::remove_file(fixture.root.join(target)).unwrap(),
                    _ => unreachable!(),
                }
                let prepared = fixture.prepare();
                fixture.write(target, &current);
                if recover {
                    fixture.write(
                        Path::new(".lorekeeper/mutation/pending.json"),
                        &json!({"version":1,"plan":prepared.plan}).to_string(),
                    );
                    assert!(
                        MutationSession::acquire(&fixture.root).is_err(),
                        "{decision}"
                    );
                } else {
                    let session = MutationSession::acquire(&fixture.root).unwrap();
                    assert!(prepared.apply(&session).is_err(), "{decision}");
                    assert!(
                        !fixture
                            .root
                            .join(".lorekeeper/mutation/pending.json")
                            .exists()
                    );
                }
                assert_eq!(
                    std::fs::read_to_string(fixture.root.join(".lorekeeper/queue/run.jsonl"))
                        .unwrap(),
                    fixture.pending
                );
                assert!(fixture.root.join(&fixture.action).exists());
                assert!(!fixture.root.join(".lorekeeper/queue/processed").exists());
            }
        }
    }

    #[test]
    fn retirement_guards_regular_and_rss_sources_that_made_synthesis_stale() {
        for rss in [false, true] {
            let fixture = Fixture::new();
            let concept = "wiki/concepts/topic.md";
            fixture.write(
                Path::new(concept),
                "---\ntype: concept\n---\n## Synthesis\n\n## Related Concepts\n",
            );
            let (origin, original, changed, citation) = if rss {
                let event: lk_core::event::Event = serde_json::from_value(json!({
                    "id":"rss:2026-10-18:0000000000000001","source_id":"rss","source_type":"rss",
                    "timestamp":"2026-10-18T00:00:00Z","date":"2026-10-18","title":"Title","body":"Original evidence.",
                    "url":"https://example.com","author":null,"labels":[],"category":null,
                    "performance_category":null,"is_self":false,"is_personal":false,"metadata":{}
                })).unwrap();
                let capture =
                    lk_core::rss_original::RssOriginalCapture::from_events(&[&event]).unwrap();
                let session = MutationSession::acquire(&fixture.root).unwrap();
                let reference = lk_vault::publish_rss_original(&session, &capture).unwrap();
                drop(session);
                let selected = &capture.manifest.events[0];
                let bindings = json!([{"schema":lk_core::rss_evidence::RSS_EVIDENCE_SCHEMA,
                    "concept":concept,"manifest":reference,
                    "events":[{"event_id":selected.event_id,"revision":selected.revision}]}]);
                let original = format!(
                    "---\ntype: daily\nsource: rss\ncreated: 2026-10-18\nsource_record: {{rss_original: {}}}\nconcept_evidence: {bindings}\n---\n## Related Concepts\n- [Topic](../wiki/concepts/topic.md)\n",
                    serde_json::to_string(&reference).unwrap()
                );
                let origin = "daily/rss.md";
                let citation =
                    lk_vault::rss_concept_citations(&fixture.root, Path::new(origin), &original)
                        .unwrap()[concept][0]
                        .clone();
                let changed = original.replace(
                    &format!("concept_evidence: {bindings}"),
                    "concept_evidence: []",
                );
                (origin, original, changed, citation)
            } else {
                let origin = "wiki/documents/evidence.md";
                let original =
                    "---\ntype: document\n---\n## Content\nOriginal evidence.\n".to_owned();
                let citation = lk_core::concept::Citation::new(
                    Path::new(origin),
                    &lk_vault::parse_page(&original).unwrap(),
                );
                let changed = original.replace("Original", "Changed");
                (origin, original, changed, citation)
            };
            let task = lk_queue::QueueLlmClient::new(fixture.root.join(".lorekeeper/queue"))
                .build_task(
                    "retirement",
                    lk_queue::TaskKind::SynthesizeConcept,
                    lk_queue::ConceptSynthesisRequest {
                        citations: vec![citation],
                        locale: lk_core::i18n::Locale::En,
                        related_anchor: Some("## Related Concepts".into()),
                        target: lk_queue::TaskTarget {
                            vault_path: concept.into(),
                            kind: lk_queue::TargetKind::ConceptSynthesis,
                            anchor: "## Synthesis".into(),
                        },
                    },
                );
            let pending = lk_queue::serialize_tasks(std::slice::from_ref(&task)).unwrap();
            fixture.write(Path::new(".lorekeeper/queue/run.jsonl"), &pending);
            fixture.write(Path::new(concept), &format!("---\ntype: concept\nllm_inputs:\n  synthesis: {}\n---\n## Synthesis\n\n## Related Concepts\n",task.cache_hash));
            fixture.write(Path::new(origin), &changed);
            if !rss {
                let current = lk_queue::QueueLlmClient::new(fixture.root.join(".lorekeeper/queue"))
                    .build_task(
                        "current",
                        lk_queue::TaskKind::SynthesizeConcept,
                        lk_queue::ConceptSynthesisRequest {
                            citations: vec![lk_core::concept::Citation::new(
                                Path::new(origin),
                                &lk_vault::parse_page(&changed).unwrap(),
                            )],
                            locale: lk_core::i18n::Locale::En,
                            related_anchor: Some("## Related Concepts".into()),
                            target: lk_queue::TaskTarget {
                                vault_path: "wiki/concepts/current.md".into(),
                                kind: lk_queue::TargetKind::ConceptSynthesis,
                                anchor: "## Synthesis".into(),
                            },
                        },
                    );
                fixture.write(Path::new("wiki/concepts/current.md"), &format!("---\ntype: concept\nllm_inputs:\n  synthesis: {}\n---\n## Synthesis\n\n## Related Concepts\n", current.cache_hash));
                fixture.write(
                    Path::new(".lorekeeper/queue/a-current.jsonl"),
                    &lk_queue::serialize_tasks(&[current]).unwrap(),
                );
            }
            let prepared = fixture.prepare();
            assert_eq!(prepared.summary.pruned_stale, 1);
            assert_eq!(prepared.summary.kept_current, usize::from(!rss));
            fixture.write(Path::new(origin), &original);
            let session = MutationSession::acquire(&fixture.root).unwrap();
            assert!(prepared.apply(&session).is_err(), "rss={rss}");
            assert_eq!(
                std::fs::read_to_string(fixture.root.join(".lorekeeper/queue/run.jsonl")).unwrap(),
                pending
            );
            assert!(fixture.root.join(&fixture.action).exists());
        }
    }

    #[test]
    fn unrelated_retained_current_page_edits_do_not_block_retirement() {
        let fixture = Fixture::new();
        let task: lk_queue::QueueTask = serde_json::from_str(fixture.pending.trim()).unwrap();
        let mut current = task.clone();
        current.task_id.push_str("-current");
        current.target.vault_path = "wiki/documents/current.md".into();
        let raw = format!(
            "---\ntype: document\nllm_inputs:\n  summary: {}\n---\n## Summary\n",
            current.cache_hash
        );
        fixture.write(Path::new(&current.target.vault_path), &raw);
        let pending = lk_queue::serialize_tasks(&[current.clone()]).unwrap();
        fixture.write(Path::new(".lorekeeper/queue/current.jsonl"), &pending);
        let prepared = fixture.prepare();
        fixture.write(
            Path::new(&current.target.vault_path),
            &format!("{raw}\nUnrelated edit.\n"),
        );
        let session = MutationSession::acquire(&fixture.root).unwrap();
        prepared.apply(&session).unwrap();
        fixture.verify();
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(".lorekeeper/queue/current.jsonl")).unwrap(),
            pending
        );
    }

    #[test]
    fn cached_stale_source_is_reclassified_before_retirement_gets_a_fresh_guard() {
        let fixture = Fixture::new();
        let origin = Path::new("wiki/documents/evidence.md");
        let original = "---\ntype: document\n---\n## Content\nOriginal evidence.\n";
        let citation =
            lk_core::concept::Citation::new(origin, &lk_vault::parse_page(original).unwrap());
        let task = lk_queue::QueueLlmClient::new(fixture.root.join(".lorekeeper/queue"))
            .build_task(
                "retirement",
                lk_queue::TaskKind::SynthesizeConcept,
                lk_queue::ConceptSynthesisRequest {
                    citations: vec![citation],
                    locale: lk_core::i18n::Locale::En,
                    related_anchor: Some("## Related Concepts".into()),
                    target: lk_queue::TaskTarget {
                        vault_path: "wiki/concepts/topic.md".into(),
                        kind: lk_queue::TargetKind::ConceptSynthesis,
                        anchor: "## Synthesis".into(),
                    },
                },
            );
        fixture.write(Path::new(&task.target.vault_path), &format!("---\ntype: concept\nllm_inputs:\n  synthesis: {}\n---\n## Synthesis\n\n## Related Concepts\n",task.cache_hash));
        fixture.write(origin, &original.replace("Original", "Changed"));
        let mut revisions = BTreeMap::from([(
            origin.to_path_buf(),
            super::super::read_source_revision(&fixture.root, origin),
        )]);
        fixture.write(origin, original);
        let classification = classify_for_retirement(&fixture.root, &task, &mut revisions).unwrap();
        assert_eq!(classification.status(), TaskStatus::Current);
        assert!(classification.into_pages().is_empty());
    }

    #[test]
    fn answered_rows_retire_beside_owed_work_only_while_their_proof_holds() {
        for edited in [false, true] {
            let fixture = Fixture::new();
            let done: lk_queue::QueueTask = serde_json::from_str(fixture.pending.trim()).unwrap();
            let mut current = done.clone();
            current.task_id.push_str("-current");
            current.target.vault_path = "wiki/documents/current.md".into();
            let raw = format!(
                "---\ntype: document\nllm_inputs:\n  summary: {}\n---\n## Summary\n",
                current.cache_hash
            );
            fixture.write(Path::new(&current.target.vault_path), &raw);
            let pending = lk_queue::serialize_tasks(&[done.clone(), current.clone()]).unwrap();
            fixture.write(Path::new(".lorekeeper/queue/run.jsonl"), &pending);
            let prepared = fixture.prepare();
            assert_eq!(prepared.summary.retired_done, 1);
            if edited {
                fixture.write(Path::new(&done.target.vault_path), &raw);
            }
            let session = MutationSession::acquire(&fixture.root).unwrap();
            let applied = prepared.apply(&session);
            let queue = fixture.root.join(".lorekeeper/queue");
            if edited {
                assert!(applied.is_err(), "a changed proof refuses the retirement");
                assert_eq!(
                    std::fs::read_to_string(queue.join("run.jsonl")).unwrap(),
                    pending
                );
                assert!(!queue.join("processed").exists());
                assert!(fixture.root.join(&fixture.action).exists());
                continue;
            }
            applied.unwrap();
            let retained = lk_queue::read_tasks(&queue.join("run.jsonl")).unwrap();
            assert_eq!(retained.tasks.len(), 1);
            assert_eq!(retained.tasks[0].task_id, current.task_id);
            let history = lk_queue::read_tasks(&queue.join("processed/run.jsonl")).unwrap();
            assert_eq!(history.tasks.len(), 1);
            assert_eq!(history.tasks[0].task_id, done.task_id);
            assert!(
                !fixture.root.join(&fixture.action).exists(),
                "a retired answer keeps no pending action"
            );
        }
    }

    #[test]
    fn row_retirement_leaves_sidecars_and_disagreeing_proofs_across_runs() {
        let fixture = Fixture::new();
        let done: lk_queue::QueueTask = serde_json::from_str(fixture.pending.trim()).unwrap();
        let mut later = done.clone();
        later.task_id.push_str("-later");
        let mut current = done.clone();
        current.task_id.push_str("-current");
        current.target.vault_path = "wiki/documents/current.md".into();
        fixture.write(
            Path::new(&current.target.vault_path),
            &format!(
                "---\ntype: document\nllm_inputs: {{summary: {}}}\n---\n## Summary\n",
                current.cache_hash
            ),
        );
        let first = lk_queue::serialize_tasks(&[done.clone(), current.clone()]).unwrap();
        let second = lk_queue::serialize_tasks(&[later.clone(), current.clone()]).unwrap();
        fixture.write(Path::new(".lorekeeper/queue/a.jsonl"), &first);
        fixture.write(Path::new(".lorekeeper/queue/b.jsonl"), &second);
        std::fs::remove_file(fixture.root.join(".lorekeeper/queue/run.jsonl")).unwrap();
        let target = Path::new(&done.target.vault_path);
        let edited = format!(
            "{}Authored addition.\n",
            std::fs::read_to_string(fixture.root.join(target)).unwrap()
        );
        let prepared = prepare_with_classifier(
            &fixture.root,
            &fixture.root.join(".lorekeeper/queue"),
            Reach::Rows,
            &lk_vault::ReadControl::default(),
            &mut |task, revisions| {
                let classification = classify_for_retirement(&fixture.root, task, revisions)?;
                if task.task_id == done.task_id {
                    fixture.write(target, &edited);
                }
                Ok(classification)
            },
        )
        .unwrap();
        assert_eq!(prepared.summary.retired_done, 1);
        assert_eq!(prepared.summary.kept_answered, 1);
        prepared
            .apply(&MutationSession::acquire(&fixture.root).unwrap())
            .unwrap_err();
        let queue = fixture.root.join(".lorekeeper/queue");
        assert_eq!(
            std::fs::read_to_string(queue.join("a.jsonl")).unwrap(),
            first
        );
        assert_eq!(
            std::fs::read_to_string(queue.join("b.jsonl")).unwrap(),
            second
        );

        std::fs::write(fixture.root.join(target), {
            let raw = std::fs::read_to_string(fixture.root.join(target)).unwrap();
            raw.replace("Authored addition.\n", "")
        })
        .unwrap();
        let prepared = prepare(
            &fixture.root,
            &queue,
            Reach::Rows,
            &lk_vault::ReadControl::default(),
        )
        .unwrap();
        prepared
            .apply(&MutationSession::acquire(&fixture.root).unwrap())
            .unwrap();
        for (file, retired) in [("a.jsonl", &done), ("b.jsonl", &later)] {
            let pending = lk_queue::read_tasks(&queue.join(file)).unwrap().tasks;
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].task_id, current.task_id);
            let history = lk_queue::read_tasks(&queue.join("processed").join(file))
                .unwrap()
                .tasks;
            assert_eq!(history[0].task_id, retired.task_id);
        }
        assert!(
            fixture.root.join(&fixture.action).exists(),
            "row retirement leaves sidecar retention to maintenance"
        );
    }

    #[test]
    fn a_split_moves_and_keeps_rows_as_their_original_bytes() {
        let fixture = Fixture::new();
        let done: lk_queue::QueueTask = serde_json::from_str(fixture.pending.trim()).unwrap();
        let mut current = done.clone();
        current.task_id.push_str("-current");
        current.target.vault_path = "wiki/documents/current.md".into();
        fixture.write(
            Path::new(&current.target.vault_path),
            &format!(
                "---\ntype: document\nllm_inputs: {{summary: {}}}\n---\n## Summary\n",
                current.cache_hash
            ),
        );
        let extended = |task: &lk_queue::QueueTask| {
            let mut row = serde_json::to_value(task).unwrap();
            row["extension"] = json!({"kept": true});
            format!("{}\r\n", row)
        };
        let mut unheld = done.clone();
        unheld.task_id.push_str("-extended");
        let answered = format!("{}\r\n", serde_json::to_string(&done).unwrap());
        let (unretainable, owed) = (extended(&unheld), extended(&current));
        fixture.write(
            Path::new(".lorekeeper/queue/run.jsonl"),
            &format!("{answered}\n{unretainable}{owed}"),
        );
        let prepared = fixture.prepare();
        assert_eq!(prepared.summary.retired_done, 1);
        assert_eq!(
            prepared.summary.kept_answered, 1,
            "a row processed history cannot hold stays pending"
        );
        prepared
            .apply(&MutationSession::acquire(&fixture.root).unwrap())
            .unwrap();
        let queue = fixture.root.join(".lorekeeper/queue");
        assert_eq!(
            std::fs::read_to_string(queue.join("processed/run.jsonl")).unwrap(),
            answered
        );
        assert_eq!(
            std::fs::read_to_string(queue.join("run.jsonl")).unwrap(),
            format!("{unretainable}{owed}")
        );
    }

    #[test]
    fn malformed_markers_preserve_pending_requests_and_action_bytes() {
        let fixture = Fixture::new();
        let task: lk_queue::QueueTask = serde_json::from_str(fixture.pending.trim()).unwrap();
        for inputs in [
            json!("broken"),
            json!([]),
            json!({"summary": 42}),
            json!({"summary": "other", "summary_done": []}),
            json!({"summary_policy_requested_input": 42}),
            json!({"summary": task.cache_hash, "summary_done": [], "summary_policy_requested_input": task.cache_hash, "summary_policy_requested": "changed-policy"}),
        ] {
            let raw = format!("---\ntype: document\nllm_inputs: {inputs}\n---\n## Summary\n");
            let target = Path::new("wiki/documents/source.md");
            fixture.write(target, &raw);
            let action = std::fs::read(fixture.root.join(&fixture.action)).unwrap();
            for dry_run in [true, false] {
                let summary = super::super::prune_queue(
                    &fixture.root,
                    &crate::commands::ConfigurationPin::Absent,
                    &fixture.root.join(".lorekeeper/queue"),
                    dry_run,
                )
                .unwrap();
                assert_eq!(summary.kept_unreadable, 1, "{inputs}");
                assert_eq!(summary.pruned_stale, 0);
                assert_eq!(summary.files_archived, 0);
                assert_eq!(
                    std::fs::read_to_string(fixture.root.join(".lorekeeper/queue/run.jsonl"))
                        .unwrap(),
                    fixture.pending
                );
                assert_eq!(
                    std::fs::read(fixture.root.join(&fixture.action)).unwrap(),
                    action
                );
                assert_eq!(
                    std::fs::read_to_string(fixture.root.join(target)).unwrap(),
                    raw
                );
                assert_eq!(
                    std::fs::read_to_string(fixture.root.join(".lorekeeper/work/paid/node.json"))
                        .unwrap(),
                    "untouched paid bytes\n"
                );
            }
        }
    }

    #[test]
    fn retained_done_capture_conflicts_do_not_block_independent_retirement() {
        for mixed in [true, false] {
            for preview in [true, false] {
                let fixture = Fixture::new();
                let first: lk_queue::QueueTask =
                    serde_json::from_str(fixture.pending.trim()).unwrap();
                let mut second = first.clone();
                second.task_id.push_str("-second");
                let mut missing = first.clone();
                missing.task_id.push_str("-missing");
                missing.target.vault_path = "wiki/documents/missing.md".into();
                let mut tasks = vec![first.clone(), second, missing];
                if mixed {
                    let mut current = first.clone();
                    current.task_id.push_str("-current");
                    current.target.vault_path = "wiki/documents/current.md".into();
                    fixture.write(
                        Path::new(&current.target.vault_path),
                        &format!(
                            "---\ntype: document\nllm_inputs: {{summary: {}}}\n---\n## Summary\n",
                            current.cache_hash
                        ),
                    );
                    tasks.push(current);
                }
                let pending = lk_queue::serialize_tasks(&tasks).unwrap();
                fixture.write(Path::new(".lorekeeper/queue/run.jsonl"), &pending);
                let action = std::fs::read(fixture.root.join(&fixture.action)).unwrap();
                let target = Path::new(&first.target.vault_path);
                let edited = format!(
                    "{}Authored addition.\n",
                    std::fs::read_to_string(fixture.root.join(target)).unwrap()
                );
                let prepared = prepare_with_classifier(
                    &fixture.root,
                    &fixture.root.join(".lorekeeper/queue"),
                    Reach::Sidecars,
                    &lk_vault::ReadControl::default(),
                    &mut |task, revisions| {
                        let classification =
                            classify_for_retirement(&fixture.root, task, revisions)?;
                        if task.task_id == first.task_id {
                            fixture.write(target, &edited);
                        }
                        Ok(classification)
                    },
                );
                if !mixed {
                    assert!(
                        prepared.is_err(),
                        "archival must reject conflicting evidence"
                    );
                    assert_eq!(
                        std::fs::read_to_string(fixture.root.join(".lorekeeper/queue/run.jsonl"))
                            .unwrap(),
                        pending
                    );
                    continue;
                }
                let prepared = prepared.unwrap();
                assert_eq!(prepared.summary.pruned_missing_target, 1);
                if preview {
                    prepared.preview().unwrap();
                    assert_eq!(
                        std::fs::read_to_string(fixture.root.join(".lorekeeper/queue/run.jsonl"))
                            .unwrap(),
                        pending
                    );
                } else {
                    let session = MutationSession::acquire(&fixture.root).unwrap();
                    prepared.apply(&session).unwrap();
                    tasks.remove(2);
                    assert_eq!(
                        std::fs::read_to_string(fixture.root.join(".lorekeeper/queue/run.jsonl"))
                            .unwrap(),
                        lk_queue::serialize_tasks(&tasks).unwrap()
                    );
                }
                assert_eq!(
                    std::fs::read_to_string(fixture.root.join(target)).unwrap(),
                    edited
                );
                assert_eq!(
                    std::fs::read(fixture.root.join(&fixture.action)).unwrap(),
                    action
                );
                assert!(
                    !fixture
                        .root
                        .join(".lorekeeper/mutation/pending.json")
                        .exists()
                );
            }
        }
    }

    #[test]
    fn discovery_retention_checks_directory_containment_before_reading_entries() {
        for external in [false, true] {
            let fixture = Fixture::new();
            let outside = tempfile::tempdir().unwrap();
            let destination = if external {
                outside.path().join("discovery")
            } else {
                fixture.root.join("retained-discovery")
            };
            std::fs::create_dir_all(&destination).unwrap();
            let directory = fixture.root.join(".lorekeeper/queue/discovery");
            std::os::unix::fs::symlink(&destination, &directory).unwrap();
            let mut plan = MutationPlan::default();
            let result = super::super::super::work::plan_prune_discovery(
                &fixture.root,
                &BTreeSet::new(),
                &mut plan,
            );
            if external {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap(), 0);
            }
            assert!(!plan.has_changes());
            assert!(destination.is_dir());
        }
    }

    #[test]
    fn unchanged_archive_preserves_the_processed_history_retention_clock() {
        let fixture = Fixture::new();
        let path = fixture.root.join(".lorekeeper/queue/run.jsonl");
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let before = std::fs::metadata(path).unwrap().modified().unwrap();
        let session = MutationSession::acquire(&fixture.root).unwrap();
        fixture.prepare().apply(&session).unwrap();
        fixture.verify();
        assert_eq!(
            std::fs::metadata(fixture.root.join(".lorekeeper/queue/processed/run.jsonl"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
    }

    #[test]
    fn retirement_recovers_every_prepared_prefix_with_exact_archive_and_paid_bytes() {
        for rewritten in [false, true] {
            let mut fixture = Fixture::new();
            if rewritten {
                fixture.add_missing_task();
            }
            let count = serde_json::to_value(fixture.prepare().plan).unwrap()["steps"]
                .as_array()
                .unwrap()
                .len();
            for prefix in 0..=count {
                let mut fixture = Fixture::new();
                if rewritten {
                    fixture.add_missing_task();
                }
                let value = serde_json::to_value(fixture.prepare().plan).unwrap();
                let steps = value["steps"].as_array().unwrap();
                let is_guard = |step: &serde_json::Value| {
                    step["kind"] == "require-absent"
                        || (step["kind"] == "edit" && step["before"] == step["after"])
                };
                let durable = steps[..prefix]
                    .iter()
                    .enumerate()
                    .filter(|(_, step)| !is_guard(step))
                    .map(|(index, _)| index + 1)
                    .collect::<Vec<_>>();
                let last = durable.last().copied().unwrap_or(0);
                let mut checkpoints = BTreeSet::from([last]);
                if prefix == last && last != 0 {
                    checkpoints.insert(durable.iter().rev().nth(1).copied().unwrap_or(0));
                }
                for completed in checkpoints {
                    let mut fixture = Fixture::new();
                    if rewritten {
                        fixture.add_missing_task();
                    }
                    let prepared = fixture.prepare();
                    assert!(prepared.plan.has_changes());
                    let revision = lk_core::concept::file_revision(
                        &serde_json::to_string(&prepared.plan).unwrap(),
                    );
                    let plan = serde_json::to_value(&prepared.plan).unwrap();
                    let steps = plan["steps"].as_array().unwrap();
                    assert_eq!(steps.len(), count);
                    fixture.write(
                        Path::new(".lorekeeper/mutation/pending.json"),
                        &json!({"version":1, "plan":plan}).to_string(),
                    );
                    if completed != 0 {
                        let checksum =
                            lk_core::concept::file_revision(&format!("{revision}:{completed}"));
                        fixture.write(Path::new(".lorekeeper/mutation/progress.json"), &json!({"plan_revision":revision,"completed":completed,"checksum":checksum}).to_string());
                    }
                    for step in &steps[..prefix] {
                        if is_guard(step) {
                            continue;
                        }
                        match step["kind"].as_str().unwrap() {
                            "rename" => {
                                let from = fixture.root.join(step["from"].as_str().unwrap());
                                let to = fixture.root.join(step["to"].as_str().unwrap());
                                std::fs::create_dir_all(to.parent().unwrap()).unwrap();
                                std::fs::rename(from, to).unwrap();
                            }
                            "edit" => fixture.write(
                                Path::new(step["path"].as_str().unwrap()),
                                step["after"].as_str().unwrap(),
                            ),
                            "remove" => std::fs::remove_file(
                                fixture.root.join(step["path"].as_str().unwrap()),
                            )
                            .unwrap(),
                            _ => panic!("unexpected retirement step"),
                        }
                    }
                    let session = MutationSession::acquire(&fixture.root).unwrap();
                    fixture.verify();
                    assert!(
                        !fixture
                            .root
                            .join(".lorekeeper/mutation/progress.json")
                            .exists()
                    );
                    assert!(!fixture.prepare().plan.has_changes());
                    drop(session);
                }
            }
        }
    }

    #[test]
    fn late_store_corruption_and_changed_inputs_leave_every_queued_file_unchanged() {
        for store in [
            "section-actions/unknown.json",
            "result-actions/unknown.json",
            "discovery/unknown.recovery.json",
            "screening/unknown",
        ] {
            let fixture = Fixture::new();
            let path = Path::new(".lorekeeper/queue").join(store);
            fixture.write(&path, "{}");
            let before = std::fs::read(fixture.root.join(&fixture.action)).unwrap();
            for dry_run in [true, false] {
                assert!(
                    super::super::prune_queue(
                        &fixture.root,
                        &crate::commands::ConfigurationPin::Absent,
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
