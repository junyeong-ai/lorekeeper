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
    prepare_with_classifier(root, queue, &mut |task, revisions| {
        classify_for_retirement(root, task, revisions)
    })
}

fn prepare_with_classifier(
    root: &Path,
    queue: &Path,
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
    for file in super::pending_queue_files(queue)? {
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
                plan.require_unchanged(relative, raw);
            }
            continue;
        }
        let raw = String::from_utf8(raw).into_diagnostic()?;
        let mut tasks = Vec::with_capacity(read.tasks.len());
        let mut done_pages = Ok(BTreeMap::new());
        let (mut dropped, mut work_left) = (0, 0);
        for task in read.tasks {
            let valid = task.generation_policy().is_ok() && task.validate_retained_input().is_ok();
            let classification = if valid {
                classify(&task, &mut revisions)?
            } else {
                complete = false;
                CapturedClassification {
                    status: super::classify_task(root, &task, &mut revisions)?,
                    pages: BTreeMap::new(),
                }
            };
            match classification.status {
                TaskStatus::Current => {
                    work_left += 1;
                    summary.kept_current += 1;
                    tasks.push(task);
                }
                TaskStatus::Done => {
                    done_pages = done_pages.and_then(|mut pages| {
                        retain_snapshots(&mut pages, classification.pages)?;
                        Ok(pages)
                    });
                    summary.retired_done += 1;
                    tasks.push(task);
                }
                TaskStatus::Stale => {
                    retain_snapshots(&mut pages, classification.pages)
                        .map_err(classification_drift)?;
                    summary.pruned_stale += 1;
                    dropped += 1;
                }
                TaskStatus::MissingTarget => {
                    retain_snapshots(&mut pages, classification.pages)
                        .map_err(classification_drift)?;
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
        if work_left == 0 {
            retain_snapshots(&mut pages, done_pages.map_err(classification_drift)?)
                .map_err(classification_drift)?;
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
            if dropped == 0 {
                plan.rename(relative, destination, raw);
            } else {
                let after = lk_queue::serialize_tasks(&tasks).into_diagnostic()?;
                plan.rename(relative, destination.clone(), raw.clone());
                plan.edit(destination, raw, after);
            }
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
    let mut guarded = MutationPlan::default();
    for (path, raw) in pages {
        match raw {
            Some(raw) => guarded.require_unchanged(path, raw),
            None => guarded.require_absent(path),
        }
    }
    guarded.extend(plan);
    let mut plan = guarded;
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

struct CapturedClassification {
    status: TaskStatus,
    pages: BTreeMap<PathBuf, Option<String>>,
}

fn classify_for_retirement(
    root: &Path,
    task: &lk_queue::QueueTask,
    revisions: &mut BTreeMap<PathBuf, Result<String, TaskStatus>>,
) -> miette::Result<CapturedClassification> {
    let mut pages = BTreeMap::new();
    let mut classification = super::classify_with_reader(root, task, revisions, &mut |path| {
        read_snapshot(root, path, &mut pages)
    })?;
    let retires = |status| {
        matches!(
            status,
            TaskStatus::Done | TaskStatus::Stale | TaskStatus::MissingTarget
        )
    };
    if retires(classification.status)
        && classification
            .dependencies
            .iter()
            .any(|path| !pages.contains_key(path))
    {
        pages.clear();
        let mut fresh_revisions = BTreeMap::new();
        classification =
            super::classify_with_reader(root, task, &mut fresh_revisions, &mut |path| {
                read_snapshot(root, path, &mut pages)
            })?;
        revisions.extend(fresh_revisions);
    }
    if retires(classification.status) {
        pages.retain(|path, _| classification.dependencies.contains(path));
    } else {
        pages.clear();
    }
    Ok(CapturedClassification {
        status: classification.status,
        pages,
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

fn classification_drift(path: PathBuf) -> miette::Report {
    miette::miette!(
        "queue classification inputs changed during retirement: {}",
        path.display()
    )
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
            prepare(&self.root, &self.root.join(".lorekeeper/queue")).unwrap()
        }

        fn add_missing_task(&mut self) {
            let mut task: lk_queue::QueueTask = serde_json::from_str(self.pending.trim()).unwrap();
            self.archived = lk_queue::serialize_tasks(std::slice::from_ref(&task)).unwrap();
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
                let result = super::super::prune_queue(&fixture.root, &queue, dry_run);
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
        assert_eq!(classification.status, TaskStatus::Current);
        assert!(classification.pages.is_empty());
    }

    #[test]
    fn retained_done_rows_do_not_guard_their_pages_when_other_rows_are_pruned() {
        let fixture = Fixture::new();
        let done: lk_queue::QueueTask = serde_json::from_str(fixture.pending.trim()).unwrap();
        let mut current = done.clone();
        current.task_id.push_str("-current");
        current.target.vault_path = "wiki/documents/current.md".into();
        let mut missing = done.clone();
        missing.task_id.push_str("-missing");
        missing.target.vault_path = "wiki/documents/missing.md".into();
        let raw = format!(
            "---\ntype: document\nllm_inputs:\n  summary: {}\n---\n## Summary\n",
            current.cache_hash
        );
        fixture.write(Path::new(&current.target.vault_path), &raw);
        fixture.write(
            Path::new(".lorekeeper/queue/run.jsonl"),
            &lk_queue::serialize_tasks(&[done.clone(), current.clone(), missing]).unwrap(),
        );
        let prepared = fixture.prepare();
        assert_eq!(prepared.summary.pruned_missing_target, 1);
        fixture.write(Path::new(&done.target.vault_path), &raw);
        let session = MutationSession::acquire(&fixture.root).unwrap();
        prepared.apply(&session).unwrap();
        let retained =
            lk_queue::read_tasks(&fixture.root.join(".lorekeeper/queue/run.jsonl")).unwrap();
        assert_eq!(retained.tasks.len(), 2);
        assert_eq!(retained.tasks[0].task_id, done.task_id);
        assert_eq!(retained.tasks[1].task_id, current.task_id);
        assert!(fixture.root.join(&fixture.action).exists());
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
